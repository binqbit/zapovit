use super::*;
use adapters::{bot::BotUi, telegram::Telegram};
use axum::{Json, Router, extract::Path, routing::post};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) type Screens = Arc<Mutex<Vec<(String, Value)>>>;

pub(super) fn labels(body: &Value) -> Vec<&str> {
    body["reply_markup"]["inline_keyboard"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|row| row.as_array().unwrap())
        .map(|button| button["text"].as_str().unwrap())
        .collect()
}
pub(super) fn button(body: &Value, label: &str) -> String {
    body["reply_markup"]["inline_keyboard"].as_array().unwrap().iter()
        .flat_map(|row| row.as_array().unwrap()).find(|button| button["text"] == label)
        .unwrap_or_else(|| panic!("Missing button {label}; visible: {:?}", labels(body)))
        ["callback_data"].as_str().unwrap().into()
}
pub(super) async fn last(screens: &Screens) -> Value {
    screens.lock().await.last().unwrap().1.clone()
}
pub(super) async fn press(ui: &BotUi, screens: &Screens, seq: &mut i64, user: i64, label: &str) {
    let data = button(&last(screens).await, label);
    callback(ui, seq, user, data).await;
}
pub(super) async fn callback(ui: &BotUi, seq: &mut i64, user: i64, data: String) {
    *seq += 1;
    ui.handle(
        100,
        &json!({"update_id":*seq,"callback_query":{
            "id":seq.to_string(),"from":{"id":user,"is_bot":false},"data":data,
            "message":{"message_id":42,"chat":{"id":user,"type":"private"}}
        }}),
    )
    .await
    .unwrap();
}
pub(super) async fn command(ui: &BotUi, seq: &mut i64, user: i64, text: &str) {
    *seq += 1;
    ui.handle(
        100,
        &json!({"update_id":*seq,"message":{
            "message_id":*seq,"from":{"id":user,"is_bot":false},
            "chat":{"id":user,"type":"private"},"text":text
        }}),
    )
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn menus_group_settings_localize_and_preserve_owner_controls() {
    let f = fixture().await;
    let owner = f.engine.account(7101, 7101, "en").await.unwrap();
    let plan = f.engine.create_profile(owner.id).await.unwrap();
    let screens: Screens = Default::default();
    let reject_edit = Arc::new(AtomicBool::new(false));
    let captured = screens.clone();
    let rejected = reject_edit.clone();
    let app = Router::new().route("/bot1:test/{method}", post(move |Path(method): Path<String>, Json(body): Json<Value>| {
        let screens = captured.clone();
        let rejected = rejected.clone();
        async move {
            if method == "answerCallbackQuery" {
                return Json(json!({"ok":true,"result":true}));
            }
            assert!(matches!(method.as_str(), "sendMessage" | "editMessageText"));
            screens.lock().await.push((method.clone(), body));
            if method == "editMessageText" && rejected.load(Ordering::Relaxed) {
                Json(json!({"ok":false,"error_code":400,"description":"message to edit not found"}))
            } else {
                Json(json!({"ok":true,"result":{"message_id":42}}))
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let ui = BotUi {
        engine: f.engine.clone(),
        telegram: Telegram::new(
            &format!("http://{address}"),
            Zeroizing::new("1:test".into()),
        )
        .unwrap(),
        username: "synthetic_bot".into(),
    };
    let mut seq = 9000;
    command(&ui, &mut seq, 7101, "/start").await;
    assert_eq!(
        labels(&last(&screens).await),
        [
            "Confirm activity",
            "My plan",
            "Guardian requests",
            "Settings",
            "STOP transmission"
        ]
    );
    press(&ui, &screens, &mut seq, 7101, "Settings").await;
    assert_eq!(
        labels(&last(&screens).await),
        [
            "Language",
            "Access and recovery",
            "Delete permanently",
            "← Back"
        ]
    );
    assert_eq!(screens.lock().await.last().unwrap().0, "editMessageText");
    assert_eq!(last(&screens).await["message_id"], 42);
    press(&ui, &screens, &mut seq, 7101, "Language").await;
    assert!(labels(&last(&screens).await).contains(&"✓ English"));
    press(&ui, &screens, &mut seq, 7101, "Українська").await;
    assert!(labels(&last(&screens).await).contains(&"✓ Українська"));
    assert_eq!(
        f.engine.account(7101, 7101, "en").await.unwrap().locale,
        "uk"
    );
    press(&ui, &screens, &mut seq, 7101, "← Назад").await;
    assert_eq!(last(&screens).await["text"], "Налаштування");
    press(&ui, &screens, &mut seq, 7101, "← Назад").await;
    press(&ui, &screens, &mut seq, 7101, "Мій план").await;
    assert_eq!(
        labels(&last(&screens).await),
        [
            "Секрети та статус",
            "Новий секрет",
            "Довірені люди",
            "Активувати план",
            "← Назад"
        ]
    );
    press(&ui, &screens, &mut seq, 7101, "← Назад").await;
    press(&ui, &screens, &mut seq, 7101, "STOP передачі").await;
    assert_eq!(
        f.engine.own_plan(owner.id).await.unwrap().1.state,
        PlanState::Paused
    );
    press(&ui, &screens, &mut seq, 7101, "Головне меню").await;
    press(&ui, &screens, &mut seq, 7101, "Налаштування").await;
    press(&ui, &screens, &mut seq, 7101, "Доступ і відновлення").await;
    assert!(labels(&last(&screens).await).contains(&"Замінити ключ відновлення"));
    press(&ui, &screens, &mut seq, 7101, "← Назад").await;
    let delete = button(&last(&screens).await, "Видалити назавжди");
    press(&ui, &screens, &mut seq, 7101, "Видалити назавжди").await;
    assert!(labels(&last(&screens).await).contains(&"Так, видалити назавжди"));
    press(&ui, &screens, &mut seq, 7101, "← Назад").await;
    assert_eq!(f.engine.own_plan(owner.id).await.unwrap().1.id, plan);

    // Settings and language selection also work without an owned plan.
    command(&ui, &mut seq, 7102, "/settings").await;
    assert_eq!(
        labels(&last(&screens).await),
        ["Language", "Access and recovery", "← Back"]
    );
    press(&ui, &screens, &mut seq, 7102, "Access and recovery").await;
    assert!(!labels(&last(&screens).await).contains(&"Replace recovery key"));
    press(&ui, &screens, &mut seq, 7102, "Recover access").await;
    let guest = f.engine.account(7102, 7102, "en").await.unwrap();
    let mut tx = f.db.begin().await.unwrap();
    assert_eq!(
        get::<Dialog>(&mut *tx, guest.id).await.unwrap().step,
        "recover"
    );
    tx.commit().await.unwrap();
    reject_edit.store(true, Ordering::Relaxed);
    press(&ui, &screens, &mut seq, 7102, "← Back").await;
    assert_eq!(screens.lock().await.last().unwrap().0, "sendMessage");
    let mut tx = f.db.begin().await.unwrap();
    assert!(tx.get(Kind::Dialog, guest.id).await.unwrap().is_none());
    tx.commit().await.unwrap();
    // A callback copied from another person's settings cannot open deletion.
    callback(&ui, &mut seq, 7102, delete).await;
    assert_eq!(
        last(&screens).await["text"],
        adapters::localization::tr("en", "stale-action")
    );
    assert_eq!(
        f.engine.own_plan(owner.id).await.unwrap().1.state,
        PlanState::Paused
    );
    server.abort();
}
