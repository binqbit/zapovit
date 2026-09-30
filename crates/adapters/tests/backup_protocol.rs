use adapters::{backup, postgres::PgDatabase};
use application::{BlobStore, Database, FileObject, put};
use domain::Id;
use sha2::{Digest, Sha256};
use std::time::Duration;

#[derive(Default)]
struct MemoryObjects(tokio::sync::Mutex<std::collections::BTreeMap<String, Vec<u8>>>);
#[async_trait::async_trait]
impl BlobStore for MemoryObjects {
    async fn put(&self, key: &str, value: &[u8]) -> application::Result<()> {
        self.0.lock().await.insert(key.into(), value.into());
        Ok(())
    }
    async fn get(&self, key: &str, max: usize) -> application::Result<Vec<u8>> {
        let value = self
            .0
            .lock()
            .await
            .get(key)
            .cloned()
            .ok_or(application::Error::NotFound)?;
        if value.len() > max {
            return Err(application::Error::Storage);
        }
        Ok(value)
    }
    async fn delete(&self, key: &str) -> application::Result<()> {
        self.0.lock().await.remove(key);
        Ok(())
    }
    async fn exists(&self, key: &str) -> application::Result<bool> {
        Ok(self.0.lock().await.contains_key(key))
    }
}

#[tokio::test]
#[ignore = "requires isolated local PostgreSQL"]
async fn backup_pages_beyond_old_ten_thousand_row_limit() {
    let db = database().await;
    let blobs = MemoryObjects::default();
    let mut objects = Vec::new();
    for n in 1..=10_001 {
        objects.push(FileObject {
            id: Id::from_u128(n),
            plan_id: Id::from_u128(999_999),
            draft_id: Id::from_u128(999_998),
            operation_id: Id::new_v4(),
            key: format!("draft/{n}"),
            size: 0,
            digest: String::new(),
            state: "pending".into(),
            created_at: 0,
            due_at: 0,
        });
    }
    let bytes = b"synthetic ciphertext after 10001 ledger entries";
    let last = FileObject {
        id: Id::from_u128(10_002),
        plan_id: Id::from_u128(999_999),
        draft_id: Id::from_u128(999_998),
        operation_id: Id::new_v4(),
        key: "sealed/last".into(),
        size: bytes.len() as u64,
        digest: hex::encode(Sha256::digest(bytes)),
        state: "sealed".into(),
        created_at: 0,
        due_at: 0,
    };
    objects.push(last.clone());
    sqlx::query("INSERT INTO file_objects(id,scope_id,data) SELECT (value->>'id')::uuid,(value->>'plan_id')::uuid,value FROM jsonb_array_elements($1::jsonb)")
        .bind(sqlx::types::Json(objects)).execute(&db.pool).await.unwrap();
    blobs.put(&last.key, bytes).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let exported = dir.path().join("objects");
    assert!(
        backup::export_objects_from(&db, &blobs, &exported)
            .await
            .is_err()
    );
    let session = backup::begin_backup_on_db(&db).await.unwrap();
    assert!(
        backup::export_objects_from(&db, &blobs, &exported)
            .await
            .is_err()
    );
    backup::wait_for_drain_on_db(&db, session, Duration::from_secs(2))
        .await
        .unwrap();
    backup::export_objects_from(&db, &blobs, &exported)
        .await
        .unwrap();
    blobs.delete(&last.key).await.unwrap();
    backup::import_objects_into(&db, &blobs, &exported)
        .await
        .unwrap();
    assert_eq!(blobs.get(&last.key, 1024).await.unwrap(), bytes);
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(exported.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["objects"], 1);
    // The previous export format stored the object array directly in manifest.json.
    std::fs::copy(
        exported.join("manifest-0000.json"),
        exported.join("manifest.json"),
    )
    .unwrap();
    blobs.delete(&last.key).await.unwrap();
    backup::import_objects_into(&db, &blobs, &exported)
        .await
        .unwrap();
    assert_eq!(blobs.get(&last.key, 1024).await.unwrap(), bytes);
}

#[tokio::test]
#[ignore = "requires isolated local PostgreSQL and Garage"]
async fn garage_backup_restores_exact_ciphertext_and_rejects_tampered_page() {
    use adapters::{settings::StorageCredentials, storage::S3Storage};
    let endpoint = std::env::var("TEST_S3_ENDPOINT").expect("TEST_S3_ENDPOINT");
    let parsed = reqwest::Url::parse(&endpoint).unwrap();
    assert!(matches!(
        parsed.host_str(),
        Some("127.0.0.1" | "localhost" | "[::1]")
    ));
    let path = std::path::PathBuf::from(
        std::env::var("TEST_S3_CREDENTIALS_FILE").expect("TEST_S3_CREDENTIALS_FILE"),
    );
    assert!(path.is_absolute());
    let credentials: StorageCredentials =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let store = S3Storage::new(
        &endpoint,
        "garage",
        "zapovit",
        &credentials.access_key_id,
        &credentials.secret_access_key,
    )
    .unwrap();
    let db = database().await;
    let session = backup::begin_backup_on_db(&db).await.unwrap();
    backup::wait_for_drain_on_db(&db, session, Duration::from_secs(2))
        .await
        .unwrap();
    let id = Id::new_v4();
    // Object backup treats these synthetic bytes as opaque ciphertext; crypto
    // and pipeline tests independently check encryption and decryption.
    let bytes = b"synthetic opaque encrypted object fixture".to_vec();
    let object = FileObject {
        id,
        plan_id: Id::new_v4(),
        draft_id: Id::new_v4(),
        operation_id: Id::new_v4(),
        key: format!("sealed/{id}"),
        size: bytes.len() as u64,
        digest: hex::encode(Sha256::digest(&bytes)),
        state: "sealed".into(),
        created_at: 0,
        due_at: i64::MAX,
    };
    let mut tx = db.begin().await.unwrap();
    put(&mut *tx, Some(object.plan_id), &object).await.unwrap();
    tx.commit().await.unwrap();
    store.put(&object.key, &bytes).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let export = dir.path().join("objects");
    backup::export_objects_from(&db, &store, &export)
        .await
        .unwrap();
    store.delete(&object.key).await.unwrap();
    assert!(!store.exists(&object.key).await.unwrap());
    backup::import_objects_into(&db, &store, &export)
        .await
        .unwrap();
    assert_eq!(store.get(&object.key, 1024).await.unwrap(), bytes);
    std::fs::write(export.join("manifest-0000.json"), b"[]").unwrap();
    assert!(
        backup::import_objects_into(&db, &store, &export)
            .await
            .is_err()
    );
    store.delete(&object.key).await.unwrap();
}

