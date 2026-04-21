/// Integration tests using an in-memory libsql database.
/// These cover store, retrieve, and migrations together.
use tyto::{migrations, retrieve, store};

/// Insert a minimal memory row directly, bypassing store (no embedding needed).
async fn insert_raw(db: &TestDb, id: &str, title: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    db.conn
        .execute(
            "INSERT INTO memories \
             (id, project_id, type, title, content, created_at, updated_at, content_hash) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            libsql::params![
                id.to_string(),
                "test-project".to_string(),
                "fact".to_string(),
                title.to_string(),
                "test content".to_string(),
                now.clone(),
                now,
                id.to_string()
            ],
        )
        .await
        .unwrap();
}

struct TestDb {
    pub conn: libsql::Connection,
    // Keep _db alive: dropping it destroys the in-memory database.
    _db: libsql::Database,
}

async fn migrated_db() -> TestDb {
    let db = libsql::Builder::new_local(":memory:").build().await.unwrap();
    let conn = db.connect().unwrap();
    migrations::run(&conn).await.unwrap();
    TestDb { conn, _db: db }
}

/// A dummy 384-dim embedding (all 0.1). Used where vector content does not matter.
fn dummy_embedding() -> Vec<f32> {
    vec![0.1f32; 384]
}

fn basic_request(content: &str) -> store::StoreRequest {
    store::StoreRequest {
        content: content.to_string(),
        memory_type: "decision".to_string(),
        title: "Test memory".to_string(),
        tags: vec![],
        topic_key: None,
        project_id: "test-project".to_string(),
        session_id: "test-session".to_string(),
        importance: Some(0.7),
        facts: vec![],
        source: None,
        pinned: None,
    }
}

// --- migrations ---

#[tokio::test]
async fn migrations_run_on_fresh_db() {
    migrated_db().await; // must not panic or error
}

#[tokio::test]
async fn migrations_are_idempotent() {
    let db = migrated_db().await;
    // Running a second time must be a no-op, not an error.
    migrations::run(&db.conn).await.unwrap();
}

// --- store + get_full roundtrip ---

#[tokio::test]
async fn store_and_get_full_roundtrip() {
    let db = migrated_db().await;
    let lock = store::new_write_lock();

    let result = store::store_memory(
        &db.conn,
        dummy_embedding(),
        &lock,
        basic_request("This is a test memory about Rust"),
        30,
    )
    .await
    .unwrap();

    assert!(!result.id.is_empty());
    assert!(!result.upserted);

    let mem = retrieve::get_full_batch(&db.conn, &[result.id.clone()], "test-project").await.unwrap().into_iter().next().unwrap();
    assert_eq!(mem.content, "This is a test memory about Rust");
    assert_eq!(mem.memory_type, "decision");
    assert!((mem.importance - 0.7).abs() < 0.001);
}

#[tokio::test]
async fn get_full_batch_returns_empty_for_unknown_ids() {
    let db = migrated_db().await;
    let results = retrieve::get_full_batch(&db.conn, &["does-not-exist".to_string()], "test-project").await.unwrap();
    assert!(results.is_empty());
}

// --- dedup ---

#[tokio::test]
async fn store_dedup_within_window_returns_same_id() {
    let db = migrated_db().await;
    let lock = store::new_write_lock();

    let r1 = store::store_memory(&db.conn, dummy_embedding(), &lock, basic_request("Duplicate content"), 30)
        .await
        .unwrap();
    let r2 = store::store_memory(&db.conn, dummy_embedding(), &lock, basic_request("Duplicate content"), 30)
        .await
        .unwrap();

    assert_eq!(r1.id, r2.id, "same content in same session should deduplicate");
    assert!(!r2.upserted);
}

// --- topic-key upsert ---

