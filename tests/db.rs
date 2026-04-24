use turso::Connection;
use tyto::store::{StoreRequest, new_write_lock};

fn dummy_embedding() -> Vec<f32> {
    vec![0.1f32; 384]
}

fn basic_request(content: &str) -> StoreRequest {
    StoreRequest {
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

async fn seed_memory(conn: &Connection, id: &str, project_id: &str) {
    conn.execute(
        "INSERT INTO memories \
         (id, project_id, type, title, content, created_at, updated_at, content_hash) \
         VALUES (?1, ?2, 'fact', 'Title', 'Content', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z', 'hash')",
        (id.to_string(), project_id.to_string()),
    )
    .await
    .unwrap();
}

struct TestDb {
    conn: Connection,
    #[allow(dead_code)]
    _db: turso::Database,
}

async fn setup() -> TestDb {
    let db = turso::Builder::new_local(":memory:").build().await.unwrap();
    let conn = db.connect().unwrap();
    tyto::migrations::run(&conn).await.unwrap();
    TestDb { conn, _db: db }
}

// --- migrations ---

#[tokio::test]
async fn migrations_run_on_fresh_db() {
    setup().await;
}

#[tokio::test]
async fn migrations_are_idempotent() {
    let db = setup().await;
    tyto::migrations::run(&db.conn).await.unwrap();
}

// --- delete_batch ---

#[tokio::test]
async fn delete_batch_soft_deletes() {
    let db = setup().await;
    seed_memory(&db.conn, "id-a", "test-project").await;
    seed_memory(&db.conn, "id-b", "test-project").await;

    let ids = vec!["id-a".to_string(), "id-b".to_string()];
    let n = tyto::retrieve::delete_batch(&db.conn, &ids, "test-project").await.unwrap();
    assert_eq!(n, 2);

    let results = tyto::retrieve::get_full_batch(&db.conn, &ids, "test-project").await.unwrap();
    assert!(results.iter().all(|m| m.status == "deleted"));
}

#[tokio::test]
async fn delete_batch_missing_ids_not_counted() {
    let db = setup().await;
    seed_memory(&db.conn, "id-a", "test-project").await;

    let ids = vec!["id-a".to_string(), "nonexistent".to_string()];
    let n = tyto::retrieve::delete_batch(&db.conn, &ids, "test-project").await.unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn delete_batch_is_isolated_by_project_id() {
    let db = setup().await;
    seed_memory(&db.conn, "id-a", "test-project").await;

    let ids = vec!["id-a".to_string()];
    let n = tyto::retrieve::delete_batch(&db.conn, &ids, "other-project").await.unwrap();
    assert_eq!(n, 0, "foreign project must not delete another project's memory");

    let results = tyto::retrieve::get_full_batch(&db.conn, &ids, "test-project").await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, "active");
}

// --- get_full_batch ---

#[tokio::test]
async fn get_full_batch_returns_all_found() {
    let db = setup().await;
    seed_memory(&db.conn, "id-a", "test-project").await;
    seed_memory(&db.conn, "id-b", "test-project").await;

    let ids = vec!["id-a".to_string(), "id-b".to_string()];
    let results = tyto::retrieve::get_full_batch(&db.conn, &ids, "test-project").await.unwrap();
    assert_eq!(results.len(), 2);
}

#[tokio::test]
async fn get_full_batch_is_isolated_by_project_id() {
    let db = setup().await;
    seed_memory(&db.conn, "id-a", "test-project").await;

    let ids = vec!["id-a".to_string()];
    let results = tyto::retrieve::get_full_batch(&db.conn, &ids, "other-project").await.unwrap();
    assert!(results.is_empty(), "foreign project must not read another project's memory");
}

// --- pin_batch ---

#[tokio::test]
async fn pin_batch_pins_and_unpins() {
    let db = setup().await;
    seed_memory(&db.conn, "id-a", "test-project").await;

    let ids = vec!["id-a".to_string()];
    let n = tyto::retrieve::pin_batch(&db.conn, &ids, "test-project", true).await.unwrap();
    assert_eq!(n, 1);
    let results = tyto::retrieve::get_full_batch(&db.conn, &ids, "test-project").await.unwrap();
    assert!(results.iter().all(|m| m.pinned));

    let n = tyto::retrieve::pin_batch(&db.conn, &ids, "test-project", false).await.unwrap();
    assert_eq!(n, 1);
    let results = tyto::retrieve::get_full_batch(&db.conn, &ids, "test-project").await.unwrap();
    assert!(results.iter().all(|m| !m.pinned));
}

#[tokio::test]
async fn pin_batch_is_isolated_by_project_id() {
    let db = setup().await;
    seed_memory(&db.conn, "id-a", "test-project").await;

    let ids = vec!["id-a".to_string()];
    let n = tyto::retrieve::pin_batch(&db.conn, &ids, "other-project", true).await.unwrap();
    assert_eq!(n, 0, "foreign project must not pin another project's memory");
}

#[tokio::test]
async fn pin_batch_skips_deleted_memories() {
    let db = setup().await;
    seed_memory(&db.conn, "id-a", "test-project").await;

    let ids = vec!["id-a".to_string()];
    tyto::retrieve::delete_batch(&db.conn, &ids, "test-project").await.unwrap();

    let n = tyto::retrieve::pin_batch(&db.conn, &ids, "test-project", true).await.unwrap();
    assert_eq!(n, 0);
}

// --- list ---

#[tokio::test]
async fn list_returns_stored_memories() {
    let db = setup().await;
    seed_memory(&db.conn, "id-a", "test-project").await;

    let results = tyto::retrieve::list(&db.conn, "test-project", None, &[], 10, 0.0).await.unwrap();
    assert!(!results.is_empty());
    assert!(results.iter().any(|r| r.id == "id-a"));
}

#[tokio::test]
async fn list_type_filter() {
    let db = setup().await;
    // seed_memory inserts type='fact'; confirm filter works
    seed_memory(&db.conn, "id-a", "test-project").await;

    let facts = tyto::retrieve::list(&db.conn, "test-project", Some("fact"), &[], 10, 0.0).await.unwrap();
    assert!(facts.iter().all(|r| r.memory_type == "fact"));

    let decisions = tyto::retrieve::list(&db.conn, "test-project", Some("decision"), &[], 10, 0.0).await.unwrap();
    assert!(decisions.is_empty());
}

// --- store + get_full roundtrip ---

#[tokio::test]
async fn store_and_get_full_roundtrip() {
    let db = setup().await;
    let lock = new_write_lock();

    let result = tyto::store::store_memory(
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

    let mem = tyto::retrieve::get_full_batch(&db.conn, &[result.id.clone()], "test-project")
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(mem.content, "This is a test memory about Rust");
    assert_eq!(mem.memory_type, "decision");
    assert!((mem.importance - 0.7).abs() < 0.001);
}

#[tokio::test]
async fn store_dedup_within_window_returns_same_id() {
    let db = setup().await;
    let lock = new_write_lock();

    let r1 = tyto::store::store_memory(&db.conn, dummy_embedding(), &lock, basic_request("Duplicate content"), 30)
        .await
        .unwrap();
    let r2 = tyto::store::store_memory(&db.conn, dummy_embedding(), &lock, basic_request("Duplicate content"), 30)
        .await
        .unwrap();

    assert_eq!(r1.id, r2.id, "same content in same session should deduplicate");
    assert!(!r2.upserted);
}

#[tokio::test]
async fn topic_key_upsert_updates_content() {
    let db = setup().await;
    let lock = new_write_lock();

    let mut req1 = basic_request("Original content");
    req1.topic_key = Some("my-topic".to_string());
    let r1 = tyto::store::store_memory(&db.conn, dummy_embedding(), &lock, req1, 30)
        .await
        .unwrap();

    // Different session to bypass dedup window.
    let mut req2 = basic_request("Updated content");
    req2.topic_key = Some("my-topic".to_string());
    req2.session_id = "other-session".to_string();
    let r2 = tyto::store::store_memory(&db.conn, dummy_embedding(), &lock, req2, 30)
        .await
        .unwrap();

    assert_eq!(r1.id, r2.id, "upsert should keep the same ID");
    assert!(r2.upserted);

    let mem = tyto::retrieve::get_full_batch(&db.conn, &[r1.id.clone()], "test-project")
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(mem.content, "Updated content");
}

#[tokio::test]
async fn store_keyword_search_finds_by_word() {
    let db = setup().await;
    let lock = new_write_lock();

    let mut req = basic_request("rustaceans love ownership and borrowing");
    req.title = "Rust ownership".to_string();
    tyto::store::store_memory(&db.conn, dummy_embedding(), &lock, req, 30)
        .await
        .unwrap();

    let results = tyto::retrieve::search_bm25(&db.conn, "ownership", "test-project", 5)
        .await
        .unwrap();

    assert!(!results.is_empty(), "keyword search should find the stored memory");
    assert!(results.iter().any(|r| r.title == "Rust ownership"));
}
