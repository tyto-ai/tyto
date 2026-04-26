use anyhow::Result;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crate::embed;

const RRF_K: f64 = 60.0;

#[derive(Debug)]
pub struct CodeResult {
    pub id: String,
    pub symbol_name: String,
    pub qualified_name: String,
    pub symbol_kind: String,
    pub file_path: String,
    pub line_start: i64,
    pub line_end: i64,
    pub signature: Option<String>,
    pub doc_comment: Option<String>,
    pub body_preview: Option<String>,
    pub churn_count: i64,
    pub hotspot_score: f64,
    pub language: String,
    pub rrf_score: f64,
    pub related_commits: Vec<String>,
    /// Number of identical (same body hash) results collapsed into this entry.
    pub duplicate_count: usize,
}

/// Hybrid vector + FTS search over indexed code chunks.
pub async fn search_code(
    conn: &Arc<turso::Connection>,
    embedding: Vec<f32>,
    query: &str,
    limit: usize,
) -> Result<Vec<CodeResult>> {
    let t_total = Instant::now();
    let blob = embed::floats_to_blob(&embedding);
    let model = embed::model_id();
    let k = limit * 2;

    // Stream A: vector search (brute-force cosine distance over index_vectors).
    // Same approach as memory search — turso's vector_distance_cos + vector32 work
    // on any local DB opened with experimental_index_method(true).
    // Gracefully degrades to FTS-only if index_vectors is empty or query fails.
    let t_vec = Instant::now();
    let mut vector_ranks: HashMap<String, usize> = HashMap::new();
    match conn.query(
        "SELECT ic.id, vector_distance_cos(iv.embedding, vector32(?1)) as dist
         FROM index_chunks ic
         JOIN index_vectors iv ON iv.chunk_id = ic.id
         WHERE iv.embed_model = ?2
         ORDER BY dist
         LIMIT ?3",
        (blob, model, k as i64),
    ).await {
        Ok(mut rows) => {
            let mut rank = 0usize;
            while let Some(row) = rows.next().await? {
                if let Ok(id) = row.get::<String>(0) {
                    vector_ranks.insert(id, rank);
                    rank += 1;
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "code vector search failed, falling back to FTS-only");
        }
    }
    tracing::debug!(elapsed_ms = t_vec.elapsed().as_millis(), results = vector_ranks.len(), "code vector search");

    // Stream B: native FTS using turso's fts_match() / fts_score() functions.
    // Syntax: WHERE fts_match(col1, col2, ..., query) — NOT the SQLite FTS5
    // "table_name MATCH query" form which turso does not support.
    // Results ordered by fts_score() DESC so RRF rank reflects relevance order.
    let t_fts = Instant::now();
    let fts_ranks: HashMap<String, usize> = {
        let fts_q = build_fts_query(query);
        if fts_q.is_empty() {
            HashMap::new()
        } else {
            let mut rows = conn.query(
                "SELECT id
                 FROM index_chunks
                 WHERE fts_match(symbol_name, qualified_name, signature, doc_comment, body_preview, ?1)
                 ORDER BY fts_score(symbol_name, qualified_name, signature, doc_comment, body_preview, ?1) DESC
                 LIMIT ?2",
                (fts_q, k as i64)
            ).await?;
            let mut ranks = HashMap::new();
            let mut i = 0;
            while let Some(row) = rows.next().await? {
                if let Ok(id) = row.get::<String>(0) {
                    ranks.insert(id, i);
                    i += 1;
                }
            }
            ranks
        }
    };
    tracing::debug!(elapsed_ms = t_fts.elapsed().as_millis(), results = fts_ranks.len(), "code fts search");

    // Merge candidate IDs from both streams
    let mut all_ids: Vec<String> = vector_ranks.keys().cloned().collect();
    for id in fts_ranks.keys() {
        if !vector_ranks.contains_key(id) {
            all_ids.push(id.clone());
        }
    }

    if all_ids.is_empty() {
        return Ok(vec![]);
    }

    // Fetch metadata and compute two-stream RRF scores
    let t_meta = Instant::now();
    let placeholders = all_ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!(
        "SELECT id, symbol_name, qualified_name, symbol_kind, file_path,
                line_start, line_end, signature, doc_comment, churn_count, hotspot_score, language,
                body_preview
         FROM index_chunks WHERE id IN ({placeholders})"
    );
    let mut rows = conn.query(&sql, turso::params_from_iter(all_ids.clone())).await?;
    let mut scored: Vec<CodeResult> = Vec::new();
    while let Some(row) = rows.next().await? {
        let id: String = row.get(0)?;
        let rrf_v = vector_ranks.get(&id).map(|&r| 1.0 / (RRF_K + r as f64)).unwrap_or(0.0);
        let rrf_f = fts_ranks.get(&id).map(|&r| 1.0 / (RRF_K + r as f64)).unwrap_or(0.0);
        scored.push(CodeResult {
            id,
            symbol_name: row.get(1)?,
            qualified_name: row.get(2)?,
            symbol_kind: row.get(3)?,
            file_path: row.get(4)?,
            line_start: row.get(5)?,
            line_end: row.get(6)?,
            signature: row.get(7)?,
            doc_comment: row.get(8)?,
            churn_count: row.get(9).unwrap_or(0),
            hotspot_score: row.get(10).unwrap_or(0.0),
            language: row.get(11).unwrap_or_default(),
            body_preview: row.get(12).ok(),
            rrf_score: rrf_v + rrf_f,
            related_commits: vec![],
            duplicate_count: 0,
        });
    }

    scored.sort_by(|a, b| b.rrf_score.partial_cmp(&a.rrf_score).unwrap_or(std::cmp::Ordering::Equal));

    // Deduplicate by normalized body hash; keep highest-scoring entry per hash.
    // Iterating in score order means the first occurrence is always the best.
    let mut seen_hashes: HashMap<[u8; 32], usize> = HashMap::new();
    let mut deduped: Vec<CodeResult> = Vec::new();
    for r in scored {
        let hash = body_hash(r.body_preview.as_deref());
        if let Some(&idx) = seen_hashes.get(&hash) {
            deduped[idx].duplicate_count += 1;
        } else {
            seen_hashes.insert(hash, deduped.len());
            deduped.push(r);
        }
    }
    let mut scored = deduped;
    scored.truncate(limit);
    tracing::debug!(elapsed_ms = t_meta.elapsed().as_millis(), candidates = scored.len(), "code metadata fetch");

    let t_commits = Instant::now();
    let ids: Vec<String> = scored.iter().map(|r| r.id.clone()).collect();
    let commit_map = fetch_related_commits_batch(conn, &ids, 3).await?;
    for result in &mut scored {
        result.related_commits = commit_map.get(&result.id).cloned().unwrap_or_default();
    }
    tracing::debug!(elapsed_ms = t_commits.elapsed().as_millis(), "code commit fetch batch");

    tracing::debug!(elapsed_ms = t_total.elapsed().as_millis(), results = scored.len(), "search_code total");
    Ok(scored)
}

async fn fetch_related_commits_batch(
    conn: &Arc<turso::Connection>,
    chunk_ids: &[String],
    per_chunk_limit: usize,
) -> Result<HashMap<String, Vec<String>>> {
    if chunk_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = chunk_ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!(
        "SELECT cc.chunk_id, c.message
         FROM index_commits c
         JOIN index_chunk_commits cc ON cc.commit_sha = c.sha
         WHERE cc.chunk_id IN ({placeholders})
         ORDER BY cc.chunk_id, c.sha"
    );
    let mut rows = conn.query(&sql, turso::params_from_iter(chunk_ids.iter().cloned())).await?;
    let mut result: HashMap<String, Vec<String>> = HashMap::new();
    while let Some(row) = rows.next().await? {
        let chunk_id: String = row.get(0)?;
        let message: String = row.get(1)?;
        let msgs = result.entry(chunk_id).or_default();
        if msgs.len() < per_chunk_limit {
            msgs.push(message);
        }
    }
    Ok(result)
}

fn body_hash(body: Option<&str>) -> [u8; 32] {
    let normalized = body.unwrap_or("").lines()
        .map(|l| l.trim())
        .collect::<Vec<_>>()
        .join("\n");
    Sha256::digest(normalized.as_bytes()).into()
}

/// Lookup a symbol by name (and optionally file path).
pub async fn get_symbol(
    conn: &Arc<turso::Connection>,
    name_path: &str,
    file_path: Option<&str>,
) -> Result<Vec<CodeResult>> {
    let (sql, params): (String, Vec<String>) = if let Some(fp) = file_path {
        (
            "SELECT id, symbol_name, qualified_name, symbol_kind, file_path,
                    line_start, line_end, signature, doc_comment, churn_count, hotspot_score, language,
                    body_preview
             FROM index_chunks
             WHERE (symbol_name = ?1 OR qualified_name = ?1 OR qualified_name LIKE ?2)
               AND file_path = ?3
             ORDER BY line_start
             LIMIT 20".to_string(),
            vec![name_path.to_string(), format!("%::{name_path}"), fp.to_string()],
        )
    } else {
        (
            "SELECT id, symbol_name, qualified_name, symbol_kind, file_path,
                    line_start, line_end, signature, doc_comment, churn_count, hotspot_score, language,
                    body_preview
             FROM index_chunks
             WHERE symbol_name = ?1 OR qualified_name = ?1 OR qualified_name LIKE ?2
             ORDER BY hotspot_score DESC, churn_count DESC, line_start
             LIMIT 20".to_string(),
            vec![name_path.to_string(), format!("%::{name_path}")],
        )
    };

    let mut rows = conn.query(&sql, turso::params_from_iter(params)).await?;
    let mut results = Vec::new();
    while let Some(row) = rows.next().await? {
        results.push(CodeResult {
            id: row.get(0)?,
            symbol_name: row.get(1)?,
            qualified_name: row.get(2)?,
            symbol_kind: row.get(3)?,
            file_path: row.get(4)?,
            line_start: row.get(5)?,
            line_end: row.get(6)?,
            signature: row.get(7)?,
            doc_comment: row.get(8)?,
            churn_count: row.get(9).unwrap_or(0),
            hotspot_score: row.get(10).unwrap_or(0.0),
            language: row.get(11).unwrap_or_default(),
            body_preview: row.get(12).ok(),
            rrf_score: 0.0,
            related_commits: vec![],
            duplicate_count: 0,
        });
    }

    let ids: Vec<String> = results.iter().map(|r| r.id.clone()).collect();
    let commit_map = fetch_related_commits_batch(conn, &ids, 5).await?;
    for r in &mut results {
        r.related_commits = commit_map.get(&r.id).cloned().unwrap_or_default();
    }
    Ok(results)
}

/// Retrieve overall index statistics.
pub async fn index_stats(conn: &Arc<turso::Connection>) -> Result<(i64, i64)> {
    let mut rows = conn.query("SELECT COUNT(*) FROM index_files", ()).await?;
    let files = rows.next().await?.map(|r| r.get::<i64>(0)).transpose()?.unwrap_or(0);
    let mut rows = conn.query("SELECT COUNT(*) FROM index_chunks", ()).await?;
    let chunks = rows.next().await?.map(|r| r.get::<i64>(0)).transpose()?.unwrap_or(0);
    Ok((files, chunks))
}

/// Format a CodeResult for display to the agent.
pub fn format_result(r: &CodeResult, verbose: bool) -> String {
    let dup_suffix = if r.duplicate_count > 0 {
        format!("  (+{} duplicates)", r.duplicate_count)
    } else {
        String::new()
    };
    let mut out = format!(
        "[{}] {:.3}  {}:{}-{}{} {}\n",
        r.symbol_kind, r.rrf_score, r.file_path, r.line_start, r.line_end, dup_suffix, r.qualified_name
    );
    if let Some(ref sig) = r.signature {
        out.push_str(&format!("Signature: {sig}\n"));
    }
    if verbose && let Some(ref doc) = r.doc_comment {
        let first = doc.lines().next().unwrap_or("").trim();
        if !first.is_empty() {
            let cleaned = first
                .trim_start_matches("///")
                .trim_start_matches("//!")
                .trim();
            out.push_str(&format!("Doc: {cleaned}\n"));
        }
    }
    if r.churn_count > 0 {
        out.push_str(&format!("Churn: {} commits\n", r.churn_count));
    }
    if r.hotspot_score > 0.01 {
        out.push_str(&format!("Hotspot: {:.2}\n", r.hotspot_score));
    }
    if !r.related_commits.is_empty() {
        out.push_str(&format!(
            "History: {}\n",
            r.related_commits.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(", ")
        ));
    }
    out
}

pub(crate) fn build_fts_query(query: &str) -> String {
    // Tantivy query syntax: bare `token*` for prefix match.
    // Operators (AND/OR/NOT) are uppercase in Tantivy, so lowercase terms are safe.
    query
        .split_whitespace()
        .filter_map(|w| {
            let clean: String = w.chars().filter(|c| c.is_alphanumeric() || *c == '_').collect();
            if clean.is_empty() { None } else { Some(format!("{clean}*")) }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::build_fts_query;

    #[test]
    fn fts_basic_terms() {
        let q = build_fts_query("index file");
        assert!(q.contains("index*"));
        assert!(q.contains("file*"));
    }

    #[test]
    fn fts_strips_special_chars() {
        let q = build_fts_query("file.path foo-bar");
        assert!(q.contains("filepath*"));
        assert!(q.contains("foobar*"));
    }

    #[test]
    fn fts_empty_tokens_dropped() {
        let q = build_fts_query("... --- !!!");
        assert!(q.is_empty());
    }

    #[test]
    fn fts_single_term() {
        let q = build_fts_query("ownership");
        assert_eq!(q, "ownership*");
    }

    #[test]
    fn fts_underscores_preserved() {
        let q = build_fts_query("my_function");
        assert!(q.contains("my_function*"));
    }

    #[test]
    fn fts_empty_input() {
        assert!(build_fts_query("").is_empty());
        assert!(build_fts_query("   ").is_empty());
    }
}
