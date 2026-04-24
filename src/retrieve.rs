use anyhow::Result;
use chrono::Utc;
use turso::{params_from_iter, Connection, Value};
use std::collections::HashMap;

use crate::embed;

const RRF_K: f64 = 60.0;

/// Escape a user query string for use in a SQLite LIKE pattern with ESCAPE '\'.
fn like_escape(s: &str) -> String {
    s.replace('\\', r"\\").replace('%', r"\%").replace('_', r"\_")
}

/// Type salience weights for retention scoring.
fn type_weight(memory_type: &str) -> f64 {
    match memory_type {
        "decision"         => 0.90,
        "gotcha"           => 0.88,
        "preference"       => 0.85,
        "problem-solution" => 0.82,
        "how-it-works"     => 0.75,
        "trade-off"        => 0.72,
        "workflow"         => 0.68,
        "discovery"        => 0.65,
        "what-changed"     => 0.60,
        "fact"             => 0.55,
        _                  => 0.50,
    }
}

fn retention_score(
    memory_type: &str,
    importance: f64,
    days_since_access: f64,
    access_count: i64,
    source: &str,
) -> f64 {
    let salience = type_weight(memory_type) * importance;
    let decay = (-0.01 * days_since_access).exp();
    let freq = 0.03 * ((access_count + 1) as f64).ln();
    // Memories stored during session-start review have broader context and hindsight.
    let review_boost = if source == "reviewed" { 0.1 } else { 0.0 };
    (salience * decay + freq.min(0.3) + review_boost).min(1.0)
}

/// Boost applied to type weights based on query intent keywords.
fn source_boost(query: &str, memory_type: &str) -> f64 {
    let q = query.to_lowercase();
    if (q.contains("why") || q.contains("reason") || q.contains("decision"))
        && matches!(memory_type, "decision" | "trade-off")
    {
        return 1.3;
    }
    if (q.contains("how") || q.contains("steps") || q.contains("workflow"))
        && matches!(memory_type, "workflow" | "how-it-works")
    {
        return 1.3;
    }
    if (q.contains("error") || q.contains("broken") || q.contains("fail"))
        && matches!(memory_type, "gotcha" | "problem-solution")
    {
        return 1.3;
    }
    if (q.contains("changed") || q.contains("update") || q.contains("new"))
        && memory_type == "what-changed"
    {
        return 1.3;
    }
    1.0
}

#[derive(Debug)]
pub struct CompactResult {
    pub id: String,
    pub memory_type: String,
    pub title: String,
    pub created_at: String,
    pub importance: f64,
    pub score: f64,
    /// Character length of the full content, for budget-aware fetching decisions.
    pub content_len: usize,
    /// Raw JSON array of fact strings from the DB; None if the column is NULL.
    pub facts_json: Option<String>,
    /// Raw JSON array of tag strings from the DB; None if the column is NULL.
    pub tags_json: Option<String>,
}

