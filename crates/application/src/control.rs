use crate::*;
use domain::{DAY, Id, PlanState, RuleError, SecretState};

fn secret_target(control: &Control) -> Option<Id> {
    match control {
        Control::StopSecret { secret_id }
        | Control::RearmSecret { secret_id }
        | Control::DeleteSecret { secret_id } => Some(*secret_id),
        _ => None,
    }
}

async fn tombstone(
    tx: &mut dyn Transaction,
    id: Id,
    scope: DeletedScope,
    operation: Id,
) -> Result<()> {
    put(
        tx,
        None,
        &DeletionTombstone {
            id,
            scope,
            journal_operation_id: operation,
        },
    )
    .await
}

async fn remove_scope(tx: &mut dyn Transaction, kind: Kind, scope: Id) -> Result<()> {
    let mut after = None;
    loop {
        let records = tx.page(kind, Some(scope), after, 200).await?;
        if records.is_empty() {
            break;
        }
        for raw in records {
            let id: Id = serde_json::from_value(raw.get("id").cloned().ok_or(Error::Internal)?)
                .map_err(|_| Error::Internal)?;
            after = Some(id);
            tx.remove(kind, id).await?;
        }
    }
    Ok(())
}

pub(crate) async fn remove_draft(tx: &mut dyn Transaction, draft: &Draft) -> Result<()> {
    let now = tx.now().await?;
    for (chat_id, message_id) in &draft.sources {
        if draft.cleanup_scheduled.contains(&(*chat_id, *message_id)) {
            continue;
        }
        if let Some(account) = find::<Account>(tx, "chat_id", &chat_id.to_string())
            .await?
            .into_iter()
            .next()
        {
            enqueue(
                tx,
                Some(draft.plan_id),
                Task::CleanupMessage {
                    chat_id: *chat_id,
                    message_id: *message_id,
                    sent_at: draft.created_at,
                    account_id: account.id,
                },
                now,
                now + DAY,
                0,
            )
            .await?;
        }
    }
    for mut object in find::<FileObject>(tx, "draft_id", &draft.id.to_string()).await? {
        if object.draft_id == draft.id && matches!(object.state.as_str(), "draft" | "pending") {
            object.state = "gc".into();
            object.due_at = now;
            put(tx, Some(draft.plan_id), &object).await?;
        }
    }
    let mut after = None;
    loop {
        let records = tx.page(Kind::Job, Some(draft.plan_id), after, 200).await?;
        if records.is_empty() {
            break;
        }
        for raw in records {
            let job: Job = serde_json::from_value(raw).map_err(|_| Error::Internal)?;
            after = Some(job.id);
            if matches!(job.task, Task::DownloadFile { draft_id, .. } if draft_id == draft.id) {
                remove_scope(tx, Kind::Attempt, job.id).await?;
                tx.remove(Kind::Job, job.id).await?;
            }
        }
    }
    tx.remove(Kind::PrivateMetadata, draft.id).await?;
    for session in list::<DraftSession>(tx, Some(draft.plan_id)).await? {
        if session.dialog.draft_id == Some(draft.id) {
            tx.remove(Kind::DraftSession, session.id).await?;
        }
    }
    tx.remove(Kind::Draft, draft.id).await
}

