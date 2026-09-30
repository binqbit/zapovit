use super::*;
use tokio::sync::Notify;

async fn cleanups(f: &Fixture, source: (i64, i64)) -> Vec<Job> {
    let mut tx = f.db.begin().await.unwrap();
    let jobs = list::<Job>(&mut *tx, None).await.unwrap().into_iter().filter(|job| {
        matches!(job.task, Task::CleanupMessage { chat_id, message_id, .. } if (chat_id,message_id)==source)
    }).collect();
    tx.commit().await.unwrap();
    jobs
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn source_cleanup_text_is_durable_before_save_and_not_recreated_after_success() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    let draft = f.engine.new_draft(owner.id, plan).await.unwrap();
    let source = (owner.chat_id, 901);
    let block = Block::Text {
        text: "Synthetic private original".into(),
    };
    f.engine
        .append_block(owner.id, draft, block.clone(), source)
        .await
        .unwrap();
    assert_eq!(
        f.engine.draft_blocks(owner.id, draft).await.unwrap().len(),
        1
    );
    let jobs = cleanups(&f, source).await;
    assert_eq!(
        jobs.len(),
        1,
        "durable text must immediately have durable cleanup without Save"
    );
    assert_eq!(
        jobs[0].priority, 0,
        "private source cleanup precedes ordinary notices and uploads"
    );
    let claimed = f
        .engine
        .claim_job(jobs[0].id)
        .await
        .unwrap()
        .expect("cleanup is due while the plan is still in setup");
    f.engine
        .finish_job(claimed.id, claimed.lease_token, SendResult::RetryAfter(1))
        .await
        .unwrap();
    assert_eq!(
        cleanups(&f, source).await[0].state,
        PartState::RetryableFailed
    );
    // Provider retry timing can be advanced deterministically without sleeping.
    let mut tx = f.db.begin().await.unwrap();
    let mut retry: Job = get(&mut *tx, claimed.id).await.unwrap();
    retry.due_at = tx.now().await.unwrap();
    put(&mut *tx, Some(plan), &retry).await.unwrap();
    tx.commit().await.unwrap();
    let retry = f.engine.claim_job(claimed.id).await.unwrap().unwrap();
    f.engine
        .finish_job(retry.id, retry.lease_token, SendResult::Sent(0))
        .await
        .unwrap();
    assert!(cleanups(&f, source).await.is_empty());
    f.engine
        .append_block(owner.id, draft, block, source)
        .await
        .unwrap();
    assert!(
        cleanups(&f, source).await.is_empty(),
        "a committed retry must not recreate completed cleanup"
    );
    // Preview messages still need the existing seal-time cleanup fallback.
    let preview = (owner.chat_id, 902);
    let mut tx = f.db.begin().await.unwrap();
    let mut value: Draft = get(&mut *tx, draft).await.unwrap();
    value.sources.push(preview);
    put(&mut *tx, Some(plan), &value).await.unwrap();
    tx.commit().await.unwrap();
    f.engine
        .draft_policy(
            owner.id,
            draft,
            Policy {
                guardians: [people[0].id].into(),
                recipients: [people[1].id].into(),
                threshold: 1,
                timing: Timing::default(),
            },
        )
        .await
        .unwrap();
    let saved = f.engine.save(owner.id, draft).await.unwrap();
    assert!(
        cleanups(&f, source).await.is_empty(),
        "seal must not recreate source cleanup after deletion succeeded"
    );
    assert_eq!(cleanups(&f, preview).await.len(), 1);
    assert_eq!(f.engine.save(owner.id, draft).await.unwrap(), saved);
    assert_eq!(cleanups(&f, preview).await.len(), 1);
}

