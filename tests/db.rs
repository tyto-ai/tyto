use turso::Connection;

pub async fn seed_memory(
    db: &tyto::db::Db,
    id: &str,
    project_id: &str,
) {
    let _: u64 = db.conn
        .execute(
            "INSERT INTO memories \
             (id, project_id, type, title, content, created_at, updated_at, content_hash) \
             VALUES (?1, ?2, 'fact', 'Title', 'Content', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z', 'hash')",
            (
                id.to_string(),
                project_id.to_string(),
            ),
        )
        .await
        .unwrap();
}

pub struct TestDb {
    pub conn: Connection,
    #[allow(dead_code)]
    _db: turso::Database,
}

pub async fn setup() -> TestDb {
    let db = turso::Builder::new_local(":memory:").build().await.unwrap();
    let conn = db.connect().unwrap();
    let _: () = tyto::migrations::run(&conn).await.unwrap();
    TestDb { conn, _db: db }
}
