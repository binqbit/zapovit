use crate::*;
use domain::{DAY, Id, PartState, SecretState};

impl Engine {
    pub async fn cleanup_plan(&self, plan_id: Id) -> Result<()> {
        let mut tx = self.db.begin().await?;
        tx.lock(Kind::Plan, plan_id).await?;
        let now = tx.now().await?;
        if tx.get(Kind::Plan, plan_id).await?.is_none() {
            return Ok(());
        }
        let plan: Plan = get(&mut *tx, plan_id).await?;
        for mut secret in list::<Secret>(&mut *tx, Some(plan_id)).await? {
            if secret.state == SecretState::Provisioning && secret.created_at + DAY <= now {
                secret.state = SecretState::SetupFailed;
                put(&mut *tx, Some(plan_id), &secret).await?;
                for mut grant in list::<GuardianGrant>(&mut *tx, Some(secret.id)).await? {
                    grant.delivery = None;
                    put(&mut *tx, Some(secret.id), &grant).await?;
                }
            }
            for raw in tx.active_cases(secret.id).await? {
                let mut case: CaseRecord =
                    serde_json::from_value(raw).map_err(|_| Error::Internal)?;
                if case.started_delivery.is_some_and(|at| now >= at + 7 * DAY)
                    && matches!(
                        case.case.state,
                        domain::CaseState::Delivering | domain::CaseState::Partial
                    )
                {
                    for sub in list::<Submission>(&mut *tx, Some(case.id)).await? {
                        tx.remove(Kind::Submission, sub.id).await?;
                    }
                    case.case.state = domain::CaseState::NeedsAttention;
                    case.due_at = i64::MAX;
                    put(&mut *tx, Some(secret.id), &case).await?;
                    // Historical cases cannot change a newer case or lift a persisted STOP.
                    if secret.last_case == Some(case.id)
                        && secret.pending_control.is_none()
                        && matches!(
                            secret.state,
                            SecretState::Armed | SecretState::Partial | SecretState::NeedsAttention
                        )
                    {
                        secret.state = SecretState::NeedsAttention;
                        put(&mut *tx, Some(plan_id), &secret).await?;
                    }
                }
            }
        }
        for raw in tx.expired(Kind::Draft, Some(plan_id), now, 100).await? {
            let draft: Draft = serde_json::from_value(raw).map_err(|_| Error::Internal)?;
            crate::control::remove_draft(&mut *tx, &draft).await?;
        }

        for raw in tx.expired(Kind::Job, Some(plan_id), now, 200).await? {
            let job: Job = serde_json::from_value(raw).map_err(|_| Error::Internal)?;
            if job.expires_at <= now
                && !matches!(job.state, PartState::Dispatching | PartState::Claimed)
            {
                // Purge expired recovery/code envelopes as well as their delivery records.
                if matches!(
                    job.task,
                    Task::RecoveryCode { .. }
                        | Task::ClaimCode { .. }
                        | Task::GuardianCode { .. }
                        | Task::DownloadFile { .. }
                ) {
                    tx.remove(Kind::Job, job.id).await?;
                }
            }
        }
        let mut profile: Profile = get(&mut *tx, plan.profile_id).await?;
        for claim in list::<Claim>(&mut *tx, Some(profile.id)).await? {
            if claim.expires_at <= now {
                if profile.pending_claim == Some(claim.id) {
                    profile.pending_claim = None;
                    put(&mut *tx, Some(profile.owner_id), &profile).await?;
                }
                tx.remove(Kind::Claim, claim.id).await?;
            }
        }
        for raw in tx.expired(Kind::Action, Some(plan_id), now, 200).await? {
            let action: Action = serde_json::from_value(raw).map_err(|_| Error::Internal)?;
            if action.expires_at <= now {
                tx.remove(Kind::Action, action.id).await?;
            }
        }
        tx.commit().await
    }
    pub async fn garbage_collect(&self, object_id: Id) -> Result<()> {
        let mut tx = self.db.begin().await?;
        if !tx.writes_ready().await? {
            return Ok(());
        }
        let Some(raw) = tx.get(Kind::FileObject, object_id).await? else {
            return Ok(());
        };
        let o: FileObject = serde_json::from_value(raw).map_err(|_| Error::Internal)?;
        let now = tx.now().await?;
        if o.due_at > now || !["pending", "gc"].contains(&o.state.as_str()) {
            return Ok(());
        }
        tx.commit().await?;
        self.blobs.delete(&o.key).await?;
        let exists = self.blobs.exists(&o.key).await?;
        let mut tx = self.db.begin().await?;
        tx.lock(Kind::Plan, o.plan_id).await?;
        let Some(raw) = tx.get(Kind::FileObject, o.id).await? else {
            return Ok(());
        };
        let mut fresh: FileObject = serde_json::from_value(raw).map_err(|_| Error::Internal)?;
        if !["pending", "gc"].contains(&fresh.state.as_str()) {
            return Ok(());
        }
        // Keep the key through the bounded PUT lifetime and a full day of repeated checks.
        // Missing on the first scan does not prove an earlier request cannot complete later.
        if !exists && now > fresh.created_at + 2 * DAY {
            tx.remove(Kind::FileObject, fresh.id).await?;
            tx.release_resource("blob_bytes", fresh.id).await?;
        } else {
            fresh.state = "gc".into();
            fresh.due_at = now + 300;
            put(&mut *tx, Some(o.plan_id), &fresh).await?;
        }
        tx.commit().await
    }
}
