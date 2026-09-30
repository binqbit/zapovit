use crate::*;
use domain::{DAY, Id, PartState, RuleError, SecretState};

impl Engine {
    pub async fn utc_offset_minutes(&self, actor: Id) -> Result<i16> {
        let mut tx = self.db.begin().await?;
        get::<Account>(&mut *tx, actor).await?;
        Ok(match tx.get(Kind::AccountPreference, actor).await? {
            Some(raw) => {
                serde_json::from_value::<AccountPreference>(raw)
                    .map_err(|_| Error::Internal)?
                    .utc_offset_minutes
            }
            None => 0,
        })
    }

    pub async fn set_utc_offset_minutes(&self, actor: Id, minutes: i16) -> Result<()> {
        if !(-720..=840).contains(&minutes) || minutes % 15 != 0 {
            return Err(Error::InvalidInput);
        }
        let mut tx = self.db.begin().await?;
        tx.lock(Kind::Account, actor).await?;
        get::<Account>(&mut *tx, actor).await?;
        put(
            &mut *tx,
            Some(actor),
            &AccountPreference {
                id: actor,
                utc_offset_minutes: minutes,
            },
        )
        .await?;
        tx.commit().await
    }

    pub async fn invitations(&self, actor: Id, plan: Id) -> Result<Vec<InvitationView>> {
        let mut tx = self.db.begin().await?;
        Self::owner(&mut *tx, actor, plan).await?;
        let owner: Account = get(&mut *tx, actor).await?;
        let now = tx.now().await?;
        let mut result = Vec::new();
        for invitation in list::<Invitation>(&mut *tx, Some(plan)).await? {
            let state = Self::invitation_state(&mut *tx, &invitation).await?;
            result.push(InvitationView {
                id: invitation.id,
                plan_id: plan,
                owner_telegram_id: owner.telegram_id,
                expires_at: invitation.expires_at,
                status: if state.revoked {
                    InvitationStatus::Revoked
                } else if invitation.accepted_by.is_some() {
                    InvitationStatus::Accepted
                } else if invitation.expires_at <= now {
                    InvitationStatus::Expired
                } else {
                    InvitationStatus::Pending
                },
            });
        }
        Ok(result)
    }
    pub(crate) async fn validate_people(
        tx: &mut dyn Transaction,
        plan: Id,
        policy: &domain::Policy,
    ) -> Result<()> {
        let participants = list::<Participant>(tx, Some(plan)).await?;
        for account in policy.guardians.union(&policy.recipients) {
            let participant = participants
                .iter()
                .find(|p| p.account_id == *account && p.confirmed)
                .ok_or(RuleError::NotReady)?;
            if !Self::contact_active(tx, participant.id).await? {
                return Err(RuleError::NotReady.into());
            }
        }
        Ok(())
    }
    pub(crate) async fn contact_active(tx: &mut dyn Transaction, id: Id) -> Result<bool> {
        Ok(match tx.get(Kind::ContactState, id).await? {
            Some(raw) => {
                !serde_json::from_value::<ContactState>(raw)
                    .map_err(|_| Error::Internal)?
                    .archived
            }
            None => true,
        })
    }

    pub(crate) async fn label_in(
        &self,
        tx: &mut dyn Transaction,
        plan: Id,
        target: Id,
    ) -> Result<Option<String>> {
        let Some(raw) = tx.get(Kind::PrivateMetadata, target).await? else {
            return Ok(None);
        };
        let metadata: PrivateMetadata = serde_json::from_value(raw).map_err(|_| Error::Internal)?;
        if metadata.plan_id != plan {
            return Err(RuleError::AccessDenied.into());
        }
        let bytes = self
            .crypto
            .unwrap(&format!("owner-label/{plan}"), target, &metadata.label)?;
        Ok(Some(
            std::str::from_utf8(&bytes)
                .map_err(|_| Error::Crypto)?
                .to_owned(),
        ))
    }

    pub async fn owner_label(&self, actor: Id, plan: Id, target: Id) -> Result<Option<String>> {
        let mut tx = self.db.begin().await?;
        Self::owner(&mut *tx, actor, plan).await?;
        self.label_in(&mut *tx, plan, target).await
    }

