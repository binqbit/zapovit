use crate::{ControlIntent, Envelope, Kind, Record};
use async_trait::async_trait;
use domain::{Id, Policy, RuleError};
use serde_json::Value;
use zeroize::Zeroizing;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Rule(#[from] RuleError),
    #[error("not_found")]
    NotFound,
    #[error("invalid_input")]
    InvalidInput,
    #[error("invalid_code")]
    InvalidCode,
    #[error("rate_limited")]
    RateLimited,
    #[error("storage_unavailable")]
    Storage,
    #[error("message_unavailable")]
    MessageUnavailable,
    #[error("crypto_failure")]
    Crypto,
    #[error("configuration_error")]
    Config,
    #[error("internal_error")]
    Internal,
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationalBlocker {
    Maintenance,
    PollStale,
    SchedulerStale,
    ControlBacklog,
    QuarantinedControl,
    Hold,
    IntegrityFailure,
}

#[derive(Clone, Debug)]
pub struct OperationalStatus {
    pub ready: bool,
    pub writes_ready: bool,
    pub hold_until: Option<i64>,
    pub reasons: Vec<OperationalBlocker>,
}

#[async_trait]
pub trait Transaction: Send {
    async fn now(&mut self) -> Result<i64>;
    async fn operational_ready(&mut self) -> Result<bool>;
    async fn operational_status(&mut self, _plan_id: Option<Id>) -> Result<OperationalStatus> {
        let ready = self.operational_ready().await?;
        Ok(OperationalStatus {
            ready,
            writes_ready: self.writes_ready().await?,
            hold_until: None,
            reasons: if ready {
                vec![]
            } else {
                vec![OperationalBlocker::Hold]
            },
        })
    }
    async fn writes_ready(&mut self) -> Result<bool>;
    async fn lock(&mut self, kind: Kind, id: Id) -> Result<()>;
    async fn get(&mut self, kind: Kind, id: Id) -> Result<Option<Value>>;
    async fn list(&mut self, kind: Kind, scope: Option<Id>) -> Result<Vec<Value>>;
    async fn page(
        &mut self,
        kind: Kind,
        scope: Option<Id>,
        after: Option<Id>,
        limit: i64,
    ) -> Result<Vec<Value>> {
        Ok(self
            .list(kind, scope)
            .await?
            .into_iter()
            .filter(|value| {
                after.is_none_or(|after| {
                    value["id"]
                        .as_str()
                        .and_then(|id| Id::parse_str(id).ok())
                        .is_some_and(|id| id > after)
                })
            })
            .take(limit.clamp(1, 1000) as usize)
            .collect())
    }
    async fn related_secrets(
        &mut self,
        actor: Id,
        after: Option<Id>,
        limit: i64,
    ) -> Result<Vec<Value>> {
        let actor = Value::String(actor.to_string());
        Ok(self
            .list(Kind::Secret, None)
            .await?
            .into_iter()
            .filter(|v| {
                ["guardians", "recipients"].iter().any(|role| {
                    v["policy"][role]
                        .as_array()
                        .is_some_and(|ids| ids.contains(&actor))
                })
            })
            .filter(|value| {
                after.is_none_or(|after| {
                    value["id"]
                        .as_str()
                        .and_then(|id| Id::parse_str(id).ok())
                        .is_some_and(|id| id > after)
                })
            })
            .take(limit.clamp(1, 1000) as usize)
            .collect())
    }
    async fn find(&mut self, kind: Kind, field: &str, value: &str) -> Result<Vec<Value>>;
    async fn due(&mut self, kind: Kind, now: i64, limit: i64) -> Result<Vec<Value>>;
    async fn expired(
        &mut self,
        kind: Kind,
        scope: Option<Id>,
        before: i64,
        limit: i64,
    ) -> Result<Vec<Value>> {
        Ok(self
            .list(kind, scope)
            .await?
            .into_iter()
            .filter(|value| value["expires_at"].as_i64().is_some_and(|at| at <= before))
            .take(limit.clamp(1, 1000) as usize)
            .collect())
    }
    async fn active_cases(&mut self, secret: Id) -> Result<Vec<Value>> {
        Ok(self
            .list(Kind::Case, Some(secret))
            .await?
            .into_iter()
            .filter(|value| {
                value["case"]["state"].as_str().is_some_and(|state| {
                    matches!(
                        state,
                        "collecting" | "waiting" | "ready" | "delivering" | "partial"
                    )
                })
            })
            .collect())
    }
    async fn put(&mut self, kind: Kind, id: Id, scope: Option<Id>, value: Value) -> Result<()>;
    async fn remove(&mut self, kind: Kind, id: Id) -> Result<()>;
    async fn rate_limit(&mut self, key: &str, capacity: i64, period: i64) -> Result<bool>;
    /// Reserve before external PUT/expensive work. The stable reservation ID makes retries idempotent.
    async fn reserve_resource(
        &mut self,
        _resource: &str,
        _reservation_id: Id,
        _amount: i64,
        _capacity: i64,
    ) -> Result<bool> {
        Err(Error::Storage)
    }
    /// Release only when the physical resource is confirmed gone, not merely queued for deletion.
    async fn release_resource(&mut self, _resource: &str, _reservation_id: Id) -> Result<()> {
        Err(Error::Storage)
    }
    /// Only call after credential verification (or authenticated control replay).
    async fn admit_recovery_account(
        &mut self,
        _account: &crate::Account,
        _profile_id: Id,
        _selector: Id,
    ) -> Result<()> {
        Err(Error::Storage)
    }
    async fn commit(self: Box<Self>) -> Result<()>;
}

#[async_trait]
pub trait Database: Send + Sync {
    async fn begin(&self) -> Result<Box<dyn Transaction>>;
}

pub async fn get<T: Record>(tx: &mut dyn Transaction, id: Id) -> Result<T> {
    let raw = tx.get(T::KIND, id).await?.ok_or(Error::NotFound)?;
    serde_json::from_value(raw).map_err(|_| Error::Internal)
}
pub async fn put<T: Record>(tx: &mut dyn Transaction, scope: Option<Id>, record: &T) -> Result<()> {
    tx.put(
        T::KIND,
        record.id(),
        scope,
        serde_json::to_value(record).map_err(|_| Error::Internal)?,
    )
    .await
}
pub async fn list<T: Record>(tx: &mut dyn Transaction, scope: Option<Id>) -> Result<Vec<T>> {
    tx.list(T::KIND, scope)
        .await?
        .into_iter()
        .map(|v| serde_json::from_value(v).map_err(|_| Error::Internal))
        .collect()
}
pub async fn find<T: Record>(tx: &mut dyn Transaction, field: &str, value: &str) -> Result<Vec<T>> {
    tx.find(T::KIND, field, value)
        .await?
        .into_iter()
        .map(|v| serde_json::from_value(v).map_err(|_| Error::Internal))
        .collect()
}

/// All input/output plaintext is intentionally non-Debug and zeroized on drop.
pub trait Crypto: Send + Sync {
    fn random_key(&self) -> Result<Zeroizing<Vec<u8>>>;
    fn seal(&self, key: &[u8], aad: &[u8], plaintext: &[u8]) -> Result<Envelope>;
    fn open(&self, key: &[u8], aad: &[u8], envelope: &Envelope) -> Result<Zeroizing<Vec<u8>>>;
    fn wrap(&self, context: &str, id: Id, plaintext: &[u8]) -> Result<Envelope>;
    fn unwrap(&self, context: &str, id: Id, envelope: &Envelope) -> Result<Zeroizing<Vec<u8>>>;
    fn split(&self, secret_id: Id, key: &[u8], policy: &Policy) -> Result<Vec<GuardianCode>>;
    fn verify_code(
        &self,
        secret_id: Id,
        actor: Id,
        policy: &Policy,
        key_id: &str,
        verifier: &str,
        code: &str,
    ) -> Result<Zeroizing<Vec<u8>>>;
    fn combine(&self, threshold: u8, shares: &[Zeroizing<Vec<u8>>]) -> Result<Zeroizing<Vec<u8>>>;
    fn digest(&self, bytes: &[u8]) -> String;
}

pub struct GuardianCode {
    pub account_id: Id,
    pub index: u8,
    pub code: Zeroizing<String>,
    pub verifier_key_id: String,
    pub verifier: String,
}

#[async_trait]
pub trait RecoveryHasher: Send + Sync {
    async fn issue(&self) -> Result<(Id, Zeroizing<String>, String)>;
    async fn verify(&self, token: &str, selector: Id, verifier: &str) -> Result<bool>;
}

#[async_trait]
pub trait BlobStore: Send + Sync {
    async fn put(&self, key: &str, ciphertext: &[u8]) -> Result<()>;
    async fn get(&self, key: &str, max_bytes: usize) -> Result<Vec<u8>>;
    async fn delete(&self, key: &str) -> Result<()>;
    async fn exists(&self, key: &str) -> Result<bool>;
}

#[async_trait]
pub trait ControlJournal: Send + Sync {
    async fn append(&self, intent: &ControlIntent) -> Result<u64>;
    async fn read(&self) -> Result<Vec<ControlIntent>>;
}
