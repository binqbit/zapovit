use crate::*;
use domain::{
    Block, CaseState, DAY, Id, PartState, PlanState, ReleaseCase, RuleError, SecretState,
};
use time::OffsetDateTime;
use zeroize::Zeroizing;

fn timestamp(now: i64) -> Result<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(now).map_err(|_| Error::Internal)
}
fn part_aad(id: Id, secret: Id, recipient: Id) -> Vec<u8> {
    let mut b = b"zapovit/part/v1\0".to_vec();
    for id in [id, secret, recipient] {
        b.extend_from_slice(id.as_bytes());
    }
    b
}

async fn manual_cleanup_notice(tx: &mut dyn Transaction, job: &Job, now: i64) -> Result<()> {
    if let Task::CleanupMessage { account_id, .. } = &job.task
        && tx.get(Kind::Account, *account_id).await?.is_some()
    {
        enqueue(
            tx,
            None,
            Task::Notice {
                account_id: *account_id,
                key: "manual-delete".into(),
                buttons: vec![],
            },
            now,
            now + DAY,
            0,
        )
        .await?;
    }
    Ok(())
}

impl Engine {
    pub async fn submit_code(&self, actor: Id, case_id: Id, code: &str) -> Result<()> {
        self.limit(&format!("code:{actor}:{case_id}"), 5, 900)
            .await?;
        let mut tx = self.db.begin().await?;
        let c: CaseRecord = get(&mut *tx, case_id).await?;
        tx.lock(Kind::Plan, c.plan_id).await?;
        let mut c: CaseRecord = get(&mut *tx, case_id).await?;
        let plan: Plan = get(&mut *tx, c.plan_id).await?;
        let secret: Secret = get(&mut *tx, c.secret_id).await?;
        let now = tx.now().await?;
        if plan.state != PlanState::Active
            || plan.pending_control.is_some()
            || secret.pending_control.is_some()
            || secret.state != SecretState::Armed
            || plan.epoch != c.case.plan_epoch
            || secret.epoch != c.case.secret_epoch
        {
            return Err(RuleError::StaleAction.into());
        }
        if c.case.confirmed.contains(&actor) {
            return Ok(());
        }
        let grant = list::<GuardianGrant>(&mut *tx, Some(secret.id))
            .await?
            .into_iter()
            .find(|g| g.account_id == actor && g.ready)
            .ok_or(RuleError::AccessDenied)?;
        let share = self.crypto.verify_code(
            secret.id,
            actor,
            &secret.policy,
            &grant.verifier_key_id,
            &grant.verifier,
            code,
        )?;
        c.case.confirm(actor, &secret.policy, timestamp(now)?)?;
        let id = Id::new_v4();
        let submission = Submission {
            id,
            case_id,
            account_id: actor,
            share: self
                .crypto
                .wrap(&format!("submission/{case_id}/{actor}"), id, &share)?,
        };
        put(&mut *tx, Some(case_id), &submission).await?;
        if let Some(release) = c.case.release_at {
            c.due_at = release.unix_timestamp();
            let profile: Profile = get(&mut *tx, plan.profile_id).await?;
            notice(&mut *tx, plan.id, profile.owner_id, "quorum-reached", now).await?;
        }
        put(&mut *tx, Some(secret.id), &c).await?;
        tx.commit().await
    }
    async fn case_key(
        &self,
        tx: &mut dyn Transaction,
        case: &CaseRecord,
        secret: &Secret,
    ) -> Result<Zeroizing<Vec<u8>>> {
        let mut shares = Vec::new();
        for sub in list::<Submission>(tx, Some(case.id)).await? {
            if !case.case.confirmed.contains(&sub.account_id)
                || !secret.policy.guardians.contains(&sub.account_id)
            {
                return Err(Error::Crypto);
            }
            shares.push(self.crypto.unwrap(
                &format!("submission/{}/{}", case.id, sub.account_id),
                sub.id,
                &sub.share,
            )?);
        }
        self.crypto.combine(secret.policy.threshold, &shares)
    }
    /// Work is selected from persisted deadlines; all transitions are serialized on the plan.
    pub async fn tick_plan(&self, plan_id: Id) -> Result<()> {
        let mut tx = self.db.begin().await?;
        tx.lock(Kind::Plan, plan_id).await?;
        if tx.get(Kind::Plan, plan_id).await?.is_none() {
            return Ok(());
        }
        let mut plan: Plan = get(&mut *tx, plan_id).await?;
        let now = tx.now().await?;
        if plan.state != PlanState::Active
            || plan.pending_control.is_some()
            || plan.hold_until > now
            || !tx.operational_status(Some(plan.id)).await?.ready
        {
            return Ok(());
        }
        let profile: Profile = get(&mut *tx, plan.profile_id).await?;
        let secrets = list::<Secret>(&mut *tx, Some(plan.id)).await?;
        let reminder_seconds = secrets
            .iter()
            .filter(|secret| secret.state == SecretState::Armed)
            .map(|secret| secret.policy.timing.reminder_seconds)
            .min()
            .unwrap_or(plan.timing.reminder_seconds);
        if plan.next_reminder <= now {
            notice(&mut *tx, plan.id, profile.owner_id, "checkin-reminder", now).await?;
            plan.next_reminder = now + reminder_seconds;
            put(&mut *tx, Some(profile.id), &plan).await?;
        }
        for mut secret in secrets {
            if secret.state != SecretState::Armed || secret.pending_control.is_some() {
                continue;
            }
            if let Some(case_id) = secret.last_case {
                let mut c: CaseRecord = get(&mut *tx, case_id).await?;
                if c.case.state == CaseState::Collecting
                    && now >= c.case.created_at.unix_timestamp() + 30 * DAY
                {
                    c.case.state = CaseState::Expired;
                    c.due_at = i64::MAX;
                    put(&mut *tx, Some(secret.id), &c).await?;
                    for sub in list::<Submission>(&mut *tx, Some(c.id)).await? {
                        tx.remove(Kind::Submission, sub.id).await?;
                    }
                    secret.last_case = None;
                    secret.due_at = now + 7 * DAY;
                    put(&mut *tx, Some(plan.id), &secret).await?;
                    notice(&mut *tx, plan.id, profile.owner_id, "case-expired", now).await?;
                } else if c.case.may_dispatch(
                    plan.state,
                    plan.epoch,
                    secret.epoch,
                    &secret.policy,
                    timestamp(now)?,
                    false,
                ) && c.started_delivery.is_none()
                {
                    let key = self.case_key(&mut *tx, &c, &secret).await?;
                    let plain = self.crypto.open(
                        &key,
                        &content_aad(secret.id, &secret.policy, None),
                        &secret.payload,
                    )?;
                    let blocks: Vec<Block> =
                        serde_json::from_slice(&plain).map_err(|_| Error::Crypto)?;
                    let blocks = delivery_blocks(blocks);
                    let existing = list::<DeliveryPart>(&mut *tx, Some(secret.id)).await?;
                    for recipient in &secret.policy.recipients {
                        for (index, block) in blocks.iter().enumerate() {
                            let part = if let Some(old) = existing
                                .iter()
                                .find(|p| p.recipient_id == *recipient && p.index == index)
                            {
                                // Sent or ambiguous sends are never repeated by a new inactivity case.
                                if matches!(
                                    old.state,
                                    PartState::Sent
                                        | PartState::Unknown
                                        | PartState::Dispatching
                                        | PartState::PermanentFailed
                                ) {
                                    continue;
                                }
                                let mut p = old.clone();
                                p.state = PartState::Queued;
                                p
                            } else {
                                let id = Id::new_v4();
                                let bytes = Zeroizing::new(
                                    serde_json::to_vec(block).map_err(|_| Error::Internal)?,
                                );
                                DeliveryPart {
                                    id,
                                    secret_id: secret.id,
                                    recipient_id: *recipient,
                                    index,
                                    manifest: self.crypto.seal(
                                        &key,
                                        &part_aad(id, secret.id, *recipient),
                                        &bytes,
                                    )?,
                                    state: PartState::Queued,
                                    last_attempt: None,
                                }
                            };
                            put(&mut *tx, Some(secret.id), &part).await?;
                            enqueue(
                                &mut *tx,
                                Some(plan.id),
                                Task::Deliver {
                                    part_id: part.id,
                                    case_id: c.id,
                                },
                                now,
                                now + 7 * DAY,
                                3,
                            )
                            .await?;
                        }
                    }
                    c.started_delivery = Some(now);
                    c.case.state = CaseState::Delivering;
                    put(&mut *tx, Some(secret.id), &c).await?;
                }
            } else if secret.due_at <= now {
                let case = ReleaseCase::new(secret.id, plan.epoch, secret.epoch, timestamp(now)?);
                let c = CaseRecord {
                    id: case.id,
                    plan_id: plan.id,
                    secret_id: secret.id,
                    case,
                    due_at: now + 30 * DAY,
                    started_delivery: None,
                };
                secret.last_case = Some(c.id);
                put(&mut *tx, Some(secret.id), &c).await?;
                put(&mut *tx, Some(plan.id), &secret).await?;
                for guardian in &secret.policy.guardians {
                    enqueue(
                        &mut *tx,
                        Some(plan.id),
                        Task::GuardianRequest {
                            case_id: c.id,
                            account_id: *guardian,
                        },
                        now,
                        now + DAY,
                        2,
                    )
                    .await?;
                }
                notice(
                    &mut *tx,
                    plan.id,
                    profile.owner_id,
                    "inactivity-detected",
                    now,
                )
                .await?;
            }
        }
        tx.commit().await
    }
    pub async fn claim_job(&self, id: Id) -> Result<Option<Job>> {
        let mut tx = self.db.begin().await?;
        tx.lock(Kind::Job, id).await?;
        let Some(raw) = tx.get(Kind::Job, id).await? else {
            return Ok(None);
        };
        let mut job: Job = serde_json::from_value(raw).map_err(|_| Error::Internal)?;
        let now = tx.now().await?;
        if matches!(job.state, PartState::Claimed | PartState::Dispatching)
            && job.lease_until <= now
        {
            job.state = if matches!(
                job.task,
                Task::CleanupMessage { .. } | Task::DeleteObject { .. }
            ) {
                // Deletion is idempotent, and must remain executable after its parent is purged.
                PartState::Queued
            } else {
                job.state.after_worker_loss()
            };
            if job.state == PartState::Unknown {
                put(&mut *tx, job.plan_id, &job).await?;
                tx.commit().await?;
                self.finish_job(id, job.lease_token, SendResult::Unknown)
                    .await?;
                return Ok(None);
            }
        }
        if job.expires_at <= now {
            if matches!(
                job.task,
                Task::CleanupMessage { .. } | Task::DeleteObject { .. }
            ) {
                // Expiry must not silently leave private source messages behind.
                // The notice and job removal commit together, including on retry.
                manual_cleanup_notice(&mut *tx, &job, now).await?;
                for attempt in list::<Attempt>(&mut *tx, Some(id)).await? {
                    tx.remove(Kind::Attempt, attempt.id).await?;
                }
                tx.remove(Kind::Job, id).await?;
                tx.commit().await?;
                return Ok(None);
            }
            if matches!(job.state, PartState::Queued | PartState::RetryableFailed) {
                job.state = PartState::Cancelled;
                put(&mut *tx, job.plan_id, &job).await?;
                tx.commit().await?;
            }
            return Ok(None);
        }
        if !matches!(job.state, PartState::Queued | PartState::RetryableFailed) || job.due_at > now
        {
            return Ok(None);
        }
        // Backup maintenance drains existing bounded I/O without admitting more.
        // Protective inbox controls remain available independently of workers.
        if !tx.writes_ready().await? {
            return Ok(None);
        }
        job.state = PartState::Claimed;
        job.lease_token = Id::new_v4();
        job.lease_until = now + 120;
        job.attempts += 1;
        put(&mut *tx, job.plan_id, &job).await?;
        tx.commit().await?;
        Ok(Some(job))
    }
    pub async fn delivery_block(
        &self,
        job: &Job,
    ) -> Result<(Account, Block, Option<Zeroizing<Vec<u8>>>)> {
        let Task::Deliver { part_id, case_id } = job.task else {
            return Err(Error::InvalidInput);
        };
        let mut tx = self.db.begin().await?;
        let part: DeliveryPart = get(&mut *tx, part_id).await?;
        let c: CaseRecord = get(&mut *tx, case_id).await?;
        let secret: Secret = get(&mut *tx, c.secret_id).await?;
        if part.secret_id != secret.id || !secret.policy.recipients.contains(&part.recipient_id) {
            return Err(RuleError::AccessDenied.into());
        }
        let key = self.case_key(&mut *tx, &c, &secret).await?;
        let bytes = self.crypto.open(
            &key,
            &part_aad(part.id, secret.id, part.recipient_id),
            &part.manifest,
        )?;
        let block: Block = serde_json::from_slice(&bytes).map_err(|_| Error::Crypto)?;
        let account: Account = get(&mut *tx, part.recipient_id).await?;
        tx.commit().await?;
        let file = if let Block::File { file, .. } = &block {
            let ciphertext = self.blobs.get(&file.object_key, 15 * 1024 * 1024).await?;
            if self.crypto.digest(&ciphertext) != file.sha256 {
                return Err(Error::Crypto);
            }
            let envelope: Envelope =
                serde_json::from_slice(&ciphertext).map_err(|_| Error::Crypto)?;
            Some(self.crypto.open(
                &key,
                &content_aad(secret.id, &secret.policy, Some(file.id)),
                &envelope,
            )?)
        } else {
            None
        };
        Ok((account, block, file))
    }
    async fn notice_dispatch_valid(
        tx: &mut dyn Transaction,
        job: &Job,
        chat_id: i64,
        now: i64,
    ) -> Result<bool> {
        let account_id = match &job.task {
            Task::GuardianCode { grant_id } => {
                let grant: GuardianGrant = get(tx, *grant_id).await?;
                let secret: Secret = get(tx, grant.secret_id).await?;
                let plan: Plan = get(tx, secret.plan_id).await?;
                let profile: Profile = get(tx, plan.profile_id).await?;
                if job.plan_id != Some(plan.id)
                    || profile.state != "active"
                    || profile.pending_claim.is_some()
                    || matches!(plan.state, PlanState::Deleted | PlanState::DeletionPending)
                    || plan.pending_control.is_some()
                    || secret.pending_control.is_some()
                    || secret.state != SecretState::Provisioning
                    || !secret.policy.guardians.contains(&grant.account_id)
                    || grant.ready
                    || grant.delivery.is_none()
                    || grant.expires_at <= now
                {
                    return Ok(false);
                }
                grant.account_id
            }
            Task::RecoveryCode {
                profile_id,
                account_id,
                selector,
                ..
            } => {
                let profile: Profile = get(tx, *profile_id).await?;
                let plan: Plan = get(tx, job.plan_id.ok_or(Error::InvalidInput)?).await?;
                if plan.profile_id != profile.id
                    || profile.state != "active"
                    || profile.owner_id != *account_id
                    || profile.recovery_selector != *selector
                    || profile.recovery_saved
                    || profile.pending_claim.is_some()
                    || matches!(plan.state, PlanState::Deleted | PlanState::DeletionPending)
                    || plan.pending_control.is_some()
                {
                    return Ok(false);
                }
                *account_id
            }
            Task::ClaimCode { claim_id } => {
                let claim: Claim = get(tx, *claim_id).await?;
                let profile: Profile = get(tx, claim.profile_id).await?;
                let plan: Plan = get(tx, job.plan_id.ok_or(Error::InvalidInput)?).await?;
                if plan.profile_id != profile.id
                    || profile.state != "active"
                    || profile.pending_claim != Some(claim.id)
                    || profile.recovery_selector != claim.old_selector
                    || claim.expires_at <= now
                    || claim.delivery.ciphertext.is_empty()
                    || matches!(plan.state, PlanState::Deleted | PlanState::DeletionPending)
                    || plan.pending_control.is_some()
                {
                    return Ok(false);
                }
                claim.target.id
            }
            Task::GuardianRequest {
                case_id,
                account_id,
            } => {
                let case: CaseRecord = get(tx, *case_id).await?;
                let secret: Secret = get(tx, case.secret_id).await?;
                let plan: Plan = get(tx, case.plan_id).await?;
                let profile: Profile = get(tx, plan.profile_id).await?;
                if job.plan_id != Some(plan.id)
                    || secret.plan_id != plan.id
                    || profile.state != "active"
                    || profile.pending_claim.is_some()
                    || plan.state != PlanState::Active
                    || plan.pending_control.is_some()
                    || secret.pending_control.is_some()
                    || secret.state != SecretState::Armed
                    || !secret.policy.guardians.contains(account_id)
                    || case.case.state != CaseState::Collecting
                    || case.case.plan_epoch != plan.epoch
                    || case.case.secret_epoch != secret.epoch
                    || now >= case.case.created_at.unix_timestamp() + 30 * DAY
                {
                    return Ok(false);
                }
                *account_id
            }
            Task::Notice { account_id, .. } => *account_id,
            _ => return Ok(true),
        };
        Ok(get::<Account>(tx, account_id).await?.chat_id == chat_id)
    }

