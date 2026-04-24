use turso::Builder;
use anyhow::{Result};
use std::fs;

#[tokio::main]
async fn main() -> Result<()> {
    println!("=== Turso Library Issues Reproduction ===\n");

    // Issue 1: WITHOUT ROWID
    {
        let db_path = "repro_without_rowid.db";
        let _ = fs::remove_file(db_path);

        println!("Issue 1: WITHOUT ROWID tables cause failures");
        
        let db = Builder::new_local(db_path)
            .experimental_multiprocess_wal(true)
            .build()
            .await?;
        let conn = db.connect()?;

        println!("Attempting to CREATE a WITHOUT ROWID table via Turso connection...");
        match conn.execute("CREATE TABLE test (id TEXT PRIMARY KEY) WITHOUT ROWID", ()).await {
            Ok(_) => println!("  SUCCESS: Created WITHOUT ROWID table"),
            Err(e) => println!("  EXPECTED FAILURE on CREATE: {}", e),
        }

        println!("\nCreating a WITHOUT ROWID table using rusqlite for re-open test...");
        {
            let conn = rusqlite::Connection::open(db_path)?;
            conn.execute("CREATE TABLE test2 (id TEXT PRIMARY KEY) WITHOUT ROWID", [])?;
        }

        println!("Attempting to open existing database with WITHOUT ROWID via Turso...");
        let db = Builder::new_local(db_path)
            .experimental_multiprocess_wal(true)
            .build()
            .await;

        match db {
            Ok(db) => {
                println!("  SUCCESS: Opened database");
                let conn = db.connect()?;
                println!("Attempting to query the WITHOUT ROWID table...");
                match conn.query("SELECT * FROM test2", ()).await {
                    Ok(_) => println!("  SUCCESS: Queried WITHOUT ROWID table"),
                    Err(e) => println!("  EXPECTED FAILURE on query: {}", e),
                }
            }
            Err(e) => println!("  EXPECTED FAILURE on Builder::build(): {}", e),
        }
    }

    println!("\n------------------------------------------\n");

    // Issue 2: Unexpected row during execution (PRAGMAs)
    {
        let db_path = "repro_pragma.db";
        let _ = fs::remove_file(db_path);

        println!("Issue 2: execute() fails if a statement returns rows (like some PRAGMAs)");
        let db = Builder::new_local(db_path)
            .experimental_multiprocess_wal(true)
            .build()
            .await?;
        let conn = db.connect()?;

        println!("Executing 'PRAGMA journal_mode=WAL;' via execute()...");
        match conn.execute("PRAGMA journal_mode=WAL;", ()).await {
            Ok(_) => println!("  SUCCESS: execute() worked"),
            Err(e) => println!("  EXPECTED FAILURE: {}", e),
        }
        
        println!("\nExecuting 'PRAGMA journal_mode=WAL;' via execute_batch()...");
        match conn.execute_batch("PRAGMA journal_mode=WAL;").await {
            Ok(_) => println!("  SUCCESS: execute_batch() worked"),
            Err(e) => println!("  EXPECTED FAILURE: {}", e),
        }

        println!("\nExecuting 'PRAGMA journal_mode=WAL;' via pragma_update()...");
        match conn.pragma_update("journal_mode", "WAL").await {
             Ok(_) => println!("  SUCCESS: pragma_update worked"),
             Err(e) => println!("  FAILURE: {}", e),
        }
    }

    println!("\nIssue 3: FTS5 and shadow tables");
    {
        let db_path = "repro_fts5.db";
        let _ = fs::remove_file(db_path);

        println!("Creating an FTS5 table using rusqlite...");
        {
            let conn = rusqlite::Connection::open(db_path)?;
            conn.execute_batch(
                "CREATE TABLE memories (id TEXT PRIMARY KEY, title TEXT);
                 CREATE VIRTUAL TABLE memories_fts USING fts5(title, content=memories);"
            )?;
        }

        println!("Attempting to open with Turso...");
        let db = Builder::new_local(db_path)
            .experimental_multiprocess_wal(true)
            .build()
            .await;

        match db {
            Ok(_) => println!("  SUCCESS: Opened database (Turso might be skipping virtual/shadow table parsing)"),
            Err(e) => println!("  FAILURE: {}", e),
        }
    }

    Ok(())
}
