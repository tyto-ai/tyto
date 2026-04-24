# Technical Report: Turso 0.6.0 (Limbo) Migration Experiment

## Objective
Migrate `tyto` from the C-based `libsql` crate to the pure-Rust **Turso** library (version `0.6.0-pre.22`, codenamed **Limbo**). This experiment sought to evaluate Turso's readiness for multi-process concurrency and its compatibility with our existing SQLite/FTS5 schema.

---

## Phase 1: Multi-process Concurrency Verification

Previous versions (0.5.x) used exclusive `fcntl` locks. We tested 0.6.0-pre.22 using a worker-based reproduction script.

### Discovery
Turso 0.6.0-pre.22 supports multi-process concurrency on Linux but requires explicit configuration and handling.

### Verification Code (`examples/turso_locking.rs`)
```rust
async fn run_worker(db_path: &str, id: &str) -> Result<()> {
    // 1. MUST enable experimental_multiprocess_wal
    let db = turso::Builder::new_local(db_path)
        .experimental_multiprocess_wal(true)
        .build().await?;

    // 2. MUST implement retry loops for connect() and build()
    let mut conn = None;
    for i in 0..10 {
        match db.connect() {
            Ok(c) => { conn = Some(c); break; }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    let conn = conn.expect("Failed to connect after 10 attempts");
    // ... operations ...
}
```

### Success Output
```text
Worker 1: Opening database...
Worker 2: Opening database...
Worker 2: Build attempt 1 failed: Locking error: Failed locking file. Retrying...
Worker 1: Success. Row count is 1
Worker 2: Success. Row count is 2
SUCCESS: Multi-process concurrency works!
```

---

## Phase 2: The `WITHOUT ROWID` & FTS5 Crisis

### The Failure
Replicas failed to sync immediately upon startup:
```text
[2026-04-23 13:27:01] tyto: replica open failed (Failed to build replica: sync engine operation failed: database sync engine error: unable to open database file: Parse error: WITHOUT ROWID tables are not supported)
```

### The "Gotcha"
SQLite's **FTS5 extension** uses `WITHOUT ROWID` for its internal shadow tables. Because Turso (Limbo) does not yet support `WITHOUT ROWID`, any database containing an FTS5 table is unreadable by the library.

### Shadow Tables Identified
-   `memories_fts_content`
-   `memories_fts_idx`
-   `memories_fts_config`
-   **`libsql_vector_meta_shadow`**: An internal Turso/libsql metadata table that also uses `WITHOUT ROWID`.

### Safe Recovery Workflow
We developed a "Remote Cleanup" strategy to salvage data:
1.  **Local Backup:** Connect via `libsql` (the old crate) and pull data to a local `.db` file.
    *   **Failure:** `libsql` panicked with `internal error: entered unreachable code: invalid value type` when reading vector blobs.
    *   **Fix:** Use `CAST(embedding AS BLOB)` in the SELECT query to force standard byte handling.
2.  **Schema Purge:** Explicitly `DROP` all FTS5 tables and the `libsql_vector_meta_shadow` table on the remote primary.
3.  **Wipe & Sync:** Delete local `.replica.db*` files and let Turso perform a fresh, clean sync.

---

## Phase 3: The FTS Pivot

### Failure: `no such module: fts5`
The Turso library is a rewrite and does not include the C-based FTS5 module. It uses a native **Tantivy** engine.

### Attempted Syntax: `CREATE INDEX ... USING fts`
We tested the new native syntax:
```sql
-- Enabling required: experimental_index_method(true)
CREATE INDEX idx ON test USING fts(content);
```
**The Discovery:** While functional in local databases, `USING fts` is an experimental feature and **not yet supported in Turso Replica mode**.

### Final Strategy
-   **Replica Search:** Reverted to `LIKE` queries for keyword matching.
-   **Local Search:** Switched to a hybrid backend (see Phase 5).

---

## Phase 4: "Unexpected row during execution" Saga

### The Error
```text
Error: unexpected row during execution
```
This occurred in `src/index/schema.rs` during local `index.db` initialization when executing `PRAGMA` statements.

### Root Cause
A bug/strictness in the Turso Rust driver where `.execute()` (which expects only a "Done" status) fails if a statement returns *any* metadata or status rows. Common SQLite pragmas like `journal_mode=WAL` return a row containing the new value.

### The Fix: `pragma_update()`
The Turso library provides a specialized `.pragma_update(name, value)` method designed specifically for this purpose. It correctly handles the returning rows and is the idiomatic way to update database pragmas.

