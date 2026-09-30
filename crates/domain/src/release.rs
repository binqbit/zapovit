use crate::{DAY, Id, PlanState, Policy, RuleError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use time::{Duration, OffsetDateTime};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CaseState {
    Collecting,
    Waiting,
    Ready,
    Delivering,
    Complete,
    Cancelled,
    Expired,
    NeedsAttention,
    Partial,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReleaseCase {
    pub id: Id,
    pub secret_id: Id,
    pub state: CaseState,
    pub plan_epoch: i64,
    pub secret_epoch: i64,
    pub created_at: OffsetDateTime,
    pub quorum_at: Option<OffsetDateTime>,
    pub release_at: Option<OffsetDateTime>,
    pub confirmed: BTreeSet<Id>,
}

impl ReleaseCase {
    pub fn new(secret_id: Id, plan_epoch: i64, secret_epoch: i64, now: OffsetDateTime) -> Self {
        Self {
            id: Id::new_v4(),
            secret_id,
            state: CaseState::Collecting,
            plan_epoch,
            secret_epoch,
            created_at: now,
            quorum_at: None,
            release_at: None,
            confirmed: BTreeSet::new(),
        }
    }

    pub fn confirm(
        &mut self,
        actor: Id,
        policy: &Policy,
        now: OffsetDateTime,
    ) -> Result<bool, RuleError> {
        if !policy.guardians.contains(&actor) {
            return Err(RuleError::AccessDenied);
        }
        if self.state != CaseState::Collecting {
            return Err(RuleError::InvalidState);
        }
        if now >= self.created_at + Duration::seconds(30 * DAY) {
            return Err(RuleError::Expired);
        }
        if !self.confirmed.insert(actor) {
            return Ok(false);
        }
        if self.confirmed.len() >= usize::from(policy.threshold) {
            self.quorum_at = Some(now);
            self.release_at = Some(now + Duration::seconds(policy.timing.release_delay_seconds));
            self.state = CaseState::Waiting;
        }
        Ok(true)
    }

    pub fn may_dispatch(
        &self,
        state: PlanState,
        plan_epoch: i64,
        secret_epoch: i64,
        policy: &Policy,
        now: OffsetDateTime,
        hold: bool,
    ) -> bool {
        !hold
            && state == PlanState::Active
            && self.plan_epoch == plan_epoch
            && self.secret_epoch == secret_epoch
            && matches!(
                self.state,
                CaseState::Waiting | CaseState::Ready | CaseState::Delivering | CaseState::Partial
            )
            && self.confirmed.len() >= usize::from(policy.threshold)
            && self.release_at.is_some_and(|at| now >= at)
    }

    pub fn cancel(&mut self) {
        if self.state != CaseState::Complete {
            self.state = CaseState::Cancelled;
        }
        self.confirmed.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Timing;
    #[test]
    fn quorum_starts_grace_and_epoch_fences_delivery() {
        let guardians: BTreeSet<_> = (1..=5).map(Id::from_u128).collect();
        let p = Policy {
            guardians: guardians.clone(),
            recipients: guardians.clone(),
            threshold: 3,
            timing: Timing::default(),
        };
        let now = OffsetDateTime::UNIX_EPOCH;
        let mut case = ReleaseCase::new(Id::new_v4(), 5, 2, now);
        assert!(case.confirm(Id::from_u128(1), &p, now).unwrap());
        assert!(!case.confirm(Id::from_u128(1), &p, now).unwrap());
        assert!(case.quorum_at.is_none());
        case.confirm(Id::from_u128(2), &p, now).unwrap();
        case.confirm(Id::from_u128(3), &p, now).unwrap();
        let at = now + Duration::days(7);
        assert!(!case.may_dispatch(
            PlanState::Active,
            5,
            2,
            &p,
            at - Duration::seconds(1),
            false
        ));
        assert!(case.may_dispatch(PlanState::Active, 5, 2, &p, at, false));
        assert!(!case.may_dispatch(PlanState::Active, 6, 2, &p, at, false));
        assert!(!case.may_dispatch(PlanState::Paused, 5, 2, &p, at, false));
        assert!(!case.may_dispatch(PlanState::Active, 5, 2, &p, at, true));
        case.cancel();
        assert!(!case.may_dispatch(PlanState::Active, 5, 2, &p, at, false));
    }
    #[test]
    fn expired_collection_cannot_create_quorum() {
        let who = Id::new_v4();
        let p = Policy {
            guardians: [who].into(),
            recipients: [who].into(),
            threshold: 1,
            timing: Timing::default(),
        };
        let mut c = ReleaseCase::new(Id::new_v4(), 0, 0, OffsetDateTime::UNIX_EPOCH);
        assert_eq!(
            c.confirm(who, &p, OffsetDateTime::UNIX_EPOCH + Duration::days(30)),
            Err(RuleError::Expired)
        );
    }
}