struct UploadGate {
    inner: Arc<Blobs>,
    entered: Notify,
    resume: Notify,
    fail: bool,
}
#[async_trait]
impl BlobStore for UploadGate {
    async fn put(&self, key: &str, bytes: &[u8]) -> Result<()> {
        if self.fail {
            return Err(Error::Storage);
        }
        self.entered.notify_one();
        self.resume.notified().await;
        self.inner.put(key, bytes).await
    }
    async fn get(&self, key: &str, max: usize) -> Result<Vec<u8>> {
        self.inner.get(key, max).await
    }
    async fn delete(&self, key: &str) -> Result<()> {
        self.inner.delete(key).await
    }
    async fn exists(&self, key: &str) -> Result<bool> {
        self.inner.exists(key).await
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn source_cleanup_file_waits_for_completed_upload_and_durable_attachment() {
    let f = fixture().await;
    let (owner, plan, _) = participants(&f).await;
    let draft = f.engine.new_draft(owner.id, plan).await.unwrap();
    let source = (owner.chat_id, 911);
    let gate = Arc::new(UploadGate {
        inner: f.blobs.clone(),
        entered: Notify::new(),
        resume: Notify::new(),
        fail: false,
    });
    let mut engine = f.engine.clone();
    engine.blobs = gate.clone();
    let actor = owner.id;
    let upload = tokio::spawn(async move {
        engine
            .append_file(
                actor,
                draft,
                "private.txt".into(),
                "".into(),
                Zeroizing::new(b"synthetic file".to_vec()),
                source,
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified())
        .await
        .unwrap();
    assert!(
        cleanups(&f, source).await.is_empty(),
        "upload reservation is not durable file content"
    );
    assert!(
        f.engine
            .draft_blocks(owner.id, draft)
            .await
            .unwrap()
            .is_empty()
    );
    gate.resume.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), upload)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        f.engine.draft_blocks(owner.id, draft).await.unwrap().len(),
        1
    );
    assert_eq!(cleanups(&f, source).await.len(), 1);
    f.engine
        .append_file(
            owner.id,
            draft,
            "private.txt".into(),
            "".into(),
            Zeroizing::new(b"synthetic file".to_vec()),
            source,
        )
        .await
        .unwrap();
    assert_eq!(cleanups(&f, source).await.len(), 1);
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn source_cleanup_never_deletes_rejected_text_or_failed_upload() {
    let f = fixture().await;
    let (owner, plan, _) = participants(&f).await;
    let draft = f.engine.new_draft(owner.id, plan).await.unwrap();
    let text = (owner.chat_id, 921);
    assert!(
        f.engine
            .append_block(
                owner.id,
                draft,
                Block::Text {
                    text: "x".repeat(32769)
                },
                text
            )
            .await
            .is_err()
    );
    assert!(cleanups(&f, text).await.is_empty());
    let source = (owner.chat_id, 922);
    let mut engine = f.engine.clone();
    engine.blobs = Arc::new(UploadGate {
        inner: f.blobs.clone(),
        entered: Notify::new(),
        resume: Notify::new(),
        fail: true,
    });
    assert!(
        engine
            .append_file(
                owner.id,
                draft,
                "private.txt".into(),
                "".into(),
                Zeroizing::new(b"synthetic".to_vec()),
                source
            )
            .await
            .is_err()
    );
    assert!(cleanups(&f, source).await.is_empty());
    assert!(
        f.engine
            .draft_blocks(owner.id, draft)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn source_cleanup_cancel_preserves_pending_cleanup_without_repeating_completed_cleanup() {
    let f = fixture().await;
    let (owner, plan, _) = participants(&f).await;
    let draft = f.engine.new_draft(owner.id, plan).await.unwrap();
    let completed = (owner.chat_id, 941);
    let pending = (owner.chat_id, 942);
    for source in [completed, pending] {
        f.engine
            .append_block(
                owner.id,
                draft,
                Block::Text {
                    text: "Synthetic".into(),
                },
                source,
            )
            .await
            .unwrap();
    }
    let completed_job = cleanups(&f, completed)
        .await
        .into_iter()
        .next()
        .expect("cleanup scheduled at append");
    let claimed = f.engine.claim_job(completed_job.id).await.unwrap().unwrap();
    f.engine
        .finish_job(claimed.id, claimed.lease_token, SendResult::Sent(0))
        .await
        .unwrap();
    let pending_id = cleanups(&f, pending).await[0].id;
    let mut tx = f.db.begin().await.unwrap();
    let mut value: Draft = get(&mut *tx, draft).await.unwrap();
    let preview = (owner.chat_id, 943);
    value.sources.push(preview);
    put(&mut *tx, Some(plan), &value).await.unwrap();
    tx.commit().await.unwrap();
    f.engine
        .cancel_draft(owner.id, draft, value.revision)
        .await
        .unwrap();
    assert!(cleanups(&f, completed).await.is_empty());
    assert_eq!(
        cleanups(&f, pending)
            .await
            .iter()
            .map(|j| j.id)
            .collect::<Vec<_>>(),
        vec![pending_id]
    );
    assert_eq!(cleanups(&f, preview).await.len(), 1);
    assert!(
        f.engine.claim_job(pending_id).await.unwrap().is_some(),
        "cleanup must survive removal of its draft"
    );
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn source_cleanup_late_upload_after_cancel_keeps_unpersisted_source() {
    let f = fixture().await;
    let (owner, plan, _) = participants(&f).await;
    let draft = f.engine.new_draft(owner.id, plan).await.unwrap();
    let source = (owner.chat_id, 951);
    let gate = Arc::new(UploadGate {
        inner: f.blobs.clone(),
        entered: Notify::new(),
        resume: Notify::new(),
        fail: false,
    });
    let mut engine = f.engine.clone();
    engine.blobs = gate.clone();
    let actor = owner.id;
    let upload = tokio::spawn(async move {
        engine
            .append_file(
                actor,
                draft,
                "private.txt".into(),
                "".into(),
                Zeroizing::new(b"synthetic file".to_vec()),
                source,
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified())
        .await
        .unwrap();
    f.engine.cancel_draft(owner.id, draft, 0).await.unwrap();
    gate.resume.notify_one();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), upload)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert!(
        cleanups(&f, source).await.is_empty(),
        "a stored object without a committed draft attachment must not erase the user's input"
    );
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn source_cleanup_expiry_notifies_manual_deletion_exactly_once() {
    let f = fixture().await;
    let owner = f.engine.account(1001, 1001, "en").await.unwrap();
    let mut tx = f.db.begin().await.unwrap();
    let now = tx.now().await.unwrap();
    let id = enqueue(
        &mut *tx,
        None,
        Task::CleanupMessage {
            chat_id: owner.chat_id,
            message_id: 931,
            sent_at: now - DAY,
            account_id: owner.id,
        },
        now - 2,
        now - 1,
        0,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert!(f.engine.claim_job(id).await.unwrap().is_none());
    assert!(f.engine.claim_job(id).await.unwrap().is_none());
    let mut tx = f.db.begin().await.unwrap();
    let notices = list::<Job>(&mut *tx,None).await.unwrap().into_iter().filter(|j| {
        matches!(&j.task, Task::Notice { account_id,key,.. } if *account_id==owner.id && key=="manual-delete")
    }).count();
    assert_eq!(
        notices, 1,
        "expired cleanup must not silently abandon a private source message"
    );
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn source_cleanup_permanent_failure_notifies_once_and_ignores_stale_completion() {
    let f = fixture().await;
    let owner = f.engine.account(1001, 1001, "en").await.unwrap();
    let mut tx = f.db.begin().await.unwrap();
    let now = tx.now().await.unwrap();
    let job = enqueue(
        &mut *tx,
        None,
        Task::CleanupMessage {
            chat_id: owner.chat_id,
            message_id: 961,
            sent_at: now,
            account_id: owner.id,
        },
        now,
        now + DAY,
        0,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let claimed = f.engine.claim_job(job).await.unwrap().unwrap();
    f.engine
        .finish_job(job, Id::new_v4(), SendResult::Permanent)
        .await
        .unwrap();
    let mut tx = f.db.begin().await.unwrap();
    assert!(
        list::<Job>(&mut *tx, None)
            .await
            .unwrap()
            .iter()
            .all(|j| !matches!(&j.task, Task::Notice { key, .. } if key == "manual-delete"))
    );
    tx.commit().await.unwrap();
    for _ in 0..2 {
        f.engine
            .finish_job(job, claimed.lease_token, SendResult::Permanent)
            .await
            .unwrap();
    }
    let mut tx = f.db.begin().await.unwrap();
    assert!(tx.get(Kind::Job, job).await.unwrap().is_none());
    let notices = list::<Job>(&mut *tx, None).await.unwrap().into_iter().filter(|j| {
        matches!(&j.task, Task::Notice { account_id, key, .. } if *account_id == owner.id && key == "manual-delete")
    }).count();
    assert_eq!(notices, 1);
    // A forgotten account cannot receive a notice; its cleanup may still expire safely.
    let absent = enqueue(
        &mut *tx,
        None,
        Task::CleanupMessage {
            chat_id: 9999,
            message_id: 1,
            sent_at: now - DAY,
            account_id: Id::new_v4(),
        },
        now - 2,
        now - 1,
        0,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert!(f.engine.claim_job(absent).await.unwrap().is_none());
    let mut tx = f.db.begin().await.unwrap();
    assert_eq!(
        list::<Job>(&mut *tx, None)
            .await
            .unwrap()
            .into_iter()
            .filter(|j| matches!(&j.task, Task::Notice { key, .. } if key == "manual-delete"))
            .count(),
        1
    );
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn source_cleanup_legacy_draft_still_cleans_sources_at_save() {
    let f = fixture().await;
    let (owner, plan, people) = participants(&f).await;
    let draft = f.engine.new_draft(owner.id, plan).await.unwrap();
    let source = (owner.chat_id, 971);
    f.engine
        .append_block(
            owner.id,
            draft,
            Block::Text {
                text: "Legacy synthetic secret".into(),
            },
            source,
        )
        .await
        .unwrap();
    f.engine
        .draft_policy(
            owner.id,
            draft,
            Policy {
                guardians: [people[0].id].into(),
                recipients: [people[1].id].into(),
                threshold: 1,
                timing: Timing::default(),
            },
        )
        .await
        .unwrap();
    // Recreate the persisted shape from releases that queued cleanup only at Save.
    let mut tx = f.db.begin().await.unwrap();
    let mut raw = tx.get(Kind::Draft, draft).await.unwrap().unwrap();
    raw.as_object_mut().unwrap().remove("cleanup_scheduled");
    tx.put(Kind::Draft, draft, Some(plan), raw).await.unwrap();
    for job in list::<Job>(&mut *tx, Some(plan)).await.unwrap() {
        if matches!(job.task, Task::CleanupMessage { chat_id, message_id, .. } if (chat_id, message_id) == source)
        {
            tx.remove(Kind::Job, job.id).await.unwrap();
        }
    }
    tx.commit().await.unwrap();
    assert!(cleanups(&f, source).await.is_empty());
    f.engine.save(owner.id, draft).await.unwrap();
    assert_eq!(cleanups(&f, source).await.len(), 1);
}