#[tokio::test]
async fn topic_key_upsert_updates_content() {
    let db = migrated_db().await;
    let lock = store::new_write_lock();

    let mut req1 = basic_request("Original content");
    req1.topic_key = Some("my-topic".to_string());
    let r1 = store::store_memory(&db.conn, dummy_embedding(), &lock, req1, 30)
        .await
        .unwrap();

    // Use a different session_id to bypass the dedup window.
    let mut req2 = basic_request("Updated content");
    req2.topic_key = Some("my-topic".to_string());
    req2.session_id = "other-session".to_string();
    let r2 = store::store_memory(&db.conn, dummy_embedding(), &lock, req2, 30)
        .await
        .unwrap();

    assert_eq!(r1.id, r2.id, "upsert should keep the same ID");
    assert!(r2.upserted);

    let mut batch = retrieve::get_full_batch(&db.conn, &[r1.id.clone()], "test-project").await.unwrap();
    let mem = batch.pop().unwrap();
    assert_eq!(mem.content, "Updated content");
}

// --- list ---

#[tokio::test]
async fn list_returns_stored_memories() {
    let db = migrated_db().await;
    let lock = store::new_write_lock();
    store::store_memory(&db.conn, dummy_embedding(), &lock, basic_request("Listed memory"), 30)
        .await
        .unwrap();

    let results = retrieve::list(&db.conn, "test-project", None, &[], 10, 0.0)
        .await
        .unwrap();

    assert!(!results.is_empty());
    assert!(results.iter().any(|r| r.title == "Test memory"));
}

#[tokio::test]
async fn list_type_filter() {
    let db = migrated_db().await;
    let lock = store::new_write_lock();

    store::store_memory(&db.conn, dummy_embedding(), &lock, basic_request("Decision memory"), 30)
        .await
        .unwrap();

    let mut gotcha_req = basic_request("Gotcha memory");
    gotcha_req.memory_type = "gotcha".to_string();
    gotcha_req.session_id = "s2".to_string();
    store::store_memory(&db.conn, dummy_embedding(), &lock, gotcha_req, 30)
        .await
        .unwrap();

    let decisions = retrieve::list(&db.conn, "test-project", Some("decision"), &[], 10, 0.0)
        .await
        .unwrap();
    assert!(decisions.iter().all(|r| r.memory_type == "decision"));

    let gotchas = retrieve::list(&db.conn, "test-project", Some("gotcha"), &[], 10, 0.0)
        .await
        .unwrap();
    assert!(gotchas.iter().all(|r| r.memory_type == "gotcha"));
}

// --- search_bm25 ---

#[tokio::test]
async fn search_bm25_finds_by_keyword() {
    let db = migrated_db().await;
    let lock = store::new_write_lock();

    let mut req = basic_request("rustaceans love ownership and borrowing");
    req.title = "Rust ownership".to_string();
    store::store_memory(&db.conn, dummy_embedding(), &lock, req, 30)
        .await
        .unwrap();

    let results = retrieve::search_bm25(&db.conn, "ownership", "test-project", 5)
        .await
        .unwrap();

    assert!(!results.is_empty(), "BM25 should find the stored memory by keyword");
    assert!(results.iter().any(|r| r.title == "Rust ownership"));
}

// --- get_full_batch ---

#[tokio::test]
async fn get_full_batch_returns_all_found() {
    let db = migrated_db().await;
    insert_raw(&db, "id-a", "Memory A").await;
    insert_raw(&db, "id-b", "Memory B").await;

    let ids = vec!["id-a".to_string(), "id-b".to_string()];
    let results = retrieve::get_full_batch(&db.conn, &ids, "test-project").await.unwrap();

    assert_eq!(results.len(), 2);
    let titles: Vec<&str> = results.iter().map(|m| m.title.as_str()).collect();
    assert!(titles.contains(&"Memory A"));
    assert!(titles.contains(&"Memory B"));
}

#[tokio::test]
async fn get_full_batch_omits_missing_ids() {
    let db = migrated_db().await;
    insert_raw(&db, "id-a", "Memory A").await;

    let ids = vec!["id-a".to_string(), "nonexistent".to_string()];
    let results = retrieve::get_full_batch(&db.conn, &ids, "test-project").await.unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, "id-a");
}

// --- pin_batch ---

