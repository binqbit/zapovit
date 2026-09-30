use crate::{OperationReceipt, OperationalStatus};
use domain::{CaseState, Id, PartState, PlanState, SecretState, Timing};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NextAction {
    SaveRecovery,
    PreparePeople,
    CreateSecret,
    AwaitCodes,
    ResumePlan,
    ResolveRecovery,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadinessBlocker {
    RecoveryNotSaved,
    RecoveryPending,
    ControlPending,
    NoConfirmedPeople,
    NoSecrets,
    NoReadySecrets,
    PlanPaused,
    SecretPaused,
    CodesPending,
    SetupFailed,
}

pub struct UserOverview {
    pub now: i64,
    pub own: Option<PlanOverview>,
    pub guardian: Vec<GuardianOverview>,
    pub receiving: Vec<RecipientOverview>,
    pub receipts: Vec<OperationReceipt>,
}

pub struct PlanOverview {
    pub id: Id,
    pub state: PlanState,
    pub last_activity: i64,
    pub next_reminder: Option<i64>,
    pub nearest_inactivity: Option<i64>,
    pub recovery_saved: bool,
    pub pending_claim: Option<Id>,
    pub recovery_expires_at: Option<i64>,
    pub confirmed_people: usize,
    pub ready_secrets: usize,
    pub secrets: Vec<SecretOverview>,
    pub blockers: Vec<ReadinessBlocker>,
    pub operational: OperationalStatus,
    pub next_action: NextAction,
    pub can_create_draft: bool,
    pub can_resume: bool,
}

pub struct SecretOverview {
    pub id: Id,
    pub label: Option<String>,
    pub state: SecretState,
    pub timing: Timing,
    pub threshold: u8,
    pub guardians: Vec<GuardianReadiness>,
    pub recipients: Vec<Id>,
    pub provisioning_expires_at: Option<i64>,
    pub inactivity_at: i64,
    pub case_state: Option<CaseState>,
    pub release_at: Option<i64>,
    pub sent_parts: usize,
    pub unknown_parts: usize,
    pub total_parts: usize,
    pub blockers: Vec<ReadinessBlocker>,
    pub can_stop: bool,
    pub can_resume: bool,
}

pub struct GuardianReadiness {
    pub account_id: Id,
    pub grant_id: Id,
    pub ready: bool,
}

pub struct GuardianOverview {
    pub secret_id: Id,
    pub plan_id: Id,
    pub owner_telegram_id: i64,
    pub state: SecretState,
    pub grant_id: Option<Id>,
    pub code_ready: bool,
    pub can_resend_code: bool,
    pub provisioning_expires_at: Option<i64>,
    pub case_id: Option<Id>,
    pub case_state: Option<CaseState>,
    pub case_expires_at: Option<i64>,
    pub confirmed: bool,
    pub can_submit: bool,
}

pub struct RecipientOverview {
    pub secret_id: Id,
    pub plan_id: Id,
    pub owner_telegram_id: i64,
    pub state: SecretState,
    pub parts: Vec<RecipientPart>,
    pub retry_until: Option<i64>,
}

pub struct RecipientPart {
    pub id: Id,
    pub index: usize,
    pub state: PartState,
    pub can_retry: bool,
}

pub struct ContactView {
    pub id: Id,
    pub account_id: Id,
    pub telegram_id: i64,
    pub label: Option<String>,
    pub confirmed: bool,
    pub archived: bool,
    pub dependent_secrets: Vec<Id>,
    pub delivery_failed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvitationStatus {
    Pending,
    Accepted,
    Declined,
    Revoked,
    Expired,
    Unavailable,
}

pub struct InvitationView {
    pub id: Id,
    pub plan_id: Id,
    pub owner_telegram_id: i64,
    pub expires_at: i64,
    pub status: InvitationStatus,
}
