use anyhow::Result;
use libsql::{params, params_from_iter};
use std::collections::HashMap;

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
    pub churn_count: i64,
    pub language: String,
    pub rrf_score: f64,
    pub related_commits: Vec<String>,
}

/// Hybrid vector + FTS5 search over indexed code chunks.
pub async fn search_code(
    conn: &libsql::Connection,
    embedding: Vec<f32>,
    query: &str,
    limit: usize,
) -> Result<Vec<CodeResult>> {
    let blob = embed::floats_to_blob(&embedding);
    let model = embed::model_id();
    let k = (limit * 2) as i64;

    // Stream A: vector search
    let mut vector_ranks: HashMap<String, usize> = HashMap::new();
    {
        let mut rows = conn.query(
            "SELECT c.id
             FROM index_chunks c
             JOIN index_vectors v ON v.chunk_id = c.id
             WHERE v.embed_model = ?1
             ORDER BY vector_distance_cos(v.embedding, vector32(?2))
             LIMIT ?3",
            params![model.clone(), blob, k],
        ).await?;
        let mut rank = 0usize;
        while let Some(row) = rows.next().await? {
            let id: String = row.get(0)?;
            vector_ranks.insert(id, rank);
            rank += 1;
        }
    }

    // Stream B: FTS5 search
    let mut fts_ranks: HashMap<String, usize> = HashMap::new();
    {
        let fts_q = build_fts_query(query);
        if !fts_q.is_empty() {
            let mut rows = conn.query(
                "SELECT c.id
                 FROM index_chunks c
                 JOIN index_chunks_fts ON index_chunks_fts.rowid = c.rowid
                 WHERE index_chunks_fts MATCH ?1
                 ORDER BY bm25(index_chunks_fts)
                 LIMIT ?2",
                params![fts_q, k],
            ).await?;
            let mut rank = 0usize;
            while let Some(row) = rows.next().await? {
                let id: String = row.get(0)?;
                fts_ranks.insert(id, rank);
                rank += 1;
            }
        }
    }

    // Collect all candidate IDs
    let mut all_ids: Vec<String> = vector_ranks.keys().cloned().collect();
    for id in fts_ranks.keys() {
        if !vector_ranks.contains_key(id) {
            all_ids.push(id.clone());
        }
    }

    if all_ids.is_empty() {
        return Ok(vec![]);
    }

    // Fetch metadata in one query
    let placeholders = all_ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!(
        "SELECT id, symbol_name, qualified_name, symbol_kind, file_path,
                line_start, line_end, signature, doc_comment, churn_count, language
         FROM index_chunks WHERE id IN ({placeholders})"
    );
    let mut rows = conn.query(&sql, params_from_iter(all_ids.iter().cloned())).await?;

    let mut scored: Vec<CodeResult> = Vec::new();
    while let Some(row) = rows.next().await? {
        let id: String = row.get(0)?;
        let rrf_v = vector_ranks.get(&id).map(|&r| 1.0 / (RRF_K + r as f64)).unwrap_or(0.0);
        let rrf_f = fts_ranks.get(&id).map(|&r| 1.0 / (RRF_K + r as f64)).unwrap_or(0.0);

        scored.push(CodeResult {
            id: id.clone(),
            symbol_name: row.get(1)?,
            qualified_name: row.get(2)?,
            symbol_kind: row.get(3)?,
            file_path: row.get(4)?,
            line_start: row.get(5)?,
            line_end: row.get(6)?,
            signature: row.get(7).ok(),
            doc_comment: row.get(8).ok(),
            churn_count: row.get(9).unwrap_or(0),
            language: row.get(10).unwrap_or_default(),
            rrf_score: rrf_v + rrf_f,
            related_commits: vec![],
        });
    }

    scored.sort_by(|a, b| b.rrf_score.partial_cmp(&a.rrf_score).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);

    // Fetch related commits for top results
    for result in &mut scored {
        result.related_commits = fetch_related_commits(conn, &result.id, 3).await?;
    }

    Ok(scored)
}

async fn fetch_related_commits(
    conn: &libsql::Connection,
    chunk_id: &str,
    limit: usize,
) -> Result<Vec<String>> {
    let mut rows = conn.query(
        "SELECT c.message
         FROM index_commits c
         JOIN index_chunk_commits cc ON cc.commit_sha = c.sha
         WHERE cc.chunk_id = ?1
         LIMIT ?2",
        params![chunk_id.to_string(), limit as i64],
    ).await?;
    let mut msgs = Vec::new();
    while let Some(row) = rows.next().await? {
        if let Ok(msg) = row.get::<String>(0) {
            msgs.push(msg);
        }
    }
    Ok(msgs)
}

