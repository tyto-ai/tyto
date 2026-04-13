use anyhow::{bail, Context, Result};
use libsql::{Builder, Connection};
use crate::config::{BackendMode, RemoteMode, Config};

pub async fn enable(
    config: &Config,
    url: Option<String>,
    token: Option<String>,
    force: bool,
) -> Result<()> {
    let url = url.context("--url is required")?;
    let token = token.context("--token is required")?;

    let local_path = config.local_db_path();
    if !local_path.exists() {
        bail!("Local database not found at {}", local_path.display());
    }

    println!("memso remote enable");
    println!("===================");
    println!();
    println!("NOTE: Ensure Claude Code (memso serve) is not running before proceeding.");
    println!("      Concurrent writes during migration can result in data loss.");
    println!();

    println!("[1/6] Opening local database at {} ...", local_path.display());
    let local_db = Builder::new_local(&local_path)
        .build()
        .await
        .context("Failed to open local DB")?;
    let local = local_db.connect().context("Failed to connect to local DB")?;

    println!("[2/6] Flushing WAL to ensure all data is captured ...");
    local.execute("PRAGMA wal_checkpoint(TRUNCATE)", libsql::params![]).await
        .context("Failed to checkpoint WAL")?;

    println!("[3/6] Connecting to remote at {} ...", url);
    let remote_db = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        Builder::new_remote(url.clone(), token).build(),
    )
    .await
    .map_err(|_| anyhow::anyhow!(
        "Timed out connecting to remote at {url} (10s). Check the URL and token."
    ))
    .and_then(|r| r.context("Failed to connect to remote - check the URL and token"))?;
    let remote = remote_db.connect().context("Failed to connect to remote DB")?;

    println!("[4/6] Running migrations on remote database ...");
    crate::migrations::run(&remote).await?;

    if !force {
        let count = row_count(&remote, "memories").await?;
        if count > 0 {
            bail!(
                "Remote database already has {count} memories. Use --force to overwrite."
            );
        }
    }

    println!("[5/6] Copying data to remote ...");
    let local_count = row_count(&local, "memories").await?;
    let (memories, vectors, captures) = copy_all_verbose(&local, &remote, local_count).await?;
    println!("      Done: {memories} memories, {vectors} vectors, {captures} captures.");

    println!("[6/6] Updating config ...");
    // The local database stays at memory.db. Replica mode uses memory.replica.db,
    // so the two files never conflict and memory.db serves as a natural backup.
    update_config(config, &url)?;
    println!("      Config updated to remote/replica mode.");
    println!("      Local backup retained at {}", local_path.display());

    println!();
    println!("Done. Set MEMSO_BACKEND__AUTH_TOKEN in your environment and restart Claude Code.");

    Ok(())
}

/// Seed remote from the local database (`memory.db`). Returns a status string
/// suitable for both CLI output and MCP tool responses.
///
/// Checks (in order):
/// 1. Config is in replica mode
/// 2. Remote memory count is 0 (unless force)
/// 3. `.memso/memory.db` exists (the natural backup left in place by `remote enable`)
pub async fn sync(config: &Config, force: bool) -> Result<String> {
    if !matches!(
        (&config.backend.mode, &config.backend.remote_mode),
        (BackendMode::Remote, RemoteMode::Replica)
    ) {
        return Ok(
            "Not in remote/replica mode. Run `memso remote enable` first to configure cloud sync."
                .to_string(),
        );
    }

    let url = config
        .backend
        .remote_url
        .as_deref()
        .context("remote replica mode requires backend.remote_url")?;
    let token = config
        .backend
        .auth_token
        .as_deref()
        .context("remote replica mode requires backend.auth_token (set MEMSO_BACKEND__AUTH_TOKEN)")?;

    let local_path = config.local_db_path();
    if !local_path.exists() {
        return Ok(format!(
            "No local database found at {}. Nothing to seed from.",
            local_path.display()
        ));
    }

    println!("Connecting to remote at {} ...", url);
    let remote_db = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        Builder::new_remote(url.to_string(), token.to_string()).build(),
    )
    .await
    .map_err(|_| anyhow::anyhow!(
        "Timed out connecting to remote at {url} (10s). Check the URL and token."
    ))
    .and_then(|r| r.context("Failed to connect to remote DB"))?;
    let remote = remote_db.connect()?;

    println!("Running migrations on remote database ...");
    crate::migrations::run(&remote).await?;

    let count = row_count(&remote, "memories").await?;
    if count > 0 && !force {
        return Ok(format!(
            "Remote already has {count} memories. Use --force to overwrite."
        ));
    }

    println!("Opening local database at {} ...", local_path.display());
    let local_db = Builder::new_local(&local_path)
        .build()
        .await
        .context("Failed to open local DB")?;
    let local = local_db.connect()?;

    let local_count = row_count(&local, "memories").await?;
    println!("Copying {local_count} memories to remote ...");
    let (memories, vectors, captures) = copy_all_verbose(&local, &remote, local_count).await?;

    Ok(format!(
        "Done: seeded {memories} memories, {vectors} vectors, {captures} captures to remote."
    ))
}

