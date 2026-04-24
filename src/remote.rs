use anyhow::{bail, Context, Result};
use turso::{Builder, Connection, Value, params_from_iter};
use crate::config::{StorageMode, RemoteMode, Config};

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

    println!("tyto remote enable");
    println!("==================");
    println!();
    println!("NOTE: Ensure Claude Code (tyto serve) is not running before proceeding.");
    println!("      Concurrent writes during migration can result in data loss.");
    println!();

    println!("[1/6] Opening local database at {} ...", local_path.display());
    let local_db = Builder::new_local(local_path.to_str().context("local path is not valid UTF-8")?)
        .build()
        .await
        .context("Failed to open local DB")?;
    let local = local_db.connect().context("Failed to connect to local DB")?;

    println!("[2/6] Flushing WAL to ensure all data is captured ...");
    local.execute("PRAGMA wal_checkpoint(TRUNCATE)", ()).await
        .context("Failed to checkpoint WAL")?;

    println!("[3/6] Connecting to remote at {} ...", url);
    // Limbo 0.6.0 does not yet support direct remote client mode.
    // We use a temporary file replica as a workaround.
    let tmp_dir = std::env::temp_dir().join("tyto-remote-migrate");
    std::fs::create_dir_all(&tmp_dir)?;
    let path = tmp_dir.join("remote.db");
    let path_str = path.to_str().context("temp path is not valid UTF-8")?;
    
    let remote_db = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        turso::sync::Builder::new_remote(path_str)
            .with_remote_url(url.clone())
            .with_auth_token(token.clone())
            .build(),
    )
    .await
    .map_err(|_| anyhow::anyhow!(
        "Timed out connecting to remote at {url} (10s). Check the URL and token."
    ))
    .and_then(|r| r.context("Failed to connect to remote - check the URL and token"))?;
    let remote = remote_db.connect().await.context("Failed to connect to remote DB")?;

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
    println!("Done. Set TYTO__MEMORY__REMOTE_AUTH_TOKEN in your environment and restart Claude Code.");

    Ok(())
}

/// Seed remote from the local database (`memory.db`). Returns a status string
/// suitable for both CLI output and MCP tool responses.
///
/// Checks (in order):
/// 1. Config is in replica mode
/// 2. Remote memory count is 0 (unless force)
/// 3. local memory.db exists (the natural backup left in place by `remote enable`)
pub async fn sync(config: &Config, force: bool) -> Result<String> {
    let s = &config.memory.storage;
    if !matches!((&s.mode, &s.remote_mode), (StorageMode::Remote, RemoteMode::Replica)) {
        return Ok(
            "Not in remote/replica mode. Run `tyto remote enable` first to configure cloud sync."
                .to_string(),
        );
    }

    let url = s
        .remote_url
        .as_deref()
        .context("remote replica mode requires memory.remote_url")?;
    let token = s
        .remote_auth_token
        .as_deref()
        .context("remote replica mode requires memory.remote_auth_token (set TYTO__MEMORY__REMOTE_AUTH_TOKEN)")?;

    let local_path = config.local_db_path();
    if !local_path.exists() {
        return Ok(format!(
            "No local database found at {}. Nothing to seed from.",
            local_path.display()
        ));
    }

    println!("Connecting to remote at {} ...", url);
    let tmp_dir = std::env::temp_dir().join("tyto-remote-sync");
    std::fs::create_dir_all(&tmp_dir)?;
    let path = tmp_dir.join("remote.db");
    let path_str = path.to_str().context("temp path is not valid UTF-8")?;

    let remote_db = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        turso::sync::Builder::new_remote(path_str)
            .with_remote_url(url.to_string())
            .with_auth_token(token.to_string())
            .build(),
    )
    .await
    .map_err(|_| anyhow::anyhow!(
        "Timed out connecting to remote at {url} (10s). Check the URL and token."
    ))
    .and_then(|r| r.context("Failed to connect to remote DB"))?;
    let remote = remote_db.connect().await?;

    println!("Running migrations on remote database ...");
    crate::migrations::run(&remote).await?;

    let count = row_count(&remote, "memories").await?;
    if count > 0 && !force {
        return Ok(format!(
            "Remote already has {count} memories. Use --force to overwrite."
        ));
    }

    println!("Opening local database at {} ...", local_path.display());
    let local_db = Builder::new_local(local_path.to_str().context("local path is not valid UTF-8")?)
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
    let mut rows: turso::Rows = conn
        .query(&sql, ())
        .await?;
    let count = rows
        .next()
        .await?
        .map(|r| r.get::<i64>(0).unwrap_or(0))
        .unwrap_or(0);
    Ok(count)
}

