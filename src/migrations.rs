use anyhow::Result;
use chrono::Utc;
use turso::Connection;
use sha2::{Digest, Sha256};

use crate::embed;

struct Migration {
    name: &'static str,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        name: "v001_initial",
        sql: include_str!("migrations/v001_initial.sql"),
    },
    Migration {
        name: "v002_embed_model",
        sql: include_str!("migrations/v002_embed_model.sql"),
    },
    Migration {
        name: "v003_drop_unused",
        sql: include_str!("migrations/v003_drop_unused.sql"),
    },
];

pub async fn run(conn: &Connection) -> Result<()> {
    // Ensure schema_migrations table exists (idempotent DDL).
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            name       TEXT PRIMARY KEY,
            applied_at TEXT NOT NULL,
            checksum   TEXT NOT NULL
        );",
    )
    .await?;

    // Seed schema_migrations from legacy schema_version on first upgrade.
    seed_from_legacy(conn).await?;

    // Apply pending migrations.
    for migration in MIGRATIONS {
        if is_applied(conn, migration.name).await? {
            continue;
        }
        apply(conn, migration).await?;
    }

    // Validate checksums of all applied migrations (warn only - DB is already in that state).
    for migration in MIGRATIONS {
        validate_checksum(conn, migration).await?;
    }

    Ok(())
}

/// Returns true if the named migration is recorded in schema_migrations.
async fn is_applied(conn: &Connection, name: &str) -> Result<bool> {
    let mut rows = conn
        .query(
            "SELECT 1 FROM schema_migrations WHERE name = ?1 LIMIT 1",
            (name,),
        )
        .await?;
    Ok(rows.next().await?.is_some())
}

/// Execute a migration's SQL and record it in schema_migrations.
async fn apply(conn: &Connection, migration: &Migration) -> Result<()> {
    match migration.name {
        "v002_embed_model" => apply_v002(conn, migration).await?,
        "v003_drop_unused" => apply_v003(conn, migration).await?,
        _ => { conn.execute_batch(migration.sql).await?; }
    }

    let checksum = sha256(migration.sql);
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO schema_migrations (name, applied_at, checksum) VALUES (?1, ?2, ?3)",
        (migration.name, now, checksum),
    )
    .await?;

    Ok(())
}

/// v002: ADD COLUMN with "duplicate column name" idempotency, then backfill.
async fn apply_v002(conn: &Connection, _migration: &Migration) -> Result<()> {
    if let Err(e) = conn
        .execute(
            "ALTER TABLE memory_vectors ADD COLUMN embed_model TEXT NOT NULL DEFAULT ''",
            (),
        )
        .await
        && !e.to_string().contains("duplicate column name")
    {
        return Err(anyhow::anyhow!("v002 migration: {e}"));
    }
    conn.execute(
        "UPDATE memory_vectors SET embed_model = ?1 WHERE embed_model = ''",
        (embed::model_id(),),
    )
    .await?;
    Ok(())
}

/// v003: DROP TABLE IF EXISTS is safe; DROP COLUMN with "no such column" idempotency.
async fn apply_v003(conn: &Connection, _migration: &Migration) -> Result<()> {
    conn.execute("DROP TABLE IF EXISTS sessions", ())
        .await?;
    for col in ["confidence", "supersedes"] {
        let sql = format!("ALTER TABLE memories DROP COLUMN {col}");
        if let Err(e) = conn.execute(&sql, ()).await
            && !e.to_string().contains("no such column")
        {
            return Err(anyhow::anyhow!("v003 migration: {e}"));
        }
    }
    Ok(())
}

/// Seed schema_migrations from the legacy schema_version table on first upgrade.
/// If schema_migrations is already populated this is a no-op.
async fn seed_from_legacy(conn: &Connection) -> Result<()> {
    // Check if schema_migrations already has entries.
    let mut rows = conn
        .query(
            "SELECT COUNT(*) FROM schema_migrations",
            (),
        )
        .await?;
    let count: i64 = rows
        .next()
        .await?
        .map(|r| r.get::<i64>(0).unwrap_or(0))
        .unwrap_or(0);
    if count > 0 {
        return Ok(());
    }

    // Read legacy version (0 if table doesn't exist yet).
    let legacy_version = get_legacy_version(conn).await?;
    if legacy_version == 0 {
        return Ok(());
    }

    // Mark v001..v00N as applied with a synthetic checksum so the runner skips them.
    let now = Utc::now().to_rfc3339();
    for migration in MIGRATIONS.iter().take(legacy_version as usize) {
        let checksum = sha256(migration.sql);
        conn.execute(
            "INSERT OR IGNORE INTO schema_migrations (name, applied_at, checksum) VALUES (?1, ?2, ?3)",
            (migration.name, now.as_str(), checksum),
        )
        .await?;
    }

    Ok(())
}

/// Read the legacy schema_version value (0 if table or row is absent).
async fn get_legacy_version(conn: &Connection) -> Result<i64> {
    // schema_version may not exist on a fresh DB - handle the error gracefully.
    match conn
        .query("SELECT version FROM schema_version LIMIT 1", ())
        .await
    {
        Ok(mut rows) => Ok(rows
            .next()
            .await?
            .map(|r| r.get::<i64>(0).unwrap_or(0))
            .unwrap_or(0)),
        Err(_) => Ok(0),
    }
}

/// Recompute the checksum of a migration file and log a warning if it differs from stored.
async fn validate_checksum(conn: &Connection, migration: &Migration) -> Result<()> {
    let mut rows = conn
        .query(
            "SELECT checksum FROM schema_migrations WHERE name = ?1 LIMIT 1",
            (migration.name,),
        )
        .await?;
    let stored = match rows.next().await? {
        Some(row) => row.get::<String>(0)?,
        None => return Ok(()), // Not yet applied - no checksum to validate.
    };
    let current = sha256(migration.sql);
    if stored != current {
        eprintln!(
            "[tyto] WARNING: migration '{}' checksum mismatch (stored={}, current={}). \
             The migration file was modified after it was applied.",
            migration.name, stored, current
        );
    }
    Ok(())
}

fn sha256(data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    hex::encode(hasher.finalize())
}
