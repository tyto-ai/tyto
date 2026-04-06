use anyhow::Result;
use chrono::Utc;
use libsql::params;
use std::collections::HashMap;

use crate::embed::Embedder;

const RRF_K: f64 = 60.0;

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
    pub score: f64,
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

pub async fn search(
    conn: &libsql::Connection,
    embedder: &mut Embedder,
    query: &str,
    project_id: &str,
    limit: usize,
) -> Result<Vec<CompactResult>> {
    let embedding = embedder.embed(query)?;
    let blob = floats_to_blob(&embedding);

    // Stream A: vector search
    let mut vector_ranks: HashMap<String, usize> = HashMap::new();
    {
        let mut rows = conn
            .query(
                "SELECT m.id
                 FROM memories m
                 JOIN memory_vectors v ON v.memory_id = m.id
                 WHERE m.project_id = ?1 AND m.status = 'active'
                 ORDER BY vector_distance_cos(v.embedding, vector32(?2))
                 LIMIT ?3",
                params![project_id.to_string(), blob, (limit * 2) as i64],
            )
            .await?;
        let mut rank = 0usize;
        while let Some(row) = rows.next().await? {
            let id: String = row.get(0)?;
            vector_ranks.insert(id, rank);
            rank += 1;
        }
    }

    // Stream B: BM25 full-text search
    let mut bm25_ranks: HashMap<String, usize> = HashMap::new();
    {
        let fts_query = build_fts_query(query);
        let mut rows = conn
            .query(
                "SELECT m.id
                 FROM memories m
                 JOIN memories_fts ON memories_fts.rowid = m.rowid
                 WHERE memories_fts MATCH ?1
                   AND m.project_id = ?2
                   AND m.status = 'active'
                 ORDER BY bm25(memories_fts)
                 LIMIT ?3",
                params![fts_query, project_id.to_string(), (limit * 2) as i64],
            )
            .await?;
        let mut rank = 0usize;
        while let Some(row) = rows.next().await? {
            let id: String = row.get(0)?;
            bm25_ranks.insert(id, rank);
            rank += 1;
        }
    }

    // Collect all candidate IDs
    let mut all_ids: Vec<String> = vector_ranks.keys().cloned().collect();
    for id in bm25_ranks.keys() {
        if !vector_ranks.contains_key(id) {
            all_ids.push(id.clone());
        }
    }

    if all_ids.is_empty() {
        return Ok(vec![]);
    }

    // Fetch metadata for scoring - query each ID individually to avoid
    // dynamic IN clause parameter binding complexity
    let now = Utc::now();
    let mut scored: Vec<CompactResult> = Vec::new();

    for id in &all_ids {
        let mut rows = conn
            .query(
                "SELECT id, type, title, created_at, importance, access_count, last_accessed, source
                 FROM memories WHERE id = ?1",
                params![id.clone()],
            )
            .await?;

        if let Some(row) = rows.next().await? {
            let id: String = row.get(0)?;
            let memory_type: String = row.get(1)?;
            let title: String = row.get(2)?;
            let created_at: String = row.get(3)?;
            let importance: f64 = row.get(4)?;
            let access_count: i64 = row.get(5)?;
            let last_accessed: Option<String> = row.get(6).ok();
            let source: String = row.get(7).unwrap_or_else(|_| "realtime".to_string());

            let days = days_since(last_accessed.as_deref().unwrap_or(&created_at), &now);
            let ret = retention_score(&memory_type, importance, days, access_count, &source);

            let rrf_v = vector_ranks
                .get(&id)
                .map(|&r| 1.0 / (RRF_K + r as f64))
                .unwrap_or(0.0);
            let rrf_b = bm25_ranks
                .get(&id)
                .map(|&r| 1.0 / (RRF_K + r as f64))
                .unwrap_or(0.0);
            let boost = source_boost(query, &memory_type);
            let score = (rrf_v + rrf_b) * ret * boost;

            scored.push(CompactResult { id, memory_type, title, created_at, score });
        }
    }

    scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);

    // Increment access counts
    for r in &scored {
        let _ = conn
            .execute(
                "UPDATE memories SET access_count = access_count + 1, last_accessed = ?1 WHERE id = ?2",
                params![now.to_rfc3339(), r.id.clone()],
            )
            .await;
    }

    Ok(scored)
}

pub async fn get_full(conn: &libsql::Connection, id: &str) -> Result<Option<FullMemory>> {
    let mut rows = conn
        .query(
            "SELECT id, project_id, type, title, content, facts, tags,
                    importance, access_count, pinned, status, created_at, updated_at
             FROM memories WHERE id = ?1",
            params![id.to_string()],
        )
        .await?;

    let row = match rows.next().await? {
        Some(r) => r,
        None => return Ok(None),
    };

    // Increment access count
    let _ = conn
        .execute(
            "UPDATE memories SET access_count = access_count + 1, last_accessed = ?1 WHERE id = ?2",
            params![Utc::now().to_rfc3339(), id.to_string()],
        )
        .await;

    Ok(Some(FullMemory {
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
    }))
}