async fn purge_plan_content(
    tx: &mut dyn Transaction,
    plan_id: Id,
    only_secret: Option<Id>,
    intent: &ControlIntent,
) -> Result<()> {
    let now = tx.now().await?;
    let mut affected = std::collections::BTreeSet::new();
    let mut secret_ids = std::collections::BTreeSet::new();
    for secret in list::<Secret>(tx, Some(plan_id)).await? {
        if only_secret.is_some_and(|id| id != secret.id) {
            continue;
        }
        secret_ids.insert(secret.id);
        affected.insert(secret.id);
        for case in list::<CaseRecord>(tx, Some(secret.id)).await? {
            affected.insert(case.id);
            remove_scope(tx, Kind::Submission, case.id).await?;
            tx.remove(Kind::Case, case.id).await?;
        }
        for grant in list::<GuardianGrant>(tx, Some(secret.id)).await? {
            affected.insert(grant.id);
            tx.remove(Kind::GuardianGrant, grant.id).await?;
        }
        for part in list::<DeliveryPart>(tx, Some(secret.id)).await? {
            affected.insert(part.id);
            tx.remove(Kind::DeliveryPart, part.id).await?;
        }
        tx.remove(Kind::Secret, secret.id).await?;
        tx.remove(Kind::PrivateMetadata, secret.id).await?;
        tombstone(tx, secret.id, DeletedScope::Secret, intent.id).await?;
    }
    // A pre-Save snapshot can contain a readable draft while the newer journal only
    // knows its eventual secret ID. Prefer losing an unfinished restored draft to
    // exposing a copy of content that has since been saved and deleted.
    let discard_restored_drafts = only_secret.is_some() && secret_ids.is_empty();
    if let Some(id) = only_secret {
        secret_ids.insert(id);
        affected.insert(id);
        tombstone(tx, id, DeletedScope::Secret, intent.id).await?;
    }
    let mut drafts = std::collections::BTreeSet::new();
    for draft in list::<Draft>(tx, Some(plan_id)).await? {
        if only_secret.is_none()
            || (discard_restored_drafts && draft.saved_secret.is_none())
            || draft
                .saved_secret
                .is_some_and(|id| secret_ids.contains(&id))
        {
            drafts.insert(draft.id);
            affected.insert(draft.id);
            remove_draft(tx, &draft).await?;
        }
    }
    for cancellation in list::<Cancellation>(tx, Some(plan_id)).await? {
        if only_secret.is_none()
            || cancellation
                .secrets
                .iter()
                .any(|id| secret_ids.contains(id))
        {
            affected.insert(cancellation.id);
            tx.remove(Kind::Cancellation, cancellation.id).await?;
        }
    }
    for action in list::<Action>(tx, Some(plan_id)).await? {
        if only_secret.is_none() || action.target.is_some_and(|id| affected.contains(&id)) {
            tx.remove(Kind::Action, action.id).await?;
        }
    }
    for dialog in list::<Dialog>(tx, Some(plan_id)).await? {
        if only_secret.is_none()
            || dialog.draft_id.is_some_and(|id| affected.contains(&id))
            || dialog.case_id.is_some_and(|id| affected.contains(&id))
        {
            tx.remove(Kind::Dialog, dialog.id).await?;
        }
    }
    for job in list::<Job>(tx, Some(plan_id)).await? {
        let applies = only_secret.is_none()
            || match &job.task {
                Task::Deliver { part_id, case_id } => {
                    affected.contains(part_id) || affected.contains(case_id)
                }
                Task::GuardianCode { grant_id } => affected.contains(grant_id),
                Task::GuardianRequest { case_id, .. } => affected.contains(case_id),
                Task::DownloadFile { draft_id, .. } => drafts.contains(draft_id),
                _ => false,
            };
        let actionable_cleanup = matches!(
            job.task,
            Task::CleanupMessage { .. } | Task::DeleteObject { .. }
        ) && matches!(
            job.state,
            domain::PartState::Queued
                | domain::PartState::Claimed
                | domain::PartState::Dispatching
                | domain::PartState::RetryableFailed
        ) && job.expires_at > now;
        if applies && !actionable_cleanup {
            remove_scope(tx, Kind::Attempt, job.id).await?;
            tx.remove(Kind::Job, job.id).await?;
        }
    }
    for mut object in list::<FileObject>(tx, Some(plan_id)).await? {
        if only_secret.is_none()
            || secret_ids.contains(&object.operation_id)
            || drafts.contains(&object.draft_id)
        {
            object.state = "gc".into();
            object.due_at = now;
            put(tx, Some(plan_id), &object).await?;
        }
    }
    for previous in list::<ControlIntent>(tx, Some(plan_id)).await? {
        if only_secret.is_none()
            || secret_target(&previous.operation).is_some_and(|id| secret_ids.contains(&id))
        {
            tx.remove(Kind::ControlIntent, previous.id).await?;
        }
    }
    if only_secret.is_none() {
        remove_scope(tx, Kind::PrivateMetadata, plan_id).await?;
        remove_scope(tx, Kind::ContactState, plan_id).await?;
        remove_scope(tx, Kind::InvitationState, plan_id).await?;
        remove_scope(tx, Kind::DraftSession, plan_id).await?;
        remove_scope(tx, Kind::Participant, plan_id).await?;
        remove_scope(tx, Kind::Invitation, plan_id).await?;
        tx.remove(Kind::Plan, plan_id).await?;
        tombstone(tx, plan_id, DeletedScope::Plan, intent.id).await?;
    }
    Ok(())
}