#[derive(Debug)]
pub struct FullMemory {
    pub id: String,
    pub project_id: String,
    pub memory_type: String,
    pub title: String,
    pub content: String,
    pub facts: Option<String>,
    pub tags: Option<String>,
    pub importance: f64,
    pub access_count: i64,
    pub pinned: bool,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Hybrid RRF search combining vector similarity and keyword search.
///
/// `embedding` must be pre-computed by the caller before any locks are held,
/// so the embedding step does not block concurrent tool calls.
pub async fn search(
    conn: &Connection,
    embedding: Vec<f32>,
    query: &str,
    project_id: &str,
    limit: usize,
) -> Result<Vec<CompactResult>> {
    let t_total = std::time::Instant::now();
    let blob = embed::floats_to_blob(&embedding);

    // Stream A: vector search (filtered to current model only; memories lacking a
    // current-model vector fall through to keyword search gracefully during re-embedding).
    // Also captures the top cosine distance as an absolute relevance gate: if the
    // nearest neighbour is too far away, the query has no relevant memories at all.
    const MAX_COSINE_DIST: f64 = 0.38;
    let mut vector_ranks: HashMap<String, usize> = HashMap::new();
    {
        let t = std::time::Instant::now();
        let mut rows = conn
            .query(
                "SELECT m.id, vector_distance_cos(v.embedding, vector32(?3)) as dist
                 FROM memories m
                 JOIN memory_vectors v ON v.memory_id = m.id
                 WHERE m.project_id = ?1 AND m.status = 'active' AND v.embed_model = ?2
                 ORDER BY dist
                 LIMIT ?4",
                (project_id.to_string(), embed::model_id(), blob, (limit * 2) as i64),
            )
            .await?;
        let mut rank = 0usize;
        let mut top_dist: Option<f64> = None;
        while let Some(row) = rows.next().await? {
            let id: String = row.get(0)?;
            let dist: f64 = row.get(1).unwrap_or(1.0);
            if top_dist.is_none() { top_dist = Some(dist); }
            vector_ranks.insert(id, rank);
            rank += 1;
        }
        let top_dist = top_dist.unwrap_or(1.0);
        tracing::debug!(elapsed_ms = t.elapsed().as_millis(), results = vector_ranks.len(), top_cosine_dist = format!("{top_dist:.3}"), "vector search");
        if top_dist > MAX_COSINE_DIST {
            tracing::debug!(top_cosine_dist = format!("{top_dist:.3}"), threshold = MAX_COSINE_DIST, "cosine gate: no relevant memories");
            return Ok(vec![]);
        }
    }

    // Stream B: Keyword search (fallback for FTS5 which is not supported by Limbo yet)
    let mut kw_ranks: HashMap<String, usize> = HashMap::new();
    {
        let t = std::time::Instant::now();
        let kw_query = format!("%{}%", like_escape(query));
        let mut rows = conn
            .query(
                "SELECT id
                 FROM memories
                 WHERE (title LIKE ?1 ESCAPE '\\' OR content LIKE ?1 ESCAPE '\\' OR facts LIKE ?1 ESCAPE '\\')
                   AND project_id = ?2
                   AND status = 'active'
                 LIMIT ?3",
                (kw_query, project_id.to_string(), (limit * 2) as i64),
            )
            .await?;
        let mut rank = 0usize;
        while let Some(row) = rows.next().await? {
            let id: String = row.get(0)?;
            kw_ranks.insert(id, rank);
            rank += 1;
        }
        tracing::debug!(elapsed_ms = t.elapsed().as_millis(), results = kw_ranks.len(), "keyword search");
    }

    // Collect all candidate IDs
    let mut all_ids: Vec<String> = vector_ranks.keys().cloned().collect();
    for id in kw_ranks.keys() {
        if !vector_ranks.contains_key(id) {
            all_ids.push(id.clone());
        }
    }

    if all_ids.is_empty() {
        tracing::debug!(elapsed_ms = t_total.elapsed().as_millis(), "search total (no candidates)");
        return Ok(vec![]);
    }

    // Fetch metadata for all candidates in a single IN query.
    let t = std::time::Instant::now();
    let now = Utc::now();
    let placeholders = all_ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let meta_sql = format!(
        "SELECT id, type, title, created_at, importance, access_count, last_accessed, source,
                length(content), facts, tags
         FROM memories WHERE id IN ({placeholders}) AND status = 'active'"
    );
    let mut rows = conn
        .query(&meta_sql, params_from_iter(all_ids.iter().cloned().map(Value::Text)))
        .await?;

    let mut scored: Vec<CompactResult> = Vec::new();
    while let Some(row) = rows.next().await? {
        let id: String = row.get(0)?;
        let memory_type: String = row.get(1)?;
        let title: String = row.get(2)?;
        let created_at: String = row.get(3)?;
        let importance: f64 = row.get(4)?;
        let access_count: i64 = row.get(5)?;
        let last_accessed: Option<String> = row.get(6).ok();
        let source: String = row.get(7).unwrap_or_else(|_| "realtime".to_string());
        let content_len: i64 = row.get(8).unwrap_or(0);
        let facts_json: Option<String> = row.get(9).ok();
        let tags_json: Option<String> = row.get(10).ok();

        let days = days_since(last_accessed.as_deref().unwrap_or(&created_at), &now);
        let ret = retention_score(&memory_type, importance, days, access_count, &source);

        let rrf_v = vector_ranks
            .get(&id)
            .map(|&r| 1.0 / (RRF_K + r as f64))
            .unwrap_or(0.0);
        let rrf_kw = kw_ranks
            .get(&id)
            .map(|&r| 1.0 / (RRF_K + r as f64))
            .unwrap_or(0.0);
        let boost = source_boost(query, &memory_type);
        // Relevance-primary scoring: retention boosts up to +SALIENCE_ALPHA rather than
        // multiplying, so a low-retention memory cannot be suppressed below its raw RRF.
        // TUNING: SALIENCE_ALPHA controls how much a high-retention memory can outrank
        // an equally-relevant low-retention one (0.3 = up to 30% boost).
        const SALIENCE_ALPHA: f64 = 0.3;
        let score = (rrf_v + rrf_kw) * boost * (1.0 + SALIENCE_ALPHA * ret);

        scored.push(CompactResult { id, memory_type, title, created_at, importance, score, content_len: content_len as usize, facts_json, tags_json });
    }
    tracing::debug!(elapsed_ms = t.elapsed().as_millis(), candidates = scored.len(), "metadata fetch");

    scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);