/// Lookup a symbol by name (and optionally file path).
pub async fn get_symbol(
    conn: &libsql::Connection,
    name_path: &str,
    file_path: Option<&str>,
) -> Result<Vec<CodeResult>> {
    let (sql, params_vec): (String, Vec<libsql::Value>) = if let Some(fp) = file_path {
        (
            "SELECT id, symbol_name, qualified_name, symbol_kind, file_path,
                    line_start, line_end, signature, doc_comment, churn_count, language
             FROM index_chunks
             WHERE (symbol_name = ?1 OR qualified_name = ?1 OR qualified_name LIKE ?2)
               AND file_path = ?3
             ORDER BY line_start
             LIMIT 20".to_string(),
            vec![
                libsql::Value::Text(name_path.to_string()),
                libsql::Value::Text(format!("%::{name_path}")),
                libsql::Value::Text(fp.to_string()),
            ],
        )
    } else {
        (
            "SELECT id, symbol_name, qualified_name, symbol_kind, file_path,
                    line_start, line_end, signature, doc_comment, churn_count, language
             FROM index_chunks
             WHERE symbol_name = ?1 OR qualified_name = ?1 OR qualified_name LIKE ?2
             ORDER BY churn_count DESC, line_start
             LIMIT 20".to_string(),
            vec![
                libsql::Value::Text(name_path.to_string()),
                libsql::Value::Text(format!("%::{name_path}")),
            ],
        )
    };

    let mut rows = conn.query(&sql, params_from_iter(params_vec)).await?;
    let mut results = Vec::new();
    while let Some(row) = rows.next().await? {
        let id: String = row.get(0)?;
        let mut r = CodeResult {
            id: id.clone(),
            symbol_name: row.get(1)?,
            qualified_name: row.get(2)?,
            symbol_kind: row.get(3)?,
            file_path: row.get(4)?,
            line_start: row.get(5)?,
            line_end: row.get(6)?,
            signature: row.get(7).ok(),
            doc_comment: row.get(8).ok(),
            churn_count: row.get(9).unwrap_or(0),
            language: row.get(10).unwrap_or_default(),
            rrf_score: 0.0,
            related_commits: vec![],
        };
        r.related_commits = fetch_related_commits(conn, &id, 5).await?;
        results.push(r);
    }
    Ok(results)
}

/// List the most-changed symbols (hotspots).
pub async fn list_hotspots(
    conn: &libsql::Connection,
    min_churn: i64,
    limit: usize,
) -> Result<Vec<CodeResult>> {
    let mut rows = conn.query(
        "SELECT id, symbol_name, qualified_name, symbol_kind, file_path,
                line_start, line_end, signature, doc_comment, churn_count, language
         FROM index_chunks
         WHERE churn_count >= ?1
         ORDER BY churn_count DESC
         LIMIT ?2",
        params![min_churn, limit as i64],
    ).await?;

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
            signature: row.get(7).ok(),
            doc_comment: row.get(8).ok(),
            churn_count: row.get(9).unwrap_or(0),
            language: row.get(10).unwrap_or_default(),
            rrf_score: 0.0,
            related_commits: vec![],
        });
    }
    Ok(results)
}

/// Retrieve overall index statistics.
pub async fn index_stats(conn: &libsql::Connection) -> Result<(i64, i64)> {
    let mut rows = conn.query(
        "SELECT COUNT(*) FROM index_files",
        params![],
    ).await?;
    let files: i64 = rows.next().await?
        .map(|r| r.get::<i64>(0).unwrap_or(0))
        .unwrap_or(0);

    let mut rows2 = conn.query(
        "SELECT COUNT(*) FROM index_chunks",
        params![],
    ).await?;
    let chunks: i64 = rows2.next().await?
        .map(|r| r.get::<i64>(0).unwrap_or(0))
        .unwrap_or(0);

    Ok((files, chunks))
}

/// Format a CodeResult for display to the agent.
pub fn format_result(r: &CodeResult, verbose: bool) -> String {
    let mut out = format!(
        "[{}] {}:{}-{} {}\n",
        r.symbol_kind, r.file_path, r.line_start, r.line_end, r.qualified_name
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
    if !r.related_commits.is_empty() {
        out.push_str(&format!(
            "History: {}\n",
            r.related_commits.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(", ")
        ));
    }
    out
}

pub(crate) fn build_fts_query(query: &str) -> String {
    query
        .split_whitespace()
        .filter_map(|w| {
            let clean: String = w.chars().filter(|c| c.is_alphanumeric() || *c == '_').collect();
            if clean.is_empty() { None } else { Some(format!("\"{clean}\"*")) }
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
        assert!(q.contains("\"index\"*"));
        assert!(q.contains("\"file\"*"));
    }

    #[test]
    fn fts_strips_special_chars() {
        // Dots, dashes, and other non-alphanumeric chars stripped
        let q = build_fts_query("file.path foo-bar");
        // "file.path" → "filepath" (dot removed)
        assert!(q.contains("\"filepath\"*"));
        // "foo-bar" → "foobar" (dash removed)
        assert!(q.contains("\"foobar\"*"));
    }

    #[test]
    fn fts_empty_tokens_dropped() {
        // A query of only special chars produces no terms
        let q = build_fts_query("... --- !!!");
        assert!(q.is_empty());
    }

    #[test]
    fn fts_single_term() {
        let q = build_fts_query("ownership");
        assert_eq!(q, "\"ownership\"*");
    }

    #[test]
    fn fts_underscores_preserved() {
        // Underscores are alphanumeric-adjacent and kept
        let q = build_fts_query("my_function");
        assert!(q.contains("\"my_function\"*"));
    }

    #[test]
    fn fts_empty_input() {
        assert!(build_fts_query("").is_empty());
        assert!(build_fts_query("   ").is_empty());
    }
}
