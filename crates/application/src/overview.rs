use crate::*;
use domain::{CaseState, DAY, Id, PartState, PlanState, SecretState};
use time::OffsetDateTime;

impl Engine {
    /// Role-scoped projections contain no payload, code envelope, or recovery verifier.
    pub async fn overview(&self, actor: Id) -> Result<UserOverview> {
        let mut tx = self.db.begin().await?;
        get::<Account>(&mut *tx, actor).await?;
        let now = tx.now().await?;
        let profile = find::<Profile>(&mut *tx, "owner_id", &actor.to_string())
            .await?
            .into_iter()
            .find(|profile| profile.state == "active");
        let mut own = None;
        if let Some(profile) = profile
            && let Some(plan) = list::<Plan>(&mut *tx, Some(profile.id))
                .await?
                .into_iter()
                .find(|plan| plan.state != PlanState::Deleted)
        {
            let mut confirmed_people = 0;
            for person in list::<Participant>(&mut *tx, Some(plan.id)).await? {
                if person.confirmed && Self::contact_active(&mut *tx, person.id).await? {
                    confirmed_people += 1;
                }
            }
            let mut secrets = Vec::new();
            let mut records = list::<Secret>(&mut *tx, Some(plan.id)).await?;
            records.sort_by_key(|secret| (secret.created_at, secret.id));
            for secret in records
                .into_iter()
                .filter(|secret| secret.state != SecretState::Deleted)
            {
                let grants = list::<GuardianGrant>(&mut *tx, Some(secret.id)).await?;
                let parts = list::<DeliveryPart>(&mut *tx, Some(secret.id)).await?;
                let case = if let Some(id) = secret.last_case {
                    Some(get::<CaseRecord>(&mut *tx, id).await?)
                } else {
                    None
                };
                let mut blockers = Vec::new();
                if secret.pending_control.is_some() {
                    blockers.push(ReadinessBlocker::ControlPending);
                }
                match secret.state {
                    SecretState::Paused => blockers.push(ReadinessBlocker::SecretPaused),
                    SecretState::Provisioning => blockers.push(ReadinessBlocker::CodesPending),
                    SecretState::SetupFailed => blockers.push(ReadinessBlocker::SetupFailed),
                    _ => {}
                }
                let can_resume = secret.state == SecretState::Paused
                    && secret.pending_control.is_none()
                    && plan.pending_control.is_none()
                    && profile.pending_claim.is_none()
                    && profile.recovery_saved
                    && grants.len() == secret.policy.guardians.len()
                    && grants.iter().all(|grant| grant.ready);
                secrets.push(SecretOverview {
                    id: secret.id,
                    label: self.label_in(&mut *tx, plan.id, secret.id).await?,
                    state: secret.state,
                    timing: secret.policy.timing.clone(),
                    threshold: secret.policy.threshold,
                    guardians: grants
                        .into_iter()
                        .map(|grant| GuardianReadiness {
                            account_id: grant.account_id,
                            grant_id: grant.id,
                            ready: grant.ready,
                        })
                        .collect(),
                    recipients: secret.policy.recipients.iter().copied().collect(),
                    provisioning_expires_at: (secret.state == SecretState::Provisioning)
                        .then_some(secret.created_at + DAY),
                    inactivity_at: secret.due_at,
                    case_state: case.as_ref().map(|case| case.case.state),
                    release_at: case
                        .as_ref()
                        .and_then(|case| case.case.release_at.map(|at| at.unix_timestamp())),
                    sent_parts: parts
                        .iter()
                        .filter(|part| part.state == PartState::Sent)
                        .count(),
                    unknown_parts: parts
                        .iter()
                        .filter(|part| {
                            matches!(part.state, PartState::Unknown | PartState::Dispatching)
                        })
                        .count(),
                    total_parts: parts.len(),
                    blockers,
                    can_stop: matches!(
                        secret.state,
                        SecretState::Armed | SecretState::Partial | SecretState::NeedsAttention
                    ),
                    can_resume,
                });
            }
            let ready_secrets = secrets
                .iter()
                .filter(|secret| {
                    matches!(
                        secret.state,
                        SecretState::Armed | SecretState::Partial | SecretState::NeedsAttention
                    )
                })
                .count();
            let mut blockers = Vec::new();
            if !profile.recovery_saved {
                blockers.push(ReadinessBlocker::RecoveryNotSaved);
            }
            if profile.pending_claim.is_some() {
                blockers.push(ReadinessBlocker::RecoveryPending);
            }
            if plan.pending_control.is_some() {
                blockers.push(ReadinessBlocker::ControlPending);
            }
            if confirmed_people == 0 {
                blockers.push(ReadinessBlocker::NoConfirmedPeople);
            }
            if secrets.is_empty() {
                blockers.push(ReadinessBlocker::NoSecrets);
            } else if ready_secrets == 0 {
                blockers.push(ReadinessBlocker::NoReadySecrets);
            }
            if plan.state == PlanState::Paused {
                blockers.push(ReadinessBlocker::PlanPaused);
            }
            let operational = tx.operational_status(Some(plan.id)).await?;
            let can_resume = matches!(plan.state, PlanState::Setup | PlanState::Paused)
                && ready_secrets > 0
                && profile.recovery_saved
                && profile.pending_claim.is_none()
                && plan.pending_control.is_none();
            let can_create_draft = profile.recovery_saved
                && profile.pending_claim.is_none()
                && plan.pending_control.is_none()
                && confirmed_people > 0
                && secrets.len() < 50
                && operational.writes_ready;
            let next_action = if profile.pending_claim.is_some() {
                NextAction::ResolveRecovery
            } else if !profile.recovery_saved {
                NextAction::SaveRecovery
            } else if confirmed_people == 0 {
                NextAction::PreparePeople
            } else if secrets.is_empty()
                || secrets.iter().all(|secret| {
                    matches!(
                        secret.state,
                        SecretState::SetupFailed | SecretState::Delivered
                    )
                })
            {
                NextAction::CreateSecret
            } else if can_resume {
                NextAction::ResumePlan
            } else if ready_secrets == 0
                && secrets
                    .iter()
                    .any(|secret| secret.state == SecretState::Provisioning)
            {
                NextAction::AwaitCodes
            } else {
                NextAction::None
            };
            let nearest_inactivity = secrets
                .iter()
                .filter(|secret| secret.state == SecretState::Armed && secret.case_state.is_none())
                .map(|secret| secret.inactivity_at)
                .min();
            let recovery_expires_at = match profile.pending_claim {
                Some(id) => tx
                    .get(Kind::Claim, id)
                    .await?
                    .map(|raw| serde_json::from_value::<Claim>(raw).map(|claim| claim.expires_at))
                    .transpose()
                    .map_err(|_| Error::Internal)?,
                None => None,
            };
            own = Some(PlanOverview {
                id: plan.id,
                state: plan.state,
                last_activity: plan.last_activity,
                next_reminder: (plan.state == PlanState::Active).then_some(plan.next_reminder),
                nearest_inactivity,
                recovery_saved: profile.recovery_saved,
                pending_claim: profile.pending_claim,
                recovery_expires_at,
                confirmed_people,
                ready_secrets,
                secrets,
                blockers,
                operational,
                next_action,
                can_create_draft,
                can_resume,
            });
        }

        let mut guardian = Vec::new();
        let mut receiving = Vec::new();
        let mut after = None;
        loop {
            let records = tx.related_secrets(actor, after, 200).await?;
            if records.is_empty() {
                break;
            }
            for raw in records {
                let secret: Secret = serde_json::from_value(raw).map_err(|_| Error::Internal)?;
                after = Some(secret.id);
                if secret.state == SecretState::Deleted {
                    continue;
                }
                let plan: Plan = get(&mut *tx, secret.plan_id).await?;
                let profile: Profile = get(&mut *tx, plan.profile_id).await?;
                let owner: Account = get(&mut *tx, profile.owner_id).await?;
                let owner_display_name = self.account_display_name(&owner)?;
                let case = if let Some(id) = secret.last_case {
                    Some(get::<CaseRecord>(&mut *tx, id).await?)
                } else {
                    None
                };
                let valid_scope = profile.pending_claim.is_none()
                    && plan.pending_control.is_none()
                    && secret.pending_control.is_none()
                    && plan.state == PlanState::Active;
                if secret.policy.guardians.contains(&actor) {
                    let grant = list::<GuardianGrant>(&mut *tx, Some(secret.id))
                        .await?
                        .into_iter()
                        .find(|grant| grant.account_id == actor);
                    let confirmed = case
                        .as_ref()
                        .is_some_and(|case| case.case.confirmed.contains(&actor));
                    guardian.push(GuardianOverview {
                        secret_id: secret.id,
                        plan_id: plan.id,
                        owner_telegram_id: owner.telegram_id,
                        owner_display_name: owner_display_name.clone(),
                        state: secret.state,
                        grant_id: grant.as_ref().map(|grant| grant.id),
                        code_ready: grant.as_ref().is_some_and(|grant| grant.ready),
                        can_resend_code: secret.state == SecretState::Provisioning
                            && profile.pending_claim.is_none()
                            && grant.as_ref().is_some_and(|grant| {
                                !grant.ready && grant.delivery.is_some() && grant.expires_at > now
                            }),
                        provisioning_expires_at: (secret.state == SecretState::Provisioning)
                            .then_some(secret.created_at + DAY),
                        case_id: case.as_ref().map(|case| case.id),
                        case_state: case.as_ref().map(|case| case.case.state),
                        case_expires_at: case
                            .as_ref()
                            .map(|case| case.case.created_at.unix_timestamp() + 30 * DAY),
                        confirmed,
                        can_submit: valid_scope
                            && secret.state == SecretState::Armed
                            && !confirmed
                            && grant.as_ref().is_some_and(|grant| grant.ready)
                            && case.as_ref().is_some_and(|case| {
                                case.case.state == CaseState::Collecting
                                    && case.case.plan_epoch == plan.epoch
                                    && case.case.secret_epoch == secret.epoch
                                    && now < case.case.created_at.unix_timestamp() + 30 * DAY
                            }),
                    });
                }
                if secret.policy.recipients.contains(&actor) {
                    let operational = tx.operational_status(Some(plan.id)).await?;
                    let retry_until = case
                        .as_ref()
                        .and_then(|case| case.started_delivery.map(|at| at + 7 * DAY));
                    let may_retry = valid_scope
                        && operational.ready
                        && retry_until.is_some_and(|until| until > now)
                        && case.as_ref().is_some_and(|case| {
                            OffsetDateTime::from_unix_timestamp(now).is_ok_and(|at| {
                                case.case.may_dispatch(
                                    plan.state,
                                    plan.epoch,
                                    secret.epoch,
                                    &secret.policy,
                                    at,
                                    plan.hold_until > now,
                                )
                            })
                        });
                    let mut parts: Vec<_> = list::<DeliveryPart>(&mut *tx, Some(secret.id))
                        .await?
                        .into_iter()
                        .filter(|part| part.recipient_id == actor)
                        .map(|part| RecipientPart {
                            id: part.id,
                            index: part.index,
                            state: part.state,
                            can_retry: may_retry
                                && matches!(
                                    part.state,
                                    PartState::Unknown
                                        | PartState::PermanentFailed
                                        | PartState::RetryableFailed
                                ),
                        })
                        .collect();
                    parts.sort_by_key(|part| part.index);
                    receiving.push(RecipientOverview {
                        secret_id: secret.id,
                        plan_id: plan.id,
                        owner_telegram_id: owner.telegram_id,
                        owner_display_name,
                        state: secret.state,
                        parts,
                        retry_until,
                    });
                }
            }
        }
        let mut receipts = list::<OperationReceipt>(&mut *tx, Some(actor)).await?;
        receipts.retain(|receipt| receipt.due_at > now);
        for receipt in &mut receipts {
            Self::refresh_receipt(&mut *tx, receipt).await?;
        }
        receipts.sort_by_key(|receipt| std::cmp::Reverse((receipt.at, receipt.id)));
        tx.commit().await?;
        Ok(UserOverview {
            now,
            own,
            guardian,
            receiving,
            receipts,
        })
    }
}