/// BM25-only search: no embedding required. Used by inject --type prompt to avoid
/// loading the ONNX model on every UserPromptSubmit hook invocation.
pub async fn search_bm25(
    conn: &libsql::Connection,
    query: &str,
    project_id: &str,
    limit: usize,
) -> Result<Vec<CompactResult>> {
    let now = Utc::now();
    let fts_query = build_fts_query(query);
    let mut rows = conn
        .query(
            "SELECT m.id, m.type, m.title, m.created_at, m.importance,
                    m.access_count, m.last_accessed, m.source
             FROM memories m
             JOIN memories_fts ON memories_fts.rowid = m.rowid
             WHERE memories_fts MATCH ?1
               AND m.project_id = ?2
               AND m.status = 'active'
             ORDER BY bm25(memories_fts)
             LIMIT ?3",
            params![fts_query, project_id.to_string(), limit as i64],
        )
        .await?;

    let mut results: Vec<CompactResult> = Vec::new();
    while let Some(row) = rows.next().await? {
        let memory_type: String = row.get(1)?;
        let created_at: String = row.get(3)?;
        let importance: f64 = row.get(4)?;
        let access_count: i64 = row.get(5)?;
        let last_accessed: Option<String> = row.get(6).ok();
        let source: String = row.get(7).unwrap_or_else(|_| "realtime".to_string());
        let days = days_since(last_accessed.as_deref().unwrap_or(&created_at), &now);
        results.push(CompactResult {
            id: row.get(0)?,
            title: row.get(2)?,
            created_at,
            score: retention_score(&memory_type, importance, days, access_count, &source),
            memory_type,
        });
    }

    // Update access counts
    for r in &results {
        let _ = conn
            .execute(
                "UPDATE memories SET access_count = access_count + 1, last_accessed = ?1 WHERE id = ?2",
                params![now.to_rfc3339(), r.id.clone()],
            )
            .await;
    }

    Ok(results)
}

pub async fn list(
    conn: &libsql::Connection,
    project_id: &str,
    filter_type: Option<&str>,
    filter_tags: &[String],
    limit: usize,
    min_importance: f64,
) -> Result<Vec<CompactResult>> {
    let now = Utc::now();

    // Fetch a ceiling of candidates ordered by recency; scoring and importance
    // filtering happen in-process. The ceiling is generous so important older
    // memories are not dropped before scoring.
    let ceiling = (limit * 4).max(200) as i64;
    let mut rows = conn
        .query(
            "SELECT id, type, title, created_at, importance, access_count, last_accessed, source
             FROM memories
             WHERE project_id = ?1 AND status = 'active' AND importance >= ?2
             ORDER BY created_at DESC
             LIMIT ?3",
            params![project_id.to_string(), min_importance, ceiling],
        )
        .await?;

    let mut results: Vec<CompactResult> = Vec::new();
    while let Some(row) = rows.next().await? {
        let memory_type: String = row.get(1)?;

        if let Some(ft) = filter_type
            && memory_type != ft
        {
            continue;
        }

        let created_at: String = row.get(3)?;
        let importance: f64 = row.get(4)?;
        let access_count: i64 = row.get(5)?;
        let last_accessed: Option<String> = row.get(6).ok();
        let source: String = row.get(7).unwrap_or_else(|_| "realtime".to_string());
        let days = days_since(last_accessed.as_deref().unwrap_or(&created_at), &now);

        results.push(CompactResult {
            id: row.get(0)?,
            title: row.get(2)?,
            created_at,
            score: retention_score(&memory_type, importance, days, access_count, &source),
            memory_type,
        });
    }

    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    results.truncate(limit);

    // tag filtering is post-query for simplicity (tags stored as JSON)
    if !filter_tags.is_empty() {
        results.retain(|_| true); // TODO: fetch tags and filter - deferred
    }

    Ok(results)
}

fn build_fts_query(query: &str) -> String {
    // Wrap each word so FTS5 treats them as prefix matches
    query
        .split_whitespace()
        .map(|w| format!("{w}*"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn days_since(datetime_str: &str, now: &chrono::DateTime<Utc>) -> f64 {
    chrono::DateTime::parse_from_rfc3339(datetime_str)
        .map(|dt| (*now - dt.with_timezone(&Utc)).num_seconds() as f64 / 86400.0)
        .unwrap_or(0.0)
}

fn floats_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}
