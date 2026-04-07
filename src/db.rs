use anyhow::{Context, Result};
use libsql::{Builder, Connection, Database};
use std::path::Path;

use crate::config::{BackendMode, Config};

pub struct Db {
    pub conn: Connection,
    _db: Database,
}

impl Db {
    pub async fn open(config: &Config) -> Result<Self> {
        let db = match config.backend.mode {
            BackendMode::Local => {
                let path = config.db_path();
                ensure_parent_dir(&path)?;
                Builder::new_local(&path)
                    .build()
                    .await
                    .with_context(|| format!("Failed to open local DB at {}", path.display()))?
            }
            BackendMode::Replica => {
                let path = config.db_path();
                ensure_parent_dir(&path)?;
                let url = config
                    .backend
                    .remote_url
                    .as_deref()
                    .context("replica mode requires backend.remote_url")?;
                let token = config
                    .backend
                    .auth_token
                    .as_deref()
                    .context("replica mode requires backend.auth_token")?;
                let path_str = path.to_str().context("replica DB path is not valid UTF-8")?;
                open_replica_with_recovery(path_str, path.as_ref(), url, token).await?
            }
        };

        let conn = db.connect().context("Failed to connect to database")?;

        // Enable WAL mode and a generous busy timeout for local mode only.
        // WAL allows concurrent readers while a writer holds the lock; busy_timeout
        // makes writers retry for up to 5s instead of immediately returning SQLITE_BUSY.
        // This makes local mode safe for multiple concurrent memso processes (e.g.
        // multiple agents or IDE windows on the same project).
        //
        // Skipped for replica mode: the local replica file is managed by libsql's
        // sync engine and pragma behaviour there is undocumented - leave it alone.
        //
        // Known gap: the in-process WriteLock dedup guard does not extend across
        // processes, so concurrent agents may occasionally write duplicate memories.
        // Acceptable for v1; a shared-lock or daemon model can address this later.
        if matches!(config.backend.mode, BackendMode::Local) {
            conn.execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;"
            )
            .await
            .context("Failed to set WAL mode / busy_timeout")?;
        }

        Ok(Self { conn, _db: db })
    }
}

/// Open a remote replica, automatically recovering from local file corruption.
///
/// If the initial open fails (e.g. "database disk image is malformed" after a
/// mid-write process kill), delete all local replica files and retry once.
/// The remote is the source of truth, so this is always safe.
async fn open_replica_with_recovery(
    path_str: &str,
    path: &Path,
    url: &str,
    token: &str,
) -> Result<Database> {
    let build = || {
        Builder::new_remote_replica(path_str, url.to_string(), token.to_string())
            .sync_interval(std::time::Duration::from_secs(1))
            .build()
    };

    match build().await {
        Ok(db) => Ok(db),
        Err(first_err) => {
            // Replica files (`memory.replica.db`) are distinct from local-mode files
            // (`memory.db`), so purging is always safe - there is no risk of deleting
            // the user's local database. The remote is the source of truth.
            eprintln!(
                "memso: replica open failed ({first_err:#}), purging local files and retrying..."
            );
            purge_replica_files(path)?;
            build()
                .await
                .with_context(|| format!("Failed to open replica DB at {} (after recovery attempt)", path.display()))
        }
    }
}

/// Delete all libsql replica local files so the next open does a clean re-sync.
/// Removes `<path>`, `<path>-shm`, `<path>-wal`, and `<path>-info`.
fn purge_replica_files(path: &Path) -> Result<()> {
    let suffixes = ["", "-shm", "-wal", "-info"];
    for suffix in suffixes {
        let candidate = if suffix.is_empty() {
            path.to_path_buf()
        } else {
            let name = format!(
                "{}{}",
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default(),
                suffix
            );
            path.with_file_name(name)
        };
        if candidate.exists() {
            std::fs::remove_file(&candidate)
                .with_context(|| format!("Failed to remove {}", candidate.display()))?;
        }
    }
    Ok(())
}

fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }
    Ok(())
}
