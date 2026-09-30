use crate::*;
use domain::{DAY, Id, PlanState, RuleError, SecretState};

impl Engine {
    pub(crate) async fn record_receipt(
        tx: &mut dyn Transaction,
        receipt: OperationReceipt,
    ) -> Result<()> {
        let mut previous = list::<OperationReceipt>(tx, Some(receipt.actor_id)).await?;
        previous.sort_by_key(|r| (r.at, r.id));
        let now = tx.now().await?;
        let excess = previous.len().saturating_sub(99);
        for (index, old) in previous.into_iter().enumerate() {
            if (index < excess || old.due_at <= now) && old.id != receipt.id {
                tx.remove(Kind::OperationReceipt, old.id).await?;
            }
        }
        put(tx, Some(receipt.actor_id), &receipt).await
    }

    pub(crate) async fn control_receipt(
        tx: &mut dyn Transaction,
        intent: &ControlIntent,
        actor: Id,
    ) -> Result<()> {
        let (operation, secret_id) = match intent.operation {
            Control::CheckIn => (OperationKind::CheckIn, None),
            Control::Stop => (OperationKind::Stop, None),
            Control::Rearm => (OperationKind::Resume, None),
            Control::StopSecret { secret_id } => (OperationKind::StopSecret, Some(secret_id)),
            Control::RearmSecret { secret_id } => (OperationKind::ResumeSecret, Some(secret_id)),
            Control::DeleteSecret { secret_id } => (OperationKind::DeleteSecret, Some(secret_id)),
            Control::DeletePlan => (OperationKind::DeletePlan, None),
            Control::DeleteProfile => (OperationKind::DeleteProfile, None),
            Control::RecoveryBegin { .. } => (OperationKind::RecoveryStarted, None),
            Control::RecoveryComplete { .. } => (OperationKind::RecoveryCompleted, None),
        };
        let mut pending_objects = std::collections::BTreeSet::new();
        if matches!(
            operation,
            OperationKind::DeleteSecret | OperationKind::DeletePlan | OperationKind::DeleteProfile
        ) {
            let drafts = list::<Draft>(tx, Some(intent.plan_id)).await?;
            for object in list::<FileObject>(tx, Some(intent.plan_id)).await? {
                if secret_id.is_none_or(|id| {
                    object.operation_id == id
                        || drafts
                            .iter()
                            .any(|d| d.id == object.draft_id && d.saved_secret == Some(id))
                }) {
                    pending_objects.insert(object.id);
                }
            }
        }
        let status = if pending_objects.is_empty() {
            ReceiptStatus::Completed
        } else {
            ReceiptStatus::CleanupPending
        };
        Self::record_receipt(
            tx,
            OperationReceipt {
                id: intent.id,
                actor_id: actor,
                plan_id: Some(intent.plan_id),
                secret_id,
                operation,
                status,
                at: intent.at,
                due_at: intent.at + 7 * DAY,
                pending_objects,
            },
        )
        .await
    }

    pub async fn operation_receipt(&self, actor: Id, id: Id) -> Result<OperationReceipt> {
        let mut tx = self.db.begin().await?;
        let mut receipt: OperationReceipt = get(&mut *tx, id).await?;
        if receipt.actor_id != actor {
            return Err(RuleError::AccessDenied.into());
        }
        if receipt.due_at <= tx.now().await? {
            return Err(RuleError::Expired.into());
        }
        Self::refresh_receipt(&mut *tx, &mut receipt).await?;
        tx.commit().await?;
        Ok(receipt)
    }

    pub(crate) async fn refresh_receipt(
        tx: &mut dyn Transaction,
        receipt: &mut OperationReceipt,
    ) -> Result<()> {
        if receipt.status == ReceiptStatus::CleanupPending {
            let mut pending = std::collections::BTreeSet::new();
            for id in &receipt.pending_objects {
                if tx.get(Kind::FileObject, *id).await?.is_some() {
                    pending.insert(*id);
                }
            }
            receipt.pending_objects = pending;
            if receipt.pending_objects.is_empty() {
                receipt.status = ReceiptStatus::Completed;
            }
            put(tx, Some(receipt.actor_id), receipt).await?;
        }
        Ok(())
    }