impl Engine {
    pub async fn control(&self, actor: Id, plan_id: Id, id: Id, operation: Control) -> Result<()> {
        if matches!(
            operation,
            Control::RecoveryBegin { .. } | Control::RecoveryComplete { .. }
        ) {
            return Err(RuleError::AccessDenied.into());
        }
        let mut tx = self.db.begin().await?;
        let (profile, plan) = Self::owner(&mut *tx, actor, plan_id).await?;
        if let Some(raw) = tx.get(Kind::ControlIntent, id).await? {
            let existing: ControlIntent =
                serde_json::from_value(raw).map_err(|_| Error::Internal)?;
            if existing.plan_id != plan_id
                || serde_json::to_value(&existing.operation).map_err(|_| Error::Internal)?
                    != serde_json::to_value(&operation).map_err(|_| Error::Internal)?
            {
                return Err(RuleError::AccessDenied.into());
            }
            tx.commit().await?;
            if !existing.applied {
                self.journal.append(&existing).await?;
                self.apply_control(&existing, false).await?;
            }
            return Ok(());
        }
        if matches!(operation, Control::Rearm | Control::RearmSecret { .. })
            && plan.pending_control.is_some_and(|pending| pending != id)
        {
            return Err(RuleError::NotReady.into());
        }
        if matches!(operation, Control::Rearm | Control::RearmSecret { .. })
            && (profile.pending_claim.is_some() || !profile.recovery_saved)
        {
            return Err(RuleError::NotReady.into());
        }
        if matches!(operation, Control::Rearm)
            && !list::<Secret>(&mut *tx, Some(plan.id))
                .await?
                .iter()
                .any(|s| {
                    matches!(
                        s.state,
                        SecretState::Armed | SecretState::Partial | SecretState::NeedsAttention
                    )
                })
        {
            return Err(RuleError::NotReady.into());
        }
        if let Control::RearmSecret { secret_id } = operation {
            let secret: Secret = get(&mut *tx, secret_id).await?;
            let grants = list::<GuardianGrant>(&mut *tx, Some(secret_id)).await?;
            if secret.plan_id != plan.id
                || secret.state != SecretState::Paused
                || secret.pending_control.is_some()
                || grants.len() != secret.policy.guardians.len()
                || grants.iter().any(|g| !g.ready)
            {
                return Err(RuleError::NotReady.into());
            }
        }
        let intent = self
            .stage_control(&mut *tx, profile, plan, id, operation)
            .await?;
        tx.commit().await?;
        self.journal.append(&intent).await?;
        self.apply_control(&intent, false).await
    }
    pub(crate) async fn stage_control(
        &self,
        tx: &mut dyn Transaction,
        profile: Profile,
        mut plan: Plan,
        id: Id,
        operation: Control,
    ) -> Result<ControlIntent> {
        if let Some(raw) = tx.get(Kind::ControlIntent, id).await? {
            let mut existing: ControlIntent =
                serde_json::from_value(raw).map_err(|_| Error::Internal)?;
            if existing.plan_id != plan.id
                || serde_json::to_value(&existing.operation).map_err(|_| Error::Internal)?
                    != serde_json::to_value(&operation).map_err(|_| Error::Internal)?
            {
                return Err(RuleError::AccessDenied.into());
            }
            existing.applied = false;
            return Ok(existing);
        }
        let now = tx.now().await?;
        let previous_state = plan.state;
        let secret_epoch = if let Some(secret_id) = secret_target(&operation) {
            let mut s: Secret = get(tx, secret_id).await?;
            if s.plan_id != plan.id || s.state == SecretState::Deleted {
                return Err(RuleError::AccessDenied.into());
            }
            if matches!(operation, Control::RearmSecret { .. }) && s.state == SecretState::Delivered
            {
                return Err(RuleError::InvalidState.into());
            }
            s.epoch += 1;
            s.pending_control = Some(id);
            if s.state != SecretState::Delivered {
                s.state = SecretState::Paused;
            }
            put(tx, Some(plan.id), &s).await?;
            Some(s.epoch)
        } else {
            plan.epoch += 1;
            plan.state = PlanState::Paused;
            plan.pending_control = Some(id);
            put(tx, Some(profile.id), &plan).await?;
            None
        };
        let intent = ControlIntent {
            id,
            profile_id: profile.id,
            plan_id: plan.id,
            epoch: plan.epoch,
            owner_epoch: profile.owner_epoch,
            at: now,
            previous_state,
            operation,
            applied: false,
            secret_epoch,
        };
        put(tx, Some(plan.id), &intent).await?;
        Ok(intent)
    }
    /// Called before polling or workers. Missing or invalid journal data is a startup failure.
    pub async fn replay_controls(&self) -> Result<()> {
        let entries = self.journal.read().await?;
        for intent in &entries {
            self.apply_control(intent, true).await?;
        }
        let mut after = None;
        loop {
            let mut tx = self.db.begin().await?;
            let page = tx.page(Kind::ControlIntent, None, after, 200).await?;
            tx.commit().await?;
            if page.is_empty() {
                break;
            }
            for raw in page {
                let intent: ControlIntent =
                    serde_json::from_value(raw).map_err(|_| Error::Internal)?;
                after = Some(intent.id);
                if !intent.applied {
                    self.journal.append(&intent).await?;
                    self.apply_control(&intent, false).await?;
                }
            }
        }
        self.discard_unfinished_drafts().await
    }

