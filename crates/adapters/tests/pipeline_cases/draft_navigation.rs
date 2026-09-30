use super::menu_navigation::{Screens, button, callback, command, labels, last, press};
use super::*;
use adapters::{bot::BotUi, localization::tr, telegram::Telegram};
use axum::{Json, Router, extract::Path, routing::post};
use serde_json::{Value, json};

async fn ui(f: &Fixture) -> (BotUi, Screens, tokio::task::JoinHandle<()>) {
    let screens: Screens = Default::default();
    let captured = screens.clone();
    let app = Router::new().route(
        "/bot1:test/{method}",
        post(move |Path(method): Path<String>, Json(body): Json<Value>| {
            let screens = captured.clone();
            async move {
                if method == "answerCallbackQuery" {
                    return Json(json!({"ok":true,"result":true}));
                }
                assert!(matches!(method.as_str(), "sendMessage" | "editMessageText"));
                screens.lock().await.push((method, body));
                Json(json!({"ok":true,"result":{"message_id":42}}))
            }
        }),
    );
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

async fn ready_draft(f: &Fixture, owner: &Account, plan: Id, people: &[Account]) -> Id {
    let draft = f.engine.new_draft(owner.id, plan).await.unwrap();
    f.engine
        .append_block(
            owner.id,
            draft,
            Block::Text {
                text: "Synthetic draft".into(),
            },
            (owner.chat_id, 100),
        )
        .await
        .unwrap();
    let policy = Policy {
        guardians: [people[0].id].into(),
        recipients: [people[1].id].into(),
        threshold: 1,
        timing: Timing::default(),
    };
    f.engine
        .draft_policy(owner.id, draft, policy.clone())
        .await
        .unwrap();
    let (profile, _) = f.engine.own_plan(owner.id).await.unwrap();
    let mut tx = f.db.begin().await.unwrap();
    let now = tx.now().await.unwrap();
    put(
        &mut *tx,
        Some(plan),
        &Dialog {
            id: owner.id,
            plan_id: Some(plan),
            owner_epoch: Some(profile.owner_epoch),
            step: "ready".into(),
            draft_id: Some(draft),
            expires_at: now + 900,
            selected: Default::default(),
            reply_to: None,
            case_id: None,
            guardians: policy.guardians,
            recipients: policy.recipients,
            threshold: 1,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    draft
}

async fn dialog(f: &Fixture, owner: &Account) -> Dialog {
    let mut tx = f.db.begin().await.unwrap();
    get(&mut *tx, owner.id).await.unwrap()
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn draft_buttons_cannot_apply_to_another_step_revision_or_draft() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    f.engine.locale(owner.id, "en").await.unwrap();
    let first = ready_draft(&f, &owner, plan, &people).await;
    let (ui, screens, server) = ui(&f).await;
    let mut seq = 12000;
    command(&ui, &mut seq, owner.telegram_id, "/start").await;
    press(&ui, &screens, &mut seq, owner.telegram_id, "My plan").await;
    press(&ui, &screens, &mut seq, owner.telegram_id, "Continue draft").await;
    let old_save = button(&last(&screens).await, "Save and encrypt");
    press(
        &ui,
        &screens,
        &mut seq,
        owner.telegram_id,
        "Add more blocks",
    )
    .await;
    press(&ui, &screens, &mut seq, owner.telegram_id, "Text").await;
    command(&ui, &mut seq, owner.telegram_id, "A new synthetic block").await;
    press(
        &ui,
        &screens,
        &mut seq,
        owner.telegram_id,
        "Choose trusted people",
    )
    .await;
    let guardian_toggle = button(&last(&screens).await, "○ 2002");
    press(&ui, &screens, &mut seq, owner.telegram_id, "Done").await;
    callback(&ui, &mut seq, owner.telegram_id, guardian_toggle).await;
    assert_eq!(last(&screens).await["text"], tr("en", "stale-action"));
    let current = dialog(&f, &owner).await;
    assert_eq!(current.step, "recipients");
    assert_eq!(current.selected, [people[1].id].into());
    press(&ui, &screens, &mut seq, owner.telegram_id, "Continue draft").await;
    press(&ui, &screens, &mut seq, owner.telegram_id, "Done").await;
    press(&ui, &screens, &mut seq, owner.telegram_id, "1 / 1").await;
    press(
        &ui,
        &screens,
        &mut seq,
        owner.telegram_id,
        "Set my own intervals",
    )
    .await;
    command(&ui, &mut seq, owner.telegram_id, "7 10 7").await;
    assert_eq!(dialog(&f, &owner).await.step, "timing");
    assert!(
        screens
            .lock()
            .await
            .iter()
            .any(|(_, body)| body["text"] == tr("en", "invalid-timing"))
    );
    command(&ui, &mut seq, owner.telegram_id, "7 28 7").await;
    assert_eq!(dialog(&f, &owner).await.step, "ready");
    callback(&ui, &mut seq, owner.telegram_id, old_save.clone()).await;
    assert_eq!(last(&screens).await["text"], tr("en", "stale-action"));
    let mut tx = f.db.begin().await.unwrap();
    assert!(
        get::<Draft>(&mut *tx, first)
            .await
            .unwrap()
            .saved_secret
            .is_none()
    );
    tx.commit().await.unwrap();
    press(&ui, &screens, &mut seq, owner.telegram_id, "Continue draft").await;
    press(
        &ui,
        &screens,
        &mut seq,
        owner.telegram_id,
        "Save and encrypt",
    )
    .await;
    let second = ready_draft(&f, &owner, plan, &people).await;
    assert_ne!(first, second);
    callback(&ui, &mut seq, owner.telegram_id, old_save).await;
    assert_eq!(last(&screens).await["text"], tr("en", "stale-action"));
    let mut tx = f.db.begin().await.unwrap();
    assert!(
        get::<Draft>(&mut *tx, second)
            .await
            .unwrap()
            .saved_secret
            .is_none()
    );
    assert!(
        get::<Draft>(&mut *tx, first)
            .await
            .unwrap()
            .saved_secret
            .is_some()
    );
    server.abort();
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn draft_selection_resumes_and_updates_in_place_after_settings() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    f.engine.locale(owner.id, "en").await.unwrap();
    ready_draft(&f, &owner, plan, &people).await;
    let (ui, screens, server) = ui(&f).await;
    let mut seq = 13000;
    command(&ui, &mut seq, owner.telegram_id, "/start").await;
    press(&ui, &screens, &mut seq, owner.telegram_id, "My plan").await;
    press(&ui, &screens, &mut seq, owner.telegram_id, "Continue draft").await;
    press(
        &ui,
        &screens,
        &mut seq,
        owner.telegram_id,
        "Change people and timing",
    )
    .await;
    press(&ui, &screens, &mut seq, owner.telegram_id, "○ 2003").await;
    assert_eq!(screens.lock().await.last().unwrap().0, "editMessageText");
    let before = dialog(&f, &owner).await;
    command(&ui, &mut seq, owner.telegram_id, "/settings").await;
    press(&ui, &screens, &mut seq, owner.telegram_id, "← Back").await;
    press(&ui, &screens, &mut seq, owner.telegram_id, "My plan").await;
    press(&ui, &screens, &mut seq, owner.telegram_id, "Continue draft").await;
    assert!(labels(&last(&screens).await).contains(&"✓ 2003"));
    let after = dialog(&f, &owner).await;
    assert_eq!(after.selected, before.selected);
    assert_eq!(after.guardians, before.guardians);
    assert_eq!(after.recipients, before.recipients);
    // Dialog age does not expire a draft that still has valid Engine content TTL.
    let mut tx = f.db.begin().await.unwrap();
    let mut d = after;
    d.expires_at = 0;
    put(&mut *tx, Some(plan), &d).await.unwrap();
    tx.commit().await.unwrap();
    command(&ui, &mut seq, owner.telegram_id, "/start").await;
    press(&ui, &screens, &mut seq, owner.telegram_id, "My plan").await;
    assert!(labels(&last(&screens).await).contains(&"Continue draft"));
    server.abort();
}

#[tokio::test]
#[ignore = "requires explicitly configured local PostgreSQL"]
async fn cancellation_and_deletion_screens_name_their_scope() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    f.engine.locale(owner.id, "en").await.unwrap();
    let (secret, _) = secret(&f, &owner, plan, &people, false).await;
    let (ui, screens, server) = ui(&f).await;
    let mut seq = 14000;
    command(&ui, &mut seq, people[0].telegram_id, "/guardians").await;
    let scope_screen = screens
        .lock()
        .await
        .iter()
        .map(|(_, body)| body)
        .find(|body| {
            body["text"]
                .as_str()
                .is_some_and(|text| text.contains(&secret.to_string()))
        })
        .unwrap()
        .clone();
    let secret_cancel = button(&scope_screen, "Request cancellation of this secret");
    let plan_cancel = button(&scope_screen, "Request cancellation of the whole plan");
    let mut tx = f.db.begin().await.unwrap();
    assert_eq!(
        get::<Action>(&mut *tx, Id::parse_str(&secret_cancel).unwrap())
            .await
            .unwrap()
            .target,
        Some(secret)
    );
    assert_eq!(
        get::<Action>(&mut *tx, Id::parse_str(&plan_cancel).unwrap())
            .await
            .unwrap()
            .target,
        None
    );
    tx.commit().await.unwrap();
    command(&ui, &mut seq, owner.telegram_id, "/status").await;
    let delete = screens
        .lock()
        .await
        .iter()
        .rev()
        .map(|(_, body)| body)
        .find(|body| {
            body["text"]
                .as_str()
                .is_some_and(|text| text.starts_with(&secret.to_string()))
        })
        .map(|body| button(body, "Delete permanently"))
        .unwrap();
    callback(&ui, &mut seq, owner.telegram_id, delete).await;
    assert_eq!(
        last(&screens).await["text"],
        tr("en", "delete-secret-confirm")
    );
    assert!(screens.lock().await.iter().any(|(_, body)| {
        body["text"].as_str().is_some_and(|text| {
            text.contains("Last activity confirmation:") && text.contains("UTC")
        })
    }));
    server.abort();
}
