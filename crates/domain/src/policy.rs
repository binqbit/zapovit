use crate::{Id, RuleError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const DAY: i64 = 86_400;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Timing {
    pub reminder_seconds: i64,
    pub inactivity_seconds: i64,
    pub release_delay_seconds: i64,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            reminder_seconds: 7 * DAY,
            inactivity_seconds: 28 * DAY,
            release_delay_seconds: 7 * DAY,
        }
    }
}

impl Timing {
    pub fn validate(&self) -> Result<(), RuleError> {
        if !(DAY..=30 * DAY).contains(&self.reminder_seconds)
            || !(2 * DAY..=365 * DAY).contains(&self.inactivity_seconds)
            || self.inactivity_seconds < 2 * self.reminder_seconds
            || !(DAY..=30 * DAY).contains(&self.release_delay_seconds)
        {
            return Err(RuleError::InvalidPolicy);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Policy {
    pub guardians: BTreeSet<Id>,
    pub recipients: BTreeSet<Id>,
    pub threshold: u8,
    pub timing: Timing,
}

impl Policy {
    pub fn validate(&self, owner: Id) -> Result<(), RuleError> {
        self.timing.validate()?;
        if self.guardians.is_empty()
            || self.guardians.len() > 10
            || self.recipients.is_empty()
            || self.recipients.len() > 10
            || self.threshold == 0
            || usize::from(self.threshold) > self.guardians.len()
            || self.guardians.contains(&owner)
            || self.recipients.contains(&owner)
        {
            return Err(RuleError::InvalidPolicy);
        }
        Ok(())
    }

    /// Explicit, versioned encoding used for authenticated policy digests.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = b"zapovit/policy/v1\0".to_vec();
        out.push(if self.threshold == 1 { 1 } else { 2 });
        out.push(self.threshold);
        for set in [&self.guardians, &self.recipients] {
            out.extend_from_slice(&(set.len() as u32).to_be_bytes());
            for id in set {
                out.extend_from_slice(id.as_bytes());
            }
        }
        for n in [
            self.timing.reminder_seconds,
            self.timing.inactivity_seconds,
            self.timing.release_delay_seconds,
        ] {
            out.extend_from_slice(&n.to_be_bytes());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn owner_cannot_be_recipient_or_guardian() {
        let owner = Id::new_v4();
        let guardian = Id::new_v4();
        let mut p = Policy {
            guardians: [guardian].into(),
            recipients: [owner].into(),
            threshold: 1,
            timing: Timing::default(),
        };
        assert_eq!(p.validate(owner), Err(RuleError::InvalidPolicy));
        p.recipients = [guardian].into();
        assert_eq!(p.validate(owner), Ok(()));
        p.threshold = 2;
        assert_eq!(p.validate(owner), Err(RuleError::InvalidPolicy));
    }
    #[test]
    fn policy_encoding_is_unambiguous_and_order_independent() {
        let a = Id::from_u128(1);
        let b = Id::from_u128(2);
        let p = Policy {
            guardians: [a, b].into(),
            recipients: [b].into(),
            threshold: 2,
            timing: Timing::default(),
        };
        let mut q = p.clone();
        q.guardians = [b, a].into();
        assert_eq!(p.canonical_bytes(), q.canonical_bytes());
        q.threshold = 1;
        assert_ne!(p.canonical_bytes(), q.canonical_bytes());
    }
}
