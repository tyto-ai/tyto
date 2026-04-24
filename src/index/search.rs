use anyhow::Result;
use std::collections::HashMap;
use tokio::sync::Mutex;
use std::sync::Arc;

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
    pub hotspot_score: f64,
    pub language: String,
    pub rrf_score: f64,
    pub related_commits: Vec<String>,
}

/// Hybrid vector + FTS search over indexed code chunks.
pub async fn search_code(
    conn: &Arc<Mutex<rusqlite::Connection>>,
    embedding: Vec<f32>,
    query: &str,
    limit: usize,
) -> Result<Vec<CodeResult>> {
    let _blob = embed::floats_to_blob(&embedding);
    let _model = embed::model_id();
    let k = (limit * 2) as i64;

    // Stream A: vector search (deferred: rusqlite doesn't have native vector ANN extension)
    
    // Stream B: FTS5 search
    let fts_ranks: HashMap<String, usize> = {
        let fts_q = build_fts_query(query);
        if fts_q.is_empty() {
            HashMap::new()
        } else {
            let conn = Arc::clone(conn);
            tokio::task::spawn_blocking(move || {
                let conn = conn.blocking_lock();
                let mut stmt = conn.prepare(
                    "SELECT c.id
                     FROM index_chunks c
                     JOIN index_chunks_fts ON index_chunks_fts.rowid = c.rowid
                     WHERE index_chunks_fts MATCH ?1
                     ORDER BY bm25(index_chunks_fts)
                     LIMIT ?2"
                )?;
                let rows = stmt.query_map([fts_q, k.to_string()], |row| row.get::<_, String>(0))?;
                let mut ranks = HashMap::new();
                for (i, id) in rows.enumerate() {
                    if let Ok(id) = id {
                        ranks.insert(id, i);
                    }
                }
                Ok::<_, anyhow::Error>(ranks)
            }).await??
        }
    };

    if fts_ranks.is_empty() {
        return Ok(vec![]);
    }

    let all_ids: Vec<String> = fts_ranks.keys().cloned().collect();

    // Fetch metadata in one query
    let scored: Vec<CodeResult> = {
        let placeholders = all_ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT id, symbol_name, qualified_name, symbol_kind, file_path,
                    line_start, line_end, signature, doc_comment, churn_count, hotspot_score, language
             FROM index_chunks WHERE id IN ({placeholders})"
        );
        let conn = Arc::clone(conn);
        let ids_clone = all_ids.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(ids_clone), |row| {
                let id: String = row.get(0)?;
                let rrf_f = fts_ranks.get(&id).map(|&r| 1.0 / (RRF_K + r as f64)).unwrap_or(0.0);
                Ok(CodeResult {
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
                    rrf_score: rrf_f,
                    related_commits: vec![],
                })
            })?;
            let mut results = Vec::new();
            for r in rows {
                results.push(r?);
            }
            Ok::<_, anyhow::Error>(results)
        }).await??
    };

    let mut scored = scored;
    scored.sort_by(|a, b| b.rrf_score.partial_cmp(&a.rrf_score).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);

    // Fetch related commits for top results
    for result in &mut scored {
        result.related_commits = fetch_related_commits_sync(conn, &result.id, 3).await?;
    }

    Ok(scored)
}

async fn fetch_related_commits_sync(
    conn: &Arc<Mutex<rusqlite::Connection>>,
    chunk_id: &str,
    limit: usize,
) -> Result<Vec<String>> {
    let conn = Arc::clone(conn);
    let chunk_id = chunk_id.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = conn.blocking_lock();
        let mut stmt = conn.prepare(
            "SELECT c.message
             FROM index_commits c
             JOIN index_chunk_commits cc ON cc.commit_sha = c.sha
             WHERE cc.chunk_id = ?1
             LIMIT ?2"
        )?;
        let rows = stmt.query_map([chunk_id, limit.to_string()], |row| row.get::<_, String>(0))?;
        let mut msgs = Vec::new();
        for r in rows {
            if let Ok(msg) = r {
                msgs.push(msg);
            }
        }
        Ok::<_, anyhow::Error>(msgs)
    }).await?
}

/// Lookup a symbol by name (and optionally file path).
pub async fn get_symbol(
    conn: &Arc<Mutex<rusqlite::Connection>>,
    name_path: &str,
    file_path: Option<&str>,
) -> Result<Vec<CodeResult>> {
    let name_path = name_path.to_string();
    let file_path = file_path.map(|s| s.to_string());
    let conn_clone = Arc::clone(conn);

    let results = tokio::task::spawn_blocking(move || {
        let conn = conn_clone.blocking_lock();
        let (sql, params): (String, Vec<String>) = if let Some(ref fp) = file_path {
            (
                "SELECT id, symbol_name, qualified_name, symbol_kind, file_path,
                        line_start, line_end, signature, doc_comment, churn_count, hotspot_score, language
                 FROM index_chunks
                 WHERE (symbol_name = ?1 OR qualified_name = ?1 OR qualified_name LIKE ?2)
                   AND file_path = ?3
                 ORDER BY line_start
                 LIMIT 20".to_string(),
                vec![name_path.clone(), format!("%::{name_path}"), fp.clone()],
            )
        } else {
            (
                "SELECT id, symbol_name, qualified_name, symbol_kind, file_path,
                        line_start, line_end, signature, doc_comment, churn_count, hotspot_score, language
                 FROM index_chunks
                 WHERE symbol_name = ?1 OR qualified_name = ?1 OR qualified_name LIKE ?2
                 ORDER BY hotspot_score DESC, churn_count DESC, line_start
                 LIMIT 20".to_string(),
                vec![name_path.clone(), format!("%::{name_path}")],
            )
        };

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params), |row| {
            Ok(CodeResult {
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
                rrf_score: 0.0,
                related_commits: vec![],
            })
        })?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok::<_, anyhow::Error>(results)
    }).await??;

    let mut final_results = results;
    for r in &mut final_results {
        r.related_commits = fetch_related_commits_sync(conn, &r.id, 5).await?;
    }
    Ok(final_results)
}

/// Retrieve overall index statistics.
pub async fn index_stats(conn: &Arc<Mutex<rusqlite::Connection>>) -> Result<(i64, i64)> {
    let conn = Arc::clone(conn);
    tokio::task::spawn_blocking(move || {
        let conn = conn.blocking_lock();
        let files: i64 = conn.query_row("SELECT COUNT(*) FROM index_files", [], |row| row.get(0))?;
        let chunks: i64 = conn.query_row("SELECT COUNT(*) FROM index_chunks", [], |row| row.get(0))?;
        Ok::<(i64, i64), anyhow::Error>((files, chunks))
    }).await?
}

/// Format a CodeResult for display to the agent.
pub fn format_result(r: &CodeResult, verbose: bool) -> String {
    let mut out = format!(
        "[{}] {:.3}  {}:{}-{} {}\n",
        r.symbol_kind, r.rrf_score, r.file_path, r.line_start, r.line_end, r.qualified_name
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
