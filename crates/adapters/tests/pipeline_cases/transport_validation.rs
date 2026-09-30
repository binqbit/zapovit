use super::*;
use adapters::{backup::set_maintenance, bot::BotUi, telegram::Telegram};
use axum::{Json, Router, routing::post};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

async fn fake_ui(
    f: &Fixture,
    unavailable: bool,
) -> (BotUi, tokio::task::JoinHandle<()>, Arc<AtomicUsize>) {
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    let app = Router::new().route(
        "/bot1:test/sendMessage",
        post(move |Json(body): Json<Value>| {
            let count = count.clone();
            async move {
                count.fetch_add(1, Ordering::Relaxed);
                if unavailable && body["chat_id"] == 9001 {
                    Json(json!({"ok":false,"error_code":403}))
                } else if unavailable {
                    Json(json!({"ok":true,"result":{}}))
                } else {
                    Json(json!({"ok":true,"result":{"message_id":42}}))
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let telegram = Telegram::new(
        &format!("http://{address}"),
        Zeroizing::new("1:test".into()),
    )
    .unwrap();
    (
        BotUi {
            engine: f.engine.clone(),
            telegram,
            username: "synthetic_bot".into(),
        },
        server,
        requests,
    )
}
fn update(id: i64, user: i64, text: &str) -> Value {
    json!({"update_id":id,"message":{"message_id":id,"date":0,"from":{"id":user,"is_bot":false},"chat":{"id":user,"type":"private"},"text":text}})
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn unavailable_ui_replies_do_not_poison_the_shared_inbox() {
    let f = fixture().await;
    let (ui, server, requests) = fake_ui(&f, true).await;
    for (id, user) in [(400, 9001), (401, 9002)] {
        let event = Id::from_u128((100_u128 << 64) | id as u128);
        let update = update(id, user, "/start");
        let envelope = f
            .engine
            .crypto
            .wrap(
                "telegram-inbox",
                event,
                &serde_json::to_vec(&update).unwrap(),
            )
            .unwrap();
        f.db.ingest(100, &[(id, envelope)]).await.unwrap();
        ui.handle(100, &update).await.unwrap();
        f.db.complete_update(100, id).await.unwrap();
        // An uncertain UI send is not automatically sent again on replay.
        ui.handle(100, &update).await.unwrap();
    }
    assert_eq!(requests.load(Ordering::Relaxed), 2);
    assert!(!f.db.has_backlog().await.unwrap());
    server.abort();
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn slash_commands_are_limited_but_stop_and_sensitive_cleanup_still_work() {
    let f = fixture().await;
    let owner = f.engine.account(9001, 9001, "en").await.unwrap();
    let plan = f.engine.create_profile(owner.id).await.unwrap();
    let (ui, server, requests) = fake_ui(&f, false).await;
    for _ in 0..30 {
        f.engine
            .limit(&format!("action:{}", owner.id), 30, 60)
            .await
            .unwrap();
    }
    ui.handle(100, &update(501, 9001, "/start")).await.unwrap();
    assert_eq!(requests.load(Ordering::Relaxed), 1);
    ui.handle(100, &update(504, 9001, "/start")).await.unwrap();
    ui.handle(100, &update(505, 9001, "/settings"))
        .await
        .unwrap();
    assert_eq!(
        requests.load(Ordering::Relaxed),
        1,
        "limit feedback is sent once per minute"
    );
    ui.handle(100, &update(502, 9001, "/stop")).await.unwrap();
    assert_eq!(
        requests.load(Ordering::Relaxed),
        2,
        "STOP confirmation bypasses the ordinary limit"
    );
    let mut tx = f.engine.db.begin().await.unwrap();
    assert_eq!(
        get::<Plan>(&mut *tx, plan).await.unwrap().state,
        PlanState::Paused
    );
    assert!(
        list::<Action>(&mut *tx, Some(plan))
            .await
            .unwrap()
            .is_empty()
    );
    // Cleanup is scheduled even for an invalid token, expired dialog and full rate limit.
    put(
        &mut *tx,
        None,
        &Dialog {
            id: owner.id,
            plan_id: None,
            owner_epoch: None,
            step: "recover".into(),
            draft_id: None,
            expires_at: 0,
            selected: Default::default(),
            reply_to: None,
            case_id: None,
            guardians: Default::default(),
            recipients: Default::default(),
            threshold: 0,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    ui.handle(100, &update(503, 9001, "invalid synthetic token"))
        .await
        .unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    let event = Id::from_u128((100_u128 << 64) | 503);
    let job: Job = get(&mut *tx, event).await.unwrap();
    assert!(matches!(
        job.task,
        Task::CleanupMessage {
            message_id: 503,
            chat_id: 9001,
            ..
        }
    ));
    assert!(
        !serde_json::to_string(&job)
            .unwrap()
            .contains("invalid synthetic token")
    );
    tx.commit().await.unwrap();
    server.abort();
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn maintenance_exit_publishes_the_hold_atomically() {
    let f = fixture().await;
    set_maintenance(&f.db, true).await.unwrap();
    let mut blocker = f.db.pool.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar(
        "SELECT pg_backend_pid() FROM telegram_cursor WHERE singleton FOR UPDATE",
    )
    .fetch_one(&mut *blocker)
    .await
    .unwrap();
    let db = f.db.clone();
    let change = tokio::spawn(async move { set_maintenance(&db, false).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)))",
            )
            .bind(pid)
            .fetch_one(&f.db.pool)
            .await
            .unwrap();
            if waiting {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let enabled: bool = sqlx::query_scalar("SELECT enabled FROM maintenance WHERE singleton")
        .fetch_one(&f.db.pool)
        .await
        .unwrap();
    assert!(
        enabled,
        "readers must not see maintenance cleared while hold update is pending"
    );
    blocker.commit().await.unwrap();
    change.await.unwrap().unwrap();
    let safe: bool = sqlx::query_scalar("SELECT NOT enabled AND hold_until>=floor(extract(epoch from clock_timestamp()))::bigint+86395 FROM maintenance CROSS JOIN telegram_cursor")
        .fetch_one(&f.db.pool).await.unwrap();
    assert!(safe);
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn emergency_priority_is_bound_to_the_current_owner() {
    let f = fixture().await;
    let owner = f.engine.account(9001, 9001, "en").await.unwrap();
    let outsider = f.engine.account(9002, 9002, "en").await.unwrap();
    let plan = f.engine.create_profile(owner.id).await.unwrap();
    let (ui, server, requests) = fake_ui(&f, false).await;
    let (_, data) = ui
        .button(&owner, "stop", None, Some(plan), true, "STOP")
        .await
        .unwrap();
    let action = Id::parse_str(&data).unwrap();
    assert!(f.db.priority_owner(owner.telegram_id).await.unwrap());
    assert!(!f.db.priority_owner(outsider.telegram_id).await.unwrap());
    assert!(
        f.db.priority_callback(action, owner.telegram_id)
            .await
            .unwrap()
    );
    assert!(
        !f.db
            .priority_callback(action, outsider.telegram_id)
            .await
            .unwrap()
    );
    for _ in 0..30 {
        f.engine
            .limit(&format!("action:{}", outsider.id), 30, 60)
            .await
            .unwrap();
    }
    ui.handle(100, &update(601, outsider.telegram_id, "/stop"))
        .await
        .unwrap();
    assert_eq!(
        requests.load(Ordering::Relaxed),
        1,
        "nonowner emergency commands use the normal quota and get limit feedback"
    );
    assert_eq!(
        f.engine.own_plan(owner.id).await.unwrap().1.state,
        PlanState::Setup
    );
    let (mut profile, _) = f.engine.own_plan(owner.id).await.unwrap();
    profile.owner_epoch += 1;
    let mut tx = f.engine.db.begin().await.unwrap();
    put(&mut *tx, None, &profile).await.unwrap();
    tx.commit().await.unwrap();
    assert!(
        !f.db
            .priority_callback(action, owner.telegram_id)
            .await
            .unwrap()
    );
    server.abort();
}
