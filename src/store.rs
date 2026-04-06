use anyhow::Result;
use chrono::Utc;
use libsql::params;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{embed::Embedder, sanitize};

pub struct StoreRequest {
    pub content: String,
    pub memory_type: String,
    pub title: String,
    pub tags: Vec<String>,
    pub topic_key: Option<String>,
    pub project_id: String,
    pub session_id: String,
    /// None lets the DB default ('realtime') apply. Set Some("reviewed") during
    /// session-start review to receive a retention boost.
    pub importance: Option<f32>,
    pub facts: Vec<String>,
    /// None lets the DB default ('realtime') apply. Set Some("reviewed") during
    /// session-start review to receive a retention boost.
    pub source: Option<String>,
}

pub struct StoreResult {
    pub id: String,
    pub upserted: bool,
}

/// Shared write lock - prevents TOCTOU races when multiple hooks fire concurrently.
pub type WriteLock = Arc<Mutex<()>>;

pub fn new_write_lock() -> WriteLock {
    Arc::new(Mutex::new(()))
}

pub async fn store_memory(
    conn: &libsql::Connection,
    embedder: &mut Embedder,
    lock: &WriteLock,
    req: StoreRequest,
    dedup_window_secs: i64,
) -> Result<StoreResult> {
    let content = sanitize::sanitize(&req.content);
    let title = sanitize::sanitize(&req.title);

    let hash = content_hash(&content);
    let now = Utc::now();
    let now_str = now.to_rfc3339();

    let importance = req.importance.unwrap_or(0.5) as f64;
    let embedding = embedder.embed(&format!("{title} {content}"))?;
    let embedding_blob = floats_to_blob(&embedding);

    let tags_json = serde_json::to_string(&req.tags)?;
    let facts_json = if req.facts.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&req.facts)?)
    };

    let _guard = lock.lock().await;

    // Content hash dedup: skip if same content seen within window for this session
    let recent: u32 = conn
        .query(
            "SELECT COUNT(*) FROM memories
             WHERE content_hash = ?1
               AND session_id   = ?2
               AND (julianday(?3) - julianday(created_at)) * 86400 < ?4",
            params![hash.clone(), req.session_id.clone(), now_str.clone(), dedup_window_secs],
        )
        .await?
        .next()
        .await?
        .map(|r| r.get::<u32>(0).unwrap_or(0))
        .unwrap_or(0);

    if recent > 0 {
        // Return the existing memory's ID
        let existing_id: String = conn
            .query(
                "SELECT id FROM memories WHERE content_hash = ?1 AND session_id = ?2 LIMIT 1",
                params![hash, req.session_id],
            )
            .await?
            .next()
            .await?
            .map(|r| r.get::<String>(0).unwrap_or_default())
            .unwrap_or_default();
        return Ok(StoreResult { id: existing_id, upserted: false });
    }

    // Topic key upsert
    if let Some(ref topic_key) = req.topic_key {
        let existing: Option<String> = conn
            .query(
                "SELECT id FROM memories WHERE project_id = ?1 AND topic_key = ?2 LIMIT 1",
                params![req.project_id.clone(), topic_key.clone()],
            )
            .await?
            .next()
            .await?
            .and_then(|r| r.get::<String>(0).ok());

        if let Some(ref id) = existing {
            conn.execute(
                "UPDATE memories
                 SET content = ?1, title = ?2, facts = ?3, importance = ?4,
                     content_hash = ?5, updated_at = ?6, source = COALESCE(?7, source)
                 WHERE id = ?8",
                params![
                    content,
                    title,
                    facts_json,
                    importance,
                    hash,
                    now_str.clone(),
                    req.source.clone(),
                    id.clone()
                ],
            )
            .await?;

            // Replace embedding
            conn.execute("DELETE FROM memory_vectors WHERE memory_id = ?1", params![id.clone()])
                .await?;
            conn.execute(
                "INSERT INTO memory_vectors (memory_id, embedding) VALUES (?1, ?2)",
                params![id.clone(), embedding_blob],
            )
            .await?;

            return Ok(StoreResult { id: id.clone(), upserted: true });
        }
    }

    // Insert new memory. source is omitted when None so the DB default ('realtime') applies.
    let id = Uuid::new_v4().to_string();
    if let Some(ref source) = req.source {
        conn.execute(
            "INSERT INTO memories
                (id, project_id, topic_key, type, title, content, facts, tags,
                 importance, session_id, source, created_at, updated_at, content_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                id.clone(), req.project_id, req.topic_key, req.memory_type,
                title, content, facts_json, tags_json,
                importance, req.session_id, source.clone(),
                now_str.clone(), now_str, hash
            ],
        )
        .await?;
    } else {
        conn.execute(
            "INSERT INTO memories
                (id, project_id, topic_key, type, title, content, facts, tags,
                 importance, session_id, created_at, updated_at, content_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                id.clone(), req.project_id, req.topic_key, req.memory_type,
                title, content, facts_json, tags_json,
                importance, req.session_id,
                now_str.clone(), now_str, hash
            ],
        )
        .await?;
    }

    conn.execute(
        "INSERT INTO memory_vectors (memory_id, embedding) VALUES (?1, ?2)",
        params![id.clone(), embedding_blob],
    )
    .await?;

    Ok(StoreResult { id, upserted: false })
}

fn content_hash(content: &str) -> String {
    let mut h = Sha256::new();
    h.update(content.as_bytes());
    hex::encode(h.finalize())
}

fn floats_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}
