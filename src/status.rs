use anyhow::Result;

use crate::{config::{Config, StorageMode}, db::Db, migrations, project_id};

pub async fn run(config: &Config) -> Result<()> {
    let pid = project_id::resolve(&config.project_root, config.project_id.as_deref());

    let db = Db::open(config).await?;
    let conn = db.conn;
    migrations::run(&conn).await?;

    let db_path = config.db_path();
    let s = &config.memory.storage;
    let mode = match s.mode {
        StorageMode::Managed => "managed".to_string(),
        StorageMode::Local => "local".to_string(),
        StorageMode::Remote => format!(
            "remote/{}",
            format!("{:?}", s.remote_mode).to_lowercase()
        ),
        StorageMode::Disabled => "disabled".to_string(),
    };

    let total: i64 = conn
        .query("SELECT COUNT(*) FROM memories WHERE status = 'active'", ())
        .await?
        .next()
        .await?
        .map(|r| r.get::<i64>(0).unwrap_or(0))
        .unwrap_or(0);

    let project_count: i64 = conn
        .query(
            "SELECT COUNT(*) FROM memories WHERE status = 'active' AND project_id = ?1",
            (pid.clone(),),
        )
        .await?
        .next()
        .await?
        .map(|r| r.get::<i64>(0).unwrap_or(0))
        .unwrap_or(0);

    let last_stored: Option<String> = conn
        .query(
            "SELECT created_at FROM memories WHERE project_id = ?1 ORDER BY created_at DESC LIMIT 1",
            (pid.clone(),),
        )
        .await?
        .next()
        .await?
        .and_then(|r| r.get::<String>(0).ok());

    println!("tyto v{}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("project:      {pid}");
    println!("backend:      {mode}");
    println!("database:     {}", db_path.display());
    println!();
    println!("memories (this project): {project_count}");
    println!("memories (all projects): {total}");
    if let Some(ts) = last_stored {
        println!("last stored:  {}", ts.get(..19).unwrap_or(&ts));
    } else {
        println!("last stored:  never");
    }

    if matches!(s.mode, StorageMode::Remote) {
        match &s.remote_url {
            Some(url) => println!("remote:       {url}"),
            None => println!("remote:       (not configured)"),
        }
    } else {
        println!();
        println!("tip: run 'tyto install' to configure Claude Code integration");
    }

    Ok(())
}