    pub async fn set_label(&self, actor: Id, plan: Id, target: Id, label: &str) -> Result<()> {
        let label = label.trim();
        if label.len() > 320 || label.chars().count() > 80 || label.chars().any(char::is_control) {
            return Err(Error::InvalidInput);
        }
        let mut tx = self.db.begin().await?;
        Self::owner(&mut *tx, actor, plan).await?;
        let mut found = false;
        for kind in [Kind::Secret, Kind::Draft, Kind::Participant] {
            if let Some(value) = tx.get(kind, target).await? {
                if value.get("plan_id").and_then(serde_json::Value::as_str)
                    != Some(&plan.to_string())
                {
                    return Err(RuleError::AccessDenied.into());
                }
                if kind == Kind::Draft {
                    let draft: Draft =
                        serde_json::from_value(value).map_err(|_| Error::Internal)?;
                    if draft.saved_secret.is_some() || draft.expires_at <= tx.now().await? {
                        return Err(RuleError::StaleAction.into());
                    }
                }
                found = true;
                break;
            }
        }
        if !found {
            return Err(Error::NotFound);
        }
        if label.is_empty() {
            tx.remove(Kind::PrivateMetadata, target).await?;
        } else {
            let metadata = PrivateMetadata {
                id: target,
                plan_id: plan,
                label: self.crypto.wrap(
                    &format!("owner-label/{plan}"),
                    target,
                    label.as_bytes(),
                )?,
            };
            put(&mut *tx, Some(plan), &metadata).await?;
        }
        tx.commit().await
    }

    pub async fn invitation(&self, actor: Id, id: Id) -> Result<InvitationView> {
        let mut tx = self.db.begin().await?;
        get::<Account>(&mut *tx, actor).await?;
        let invitation: Invitation = get(&mut *tx, id).await?;
        let owner: Account = get(&mut *tx, invitation.owner_id).await?;
        let now = tx.now().await?;
        let state = Self::invitation_state(&mut *tx, &invitation).await?;
        let status = if state.revoked {
            InvitationStatus::Revoked
        } else if state.declined.contains(&actor) {
            InvitationStatus::Declined
        } else if invitation.accepted_by == Some(actor) {
            InvitationStatus::Accepted
        } else if invitation.accepted_by.is_some() || invitation.owner_id == actor {
            InvitationStatus::Unavailable
        } else if invitation.expires_at <= now {
            InvitationStatus::Expired
        } else {
            InvitationStatus::Pending
        };
        Ok(InvitationView {
            id,
            plan_id: invitation.plan_id,
            owner_telegram_id: owner.telegram_id,
            expires_at: invitation.expires_at,
            status,
        })
    }

    pub(crate) async fn invitation_state(
        tx: &mut dyn Transaction,
        invitation: &Invitation,
    ) -> Result<InvitationState> {
        match tx.get(Kind::InvitationState, invitation.id).await? {
            Some(raw) => serde_json::from_value(raw).map_err(|_| Error::Internal),
            None => Ok(InvitationState {
                id: invitation.id,
                plan_id: invitation.plan_id,
                revoked: false,
                declined: Default::default(),
            }),
        }
    }

    pub async fn decline_invitation(&self, actor: Id, id: Id) -> Result<()> {
        self.limit(&format!("decline:{actor}"), 30, DAY).await?;
        let mut tx = self.db.begin().await?;
        let hint: Invitation = get(&mut *tx, id).await?;
        tx.lock(Kind::Plan, hint.plan_id).await?;
        let invitation: Invitation = get(&mut *tx, id).await?;
        get::<Account>(&mut *tx, actor).await?;
        if invitation.owner_id == actor || invitation.accepted_by.is_some() {
            return Err(RuleError::InvalidState.into());
        }
        let mut state = Self::invitation_state(&mut *tx, &invitation).await?;
        if state.declined.len() >= 100 && !state.declined.contains(&actor) {
            return Err(RuleError::QuotaExceeded.into());
        }
        state.declined.insert(actor);
        put(&mut *tx, Some(invitation.plan_id), &state).await?;
        tx.commit().await
    }

