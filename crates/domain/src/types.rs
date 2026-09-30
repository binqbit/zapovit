use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroize;

pub type Id = Uuid;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RuleError {
    #[error("access_denied")]
    AccessDenied,
    #[error("invalid_policy")]
    InvalidPolicy,
    #[error("invalid_state")]
    InvalidState,
    #[error("expired")]
    Expired,
    #[error("quota_exceeded")]
    QuotaExceeded,
    #[error("not_ready")]
    NotReady,
    #[error("stale_action")]
    StaleAction,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanState {
    Setup,
    Active,
    Paused,
    DeletionPending,
    Deleted,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SecretState {
    Provisioning,
    Armed,
    Paused,
    SetupFailed,
    Partial,
    NeedsAttention,
    Delivered,
    Deleted,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PartState {
    Queued,
    Claimed,
    Dispatching,
    Sent,
    RetryableFailed,
    PermanentFailed,
    Unknown,
    Cancelled,
}

impl PartState {
    pub fn after_worker_loss(self) -> Self {
        match self {
            Self::Claimed => Self::Queued,
            Self::Dispatching => Self::Unknown,
            state => state,
        }
    }
    pub fn after_stop(self) -> Self {
        match self {
            Self::Sent | Self::Dispatching | Self::Unknown => self,
            _ => Self::Cancelled,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileRef {
    pub id: Id,
    pub object_key: String,
    pub encrypted_size: u64,
    pub sha256: String,
}

/// Serialized only inside an authenticated encrypted envelope. Never log or Debug.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    Copyable {
        text: String,
    },
    Spoiler {
        text: String,
    },
    File {
        file: FileRef,
        name: String,
        caption: String,
    },
}

impl Drop for Block {
    fn drop(&mut self) {
        match self {
            Self::Text { text } | Self::Copyable { text } | Self::Spoiler { text } => {
                text.zeroize()
            }
            Self::File { name, caption, .. } => {
                name.zeroize();
                caption.zeroize();
            }
        }
    }
}

impl Block {
    pub fn text_bytes(&self) -> usize {
        match self {
            Self::Text { text } | Self::Copyable { text } | Self::Spoiler { text } => text.len(),
            Self::File { name, caption, .. } => name.len() + caption.len(),
        }
    }
}

pub fn validate_blocks(blocks: &[Block]) -> Result<(), RuleError> {
    if blocks.is_empty()
        || blocks.len() > 20
        || blocks.iter().map(Block::text_bytes).sum::<usize>() > 32768
    {
        return Err(RuleError::QuotaExceeded);
    }
    let files: Vec<_> = blocks
        .iter()
        .filter_map(|b| {
            if let Block::File { file, .. } = b {
                Some(file)
            } else {
                None
            }
        })
        .collect();
    if files.len() > 3
        || files
            .iter()
            .any(|f| f.encrypted_size > 10 * 1024 * 1024 + 4096)
        || files.iter().map(|f| f.encrypted_size).sum::<u64>() > 25 * 1024 * 1024 + 12288
    {
        return Err(RuleError::QuotaExceeded);
    }
    Ok(())
}
