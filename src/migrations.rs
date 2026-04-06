use anyhow::Result;
use libsql::Connection;

pub async fn run(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA).await?;
    // Fix rows written with the old default 'agent' before the 'realtime' rename.
    conn.execute(
        "UPDATE memories SET source = 'realtime' WHERE source = 'agent'",
        libsql::params![],
    )
    .await?;
    Ok(())
}

const SCHEMA: &str = "
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
    confidence    REAL    DEFAULT 1.0,
    access_count  INTEGER DEFAULT 0,
    last_accessed TEXT,
    pinned        INTEGER DEFAULT 0,
    status        TEXT    DEFAULT 'active',
    supersedes    TEXT,
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

CREATE TABLE IF NOT EXISTS sessions (
    id         TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    started_at TEXT NOT NULL,
    ended_at   TEXT,
    status     TEXT DEFAULT 'active',
    agent      TEXT
);

CREATE TABLE IF NOT EXISTS memory_vectors (
    memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    embedding F32_BLOB(384) NOT NULL
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