    pub async fn revoke_invitation(&self, actor: Id, id: Id) -> Result<()> {
        let mut tx = self.db.begin().await?;
        let invitation: Invitation = get(&mut *tx, id).await?;
        Self::owner(&mut *tx, actor, invitation.plan_id).await?;
        let mut state = Self::invitation_state(&mut *tx, &invitation).await?;
        state.revoked = true;
        put(&mut *tx, Some(invitation.plan_id), &state).await?;
        tx.commit().await
    }

    pub async fn reject_participant(&self, actor: Id, plan: Id, id: Id) -> Result<()> {
        let mut tx = self.db.begin().await?;
        Self::owner(&mut *tx, actor, plan).await?;
        let Some(raw) = tx.get(Kind::Participant, id).await? else {
            return Ok(());
        };
        let participant: Participant = serde_json::from_value(raw).map_err(|_| Error::Internal)?;
        if participant.plan_id != plan || participant.confirmed {
            return Err(RuleError::AccessDenied.into());
        }
        for invitation in list::<Invitation>(&mut *tx, Some(plan)).await? {
            if invitation.accepted_by == Some(participant.account_id) {
                let mut state = Self::invitation_state(&mut *tx, &invitation).await?;
                state.revoked = true;
                put(&mut *tx, Some(plan), &state).await?;
            }
        }
        tx.remove(Kind::Participant, id).await?;
        tx.remove(Kind::ContactState, id).await?;
        tx.remove(Kind::PrivateMetadata, id).await?;
        tx.commit().await
    }

    pub async fn archive_participant(&self, actor: Id, plan: Id, id: Id) -> Result<()> {
        let mut tx = self.db.begin().await?;
        Self::owner(&mut *tx, actor, plan).await?;
        let participant: Participant = get(&mut *tx, id).await?;
        if participant.plan_id != plan {
            return Err(RuleError::AccessDenied.into());
        }
        if list::<Secret>(&mut *tx, Some(plan))
            .await?
            .iter()
            .any(|secret| {
                secret.state != SecretState::Deleted
                    && (secret.policy.guardians.contains(&participant.account_id)
                        || secret.policy.recipients.contains(&participant.account_id))
            })
        {
            return Err(RuleError::InvalidState.into());
        }
        put(
            &mut *tx,
            Some(plan),
            &ContactState {
                id,
                plan_id: plan,
                archived: true,
            },
        )
        .await?;
        tx.commit().await
    }

    pub async fn contacts(&self, actor: Id, plan: Id) -> Result<Vec<ContactView>> {
        let mut tx = self.db.begin().await?;
        Self::owner(&mut *tx, actor, plan).await?;
        let secrets = list::<Secret>(&mut *tx, Some(plan)).await?;
        let mut result = Vec::new();
        for participant in list::<Participant>(&mut *tx, Some(plan)).await? {
            let account: Account = get(&mut *tx, participant.account_id).await?;
            let mut dependent = Vec::new();
            let mut delivery_failed = false;
            for secret in &secrets {
                if secret.state != SecretState::Deleted
                    && (secret.policy.guardians.contains(&account.id)
                        || secret.policy.recipients.contains(&account.id))
                {
                    dependent.push(secret.id);
                    delivery_failed |= list::<DeliveryPart>(&mut *tx, Some(secret.id))
                        .await?
                        .iter()
                        .any(|part| {
                            part.recipient_id == account.id
                                && matches!(
                                    part.state,
                                    PartState::PermanentFailed | PartState::Unknown
                                )
                        });
                }
            }
            result.push(ContactView {
                id: participant.id,
                account_id: account.id,
                telegram_id: account.telegram_id,
                label: self.label_in(&mut *tx, plan, participant.id).await?,
                confirmed: participant.confirmed,
                archived: !Self::contact_active(&mut *tx, participant.id).await?,
                dependent_secrets: dependent,
                delivery_failed,
            });
        }
        Ok(result)
    }
}
