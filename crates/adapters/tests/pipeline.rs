use adapters::{
    crypto::{ArgonHasher, CryptoAdapter, Keyring, KeyringFile},
    journal::FileJournal,
    postgres::PgDatabase,
};
use application::*;
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use domain::{Block, DAY, Id, PartState, PlanState, Policy, SecretState, Timing};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::Mutex;
use zeroize::Zeroizing;

#[path = "pipeline_cases/transport_validation.rs"]
mod transport_validation;

#[path = "pipeline_cases/menu_navigation.rs"]
mod menu_navigation;

#[path = "pipeline_cases/draft_navigation.rs"]
mod draft_navigation;

#[path = "pipeline/validation_controls.rs"]
mod validation_controls;

#[path = "pipeline/validation_deletion.rs"]
mod validation_deletion;

#[derive(Default)]
struct Blobs(Mutex<BTreeMap<String, Vec<u8>>>);
#[async_trait]
impl BlobStore for Blobs {
    async fn put(&self, key: &str, bytes: &[u8]) -> Result<()> {
        self.0.lock().await.insert(key.into(), bytes.to_vec());
        Ok(())
    }
    async fn get(&self, key: &str, max: usize) -> Result<Vec<u8>> {
        let bytes = self
            .0
            .lock()
            .await
            .get(key)
            .cloned()
            .ok_or(Error::NotFound)?;
        if bytes.len() > max {
            return Err(Error::Storage);
        }
        Ok(bytes)
    }
    async fn delete(&self, key: &str) -> Result<()> {
        self.0.lock().await.remove(key);
        Ok(())
    }
    async fn exists(&self, key: &str) -> Result<bool> {
        Ok(self.0.lock().await.contains_key(key))
    }
}
fn crypto() -> Arc<dyn Crypto> {
    let key = |n| {
        Keyring::from_file_data(KeyringFile {
            active: "test".into(),
            keys: [("test".into(), URL_SAFE_NO_PAD.encode([n; 32]))].into(),
        })
        .unwrap()
    };
    Arc::new(CryptoAdapter::new(key(31), key(42)))
}
struct Fixture {
    engine: Engine,
    db: Arc<PgDatabase>,
    _dir: tempfile::TempDir,
    blobs: Arc<Blobs>,
}
async fn fixture() -> Fixture {
    let url =
        std::env::var("TEST_DATABASE_URL").expect("explicit local TEST_DATABASE_URL is required");
    let mut parsed = reqwest::Url::parse(&url).unwrap();
    assert!(
        parsed.path().ends_with("_test"),
        "integration database name must end in _test"
    );
    let schema = format!("test_{}", Id::new_v4().simple());
    let admin = PgDatabase::connect(&url, 2).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin.pool)
        .await
        .unwrap();
    parsed
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let db = Arc::new(PgDatabase::connect(parsed.as_str(), 10).await.unwrap());
    db.migrate().await.unwrap();
    db.bind_bot(100).await.unwrap();
    db.ingest(100, &[]).await.unwrap();
    db.scheduler_heartbeat().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    FileJournal::initialize(dir.path()).unwrap();
    let crypto = crypto();
    let blobs = Arc::new(Blobs::default());
    let engine = Engine {
        db: db.clone(),
        crypto: crypto.clone(),
        blobs: blobs.clone(),
        recovery: Arc::new(ArgonHasher::new(65536, 3, 1, 2).unwrap()),
        journal: Arc::new(FileJournal::open(dir.path().into(), crypto).unwrap()),
    };
    Fixture {
        engine,
        db,
        _dir: dir,
        blobs,
    }
}
async fn participants(f: &Fixture) -> (Account, Id, Vec<Account>) {
    let owner = f.engine.account(1001, 1001, "uk").await.unwrap();
    let plan = f.engine.create_profile(owner.id).await.unwrap();
    let (profile, _) = f.engine.own_plan(owner.id).await.unwrap();
    f.engine
        .acknowledge_recovery(owner.id, plan, profile.recovery_selector)
        .await
        .unwrap();
    let mut people = Vec::new();
    for id in 2001..=2005 {
        let a = f.engine.account(id, id, "en").await.unwrap();
        let invite = f.engine.invite(owner.id, plan).await.unwrap();
        f.engine.accept_invite(a.id, invite).await.unwrap();
        let mut tx = f.engine.db.begin().await.unwrap();
        let p = list::<Participant>(&mut *tx, Some(plan))
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.account_id == a.id)
            .unwrap();
        tx.commit().await.unwrap();
        f.engine
            .confirm_participant(owner.id, plan, p.id)
            .await
            .unwrap();
        people.push(a);
    }
    (owner, plan, people)
}
async fn secret(
    f: &Fixture,
    owner: &Account,
    plan: Id,
    people: &[Account],
    file: bool,
) -> (Id, Vec<(Account, String)>) {
    let draft = f.engine.new_draft(owner.id, plan).await.unwrap();
    f.engine
        .append_block(
            owner.id,
            draft,
            Block::Copyable {
                text: "synthetic 🔐 password\n<>&_*".into(),
            },
            (owner.chat_id, 12),
        )
        .await
        .unwrap();
    if file {
        f.engine
            .append_file(
                owner.id,
                draft,
                "test.bin".into(),
                "Test file".into(),
                Zeroizing::new(vec![0, 255, 0, 17]),
                (owner.chat_id, 13),
            )
            .await
            .unwrap();
    }
    let policy = Policy {
        guardians: people.iter().map(|a| a.id).collect(),
        recipients: [people[0].id].into(),
        threshold: 3,
        timing: Timing {
            reminder_seconds: DAY,
            inactivity_seconds: 2 * DAY,
            release_delay_seconds: DAY,
        },
    };
    f.engine
        .draft_policy(owner.id, draft, policy)
        .await
        .unwrap();
    let secret = f.engine.save(owner.id, draft).await.unwrap();
    assert!(f.engine.draft_blocks(owner.id, draft).await.is_err());
    assert_eq!(secret, f.engine.save(owner.id, draft).await.unwrap());
    let mut tx = f.engine.db.begin().await.unwrap();
    let grants = list::<GuardianGrant>(&mut *tx, Some(secret)).await.unwrap();
    tx.commit().await.unwrap();
    let mut codes = Vec::new();
    for grant in grants {
        let bytes = f
            .engine
            .crypto
            .unwrap("grant", grant.id, grant.delivery.as_ref().unwrap())
            .unwrap();
        codes.push((
            people
                .iter()
                .find(|a| a.id == grant.account_id)
                .unwrap()
                .clone(),
            String::from_utf8(bytes.to_vec()).unwrap(),
        ));
        f.engine
            .acknowledge_grant(grant.account_id, grant.id)
            .await
            .unwrap();
    }
    (secret, codes)
}
async fn inactive(f: &Fixture, owner: Id, plan: Id, secret: Id) -> Id {
    f.engine
        .control(owner, plan, Id::new_v4(), Control::Rearm)
        .await
        .unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    let now = tx.now().await.unwrap();
    let mut p: Plan = get(&mut *tx, plan).await.unwrap();
    p.last_activity = now - 30 * DAY;
    put(&mut *tx, Some(p.profile_id), &p).await.unwrap();
    let mut s: Secret = get(&mut *tx, secret).await.unwrap();
    s.due_at = now - 1;
    put(&mut *tx, Some(plan), &s).await.unwrap();
    tx.commit().await.unwrap();
    f.engine.tick_plan(plan).await.unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    get::<Secret>(&mut *tx, secret)
        .await
        .unwrap()
        .last_case
        .unwrap()
}
async fn wait_elapsed(f: &Fixture, case: Id) {
    let mut tx = f.engine.db.begin().await.unwrap();
    let mut c: CaseRecord = get(&mut *tx, case).await.unwrap();
    let now = tx.now().await.unwrap();
    c.case.release_at = Some(time::OffsetDateTime::from_unix_timestamp(now - 1).unwrap());
    put(&mut *tx, Some(c.secret_id), &c).await.unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn encrypted_file_quorum_grace_delivery_and_no_resend() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    let (secret, codes) = secret(&f, &owner, plan, &people, true).await;
    for bytes in f.blobs.0.lock().await.values() {
        assert!(!bytes.windows(4).any(|b| b == [0, 255, 0, 17]));
    }
    let case = inactive(&f, owner.id, plan, secret).await;
    assert!(
        f.engine
            .submit_code(owner.id, case, &codes[0].1)
            .await
            .is_err()
    );
    for (a, code) in codes.iter().take(2) {
        f.engine.submit_code(a.id, case, code).await.unwrap();
    }
    f.engine.tick_plan(plan).await.unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    assert!(
        list::<DeliveryPart>(&mut *tx, Some(secret))
            .await
            .unwrap()
            .is_empty()
    );
    tx.commit().await.unwrap();
    f.engine
        .submit_code(codes[2].0.id, case, &codes[2].1)
        .await
        .unwrap();
    f.engine.tick_plan(plan).await.unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    assert!(
        list::<DeliveryPart>(&mut *tx, Some(secret))
            .await
            .unwrap()
            .is_empty()
    );
    tx.commit().await.unwrap();
    wait_elapsed(&f, case).await;
    f.engine.tick_plan(plan).await.unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    let mut jobs = list::<Job>(&mut *tx, Some(plan))
        .await
        .unwrap()
        .into_iter()
        .filter(|j| matches!(j.task, Task::Deliver { .. }))
        .collect::<Vec<_>>();
    let parts = list::<DeliveryPart>(&mut *tx, Some(secret)).await.unwrap();
    jobs.sort_by_key(|job| match job.task {
        Task::Deliver { part_id, .. } => parts.iter().find(|p| p.id == part_id).unwrap().index,
        _ => usize::MAX,
    });
    tx.commit().await.unwrap();
    assert_eq!(jobs.len(), 2);
    for (n, job) in jobs.into_iter().enumerate() {
        let job = f.engine.claim_job(job.id).await.unwrap().unwrap();
        let (a, b, file) = f.engine.delivery_block(&job).await.unwrap();
        assert_eq!(a.id, people[0].id);
        if let Block::File { .. } = b {
            assert_eq!(*file.unwrap(), vec![0, 255, 0, 17]);
        }
        sqlx::query("DELETE FROM rate_limits WHERE key LIKE 'outbound:%'")
            .execute(&f.db.pool)
            .await
            .unwrap();
        assert!(f.engine.authorize_dispatch(&job, a.chat_id).await.unwrap());
        f.engine
            .finish_job(job.id, job.lease_token, SendResult::Sent(300 + n as i64))
            .await
            .unwrap();
        assert!(f.engine.claim_job(job.id).await.unwrap().is_none());
    }
    let mut tx = f.engine.db.begin().await.unwrap();
    assert_eq!(
        get::<Secret>(&mut *tx, secret).await.unwrap().state,
        SecretState::Delivered
    );
    assert!(
        list::<Submission>(&mut *tx, Some(case))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn stop_fences_claimed_job_and_journal_survives_stale_database() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    let (secret, codes) = secret(&f, &owner, plan, &people, false).await;
    let case = inactive(&f, owner.id, plan, secret).await;
    for (a, code) in codes.iter().take(3) {
        f.engine.submit_code(a.id, case, code).await.unwrap();
    }
    wait_elapsed(&f, case).await;
    f.engine.tick_plan(plan).await.unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    let stale: Plan = get(&mut *tx, plan).await.unwrap();
    let job = list::<Job>(&mut *tx, Some(plan))
        .await
        .unwrap()
        .into_iter()
        .find(|j| matches!(j.task, Task::Deliver { .. }))
        .unwrap();
    tx.commit().await.unwrap();
    let job = f.engine.claim_job(job.id).await.unwrap().unwrap();
    let operation = Id::new_v4();
    f.engine
        .control(owner.id, plan, operation, Control::Stop)
        .await
        .unwrap();
    assert!(
        !f.engine
            .authorize_dispatch(&job, people[0].chat_id)
            .await
            .unwrap()
    );
    let mut tx = f.engine.db.begin().await.unwrap();
    put(&mut *tx, Some(stale.profile_id), &stale).await.unwrap();
    tx.remove(Kind::ControlIntent, operation).await.unwrap();
    tx.commit().await.unwrap();
    f.engine.replay_controls().await.unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    assert_eq!(
        get::<Plan>(&mut *tx, plan).await.unwrap().state,
        PlanState::Paused
    );
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn uncertain_send_is_terminal_until_explicit_review() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    let (secret, codes) = secret(&f, &owner, plan, &people, false).await;
    let case = inactive(&f, owner.id, plan, secret).await;
    for (a, c) in codes.iter().take(3) {
        f.engine.submit_code(a.id, case, c).await.unwrap();
    }
    wait_elapsed(&f, case).await;
    f.engine.tick_plan(plan).await.unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    let j = list::<Job>(&mut *tx, Some(plan))
        .await
        .unwrap()
        .into_iter()
        .find(|j| matches!(j.task, Task::Deliver { .. }))
        .unwrap();
    tx.commit().await.unwrap();
    let j = f.engine.claim_job(j.id).await.unwrap().unwrap();
    assert!(
        f.engine
            .authorize_dispatch(&j, people[0].chat_id)
            .await
            .unwrap()
    );
    f.engine
        .finish_job(j.id, j.lease_token, SendResult::Unknown)
        .await
        .unwrap();
    assert!(f.engine.claim_job(j.id).await.unwrap().is_none());
    let mut tx = f.engine.db.begin().await.unwrap();
    assert_eq!(
        list::<DeliveryPart>(&mut *tx, Some(secret)).await.unwrap()[0].state,
        PartState::Unknown
    );
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn recovery_transfer_requires_new_key_ack_and_invalidates_old_owner() {
    let f = fixture().await;
    let owner = f.engine.account(1010, 1010, "en").await.unwrap();
    let plan = f.engine.create_profile(owner.id).await.unwrap();
    let (profile, _) = f.engine.own_plan(owner.id).await.unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    let job = list::<Job>(&mut *tx, Some(plan))
        .await
        .unwrap()
        .into_iter()
        .find(|j| matches!(j.task, Task::RecoveryCode { .. }))
        .unwrap();
    tx.commit().await.unwrap();
    let Task::RecoveryCode {
        envelope, selector, ..
    } = job.task
    else {
        panic!()
    };
    let token = f
        .engine
        .crypto
        .unwrap("recovery", selector, &envelope)
        .unwrap();
    let token = String::from_utf8(token.to_vec()).unwrap();
    let next = f.engine.account(3030, 3030, "uk").await.unwrap();
    let claim = f
        .engine
        .recover(next.id, selector, &token, false, Id::new_v4())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        f.engine.own_plan(owner.id).await.unwrap().0.owner_id,
        owner.id
    );
    assert!(
        f.engine
            .control(owner.id, plan, Id::new_v4(), Control::Rearm)
            .await
            .is_err()
    );
    f.engine
        .acknowledge_claim(next.id, claim, Id::new_v4())
        .await
        .unwrap();
    let (new_profile, new_plan) = f.engine.own_plan(next.id).await.unwrap();
    assert_eq!(new_profile.owner_epoch, profile.owner_epoch + 1);
    assert_eq!(new_plan.state, PlanState::Paused);
    assert!(
        f.engine
            .control(owner.id, plan, Id::new_v4(), Control::Rearm)
            .await
            .is_err()
    );
    assert!(
        f.engine
            .recover(owner.id, selector, &token, true, Id::new_v4())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn journal_authentication_and_truncation_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    FileJournal::initialize(dir.path()).unwrap();
    let crypto = crypto();
    let journal = FileJournal::open(dir.path().into(), crypto.clone()).unwrap();
    let intent = ControlIntent {
        id: Id::new_v4(),
        profile_id: Id::new_v4(),
        plan_id: Id::new_v4(),
        epoch: 3,
        owner_epoch: 1,
        at: 10,
        previous_state: PlanState::Active,
        operation: Control::Stop,
        applied: false,
        secret_epoch: None,
    };
    assert_eq!(journal.append(&intent).await.unwrap(), 1);
    assert_eq!(journal.append(&intent).await.unwrap(), 1);
    drop(journal);
    std::fs::write(dir.path().join("control.jsonl"), b"").unwrap();
    assert!(FileJournal::open(dir.path().into(), crypto).is_err());
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn unanimous_secret_cancellation_preserves_sibling_and_policy_is_immutable() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    let (first, _) = secret(&f, &owner, plan, &people, false).await;
    let (second, _) = secret(&f, &owner, plan, &people, false).await;
    f.engine
        .control(owner.id, plan, Id::new_v4(), Control::Rearm)
        .await
        .unwrap();
    let before = f.engine.own_plan(owner.id).await.unwrap().1.epoch;
    let request = f
        .engine
        .request_cancellation(people[0].id, plan, Some(first))
        .await
        .unwrap();
    for person in people.iter().take(4) {
        assert!(!f.engine.vote_cancel(person.id, request).await.unwrap());
    }
    let mut tx = f.engine.db.begin().await.unwrap();
    assert_eq!(
        get::<Secret>(&mut *tx, first).await.unwrap().state,
        SecretState::Armed
    );
    tx.commit().await.unwrap();
    assert!(f.engine.vote_cancel(people[4].id, request).await.unwrap());
    let mut tx = f.engine.db.begin().await.unwrap();
    assert_eq!(get::<Plan>(&mut *tx, plan).await.unwrap().epoch, before);
    assert_eq!(
        get::<Secret>(&mut *tx, first).await.unwrap().state,
        SecretState::Paused
    );
    assert_eq!(
        get::<Secret>(&mut *tx, second).await.unwrap().state,
        SecretState::Armed
    );
    tx.commit().await.unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    let mut s: Secret = get(&mut *tx, second).await.unwrap();
    s.policy.threshold = 1;
    assert!(put(&mut *tx, Some(plan), &s).await.is_err());
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn durable_inbox_and_stop_retries_are_idempotent() {
    let f = fixture().await;
    let (owner, plan, _) = participants(&f).await;
    let id = Id::new_v4();
    f.engine
        .control(owner.id, plan, id, Control::Stop)
        .await
        .unwrap();
    let epoch = f.engine.own_plan(owner.id).await.unwrap().1.epoch;
    f.engine
        .control(owner.id, plan, id, Control::Stop)
        .await
        .unwrap();
    assert_eq!(f.engine.own_plan(owner.id).await.unwrap().1.epoch, epoch);
    let envelope = f
        .engine
        .crypto
        .wrap("test", Id::nil(), b"synthetic event")
        .unwrap();
    f.db.ingest(100, &[(100, envelope.clone())]).await.unwrap();
    assert_eq!(f.db.cursor().await.unwrap(), 101);
    assert_eq!(f.db.pending_updates(100).await.unwrap().len(), 1);
    f.db.ingest(100, &[(100, envelope)]).await.unwrap();
    assert_eq!(f.db.pending_updates(100).await.unwrap().len(), 1);
    let mut tx = f.engine.db.begin().await.unwrap();
    assert!(!tx.operational_ready().await.unwrap());
    tx.commit().await.unwrap();
    f.db.complete_update(100, 100).await.unwrap();
    assert!(f.db.pending_updates(100).await.unwrap().is_empty());
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn custom_reminders_and_secret_deletion_cancel_pending_delivery() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    let (secret, codes) = secret(&f, &owner, plan, &people, false).await;
    let case = inactive(&f, owner.id, plan, secret).await;
    for (guardian, code) in codes.iter().take(3) {
        f.engine.submit_code(guardian.id, case, code).await.unwrap();
    }
    wait_elapsed(&f, case).await;
    f.engine.tick_plan(plan).await.unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    let jobs: Vec<_> = list::<Job>(&mut *tx, Some(plan))
        .await
        .unwrap()
        .into_iter()
        .filter(|j| matches!(j.task, Task::Deliver { .. }))
        .collect();
    assert!(!jobs.is_empty());
    tx.commit().await.unwrap();
    f.engine
        .control(
            owner.id,
            plan,
            Id::new_v4(),
            Control::DeleteSecret { secret_id: secret },
        )
        .await
        .unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    assert!(tx.get(Kind::Secret, secret).await.unwrap().is_none());
    assert!(
        get::<DeletionTombstone>(&mut *tx, secret)
            .await
            .unwrap()
            .scope
            == DeletedScope::Secret
    );
    assert!(
        list::<GuardianGrant>(&mut *tx, Some(secret))
            .await
            .unwrap()
            .is_empty()
    );
    for job in jobs {
        assert!(tx.get(Kind::Job, job.id).await.unwrap().is_none());
    }
    tx.commit().await.unwrap();
    let (second, _) = self::secret(&f, &owner, plan, &people, false).await;
    f.engine
        .control(owner.id, plan, Id::new_v4(), Control::CheckIn)
        .await
        .unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    let p: Plan = get(&mut *tx, plan).await.unwrap();
    assert_eq!(p.next_reminder - p.last_activity, DAY);
    assert_eq!(
        get::<Secret>(&mut *tx, second).await.unwrap().due_at - p.last_activity,
        2 * DAY
    );
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn delete_profile_purges_provisioning_and_recovery_credentials() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    let (secret, _) = secret(&f, &owner, plan, &people, true).await;
    let (profile, _) = f.engine.own_plan(owner.id).await.unwrap();
    f.engine
        .control(owner.id, plan, Id::new_v4(), Control::DeleteProfile)
        .await
        .unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    assert!(tx.get(Kind::Profile, profile.id).await.unwrap().is_none());
    assert!(tx.get(Kind::Plan, plan).await.unwrap().is_none());
    assert!(
        get::<DeletionTombstone>(&mut *tx, profile.id)
            .await
            .unwrap()
            .scope
            == DeletedScope::Profile
    );
    assert!(
        list::<Claim>(&mut *tx, Some(profile.id))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        list::<Participant>(&mut *tx, Some(plan))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        list::<GuardianGrant>(&mut *tx, Some(secret))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(tx.get(Kind::Secret, secret).await.unwrap().is_none());
    assert!(
        list::<ControlIntent>(&mut *tx, Some(plan))
            .await
            .unwrap()
            .is_empty()
    );
    for job in list::<Job>(&mut *tx, Some(plan)).await.unwrap() {
        assert!(matches!(
            job.task,
            Task::CleanupMessage { .. } | Task::DeleteObject { .. }
        ));
    }
    assert!(
        list::<FileObject>(&mut *tx, Some(plan))
            .await
            .unwrap()
            .iter()
            .all(|o| o.state == "gc")
    );
    tx.commit().await.unwrap();
    f.engine.replay_controls().await.unwrap();
}

#[path = "pipeline_cases/engine_validation.rs"]
mod engine_validation;

#[path = "pipeline_cases/late_delivery.rs"]
mod late_delivery;