async fn database() -> PgDatabase {
    database_version(true).await
}
async fn database_version(current: bool) -> PgDatabase {
    let url = std::env::var("TEST_DATABASE_URL").expect("local TEST_DATABASE_URL required");
    let mut parsed = reqwest::Url::parse(&url).unwrap();
    assert!(parsed.path().ends_with("_test"));
    assert!(matches!(
        parsed.host_str(),
        Some("127.0.0.1" | "localhost" | "[::1]")
    ));
    let schema = format!("backup_{}", Id::new_v4().simple());
    let admin = PgDatabase::connect(&url, 2).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin.pool)
        .await
        .unwrap();
    parsed
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let db = PgDatabase::connect(parsed.as_str(), 4).await.unwrap();
    if current {
        db.migrate().await.unwrap();
    } else {
        let all = sqlx::migrate!("../../migrations");
        let first = sqlx::migrate::Migrator {
            migrations: std::borrow::Cow::Owned(vec![all.iter().next().unwrap().clone()]),
            ..sqlx::migrate::Migrator::DEFAULT
        };
        first.run(&db.pool).await.unwrap();
    }
    db.bind_bot(100).await.unwrap();
    admin.pool.close().await;
    db
}

async fn snapshot_marker(db: &PgDatabase) {
    sqlx::query("UPDATE maintenance SET backup_checkpoint='{\"sequence\":0,\"digest\":\"\"}'")
        .execute(&db.pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires isolated local PostgreSQL"]
async fn daily_backups_preserve_existing_holds_and_cannot_complete_restored_dump() {
    let db = database().await;
    sqlx::query("UPDATE telegram_cursor SET hold_until=123456789")
        .execute(&db.pool)
        .await
        .unwrap();
    for _ in 0..2 {
        let session = backup::begin_backup_on_db(&db).await.unwrap();
        assert!(backup::begin_backup_on_db(&db).await.is_err());
        backup::wait_for_drain_on_db(&db, session, Duration::from_secs(2))
            .await
            .unwrap();
        // A database dump is taken here, before the live journal checkpoint.
        assert!(backup::complete_backup_on_db(&db, session).await.is_err());
        assert!(backup::set_maintenance(&db, false).await.is_err());
        snapshot_marker(&db).await;
        backup::complete_backup_on_db(&db, session).await.unwrap();
        let hold: i64 = sqlx::query_scalar("SELECT hold_until FROM telegram_cursor")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(hold, 123456789);
        assert!(backup::complete_backup_on_db(&db, session).await.is_err());
    }
}

#[tokio::test]
#[ignore = "requires isolated local PostgreSQL"]
async fn stale_backup_completion_cannot_clear_an_operator_or_restore_hold() {
    let db = database().await;
    let session = backup::begin_backup_on_db(&db).await.unwrap();
    backup::wait_for_drain_on_db(&db, session, Duration::from_secs(2))
        .await
        .unwrap();
    snapshot_marker(&db).await;
    backup::set_maintenance(&db, true).await.unwrap();
    assert!(backup::complete_backup_on_db(&db, session).await.is_err());
    assert!(backup::set_maintenance(&db, false).await.is_err());
    backup::require_restore_on_db(&db).await.unwrap();
    assert!(backup::set_maintenance(&db, false).await.is_err());
    let held: bool = sqlx::query_scalar("SELECT enabled AND restore_required FROM maintenance")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert!(held);
}

#[tokio::test]
#[ignore = "requires isolated local PostgreSQL"]
async fn ordinary_maintenance_exit_retains_recovery_delay() {
    let db = database().await;
    backup::set_maintenance(&db, true).await.unwrap();
    backup::set_maintenance(&db, false).await.unwrap();
    let delayed: bool = sqlx::query_scalar("SELECT hold_until>=floor(extract(epoch from clock_timestamp()))::bigint+86395 FROM telegram_cursor").fetch_one(&db.pool).await.unwrap();
    assert!(delayed);
}

#[tokio::test]
#[ignore = "requires isolated local PostgreSQL"]
async fn original_schema_upgrades_without_resetting_existing_records() {
    let db = database_version(false).await;
    let id = Id::new_v4();
    let account = serde_json::json!({"id":id,"telegram_id":345,"chat_id":345,"locale":"uk"});
    sqlx::query("INSERT INTO accounts(id,data) VALUES($1,$2)")
        .bind(id)
        .bind(sqlx::types::Json(&account))
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(db.schema_current().await.is_err());
    db.migrate().await.unwrap();
    db.schema_current().await.unwrap();
    let preserved: serde_json::Value = sqlx::query_scalar("SELECT data FROM accounts WHERE id=$1")
        .bind(id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(preserved, account);
    let session = backup::begin_backup_on_db(&db).await.unwrap();
    backup::wait_for_drain_on_db(&db, session, Duration::from_secs(2))
        .await
        .unwrap();
}
