use anyhow::Result;
use chrono::Utc;
use turso::Connection;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{embed, sanitize};

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
    /// None leaves the pinned flag unchanged (defaults to false on insert).
    pub pinned: Option<bool>,
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

/// Store or upsert a memory.
///
/// `embedding` must be pre-computed by the caller before acquiring any locks,
/// so the (potentially slow) embedding step does not block concurrent tool calls.
pub async fn store_memory(
    conn: &Connection,
    embedding: Vec<f32>,
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
    let embedding_blob = embed::floats_to_blob(&embedding);

    let tags_json = serde_json::to_string(&req.tags)?;
    let facts_json = if req.facts.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&req.facts)?)
    };

    let _guard = lock.lock().await;

    // Content hash dedup: skip if same content seen within window for this session.
    // Combines the count + id lookup into a single query.
    if let Some(row) = conn
        .query(
            "SELECT id FROM memories
             WHERE content_hash = ?1
               AND session_id   = ?2
               AND (julianday(?3) - julianday(created_at)) * 86400 < ?4
             LIMIT 1",
            (hash.clone(), req.session_id.clone(), now_str.clone(), dedup_window_secs),
        )
        .await?
        .next()
        .await?
    {
        let existing_id: String = row.get(0)?;
        return Ok(StoreResult { id: existing_id, upserted: false });
    }

    // Topic key upsert
    if let Some(ref topic_key) = req.topic_key {
        let existing: Option<String> = conn
            .query(
                "SELECT id FROM memories WHERE project_id = ?1 AND topic_key = ?2 LIMIT 1",
                (req.project_id.clone(), topic_key.clone()),
            )
            .await?
            .next()
            .await?
            .and_then(|r| r.get::<String>(0).ok());

        if let Some(ref id) = existing {
            let pinned_val = req.pinned.map(|p| if p { 1i64 } else { 0i64 });
        conn.execute(
                "UPDATE memories
                 SET content = ?1, title = ?2, tags = ?3, facts = ?4, importance = ?5,
                     content_hash = ?6, updated_at = ?7, source = COALESCE(?8, source),
                     pinned = COALESCE(?9, pinned)
                 WHERE id = ?10",
                (
                    content,
                    title,
                    tags_json,
                    facts_json,
                    importance,
                    hash,
                    now_str.clone(),
                    req.source.clone(),
                    pinned_val,
                    id.clone(),
                ),
            )
            .await?;

            // Replace embedding
            conn.execute("DELETE FROM memory_vectors WHERE memory_id = ?1", (id.clone(),))
                .await?;
            conn.execute(
                "INSERT INTO memory_vectors (memory_id, embed_model, embedding) VALUES (?1, ?2, ?3)",
                (id.clone(), embed::model_id(), embedding_blob),
            )
            .await?;

            return Ok(StoreResult { id: id.clone(), upserted: true });
        }
    }

    // Insert new memory. COALESCE lets the DB default apply when source/pinned is NULL.
    let pinned_val = req.pinned.map(|p| if p { 1i64 } else { 0i64 });
    let id = Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO memories
            (id, project_id, topic_key, type, title, content, facts, tags,
             importance, session_id, source, pinned, created_at, updated_at, content_hash)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, COALESCE(?11, 'realtime'), COALESCE(?12, 0), ?13, ?14, ?15)",
        (
            id.clone(), req.project_id, req.topic_key, req.memory_type,
            title, content, facts_json, tags_json,
            importance, req.session_id, req.source, pinned_val,
            now_str.clone(), now_str, hash
        ),
    )
    .await?;

    conn.execute(
        "INSERT INTO memory_vectors (memory_id, embed_model, embedding) VALUES (?1, ?2, ?3)",
        (id.clone(), embed::model_id(), embedding_blob),
    )
    .await?;

    Ok(StoreResult { id, upserted: false })
}

fn content_hash(content: &str) -> String {
    let mut h = Sha256::new();
    h.update(content.as_bytes());
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_is_deterministic_and_unique() {
        let h1 = content_hash("hello");
        let h2 = content_hash("hello");
        let h3 = content_hash("world");
        assert_eq!(h1, h2, "same input must produce same hash");
        assert_ne!(h1, h3, "different inputs must produce different hashes");
        assert_eq!(h1.len(), 64, "SHA-256 hex is 64 chars");
    }
}
