use crate::*;
use domain::{
    Block, DAY, FileRef, Id, PartState, PlanState, Policy, RuleError, SecretState, Timing,
    validate_blocks,
};
use std::{collections::BTreeSet, sync::Arc};
use zeroize::Zeroizing;

/// Backup waits ten seconds longer than the entire managed object I/O phase.
/// A timed-out PUT remains pending and reserved until conservative GC succeeds.
pub const MAX_BLOB_OPERATION_SECS: u64 = 110;

async fn blob_io<T>(
    deadline: tokio::time::Instant,
    operation: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    if tokio::time::Instant::now() >= deadline {
        return Err(Error::Storage);
    }
    tokio::time::timeout_at(deadline, operation)
        .await
        .map_err(|_| Error::Storage)?
}

#[cfg(test)]
mod blob_deadline_tests {
    use super::*;

    #[tokio::test]
    async fn all_blob_requests_share_one_deadline_and_expiry_does_not_start_io() {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(20);
        assert_eq!(blob_io(deadline, async { Ok(7) }).await.unwrap(), 7);
        let result = blob_io(deadline, std::future::pending::<Result<()>>()).await;
        assert!(matches!(result, Err(Error::Storage)));
        let mut started = false;
        let result = blob_io(deadline, async {
            started = true;
            Ok(())
        })
        .await;
        assert!(matches!(result, Err(Error::Storage)));
        assert!(
            !started,
            "a later PUT must never start with a fresh timeout"
        );
    }
}

/// Commands depend on ports; transport and database details stay in adapters.
#[derive(Clone)]
pub struct Engine {
    pub db: Arc<dyn Database>,
    pub crypto: Arc<dyn Crypto>,
    pub blobs: Arc<dyn BlobStore>,
    pub recovery: Arc<dyn RecoveryHasher>,
    pub journal: Arc<dyn ControlJournal>,
}

pub fn content_aad(secret: Id, policy: &Policy, file: Option<Id>) -> Vec<u8> {
    let mut bytes = b"zapovit/content/v1\0".to_vec();
    bytes.extend_from_slice(secret.as_bytes());
    bytes.extend_from_slice(&policy.canonical_bytes());
    match file {
        Some(id) => {
            bytes.push(1);
            bytes.extend_from_slice(id.as_bytes());
        }
        None => bytes.push(0),
    }
    bytes
}

pub async fn enqueue(
    tx: &mut dyn Transaction,
    plan_id: Option<Id>,
    task: Task,
    due_at: i64,
    expires_at: i64,
    priority: u8,
) -> Result<Id> {
    let job = Job {
        id: Id::new_v4(),
        plan_id,
        task,
        state: PartState::Queued,
        due_at,
        expires_at,
        lease_until: 0,
        lease_token: Id::nil(),
        attempts: 0,
        message_id: None,
        priority,
    };
    put(tx, plan_id, &job).await?;
    Ok(job.id)
}
pub async fn notice(
    tx: &mut dyn Transaction,
    plan: Id,
    account: Id,
    key: &str,
    now: i64,
) -> Result<()> {
    enqueue(
        tx,
        Some(plan),
        Task::Notice {
            account_id: account,
            key: key.into(),
            buttons: vec![],
        },
        now,
        now + DAY,
        5,
    )
    .await?;
    Ok(())
}

