use anyhow::{Context, Result};
use std::path::Path;
use turso::{Connection, Database};

use crate::{
    config::{Config, RemoteMode, StorageMode},
    mlog,
};

pub enum AnyDb {
    Local(Database),
    Synced(turso::sync::Database),
}

pub struct Db {
    pub conn: Connection,
    pub handle: AnyDb,
    // Keeps the temp directory alive for direct-mode replicas. None for all other modes.
    #[allow(dead_code)]
    pub temp_dir: Option<tempfile::TempDir>,
}

impl Db {
    pub async fn open(config: &Config) -> Result<Self> {
        let t = std::time::Instant::now();
        let s = &config.memory.storage;
        let (any_db, temp_dir) = match s.mode {
            StorageMode::Managed | StorageMode::Local | StorageMode::Disabled => {
                let path = config.db_path();
                ensure_parent_dir(&path)?;
                let db =
                    turso::Builder::new_local(path.to_str().context("DB path is not valid UTF-8")?)
                        .experimental_multiprocess_wal(true)
                        .experimental_index_method(true)
                        .build()
                        .await
                        .with_context(|| {
                            format!("Failed to open local DB at {}", path.display())
                        })?;
                (AnyDb::Local(db), None)
            }
            StorageMode::Remote => {
                let url = s
                    .remote_url
                    .as_deref()
                    .context("remote mode requires memory.remote_url")?;
                let token = s
                    .remote_auth_token
                    .as_deref()
                    .context("remote mode requires memory.remote_auth_token")?;
                match s.remote_mode {
                    RemoteMode::Direct => {
                        // Limbo 0.6.0 does not yet support direct remote client mode.
                        // We use a temporary file replica as a workaround. The TempDir is kept
                        // alive on Db so the directory is cleaned up when the connection closes.
                        let tmp = tempfile::Builder::new()
                            .prefix("coree-remote-direct-")
                            .tempdir()
                            .context("Failed to create temp dir for direct-mode replica")?;
                        let path = tmp.path().join("memory.db");
                        let path_str = path.to_str().context("temp path is not valid UTF-8")?;
                        let db = open_replica_with_recovery(path_str, &path, url, token).await?;
                        (AnyDb::Synced(db), Some(tmp))
                    }
                    RemoteMode::Replica => {
                        let path = config.db_path();
                        ensure_parent_dir(&path)?;
                        let path_str = path
                            .to_str()
                            .context("replica DB path is not valid UTF-8")?;
                        let db =
                            open_replica_with_recovery(path_str, path.as_ref(), url, token).await?;
                        (AnyDb::Synced(db), None)
                    }
                }
            }
        };

        let conn = match &any_db {
            AnyDb::Local(db) => db.connect().context("Failed to connect to database")?,
            AnyDb::Synced(db) => db
                .connect()
                .await
                .context("Failed to connect to synced database")?,
        };

        // Apply busy_timeout to all modes. Replica mode also needs it: the libsql background
        // sync thread can briefly lock the WAL, and without a retry budget store_memories
        // fails with "database is locked" on the first contention window.
        conn.execute_batch("PRAGMA busy_timeout=5000;")
            .await
            .context("Failed to set busy_timeout")?;

        tracing::debug!(elapsed_ms = t.elapsed().as_millis(), "Db::open");
        Ok(Self {
            conn,
            handle: any_db,
            temp_dir,
        })
    }
}

async fn open_replica_with_recovery(
    path_str: &str,
    path: &Path,
    url: &str,
    token: &str,
) -> Result<turso::sync::Database> {
    let build = || async {
        let mut last_err = None;
        // GOTCHA: In Turso 0.6.0-pre.22, 'experimental_multiprocess_wal' is NOT available
        // for synced replicas. Only one process can have a replica open at a time.
        // We use a high retry count (20 attempts / 5s) to handle process handovers during
        // quick restarts, allowing the previous process time to fully exit and release the lock.
        for i in 0..20 {
            match turso::sync::Builder::new_remote(path_str)
                .with_remote_url(url)
                .with_auth_token(token)
                .build()
                .await
            {
                Ok(db) => return Ok(db),
                Err(e) => {
                    last_err = Some(e);
                    if i % 5 == 0 && i > 0 {
                        mlog!("coree: replica build attempt {i} failed, retrying...");
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
            }
        }
        Err(match last_err {
            Some(e) => anyhow::anyhow!("Failed to build replica after 20 attempts: {e}"),
            None => anyhow::anyhow!("Failed to build replica after 20 attempts: unknown error"),
        })
    };

    let try_sync = |db: turso::sync::Database| async move {
        let mut last_err = None;
        let t_sync = std::time::Instant::now();
        for i in 0..5 {
            match db.pull().await {
                Ok(_) => {
                    tracing::debug!(
                        elapsed_ms = t_sync.elapsed().as_millis(),
                        attempts = i + 1,
                        "replica pull success"
                    );
                    return Ok(db);
                }
                Err(e) => {
                    last_err = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        }
        Err(match last_err {
            Some(e) => anyhow::anyhow!("Failed to sync replica after 5 attempts: {e}"),
            None => anyhow::anyhow!("Failed to sync replica after 5 attempts: unknown error"),
        })
    };

    let try_open = || async {
        let db = build().await?;
        try_sync(db).await
    };

    match try_open().await {
        Ok(db) => return Ok(db),
        Err(e) => mlog!(
            "coree: CRITICAL: replica open failed ({e:#}). PURGING local replica files to force full resync..."
        ),
    }

    purge_replica_files(path)?;

    try_open().await.with_context(|| {
        format!(
            "Failed to open replica DB at {} (after recovery attempt)",
            path.display()
        )
    })
}

pub fn purge_replica_files(path: &Path) -> Result<()> {
    let parent = path.parent().unwrap_or(std::path::Path::new("."));
    let prefix = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();

    let entries = std::fs::read_dir(parent)
        .with_context(|| format!("Failed to read dir {}", parent.display()))?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with(&prefix) {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => tracing::debug!(file = %name_str, "purged replica file"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("Failed to remove {}", entry.path().display()));
                }
            }
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