    tracing::debug!(elapsed_ms = t_total.elapsed().as_millis(), results = scored.len(), "search total");
    Ok(scored)
}

/// Fetch multiple memories in a single query. Returns only found memories;
/// callers can detect missing IDs by comparing against the input set.
pub async fn get_full_batch(
    conn: &Connection,
    ids: &[String],
    project_id: &str,
) -> Result<Vec<FullMemory>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!(
        "SELECT id, project_id, type, title, content, facts, tags,
                importance, access_count, pinned, status, created_at, updated_at
         FROM memories WHERE id IN ({placeholders}) AND project_id = ?"
    );
    let select_params: Vec<Value> = ids.iter().cloned().map(Value::Text)
        .chain(std::iter::once(Value::Text(project_id.to_string())))
        .collect();
    let mut rows = conn
        .query(&sql, params_from_iter(select_params))
        .await?;

    let mut memories = Vec::new();
    while let Some(row) = rows.next().await? {
        memories.push(FullMemory {
            id: row.get(0)?,
            project_id: row.get(1)?,
            memory_type: row.get(2)?,
            title: row.get(3)?,
            content: row.get(4)?,
            facts: row.get(5).ok(),
            tags: row.get(6).ok(),
            importance: row.get(7)?,
            access_count: row.get(8)?,
            pinned: row.get::<i64>(9)? != 0,
            status: row.get(10)?,
            created_at: row.get(11)?,
            updated_at: row.get(12)?,
        });
    }

    // Increment access count for all fetched memories in one query.
    let update_sql = format!(
        "UPDATE memories SET access_count = access_count + 1, last_accessed = ? \
         WHERE id IN ({placeholders}) AND project_id = ?"
    );
    let update_params: Vec<Value> = std::iter::once(Value::Text(Utc::now().to_rfc3339()))
        .chain(ids.iter().cloned().map(Value::Text))
        .chain(std::iter::once(Value::Text(project_id.to_string())))
        .collect();
    let _ = conn.execute(&update_sql, params_from_iter(update_params)).await;

    Ok(memories)
}

/// Fetch stored embedding vectors for a batch of memory IDs.
/// Returns `(memory_id, embedding)` pairs for use in per-vector similarity search.
pub async fn fetch_embeddings(
    conn: &Connection,
    ids: &[String],
    project_id: &str,
) -> Result<Vec<(String, Vec<f32>)>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!(
        "SELECT mv.memory_id, mv.embedding FROM memory_vectors mv \
         JOIN memories m ON m.id = mv.memory_id \
         WHERE mv.memory_id IN ({placeholders}) AND m.project_id = ?"
    );
    let params: Vec<Value> = ids.iter().cloned().map(Value::Text)
        .chain(std::iter::once(Value::Text(project_id.to_string())))
        .collect();
    let mut rows = conn.query(&sql, params_from_iter(params)).await?;
    let mut result = Vec::new();
    while let Some(row) = rows.next().await? {
        let id: String = row.get(0)?;
        let blob: Vec<u8> = row.get(1)?;
        result.push((id, embed::blob_to_floats(&blob)));
    }
    Ok(result)
}

/// Pin or unpin a batch of memories in a single query. Returns the number of rows updated.
pub async fn pin_batch(conn: &Connection, ids: &[String], project_id: &str, pin: bool) -> Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!(
        "UPDATE memories SET pinned = ? WHERE id IN ({placeholders}) AND status != 'deleted' AND project_id = ?"
    );
    let params: Vec<Value> = std::iter::once(Value::Integer(if pin { 1 } else { 0 }))
        .chain(ids.iter().cloned().map(Value::Text))
        .chain(std::iter::once(Value::Text(project_id.to_string())))
        .collect();
    Ok(conn.execute(&sql, params_from_iter(params)).await?)
}

/// Soft-delete a batch of memories in a single query. Returns the number of rows updated.
pub async fn delete_batch(conn: &Connection, ids: &[String], project_id: &str) -> Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!(
        "UPDATE memories SET status = 'deleted' WHERE id IN ({placeholders}) AND project_id = ?"
    );
    let params: Vec<Value> = ids.iter().cloned().map(Value::Text)
        .chain(std::iter::once(Value::Text(project_id.to_string())))
        .collect();
    Ok(conn.execute(&sql, params_from_iter(params)).await?)
}

