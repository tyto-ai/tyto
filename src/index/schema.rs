use anyhow::Result;
use libsql::Connection;

/// Apply the code intelligence schema to index.db.
/// All DDL is IF NOT EXISTS so it is safe to call on every startup.
pub async fn ensure(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA busy_timeout=5000;

         CREATE TABLE IF NOT EXISTS index_files (
             path        TEXT PRIMARY KEY,
             content_hash TEXT NOT NULL,
             indexed_at  TEXT NOT NULL
         );

         CREATE TABLE IF NOT EXISTS index_chunks (
             id             TEXT PRIMARY KEY,
             file_path      TEXT NOT NULL,
             symbol_name    TEXT NOT NULL,
             qualified_name TEXT NOT NULL,
             symbol_kind    TEXT NOT NULL,
             signature      TEXT,
             doc_comment    TEXT,
             body_preview   TEXT,
             line_start     INTEGER NOT NULL,
             line_end       INTEGER NOT NULL,
             language       TEXT NOT NULL,
             churn_count    INTEGER DEFAULT 0,
             indexed_at     TEXT NOT NULL,
             content_hash   TEXT NOT NULL
         );

         CREATE INDEX IF NOT EXISTS index_chunks_file
             ON index_chunks (file_path);

         CREATE TABLE IF NOT EXISTS index_vectors (
             chunk_id    TEXT NOT NULL REFERENCES index_chunks(id) ON DELETE CASCADE,
             embed_model TEXT NOT NULL,
             embedding   F32_BLOB(384) NOT NULL,
             PRIMARY KEY (chunk_id, embed_model)
         );

         CREATE INDEX IF NOT EXISTS index_vectors_idx
             ON index_vectors (libsql_vector_idx(embedding));

         CREATE VIRTUAL TABLE IF NOT EXISTS index_chunks_fts
             USING fts5(symbol_name, qualified_name, signature, doc_comment, body_preview,
                        content=index_chunks, content_rowid=rowid);

         CREATE TRIGGER IF NOT EXISTS index_chunks_fts_insert
             AFTER INSERT ON index_chunks BEGIN
                 INSERT INTO index_chunks_fts(rowid, symbol_name, qualified_name,
                     signature, doc_comment, body_preview)
                 VALUES (new.rowid, new.symbol_name, new.qualified_name,
                     COALESCE(new.signature, ''), COALESCE(new.doc_comment, ''),
                     COALESCE(new.body_preview, ''));
             END;

         CREATE TRIGGER IF NOT EXISTS index_chunks_fts_delete
             AFTER DELETE ON index_chunks BEGIN
                 INSERT INTO index_chunks_fts(index_chunks_fts, rowid, symbol_name,
                     qualified_name, signature, doc_comment, body_preview)
                 VALUES ('delete', old.rowid, old.symbol_name, old.qualified_name,
                     COALESCE(old.signature, ''), COALESCE(old.doc_comment, ''),
                     COALESCE(old.body_preview, ''));
             END;

         CREATE TABLE IF NOT EXISTS index_commits (
             sha       TEXT PRIMARY KEY,
             message   TEXT NOT NULL,
             author    TEXT,
             timestamp TEXT
         );

         CREATE TABLE IF NOT EXISTS index_chunk_commits (
             chunk_id   TEXT NOT NULL REFERENCES index_chunks(id) ON DELETE CASCADE,
             commit_sha TEXT NOT NULL REFERENCES index_commits(sha) ON DELETE CASCADE,
             PRIMARY KEY (chunk_id, commit_sha)
         );

         CREATE INDEX IF NOT EXISTS index_chunk_commits_by_sha
             ON index_chunk_commits (commit_sha);
        ",
    )
    .await?;
    Ok(())
}
