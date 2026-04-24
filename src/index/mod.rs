pub mod git;
pub mod indexer;
pub mod parser;
pub mod schema;
pub mod search;
pub mod watcher;

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::embed::Embedder;

pub struct IndexReady {
    pub conn: Arc<Mutex<rusqlite::Connection>>,
    pub embedder: Arc<Mutex<Embedder>>,
    pub project_root: PathBuf,
    pub git_history: bool,
}

#[derive(Clone)]
pub enum IndexState {
    /// Index DB is being opened and schema applied.
    Opening,
    /// Index is open and ready for queries; indexing may be in progress.
    Ready(Arc<IndexReady>),
    /// Indexing is disabled in config.
    Disabled,
    /// Init failed permanently.
    Failed(String),
}

/// Open the index database at `db_path`, apply schema, return an `IndexReady`.
pub async fn open(
    db_path: &std::path::Path,
    project_root: PathBuf,
    git_history: bool,
    embedder: Arc<Mutex<Embedder>>,
) -> Result<IndexReady> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    
    let db_path = db_path.to_path_buf();
    let conn = tokio::task::spawn_blocking(move || {
        let conn = rusqlite::Connection::open(db_path)?;
        Ok::<_, anyhow::Error>(conn)
    }).await??;

    let conn = Arc::new(Mutex::new(conn));
    schema::ensure(&conn).await?;
    
    Ok(IndexReady { conn, embedder, project_root, git_history })
}
