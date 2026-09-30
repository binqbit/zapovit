use adapters::{settings::StorageCredentials, storage::S3Storage, telegram::Telegram};
use application::{BlobStore, SendResult};
use axum::{Json, Router, routing::post};
use serde_json::{Value, json};
use zeroize::Zeroizing;

#[tokio::test]
async fn telegram_preserves_copyable_text_and_file_saving() {
    use adapters::telegram::entity;
    use axum::{body::Bytes, extract::State};
    use domain::{Block, FileRef, Id};
    use std::sync::Arc;
    use teloxide_core::types::MessageEntityKind;
    use tokio::sync::Mutex;
    let requests = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let app = Router::new()
        .route(
            "/bot1:test/{method}",
            post(
                |State(requests): State<Arc<Mutex<Vec<Vec<u8>>>>>, bytes: Bytes| async move {
                    requests.lock().await.push(bytes.to_vec());
                    Json(json!({"ok":true,"result":{"message_id":42}}))
                },
            ),
        )
        .with_state(requests.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let tg = Telegram::new(&format!("http://{addr}"), Zeroizing::new("1:test".into())).unwrap();
    let value = " 🔐 e\u{301}\n<>&_* ";
    let prefix = "🔑\n\n";
    let mut e = entity(value, MessageEntityKind::Pre { language: None });
    e.offset = prefix.encode_utf16().count();
    assert!(matches!(
        tg.send_text(1, &format!("{prefix}{value}"), vec![e], vec![], None)
            .await,
        SendResult::Sent(42)
    ));
    let long = "x".repeat(257);
    assert!(matches!(
        tg.send_text(
            2,
            &long,
            vec![entity(&long, MessageEntityKind::Pre { language: None })],
            vec![],
            None
        )
        .await,
        SendResult::Sent(42)
    ));
    let file = Block::File {
        name: "synthetic.bin".into(),
        caption: "caption".into(),
        file: FileRef {
            id: Id::new_v4(),
            object_key: "unused".into(),
            sha256: String::new(),
            encrypted_size: 5,
        },
    };
    assert!(matches!(
        tg.send_block(3, &file, Some(Zeroizing::new(vec![0, 255, 2, 3, 4])))
            .await,
        SendResult::Sent(42)
    ));
    let requests = requests.lock().await;
    let body: Value = serde_json::from_slice(&requests[0]).unwrap();
    assert_eq!(
        body["reply_markup"]["inline_keyboard"][0][0]["copy_text"]["text"],
        value
    );
    assert!(body.get("protect_content").is_none());
    let body: Value = serde_json::from_slice(&requests[1]).unwrap();
    assert_eq!(body["text"], long);
    assert!(body.get("reply_markup").is_none());
    assert!(!String::from_utf8_lossy(&requests[2]).contains("protect_content"));
    assert!(
        requests[2]
            .windows(5)
            .any(|bytes| bytes == [0, 255, 2, 3, 4])
    );
    server.abort();
}

#[tokio::test]
async fn telegram_deletion_retries_uncertainty_and_accepts_already_absent() {
    use adapters::telegram::DeleteResult;
    let app = Router::new().route("/bot1:test/deleteMessage", post(|Json(body): Json<Value>| async move {
        Json(match body["message_id"].as_i64().unwrap() {
            1 => json!({"ok":false,"error_code":429,"parameters":{"retry_after":9}}),
            2 => json!({"ok":false,"error_code":500}),
            3 => json!({"ok":false,"error_code":400,"description":"Bad Request: message to delete not found"}),
            _ => json!({"ok":false,"error_code":403}),
        })
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let tg = Telegram::new(&format!("http://{addr}"), Zeroizing::new("1:test".into())).unwrap();
    assert!(matches!(tg.delete(1, 2).await, DeleteResult::RetryAfter(_)));
    assert!(matches!(tg.delete(1, 3).await, DeleteResult::Deleted));
    assert!(matches!(tg.delete(1, 4).await, DeleteResult::Permanent));
    assert!(matches!(tg.delete(1, 1).await, DeleteResult::RetryAfter(9)));
    server.abort();
}

#[tokio::test]
async fn telegram_classifies_explicit_rejection_and_uncertain_response() {
    let app = Router::new().route(
        "/bot1:test/sendMessage",
        post(|Json(body): Json<Value>| async move {
            match body["text"].as_str().unwrap() {
                "limit" => {
                    Json(json!({"ok":false,"error_code":429,"parameters":{"retry_after":9}}))
                }
                "blocked" => Json(json!({"ok":false,"error_code":403})),
                "unknown" => Json(json!({"ok":true,"result":{}})),
                _ => Json(json!({"ok":true,"result":{"message_id":42}})),
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let tg = Telegram::new(&format!("http://{addr}"), Zeroizing::new("1:test".into())).unwrap();
    assert!(matches!(
        tg.send_text(1, "ok", vec![], vec![], None).await,
        SendResult::Sent(42)
    ));
    assert!(matches!(
        tg.send_text(1, "blocked", vec![], vec![], None).await,
        SendResult::Permanent
    ));
    assert!(matches!(
        tg.send_text(1, "unknown", vec![], vec![], None).await,
        SendResult::Unknown
    ));
    assert!(matches!(
        tg.send_text(1, "limit", vec![], vec![], None).await,
        SendResult::RetryAfter(9)
    ));
    server.abort();
}

#[tokio::test]
async fn telegram_retry_after_applies_to_all_chats_and_reserved_workers() {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    let app = Router::new().route(
        "/bot1:test/sendMessage",
        post(move || {
            let count = count.clone();
            async move {
                if count.fetch_add(1, Ordering::Relaxed) == 0 {
                    Json(json!({"ok":false,"error_code":429,"parameters":{"retry_after":1}}))
                } else {
                    Json(json!({"ok":true,"result":{"message_id":42}}))
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let tg = Telegram::new(&format!("http://{addr}"), Zeroizing::new("1:test".into())).unwrap();
    assert!(matches!(
        tg.send_text(1, "first", vec![], vec![], None).await,
        SendResult::RetryAfter(1)
    ));
    let start = Instant::now();
    // UI replies fail promptly without another request, allowing the inbox to advance.
    assert!(matches!(
        tg.send_text(2, "second", vec![], vec![], None).await,
        SendResult::RetryAfter(_)
    ));
    assert!(start.elapsed() < Duration::from_millis(200));
    assert!(matches!(
        tg.send_text_reserved(2, "already reserved", vec![], vec![], None)
            .await,
        SendResult::RetryAfter(_)
    ));
    assert_eq!(requests.load(Ordering::Relaxed), 1);
    tg.reserve_send(3).await;
    assert!(start.elapsed() >= Duration::from_millis(900));
    assert!(matches!(
        tg.send_text_reserved(3, "worker", vec![], vec![], None)
            .await,
        SendResult::Sent(42)
    ));
    assert_eq!(requests.load(Ordering::Relaxed), 2);
    server.abort();
}

#[tokio::test]
#[ignore = "requires explicitly configured local Garage"]
async fn garage_put_get_head_delete_with_restricted_application_key() {
    let endpoint = std::env::var("TEST_S3_ENDPOINT").expect("TEST_S3_ENDPOINT");
    assert!(endpoint.starts_with("http://127.0.0.1:"));
    let path = std::env::var("TEST_S3_CREDENTIALS_FILE").expect("TEST_S3_CREDENTIALS_FILE");
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
    let key = format!("integration/{}", uuid::Uuid::new_v4());
    let data = vec![42; 1024 * 1024];
    assert!(!store.exists(&key).await.unwrap());
    store.put(&key, &data).await.unwrap();
    assert!(store.exists(&key).await.unwrap());
    assert_eq!(store.get(&key, data.len()).await.unwrap(), data);
    assert!(store.get(&key, 10).await.is_err());
    store.delete(&key).await.unwrap();
    assert!(!store.exists(&key).await.unwrap());
    store.delete(&key).await.unwrap();
}
