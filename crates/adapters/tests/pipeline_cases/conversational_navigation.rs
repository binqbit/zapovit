use super::menu_navigation::{Screens, labels};
use super::*;
use adapters::{bot::BotUi, localization::tr, telegram::Telegram};
use axum::{
    Json, Router,
    extract::Path,
    routing::{get as http_get, post},
};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicI64, Ordering};

struct Conversation {
    ui: BotUi,
    screens: Screens,
    sequence: i64,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Conversation {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Conversation {
    async fn new(f: &Fixture) -> Self {
        let screens: Screens = Default::default();
        let captured = screens.clone();
        let next_message = Arc::new(AtomicI64::new(100_000));
        let app = Router::new().route(
            "/bot1:test/{method}",
            post(move |Path(method): Path<String>, Json(mut body): Json<Value>| {
                let screens = captured.clone();
                let next_message = next_message.clone();
                async move {
                    match method.as_str() {
                        "answerCallbackQuery" | "deleteMessage" | "editMessageReplyMarkup" => {
                            return Json(json!({"ok":true,"result":true}));
                        }
                        "getChat" => return Json(json!({"ok":true,"result":{
                            "id":body["chat_id"],"type":"private","first_name":"Synthetic person"
                        }})),
                        "getFile" => return Json(json!({"ok":true,"result":{
                            "file_path":"conversation.bin","file_size":4
                        }})),
                        "sendMessage" => {
                            body["message_id"] = json!(next_message.fetch_add(1, Ordering::Relaxed));
                        }
                        "editMessageText" => assert!(body["message_id"].is_i64()),
                        _ => panic!("Unexpected Telegram method {method}"),
                    }
                    let message = body["message_id"].clone();
                    screens.lock().await.push((method, body));
                    Json(json!({"ok":true,"result":{"message_id":message}}))
                }
            }),
        ).route("/file/bot1:test/conversation.bin", http_get(|| async { vec![0_u8, 255, 17, 42] }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            ui: BotUi {
                engine: f.engine.clone(),
                telegram: Telegram::new(
                    &format!("http://{address}"),
                    Zeroizing::new("1:test".into()),
                )
                .unwrap(),
                username: "synthetic_bot".into(),
            },
            screens,
            sequence: 1_000_000,
            server,
        }
    }

    async fn screen(&self, user: i64) -> (String, Value) {
        self.screens
            .lock()
            .await
            .iter()
            .rev()
            .find(|(_, body)| body["chat_id"] == user)
            .unwrap()
            .clone()
    }

    async fn message(&mut self, user: i64, first_name: &str, text: &str) -> i64 {
        self.sequence += 1;
        self.ui
            .handle(
                100,
                &json!({"update_id":self.sequence,"message":{
                    "message_id":self.sequence,"from":{"id":user,"first_name":first_name,
                        "username":format!("synthetic_{user}"),"language_code":"en","is_bot":false},
                    "chat":{"id":user,"type":"private"},"text":text
                }}),
            )
            .await
            .unwrap();
        self.sequence
    }

    async fn document(&mut self, user: i64) -> i64 {
        self.sequence += 1;
        self.ui.handle(100, &json!({"update_id":self.sequence,"message":{
            "message_id":self.sequence,"from":{"id":user,"is_bot":false},
            "chat":{"id":user,"type":"private"},"caption":"Synthetic attachment",
            "document":{"file_id":"synthetic-file","file_size":4,"file_name":"conversation.bin"}
        }})).await.unwrap();
        self.sequence
    }

    async fn action(&self, f: &Fixture, user: i64, name: &str) -> (Action, i64) {
        let (_, screen) = self.screen(user).await;
        let mut tx = f.db.begin().await.unwrap();
        for button in screen["reply_markup"]["inline_keyboard"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|row| row.as_array().unwrap())
        {
            let Some(data) = button["callback_data"].as_str() else {
                continue;
            };
            let id = Id::parse_str(data).unwrap();
            let action: Action = get(&mut *tx, id).await.unwrap();
            if action.name == name || action.name.starts_with(&format!("draft:{name}:")) {
                return (action, screen["message_id"].as_i64().unwrap());
            }
        }
        panic!(
            "Missing action {name}; visible labels: {:?}",
            labels(&screen)
        );
    }

    async fn press(&mut self, f: &Fixture, user: i64, name: &str) {
        let (action, message) = self.action(f, user, name).await;
        self.sequence += 1;
        self.ui
            .handle(
                100,
                &json!({"update_id":self.sequence,"callback_query":{
                    "id":self.sequence.to_string(),"from":{"id":user,"is_bot":false},
                    "data":action.id.simple().to_string(),
                    "message":{"message_id":message,"chat":{"id":user,"type":"private"}}
                }}),
            )
            .await
            .unwrap();
    }

    async fn next(&mut self, f: &Fixture, user: i64, action: &str) {
        let (_, before) = self.screen(user).await;
        self.press(f, user, action).await;
        let (method, after) = self.screen(user).await;
        assert_eq!(
            method, "sendMessage",
            "the next question must arrive as a new message"
        );
        assert_ne!(after["message_id"], before["message_id"]);
    }

    async fn deliver(&self, f: &Fixture, plan: Id, matches: impl Fn(&Task) -> bool) {
        let mut tx = f.db.begin().await.unwrap();
        let job = list::<Job>(&mut *tx, Some(plan))
            .await
            .unwrap()
            .into_iter()
            .find(|job| {
                matches(&job.task)
                    && matches!(job.state, PartState::Queued | PartState::RetryableFailed)
            })
            .expect("expected queued notification");
        tx.commit().await.unwrap();
        sqlx::query("DELETE FROM rate_limits WHERE key LIKE 'outbound:%'")
            .execute(&f.db.pool)
            .await
            .unwrap();
        let claimed = f
            .engine
            .claim_job(job.id)
            .await
            .unwrap()
            .expect("notification is due");
        self.ui.process_job(claimed).await.unwrap();
    }
}

async fn draft_dialog(f: &Fixture, actor: Id) -> Dialog {
    let mut tx = f.db.begin().await.unwrap();
    get::<DraftSession>(&mut *tx, actor).await.unwrap().dialog
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn creation_is_a_conversation_from_recovery_to_explicit_activation() {
    let f = fixture().await;
    let mut c = Conversation::new(&f).await;
    let owner_id = 9801;
    let person_id = 9802;
    c.message(owner_id, "Synthetic owner", "/start").await;
    c.next(&f, owner_id, "create").await;
    let owner = f.engine.account(owner_id, owner_id, "en").await.unwrap();
    let (_, plan) = f.engine.own_plan(owner.id).await.unwrap();
    assert_eq!(plan.state, PlanState::Setup);
    let before = c.screen(owner_id).await.1;
    c.message(
        owner_id,
        "Synthetic owner",
        "Synthetic text before recovery is ready",
    )
    .await;
    let (method, recovery_question) = c.screen(owner_id).await;
    assert_eq!(method, "sendMessage");
    assert_ne!(recovery_question["message_id"], before["message_id"]);
    assert!(
        recovery_question["text"]
            .as_str()
            .unwrap()
            .contains(&tr("en", "setup-recovery-title"))
    );
    let mut tx = f.db.begin().await.unwrap();
    assert!(
        list::<Draft>(&mut *tx, Some(plan.id))
            .await
            .unwrap()
            .is_empty()
    );
    tx.commit().await.unwrap();
    assert!(!f.engine.own_plan(owner.id).await.unwrap().0.recovery_saved);
    c.deliver(&f, plan.id, |task| {
        matches!(task, Task::RecoveryCode { .. })
    })
    .await;
    c.next(&f, owner_id, "ack-recovery").await;
    assert!(f.engine.own_plan(owner.id).await.unwrap().0.recovery_saved);
    c.next(&f, owner_id, "setup-invite").await;
    let (_, invitation_screen) = c.screen(owner_id).await;
    assert!(
        invitation_screen["text"]
            .as_str()
            .unwrap()
            .contains("https://t.me/synthetic_bot?start=invite_")
    );
    let invite = f
        .engine
        .invitations(owner.id, plan.id)
        .await
        .unwrap()
        .into_iter()
        .find(|invite| invite.status == InvitationStatus::Pending)
        .unwrap();
    c.message(
        person_id,
        "Nadia <&> Melnyk",
        &format!("/start invite_{}", invite.id.simple()),
    )
    .await;
    c.press(&f, person_id, "accept-invite").await;
    let person = f.engine.account(person_id, person_id, "en").await.unwrap();
    let pending = f
        .engine
        .contacts(owner.id, plan.id)
        .await
        .unwrap()
        .into_iter()
        .find(|contact| contact.account_id == person.id)
        .unwrap();
    assert!(
        !pending.confirmed,
        "acceptance must not establish owner trust"
    );
    c.next(&f, owner_id, "setup-people").await;
    c.next(&f, owner_id, "setup-person").await;
    let (_, identity) = c.screen(owner_id).await;
    assert!(
        identity["text"]
            .as_str()
            .unwrap()
            .contains("Nadia <&> Melnyk")
    );
    assert!(
        identity["text"]
            .as_str()
            .unwrap()
            .contains(&person_id.to_string())
    );
    assert!(identity["parse_mode"].is_null());
    c.next(&f, owner_id, "setup-confirm").await;
    assert!(
        f.engine
            .contacts(owner.id, plan.id)
            .await
            .unwrap()
            .iter()
            .any(|contact| contact.account_id == person.id && contact.confirmed)
    );
    c.next(&f, owner_id, "setup-continue").await;
    assert_eq!(draft_dialog(&f, owner.id).await.step, "guardians");
    c.press(&f, owner_id, &format!("select.{}", person.id.simple()))
        .await;
    c.next(&f, owner_id, "done").await;
    assert_eq!(draft_dialog(&f, owner.id).await.step, "recipients");
    c.press(&f, owner_id, &format!("select.{}", person.id.simple()))
        .await;
    c.next(&f, owner_id, "done").await;
    let d = draft_dialog(&f, owner.id).await;
    assert_eq!(
        d.step, "builder",
        "roles lead straight to the content question"
    );
    let draft_id = d.draft_id.unwrap();
    let source = c
        .message(
            owner_id,
            "Synthetic owner",
            "Synthetic secret sent directly as the answer",
        )
        .await;
    let blocks = f.engine.draft_blocks(owner.id, draft_id).await.unwrap();
    assert!(
        matches!(&blocks[..], [Block::Text { text }] if text == "Synthetic secret sent directly as the answer")
    );
    let mut tx = f.db.begin().await.unwrap();
    let cleanups: Vec<_> = list::<Job>(&mut *tx, Some(plan.id)).await.unwrap().into_iter()
        .filter(|job| matches!(job.task, Task::CleanupMessage { chat_id, message_id, .. } if chat_id == owner_id && message_id == source)).collect();
    assert_eq!(
        cleanups.len(),
        1,
        "accepted content immediately has durable source cleanup"
    );
    assert_eq!(cleanups[0].priority, 0);
    assert!(
        get::<Draft>(&mut *tx, draft_id)
            .await
            .unwrap()
            .saved_secret
            .is_none()
    );
    tx.commit().await.unwrap();
    c.next(&f, owner_id, "threshold").await;
    assert_eq!(draft_dialog(&f, owner.id).await.step, "threshold");
    c.next(&f, owner_id, "threshold.1").await;
    c.next(&f, owner_id, "timing-default").await;
    assert_eq!(draft_dialog(&f, owner.id).await.step, "ready");
    c.next(&f, owner_id, "save").await;
    c.next(&f, owner_id, "seal").await;
    let mut tx = f.db.begin().await.unwrap();
    let sealed = list::<Secret>(&mut *tx, Some(plan.id)).await.unwrap();
    assert_eq!(sealed.len(), 1);
    assert_eq!(sealed[0].state, SecretState::Provisioning);
    tx.commit().await.unwrap();
    assert_eq!(
        f.engine.own_plan(owner.id).await.unwrap().1.state,
        PlanState::Setup
    );
    c.deliver(&f, plan.id, |task| {
        matches!(task, Task::GuardianCode { .. })
    })
    .await;
    c.press(&f, person_id, "ack-grant").await;
    assert_eq!(
        f.engine.own_plan(owner.id).await.unwrap().1.state,
        PlanState::Setup,
        "guardian readiness must not activate the owner plan"
    );
    c.next(&f, owner_id, "setup-ready").await;
    assert!(labels(&c.screen(owner_id).await.1).contains(&tr("en", "setup-enable").as_str()));
    c.press(&f, owner_id, "resume").await;
    let activated = f.engine.own_plan(owner.id).await.unwrap().1;
    assert_eq!(activated.state, PlanState::Active);
    c.message(owner_id, "Synthetic owner", "/continue").await;
    assert_eq!(
        f.engine.own_plan(owner.id).await.unwrap().1.epoch,
        activated.epoch,
        "continuing an active conversation must not rearm the plan"
    );
    f.engine
        .control(owner.id, plan.id, Id::new_v4(), Control::Stop)
        .await
        .unwrap();
    let paused = f.engine.own_plan(owner.id).await.unwrap().1;
    c.message(owner_id, "Synthetic owner", "/continue").await;
    let after = f.engine.own_plan(owner.id).await.unwrap().1;
    assert_eq!(
        (after.state, after.epoch),
        (PlanState::Paused, paused.epoch)
    );
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn wrong_stage_text_repeats_the_question_without_saving_content() {
    let f = fixture().await;
    let (owner, plan, _) = participants(&f).await;
    f.engine.locale(owner.id, "en").await.unwrap();
    let mut c = Conversation::new(&f).await;
    c.message(owner.telegram_id, "Synthetic owner", "/continue")
        .await;
    let d = draft_dialog(&f, owner.id).await;
    assert_eq!(d.step, "guardians");
    let before = c.screen(owner.telegram_id).await.1;
    c.message(
        owner.telegram_id,
        "Synthetic owner",
        "Unexpected text while choosing a person",
    )
    .await;
    let (method, guidance) = c.screen(owner.telegram_id).await;
    assert_eq!(method, "sendMessage");
    assert_ne!(guidance["message_id"], before["message_id"]);
    assert!(
        guidance["text"]
            .as_str()
            .unwrap()
            .contains(&tr("en", "draft-stage-guardians"))
    );
    assert!(
        !guidance["text"]
            .as_str()
            .unwrap()
            .contains("Unexpected text while choosing a person")
    );
    assert_eq!(draft_dialog(&f, owner.id).await.step, "guardians");
    assert!(
        f.engine
            .draft_blocks(owner.id, d.draft_id.unwrap())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(f.engine.own_plan(owner.id).await.unwrap().1.id, plan);
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn direct_document_waits_for_durable_upload_before_cleanup_and_next_question() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    f.engine.locale(owner.id, "en").await.unwrap();
    let mut c = Conversation::new(&f).await;
    c.message(owner.telegram_id, "Synthetic owner", "/continue")
        .await;
    for _ in 0..2 {
        c.press(
            &f,
            owner.telegram_id,
            &format!("select.{}", people[0].id.simple()),
        )
        .await;
        c.next(&f, owner.telegram_id, "done").await;
    }
    let d = draft_dialog(&f, owner.id).await;
    assert_eq!(d.step, "builder");
    let draft = d.draft_id.unwrap();
    let source = c.document(owner.telegram_id).await;
    assert_eq!(draft_dialog(&f, owner.id).await.step, "file-pending");
    assert!(
        !labels(&c.screen(owner.telegram_id).await.1).contains(&tr("en", "draft-next").as_str()),
        "pending upload cannot advance configuration"
    );
    assert!(
        f.engine
            .draft_blocks(owner.id, draft)
            .await
            .unwrap()
            .is_empty()
    );
    let mut tx = f.db.begin().await.unwrap();
    let jobs = list::<Job>(&mut *tx, Some(plan)).await.unwrap();
    assert_eq!(jobs.iter().filter(|job| matches!(job.task,
        Task::DownloadFile { draft_id, source_message, .. } if draft_id == draft && source_message == source)).count(), 1);
    assert!(!jobs.iter().any(|job| matches!(job.task,
        Task::CleanupMessage { chat_id, message_id, .. } if chat_id == owner.chat_id && message_id == source)),
        "the only available copy must survive until the encrypted upload is durable");
    tx.commit().await.unwrap();
    let pending_question = c.screen(owner.telegram_id).await.1;
    c.deliver(
        &f,
        plan,
        |task| matches!(task, Task::DownloadFile { draft_id, .. } if *draft_id == draft),
    )
    .await;
    assert_eq!(draft_dialog(&f, owner.id).await.step, "builder");
    let blocks = f.engine.draft_blocks(owner.id, draft).await.unwrap();
    let [
        Block::File {
            name,
            caption,
            file,
        },
    ] = &blocks[..]
    else {
        panic!("one directly submitted document must become one file block")
    };
    assert_eq!(name, "conversation.bin");
    assert_eq!(caption, "Synthetic attachment");
    let mut tx = f.db.begin().await.unwrap();
    let object: FileObject = get(&mut *tx, file.id).await.unwrap();
    assert_eq!(object.state, "draft");
    tx.commit().await.unwrap();
    let stored = f
        .blobs
        .get(&file.object_key, object.size as usize)
        .await
        .unwrap();
    let envelope: Envelope = serde_json::from_slice(&stored).unwrap();
    assert_eq!(
        &*f.engine
            .crypto
            .unwrap("draft-file", file.id, &envelope)
            .unwrap(),
        &[0_u8, 255, 17, 42]
    );
    let (method, next_question) = c.screen(owner.telegram_id).await;
    assert_eq!(method, "sendMessage");
    assert_ne!(next_question["message_id"], pending_question["message_id"]);
    assert!(labels(&next_question).contains(&tr("en", "draft-next").as_str()));
    let mut tx = f.db.begin().await.unwrap();
    let jobs = list::<Job>(&mut *tx, Some(plan)).await.unwrap();
    assert_eq!(jobs.iter().filter(|job| matches!(job.task,
        Task::CleanupMessage { chat_id, message_id, .. } if chat_id == owner.chat_id && message_id == source)).count(), 1,
        "durably attached input has exactly one durable cleanup task");
    tx.commit().await.unwrap();
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn question_context_suspends_capture_until_a_fresh_content_question_is_shown() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    f.engine.locale(owner.id, "en").await.unwrap();
    let mut c = Conversation::new(&f).await;
    c.message(owner.telegram_id, "Synthetic owner", "/continue")
        .await;
    for _ in 0..2 {
        c.press(
            &f,
            owner.telegram_id,
            &format!("select.{}", people[0].id.simple()),
        )
        .await;
        c.next(&f, owner.telegram_id, "done").await;
    }
    let draft = draft_dialog(&f, owner.id).await.draft_id.unwrap();
    c.message(
        owner.telegram_id,
        "Synthetic owner",
        "First intentional synthetic answer",
    )
    .await;
    assert_eq!(
        f.engine.draft_blocks(owner.id, draft).await.unwrap().len(),
        1
    );
    for (index, navigation) in [tr("en", "nav-home"), "/settings".into()]
        .iter()
        .enumerate()
    {
        c.message(owner.telegram_id, "Synthetic owner", navigation)
            .await;
        let suspended = draft_dialog(&f, owner.id).await;
        assert_eq!(suspended.step, "builder");
        assert_eq!(
            suspended.reply_to, None,
            "ordinary navigation suspends the content question"
        );
        let stray = c
            .message(
                owner.telegram_id,
                "Synthetic owner",
                "This is ordinary text outside the content question",
            )
            .await;
        assert_eq!(
            f.engine.draft_blocks(owner.id, draft).await.unwrap().len(),
            index + 1,
            "text sent from another screen must never silently become secret content"
        );
        let mut tx = f.db.begin().await.unwrap();
        let jobs = list::<Job>(&mut *tx, None).await.unwrap();
        assert!(!jobs.iter().any(|job| matches!(job.task,
            Task::CleanupMessage { chat_id, message_id, .. } if chat_id == owner.chat_id && message_id == stray)),
            "uncaptured ordinary input must not be deleted as a saved secret");
        tx.commit().await.unwrap();
        let (method, question) = c.screen(owner.telegram_id).await;
        assert_eq!(method, "sendMessage");
        assert!(
            question["text"]
                .as_str()
                .unwrap()
                .contains(&tr("en", "draft-stage-content"))
        );
        assert_eq!(
            draft_dialog(&f, owner.id).await.reply_to,
            question["message_id"].as_i64()
        );
        let accepted = c
            .message(
                owner.telegram_id,
                "Synthetic owner",
                "Intentional synthetic answer after the fresh question",
            )
            .await;
        assert_eq!(
            f.engine.draft_blocks(owner.id, draft).await.unwrap().len(),
            index + 2
        );
        let mut tx = f.db.begin().await.unwrap();
        let jobs = list::<Job>(&mut *tx, Some(plan)).await.unwrap();
        assert_eq!(jobs.iter().filter(|job| matches!(job.task,
            Task::CleanupMessage { chat_id, message_id, .. } if chat_id == owner.chat_id && message_id == accepted)).count(), 1,
            "a subsequent explicit answer is captured and durably cleaned up");
        tx.commit().await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn question_context_readiness_identifies_pending_secret_when_a_newer_one_is_ready() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    f.engine.locale(owner.id, "en").await.unwrap();
    let draft = f.engine.new_draft(owner.id, plan).await.unwrap();
    f.engine
        .append_block(
            owner.id,
            draft,
            Block::Text {
                text: "Synthetic pending content".into(),
            },
            (owner.chat_id, 99001),
        )
        .await
        .unwrap();
    f.engine
        .draft_policy(
            owner.id,
            draft,
            Policy {
                guardians: [people[0].id].into(),
                recipients: [people[0].id].into(),
                threshold: 1,
                timing: Timing::default(),
            },
        )
        .await
        .unwrap();
    let pending = f.engine.save(owner.id, draft).await.unwrap();
    let (ready, _) = secret(&f, &owner, plan, &people, false).await;
    f.engine
        .set_label(owner.id, plan, pending, "Pending synthetic packet")
        .await
        .unwrap();
    f.engine
        .set_label(owner.id, plan, ready, "Ready synthetic packet")
        .await
        .unwrap();
    let mut tx = f.db.begin().await.unwrap();
    let newer: Secret = get(&mut *tx, ready).await.unwrap();
    assert_eq!(newer.state, SecretState::Armed);
    let mut earlier: Secret = get(&mut *tx, pending).await.unwrap();
    assert_eq!(earlier.state, SecretState::Provisioning);
    // Avoid same-second UUID ordering obscuring the newer-ready/older-pending case.
    earlier.created_at = newer.created_at - 1;
    put(&mut *tx, Some(plan), &earlier).await.unwrap();
    tx.commit().await.unwrap();
    let projection = f.engine.overview(owner.id).await.unwrap().own.unwrap();
    assert_eq!(projection.secrets.last().unwrap().id, ready);
    let mut c = Conversation::new(&f).await;
    c.message(owner.telegram_id, "Synthetic owner", "/continue")
        .await;
    let (_, question) = c.screen(owner.telegram_id).await;
    let text = question["text"].as_str().unwrap();
    assert!(
        text.contains("Pending synthetic packet"),
        "the waiting question must identify the blocker"
    );
    assert!(
        !text.contains("Ready synthetic packet"),
        "the newest ready secret must not mask the pending one"
    );
    assert!(text.contains('○'));
    assert!(!labels(&question).contains(&tr("en", "setup-enable").as_str()));
    assert_eq!(
        f.engine.own_plan(owner.id).await.unwrap().1.state,
        PlanState::Setup
    );
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn question_context_readiness_does_not_use_an_older_ready_secret_to_complete_a_newer_one() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    f.engine.locale(owner.id, "en").await.unwrap();
    let (older, _) = secret(&f, &owner, plan, &people, false).await;
    let (latest, _) = secret(&f, &owner, plan, &people, false).await;
    f.engine
        .set_label(owner.id, plan, older, "Older ready packet")
        .await
        .unwrap();
    f.engine
        .set_label(owner.id, plan, latest, "Latest packet")
        .await
        .unwrap();
    f.engine
        .control(owner.id, plan, Id::new_v4(), Control::Rearm)
        .await
        .unwrap();
    let mut tx = f.db.begin().await.unwrap();
    let newer: Secret = get(&mut *tx, latest).await.unwrap();
    let mut earlier: Secret = get(&mut *tx, older).await.unwrap();
    earlier.created_at = newer.created_at - 1;
    put(&mut *tx, Some(plan), &earlier).await.unwrap();
    tx.commit().await.unwrap();
    let mut c = Conversation::new(&f).await;

    for (state, pending_control, completed) in [
        (SecretState::Paused, None, false),
        (SecretState::SetupFailed, None, false),
        (SecretState::Partial, None, false),
        (SecretState::Armed, Some(Id::new_v4()), false),
        (SecretState::Armed, None, true),
    ] {
        // Keep an older Armed secret throughout, reproducing a misleading aggregate
        // ready count while the newest secret has a different actual outcome.
        let mut tx = f.db.begin().await.unwrap();
        let mut value: Secret = get(&mut *tx, latest).await.unwrap();
        value.state = state;
        value.pending_control = pending_control;
        put(&mut *tx, Some(plan), &value).await.unwrap();
        let now = tx.now().await.unwrap();
        notice(&mut *tx, plan, owner.id, "secret-armed", now)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let projection = f.engine.overview(owner.id).await.unwrap().own.unwrap();
        assert_eq!(projection.state, PlanState::Active);
        assert!(projection.ready_secrets > 0);
        assert_eq!(projection.secrets.last().unwrap().id, latest);
        c.deliver(
            &f,
            plan,
            |task| matches!(task, Task::Notice { key, .. } if key == "secret-armed"),
        )
        .await;
        c.next(&f, owner.telegram_id, "setup-ready").await;
        let (_, screen) = c.screen(owner.telegram_id).await;
        let text = screen["text"].as_str().unwrap();
        assert_eq!(
            text.contains(&tr("en", "setup-active")),
            completed,
            "completion copy must reflect latest secret {state:?} with pending control {pending_control:?}"
        );
        assert!(!labels(&screen).contains(&tr("en", "setup-enable").as_str()));
        if completed {
            assert!(text.contains("Latest packet"));
        } else {
            assert!(
                text.starts_with(&tr("en", "secrets")),
                "show actual states instead of a completed setup question"
            );
            let expected = format!(
                "Latest packet · {}",
                adapters::localization::state("en", &state)
            );
            assert!(labels(&screen).contains(&expected.as_str()));
        }
    }
}
