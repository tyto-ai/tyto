CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS schema_migrations (
    name       TEXT PRIMARY KEY,
    applied_at TEXT NOT NULL,
    checksum   TEXT NOT NULL
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

-- TODO: Re-enable when turso (Limbo) supports vector indexing.
-- CREATE INDEX IF NOT EXISTS memory_vectors_idx
--    ON memory_vectors (libsql_vector_idx(embedding));

-- TODO: Re-enable with Limbo native FTS syntax when sync support is confirmed.
-- CREATE INDEX IF NOT EXISTS memories_fts_idx ON memories USING fts(title, content, facts);

/* 
-- FTS5 is not supported by Limbo (turso lib) and causes crashes due to WITHOUT ROWID shadow tables.
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
*/

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