impl Engine {
    pub async fn limit(&self, key: &str, capacity: i64, period: i64) -> Result<()> {
        let mut tx = self.db.begin().await?;
        let allowed = tx.rate_limit(key, capacity, period).await?;
        tx.commit().await?;
        if allowed {
            Ok(())
        } else {
            Err(Error::RateLimited)
        }
    }
    pub async fn account(&self, telegram_id: i64, chat_id: i64, language: &str) -> Result<Account> {
        if telegram_id <= 0 || chat_id != telegram_id {
            return Err(RuleError::AccessDenied.into());
        }
        let id = Id::from_u128(telegram_id as u128);
        let mut tx = self.db.begin().await?;
        tx.lock(Kind::Account, id).await?;
        let existing = find::<Account>(&mut *tx, "telegram_id", &telegram_id.to_string()).await?;
        if let Some(account) = existing.into_iter().next() {
            return Ok(account);
        }
        let account = Account {
            id,
            telegram_id,
            chat_id,
            locale: if language.split('-').next() == Some("uk") {
                "uk"
            } else {
                "en"
            }
            .into(),
        };
        put(&mut *tx, None, &account).await?;
        tx.commit().await?;
        Ok(account)
    }
    pub async fn locale(&self, actor: Id, locale: &str) -> Result<()> {
        if !["en", "uk"].contains(&locale) {
            return Err(Error::InvalidInput);
        }
        let mut tx = self.db.begin().await?;
        tx.lock(Kind::Account, actor).await?;
        let mut account: Account = get(&mut *tx, actor).await?;
        account.locale = locale.into();
        put(&mut *tx, None, &account).await?;
        tx.commit().await
    }
    /// A valid existing recovery credential has a reserved account admission path
    /// even when ordinary public registration is full. No account is admitted on
    /// the strength of an unauthenticated recovery command alone.
    pub async fn account_for_recovery(
        &self,
        telegram_id: i64,
        chat_id: i64,
        language: &str,
        selector: Id,
        token: &str,
    ) -> Result<Account> {
        if telegram_id <= 0 || chat_id != telegram_id {
            return Err(RuleError::AccessDenied.into());
        }
        let id = Id::from_u128(telegram_id as u128);
        self.limit(&format!("recovery-admission:{id}"), 5, 3600)
            .await?;
        let mut tx = self.db.begin().await?;
        let profile = find::<Profile>(&mut *tx, "recovery_selector", &selector.to_string())
            .await?
            .into_iter()
            .find(|profile| profile.state == "active")
            .ok_or(Error::InvalidCode)?;
        tx.commit().await?;
        // Unknown selectors do not spend the shared memory-hard work budget.
        self.limit("admission:argon-recovery", 120, 60).await?;
        if !self
            .recovery
            .verify(token, selector, &profile.recovery_hash)
            .await?
        {
            return Err(Error::InvalidCode);
        }
        let mut tx = self.db.begin().await?;
        tx.lock(Kind::Profile, profile.id).await?;
        let fresh: Profile = get(&mut *tx, profile.id).await?;
        if fresh.state != "active"
            || fresh.recovery_selector != selector
            || fresh.recovery_hash != profile.recovery_hash
            || tx.get(Kind::DeletionTombstone, profile.id).await?.is_some()
        {
            return Err(Error::InvalidCode);
        }
        if let Some(raw) = tx.get(Kind::Account, id).await? {
            return serde_json::from_value(raw).map_err(|_| Error::Internal);
        }
        let account = Account {
            id,
            telegram_id,
            chat_id,
            locale: if language.split('-').next() == Some("uk") {
                "uk"
            } else {
                "en"
            }
            .into(),
        };
        tx.admit_recovery_account(&account, profile.id, selector)
            .await?;
        tx.commit().await?;
        Ok(account)
    }
    pub async fn own_plan(&self, actor: Id) -> Result<(Profile, Plan)> {
        let mut tx = self.db.begin().await?;
        let profile = find::<Profile>(&mut *tx, "owner_id", &actor.to_string())
            .await?
            .into_iter()
            .find(|p| p.state != "deleted")
            .ok_or(Error::NotFound)?;
        let plan = list::<Plan>(&mut *tx, Some(profile.id))
            .await?
            .into_iter()
            .next()
            .ok_or(Error::NotFound)?;
        Ok((profile, plan))
    }
    pub(crate) async fn owner(
        tx: &mut dyn Transaction,
        actor: Id,
        plan_id: Id,
    ) -> Result<(Profile, Plan)> {
        tx.lock(Kind::Plan, plan_id).await?;
        let plan: Plan = get(tx, plan_id).await?;
        let profile: Profile = get(tx, plan.profile_id).await?;
        if profile.owner_id != actor
            || profile.state != "active"
            || matches!(plan.state, PlanState::Deleted | PlanState::DeletionPending)
        {
            return Err(RuleError::AccessDenied.into());
        }
        Ok((profile, plan))
    }
    pub async fn create_profile(&self, actor: Id) -> Result<Id> {
        match self.own_plan(actor).await {
            Ok((profile, plan)) if profile.pending_claim.is_none() => return Ok(plan.id),
            Ok(_) => return Err(RuleError::NotReady.into()),
            Err(Error::NotFound) => {}
            Err(error) => return Err(error),
        }
        self.limit(&format!("profile:{actor}"), 3, DAY).await?;
        self.limit("admission:profiles", 100, 3600).await?;
        self.limit("admission:argon", 120, 60).await?;
        let (selector, token, hash) = self.recovery.issue().await?;
        let mut tx = self.db.begin().await?;
        tx.lock(Kind::Account, actor).await?;
        get::<Account>(&mut *tx, actor).await?;
        let existing = find::<Profile>(&mut *tx, "owner_id", &actor.to_string())
            .await?
            .into_iter()
            .find(|p| p.state == "active");
        let mut profile = if let Some(hint) = existing {
            tx.lock(Kind::Profile, hint.id).await?;
            let current: Profile = get(&mut *tx, hint.id).await?;
            if current.owner_id != actor || current.pending_claim.is_some() {
                return Err(RuleError::AccessDenied.into());
            }
            if let Some(plan) = list::<Plan>(&mut *tx, Some(current.id))
                .await?
                .into_iter()
                .find(|p| !matches!(p.state, PlanState::Deleted | PlanState::DeletionPending))
            {
                return Ok(plan.id);
            }
            current
        } else {
            Profile {
                id: Id::new_v4(),
                owner_id: actor,
                owner_epoch: 1,
                state: "active".into(),
                recovery_selector: selector,
                recovery_hash: hash.clone(),
                recovery_saved: false,
                pending_claim: None,
            }
        };
        // Deleting just a plan preserves the profile's acknowledged recovery key.
        // A new plan has a new ID and never inherits old jobs, contacts or policies.
        let issue_recovery = !profile.recovery_saved;
        if issue_recovery {
            profile.recovery_selector = selector;
            profile.recovery_hash = hash;
        }
        let now = tx.now().await?;
        let timing = Timing::default();
        let profile_id = profile.id;
        let plan_id = Id::new_v4();
        let plan = Plan {
            id: plan_id,
            profile_id,
            state: PlanState::Setup,
            epoch: 1,
            timing: timing.clone(),
            last_activity: now,
            due_at: now + timing.inactivity_seconds,
            next_reminder: now + timing.reminder_seconds,
            hold_until: 0,
            pending_control: None,
        };
        put(&mut *tx, Some(actor), &profile).await?;
        put(&mut *tx, Some(profile_id), &plan).await?;
        if issue_recovery {
            let envelope = self.crypto.wrap("recovery", selector, token.as_bytes())?;
            enqueue(
                &mut *tx,
                Some(plan_id),
                Task::RecoveryCode {
                    profile_id,
                    account_id: actor,
                    envelope,
                    selector,
                },
                now,
                now + 900,
                1,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(plan_id)
    }
    pub async fn acknowledge_recovery(&self, actor: Id, plan_id: Id, selector: Id) -> Result<()> {
        let mut tx = self.db.begin().await?;
        let (mut profile, _) = Self::owner(&mut *tx, actor, plan_id).await?;
        if profile.recovery_selector != selector {
            return Err(RuleError::StaleAction.into());
        }
        profile.recovery_saved = true;
        put(&mut *tx, Some(actor), &profile).await?;
        for job in list::<Job>(&mut *tx, Some(plan_id)).await? {
            if matches!(job.task,Task::RecoveryCode{selector:s,..} if s==selector) {
                tx.remove(Kind::Job, job.id).await?;
            }
        }
        tx.commit().await
    }
    pub async fn invite(&self, actor: Id, plan_id: Id) -> Result<Id> {
        self.invite_with_id(actor, plan_id, Id::new_v4()).await
    }
    pub async fn invite_with_id(&self, actor: Id, plan_id: Id, operation_id: Id) -> Result<Id> {
        let mut tx = self.db.begin().await?;
        Self::owner(&mut *tx, actor, plan_id).await?;
        if let Some(raw) = tx.get(Kind::Invitation, operation_id).await? {
            let existing: Invitation = serde_json::from_value(raw).map_err(|_| Error::Internal)?;
            if existing.plan_id != plan_id || existing.owner_id != actor {
                return Err(RuleError::AccessDenied.into());
            }
            return Ok(existing.id);
        }
        tx.commit().await?;
        self.limit(&format!("invite:{actor}"), 10, DAY).await?;
        let mut tx = self.db.begin().await?;
        Self::owner(&mut *tx, actor, plan_id).await?;
        if let Some(raw) = tx.get(Kind::Invitation, operation_id).await? {
            let existing: Invitation = serde_json::from_value(raw).map_err(|_| Error::Internal)?;
            if existing.plan_id != plan_id || existing.owner_id != actor {
                return Err(RuleError::AccessDenied.into());
            }
            return Ok(existing.id);
        }
        let mut active_people = 0;
        for person in list::<Participant>(&mut *tx, Some(plan_id)).await? {
            active_people += usize::from(Self::contact_active(&mut *tx, person.id).await?);
        }
        if active_people >= 20 {
            return Err(RuleError::QuotaExceeded.into());
        }
        let i = Invitation {
            id: operation_id,
            plan_id,
            owner_id: actor,
            expires_at: tx.now().await? + DAY,
            accepted_by: None,
        };
        put(&mut *tx, Some(plan_id), &i).await?;
        tx.commit().await?;
        Ok(i.id)
    }
    pub async fn accept_invite(&self, actor: Id, id: Id) -> Result<Id> {
        let mut tx = self.db.begin().await?;
        let hint: Invitation = get(&mut *tx, id).await?;
        tx.lock(Kind::Plan, hint.plan_id).await?;
        tx.lock(Kind::Invitation, id).await?;
        let mut i: Invitation = get(&mut *tx, id).await?;
        let invitation_state = Self::invitation_state(&mut *tx, &i).await?;
        if (i.accepted_by.is_none() && i.expires_at <= tx.now().await?)
            || i.owner_id == actor
            || i.accepted_by.is_some_and(|a| a != actor)
            || invitation_state.revoked
            || invitation_state.declined.contains(&actor)
        {
            return Err(RuleError::AccessDenied.into());
        }
        let (profile, plan) = Self::owner(&mut *tx, i.owner_id, i.plan_id).await?;
        if profile.pending_claim.is_some() || plan.pending_control.is_some() {
            return Err(RuleError::InvalidState.into());
        }
        let participants = list::<Participant>(&mut *tx, Some(i.plan_id)).await?;
        if i.accepted_by == Some(actor) {
            return if participants.iter().any(|p| p.account_id == actor) {
                Ok(i.plan_id)
            } else {
                Err(RuleError::InvalidState.into())
            };
        }
        i.accepted_by = Some(actor);
        put(&mut *tx, Some(i.plan_id), &i).await?;
        if !participants.iter().any(|p| p.account_id == actor) {
            let mut active_people = 0;
            for person in &participants {
                active_people += usize::from(Self::contact_active(&mut *tx, person.id).await?);
            }
            if active_people >= 20 {
                return Err(RuleError::QuotaExceeded.into());
            }
            put(
                &mut *tx,
                Some(i.plan_id),
                &Participant {
                    id: Id::new_v4(),
                    plan_id: i.plan_id,
                    account_id: actor,
                    confirmed: false,
                },
            )
            .await?;
        } else if let Some(person) = participants.iter().find(|p| p.account_id == actor)
            && !Self::contact_active(&mut *tx, person.id).await?
        {
            let mut person = person.clone();
            person.confirmed = false;
            tx.remove(Kind::ContactState, person.id).await?;
            put(&mut *tx, Some(i.plan_id), &person).await?;
        }
        let now = tx.now().await?;
        notice(&mut *tx, i.plan_id, i.owner_id, "participant-joined", now).await?;
        tx.commit().await?;
        Ok(i.plan_id)
    }
    pub async fn confirm_participant(
        &self,
        actor: Id,
        plan_id: Id,
        participant_id: Id,
    ) -> Result<()> {
        let mut tx = self.db.begin().await?;
        Self::owner(&mut *tx, actor, plan_id).await?;
        let mut p: Participant = get(&mut *tx, participant_id).await?;
        if p.plan_id != plan_id || !Self::contact_active(&mut *tx, p.id).await? {
            return Err(RuleError::AccessDenied.into());
        }
        p.confirmed = true;
        put(&mut *tx, Some(plan_id), &p).await?;
        tx.commit().await
    }
    pub async fn new_draft(&self, actor: Id, plan_id: Id) -> Result<Id> {
        let mut tx = self.db.begin().await?;
        if !tx.writes_ready().await? {
            return Err(RuleError::NotReady.into());
        }
        let (profile, plan) = Self::owner(&mut *tx, actor, plan_id).await?;
        if !profile.recovery_saved
            || profile.pending_claim.is_some()
            || plan.pending_control.is_some()
        {
            return Err(RuleError::NotReady.into());
        }
        let now = tx.now().await?;
        if let Some(draft) = list::<Draft>(&mut *tx, Some(plan_id))
            .await?
            .into_iter()
            .find(|d| {
                d.expires_at > now
                    && d.saved_secret.is_none()
                    && d.owner_epoch == profile.owner_epoch
            })
        {
            return Ok(draft.id);
        }
        if list::<Secret>(&mut *tx, Some(plan_id))
            .await?
            .iter()
            .filter(|s| s.state != SecretState::Deleted)
            .count()
            >= 50
        {
            return Err(RuleError::QuotaExceeded.into());
        }
        let id = Id::new_v4();
        let payload = self.crypto.wrap("draft", id, b"[]")?;
        let d = Draft {
            id,
            plan_id,
            owner_epoch: profile.owner_epoch,
            revision: 0,
            created_at: now,
            expires_at: now + 900,
            payload,
            policy: None,
            saved_secret: None,
            sources: vec![],
        };
        put(&mut *tx, Some(plan_id), &d).await?;
        tx.commit().await?;
        Ok(id)
    }
    pub(crate) async fn editable(
        &self,
        tx: &mut dyn Transaction,
        actor: Id,
        id: Id,
    ) -> Result<(Draft, Plan)> {
        if !tx.writes_ready().await? {
            return Err(RuleError::NotReady.into());
        }
        let draft: Draft = get(tx, id).await?;
        let (profile, plan) = Self::owner(tx, actor, draft.plan_id).await?;
        let draft: Draft = get(tx, id).await?;
        let now = tx.now().await?;
        if draft.saved_secret.is_some()
            || draft.expires_at <= now
            || draft.created_at + 3600 <= now
            || draft.owner_epoch != profile.owner_epoch
            || profile.pending_claim.is_some()
            || plan.pending_control.is_some()
        {
            return Err(RuleError::StaleAction.into());
        }
        Ok((draft, plan))
    }
    pub async fn draft_blocks(&self, actor: Id, id: Id) -> Result<Vec<Block>> {
        let mut tx = self.db.begin().await?;
        let (d, _) = self.editable(&mut *tx, actor, id).await?;
        serde_json::from_slice(&self.crypto.unwrap("draft", id, &d.payload)?)
            .map_err(|_| Error::Crypto)
    }
    pub async fn append_block(
        &self,
        actor: Id,
        id: Id,
        block: Block,
        source: (i64, i64),
    ) -> Result<()> {
        // File references are created only by the managed upload path, never accepted
        // from an arbitrary adapter/caller as a capability to another object.
        if matches!(block, Block::File { .. }) {
            return Err(RuleError::AccessDenied.into());
        }
        let mut tx = self.db.begin().await?;
        let (mut draft, _) = self.editable(&mut *tx, actor, id).await?;
        self.append_to_draft(&mut *tx, &mut draft, block, source)
            .await?;
        tx.commit().await
    }
    async fn append_to_draft(
        &self,
        tx: &mut dyn Transaction,
        draft: &mut Draft,
        block: Block,
        source: (i64, i64),
    ) -> Result<bool> {
        if draft.sources.contains(&source) {
            return Ok(false);
        }
        let mut blocks: Vec<Block> =
            serde_json::from_slice(&self.crypto.unwrap("draft", draft.id, &draft.payload)?)
                .map_err(|_| Error::Crypto)?;
        blocks.push(block);
        validate_blocks(&blocks)?;
        let bytes = Zeroizing::new(serde_json::to_vec(&blocks).map_err(|_| Error::Internal)?);
        draft.payload = self.crypto.wrap("draft", draft.id, &bytes)?;
        draft.revision += 1;
        draft.expires_at = (tx.now().await? + 900).min(draft.created_at + 3600);
        draft.sources.push(source);
        put(tx, Some(draft.plan_id), draft).await?;
        Ok(true)
    }
    pub async fn draft_policy(&self, actor: Id, id: Id, policy: Policy) -> Result<()> {
        policy.validate(actor)?;
        let mut tx = self.db.begin().await?;
        let (mut d, _) = self.editable(&mut *tx, actor, id).await?;
        Self::validate_people(&mut *tx, d.plan_id, &policy).await?;
        if d.policy.as_ref() == Some(&policy) {
            return Ok(());
        }
        d.policy = Some(policy);
        d.revision += 1;
        put(&mut *tx, Some(d.plan_id), &d).await?;
        tx.commit().await
    }
    pub async fn append_file(
        &self,
        actor: Id,
        draft_id: Id,
        name: String,
        caption: String,
        bytes: Zeroizing<Vec<u8>>,
        source: (i64, i64),
    ) -> Result<()> {
        if bytes.len() > 10 * 1024 * 1024 || name.len() > 255 || caption.len() > 4096 {
            return Err(RuleError::QuotaExceeded.into());
        }
        let mut tx = self.db.begin().await?;
        let (d, _) = self.editable(&mut *tx, actor, draft_id).await?;
        if d.sources.contains(&source) {
            return Ok(());
        }
        let used = list::<FileObject>(&mut *tx, Some(d.plan_id))
            .await?
            .iter()
            .filter(|o| o.state != "deleted")
            .map(|o| o.size)
            .sum::<u64>();
        let id = Id::new_v4();
        let envelope = self.crypto.wrap("draft-file", id, &bytes)?;
        let ciphertext = serde_json::to_vec(&envelope).map_err(|_| Error::Internal)?;
        if used.saturating_add(ciphertext.len() as u64) > 250 * 1024 * 1024 {
            return Err(RuleError::QuotaExceeded.into());
        }
        let now = tx.now().await?;
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_secs(MAX_BLOB_OPERATION_SECS);
        let object = FileObject {
            id,
            plan_id: d.plan_id,
            draft_id,
            operation_id: id,
            key: format!("draft/{id}"),
            size: ciphertext.len() as u64,
            digest: self.crypto.digest(&ciphertext),
            state: "pending".into(),
            created_at: now,
            due_at: now + DAY,
        };
        if !tx
            .reserve_resource(
                "blob_bytes",
                id,
                ciphertext.len() as i64,
                10 * 1024 * 1024 * 1024,
            )
            .await?
        {
            return Err(RuleError::QuotaExceeded.into());
        }
        put(&mut *tx, Some(d.plan_id), &object).await?;
        tx.commit().await?;
        blob_io(deadline, self.blobs.put(&object.key, &ciphertext)).await?;
        let block = Block::File {
            file: FileRef {
                id,
                object_key: object.key.clone(),
                encrypted_size: bytes.len() as u64 + 40,
                sha256: object.digest.clone(),
            },
            name,
            caption,
        };
        // The attachment and object state form one transaction. Revalidate after
        // external I/O so delete/expiry/recovery cannot be undone by late bookkeeping.
        let mut tx = self.db.begin().await?;
        let (mut fresh, _) = self.editable(&mut *tx, actor, draft_id).await?;
        let mut object: FileObject = get(&mut *tx, id).await?;
        if object.state != "pending"
            || object.draft_id != draft_id
            || object.plan_id != fresh.plan_id
        {
            return Err(RuleError::StaleAction.into());
        }
        if self
            .append_to_draft(&mut *tx, &mut fresh, block, source)
            .await?
        {
            object.state = "draft".into();
        } else {
            object.state = "gc".into();
            object.due_at = tx.now().await?;
        }
        put(&mut *tx, Some(fresh.plan_id), &object).await?;
        tx.commit().await
    }
    pub async fn save(&self, actor: Id, draft_id: Id) -> Result<Id> {
        let mut tx = self.db.begin().await?;
        // A retry of Save returns only the identifier, never content or keys.
        let previous: Draft = get(&mut *tx, draft_id).await?;
        Self::owner(&mut *tx, actor, previous.plan_id).await?;
        if let Some(id) = previous.saved_secret {
            return Ok(id);
        }
        let (d, plan) = self.editable(&mut *tx, actor, draft_id).await?;
        let policy = d.policy.clone().ok_or(RuleError::NotReady)?;
        policy.validate(actor)?;
        Self::validate_people(&mut *tx, plan.id, &policy).await?;
        let mut blocks: Vec<Block> =
            serde_json::from_slice(&self.crypto.unwrap("draft", draft_id, &d.payload)?)
                .map_err(|_| Error::Crypto)?;
        validate_blocks(&blocks)?;
        let id = Id::new_v4();
        let key = self.crypto.random_key()?;
        let now = tx.now().await?;
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_secs(MAX_BLOB_OPERATION_SECS);
        let mut objects = Vec::new();
        let mut reserved_bytes = list::<FileObject>(&mut *tx, Some(plan.id))
            .await?
            .iter()
            .filter(|object| object.state != "deleted")
            .map(|object| object.size)
            .sum::<u64>();
        for block in &blocks {
            if let Block::File { file, .. } = block {
                let object = FileObject {
                    id: Id::new_v4(),
                    plan_id: plan.id,
                    draft_id,
                    operation_id: id,
                    key: format!("sealed/{id}/{}", Id::new_v4()),
                    // A sealed JSON envelope is base64 encoded; reserve its full bound
                    // before releasing SQL for external storage I/O.
                    size: file.encrypted_size.saturating_mul(4).div_ceil(3) + 256,
                    digest: String::new(),
                    state: "pending".into(),
                    created_at: now,
                    due_at: now + DAY,
                };
                // Draft copies and pending cleanup still occupy storage. Saving must
                // reserve room for both copies until physical garbage collection.
                reserved_bytes = reserved_bytes.saturating_add(object.size);
                if reserved_bytes > 250 * 1024 * 1024 {
                    return Err(RuleError::QuotaExceeded.into());
                }
                if !tx
                    .reserve_resource(
                        "blob_bytes",
                        object.id,
                        object.size as i64,
                        10 * 1024 * 1024 * 1024,
                    )
                    .await?
                {
                    return Err(RuleError::QuotaExceeded.into());
                }
                put(&mut *tx, Some(plan.id), &object).await?;
                objects.push(object);
            }
        }
        tx.commit().await?;
        let mut object_index = 0;
        for block in &mut blocks {
            if let Block::File { file, .. } = block {
                let source =
                    blob_io(deadline, self.blobs.get(&file.object_key, 15 * 1024 * 1024)).await?;
                if self.crypto.digest(&source) != file.sha256 {
                    return Err(Error::Crypto);
                }
                let envelope: Envelope =
                    serde_json::from_slice(&source).map_err(|_| Error::Crypto)?;
                let plain = self.crypto.unwrap("draft-file", file.id, &envelope)?;
                let object = &mut objects[object_index];
                object_index += 1;
                let sealed =
                    self.crypto
                        .seal(&key, &content_aad(id, &policy, Some(object.id)), &plain)?;
                let ciphertext = serde_json::to_vec(&sealed).map_err(|_| Error::Internal)?;
                if ciphertext.len() as u64 > object.size {
                    return Err(RuleError::QuotaExceeded.into());
                }
                object.digest = self.crypto.digest(&ciphertext);
                object.state = "sealed".into();
                object.due_at = i64::MAX;
                blob_io(deadline, self.blobs.put(&object.key, &ciphertext)).await?;
                *file = FileRef {
                    id: object.id,
                    object_key: object.key.clone(),
                    encrypted_size: plain.len() as u64 + 40,
                    sha256: object.digest.clone(),
                };
            }
        }
        let plain = Zeroizing::new(serde_json::to_vec(&blocks).map_err(|_| Error::Internal)?);
        let payload = self
            .crypto
            .seal(&key, &content_aad(id, &policy, None), &plain)?;
        let codes = self.crypto.split(id, &key, &policy)?;
        let mut tx = self.db.begin().await?;
        let (mut fresh, fresh_plan) = self.editable(&mut *tx, actor, draft_id).await?;
        if fresh.revision != d.revision || fresh_plan.epoch != plan.epoch {
            return Err(RuleError::StaleAction.into());
        }
        Self::validate_people(&mut *tx, plan.id, &policy).await?;
        let existing = list::<Secret>(&mut *tx, Some(plan.id)).await?;
        if existing
            .iter()
            .filter(|s| s.state != SecretState::Deleted)
            .count()
            >= 50
        {
            return Err(RuleError::QuotaExceeded.into());
        }
        let mut union: BTreeSet<Id> = BTreeSet::new();
        for s in &existing {
            if s.state != SecretState::Deleted {
                union.extend(&s.policy.guardians);
            }
        }
        union.extend(&policy.guardians);
        if union.len() > 20 {
            return Err(RuleError::QuotaExceeded.into());
        }
        let now = tx.now().await?;
        let secret = Secret {
            id,
            plan_id: plan.id,
            epoch: 1,
            state: SecretState::Provisioning,
            policy,
            payload,
            created_at: now,
            due_at: now + DAY,
            last_case: None,
            pending_control: None,
        };
        put(&mut *tx, Some(plan.id), &secret).await?;
        if let Some(label) = self.label_in(&mut *tx, plan.id, draft_id).await? {
            put(
                &mut *tx,
                Some(plan.id),
                &PrivateMetadata {
                    id,
                    plan_id: plan.id,
                    label: self.crypto.wrap(
                        &format!("owner-label/{}", plan.id),
                        id,
                        label.as_bytes(),
                    )?,
                },
            )
            .await?;
            tx.remove(Kind::PrivateMetadata, draft_id).await?;
        }
        for code in codes {
            let grant_id = Id::new_v4();
            let delivery = Some(self.crypto.wrap("grant", grant_id, code.code.as_bytes())?);
            let grant = GuardianGrant {
                id: grant_id,
                secret_id: id,
                account_id: code.account_id,
                index: code.index,
                verifier: code.verifier,
                verifier_key_id: code.verifier_key_id,
                ready: false,
                delivery,
                expires_at: now + DAY,
            };
            put(&mut *tx, Some(id), &grant).await?;
            enqueue(
                &mut *tx,
                Some(plan.id),
                Task::GuardianCode { grant_id },
                now,
                now + DAY,
                2,
            )
            .await?;
        }
        for object in objects {
            put(&mut *tx, Some(plan.id), &object).await?;
        }
        for (chat_id, message_id) in &fresh.sources {
            enqueue(
                &mut *tx,
                Some(plan.id),
                Task::CleanupMessage {
                    chat_id: *chat_id,
                    message_id: *message_id,
                    sent_at: fresh.created_at,
                    account_id: actor,
                },
                now,
                now + DAY,
                1,
            )
            .await?;
        }
        fresh.payload = self.crypto.wrap("draft", draft_id, b"[]")?;
        fresh.saved_secret = Some(id);
        fresh.sources.clear();
        fresh.policy = None;
        put(&mut *tx, Some(plan.id), &fresh).await?;
        Self::record_receipt(
            &mut *tx,
            OperationReceipt {
                id: draft_id,
                actor_id: actor,
                plan_id: Some(plan.id),
                secret_id: Some(id),
                operation: OperationKind::SecretSaved,
                status: ReceiptStatus::Completed,
                at: now,
                due_at: now + 7 * DAY,
                pending_objects: Default::default(),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(id)
    }
    pub async fn acknowledge_grant(&self, actor: Id, grant_id: Id) -> Result<()> {
        let mut tx = self.db.begin().await?;
        let grant: GuardianGrant = get(&mut *tx, grant_id).await?;
        let secret: Secret = get(&mut *tx, grant.secret_id).await?;
        tx.lock(Kind::Plan, secret.plan_id).await?;
        let mut grant: GuardianGrant = get(&mut *tx, grant_id).await?;
        let mut secret: Secret = get(&mut *tx, grant.secret_id).await?;
        if grant.account_id != actor {
            return Err(RuleError::AccessDenied.into());
        }
        if grant.ready {
            return Ok(());
        }
        if grant.expires_at <= tx.now().await? || secret.state != SecretState::Provisioning {
            return Err(RuleError::Expired.into());
        }
        grant.ready = true;
        grant.delivery = None;
        put(&mut *tx, Some(secret.id), &grant).await?;
        for job in list::<Job>(&mut *tx, Some(secret.plan_id)).await? {
            if matches!(job.task,Task::GuardianCode{grant_id:id} if id==grant_id) {
                tx.remove(Kind::Job, job.id).await?;
            }
        }
        if list::<GuardianGrant>(&mut *tx, Some(secret.id))
            .await?
            .iter()
            .all(|g| g.ready)
        {
            let mut plan: Plan = get(&mut *tx, secret.plan_id).await?;
            plan.next_reminder = plan
                .next_reminder
                .min(plan.last_activity + secret.policy.timing.reminder_seconds);
            put(&mut *tx, Some(plan.profile_id), &plan).await?;
            secret.state = SecretState::Armed;
            secret.due_at = plan.last_activity + secret.policy.timing.inactivity_seconds;
            put(&mut *tx, Some(secret.plan_id), &secret).await?;
            let profile: Profile = get(&mut *tx, plan.profile_id).await?;
            let now = tx.now().await?;
            notice(&mut *tx, plan.id, profile.owner_id, "secret-armed", now).await?;
        }
        tx.commit().await
    }
    pub async fn resend_grant(&self, actor: Id, id: Id) -> Result<()> {
        self.limit(&format!("grant-resend:{actor}:{id}"), 3, 3600)
            .await?;
        let mut tx = self.db.begin().await?;
        let g: GuardianGrant = get(&mut *tx, id).await?;
        let s: Secret = get(&mut *tx, g.secret_id).await?;
        tx.lock(Kind::Plan, s.plan_id).await?;
        let g: GuardianGrant = get(&mut *tx, id).await?;
        let now = tx.now().await?;
        if g.account_id != actor
            || g.ready
            || g.delivery.is_none()
            || g.expires_at <= now
            || s.state != SecretState::Provisioning
        {
            return Err(RuleError::AccessDenied.into());
        }
        enqueue(
            &mut *tx,
            Some(s.plan_id),
            Task::GuardianCode { grant_id: id },
            now,
            g.expires_at,
            1,
        )
        .await?;
        tx.commit().await
    }
}