pub async fn list(
    conn: &Connection,
    project_id: &str,
    filter_type: Option<&str>,
    filter_tags: &[String],
    limit: usize,
    min_importance: f64,
) -> Result<Vec<CompactResult>> {
    let now = Utc::now();

    // Fetch a ceiling of candidates ordered by recency; scoring and tag filtering
    // happen in-process. The ceiling is generous so important older memories
    // are not dropped before scoring. Type filter applied in SQL when present.
    let ceiling = (limit * 4).max(200) as i64;
    let (type_clause, type_param) = match filter_type {
        Some(t) => (" AND type = ?4", Some(t.to_string())),
        None => ("", None),
    };
    let sql = format!(
        "SELECT id, type, title, created_at, importance, access_count, last_accessed, source, tags,
                length(content), facts
         FROM memories
         WHERE project_id = ?1 AND status = 'active' AND importance >= ?2
         {type_clause}
         ORDER BY created_at DESC
         LIMIT ?3"
    );
    let mut rows = if let Some(ref tp) = type_param {
        conn.query(
            &sql,
            (project_id.to_string(), min_importance, ceiling, tp.clone()),
        )
        .await?
    } else {
        conn.query(
            &sql,
            (project_id.to_string(), min_importance, ceiling),
        )
        .await?
    };

    let mut candidates: Vec<CompactResult> = Vec::new();
    while let Some(row) = rows.next().await? {
        let memory_type: String = row.get(1)?;

        let created_at: String = row.get(3)?;
        let importance: f64 = row.get(4)?;
        let access_count: i64 = row.get(5)?;
        let last_accessed: Option<String> = row.get(6).ok();
        let source: String = row.get(7).unwrap_or_else(|_| "realtime".to_string());
        let tags_json: Option<String> = row.get(8).ok();
        let content_len: i64 = row.get(9).unwrap_or(0);
        let facts_json: Option<String> = row.get(10).ok();
        let days = days_since(last_accessed.as_deref().unwrap_or(&created_at), &now);

        candidates.push(CompactResult {
            id: row.get(0)?,
            title: row.get(2)?,
            created_at,
            score: retention_score(&memory_type, importance, days, access_count, &source),
            importance,
            memory_type,
            content_len: content_len as usize,
            facts_json,
            tags_json,
        });
    }

    candidates.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

    // Apply tag filter after scoring; take limit after filtering so callers
    // get up to `limit` results even when tags reduce the candidate set.
    let results: Vec<CompactResult> = candidates
        .into_iter()
        .filter(|r| {
            if filter_tags.is_empty() {
                return true;
            }
            let tags: Vec<String> = r.tags_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();
            filter_tags.iter().all(|t| tags.contains(t))
        })
        .take(limit)
        .collect();

    Ok(results)
}

