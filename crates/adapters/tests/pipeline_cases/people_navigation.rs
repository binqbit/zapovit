use super::menu_navigation::{Screens, button, callback, command, labels, last, press};
use super::*;
use adapters::{bot::BotUi, localization::tr, telegram::Telegram};
use axum::{Json, Router, extract::Path, routing::post};
use serde_json::{Value, json};

async fn ui(f: &Fixture) -> (BotUi, Screens, tokio::task::JoinHandle<()>) {
    let screens: Screens = Default::default();
    let captured = screens.clone();
    let app = Router::new().route("/bot1:test/{method}", post(move |Path(method): Path<String>, Json(body): Json<Value>| {
        let screens = captured.clone();
        async move {
            match method.as_str() {
                "answerCallbackQuery" | "deleteMessage" | "editMessageReplyMarkup" => return Json(json!({"ok":true,"result":true})),
                "getChat" => return Json(json!({"ok":true,"result":{"id":body["chat_id"],"type":"private","first_name":"Legacy contact","username":"legacy_contact"}})),
                "sendMessage" | "editMessageText" => {}
                _ => panic!("Unexpected method {method}"),
            }
            screens.lock().await.push((method, body));
            Json(json!({"ok":true,"result":{"message_id":42}}))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (
        BotUi {
            engine: f.engine.clone(),
            telegram: Telegram::new(
                &format!("http://{address}"),
                Zeroizing::new("1:test".into()),
            )
            .unwrap(),
            username: "synthetic_bot".into(),
        },
        screens,
        server,
    )
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn named_contact_confirmation_advances_in_place_and_navigation_resumes_setup() {
    let f = fixture().await;
    let owner = f.engine.account(9601, 9601, "uk").await.unwrap();
    let person = f.engine.account(9602, 9602, "uk").await.unwrap();
    let plan = f.engine.create_profile(owner.id).await.unwrap();
    let (profile, _) = f.engine.own_plan(owner.id).await.unwrap();
    f.engine
        .acknowledge_recovery(owner.id, plan, profile.recovery_selector)
        .await
        .unwrap();
    let invitation = f.engine.invite(owner.id, plan).await.unwrap();
    f.engine.accept_invite(person.id, invitation).await.unwrap();
    let (ui, screens, server) = ui(&f).await;
    let mut seq = 70000;
    ui.handle(100, &json!({"update_id":seq,"message":{"message_id":1,"from":{"id":9602,"first_name":"Олена <&>","last_name":"Тест","username":"olena_test","language_code":"uk"},"chat":{"id":9602,"type":"private"},"text":"/start"}})).await.unwrap();
    command(&ui, &mut seq, owner.telegram_id, "/start").await;
    command(&ui, &mut seq, owner.telegram_id, &tr("uk", "nav-home")).await;
    assert_eq!(
        labels(&last(&screens).await).len(),
        3,
        "setup exposes only next step, settings and stop"
    );
    let keyboard = screens
        .lock()
        .await
        .iter()
        .find(|(_, body)| body["chat_id"] == 9601 && body["reply_markup"]["keyboard"].is_array())
        .unwrap()
        .1
        .clone();
    assert_eq!(keyboard["reply_markup"]["is_persistent"], false);
    assert_eq!(
        keyboard["reply_markup"]["keyboard"][0][0]["text"],
        tr("uk", "nav-home")
    );
    press(
        &ui,
        &screens,
        &mut seq,
        owner.telegram_id,
        &tr("uk", "settings"),
    )
    .await;
    press(
        &ui,
        &screens,
        &mut seq,
        owner.telegram_id,
        &tr("uk", "participants"),
    )
    .await;
    press(
        &ui,
        &screens,
        &mut seq,
        owner.telegram_id,
        "○ Олена <&> Тест",
    )
    .await;
    let confirmation = last(&screens).await;
    assert_eq!(
        labels(&confirmation),
        vec![
            tr("uk", "confirm-person"),
            tr("uk", "reject-person"),
            tr("uk", "back")
        ]
    );
    assert!(
        confirmation["text"]
            .as_str()
            .unwrap()
            .contains("@olena_test")
    );
    assert!(
        confirmation["parse_mode"].is_null(),
        "user name is never parsed as markup"
    );
    assert_eq!(confirmation["entities"][0]["type"], "bold");
    let confirm = button(&confirmation, &tr("uk", "confirm-person"));
    callback(&ui, &mut seq, owner.telegram_id, confirm.clone()).await;
    assert_eq!(screens.lock().await.last().unwrap().0, "editMessageText");
    assert_eq!(
        labels(&last(&screens).await),
        vec![tr("uk", "continue-setup"), tr("uk", "back-to-people")]
    );
    callback(&ui, &mut seq, owner.telegram_id, confirm).await;
    assert_eq!(
        f.engine
            .contacts(owner.id, plan)
            .await
            .unwrap()
            .iter()
            .filter(|p| p.confirmed)
            .count(),
        1
    );
    press(
        &ui,
        &screens,
        &mut seq,
        owner.telegram_id,
        &tr("uk", "continue-setup"),
    )
    .await;
    let mut tx = f.db.begin().await.unwrap();
    let session: DraftSession = get(&mut *tx, owner.id).await.unwrap();
    assert_eq!(session.dialog.step, "guardians");
    let draft = session.dialog.draft_id.unwrap();
    tx.commit().await.unwrap();
    command(&ui, &mut seq, owner.telegram_id, &tr("uk", "nav-home")).await;
    command(&ui, &mut seq, owner.telegram_id, &tr("en", "nav-continue")).await;
    let mut tx = f.db.begin().await.unwrap();
    assert_eq!(
        get::<DraftSession>(&mut *tx, owner.id)
            .await
            .unwrap()
            .dialog
            .step,
        "guardians"
    );
    tx.commit().await.unwrap();
    assert!(
        f.engine
            .draft_blocks(owner.id, draft)
            .await
            .unwrap()
            .is_empty(),
        "navigation labels must never become secret content"
    );
    assert!(
        last(&screens).await["text"]
            .as_str()
            .unwrap()
            .contains(&tr("uk", "draft-stage-guardians"))
    );
    server.abort();
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn existing_contacts_get_names_without_rejoining_and_private_labels_win() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    let (ui, screens, server) = ui(&f).await;
    let mut seq = 71000;
    let (_, action) = ui
        .button(&owner, "participants", None, Some(plan), true, "People")
        .await
        .unwrap();
    callback(&ui, &mut seq, owner.telegram_id, action).await;
    assert!(labels(&last(&screens).await).contains(&"✓ Legacy contact"));
    let contact = f
        .engine
        .contacts(owner.id, plan)
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.account_id == people[0].id)
        .unwrap();
    assert_eq!(contact.display_name.as_deref(), Some("Legacy contact"));
    f.engine
        .set_label(owner.id, plan, contact.id, "My sister")
        .await
        .unwrap();
    let (_, action) = ui
        .button(&owner, "participants", None, Some(plan), true, "People")
        .await
        .unwrap();
    callback(&ui, &mut seq, owner.telegram_id, action).await;
    assert!(labels(&last(&screens).await).contains(&"✓ My sister"));
    server.abort();
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn stop_requires_scoped_expiring_confirmation_and_cancel_changes_nothing() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    let (secret_id, _) = secret(&f, &owner, plan, &people, false).await;
    let (ui, screens, server) = ui(&f).await;
    let mut seq = 72000;
    let before = f.engine.own_plan(owner.id).await.unwrap().1.state;
    command(&ui, &mut seq, owner.telegram_id, "/stop").await;
    assert_eq!(f.engine.own_plan(owner.id).await.unwrap().1.state, before);
    assert_eq!(
        labels(&last(&screens).await),
        vec![tr("uk", "confirm-stop"), tr("uk", "cancel")]
    );
    let cancelled_confirm = button(&last(&screens).await, &tr("uk", "confirm-stop"));
    press(
        &ui,
        &screens,
        &mut seq,
        owner.telegram_id,
        &tr("uk", "cancel"),
    )
    .await;
    callback(&ui, &mut seq, owner.telegram_id, cancelled_confirm).await;
    assert_eq!(f.engine.own_plan(owner.id).await.unwrap().1.state, before);
    command(&ui, &mut seq, owner.telegram_id, "/stop").await;
    let expired = button(&last(&screens).await, &tr("uk", "confirm-stop"));
    let mut tx = f.db.begin().await.unwrap();
    let mut action: Action = get(&mut *tx, Id::parse_str(&expired).unwrap())
        .await
        .unwrap();
    let now = tx.now().await.unwrap();
    assert!((295..=300).contains(&(action.expires_at - now)));
    action.expires_at = now - 1;
    put(&mut *tx, Some(plan), &action).await.unwrap();
    tx.commit().await.unwrap();
    callback(&ui, &mut seq, owner.telegram_id, expired).await;
    assert_eq!(f.engine.own_plan(owner.id).await.unwrap().1.state, before);
    let (_, request) = ui
        .button(
            &owner,
            "stop-secret",
            Some(secret_id),
            Some(plan),
            true,
            "Stop secret",
        )
        .await
        .unwrap();
    callback(&ui, &mut seq, owner.telegram_id, request).await;
    let confirm = button(&last(&screens).await, &tr("uk", "confirm-stop"));
    assert!(
        f.db.priority_callback(Id::parse_str(&confirm).unwrap(), owner.telegram_id)
            .await
            .unwrap()
    );
    callback(&ui, &mut seq, people[0].telegram_id, confirm.clone()).await;
    let mut tx = f.db.begin().await.unwrap();
    assert_ne!(
        get::<Secret>(&mut *tx, secret_id).await.unwrap().state,
        SecretState::Paused
    );
    tx.commit().await.unwrap();
    callback(&ui, &mut seq, owner.telegram_id, confirm).await;
    let mut tx = f.db.begin().await.unwrap();
    assert_eq!(
        get::<Secret>(&mut *tx, secret_id).await.unwrap().state,
        SecretState::Paused
    );
    assert_eq!(get::<Plan>(&mut *tx, plan).await.unwrap().state, before);
    tx.commit().await.unwrap();
    server.abort();
}
