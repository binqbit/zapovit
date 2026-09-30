use std::{
    fs,
    process::{Command, Output},
};

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zapovit"));
    command.env_clear().envs([
        ("DATA_MODE", "synthetic"),
        (
            "DATABASE_URL",
            "postgresql://test:database-secret-marker@127.0.0.1:1/test",
        ),
        ("TELEGRAM_BOT_TOKEN", "1:token-secret-marker"),
        ("S3_ACCESS_KEY_ID", "test-access-key"),
        ("S3_SECRET_ACCESS_KEY", "storage-secret-marker"),
    ]);
    let keyring = r#"{"active":"v1","keys":{"v1":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}}"#;
    for name in ["KEK_KEYRING", "VERIFIER_KEYRING", "JOURNAL_KEYRING"] {
        command.env(name, keyring);
    }
    command
}

fn failed(output: Output) -> Vec<serde_json::Value> {
    assert!(!output.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap()
    );
    for secret in [
        "token-secret-marker",
        "database-secret-marker",
        "storage-secret-marker",
        "malformed-key-secret-marker",
    ] {
        assert!(
            !text.contains(secret),
            "Diagnostic exposed a test credential"
        );
    }
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn missing_token_reports_the_setting_without_other_credentials() {
    let logs = failed(
        command()
            .env_remove("TELEGRAM_BOT_TOKEN")
            .arg("check-config")
            .output()
            .unwrap(),
    );
    assert!(
        logs.iter()
            .any(|entry| entry["fields"]["setting"] == "TELEGRAM_BOT_TOKEN"
                && entry["fields"]["reason"] == "required_value_missing")
    );
}

#[test]
fn malformed_keyring_reports_the_setting_without_key_contents() {
    let logs = failed(
        command()
            .env("VERIFIER_KEYRING", "malformed-key-secret-marker")
            .arg("check-config")
            .output()
            .unwrap(),
    );
    assert!(
        logs.iter()
            .any(|entry| entry["fields"]["setting"] == "VERIFIER_KEYRING"
                && entry["fields"]["reason"] == "invalid_keyring")
    );
}

#[test]
fn invalid_workers_report_the_setting_without_the_supplied_value() {
    let logs = failed(
        command()
            .env("WORKERS", "token-secret-marker")
            .arg("check-config")
            .output()
            .unwrap(),
    );
    assert!(
        logs.iter()
            .any(|entry| entry["fields"]["setting"] == "WORKERS"
                && entry["fields"]["reason"] == "expected_integer")
    );
}

#[test]
fn verification_reports_missing_journal_without_initializing_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing");
    let logs = failed(
        command()
            .args(["verify-journal", "--directory"])
            .arg(&path)
            .output()
            .unwrap(),
    );
    assert!(logs.iter().any(|entry| {
        entry["fields"]["event"] == "journal_unavailable"
            && entry["fields"]["hint"]
                .as_str()
                .unwrap_or("")
                .contains("unused database")
    }));
    assert!(
        !path.exists(),
        "Startup must not create a replacement journal"
    );
}

#[test]
fn incomplete_journal_is_reported_and_never_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.jsonl");
    fs::write(&path, b"existing-journal-marker\n").unwrap();
    let logs = failed(
        command()
            .args(["verify-journal", "--directory"])
            .arg(dir.path())
            .output()
            .unwrap(),
    );
    assert!(
        logs.iter()
            .any(|entry| entry["fields"]["event"] == "journal_unavailable")
    );
    assert_eq!(fs::read(&path).unwrap(), b"existing-journal-marker\n");
    assert!(!dir.path().join("anchor.json").exists());
}

