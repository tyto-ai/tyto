use anyhow::Result;
use libsql::Connection;

use crate::embed;

pub async fn run(conn: &Connection) -> Result<()> {
    // Base schema: all CREATE TABLE/INDEX/TRIGGER statements are idempotent.
    conn.execute_batch(SCHEMA).await?;

    let version = get_version(conn).await?;

    // v0 -> v1: rename legacy 'agent' source value to 'realtime'.
    if version < 1 {
        conn.execute(
            "UPDATE memories SET source = 'realtime' WHERE source = 'agent'",
            libsql::params![],
        )
        .await?;
        set_version(conn, 1).await?;
    }

    // v1 -> v2: add embed_model column to memory_vectors.
    // Databases created after this change already have the column (from the base SCHEMA);
    // the ADD COLUMN is idempotent via error handling.
    // Note: PRAGMA table_info and LIMIT 0 SELECT probes are unreliable over Hrana (Turso
    // direct mode) - LIMIT 0 returns Ok even for nonexistent columns. Attempt the DDL
    // directly and ignore "duplicate column name" (already exists).
    // Existing rows are backfilled with the current model_id so they are not re-embedded.
    if version < 2 {
        if let Err(e) = conn
            .execute(
                "ALTER TABLE memory_vectors ADD COLUMN embed_model TEXT NOT NULL DEFAULT ''",
                libsql::params![],
            )
            .await
        {
            if !e.to_string().contains("duplicate column name") {
                return Err(anyhow::anyhow!("v2 migration: {e}"));
            }
        }
        conn.execute(
            "UPDATE memory_vectors SET embed_model = ?1 WHERE embed_model = ''",
            libsql::params![embed::model_id()],
        )
        .await?;
        set_version(conn, 2).await?;
    }

    // v2 -> v3: drop unused sessions table and unused columns (confidence, supersedes).
    // All three were defined in the schema but never read or written by any code path.
    //
    // IMPORTANT: Do NOT use LIMIT 0 SELECT probes here. On Turso/Hrana, a zero-row
    // SELECT returns Ok even for nonexistent columns (column validation is skipped
    // when no rows are fetched). Instead, attempt the DDL directly and ignore the
    // specific "no such column" error (idempotent via error handling, not probing).
    if version < 3 {
        conn.execute("DROP TABLE IF EXISTS sessions", libsql::params![])
            .await?;

        for col in ["confidence", "supersedes"] {
            let sql = format!("ALTER TABLE memories DROP COLUMN {col}");
            if let Err(e) = conn.execute(&sql, libsql::params![]).await {
                if !e.to_string().contains("no such column") {
                    return Err(anyhow::anyhow!("v3 migration: {e}"));
                }
            }
        }

        set_version(conn, 3).await?;
    }

    Ok(())
}

async fn get_version(conn: &Connection) -> Result<i64> {
    let row = conn
        .query("SELECT version FROM schema_version LIMIT 1", libsql::params![])
        .await?
        .next()
        .await?;
    Ok(row.map(|r| r.get::<i64>(0).unwrap_or(0)).unwrap_or(0))
}

async fn set_version(conn: &Connection, version: i64) -> Result<()> {
    conn.execute("DELETE FROM schema_version", libsql::params![]).await?;
    conn.execute(
        "INSERT INTO schema_version (version) VALUES (?1)",
        libsql::params![version],
    )
    .await?;
    Ok(())
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS memories (
    id            TEXT PRIMARY KEY,
    project_id    TEXT NOT NULL,
    topic_key     TEXT,
    type          TEXT NOT NULL,
    title         TEXT NOT NULL,
    content       TEXT NOT NULL,
    facts         TEXT,
    tags          TEXT,
    importance    REAL    DEFAULT 0.5,
    access_count  INTEGER DEFAULT 0,
    last_accessed TEXT,
    pinned        INTEGER DEFAULT 0,
    status        TEXT    DEFAULT 'active',
    session_id    TEXT,
    source        TEXT    NOT NULL DEFAULT 'realtime',
    created_at    TEXT    NOT NULL,
    updated_at    TEXT    NOT NULL,
    content_hash  TEXT    NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS memories_topic_key
    ON memories (project_id, topic_key)
    WHERE topic_key IS NOT NULL;

CREATE INDEX IF NOT EXISTS memories_project_status
    ON memories (project_id, status);

CREATE INDEX IF NOT EXISTS memories_content_hash
    ON memories (content_hash, session_id);

CREATE TABLE IF NOT EXISTS memory_vectors (
    memory_id   TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    embed_model TEXT NOT NULL DEFAULT '',
    embedding   F32_BLOB(384) NOT NULL
);

CREATE INDEX IF NOT EXISTS memory_vectors_idx
    ON memory_vectors (libsql_vector_idx(embedding));

CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts
    USING fts5(title, content, facts, content=memories, content_rowid=rowid);

CREATE TRIGGER IF NOT EXISTS memories_fts_insert
    AFTER INSERT ON memories BEGIN
        INSERT INTO memories_fts(rowid, title, content, facts)
        VALUES (new.rowid, new.title, new.content, COALESCE(new.facts, ''));
    END;

CREATE TRIGGER IF NOT EXISTS memories_fts_update
    AFTER UPDATE ON memories BEGIN
        INSERT INTO memories_fts(memories_fts, rowid, title, content, facts)
        VALUES ('delete', old.rowid, old.title, old.content, COALESCE(old.facts, ''));
        INSERT INTO memories_fts(rowid, title, content, facts)
        VALUES (new.rowid, new.title, new.content, COALESCE(new.facts, ''));
    END;

CREATE TRIGGER IF NOT EXISTS memories_fts_delete
    AFTER DELETE ON memories BEGIN
        INSERT INTO memories_fts(memories_fts, rowid, title, content, facts)
        VALUES ('delete', old.rowid, old.title, old.content, COALESCE(old.facts, ''));
    END;

CREATE TABLE IF NOT EXISTS raw_captures (
    id           TEXT PRIMARY KEY,
    project_id   TEXT NOT NULL,
    captured_at  TEXT NOT NULL,
    tool_name    TEXT NOT NULL,
    summary      TEXT NOT NULL,
    raw_data     TEXT NOT NULL,
    presented_at TEXT
);

CREATE INDEX IF NOT EXISTS raw_captures_pending
    ON raw_captures (project_id, presented_at);
";
