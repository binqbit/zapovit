use super::menu_navigation::{Screens, button, command, labels, last};
use super::*;
use adapters::{bot::BotUi, localization::tr, telegram::Telegram};
use axum::{
    Json, Router,
    extract::Path,
    routing::{get as http_get, post},
};
use serde_json::{Value, json};

async fn ui(f: &Fixture) -> (BotUi, Screens, tokio::task::JoinHandle<()>) {
    let screens: Screens = Default::default();
    let captured = screens.clone();
    let app = Router::new().route(
        "/bot1:test/{method}",
        post(move |Path(method): Path<String>, Json(mut body): Json<Value>| {
            let screens = captured.clone();
            async move {
                if method == "answerCallbackQuery" {
                    return Json(json!({"ok":true,"result":true}));
                }
                if method == "getFile" {
                    return Json(json!({"ok": true, "result": {"file_path": "synthetic.txt", "file_size": 4}}));
                }
                if method == "getChat" {
                    return Json(json!({"ok":false,"error_code":400,"description":"synthetic legacy account"}));
                }
                if method == "editMessageReplyMarkup" {
                    for (_, screen) in screens.lock().await.iter_mut() {
                        if screen["message_id"] == body["message_id"] {
                            screen["_menu_cleared"] = json!(true);
                        }
                    }
                    return Json(json!({"ok":true,"result":{"message_id":body["message_id"]}}));
                }
                assert!(matches!(method.as_str(), "sendMessage" | "editMessageText"));
                let mut screens = screens.lock().await;
                let id = body["message_id"].as_i64().unwrap_or(100 + screens.len() as i64);
                body["message_id"] = json!(id);
                screens.push((method, body));
                Json(json!({"ok":true,"result":{"message_id":id}}))
            }
        }),
    );
    let app = app.route(
        "/file/bot1:test/synthetic.txt",
        http_get(|| async { "test" }),
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

async fn callback(ui: &BotUi, screens: &Screens, seq: &mut i64, user: i64, data: String) {
    let message = screens
        .lock()
        .await
        .iter()
        .rev()
        .find_map(|(_, body)| {
            body["reply_markup"]["inline_keyboard"]
                .as_array()
                .and_then(|rows| {
                    rows.iter()
                        .filter_map(Value::as_array)
                        .flatten()
                        .any(|button| button["callback_data"] == data)
                        .then(|| body["message_id"].as_i64().unwrap())
                })
        })
        .unwrap_or(42);
    *seq += 1;
    ui.handle(
        100,
        &json!({"update_id":*seq,"callback_query":{
            "id":seq.to_string(),"from":{"id":user,"is_bot":false},"data":data,
            "message":{"message_id":message,"chat":{"id":user,"type":"private"}}
        }}),
    )
    .await
    .unwrap();
}

async fn press(ui: &BotUi, screens: &Screens, seq: &mut i64, user: i64, label: &str) {
    let data = button(&last(screens).await, label);
    callback(ui, screens, seq, user, data).await;
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
        &DraftSession {
            id: owner.id,
            dialog: Dialog {
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
                timing: Some(policy.timing),
            },
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    draft
}

async fn dialog(f: &Fixture, owner: &Account) -> Dialog {
    let mut tx = f.db.begin().await.unwrap();
    get::<DraftSession>(&mut *tx, owner.id)
        .await
        .unwrap()
        .dialog
}

async fn edit_people(ui: &BotUi, screens: &Screens, seq: &mut i64, user: i64) {
    if labels(&last(screens).await).contains(&"Add more blocks") {
        press(ui, screens, seq, user, "Add more blocks").await;
    }
    press(ui, screens, seq, user, "More options").await;
    press(ui, screens, seq, user, "Change people and timing").await;
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
    command(&ui, &mut seq, owner.telegram_id, &tr("en", "nav-home")).await;
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
    command(&ui, &mut seq, owner.telegram_id, "A new synthetic block").await;
    edit_people(&ui, &screens, &mut seq, owner.telegram_id).await;
    let guardian_toggle = button(&last(&screens).await, "○ 2002");
    press(&ui, &screens, &mut seq, owner.telegram_id, "Continue →").await;
    callback(&ui, &screens, &mut seq, owner.telegram_id, guardian_toggle).await;
    assert_eq!(last(&screens).await["text"], tr("en", "stale-action"));
    let current = dialog(&f, &owner).await;
    assert_eq!(current.step, "recipients");
    assert_eq!(current.selected, [people[1].id].into());
    press(&ui, &screens, &mut seq, owner.telegram_id, "Continue draft").await;
    press(&ui, &screens, &mut seq, owner.telegram_id, "Continue →").await;
    press(&ui, &screens, &mut seq, owner.telegram_id, "Continue →").await;
    press(&ui, &screens, &mut seq, owner.telegram_id, "1 / 1").await;
    press(
        &ui,
        &screens,
        &mut seq,
        owner.telegram_id,
        "Set my own intervals",
    )
    .await;
    command(&ui, &mut seq, owner.telegram_id, "0").await;
    assert_eq!(dialog(&f, &owner).await.step, "timing-reminder");
    assert!(
        screens
            .lock()
            .await
            .iter()
            .any(|(_, body)| body["text"] == tr("en", "invalid-timing"))
    );
    command(&ui, &mut seq, owner.telegram_id, "7").await;
    command(&ui, &mut seq, owner.telegram_id, "28").await;
    command(&ui, &mut seq, owner.telegram_id, "7").await;
    assert_eq!(dialog(&f, &owner).await.step, "ready");
    callback(&ui, &screens, &mut seq, owner.telegram_id, old_save.clone()).await;
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
    let sealed = button(&last(&screens).await, "I understand — encrypt and save");
    callback(&ui, &screens, &mut seq, owner.telegram_id, sealed.clone()).await;
    callback(&ui, &screens, &mut seq, owner.telegram_id, sealed).await;
    assert!(
        last(&screens).await["text"]
            .as_str()
            .unwrap()
            .starts_with(&tr("en", "history-explained"))
    );
    let second = ready_draft(&f, &owner, plan, &people).await;
    assert_ne!(first, second);
    callback(&ui, &screens, &mut seq, owner.telegram_id, old_save).await;
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
    command(&ui, &mut seq, owner.telegram_id, &tr("en", "nav-home")).await;
    press(&ui, &screens, &mut seq, owner.telegram_id, "Continue draft").await;
    edit_people(&ui, &screens, &mut seq, owner.telegram_id).await;
    press(&ui, &screens, &mut seq, owner.telegram_id, "○ 2003").await;
    assert_eq!(screens.lock().await.last().unwrap().0, "editMessageText");
    let before = dialog(&f, &owner).await;
    command(&ui, &mut seq, owner.telegram_id, "/settings").await;
    press(&ui, &screens, &mut seq, owner.telegram_id, "← Back").await;
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
    put(
        &mut *tx,
        Some(plan),
        &DraftSession {
            id: d.id,
            dialog: d,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    command(&ui, &mut seq, owner.telegram_id, &tr("en", "nav-home")).await;
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
    f.engine
        .control(owner.id, plan, Id::new_v4(), Control::Rearm)
        .await
        .unwrap();
    let (ui, screens, server) = ui(&f).await;
    let mut seq = 14000;
    command(&ui, &mut seq, people[0].telegram_id, "/guardians").await;
    let first = labels(&last(&screens).await)[0].to_owned();
    press(&ui, &screens, &mut seq, people[0].telegram_id, &first).await;
    press(
        &ui,
        &screens,
        &mut seq,
        people[0].telegram_id,
        "More details",
    )
    .await;
    let scope_screen = last(&screens).await;
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
    let first = labels(&last(&screens).await)[0].to_owned();
    press(&ui, &screens, &mut seq, owner.telegram_id, &first).await;
    press(&ui, &screens, &mut seq, owner.telegram_id, "More options").await;
    let delete = button(&last(&screens).await, "Delete permanently");
    callback(&ui, &screens, &mut seq, owner.telegram_id, delete).await;
    assert_eq!(
        last(&screens).await["text"],
        tr("en", "delete-secret-confirm")
    );
    assert!(screens.lock().await.iter().any(|(_, body)| {
        body["text"]
            .as_str()
            .is_some_and(|text| text.contains("UTC"))
    }));
    let confirmed = button(&last(&screens).await, "Yes, delete permanently");
    callback(
        &ui,
        &screens,
        &mut seq,
        owner.telegram_id,
        confirmed.clone(),
    )
    .await;
    callback(&ui, &screens, &mut seq, owner.telegram_id, confirmed).await;
    assert!(
        last(&screens).await["text"]
            .as_str()
            .unwrap()
            .starts_with(&tr("en", "history-explained"))
    );
    server.abort();
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn invitation_preview_requires_consent_and_content_waits_for_people() {
    let f = fixture().await;
    let owner = f.engine.account(7501, 7501, "en").await.unwrap();
    let plan = f.engine.create_profile(owner.id).await.unwrap();
    let (ui, screens, server) = ui(&f).await;
    let mut seq = 17000;
    let (_, new) = ui
        .button(&owner, "new-secret", None, Some(plan), true, "New secret")
        .await
        .unwrap();
    callback(&ui, &screens, &mut seq, owner.telegram_id, new).await;
    assert!(
        last(&screens).await["text"]
            .as_str()
            .unwrap()
            .starts_with(&tr("en", "setup-recovery-title"))
    );
    let mut tx = f.db.begin().await.unwrap();
    assert!(
        list::<Draft>(&mut *tx, Some(plan))
            .await
            .unwrap()
            .is_empty()
    );
    tx.commit().await.unwrap();
    let invite = f.engine.invite(owner.id, plan).await.unwrap();
    command(
        &ui,
        &mut seq,
        7502,
        &format!("/start invite_{}", invite.simple()),
    )
    .await;
    let guest = f.engine.account(7502, 7502, "en").await.unwrap();
    assert!(f.engine.own_plan(guest.id).await.is_err());
    assert!(f.engine.contacts(owner.id, plan).await.unwrap().is_empty());
    assert!(
        last(&screens).await["text"]
            .as_str()
            .unwrap()
            .contains("7501")
    );
    press(&ui, &screens, &mut seq, 7502, &tr("en", "accept-invite")).await;
    let contacts = f.engine.contacts(owner.id, plan).await.unwrap();
    assert_eq!(contacts.len(), 1);
    assert!(!contacts[0].confirmed);
    server.abort();
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn upload_finishes_while_separate_prompt_preserves_the_draft() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    f.engine.locale(owner.id, "en").await.unwrap();
    let draft_id = ready_draft(&f, &owner, plan, &people).await;
    let mut d = dialog(&f, &owner).await;
    d.step = "file-pending".into();
    let mut tx = f.db.begin().await.unwrap();
    let now = tx.now().await.unwrap();
    put(
        &mut *tx,
        Some(plan),
        &DraftSession {
            id: owner.id,
            dialog: d.clone(),
        },
    )
    .await
    .unwrap();
    let mut prompt = d;
    prompt.draft_id = None;
    prompt.step = "code".into();
    put(&mut *tx, Some(plan), &prompt).await.unwrap();
    let job = Job {
        id: Id::new_v4(),
        plan_id: Some(plan),
        task: Task::DownloadFile {
            account_id: owner.id,
            draft_id,
            file_id: f
                .engine
                .crypto
                .wrap("telegram-file", draft_id, b"synthetic-file")
                .unwrap(),
            name: f
                .engine
                .crypto
                .wrap(
                    "telegram-file-meta",
                    draft_id,
                    &serde_json::to_vec(&("test.txt", "")).unwrap(),
                )
                .unwrap(),
            source_message: 99,
        },
        state: PartState::Claimed,
        due_at: now,
        expires_at: now + 900,
        lease_until: now + 120,
        lease_token: Id::new_v4(),
        attempts: 1,
        message_id: None,
        priority: 4,
    };
    put(&mut *tx, Some(plan), &job).await.unwrap();
    tx.commit().await.unwrap();
    let (ui, screens, server) = ui(&f).await;
    ui.process_job(job.clone()).await.unwrap();
    let mut tx = f.db.begin().await.unwrap();
    assert_eq!(
        get::<Job>(&mut *tx, job.id).await.unwrap().state,
        PartState::Sent
    );
    assert_eq!(
        get::<Dialog>(&mut *tx, owner.id).await.unwrap().step,
        "code"
    );
    let draft = get::<DraftSession>(&mut *tx, owner.id)
        .await
        .unwrap()
        .dialog;
    assert_eq!(draft.draft_id, Some(draft_id));
    assert_eq!(draft.guardians, prompt.guardians);
    assert_eq!(draft.step, "builder");
    tx.commit().await.unwrap();
    assert_eq!(
        f.engine
            .draft_blocks(owner.id, draft_id)
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        screens.lock().await.len(),
        1,
        "Upload feedback must not replace a sensitive prompt with the draft menu"
    );
    let mut seq = 18000;
    command(&ui, &mut seq, owner.telegram_id, &tr("en", "nav-home")).await;
    press(&ui, &screens, &mut seq, owner.telegram_id, "Continue draft").await;
    assert!(labels(&last(&screens).await).contains(&"Continue →"));
    server.abort();
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn stale_draft_buttons_preserve_prompts_and_late_keys_never_become_content() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    f.engine.locale(owner.id, "en").await.unwrap();
    let draft = ready_draft(&f, &owner, plan, &people).await;
    let (ui, screens, server) = ui(&f).await;
    let mut seq = 19000;
    command(&ui, &mut seq, owner.telegram_id, &tr("en", "nav-home")).await;
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
    let mut prompt = dialog(&f, &owner).await;
    prompt.draft_id = None;
    prompt.step = "code".into();
    let mut tx = f.db.begin().await.unwrap();
    put(&mut *tx, Some(plan), &prompt).await.unwrap();
    tx.commit().await.unwrap();
    callback(&ui, &screens, &mut seq, owner.telegram_id, old_save).await;
    let mut tx = f.db.begin().await.unwrap();
    assert_eq!(
        get::<Dialog>(&mut *tx, owner.id).await.unwrap().step,
        "code"
    );
    tx.commit().await.unwrap();
    command(&ui, &mut seq, owner.telegram_id, &tr("en", "nav-home")).await;
    press(&ui, &screens, &mut seq, owner.telegram_id, "Continue draft").await;
    for token in ["R1.synthetic-expired-key", "  Z1.synthetic-expired-code"] {
        command(&ui, &mut seq, owner.telegram_id, token).await;
        assert_eq!(last(&screens).await["text"], tr("en", "stale-action"));
    }
    assert_eq!(
        f.engine.draft_blocks(owner.id, draft).await.unwrap().len(),
        1
    );
    let mut tx = f.db.begin().await.unwrap();
    for message in [seq - 1, seq] {
        let event = Id::from_u128((100_u128 << 64) | message as u128);
        assert!(matches!(
            get::<Job>(&mut *tx, event).await.unwrap().task,
            Task::CleanupMessage { .. }
        ));
    }
    server.abort();
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn home_guides_individually_paused_secrets_and_explains_write_hold() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    f.engine.locale(owner.id, "en").await.unwrap();
    let (secret, _) = secret(&f, &owner, plan, &people, false).await;
    f.engine
        .control(owner.id, plan, Id::new_v4(), Control::Rearm)
        .await
        .unwrap();
    f.engine
        .control(
            owner.id,
            plan,
            Id::new_v4(),
            Control::StopSecret { secret_id: secret },
        )
        .await
        .unwrap();
    let (ui, screens, server) = ui(&f).await;
    let mut seq = 20000;
    command(&ui, &mut seq, owner.telegram_id, &tr("en", "nav-home")).await;
    assert_eq!(labels(&last(&screens).await)[0], "Secrets");
    press(&ui, &screens, &mut seq, owner.telegram_id, "Secrets").await;
    let first = labels(&last(&screens).await)[0].to_owned();
    press(&ui, &screens, &mut seq, owner.telegram_id, &first).await;
    assert!(labels(&last(&screens).await).contains(&tr("en", "resume-secret-review").as_str()));
    assert!(!labels(&last(&screens).await).contains(&"Stop this secret"));
    adapters::backup::set_maintenance(&f.db, true)
        .await
        .unwrap();
    let (_, create) = ui
        .button(&owner, "new-secret", None, Some(plan), true, "New secret")
        .await
        .unwrap();
    callback(&ui, &screens, &mut seq, owner.telegram_id, create).await;
    assert_eq!(last(&screens).await["text"], tr("en", "service-delayed"));
    let mut tx = f.db.begin().await.unwrap();
    assert!(
        list::<Draft>(&mut *tx, Some(plan))
            .await
            .unwrap()
            .iter()
            .all(|draft| draft.saved_secret.is_some())
    );
    server.abort();
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn draft_people_selection_has_explicit_progress_and_never_backs_into_unconfigured_content() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    f.engine.locale(owner.id, "en").await.unwrap();
    f.engine
        .update_account_display(
            people[0].id,
            Some("Nadia"),
            Some("Melnyk"),
            Some("nadia_test"),
        )
        .await
        .unwrap();
    f.engine
        .update_account_display(people[1].id, Some("Danylo"), None, None)
        .await
        .unwrap();
    let doctor = f
        .engine
        .contacts(owner.id, plan)
        .await
        .unwrap()
        .into_iter()
        .find(|person| person.account_id == people[1].id)
        .unwrap();
    f.engine
        .set_label(owner.id, plan, doctor.id, "Doctor")
        .await
        .unwrap();
    let (ui, screens, server) = ui(&f).await;
    let mut seq = 22000;
    let (_, new) = ui
        .button(&owner, "new-secret", None, Some(plan), true, "New secret")
        .await
        .unwrap();
    callback(&ui, &screens, &mut seq, owner.telegram_id, new).await;
    assert_eq!(dialog(&f, &owner).await.step, "guardians");
    let initial = last(&screens).await;
    assert_eq!(
        labels(&initial).len(),
        7,
        "five people, invitation and one way back"
    );
    assert!(labels(&initial).contains(&"○ Nadia Melnyk"));
    assert!(
        labels(&initial).contains(&"○ Doctor"),
        "private label wins over Telegram display name"
    );
    assert!(!labels(&initial).contains(&"Text"));
    assert!(!labels(&initial).contains(&"Discard draft"));
    assert!(
        initial["text"]
            .as_str()
            .unwrap()
            .starts_with(&format!("{}\n\n", tr("en", "draft-stage-guardians")))
    );
    press(&ui, &screens, &mut seq, owner.telegram_id, "← Back").await;
    assert_eq!(
        dialog(&f, &owner).await.step,
        "guardians",
        "leaving initial selection preserves its stage"
    );
    command(&ui, &mut seq, owner.telegram_id, "/continue").await;
    press(&ui, &screens, &mut seq, owner.telegram_id, "○ Nadia Melnyk").await;
    assert_eq!(
        dialog(&f, &owner).await.step,
        "guardians",
        "selecting does not silently advance a multi-person choice"
    );
    assert_eq!(labels(&last(&screens).await)[0], "Continue →");
    press(&ui, &screens, &mut seq, owner.telegram_id, "○ Doctor").await;
    let selected = last(&screens).await;
    assert!(
        selected["text"]
            .as_str()
            .unwrap()
            .contains("Selected: 2/10")
    );
    assert!(selected["text"].as_str().unwrap().contains("Nadia Melnyk"));
    assert!(selected["text"].as_str().unwrap().contains("Doctor"));
    assert_eq!(screens.lock().await.last().unwrap().0, "editMessageText");
    press(&ui, &screens, &mut seq, owner.telegram_id, "← Back").await;
    command(&ui, &mut seq, owner.telegram_id, "/continue").await;
    assert_eq!(
        dialog(&f, &owner).await.selected,
        [people[0].id, people[1].id].into()
    );
    press(&ui, &screens, &mut seq, owner.telegram_id, "Continue →").await;
    assert_eq!(dialog(&f, &owner).await.step, "recipients");
    assert!(
        last(&screens).await["text"]
            .as_str()
            .unwrap()
            .contains("Nobody selected yet")
    );
    press(&ui, &screens, &mut seq, owner.telegram_id, "○ Nadia Melnyk").await;
    press(&ui, &screens, &mut seq, owner.telegram_id, "← Back").await;
    assert_eq!(dialog(&f, &owner).await.step, "guardians");
    assert_eq!(
        dialog(&f, &owner).await.selected,
        [people[0].id, people[1].id].into()
    );
    press(&ui, &screens, &mut seq, owner.telegram_id, "Continue →").await;
    assert_eq!(
        dialog(&f, &owner).await.selected,
        [people[0].id].into(),
        "Back preserves the recipient choice separately"
    );
    press(&ui, &screens, &mut seq, owner.telegram_id, "Continue →").await;
    assert_eq!(dialog(&f, &owner).await.step, "builder");
    assert_eq!(labels(&last(&screens).await), ["More options", "← Back"]);
    command(
        &ui,
        &mut seq,
        owner.telegram_id,
        "Synthetic message directly in the conversation",
    )
    .await;
    assert_eq!(
        labels(&last(&screens).await),
        ["Continue →", "More options", "← Back"]
    );
    press(&ui, &screens, &mut seq, owner.telegram_id, "Continue →").await;
    assert_eq!(dialog(&f, &owner).await.step, "threshold");
    press(&ui, &screens, &mut seq, owner.telegram_id, "← Back").await;
    assert_eq!(dialog(&f, &owner).await.step, "builder");
    server.abort();
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn draft_timing_backtracks_one_step_and_content_tools_stay_contextual() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    f.engine.locale(owner.id, "en").await.unwrap();
    ready_draft(&f, &owner, plan, &people).await;
    // An unfinished policy edit may leave tentative choices different from the
    // stored policy. The review must explain the rules that will be saved.
    let mut draft_session = dialog(&f, &owner).await;
    draft_session.threshold = 2;
    let mut tx = f.db.begin().await.unwrap();
    put(
        &mut *tx,
        Some(plan),
        &DraftSession {
            id: owner.id,
            dialog: draft_session,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let (ui, screens, server) = ui(&f).await;
    let mut seq = 23000;
    command(&ui, &mut seq, owner.telegram_id, "/continue").await;
    assert!(
        last(&screens).await["text"]
            .as_str()
            .unwrap()
            .contains(&tr("en", "single-approval-warning"))
    );
    edit_people(&ui, &screens, &mut seq, owner.telegram_id).await;
    for _ in 0..3 {
        press(&ui, &screens, &mut seq, owner.telegram_id, "Continue →").await;
    }
    press(&ui, &screens, &mut seq, owner.telegram_id, "1 / 1").await;
    press(
        &ui,
        &screens,
        &mut seq,
        owner.telegram_id,
        "Set my own intervals",
    )
    .await;
    assert_eq!(
        labels(&last(&screens).await),
        ["← Back"],
        "one input prompt has only its preceding step"
    );
    command(&ui, &mut seq, owner.telegram_id, "7").await;
    assert_eq!(dialog(&f, &owner).await.step, "timing-inactivity");
    press(&ui, &screens, &mut seq, owner.telegram_id, "← Back").await;
    assert_eq!(dialog(&f, &owner).await.step, "timing-reminder");
    command(&ui, &mut seq, owner.telegram_id, "8").await;
    command(&ui, &mut seq, owner.telegram_id, "32").await;
    assert_eq!(dialog(&f, &owner).await.step, "timing-wait");
    for expected in ["timing-inactivity", "timing-reminder", "timing-choice"] {
        press(&ui, &screens, &mut seq, owner.telegram_id, "← Back").await;
        assert_eq!(dialog(&f, &owner).await.step, expected);
    }
    assert_eq!(
        dialog(&f, &owner).await.timing.unwrap().reminder_seconds,
        8 * DAY
    );
    assert_eq!(labels(&last(&screens).await).len(), 3);
    press(
        &ui,
        &screens,
        &mut seq,
        owner.telegram_id,
        "Use 7 / 28 / 7 days",
    )
    .await;
    assert_eq!(
        labels(&last(&screens).await),
        ["Preview the draft", "Save and encrypt", "Add more blocks"]
    );
    press(
        &ui,
        &screens,
        &mut seq,
        owner.telegram_id,
        "Add more blocks",
    )
    .await;
    assert_eq!(
        labels(&last(&screens).await),
        ["Continue →", "More options", "← Back"]
    );
    press(&ui, &screens, &mut seq, owner.telegram_id, "More options").await;
    press(&ui, &screens, &mut seq, owner.telegram_id, "Text").await;
    assert_eq!(labels(&last(&screens).await), ["← Back"]);
    assert!(
        last(&screens).await["text"]
            .as_str()
            .unwrap()
            .starts_with("Text\n\n")
    );
    assert!(
        !last(&screens).await["text"]
            .as_str()
            .unwrap()
            .contains("Draft available until"),
        "focused content input does not repeat the overall draft status"
    );
    press(&ui, &screens, &mut seq, owner.telegram_id, "← Back").await;
    assert!(labels(&last(&screens).await).contains(&"Discard draft"));
    assert!(labels(&last(&screens).await).contains(&"Name this secret"));
    press(
        &ui,
        &screens,
        &mut seq,
        owner.telegram_id,
        "Name this secret",
    )
    .await;
    assert_eq!(labels(&last(&screens).await), ["← Back"]);
    press(&ui, &screens, &mut seq, owner.telegram_id, "← Back").await;
    assert_eq!(dialog(&f, &owner).await.step, "options");
    let mut tx = f.db.begin().await.unwrap();
    assert!(matches!(
        get::<Dialog>(&mut *tx, owner.id).await,
        Err(Error::NotFound)
    ));
    tx.commit().await.unwrap();
    press(
        &ui,
        &screens,
        &mut seq,
        owner.telegram_id,
        "Name this secret",
    )
    .await;
    command(&ui, &mut seq, owner.telegram_id, "Instructions").await;
    assert_eq!(dialog(&f, &owner).await.step, "options");
    assert!(labels(&last(&screens).await).contains(&"More block types"));
    press(
        &ui,
        &screens,
        &mut seq,
        owner.telegram_id,
        "More block types",
    )
    .await;
    press(&ui, &screens, &mut seq, owner.telegram_id, "Hidden text").await;
    for expected in ["formatting", "options", "builder"] {
        press(&ui, &screens, &mut seq, owner.telegram_id, "← Back").await;
        assert_eq!(dialog(&f, &owner).await.step, expected);
    }
    server.abort();
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn draft_people_pages_preserve_selection_and_resume_the_same_page() {
    let f = fixture().await;
    let (owner, plan, _) = participants(&f).await;
    f.engine.locale(owner.id, "en").await.unwrap();
    for id in 9701..9705 {
        let person = f.engine.account(id, id, "en").await.unwrap();
        let invitation = f.engine.invite(owner.id, plan).await.unwrap();
        f.engine.accept_invite(person.id, invitation).await.unwrap();
        let contact = f
            .engine
            .contacts(owner.id, plan)
            .await
            .unwrap()
            .into_iter()
            .find(|contact| contact.account_id == person.id)
            .unwrap();
        f.engine
            .confirm_participant(owner.id, plan, contact.id)
            .await
            .unwrap();
    }
    let (ui, screens, server) = ui(&f).await;
    let mut seq = 24000;
    let (_, new) = ui
        .button(&owner, "new-secret", None, Some(plan), true, "New secret")
        .await
        .unwrap();
    callback(&ui, &screens, &mut seq, owner.telegram_id, new).await;
    let first_page = last(&screens).await;
    assert_eq!(
        labels(&first_page)
            .iter()
            .filter(|label| label.starts_with("○ "))
            .count(),
        6
    );
    assert!(labels(&first_page).len() <= 9);
    let first = labels(&first_page)
        .into_iter()
        .find(|label| label.starts_with("○ "))
        .unwrap()
        .to_owned();
    press(&ui, &screens, &mut seq, owner.telegram_id, &first).await;
    press(&ui, &screens, &mut seq, owner.telegram_id, "Next →").await;
    assert_eq!(dialog(&f, &owner).await.step, "guardians.page.1");
    let page = last(&screens).await;
    let second = labels(&page)
        .into_iter()
        .find(|label| label.starts_with("○ "))
        .unwrap()
        .to_owned();
    press(&ui, &screens, &mut seq, owner.telegram_id, &second).await;
    let before = dialog(&f, &owner).await;
    assert_eq!(before.selected.len(), 2);
    command(&ui, &mut seq, owner.telegram_id, &tr("en", "nav-home")).await;
    command(&ui, &mut seq, owner.telegram_id, "/continue").await;
    assert_eq!(dialog(&f, &owner).await.step, "guardians.page.1");
    assert_eq!(dialog(&f, &owner).await.selected, before.selected);
    assert!(
        last(&screens).await["text"]
            .as_str()
            .unwrap()
            .contains("Selected: 2/10")
    );
    press(&ui, &screens, &mut seq, owner.telegram_id, "← Previous").await;
    assert!(
        labels(&last(&screens).await)
            .contains(&format!("✓ {}", first.strip_prefix("○ ").unwrap()).as_str())
    );
    press(&ui, &screens, &mut seq, owner.telegram_id, "Continue →").await;
    assert_eq!(dialog(&f, &owner).await.guardians, before.selected);
    assert_eq!(dialog(&f, &owner).await.step, "recipients");
    server.abort();
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn draft_new_questions_retire_previous_buttons_even_after_back_to_the_same_step() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    f.engine.locale(owner.id, "en").await.unwrap();
    ready_draft(&f, &owner, plan, &people).await;
    let (ui, screens, server) = ui(&f).await;
    let mut seq = 25000;
    command(&ui, &mut seq, owner.telegram_id, "/continue").await;
    edit_people(&ui, &screens, &mut seq, owner.telegram_id).await;
    let first_question = last(&screens).await;
    let old_select = button(&first_question, "○ 2003");
    let old_message = first_question["message_id"].as_i64().unwrap();
    assert_eq!(dialog(&f, &owner).await.reply_to, Some(old_message));
    press(&ui, &screens, &mut seq, owner.telegram_id, "Continue →").await;
    assert_eq!(screens.lock().await.last().unwrap().0, "sendMessage");
    assert_ne!(last(&screens).await["message_id"], old_message);
    assert!(
        screens.lock().await.iter().any(|(_, body)| {
            body["message_id"] == old_message && body["_menu_cleared"] == true
        })
    );
    press(&ui, &screens, &mut seq, owner.telegram_id, "← Back").await;
    assert_eq!(dialog(&f, &owner).await.step, "guardians");
    assert_eq!(screens.lock().await.last().unwrap().0, "sendMessage");
    assert_ne!(dialog(&f, &owner).await.reply_to, Some(old_message));
    callback(&ui, &screens, &mut seq, owner.telegram_id, old_select).await;
    assert_eq!(last(&screens).await["text"], tr("en", "stale-action"));
    assert_eq!(dialog(&f, &owner).await.guardians, [people[0].id].into());
    command(&ui, &mut seq, owner.telegram_id, "/continue").await;
    let current = dialog(&f, &owner).await.reply_to;
    press(&ui, &screens, &mut seq, owner.telegram_id, "○ 2003").await;
    assert_eq!(screens.lock().await.last().unwrap().0, "editMessageText");
    assert_eq!(dialog(&f, &owner).await.reply_to, current);
    assert_eq!(
        dialog(&f, &owner).await.guardians,
        [people[0].id, people[2].id].into()
    );
    let before_invite = dialog(&f, &owner).await;
    let before_invite_button = button(&last(&screens).await, "○ 2004");
    press(
        &ui,
        &screens,
        &mut seq,
        owner.telegram_id,
        &tr("en", "setup-add-person"),
    )
    .await;
    assert_eq!(dialog(&f, &owner).await.step, "guardians");
    assert_eq!(dialog(&f, &owner).await.reply_to, None);
    callback(
        &ui,
        &screens,
        &mut seq,
        owner.telegram_id,
        before_invite_button,
    )
    .await;
    assert_eq!(last(&screens).await["text"], tr("en", "stale-action"));
    assert_eq!(dialog(&f, &owner).await.guardians, before_invite.guardians);
    command(&ui, &mut seq, owner.telegram_id, "/continue").await;
    assert_eq!(dialog(&f, &owner).await.step, "guardians");
    assert!(dialog(&f, &owner).await.reply_to.is_some());
    server.abort();
}
