use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

struct PausedPut {
    inner: Arc<Blobs>,
    entered: Notify,
    resume: Notify,
}
#[async_trait]
impl BlobStore for PausedPut {
    async fn put(&self, key: &str, bytes: &[u8]) -> Result<()> {
        self.inner.put(key, bytes).await?;
        self.entered.notify_one();
        self.resume.notified().await;
        Ok(())
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
async fn repeated_file_source_does_not_leave_an_unreferenced_attached_object() {
    let f = fixture().await;
    let (owner, plan, _) = participants(&f).await;
    let draft = f.engine.new_draft(owner.id, plan).await.unwrap();
    let gate = Arc::new(PausedPut {
        inner: f.blobs.clone(),
        entered: Notify::new(),
        resume: Notify::new(),
    });
    let mut slow = f.engine.clone();
    slow.blobs = gate.clone();
    let actor = owner.id;
    let source = (owner.chat_id, 300);
    let upload = tokio::spawn(async move {
        slow.append_file(
            actor,
            draft,
            "a.bin".into(),
            "".into(),
            Zeroizing::new(vec![1, 2]),
            source,
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), gate.entered.notified())
        .await
        .expect("upload reached the controlled boundary");
    f.engine
        .append_file(
            actor,
            draft,
            "a.bin".into(),
            "".into(),
            Zeroizing::new(vec![1, 2]),
            source,
        )
        .await
        .unwrap();
    gate.resume.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(10), upload)
        .await
        .expect("upload completed after release")
        .unwrap()
        .unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    let objects = list::<FileObject>(&mut *tx, Some(plan)).await.unwrap();
    assert_eq!(
        objects.iter().filter(|o| o.state == "draft").count(),
        1,
        "only the referenced object may be attached"
    );
    assert_eq!(
        objects.iter().filter(|o| o.state == "gc").count(),
        1,
        "duplicate PUT needs durable cleanup"
    );
    assert_eq!(f.engine.draft_blocks(actor, draft).await.unwrap().len(), 1);
}

// Pause immediately after a draft transaction commits, before the caller can perform
// subsequent bookkeeping. This deterministically interleaves deletion at that boundary.
struct CommitGate {
    armed: AtomicBool,
    entered: Notify,
    resume: Notify,
}
struct GatedDb {
    inner: Arc<dyn Database>,
    gate: Arc<CommitGate>,
}
struct GatedTx {
    inner: Box<dyn Transaction>,
    gate: Arc<CommitGate>,
    wrote_draft: bool,
}
#[async_trait]
impl Database for GatedDb {
    async fn begin(&self) -> Result<Box<dyn Transaction>> {
        Ok(Box::new(GatedTx {
            inner: self.inner.begin().await?,
            gate: self.gate.clone(),
            wrote_draft: false,
        }))
    }
}
#[async_trait]
impl Transaction for GatedTx {
    async fn now(&mut self) -> Result<i64> {
        self.inner.now().await
    }
    async fn operational_ready(&mut self) -> Result<bool> {
        self.inner.operational_ready().await
    }
    async fn operational_status(&mut self, plan: Option<Id>) -> Result<OperationalStatus> {
        self.inner.operational_status(plan).await
    }
    async fn reserve_resource(
        &mut self,
        resource: &str,
        id: Id,
        amount: i64,
        capacity: i64,
    ) -> Result<bool> {
        self.inner
            .reserve_resource(resource, id, amount, capacity)
            .await
    }
    async fn release_resource(&mut self, resource: &str, id: Id) -> Result<()> {
        self.inner.release_resource(resource, id).await
    }
    async fn writes_ready(&mut self) -> Result<bool> {
        self.inner.writes_ready().await
    }
    async fn lock(&mut self, k: Kind, id: Id) -> Result<()> {
        self.inner.lock(k, id).await
    }
    async fn get(&mut self, k: Kind, id: Id) -> Result<Option<serde_json::Value>> {
        self.inner.get(k, id).await
    }
    async fn list(&mut self, k: Kind, scope: Option<Id>) -> Result<Vec<serde_json::Value>> {
        self.inner.list(k, scope).await
    }
    async fn find(&mut self, k: Kind, field: &str, v: &str) -> Result<Vec<serde_json::Value>> {
        self.inner.find(k, field, v).await
    }
    async fn due(&mut self, k: Kind, now: i64, limit: i64) -> Result<Vec<serde_json::Value>> {
        self.inner.due(k, now, limit).await
    }
    async fn put(
        &mut self,
        k: Kind,
        id: Id,
        scope: Option<Id>,
        v: serde_json::Value,
    ) -> Result<()> {
        self.wrote_draft |= k == Kind::Draft;
        self.inner.put(k, id, scope, v).await
    }
    async fn remove(&mut self, k: Kind, id: Id) -> Result<()> {
        self.inner.remove(k, id).await
    }
    async fn rate_limit(&mut self, key: &str, cap: i64, period: i64) -> Result<bool> {
        self.inner.rate_limit(key, cap, period).await
    }
    async fn commit(self: Box<Self>) -> Result<()> {
        let Self {
            inner,
            gate,
            wrote_draft,
        } = *self;
        inner.commit().await?;
        if wrote_draft && gate.armed.swap(false, Ordering::SeqCst) {
            gate.entered.notify_one();
            gate.resume.notified().await;
        }
        Ok(())
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn deletion_between_file_attachment_and_return_cannot_resurrect_object() {
    let f = fixture().await;
    let (owner, plan, _) = participants(&f).await;
    let draft = f.engine.new_draft(owner.id, plan).await.unwrap();
    let gate = Arc::new(CommitGate {
        armed: AtomicBool::new(true),
        entered: Notify::new(),
        resume: Notify::new(),
    });
    let mut slow = f.engine.clone();
    slow.db = Arc::new(GatedDb {
        inner: f.engine.db.clone(),
        gate: gate.clone(),
    });
    let actor = owner.id;
    let upload = tokio::spawn(async move {
        slow.append_file(
            actor,
            draft,
            "a.bin".into(),
            "".into(),
            Zeroizing::new(vec![1, 2]),
            (1001, 301),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), gate.entered.notified())
        .await
        .expect("upload reached the controlled boundary");
    f.engine
        .control(actor, plan, Id::new_v4(), Control::DeleteProfile)
        .await
        .unwrap();
    gate.resume.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(10), upload)
        .await
        .expect("upload completed after release")
        .unwrap()
        .unwrap();
    let mut tx = f.engine.db.begin().await.unwrap();
    let objects = list::<FileObject>(&mut *tx, Some(plan)).await.unwrap();
    assert_eq!(
        objects.len(),
        1,
        "cleanup ledger must retain the uploaded object"
    );
    assert!(
        objects.iter().all(|o| o.state == "gc"),
        "deletion must retain ownership of cleanup"
    );
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn unrelated_account_locks_do_not_share_a_global_lock() {
    let f = fixture().await;
    let mut first = f.engine.db.begin().await.unwrap();
    first
        .lock(Kind::Account, Id::from_u128(5001))
        .await
        .unwrap();
    let mut second = f.engine.db.begin().await.unwrap();
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        second.lock(Kind::Account, Id::from_u128(5002)),
    )
    .await;
    assert!(
        outcome.is_ok(),
        "independent Telegram IDs must not share one advisory lock"
    );
    outcome.unwrap().unwrap();
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn startup_rejects_rewritten_core_checksum_and_failed_migrations() {
    let f = fixture().await;
    f.db.schema_current().await.unwrap();
    let checksum: Vec<u8> =
        sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version=1")
            .fetch_one(&f.db.pool)
            .await
            .unwrap();
    sqlx::query("UPDATE _sqlx_migrations SET checksum=decode('00','hex') WHERE version=1")
        .execute(&f.db.pool)
        .await
        .unwrap();
    assert!(matches!(f.db.schema_current().await, Err(Error::Config)));
    sqlx::query("UPDATE _sqlx_migrations SET checksum=$1,success=false WHERE version=1")
        .bind(checksum)
        .execute(&f.db.pool)
        .await
        .unwrap();
    assert!(matches!(f.db.schema_current().await, Err(Error::Config)));
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn managed_files_cannot_be_injected_as_arbitrary_object_references() {
    let f = fixture().await;
    let (owner, plan, _) = participants(&f).await;
    let draft = f.engine.new_draft(owner.id, plan).await.unwrap();
    let result = f
        .engine
        .append_block(
            owner.id,
            draft,
            Block::File {
                file: domain::FileRef {
                    id: Id::new_v4(),
                    object_key: "draft/another-plan".into(),
                    encrypted_size: 42,
                    sha256: "untrusted".into(),
                },
                name: "file.bin".into(),
                caption: "".into(),
            },
            (owner.chat_id, 302),
        )
        .await;
    assert!(matches!(
        result,
        Err(Error::Rule(domain::RuleError::AccessDenied))
    ));
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
async fn another_owner_cannot_read_modify_seal_or_stop_a_foreign_plan() {
    let f = fixture().await;
    let (owner, plan, _) = participants(&f).await;
    let outsider = f.engine.account(8801, 8801, "en").await.unwrap();
    let draft = f.engine.new_draft(owner.id, plan).await.unwrap();
    f.engine
        .append_block(
            owner.id,
            draft,
            Block::Text {
                text: "synthetic private text".into(),
            },
            (owner.chat_id, 400),
        )
        .await
        .unwrap();
    assert!(f.engine.draft_blocks(outsider.id, draft).await.is_err());
    assert!(
        f.engine
            .append_block(
                outsider.id,
                draft,
                Block::Text {
                    text: "replacement".into()
                },
                (outsider.chat_id, 401)
            )
            .await
            .is_err()
    );
    assert!(f.engine.save(outsider.id, draft).await.is_err());
    assert!(
        f.engine
            .control(outsider.id, plan, Id::new_v4(), Control::Stop)
            .await
            .is_err()
    );
    assert!(f.engine.new_draft(outsider.id, plan).await.is_err());
    let blocks = f.engine.draft_blocks(owner.id, draft).await.unwrap();
    assert!(matches!(&blocks[0], Block::Text { text } if text == "synthetic private text"));
    assert!(f.engine.account(8801, 1001, "en").await.is_err());
}