async fn copy_memories(local: &Connection, remote: &Connection, total: i64) -> Result<usize> {
    let mut rows: turso::Rows = local
        .query(
            "SELECT id, project_id, topic_key, type, title, content, facts, tags,
                    importance, access_count, last_accessed, pinned, status,
                    session_id, source, created_at, updated_at, content_hash
             FROM memories",
            (),
        )
        .await?;

    let mut count = 0usize;
    while let Some(row) = rows.next().await? {
        remote
            .execute(
                "INSERT OR IGNORE INTO memories
                    (id, project_id, topic_key, type, title, content, facts, tags,
                     importance, access_count, last_accessed, pinned, status,
                     session_id, source, created_at, updated_at, content_hash)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
                params_from_iter(vec![
                    Value::Text(row.get::<String>(0)?),
                    Value::Text(row.get::<String>(1)?),
                    Value::from(row.get::<Option<String>>(2)?),
                    Value::Text(row.get::<String>(3)?),
                    Value::Text(row.get::<String>(4)?),
                    Value::Text(row.get::<String>(5)?),
                    Value::from(row.get::<Option<String>>(6)?),
                    Value::from(row.get::<Option<String>>(7)?),
                    Value::Real(row.get::<f64>(8)?),
                    Value::Integer(row.get::<i64>(9)?),
                    Value::from(row.get::<Option<String>>(10)?),
                    Value::Integer(row.get::<i64>(11)?),
                    Value::Text(row.get::<String>(12)?),
                    Value::Text(row.get::<String>(13)?),
                    Value::Text(row.get::<String>(14)?),
                    Value::Text(row.get::<String>(15)?),
                    Value::Text(row.get::<String>(16)?),
                    Value::Text(row.get::<String>(17)?),
                ]),
            )
            .await?;
        count += 1;
        if total > 0 && count % 10 == 0 {
            println!("      {count}/{total} memories ...");
        }
    }
    Ok(count)
}

async fn copy_vectors(local: &Connection, remote: &Connection) -> Result<usize> {
    let mut rows: turso::Rows = local
        .query(
            "SELECT memory_id, embed_model, embedding FROM memory_vectors",
            (),
        )
        .await?;

    let mut count = 0usize;
    while let Some(row) = rows.next().await? {
        remote
            .execute(
                "INSERT OR REPLACE INTO memory_vectors (memory_id, embed_model, embedding) VALUES (?1, ?2, ?3)",
                (row.get::<String>(0)?, row.get::<String>(1)?, row.get::<Vec<u8>>(2)?),
            )
            .await?;
        count += 1;
    }
    Ok(count)
}

async fn copy_captures(local: &Connection, remote: &Connection) -> Result<usize> {
    let mut rows: turso::Rows = local
        .query(
            "SELECT id, project_id, captured_at, tool_name, summary, raw_data, presented_at
             FROM raw_captures",
            (),
        )
        .await?;

    let mut count = 0usize;
    while let Some(row) = rows.next().await? {
        remote
            .execute(
                "INSERT OR IGNORE INTO raw_captures
                    (id, project_id, captured_at, tool_name, summary, raw_data, presented_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                (
                    row.get::<String>(0)?,
                    row.get::<String>(1)?,
                    row.get::<String>(2)?,
                    row.get::<String>(3)?,
                    row.get::<String>(4)?,
                    row.get::<String>(5)?,
                    row.get::<Option<String>>(6)?,
                ),
            )
            .await?;
        count += 1;
    }
    Ok(count)
}

fn update_config(config: &Config, remote_url: &str) -> Result<()> {
    let config_path = config
        .source_path
        .clone()
        .unwrap_or_else(|| config.project_root.join(".tyto.toml"));

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

    doc["memory"]["mode"] = toml_edit::value("remote");
    doc["memory"]["remote_mode"] = toml_edit::value("replica");
    doc["memory"]["remote_url"] = toml_edit::value(remote_url);

    std::fs::write(&config_path, doc.to_string())
        .with_context(|| format!("Failed to write {}", config_path.display()))?;

    Ok(())
}