```rust
// SUCCESS: The correct way to update pragmas in the Turso driver
conn.pragma_update("journal_mode", "WAL").await?;
conn.pragma_update("busy_timeout", "5000").await?;
```

---

## Phase 5: The `Rusqlite` Experiment (Failed)

### The Idea
Since `index.db` is local-only, we tried switching it to `rusqlite` for stability and FTS5 support.

### The Failure
We hit a wall with **thread-safety and async boundaries**:
```text
error[E0277]: `RefCell<...>` cannot be shared between threads safely
note: required for `&rusqlite::Connection` to implement `Send`
```
**Lesson:** `rusqlite::Connection` is not `Sync`. Holding a `MutexGuard` to a connection across a `.await` point (even in `spawn_blocking`) is extremely complex and led to fragile code. We reverted this to stay pure-async.

---

## Phase 6: Miscellaneous Gory Details

### 1. `IntoParams` Tuple Limit
When copying memories in `src/remote.rs`, we tried using a large tuple for the 18 columns.
**The Failure:** `the trait bound (..., ...): IntoParams is not satisfied`.
**The Discovery:** The Turso crate's `IntoParams` trait is only implemented for tuples up to size **16**. 
**The Fix:** Use `turso::params_from_iter(vec![...])`.

### 2. Environment Dependencies
-   **SSL:** Turso's `sync` feature depends on `native-tls`, requiring `openssl` and `pkg-config` in the host environment.
-   **Workflow:** Changes to `devenv.nix` are **not hot-reloaded**. The agent must be restarted to see the new libraries.

### 3. Startup Race Condition
**Crash 2:** `main DB file doesn't exists, but metadata is`.
**Cause:** `libsql`'s `sync_interval` spawned a detached background task. If a startup opened the DB, hit an error, and called `purge_replica_files`, the background task could recreate the `.db` file *after* the purge but *before* the retry, causing an "invalid state" error.
**The Fix:** Remove `sync_interval` and implement a manual sync loop in `src/serve.rs`.

---

## Final Hybrid Architecture

| Database | Library | Logic |
| :--- | :--- | :--- |
| **Memory (Replica)** | **Turso** | Handles cloud sync; uses `LIKE` for search. |
| **Code Index (Local)** | **Turso** | Managed locally; uses native `USING fts` index. |

### Final Stability Fix for Index Schema
We discovered that Turso's `pragma_update` is the ONLY stable way to run pragmas without "unexpected row" errors. DDL should still be run via `execute_batch` but must not contain returning `PRAGMA`s.

```rust
conn.pragma_update("journal_mode", "WAL").await?;
conn.pragma_update("busy_timeout", "5000").await?;

conn.execute_batch(
    "CREATE TABLE IF NOT EXISTS ...;
     CREATE INDEX ... USING fts(...);"
).await?;
```

---

---

## Appendix: Minimal Isolated Reproduction

To facilitate debugging and potentially report these issues to the Turso maintainers, we created a minimal reproduction script.

### Key Findings
1.  **`WITHOUT ROWID` Parsing**: The Turso driver fails during `Builder::build()` if the database file contains a `WITHOUT ROWID` table. This is likely because Limbo's parser/engine does not yet support the structural representation of these tables.
2.  **Row Strictness**: The `.execute()` and `.execute_batch()` methods in the Turso driver are strictly intended for statements that return no data. Common SQLite setup commands like `PRAGMA journal_mode=WAL;` return a row containing the new mode, which triggers an `unexpected row during execution` error. **Fix:** Use the specialized `.pragma_update(name, value)` method.

### Reproduction Code (`examples/repro_turso_issues.rs`)
```rust
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

        println!("Issue 1: WITHOUT ROWID tables cause Builder::build() to fail");
        println!("Creating a WITHOUT ROWID table using rusqlite...");
        {
            let conn = rusqlite::Connection::open(db_path)?;
            conn.execute("CREATE TABLE test (id TEXT PRIMARY KEY) WITHOUT ROWID", [])?;
        }

        println!("Attempting to open with Turso...");
        let db = Builder::new_local(db_path)
            .experimental_multiprocess_wal(true)
            .build()
            .await;

        match db {
            Ok(_) => println!("  SUCCESS: Opened database (unexpected if WITHOUT ROWID is unsupported)"),
            Err(e) => println!("  EXPECTED FAILURE: {}", e),
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
```