#[test]
fn busy_journal_reports_writer_lock_without_changing_files() {
    let dir = tempfile::tempdir().unwrap();
    adapters::journal::FileJournal::initialize(dir.path()).unwrap();
    let file = fs::File::open(dir.path().join("control.jsonl")).unwrap();
    file.try_lock().unwrap();
    let logs = failed(
        command()
            .args(["verify-journal", "--directory"])
            .arg(dir.path())
            .output()
            .unwrap(),
    );
    assert!(
        logs.iter()
            .any(|entry| entry["fields"]["reason"] == "writer_lock_unavailable")
    );
    assert!(
        fs::read(dir.path().join("control.jsonl"))
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
#[ignore = "requires local TEST_DATABASE_URL ending in _test"]
async fn automatic_journal_startup_preserves_history_and_refuses_used_database() {
    use adapters::postgres::PgDatabase;
    let url = std::env::var("TEST_DATABASE_URL").unwrap();
    let mut parsed = reqwest::Url::parse(&url).unwrap();
    assert!(parsed.path().ends_with("_test"));
    assert!(matches!(
        parsed.host_str(),
        Some("localhost" | "127.0.0.1" | "::1")
    ));
    let schema = format!("startup_{}", uuid::Uuid::new_v4().simple());
    let admin = PgDatabase::connect(&url, 2).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin.pool)
        .await
        .unwrap();
    parsed
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let db = PgDatabase::connect(parsed.as_str(), 2).await.unwrap();
    db.migrate().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal");
    let run = || {
        failed(
            command()
                .env("DATABASE_URL", parsed.as_str())
                .env("DATABASE_POOL", "2")
                .env("TELEGRAM_API_BASE", "http://127.0.0.1:1")
                .env("JOURNAL_DIR", &path)
                .arg("serve")
                .output()
                .unwrap(),
        )
    };
    let reached = |logs: &[serde_json::Value], stage| {
        assert!(logs.iter().any(|entry| entry["fields"]["stage"] == stage));
    };

    // The actual serve command creates the journal before contacting Telegram.
    let logs = run();
    reached(&logs, "telegram_get_me");
    assert!(
        logs.iter()
            .any(|entry| entry["fields"]["event"] == "journal_initialized")
    );
    let anchor = fs::read(path.join("anchor.json")).unwrap();
    assert!(fs::read(path.join("control.jsonl")).unwrap().is_empty());
    let logs = run();
    reached(&logs, "telegram_get_me");
    assert!(
        !logs
            .iter()
            .any(|entry| entry["fields"]["event"] == "journal_initialized")
    );
    assert_eq!(fs::read(path.join("anchor.json")).unwrap(), anchor);

    // Leadership excludes a concurrent startup even when its volume is absent.
    let leader = db.poller_lock().await.unwrap();
    fs::rename(&path, dir.path().join("saved")).unwrap();
    reached(&run(), "database_leadership");
    assert!(!path.exists());
    leader.close().await.unwrap();

    // Maintenance and persisted state each independently block a fresh journal.
    sqlx::query("UPDATE maintenance SET enabled=true")
        .execute(&db.pool)
        .await
        .unwrap();
    reached(&run(), "journal_open");
    assert!(!path.exists());
    sqlx::query("UPDATE maintenance SET enabled=false")
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO audit_events(operation,result) VALUES('stop','ok')")
        .execute(&db.pool)
        .await
        .unwrap();
    reached(&run(), "journal_open");
    assert!(!path.exists());
    sqlx::query("DELETE FROM audit_events")
        .execute(&db.pool)
        .await
        .unwrap();
    db.bind_bot(1).await.unwrap();
    reached(&run(), "journal_open");
    assert!(!path.exists());

    // Restoring the existing journal permits startup on the used database.
    fs::rename(dir.path().join("saved"), &path).unwrap();
    reached(&run(), "telegram_get_me");
    assert_eq!(fs::read(path.join("anchor.json")).unwrap(), anchor);
    fs::remove_file(path.join("control.jsonl")).unwrap();
    reached(&run(), "journal_open");
    assert_eq!(fs::read(path.join("anchor.json")).unwrap(), anchor);
    assert!(!path.join("control.jsonl").exists());

    db.pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin.pool)
        .await
        .unwrap();
    admin.pool.close().await;
}