#[tokio::test]
async fn pin_batch_pins_and_unpins() {
    let db = migrated_db().await;
    insert_raw(&db, "id-a", "Memory A").await;
    insert_raw(&db, "id-b", "Memory B").await;

    let ids = vec!["id-a".to_string(), "id-b".to_string()];

    let n = retrieve::pin_batch(&db.conn, &ids, "test-project", true).await.unwrap();
    assert_eq!(n, 2);
    let results = retrieve::get_full_batch(&db.conn, &ids, "test-project").await.unwrap();
    assert!(results.iter().all(|m| m.pinned));

    let n = retrieve::pin_batch(&db.conn, &ids, "test-project", false).await.unwrap();
    assert_eq!(n, 2);
    let results = retrieve::get_full_batch(&db.conn, &ids, "test-project").await.unwrap();
    assert!(results.iter().all(|m| !m.pinned));
}

#[tokio::test]
async fn pin_batch_missing_ids_not_counted() {
    let db = migrated_db().await;
    insert_raw(&db, "id-a", "Memory A").await;

    let ids = vec!["id-a".to_string(), "nonexistent".to_string()];
    let n = retrieve::pin_batch(&db.conn, &ids, "test-project", true).await.unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn pin_batch_skips_deleted_memories() {
    let db = migrated_db().await;
    insert_raw(&db, "id-a", "Memory A").await;

    let ids = vec!["id-a".to_string()];
    retrieve::delete_batch(&db.conn, &ids, "test-project").await.unwrap();

    let n = retrieve::pin_batch(&db.conn, &ids, "test-project", true).await.unwrap();
    assert_eq!(n, 0);
}

// --- delete_batch ---

#[tokio::test]
async fn delete_batch_soft_deletes() {
    let db = migrated_db().await;
    insert_raw(&db, "id-a", "Memory A").await;
    insert_raw(&db, "id-b", "Memory B").await;

    let ids = vec!["id-a".to_string(), "id-b".to_string()];
    let n = retrieve::delete_batch(&db.conn, &ids, "test-project").await.unwrap();
    assert_eq!(n, 2);

    let results = retrieve::get_full_batch(&db.conn, &ids, "test-project").await.unwrap();
    assert!(results.iter().all(|m| m.status == "deleted"));
}

#[tokio::test]
async fn delete_batch_missing_ids_not_counted() {
    let db = migrated_db().await;
    insert_raw(&db, "id-a", "Memory A").await;

    let ids = vec!["id-a".to_string(), "nonexistent".to_string()];
    let n = retrieve::delete_batch(&db.conn, &ids, "test-project").await.unwrap();
    assert_eq!(n, 1);
}

// --- project_id isolation ---

/// A foreign project must not be able to read memories belonging to another project,
/// even when it supplies the exact ID.
#[tokio::test]
async fn get_full_batch_is_isolated_by_project_id() {
    let db = migrated_db().await;
    insert_raw(&db, "id-a", "Memory A").await; // belongs to "test-project"

    let ids = vec!["id-a".to_string()];
    let results = retrieve::get_full_batch(&db.conn, &ids, "other-project").await.unwrap();
    assert!(results.is_empty(), "foreign project must not read another project's memory");
}

/// A foreign project must not be able to pin memories belonging to another project.
#[tokio::test]
async fn pin_batch_is_isolated_by_project_id() {
    let db = migrated_db().await;
    insert_raw(&db, "id-a", "Memory A").await; // belongs to "test-project"

    let ids = vec!["id-a".to_string()];
    let n = retrieve::pin_batch(&db.conn, &ids, "other-project", true).await.unwrap();
    assert_eq!(n, 0, "foreign project must not pin another project's memory");
}

/// A foreign project must not be able to delete memories belonging to another project.
#[tokio::test]
async fn delete_batch_is_isolated_by_project_id() {
    let db = migrated_db().await;
    insert_raw(&db, "id-a", "Memory A").await; // belongs to "test-project"

    let ids = vec!["id-a".to_string()];
    let n = retrieve::delete_batch(&db.conn, &ids, "other-project").await.unwrap();
    assert_eq!(n, 0, "foreign project must not delete another project's memory");

    // Verify the memory is untouched.
    let results = retrieve::get_full_batch(&db.conn, &ids, "test-project").await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, "active");
}