    async fn discard_unfinished_drafts(&self) -> Result<()> {
        let mut after = None;
        loop {
            let mut tx = self.db.begin().await?;
            let page = tx.page(Kind::Plan, None, after, 200).await?;
            tx.commit().await?;
            if page.is_empty() {
                break;
            }
            for raw in page {
                let plan: Plan = serde_json::from_value(raw).map_err(|_| Error::Internal)?;
                after = Some(plan.id);
                let mut tx = self.db.begin().await?;
                tx.lock(Kind::Plan, plan.id).await?;
                if tx.get(Kind::Plan, plan.id).await?.is_none() {
                    continue;
                }
                let now = tx.now().await?;
                let mut discarded = std::collections::BTreeSet::new();
                for draft in list::<Draft>(&mut *tx, Some(plan.id)).await? {
                    if draft.saved_secret.is_none() {
                        discarded.insert(draft.id);
                        remove_draft(&mut *tx, &draft).await?;
                    }
                }
                if discarded.is_empty() {
                    continue;
                }
                for action in list::<Action>(&mut *tx, Some(plan.id)).await? {
                    if action.target.is_some_and(|id| discarded.contains(&id)) {
                        tx.remove(Kind::Action, action.id).await?;
                    }
                }
                for dialog in list::<Dialog>(&mut *tx, Some(plan.id)).await? {
                    if dialog.draft_id.is_some_and(|id| discarded.contains(&id)) {
                        tx.remove(Kind::Dialog, dialog.id).await?;
                    }
                }
                let profile: Profile = get(&mut *tx, plan.profile_id).await?;
                notice(&mut *tx, plan.id, profile.owner_id, "draft-reset", now).await?;
                tx.commit().await?;
            }
        }
        Ok(())
    }
    pub(crate) async fn apply_control(&self, intent: &ControlIntent, replay: bool) -> Result<()> {
        let mut tx = self.db.begin().await?;
        tx.lock(Kind::Plan, intent.plan_id).await?;
        if tx
            .get(Kind::DeletionTombstone, intent.profile_id)
            .await?
            .is_some()
            || tx
                .get(Kind::DeletionTombstone, intent.plan_id)
                .await?
                .is_some()
        {
            return Ok(());
        }
        if let Some(secret_id) = secret_target(&intent.operation)
            && tx.get(Kind::DeletionTombstone, secret_id).await?.is_some()
        {
            return Ok(());
        }
        if tx.get(Kind::Plan, intent.plan_id).await?.is_none() {
            // The current journal may refer to a plan created after the SQL snapshot.
            // Owner transfer and profile deletion still apply to a retained older profile.
            if replay {
                if let Some(raw) = tx.get(Kind::Profile, intent.profile_id).await? {
                    let mut profile: Profile =
                        serde_json::from_value(raw).map_err(|_| Error::Internal)?;
                    if profile.owner_epoch <= intent.owner_epoch {
                        match &intent.operation {
                            Control::RecoveryComplete {
                                target,
                                selector,
                                verifier,
                            } => {
                                tx.admit_recovery_account(
                                    target,
                                    profile.id,
                                    profile.recovery_selector,
                                )
                                .await?;
                                profile.owner_id = target.id;
                                profile.owner_epoch = intent.owner_epoch + 1;
                                profile.recovery_selector = *selector;
                                profile.recovery_hash = verifier.clone();
                                profile.recovery_saved = true;
                                profile.pending_claim = None;
                                put(&mut *tx, None, target).await?;
                                put(&mut *tx, Some(target.id), &profile).await?;
                            }
                            Control::DeleteProfile => {
                                for plan in list::<Plan>(&mut *tx, Some(profile.id)).await? {
                                    tx.lock(Kind::Plan, plan.id).await?;
                                    purge_plan_content(&mut *tx, plan.id, None, intent).await?;
                                }
                                remove_scope(&mut *tx, Kind::Claim, profile.id).await?;
                                tx.remove(Kind::Profile, profile.id).await?;
                                tombstone(&mut *tx, profile.id, DeletedScope::Profile, intent.id)
                                    .await?;
                            }
                            _ => {}
                        }
                    }
                }
                match intent.operation {
                    Control::DeletePlan | Control::DeleteProfile => {
                        tombstone(&mut *tx, intent.plan_id, DeletedScope::Plan, intent.id).await?;
                    }
                    Control::DeleteSecret { secret_id } => {
                        tombstone(&mut *tx, secret_id, DeletedScope::Secret, intent.id).await?;
                    }
                    _ => {}
                }
                tx.commit().await?;
            }
            return Ok(());
        }
        let mut plan: Plan = get(&mut *tx, intent.plan_id).await?;
        let mut profile: Profile = get(&mut *tx, intent.profile_id).await?;
        if let Some(raw) = tx.get(Kind::ControlIntent, intent.id).await? {
            let previous: ControlIntent =
                serde_json::from_value(raw).map_err(|_| Error::Internal)?;
            if previous.applied {
                return Ok(());
            }
        }
        let secret_id = secret_target(&intent.operation);
        if let Some(id) = secret_id {
            if replay && tx.get(Kind::Secret, id).await?.is_none() {
                if matches!(intent.operation, Control::DeleteSecret { .. }) {
                    purge_plan_content(&mut *tx, plan.id, Some(id), intent).await?;
                    tx.commit().await?;
                }
                return Ok(());
            }
            let mut secret: Secret = get(&mut *tx, id).await?;
            let epoch = intent.secret_epoch.ok_or(Error::Internal)?;
            if secret.epoch > epoch || (!replay && secret.pending_control != Some(intent.id)) {
                return Ok(());
            }
            secret.epoch = epoch;
            secret.pending_control = None;
            secret.state = if matches!(intent.operation, Control::DeleteSecret { .. }) {
                SecretState::Deleted
            } else if matches!(intent.operation, Control::RearmSecret { .. }) {
                let grants = list::<GuardianGrant>(&mut *tx, Some(id)).await?;
                if profile.recovery_saved
                    && profile.pending_claim.is_none()
                    && grants.len() == secret.policy.guardians.len()
                    && grants.iter().all(|g| g.ready)
                {
                    SecretState::Armed
                } else {
                    SecretState::Paused
                }
            } else if secret.state == SecretState::Delivered {
                SecretState::Delivered
            } else {
                SecretState::Paused
            };
            put(&mut *tx, Some(plan.id), &secret).await?;
        } else {
            if plan.epoch > intent.epoch || (!replay && plan.pending_control != Some(intent.id)) {
                return Ok(());
            }
            plan.epoch = intent.epoch;
            plan.pending_control = None;
            match &intent.operation {
                Control::CheckIn => {
                    plan.last_activity = intent.at;
                    plan.next_reminder = intent.at + plan.timing.reminder_seconds;
                    plan.state = if intent.previous_state == PlanState::Active {
                        PlanState::Active
                    } else {
                        PlanState::Paused
                    };
                }
                Control::Rearm => {
                    let ready_secret =
                        list::<Secret>(&mut *tx, Some(plan.id))
                            .await?
                            .iter()
                            .any(|secret| {
                                matches!(
                                    secret.state,
                                    SecretState::Armed
                                        | SecretState::Partial
                                        | SecretState::NeedsAttention
                                )
                            });
                    if profile.pending_claim.is_some() || !profile.recovery_saved || !ready_secret {
                        if !replay {
                            return Err(RuleError::NotReady.into());
                        }
                        // Readiness acknowledgements and short-lived claims can be absent
                        // from an older SQL snapshot. Restoring never guesses permission.
                        plan.state = PlanState::Paused;
                    } else {
                        plan.state = PlanState::Active;
                        plan.last_activity = intent.at;
                        plan.next_reminder = intent.at + plan.timing.reminder_seconds;
                    }
                }
                Control::DeletePlan => plan.state = PlanState::Deleted,
                Control::DeleteProfile => {
                    plan.state = PlanState::Deleted;
                    profile.state = "deleted".into();
                    profile.owner_epoch += 1;
                    profile.recovery_hash.clear();
                    profile.pending_claim = None;
                }
                Control::RecoveryBegin { claim_id } => {
                    plan.state = PlanState::Paused;
                    // A journal may be newer than the restored SQL snapshot. Its short-lived
                    // claim envelope is intentionally not in the journal; keep STOP durable,
                    // but let the still-valid old credential start a fresh claim if it is gone.
                    profile.pending_claim = match tx.get(Kind::Claim, *claim_id).await? {
                        Some(raw) => {
                            let claim: Claim =
                                serde_json::from_value(raw).map_err(|_| Error::Internal)?;
                            (claim.profile_id == profile.id
                                && claim.old_selector == profile.recovery_selector
                                && claim.expires_at > tx.now().await?)
                                .then_some(*claim_id)
                        }
                        None => None,
                    };
                }
                Control::RecoveryComplete {
                    target,
                    selector,
                    verifier,
                } => {
                    tx.admit_recovery_account(target, profile.id, profile.recovery_selector)
                        .await?;
                    plan.state = PlanState::Paused;
                    profile.owner_id = target.id;
                    profile.owner_epoch = intent.owner_epoch + 1;
                    profile.recovery_selector = *selector;
                    profile.recovery_hash = verifier.clone();
                    profile.recovery_saved = true;
                    profile.pending_claim = None;
                    put(&mut *tx, None, target).await?;
                }
                _ => plan.state = PlanState::Paused,
            }
            plan.due_at = plan.last_activity + plan.timing.inactivity_seconds;
            put(&mut *tx, Some(profile.owner_id), &profile).await?;
            put(&mut *tx, Some(profile.id), &plan).await?;
        }
        let delete_all = matches!(
            intent.operation,
            Control::DeletePlan | Control::DeleteProfile
        );
        let delete_one = matches!(intent.operation, Control::DeleteSecret { .. });
        if delete_all || delete_one {
            Self::control_receipt(&mut *tx, intent, profile.owner_id).await?;
            purge_plan_content(&mut *tx, plan.id, secret_id, intent).await?;
            if delete_all {
                remove_scope(&mut *tx, Kind::Claim, profile.id).await?;
                if matches!(intent.operation, Control::DeleteProfile) {
                    // V1 permits one live plan, but purge any restored older plan too.
                    for other in list::<Plan>(&mut *tx, Some(profile.id)).await? {
                        tx.lock(Kind::Plan, other.id).await?;
                        purge_plan_content(&mut *tx, other.id, None, intent).await?;
                    }
                    tx.remove(Kind::Profile, profile.id).await?;
                    tombstone(&mut *tx, profile.id, DeletedScope::Profile, intent.id).await?;
                } else {
                    profile.pending_claim = None;
                    put(&mut *tx, Some(profile.owner_id), &profile).await?;
                }
            }
            return tx.commit().await;
        }
        let secrets = list::<Secret>(&mut *tx, Some(plan.id)).await?;
        if matches!(intent.operation, Control::CheckIn | Control::Rearm)
            && let Some(interval) = secrets
                .iter()
                .filter(|s| s.state != SecretState::Deleted)
                .map(|s| s.policy.timing.reminder_seconds)
                .min()
        {
            plan.next_reminder = intent.at + interval;
            put(&mut *tx, Some(profile.id), &plan).await?;
        }
        let mut affected_parts = std::collections::BTreeSet::new();
        for mut secret in secrets
            .into_iter()
            .filter(|s| secret_id.is_none_or(|id| s.id == id))
        {
            for raw in tx.active_cases(secret.id).await? {
                let mut case: CaseRecord =
                    serde_json::from_value(raw).map_err(|_| Error::Internal)?;
                case.case.cancel();
                case.due_at = i64::MAX;
                put(&mut *tx, Some(secret.id), &case).await?;
                for sub in list::<Submission>(&mut *tx, Some(case.id)).await? {
                    tx.remove(Kind::Submission, sub.id).await?;
                }
            }
            for mut part in list::<DeliveryPart>(&mut *tx, Some(secret.id)).await? {
                affected_parts.insert(part.id);
                part.state = part.state.after_stop();
                put(&mut *tx, Some(secret.id), &part).await?;
            }
            secret.last_case = None;
            if matches!(intent.operation, Control::CheckIn | Control::Rearm)
                && matches!(
                    secret.state,
                    SecretState::Armed | SecretState::Partial | SecretState::NeedsAttention
                )
            {
                secret.state = SecretState::Armed;
                secret.due_at = plan.last_activity + secret.policy.timing.inactivity_seconds;
            } else if matches!(intent.operation, Control::RearmSecret { .. })
                && secret.state == SecretState::Armed
            {
                secret.due_at = intent.at + secret.policy.timing.inactivity_seconds;
            }
            put(&mut *tx, Some(plan.id), &secret).await?;
        }
        for mut job in list::<Job>(&mut *tx, Some(plan.id)).await? {
            let applies = match job.task {
                Task::Deliver { part_id, .. } => {
                    secret_id.is_none() || affected_parts.contains(&part_id)
                }
                Task::GuardianCode { .. } => false,
                Task::GuardianRequest { case_id, .. } => {
                    let case: CaseRecord = get(&mut *tx, case_id).await?;
                    secret_id.is_none_or(|id| case.secret_id == id)
                }
                _ => false,
            };
            if applies
                && !matches!(
                    job.task,
                    Task::CleanupMessage { .. } | Task::DeleteObject { .. }
                )
            {
                job.state = job.state.after_stop();
                put(&mut *tx, Some(plan.id), &job).await?;
            }
        }
        if matches!(intent.operation, Control::RecoveryComplete { .. }) {
            remove_scope(&mut *tx, Kind::DraftSession, plan.id).await?;
            for draft in list::<Draft>(&mut *tx, Some(plan.id)).await? {
                remove_draft(&mut *tx, &draft).await?;
            }
            for action in list::<Action>(&mut *tx, Some(plan.id)).await? {
                tx.remove(Kind::Action, action.id).await?;
            }
            for dialog in list::<Dialog>(&mut *tx, Some(plan.id)).await? {
                tx.remove(Kind::Dialog, dialog.id).await?;
            }
        }
        let mut done = intent.clone();
        done.applied = true;
        put(&mut *tx, Some(plan.id), &done).await?;
        Self::control_receipt(&mut *tx, intent, profile.owner_id).await?;
        tx.commit().await
    }
    pub async fn recover(
        &self,
        actor: Id,
        selector: Id,
        token: &str,
        stop_only: bool,
        operation_id: Id,
    ) -> Result<Option<Id>> {
        self.limit(&format!("recovery:{actor}"), 5, 3600).await?;
        let mut tx = self.db.begin().await?;
        let profile = find::<Profile>(&mut *tx, "recovery_selector", &selector.to_string())
            .await?
            .into_iter()
            .find(|p| p.state == "active")
            .ok_or(Error::InvalidCode)?;
        let target: Account = get(&mut *tx, actor).await?;
        tx.commit().await?;
        // Reserve expensive work only after cheap selector and actor validation.
        self.limit("admission:argon-recovery", 120, 60).await?;
        if !self
            .recovery
            .verify(token, selector, &profile.recovery_hash)
            .await?
        {
            return Err(Error::InvalidCode);
        }
        let mut tx = self.db.begin().await?;
        if let Some(raw) = tx.get(Kind::ControlIntent, operation_id).await? {
            let existing: ControlIntent =
                serde_json::from_value(raw).map_err(|_| Error::Internal)?;
            if existing.profile_id != profile.id {
                return Err(RuleError::AccessDenied.into());
            }
            let claim_id = match existing.operation {
                Control::Stop if stop_only => None,
                Control::RecoveryBegin { claim_id } if !stop_only => {
                    let claim: Claim = get(&mut *tx, claim_id).await?;
                    if claim.target.id != actor {
                        return Err(RuleError::AccessDenied.into());
                    }
                    Some(claim_id)
                }
                _ => return Err(RuleError::AccessDenied.into()),
            };
            tx.commit().await?;
            if !existing.applied {
                self.journal.append(&existing).await?;
                self.apply_control(&existing, false).await?;
            }
            return Ok(claim_id);
        }
        tx.commit().await?;
        let issued = if stop_only {
            None
        } else {
            Some(self.recovery.issue().await?)
        };
        // DeletePlan keeps the profile credential. A verified holder can recover that
        // empty profile through a new paused plan; a tombstoned plan is never reused.
        // This transaction takes only the Profile lock, and ends before the normal
        // Plan-lock flow below, avoiding an inverse Profile -> Plan lock dependency.
        let mut tx = self.db.begin().await?;
        tx.lock(Kind::Profile, profile.id).await?;
        let Some(raw) = tx.get(Kind::Profile, profile.id).await? else {
            return Err(Error::InvalidCode);
        };
        let fresh: Profile = serde_json::from_value(raw).map_err(|_| Error::Internal)?;
        if fresh.state != "active"
            || fresh.recovery_selector != selector
            || fresh.recovery_hash != profile.recovery_hash
            || tx.get(Kind::DeletionTombstone, profile.id).await?.is_some()
        {
            return Err(Error::InvalidCode);
        }
        if list::<Plan>(&mut *tx, Some(profile.id)).await?.is_empty() {
            let now = tx.now().await?;
            let timing = domain::Timing::default();
            let plan = Plan {
                id: Id::new_v4(),
                profile_id: profile.id,
                state: PlanState::Paused,
                epoch: 1,
                last_activity: now,
                due_at: now + timing.inactivity_seconds,
                next_reminder: now + timing.reminder_seconds,
                timing,
                hold_until: 0,
                pending_control: None,
            };
            put(&mut *tx, Some(profile.id), &plan).await?;
        }
        tx.commit().await?;
        let mut tx = self.db.begin().await?;
        let plan = list::<Plan>(&mut *tx, Some(profile.id))
            .await?
            .into_iter()
            .next()
            .ok_or(Error::NotFound)?;
        tx.lock(Kind::Plan, plan.id).await?;
        let plan: Plan = get(&mut *tx, plan.id).await?;
        let fresh: Profile = get(&mut *tx, profile.id).await?;
        if fresh.recovery_selector != selector
            || fresh.recovery_hash != profile.recovery_hash
            || fresh.state != "active"
            || matches!(plan.state, PlanState::Deleted | PlanState::DeletionPending)
        {
            return Err(Error::InvalidCode);
        }
        let mut claim_id = None;
        let operation = if let Some((new_selector, new_token, new_hash)) = issued {
            if let Some(existing) = fresh.pending_claim {
                let claim: Claim = get(&mut *tx, existing).await?;
                let now = tx.now().await?;
                if claim.target.id != actor
                    || claim.expires_at <= now
                    || claim.old_selector != selector
                {
                    return Err(RuleError::InvalidState.into());
                }
                enqueue(
                    &mut *tx,
                    Some(plan.id),
                    Task::ClaimCode { claim_id: existing },
                    now,
                    claim.expires_at,
                    0,
                )
                .await?;
                tx.commit().await?;
                return Ok(Some(existing));
            }
            if fresh.owner_id != actor {
                if find::<Profile>(&mut *tx, "owner_id", &actor.to_string())
                    .await?
                    .iter()
                    .any(|p| p.state != "deleted")
                {
                    return Err(RuleError::AccessDenied.into());
                }
                if list::<Secret>(&mut *tx, Some(plan.id))
                    .await?
                    .iter()
                    .any(|s| {
                        s.state != SecretState::Deleted
                            && (s.policy.guardians.contains(&actor)
                                || s.policy.recipients.contains(&actor))
                    })
                {
                    return Err(RuleError::AccessDenied.into());
                }
            }
            let id = Id::new_v4();
            let now = tx.now().await?;
            let claim = Claim {
                id,
                profile_id: profile.id,
                target,
                old_selector: selector,
                new_selector,
                new_hash,
                delivery: self.crypto.wrap("claim", id, new_token.as_bytes())?,
                expires_at: now + 900,
                rotation: fresh.owner_id == actor,
            };
            put(&mut *tx, Some(profile.id), &claim).await?;
            enqueue(
                &mut *tx,
                Some(plan.id),
                Task::ClaimCode { claim_id: id },
                now,
                now + 900,
                0,
            )
            .await?;
            claim_id = Some(id);
            Control::RecoveryBegin { claim_id: id }
        } else {
            Control::Stop
        };
        let intent = self
            .stage_control(&mut *tx, fresh, plan, operation_id, operation)
            .await?;
        tx.commit().await?;
        self.journal.append(&intent).await?;
        self.apply_control(&intent, false).await?;
        Ok(claim_id)
    }
    pub async fn acknowledge_claim(&self, actor: Id, id: Id, operation_id: Id) -> Result<()> {
        let mut tx = self.db.begin().await?;
        if let Some(raw) = tx.get(Kind::ControlIntent, operation_id).await? {
            let existing: ControlIntent =
                serde_json::from_value(raw).map_err(|_| Error::Internal)?;
            if !matches!(&existing.operation, Control::RecoveryComplete { target, .. } if target.id == actor)
            {
                return Err(RuleError::AccessDenied.into());
            }
            tx.commit().await?;
            if !existing.applied {
                self.journal.append(&existing).await?;
                self.apply_control(&existing, false).await?;
            }
            return Ok(());
        }
        let claim: Claim = get(&mut *tx, id).await?;
        let plan = list::<Plan>(&mut *tx, Some(claim.profile_id))
            .await?
            .into_iter()
            .next()
            .ok_or(Error::NotFound)?;
        tx.lock(Kind::Plan, plan.id).await?;
        let profile: Profile = get(&mut *tx, claim.profile_id).await?;
        let plan: Plan = get(&mut *tx, plan.id).await?;
        if claim.target.id != actor
            || profile.pending_claim != Some(id)
            || profile.recovery_selector != claim.old_selector
            || tx.now().await? >= claim.expires_at
        {
            return Err(RuleError::StaleAction.into());
        }
        if profile.owner_id != actor
            && find::<Profile>(&mut *tx, "owner_id", &actor.to_string())
                .await?
                .iter()
                .any(|p| p.state != "deleted")
        {
            return Err(RuleError::AccessDenied.into());
        }
        let intent = self
            .stage_control(
                &mut *tx,
                profile,
                plan,
                operation_id,
                Control::RecoveryComplete {
                    target: claim.target,
                    selector: claim.new_selector,
                    verifier: claim.new_hash,
                },
            )
            .await?;
        tx.commit().await?;
        self.journal.append(&intent).await?;
        self.apply_control(&intent, false).await?;
        let mut tx = self.db.begin().await?;
        tx.remove(Kind::Claim, id).await?;
        tx.commit().await
    }
    pub async fn rotate_recovery(&self, actor: Id, plan_id: Id, operation_id: Id) -> Result<Id> {
        let mut tx = self.db.begin().await?;
        Self::owner(&mut *tx, actor, plan_id).await?;
        if let Some(raw) = tx.get(Kind::ControlIntent, operation_id).await? {
            let existing: ControlIntent =
                serde_json::from_value(raw).map_err(|_| Error::Internal)?;
            if existing.plan_id != plan_id {
                return Err(RuleError::AccessDenied.into());
            }
            let Control::RecoveryBegin { claim_id } = existing.operation else {
                return Err(RuleError::AccessDenied.into());
            };
            let claim: Claim = get(&mut *tx, claim_id).await?;
            if claim.target.id != actor || !claim.rotation {
                return Err(RuleError::AccessDenied.into());
            }
            tx.commit().await?;
            if !existing.applied {
                self.journal.append(&existing).await?;
                self.apply_control(&existing, false).await?;
            }
            return Ok(claim_id);
        }
        tx.commit().await?;
        self.limit(&format!("recovery-rotate:{actor}"), 5, 3600)
            .await?;
        self.limit("admission:argon-recovery", 120, 60).await?;
        let (selector, token, hash) = self.recovery.issue().await?;
        let mut tx = self.db.begin().await?;
        let (profile, plan) = Self::owner(&mut *tx, actor, plan_id).await?;
        if profile.pending_claim.is_some() {
            return Err(RuleError::InvalidState.into());
        }
        let now = tx.now().await?;
        let id = Id::new_v4();
        let target: Account = get(&mut *tx, actor).await?;
        let claim = Claim {
            id,
            profile_id: profile.id,
            target,
            old_selector: profile.recovery_selector,
            new_selector: selector,
            new_hash: hash,
            delivery: self.crypto.wrap("claim", id, token.as_bytes())?,
            expires_at: now + 900,
            rotation: true,
        };
        put(&mut *tx, Some(profile.id), &claim).await?;
        enqueue(
            &mut *tx,
            Some(plan_id),
            Task::ClaimCode { claim_id: id },
            now,
            now + 900,
            0,
        )
        .await?;
        let intent = self
            .stage_control(
                &mut *tx,
                profile,
                plan,
                operation_id,
                Control::RecoveryBegin { claim_id: id },
            )
            .await?;
        tx.commit().await?;
        self.journal.append(&intent).await?;
        self.apply_control(&intent, false).await?;
        Ok(id)
    }
    pub async fn request_cancellation(
        &self,
        actor: Id,
        plan_id: Id,
        secret_id: Option<Id>,
    ) -> Result<Id> {
        self.limit(&format!("cancel:{actor}:{plan_id}:{secret_id:?}"), 3, DAY)
            .await?;
        let mut tx = self.db.begin().await?;
        tx.lock(Kind::Plan, plan_id).await?;
        let plan: Plan = get(&mut *tx, plan_id).await?;
        let profile: Profile = get(&mut *tx, plan.profile_id).await?;
        let secrets = list::<Secret>(&mut *tx, Some(plan_id))
            .await?
            .into_iter()
            .filter(|s| s.state != SecretState::Deleted && secret_id.is_none_or(|id| s.id == id))
            .collect::<Vec<_>>();
        let members = secrets
            .iter()
            .flat_map(|s| s.policy.guardians.iter().copied())
            .collect::<std::collections::BTreeSet<_>>();
        if !members.contains(&actor) {
            return Err(RuleError::AccessDenied.into());
        }
        let epoch = if let Some(id) = secret_id {
            secrets
                .iter()
                .find(|s| s.id == id)
                .ok_or(Error::NotFound)?
                .epoch
        } else {
            plan.epoch
        };
        let snapshot = secrets.iter().map(|s| s.id).collect();
        let now = tx.now().await?;
        for mut c in list::<Cancellation>(&mut *tx, Some(plan_id)).await? {
            if c.state == "open" && c.secret_id == secret_id {
                if c.expires_at > now
                    && c.epoch == epoch
                    && c.owner_epoch == profile.owner_epoch
                    && c.secrets == snapshot
                    && c.members == members
                {
                    return Ok(c.id);
                }
                c.state = if c.expires_at <= now {
                    "expired"
                } else {
                    "invalidated"
                }
                .into();
                put(&mut *tx, Some(plan_id), &c).await?;
            }
        }
        let c = Cancellation {
            id: Id::new_v4(),
            plan_id,
            secret_id,
            epoch,
            owner_epoch: profile.owner_epoch,
            members,
            votes: Default::default(),
            secrets: snapshot,
            state: "open".into(),
            expires_at: now + 7 * DAY,
        };
        put(&mut *tx, Some(plan_id), &c).await?;
        for member in &c.members {
            notice(&mut *tx, plan_id, *member, "cancel-vote-request", now).await?;
        }
        tx.commit().await?;
        Ok(c.id)
    }
    pub async fn vote_cancel(&self, actor: Id, id: Id) -> Result<bool> {
        let mut tx = self.db.begin().await?;
        let c: Cancellation = get(&mut *tx, id).await?;
        tx.lock(Kind::Plan, c.plan_id).await?;
        let mut c: Cancellation = get(&mut *tx, id).await?;
        let plan: Plan = get(&mut *tx, c.plan_id).await?;
        let profile: Profile = get(&mut *tx, plan.profile_id).await?;
        let epoch = if let Some(id) = c.secret_id {
            get::<Secret>(&mut *tx, id).await?.epoch
        } else {
            plan.epoch
        };
        if c.state != "open"
            || c.expires_at <= tx.now().await?
            || !c.members.contains(&actor)
            || c.epoch != epoch
            || c.owner_epoch != profile.owner_epoch
        {
            return Err(RuleError::StaleAction.into());
        }
        let current = list::<Secret>(&mut *tx, Some(plan.id))
            .await?
            .iter()
            .filter(|s| s.state != SecretState::Deleted && c.secret_id.is_none_or(|id| id == s.id))
            .map(|s| s.id)
            .collect::<std::collections::BTreeSet<_>>();
        if current != c.secrets {
            return Err(RuleError::StaleAction.into());
        }
        c.votes.insert(actor);
        let unanimous = c.votes == c.members;
        let intent = if unanimous {
            c.state = "approved".into();
            let operation = if let Some(secret_id) = c.secret_id {
                Control::StopSecret { secret_id }
            } else {
                Control::Stop
            };
            Some(
                self.stage_control(&mut *tx, profile, plan, Id::new_v4(), operation)
                    .await?,
            )
        } else {
            None
        };
        put(&mut *tx, Some(c.plan_id), &c).await?;
        tx.commit().await?;
        if let Some(intent) = intent {
            self.journal.append(&intent).await?;
            self.apply_control(&intent, false).await?;
        }
        Ok(unanimous)
    }
}