// Kept for when Limbo gains FTS5 support; current callers use LIKE instead.
#[allow(dead_code)]
fn build_fts_query(query: &str) -> String {
    // FTS5 tokens may only contain alphanumerics and underscores.
    // Strip other characters (e.g. "." in "install.rs") to avoid syntax errors.
    //
    // Each token is wrapped in double quotes before the "*" suffix: `"token"*`
    // This is required because FTS5 treats bare words like "and", "or", "not", "near"
    // as query operators, producing a syntax error when followed by "*".
    // Quoting a single term (`"token"*`) is valid FTS5 prefix-phrase syntax and
    // is semantically identical to `token*` for non-reserved words.
    query
        .split_whitespace()
        .filter_map(|w| {
            let clean: String = w.chars().filter(|c| c.is_alphanumeric() || *c == '_').collect();
            if clean.is_empty() { None } else { Some(format!("\"{clean}\"*")) }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

const STALE_RETENTION_THRESHOLD: f64 = 0.15;
const STALE_MIN_AGE_DAYS: f64 = 7.0;

pub struct StaleMemory {
    pub id: String,
    pub memory_type: String,
    pub title: String,
    pub importance: f64,
    pub score: f64,
    pub days_since_access: f64,
}

/// Return memories eligible for eviction: not pinned, older than STALE_MIN_AGE_DAYS, retention score below threshold.
pub async fn list_stale(
    conn: &Connection,
    project_id: &str,
) -> Result<Vec<StaleMemory>> {
    let now = Utc::now();
    let cutoff = (now - chrono::Duration::days(STALE_MIN_AGE_DAYS as i64)).to_rfc3339();
    let mut rows = conn
        .query(
            "SELECT id, type, title, importance, access_count, last_accessed, created_at, source
             FROM memories
             WHERE project_id = ?1
               AND status = 'active'
               AND pinned = 0
               AND created_at <= ?2",
            (project_id.to_string(), cutoff),
        )
        .await?;

    let mut stale = Vec::new();
    while let Some(row) = rows.next().await? {
        let memory_type: String = row.get(1)?;
        let importance: f64 = row.get(3)?;
        let access_count: i64 = row.get(4)?;
        let last_accessed: Option<String> = row.get(5).ok();
        let created_at: String = row.get(6)?;
        let source: String = row.get(7).unwrap_or_else(|_| "realtime".to_string());
        let days = days_since(last_accessed.as_deref().unwrap_or(&created_at), &now);
        let score = retention_score(&memory_type, importance, days, access_count, &source);
        if score < STALE_RETENTION_THRESHOLD {
            stale.push(StaleMemory {
                id: row.get(0)?,
                title: row.get(2)?,
                importance,
                score,
                memory_type,
                days_since_access: days,
            });
        }
    }

    stale.sort_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal));
    Ok(stale)
}

/// Hard-delete all stale memories (those returned by list_stale). Returns count deleted.
pub async fn evict_stale(
    conn: &Connection,
    project_id: &str,
) -> Result<u64> {
    let candidates = list_stale(conn, project_id).await?;
    if candidates.is_empty() {
        return Ok(0);
    }
    let ids: Vec<String> = candidates.into_iter().map(|m| m.id).collect();
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    // Prepend project_id param; WHERE clause double-checks project scope for safety.
    let sql = format!(
        "DELETE FROM memories WHERE project_id = ?1 AND id IN ({placeholders})"
    );
    let params: Vec<Value> = std::iter::once(Value::Text(project_id.to_string()))
        .chain(ids.into_iter().map(Value::Text))
        .collect();
    Ok(conn.execute(&sql, params_from_iter(params)).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_weight_known_and_unknown() {
        assert_eq!(type_weight("decision"), 0.90);
        assert_eq!(type_weight("fact"), 0.55);
        assert_eq!(type_weight("unknown_type"), 0.50);
    }

    #[test]
    fn retention_score_higher_importance_scores_higher() {
        let low = retention_score("decision", 0.3, 0.0, 0, "realtime");
        let high = retention_score("decision", 0.9, 0.0, 0, "realtime");
        assert!(high > low);
    }

    #[test]
    fn retention_score_reviewed_boost() {
        let normal = retention_score("decision", 0.7, 0.0, 0, "realtime");
        let reviewed = retention_score("decision", 0.7, 0.0, 0, "reviewed");
        assert!(reviewed > normal);
    }

    #[test]
    fn source_boost_why_query_matches_decision() {
        assert_eq!(source_boost("why did we choose this", "decision"), 1.3);
        assert_eq!(source_boost("why did we choose this", "what-changed"), 1.0);
    }

    #[test]
    fn source_boost_error_query_matches_gotcha() {
        assert_eq!(source_boost("error in prod", "gotcha"), 1.3);
        assert_eq!(source_boost("error in prod", "decision"), 1.0);
    }

    #[test]
    fn build_fts_query_appends_wildcards() {
        assert_eq!(build_fts_query("hello world"), "\"hello\"* \"world\"*");
    }

    #[test]
    fn build_fts_query_empty_input() {
        assert_eq!(build_fts_query(""), "");
    }

    #[test]
    fn build_fts_query_strips_dots() {
        // "install.rs" -> "\"installrs\"*", "settings.json" -> "\"settingsjson\"*"
        assert_eq!(build_fts_query("install.rs settings.json"), "\"installrs\"* \"settingsjson\"*");
    }

    #[test]
    fn build_fts_query_drops_punctuation_only_tokens() {
        assert_eq!(build_fts_query("hello ... world"), "\"hello\"* \"world\"*");
    }

    #[test]
    fn build_fts_query_quotes_reserved_words() {
        // "and", "or", "not", "near" are FTS5 operators; quoting prevents syntax errors.
        assert_eq!(build_fts_query("hello and world"), "\"hello\"* \"and\"* \"world\"*");
        assert_eq!(build_fts_query("not this"), "\"not\"* \"this\"*");
    }
}

fn days_since(datetime_str: &str, now: &chrono::DateTime<Utc>) -> f64 {
    chrono::DateTime::parse_from_rfc3339(datetime_str)
        .map(|dt| (*now - dt.with_timezone(&Utc)).num_seconds() as f64 / 86400.0)
        .unwrap_or(0.0)
}