    pub async fn prepare_deletion(
        &self,
        actor: Id,
        plan: Id,
        secret: Option<Id>,
        request_id: Id,
    ) -> Result<DeletionRequest> {
        let mut tx = self.db.begin().await?;
        if let Some(raw) = tx.get(Kind::DeletionRequest, request_id).await? {
            let request: DeletionRequest =
                serde_json::from_value(raw).map_err(|_| Error::Internal)?;
            if request.actor_id != actor || request.plan_id != plan || request.secret_id != secret {
                return Err(RuleError::AccessDenied.into());
            }
            if request.expires_at <= tx.now().await? {
                return Err(RuleError::Expired.into());
            }
            return Ok(request);
        }
        tx.commit().await?;
        self.control(
            actor,
            plan,
            request_id,
            secret
                .map(|secret_id| Control::StopSecret { secret_id })
                .unwrap_or(Control::Stop),
        )
        .await?;
        let mut tx = self.db.begin().await?;
        let (profile, plan_record) = Self::owner(&mut *tx, actor, plan).await?;
        let secret_epoch = if let Some(id) = secret {
            let record: Secret = get(&mut *tx, id).await?;
            if record.plan_id != plan
                || !matches!(record.state, SecretState::Paused | SecretState::Delivered)
                || record.pending_control.is_some()
            {
                return Err(RuleError::StaleAction.into());
            }
            Some(record.epoch)
        } else {
            if plan_record.state != PlanState::Paused || plan_record.pending_control.is_some() {
                return Err(RuleError::StaleAction.into());
            }
            None
        };
        let request = DeletionRequest {
            id: request_id,
            actor_id: actor,
            plan_id: plan,
            secret_id: secret,
            owner_epoch: profile.owner_epoch,
            plan_epoch: plan_record.epoch,
            secret_epoch,
            expires_at: tx.now().await? + 300,
            completed_operation: None,
        };
        put(&mut *tx, Some(plan), &request).await?;
        tx.commit().await?;
        Ok(request)
    }

    pub async fn confirm_deletion(
        &self,
        actor: Id,
        request_id: Id,
        operation_id: Id,
    ) -> Result<OperationReceipt> {
        let mut tx = self.db.begin().await?;
        if let Some(raw) = tx.get(Kind::OperationReceipt, operation_id).await? {
            let receipt: OperationReceipt =
                serde_json::from_value(raw).map_err(|_| Error::Internal)?;
            if receipt.actor_id != actor
                || !matches!(
                    receipt.operation,
                    OperationKind::DeleteSecret | OperationKind::DeleteProfile
                )
            {
                return Err(RuleError::AccessDenied.into());
            }
            tx.commit().await?;
            return self.operation_receipt(actor, operation_id).await;
        }
        let request: DeletionRequest = get(&mut *tx, request_id).await?;
        if request.actor_id != actor {
            return Err(RuleError::AccessDenied.into());
        }
        if let Some(id) = request.completed_operation {
            let pending = match tx.get(Kind::ControlIntent, id).await? {
                Some(raw) => Some(
                    serde_json::from_value::<ControlIntent>(raw).map_err(|_| Error::Internal)?,
                ),
                None => None,
            };
            tx.commit().await?;
            if let Some(intent) = pending
                && !intent.applied
            {
                self.journal.append(&intent).await?;
                self.apply_control(&intent, false).await?;
            }
            return self.operation_receipt(actor, id).await;
        }
        let (profile, plan) = Self::owner(&mut *tx, actor, request.plan_id).await?;
        if request.expires_at <= tx.now().await? {
            return Err(RuleError::Expired.into());
        }
        if profile.owner_epoch != request.owner_epoch
            || plan.epoch != request.plan_epoch
            || plan.pending_control.is_some()
        {
            return Err(RuleError::StaleAction.into());
        }
        if let Some(id) = request.secret_id {
            let secret: Secret = get(&mut *tx, id).await?;
            if secret.epoch != request.secret_epoch.ok_or(Error::Internal)?
                || !matches!(secret.state, SecretState::Paused | SecretState::Delivered)
                || secret.pending_control.is_some()
            {
                return Err(RuleError::StaleAction.into());
            }
        } else if plan.state != PlanState::Paused {
            return Err(RuleError::StaleAction.into());
        }
        // Stage while still holding the validated plan lock: a concurrent resume
        // cannot slip between confirmation validation and the destructive intent.
        let intent = self
            .stage_control(
                &mut *tx,
                profile,
                plan,
                operation_id,
                request
                    .secret_id
                    .map(|secret_id| Control::DeleteSecret { secret_id })
                    .unwrap_or(Control::DeleteProfile),
            )
            .await?;
        let mut request = request;
        request.completed_operation = Some(operation_id);
        put(&mut *tx, Some(request.plan_id), &request).await?;
        tx.commit().await?;
        self.journal.append(&intent).await?;
        self.apply_control(&intent, false).await?;
        self.operation_receipt(actor, operation_id).await
    }
}
