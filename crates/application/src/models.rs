use domain::{Id, PlanState, Policy, ReleaseCase, SecretState, Timing};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Only ciphertext crosses repository boundaries for user content.
#[derive(Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub version: u8,
    pub key_id: String,
    pub nonce: String,
    pub ciphertext: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: Id,
    pub telegram_id: i64,
    pub chat_id: i64,
    pub locale: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: Id,
    pub owner_id: Id,
    pub owner_epoch: i64,
    pub state: String,
    pub recovery_selector: Id,
    pub recovery_hash: String,
    pub recovery_saved: bool,
    pub pending_claim: Option<Id>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Plan {
    pub id: Id,
    pub profile_id: Id,
    pub state: PlanState,
    pub epoch: i64,
    pub timing: Timing,
    pub last_activity: i64,
    pub due_at: i64,
    pub next_reminder: i64,
    pub hold_until: i64,
    pub pending_control: Option<Id>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Participant {
    pub id: Id,
    pub plan_id: Id,
    pub account_id: Id,
    pub confirmed: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Invitation {
    pub id: Id,
    pub plan_id: Id,
    pub owner_id: Id,
    pub expires_at: i64,
    pub accepted_by: Option<Id>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Draft {
    pub id: Id,
    pub plan_id: Id,
    pub owner_epoch: i64,
    pub revision: i64,
    pub created_at: i64,
    pub expires_at: i64,
    pub payload: Envelope,
    pub policy: Option<Policy>,
    pub saved_secret: Option<Id>,
    pub sources: Vec<(i64, i64)>,
    /// Durable scheduling receipts remain after successful cleanup jobs are removed.
    #[serde(default)]
    pub cleanup_scheduled: Vec<(i64, i64)>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Secret {
    pub id: Id,
    pub plan_id: Id,
    pub epoch: i64,
    pub state: SecretState,
    pub policy: Policy,
    pub payload: Envelope,
    pub created_at: i64,
    pub due_at: i64,
    pub last_case: Option<Id>,
    #[serde(default)]
    pub pending_control: Option<Id>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct GuardianGrant {
    pub id: Id,
    pub secret_id: Id,
    pub account_id: Id,
    pub index: u8,
    pub verifier: String,
    pub verifier_key_id: String,
    pub ready: bool,
    pub delivery: Option<Envelope>,
    pub expires_at: i64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct CaseRecord {
    pub id: Id,
    pub plan_id: Id,
    pub secret_id: Id,
    pub case: ReleaseCase,
    pub due_at: i64,
    pub started_delivery: Option<i64>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Submission {
    pub id: Id,
    pub case_id: Id,
    pub account_id: Id,
    pub share: Envelope,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct FileObject {
    pub id: Id,
    pub plan_id: Id,
    pub draft_id: Id,
    pub operation_id: Id,
    pub key: String,
    pub size: u64,
    pub digest: String,
    pub state: String,
    pub created_at: i64,
    pub due_at: i64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct DeliveryPart {
    pub id: Id,
    pub secret_id: Id,
    pub recipient_id: Id,
    pub index: usize,
    pub manifest: Envelope,
    pub state: domain::PartState,
    pub last_attempt: Option<Id>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Claim {
    pub id: Id,
    pub profile_id: Id,
    pub target: Account,
    pub old_selector: Id,
    pub new_selector: Id,
    pub new_hash: String,
    pub delivery: Envelope,
    pub expires_at: i64,
    pub rotation: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Cancellation {
    pub id: Id,
    pub plan_id: Id,
    pub secret_id: Option<Id>,
    pub epoch: i64,
    pub owner_epoch: i64,
    pub members: BTreeSet<Id>,
    pub votes: BTreeSet<Id>,
    pub secrets: BTreeSet<Id>,
    pub state: String,
    pub expires_at: i64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Dialog {
    pub id: Id,
    pub plan_id: Option<Id>,
    pub owner_epoch: Option<i64>,
    pub step: String,
    pub draft_id: Option<Id>,
    pub expires_at: i64,
    pub selected: BTreeSet<Id>,
    pub reply_to: Option<i64>,
    pub case_id: Option<Id>,
    #[serde(default)]
    pub guardians: BTreeSet<Id>,
    #[serde(default)]
    pub recipients: BTreeSet<Id>,
    #[serde(default)]
    pub threshold: u8,
    #[serde(default)]
    pub timing: Option<Timing>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Action {
    pub id: Id,
    pub actor_id: Id,
    pub plan_id: Option<Id>,
    pub owner_epoch: Option<i64>,
    pub epoch: Option<i64>,
    pub name: String,
    pub target: Option<Id>,
    pub expires_at: i64,
    pub used: bool,
}

/// Owner-readable names are separate from the immutable sealed payload.
#[derive(Clone, Serialize, Deserialize)]
pub struct PrivateMetadata {
    pub id: Id,
    pub plan_id: Id,
    pub label: Envelope,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ContactState {
    pub id: Id,
    pub plan_id: Id,
    pub archived: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct InvitationState {
    pub id: Id,
    pub plan_id: Id,
    pub revoked: bool,
    pub declined: BTreeSet<Id>,
}

/// Long-lived preparation and short-lived draft UI do not overwrite code prompts.
#[derive(Clone, Serialize, Deserialize)]
pub struct DraftSession {
    pub id: Id,
    pub dialog: Dialog,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct AccountPreference {
    pub id: Id,
    pub utc_offset_minutes: i16,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct DeletionRequest {
    pub id: Id,
    pub actor_id: Id,
    pub plan_id: Id,
    pub secret_id: Option<Id>,
    pub owner_epoch: i64,
    pub plan_epoch: i64,
    pub secret_epoch: Option<i64>,
    pub expires_at: i64,
    pub completed_operation: Option<Id>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    CheckIn,
    Stop,
    Resume,
    StopSecret,
    ResumeSecret,
    DeleteSecret,
    DeletePlan,
    DeleteProfile,
    RecoveryStarted,
    RecoveryCompleted,
    SecretSaved,
    DraftCancelled,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptStatus {
    Completed,
    CleanupPending,
}

/// Bounded user-visible history contains identifiers and outcomes, never content.
#[derive(Clone, Serialize, Deserialize)]
pub struct OperationReceipt {
    pub id: Id,
    pub actor_id: Id,
    pub plan_id: Option<Id>,
    pub secret_id: Option<Id>,
    pub operation: OperationKind,
    pub status: ReceiptStatus,
    pub at: i64,
    pub due_at: i64,
    pub pending_objects: BTreeSet<Id>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Task {
    Notice {
        account_id: Id,
        key: String,
        buttons: Vec<(String, Id)>,
    },
    GuardianCode {
        grant_id: Id,
    },
    RecoveryCode {
        profile_id: Id,
        account_id: Id,
        envelope: Envelope,
        selector: Id,
    },
    ClaimCode {
        claim_id: Id,
    },
    GuardianRequest {
        case_id: Id,
        account_id: Id,
    },
    Deliver {
        part_id: Id,
        case_id: Id,
    },
    CleanupMessage {
        chat_id: i64,
        message_id: i64,
        sent_at: i64,
        account_id: Id,
    },
    DeleteObject {
        object_id: Id,
    },
    DownloadFile {
        draft_id: Id,
        account_id: Id,
        file_id: Envelope,
        name: Envelope,
        source_message: i64,
    },
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: Id,
    pub plan_id: Option<Id>,
    pub task: Task,
    pub state: domain::PartState,
    pub due_at: i64,
    pub expires_at: i64,
    pub lease_until: i64,
    pub lease_token: Id,
    pub attempts: u32,
    pub message_id: Option<i64>,
    pub priority: u8,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Control {
    CheckIn,
    Stop,
    Rearm,
    StopSecret {
        secret_id: Id,
    },
    RearmSecret {
        secret_id: Id,
    },
    DeletePlan,
    DeleteProfile,
    DeleteSecret {
        secret_id: Id,
    },
    RecoveryBegin {
        claim_id: Id,
    },
    RecoveryComplete {
        target: Account,
        selector: Id,
        verifier: String,
    },
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ControlIntent {
    pub id: Id,
    pub profile_id: Id,
    pub plan_id: Id,
    pub epoch: i64,
    pub owner_epoch: i64,
    pub at: i64,
    pub previous_state: PlanState,
    pub operation: Control,
    pub applied: bool,
    #[serde(default)]
    pub secret_epoch: Option<i64>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct HandledEvent {
    pub id: Id,
    pub actor_id: Id,
    pub at: i64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Attempt {
    pub id: Id,
    pub job_id: Id,
    pub state: domain::PartState,
    pub at: i64,
    pub message_id: Option<i64>,
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeletedScope {
    Secret,
    Plan,
    Profile,
}

/// Replay fence only: no owner, recipient, policy, content or recovery credential.
#[derive(Clone, Serialize, Deserialize)]
pub struct DeletionTombstone {
    pub id: Id,
    pub scope: DeletedScope,
    pub journal_operation_id: Id,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Account,
    Profile,
    Plan,
    Participant,
    Invitation,
    Draft,
    Secret,
    GuardianGrant,
    Case,
    Submission,
    FileObject,
    DeliveryPart,
    Claim,
    Cancellation,
    Dialog,
    Action,
    Job,
    ControlIntent,
    HandledEvent,
    Attempt,
    DeletionTombstone,
    PrivateMetadata,
    ContactState,
    InvitationState,
    DraftSession,
    DeletionRequest,
    OperationReceipt,
    AccountPreference,
}

impl Kind {
    pub fn table(self) -> &'static str {
        match self {
            Self::Account => "accounts",
            Self::Profile => "profiles",
            Self::Plan => "plans",
            Self::Participant => "participants",
            Self::Invitation => "invitations",
            Self::Draft => "drafts",
            Self::Secret => "secret_versions",
            Self::GuardianGrant => "secret_guardians",
            Self::Case => "release_cases",
            Self::Submission => "guardian_submissions",
            Self::FileObject => "file_objects",
            Self::DeliveryPart => "delivery_parts",
            Self::Claim => "recovery_claims",
            Self::Cancellation => "cancellation_requests",
            Self::Dialog => "dialogs",
            Self::Action => "actions",
            Self::Job => "outbox",
            Self::ControlIntent => "control_intents",
            Self::HandledEvent => "handled_events",
            Self::Attempt => "delivery_attempts",
            Self::DeletionTombstone => "deletion_tombstones",
            Self::PrivateMetadata => "private_metadata",
            Self::ContactState => "contact_states",
            Self::InvitationState => "invitation_states",
            Self::DraftSession => "draft_sessions",
            Self::DeletionRequest => "deletion_requests",
            Self::OperationReceipt => "operation_receipts",
            Self::AccountPreference => "account_preferences",
        }
    }
}

pub trait Record: Serialize + serde::de::DeserializeOwned + Send + Sync {
    const KIND: Kind;
    fn id(&self) -> Id;
}
macro_rules! record {
    ($($ty:ident => $kind:ident),* $(,)?) => { $( impl Record for $ty {
        const KIND: Kind = Kind::$kind;
        fn id(&self) -> Id { self.id }
    } )* };
}
record!(Account=>Account, Profile=>Profile, Plan=>Plan, Participant=>Participant, Invitation=>Invitation,
    Draft=>Draft, Secret=>Secret, GuardianGrant=>GuardianGrant, CaseRecord=>Case, Submission=>Submission,
    FileObject=>FileObject, DeliveryPart=>DeliveryPart, Claim=>Claim, Cancellation=>Cancellation,
    Dialog=>Dialog, Action=>Action, Job=>Job, ControlIntent=>ControlIntent, HandledEvent=>HandledEvent, Attempt=>Attempt,
    DeletionTombstone=>DeletionTombstone, PrivateMetadata=>PrivateMetadata,
    ContactState=>ContactState, InvitationState=>InvitationState, DraftSession=>DraftSession,
    DeletionRequest=>DeletionRequest, OperationReceipt=>OperationReceipt,
    AccountPreference=>AccountPreference);