/// Copy all data from `src` to `dst`. Returns (memories, vectors, captures) counts.
pub async fn copy_all(src: &Connection, dst: &Connection) -> Result<(usize, usize, usize)> {
    copy_all_verbose(src, dst, 0).await
}

async fn copy_all_verbose(
    src: &Connection,
    dst: &Connection,
    total_memories: i64,
) -> Result<(usize, usize, usize)> {
    let memories = copy_memories(src, dst, total_memories).await?;
    let vectors = copy_vectors(src, dst).await?;
    let captures = copy_captures(src, dst).await?;
    Ok((memories, vectors, captures))
}


async fn row_count(conn: &Connection, table: &str) -> Result<i64> {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    let count = conn
        .query(&sql, libsql::params![])
        .await?
        .next()
        .await?
        .map(|r| r.get::<i64>(0).unwrap_or(0))
        .unwrap_or(0);
    Ok(count)
}

async fn copy_memories(local: &Connection, remote: &Connection, total: i64) -> Result<usize> {
    let mut rows = local
        .query(
            "SELECT id, project_id, topic_key, type, title, content, facts, tags,
                    importance, confidence, access_count, last_accessed, pinned, status,
                    supersedes, session_id, source, created_at, updated_at, content_hash
             FROM memories",
            libsql::params![],
        )
        .await?;

    let mut count = 0usize;
    while let Some(row) = rows.next().await? {
        remote
            .execute(
                "INSERT OR IGNORE INTO memories
                    (id, project_id, topic_key, type, title, content, facts, tags,
                     importance, confidence, access_count, last_accessed, pinned, status,
                     supersedes, session_id, source, created_at, updated_at, content_hash)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
                libsql::params![
                    row.get_value(0)?,
                    row.get_value(1)?,
                    row.get_value(2)?,
                    row.get_value(3)?,
                    row.get_value(4)?,
                    row.get_value(5)?,
                    row.get_value(6)?,
                    row.get_value(7)?,
                    row.get_value(8)?,
                    row.get_value(9)?,
                    row.get_value(10)?,
                    row.get_value(11)?,
                    row.get_value(12)?,
                    row.get_value(13)?,
                    row.get_value(14)?,
                    row.get_value(15)?,
                    row.get_value(16)?,
                    row.get_value(17)?,
                    row.get_value(18)?,
                    row.get_value(19)?
                ],
            )
            .await?;
        count += 1;
        if total > 0 && count.is_multiple_of(10) {
            println!("      {count}/{total} memories ...");
        }
    }
    Ok(count)
}

async fn copy_vectors(local: &Connection, remote: &Connection) -> Result<usize> {
    let mut rows = local
        .query(
            "SELECT memory_id, embed_model, embedding FROM memory_vectors",
            libsql::params![],
        )
        .await?;

    let mut count = 0usize;
    while let Some(row) = rows.next().await? {
        remote
            .execute(
                "INSERT OR IGNORE INTO memory_vectors (memory_id, embed_model, embedding) VALUES (?1, ?2, ?3)",
                libsql::params![row.get_value(0)?, row.get_value(1)?, row.get_value(2)?],
            )
            .await?;
        count += 1;
    }
    Ok(count)
}

async fn copy_captures(local: &Connection, remote: &Connection) -> Result<usize> {
    let mut rows = local
        .query(
            "SELECT id, project_id, captured_at, tool_name, summary, raw_data, presented_at
             FROM raw_captures",
            libsql::params![],
        )
        .await?;

    let mut count = 0usize;
    while let Some(row) = rows.next().await? {
        remote
            .execute(
                "INSERT OR IGNORE INTO raw_captures
                    (id, project_id, captured_at, tool_name, summary, raw_data, presented_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                libsql::params![
                    row.get_value(0)?,
                    row.get_value(1)?,
                    row.get_value(2)?,
                    row.get_value(3)?,
                    row.get_value(4)?,
                    row.get_value(5)?,
                    row.get_value(6)?
                ],
            )
            .await?;
        count += 1;
    }
    Ok(count)
}

fn update_config(config: &Config, remote_url: &str) -> Result<()> {
    let config_path = config.source_path.clone().unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(".memso.toml")
    });

    let existing = if config_path.exists() {
        std::fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?
    } else {
        String::new()
    };

    // toml_edit edits in-place, preserving user comments and key ordering.
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .with_context(|| format!("Failed to parse {}", config_path.display()))?;

    doc["backend"]["mode"] = toml_edit::value("remote");
    doc["backend"]["remote_mode"] = toml_edit::value("replica");
    doc["backend"]["remote_url"] = toml_edit::value(remote_url);

    std::fs::write(&config_path, doc.to_string())
        .with_context(|| format!("Failed to write {}", config_path.display()))?;

    Ok(())
}