    pub async fn authorize_dispatch(&self, job: &Job, chat_id: i64) -> Result<bool> {
        let mut tx = self.db.begin().await?;
        if let Some(plan) = job.plan_id {
            tx.lock(Kind::Plan, plan).await?;
        }
        tx.lock(Kind::Job, job.id).await?;
        let Some(raw) = tx.get(Kind::Job, job.id).await? else {
            // A credential acknowledgement may remove its outbox record while a worker waits.
            return Ok(false);
        };
        let mut fresh: Job = serde_json::from_value(raw).map_err(|_| Error::Internal)?;
        let now = tx.now().await?;
        if fresh.lease_token != job.lease_token
            || fresh.state != PartState::Claimed
            || fresh.lease_until <= now
            || fresh.expires_at <= now
        {
            return Ok(false);
        }
        if !matches!(fresh.task, Task::Deliver { .. }) {
            if matches!(
                fresh.task,
                Task::GuardianCode { .. } | Task::RecoveryCode { .. } | Task::ClaimCode { .. }
            ) && let Some(plan_id) = fresh.plan_id
            {
                let plan: Plan = get(&mut *tx, plan_id).await?;
                let profile: Profile = get(&mut *tx, plan.profile_id).await?;
                if plan.pending_control.is_some()
                    || (matches!(fresh.task, Task::GuardianCode { .. })
                        && profile.pending_claim.is_some())
                {
                    // Control intents pause first and finish after journal fsync. Retain the
                    // issuance during that short boundary so recovery does not lose its token.
                    fresh.state = PartState::Queued;
                    fresh.due_at = now + 5;
                    put(&mut *tx, fresh.plan_id, &fresh).await?;
                    tx.commit().await?;
                    return Ok(false);
                }
            }
            let valid = match Self::notice_dispatch_valid(&mut *tx, &fresh, chat_id, now).await {
                Ok(valid) => valid,
                Err(Error::NotFound) => false,
                Err(error) => return Err(error),
            };
            if !valid {
                fresh.state = PartState::Cancelled;
                put(&mut *tx, fresh.plan_id, &fresh).await?;
                tx.commit().await?;
                return Ok(false);
            }
        }
        if !tx.rate_limit("outbound:global", 20, 1).await?
            || !tx.rate_limit(&format!("outbound:{chat_id}"), 1, 1).await?
        {
            fresh.state = PartState::Queued;
            fresh.due_at = now + 1;
            put(&mut *tx, fresh.plan_id, &fresh).await?;
            tx.commit().await?;
            return Ok(false);
        }
        if let Task::Deliver { part_id, case_id } = fresh.task {
            let mut part: DeliveryPart = get(&mut *tx, part_id).await?;
            let c: CaseRecord = get(&mut *tx, case_id).await?;
            let secret: Secret = get(&mut *tx, c.secret_id).await?;
            let plan: Plan = get(&mut *tx, c.plan_id).await?;
            if part.secret_id != secret.id
                || !secret.policy.recipients.contains(&part.recipient_id)
                || get::<Account>(&mut *tx, part.recipient_id).await?.chat_id != chat_id
                || plan.pending_control.is_some()
                || secret.pending_control.is_some()
                || !matches!(
                    secret.state,
                    SecretState::Armed | SecretState::Partial | SecretState::NeedsAttention
                )
                || !matches!(
                    part.state,
                    PartState::Queued | PartState::RetryableFailed | PartState::Claimed
                )
                || !c.case.may_dispatch(
                    plan.state,
                    plan.epoch,
                    secret.epoch,
                    &secret.policy,
                    timestamp(now)?,
                    plan.hold_until > now,
                )
                || !tx.operational_status(Some(plan.id)).await?.ready
            {
                return Ok(false);
            }
            if list::<DeliveryPart>(&mut *tx, Some(secret.id))
                .await?
                .iter()
                .any(|p| {
                    p.recipient_id == part.recipient_id
                        && p.index < part.index
                        && p.state != PartState::Sent
                })
            {
                fresh.state = PartState::Queued;
                fresh.due_at = now + 5;
                put(&mut *tx, fresh.plan_id, &fresh).await?;
                tx.commit().await?;
                return Ok(false);
            }
            part.state = PartState::Dispatching;
            part.last_attempt = Some(fresh.lease_token);
            put(&mut *tx, Some(secret.id), &part).await?;
        }
        fresh.state = PartState::Dispatching;
        put(&mut *tx, fresh.plan_id, &fresh).await?;
        put(
            &mut *tx,
            Some(fresh.id),
            &Attempt {
                id: fresh.lease_token,
                job_id: fresh.id,
                state: PartState::Dispatching,
                at: now,
                message_id: None,
            },
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }
    pub async fn finish_job(&self, id: Id, token: Id, result: SendResult) -> Result<()> {
        let mut tx = self.db.begin().await?;
        let Some(raw) = tx.get(Kind::Job, id).await? else {
            return Ok(());
        };
        let initial: Job = serde_json::from_value(raw).map_err(|_| Error::Internal)?;
        if let Some(plan) = initial.plan_id {
            tx.lock(Kind::Plan, plan).await?;
        }
        tx.lock(Kind::Job, id).await?;
        let Some(raw) = tx.get(Kind::Job, id).await? else {
            return Ok(());
        };
        let mut job: Job = serde_json::from_value(raw).map_err(|_| Error::Internal)?;
        let now = tx.now().await?;
        let (state, message_id) = match result {
            SendResult::Sent(id) => (PartState::Sent, Some(id)),
            SendResult::RetryAfter(_) => (PartState::RetryableFailed, None),
            SendResult::Permanent => (PartState::PermanentFailed, None),
            SendResult::Unknown => (PartState::Unknown, None),
        };
        if job.lease_token == token
            && matches!(
                job.task,
                Task::CleanupMessage { .. } | Task::DeleteObject { .. }
            )
            && matches!(state, PartState::Sent | PartState::PermanentFailed)
        {
            if state == PartState::PermanentFailed {
                manual_cleanup_notice(&mut *tx, &job, now).await?;
            }
            for attempt in list::<Attempt>(&mut *tx, Some(id)).await? {
                tx.remove(Kind::Attempt, attempt.id).await?;
            }
            tx.remove(Kind::Job, id).await?;
            return tx.commit().await;
        }
        if let Some(raw) = tx.get(Kind::Attempt, token).await? {
            let previous: Attempt = serde_json::from_value(raw).map_err(|_| Error::Internal)?;
            if previous.job_id != id {
                return Err(RuleError::AccessDenied.into());
            }
            // A lease reaper can race a late successful HTTP response. Confirmed delivery
            // is monotonic: Unknown (or a duplicate completion) cannot erase that evidence.
            if previous.state == PartState::Sent {
                tx.commit().await?;
                return Ok(());
            }
        }
        put(
            &mut *tx,
            Some(id),
            &Attempt {
                id: token,
                job_id: id,
                state,
                at: now,
                message_id,
            },
        )
        .await?;
        if job.lease_token != token {
            tx.commit().await?;
            return Ok(());
        }
        job.state = state;
        job.message_id = message_id;
        if let SendResult::RetryAfter(seconds) = result {
            job.due_at = now + seconds.clamp(1, 3600);
        }
        if let Task::Deliver { part_id, case_id } = job.task
            && let Some(raw) = tx.get(Kind::DeliveryPart, part_id).await?
        {
            let mut part: DeliveryPart =
                serde_json::from_value(raw).map_err(|_| Error::Internal)?;
            let current_attempt = part.last_attempt == Some(token);
            if current_attempt {
                part.state = state;
                put(&mut *tx, Some(part.secret_id), &part).await?;
            }
            let all = list::<DeliveryPart>(&mut *tx, Some(part.secret_id)).await?;
            let mut secret: Secret = get(&mut *tx, part.secret_id).await?;
            let mut c: CaseRecord = get(&mut *tx, case_id).await?;
            let plan: Plan = get(&mut *tx, secret.plan_id).await?;
            // A late HTTP result remains delivery evidence, but cannot change a
            // stopped scope, a newer case, or a superseding delivery attempt.
            let current_release = current_attempt
                && secret.last_case == Some(case_id)
                && secret.pending_control.is_none()
                && plan.pending_control.is_none()
                && plan.state == PlanState::Active
                && c.case.plan_epoch == plan.epoch
                && c.case.secret_epoch == secret.epoch
                && matches!(c.case.state, CaseState::Delivering | CaseState::Partial)
                && matches!(
                    secret.state,
                    SecretState::Armed | SecretState::Partial | SecretState::NeedsAttention
                );
            if current_release {
                if matches!(state, PartState::Unknown | PartState::PermanentFailed) {
                    secret.state = if all.iter().any(|p| p.state == PartState::Sent) {
                        SecretState::Partial
                    } else {
                        SecretState::NeedsAttention
                    };
                    put(&mut *tx, Some(secret.plan_id), &secret).await?;
                    notice(
                        &mut *tx,
                        secret.plan_id,
                        part.recipient_id,
                        "needs-attention",
                        now,
                    )
                    .await?;
                }
                if all.iter().all(|p| p.state == PartState::Sent) {
                    secret.state = SecretState::Delivered;
                    put(&mut *tx, Some(secret.plan_id), &secret).await?;
                    c.case.state = CaseState::Complete;
                    put(&mut *tx, Some(secret.id), &c).await?;
                    for sub in list::<Submission>(&mut *tx, Some(case_id)).await? {
                        tx.remove(Kind::Submission, sub.id).await?;
                    }
                }
            }
        }
        put(&mut *tx, job.plan_id, &job).await?;
        tx.commit().await
    }

    pub async fn retry_delivery(&self, actor: Id, part_id: Id) -> Result<()> {
        self.limit(&format!("delivery-retry:{actor}:{part_id}"), 3, DAY)
            .await?;
        let mut tx = self.db.begin().await?;
        let initial: DeliveryPart = get(&mut *tx, part_id).await?;
        let s: Secret = get(&mut *tx, initial.secret_id).await?;
        tx.lock(Kind::Plan, s.plan_id).await?;
        let mut part: DeliveryPart = get(&mut *tx, part_id).await?;
        let s: Secret = get(&mut *tx, part.secret_id).await?;
        let plan: Plan = get(&mut *tx, s.plan_id).await?;
        let case: CaseRecord = get(&mut *tx, s.last_case.ok_or(RuleError::StaleAction)?).await?;
        let now = tx.now().await?;
        if part.recipient_id != actor
            || !matches!(
                part.state,
                PartState::Unknown | PartState::PermanentFailed | PartState::RetryableFailed
            )
            || !case.case.may_dispatch(
                plan.state,
                plan.epoch,
                s.epoch,
                &s.policy,
                timestamp(now)?,
                plan.hold_until > now,
            )
            || case.started_delivery.is_none_or(|at| now >= at + 7 * DAY)
            || !tx.operational_status(Some(plan.id)).await?.ready
        {
            return Err(RuleError::AccessDenied.into());
        }
        part.state = PartState::Queued;
        put(&mut *tx, Some(s.id), &part).await?;
        enqueue(
            &mut *tx,
            Some(plan.id),
            Task::Deliver {
                part_id,
                case_id: case.id,
            },
            now,
            case.started_delivery.unwrap_or(now) + 7 * DAY,
            3,
        )
        .await?;
        tx.commit().await
    }
}

#[derive(Clone, Copy)]
pub enum SendResult {
    Sent(i64),
    RetryAfter(i64),
    Permanent,
    Unknown,
}
