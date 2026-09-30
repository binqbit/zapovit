//! The journal lives outside the database backup. Never truncate it during restore.
use application::{ControlIntent, ControlJournal, Crypto, Envelope, Error, Result};
use async_trait::async_trait;
use domain::Id;
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Mutex;
use zeroize::Zeroizing;

const MAX_JOURNAL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ANCHOR_BYTES: u64 = 1024;

#[derive(Serialize, Deserialize)]
struct Entry {
    sequence: u64,
    previous: String,
    operation: Id,
    payload: Envelope,
}
#[derive(Serialize, Deserialize)]
struct Anchor {
    sequence: u64,
    digest: String,
}
pub struct FileJournal {
    root: PathBuf,
    crypto: Arc<dyn Crypto>,
    gate: Mutex<()>,
    max_bytes: u64,
    _file_lock: File,
}
impl FileJournal {
    pub fn initialize(root: &Path) -> Result<()> {
        std::fs::create_dir_all(root).map_err(|_| Error::Storage)?;
        // Even an orphaned anchor or temporary file is evidence of an earlier
        // journal. Never overwrite that evidence with a new empty history.
        if std::fs::read_dir(root)
            .map_err(|_| Error::Storage)?
            .next()
            .is_some()
        {
            return Err(Error::Storage);
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join("control.jsonl"))
            .map_err(|_| Error::Storage)?;
        file.flush()
            .and_then(|()| file.sync_all())
            .map_err(|_| Error::Storage)?;
        let anchor = Anchor {
            sequence: 0,
            digest: String::new(),
        };
        Self::write_anchor(root, &anchor)?;
        Ok(())
    }
    /// The caller holds database leadership and checks whether the database
    /// is unused before granting permission to create a new journal.
    pub fn open_or_initialize(
        root: PathBuf,
        crypto: Arc<dyn Crypto>,
        allow_initialize: bool,
    ) -> Result<Self> {
        let empty = match std::fs::read_dir(&root) {
            Ok(mut entries) => entries.next().is_none(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => return Err(Error::Storage),
        };
        if empty && allow_initialize {
            Self::initialize(&root)?;
            tracing::info!(event = "journal_initialized");
        }
        Self::open(root, crypto)
    }
    pub fn open(root: PathBuf, crypto: Arc<dyn Crypto>) -> Result<Self> {
        if !root.join("control.jsonl").is_file() || !root.join("anchor.json").is_file() {
            tracing::error!(
                event = "journal_unavailable",
                reason = "required_files_missing",
                hint = "Startup initializes an empty journal only for an unused database. Restore the current journal for an existing installation and check volume permissions; do not initialize a replacement."
            );
            return Err(Error::Config);
        }
        // Keep the OS lock for the instance lifetime. It fences another process
        // even if database leadership changes before the old process shuts down.
        let file_lock = File::open(root.join("control.jsonl")).map_err(|_| Error::Storage)?;
        file_lock.try_lock().map_err(|_| {
            tracing::error!(
                event = "journal_unavailable",
                reason = "writer_lock_unavailable",
                hint = "Check for another running app process using the same journal volume."
            );
            Error::Config
        })?;
        let this = Self {
            root,
            crypto,
            gate: Mutex::new(()),
            max_bytes: MAX_JOURNAL_BYTES,
            _file_lock: file_lock,
        };
        this.load()?;
        Ok(this)
    }
    fn write_anchor(root: &Path, anchor: &Anchor) -> Result<()> {
        let mut file = File::create(root.join("anchor.tmp")).map_err(|_| Error::Storage)?;
        file.write_all(&serde_json::to_vec(anchor).map_err(|_| Error::Internal)?)
            .and_then(|()| file.sync_all())
            .map_err(|_| Error::Storage)?;
        std::fs::rename(root.join("anchor.tmp"), root.join("anchor.json"))
            .map_err(|_| Error::Storage)?;
        File::open(root)
            .and_then(|f| f.sync_all())
            .map_err(|_| Error::Storage)
    }
    fn read_bounded(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
        let file = File::open(path).map_err(|_| Error::Storage)?;
        let metadata = file.metadata().map_err(|_| Error::Storage)?;
        if !metadata.is_file() || metadata.len() > max_bytes {
            return Err(Error::Storage);
        }
        // The read bound also covers a file growing after the metadata check.
        let mut bytes = Vec::new();
        file.take(max_bytes + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Storage)?;
        if bytes.len() as u64 > max_bytes {
            return Err(Error::Storage);
        }
        Ok(bytes)
    }
    fn load(&self) -> Result<(Vec<ControlIntent>, Anchor, u64)> {
        let bytes = Self::read_bounded(&self.root.join("control.jsonl"), self.max_bytes)?;
        if !bytes.is_empty() && bytes.last() != Some(&b'\n') {
            return Err(Error::Storage);
        }
        let anchor: Anchor = serde_json::from_slice(&Self::read_bounded(
            &self.root.join("anchor.json"),
            MAX_ANCHOR_BYTES,
        )?)
        .map_err(|_| Error::Storage)?;
        let mut latest = Anchor {
            sequence: 0,
            digest: String::new(),
        };
        let mut records = Vec::new();
        let mut anchor_seen = anchor.sequence == 0 && anchor.digest.is_empty();
        for line in bytes.split(|b| *b == b'\n').filter(|v| !v.is_empty()) {
            let entry: Entry = serde_json::from_slice(line).map_err(|_| Error::Storage)?;
            if entry.sequence != latest.sequence + 1 || entry.previous != latest.digest {
                return Err(Error::Storage);
            }
            let context = format!("journal/{}/{}", entry.sequence, entry.previous);
            let plain = self
                .crypto
                .unwrap(&context, entry.operation, &entry.payload)?;
            let intent: ControlIntent =
                serde_json::from_slice(&plain).map_err(|_| Error::Crypto)?;
            if intent.id != entry.operation {
                return Err(Error::Crypto);
            }
            latest = Anchor {
                sequence: entry.sequence,
                digest: self.crypto.digest(line),
            };
            if anchor.sequence == latest.sequence {
                anchor_seen = anchor.digest == latest.digest;
            }
            records.push(intent);
        }
        if !anchor_seen {
            return Err(Error::Storage);
        }
        // A crash after fsync(data), before fsync(anchor), can only leave a valid authenticated tail.
        if latest.sequence > anchor.sequence {
            Self::write_anchor(&self.root, &latest)?;
        }
        Ok((records, latest, bytes.len() as u64))
    }
}

#[async_trait]
impl ControlJournal for FileJournal {
    async fn append(&self, intent: &ControlIntent) -> Result<u64> {
        let _guard = self.gate.lock().await;
        let (records, anchor, existing_bytes) = self.load()?;
        if let Some(index) = records.iter().position(|r| r.id == intent.id) {
            if serde_json::to_vec(&records[index]).map_err(|_| Error::Internal)?
                != serde_json::to_vec(intent).map_err(|_| Error::Internal)?
            {
                return Err(Error::Crypto);
            }
            return Ok(index as u64 + 1);
        }
        let sequence = anchor.sequence + 1;
        let plain = Zeroizing::new(serde_json::to_vec(intent).map_err(|_| Error::Internal)?);
        let payload = self.crypto.wrap(
            &format!("journal/{sequence}/{}", anchor.digest),
            intent.id,
            &plain,
        )?;
        let entry = Entry {
            sequence,
            previous: anchor.digest,
            operation: intent.id,
            payload,
        };
        let mut bytes = serde_json::to_vec(&entry).map_err(|_| Error::Internal)?;
        let digest = self.crypto.digest(&bytes);
        bytes.push(b'\n');
        // Never acknowledge an append that would make the next replay unreadable.
        // The caller keeps its staged scope paused if journal capacity is exhausted.
        if existing_bytes
            .checked_add(bytes.len() as u64)
            .is_none_or(|size| size > self.max_bytes)
        {
            return Err(Error::Storage);
        }
        let mut file = OpenOptions::new()
            .append(true)
            .open(self.root.join("control.jsonl"))
            .map_err(|_| Error::Storage)?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|_| Error::Storage)?;
        Self::write_anchor(&self.root, &Anchor { sequence, digest })?;
        Ok(sequence)
    }
    async fn read(&self) -> Result<Vec<ControlIntent>> {
        let _guard = self.gate.lock().await;
        self.load().map(|v| v.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{CryptoAdapter, Keyring, KeyringFile};
    use application::Control;
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use domain::PlanState;

    fn crypto() -> Arc<dyn Crypto> {
        let keyring = |n| {
            Keyring::from_file_data(KeyringFile {
                active: "test".into(),
                keys: [("test".into(), URL_SAFE_NO_PAD.encode([n; 32]))].into(),
            })
            .unwrap()
        };
        Arc::new(CryptoAdapter::new(keyring(31), keyring(42)))
    }

    fn intent() -> ControlIntent {
        ControlIntent {
            id: Id::new_v4(),
            profile_id: Id::new_v4(),
            plan_id: Id::new_v4(),
            epoch: 3,
            owner_epoch: 1,
            at: 10,
            previous_state: PlanState::Active,
            operation: Control::Stop,
            applied: false,
            secret_epoch: None,
        }
    }

    #[tokio::test]
    async fn automatic_initialization_is_idempotent_and_preserves_records() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("journal");
        let journal = FileJournal::open_or_initialize(root.clone(), crypto(), true).unwrap();
        let record = intent();
        journal.append(&record).await.unwrap();
        let bytes = std::fs::read(root.join("control.jsonl")).unwrap();
        let anchor = std::fs::read(root.join("anchor.json")).unwrap();
        drop(journal);
        for allow in [true, false] {
            let journal = FileJournal::open_or_initialize(root.clone(), crypto(), allow).unwrap();
            assert_eq!(journal.read().await.unwrap()[0].id, record.id);
            assert_eq!(std::fs::read(root.join("control.jsonl")).unwrap(), bytes);
            assert_eq!(std::fs::read(root.join("anchor.json")).unwrap(), anchor);
        }
    }

    #[test]
    fn automatic_initialization_requires_permission_and_an_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("journal");
        assert!(FileJournal::open_or_initialize(root.clone(), crypto(), false).is_err());
        assert!(!root.exists());
        std::fs::create_dir(&root).unwrap();
        assert!(FileJournal::open_or_initialize(root.clone(), crypto(), false).is_err());
        for name in ["anchor.json", "control.jsonl", "anchor.tmp"] {
            let file = root.join(name);
            std::fs::write(&file, b"preserve-me").unwrap();
            assert!(FileJournal::open_or_initialize(root.clone(), crypto(), true).is_err());
            assert!(FileJournal::initialize(&root).is_err());
            assert_eq!(std::fs::read(&file).unwrap(), b"preserve-me");
            assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
            std::fs::remove_file(file).unwrap();
        }
        FileJournal::open_or_initialize(root, crypto(), true).unwrap();
    }

    #[tokio::test]
    async fn capacity_rejection_preserves_replay_and_idempotence() {
        let dir = tempfile::tempdir().unwrap();
        FileJournal::initialize(dir.path()).unwrap();
        let mut journal = FileJournal::open(dir.path().into(), crypto()).unwrap();
        let first = intent();
        assert_eq!(journal.append(&first).await.unwrap(), 1);
        let bytes = std::fs::read(dir.path().join("control.jsonl")).unwrap();
        let anchor = std::fs::read(dir.path().join("anchor.json")).unwrap();
        journal.max_bytes = bytes.len() as u64;

        assert!(journal.append(&intent()).await.is_err());
        assert_eq!(
            std::fs::read(dir.path().join("control.jsonl")).unwrap(),
            bytes
        );
        assert_eq!(
            std::fs::read(dir.path().join("anchor.json")).unwrap(),
            anchor
        );
        assert_eq!(journal.append(&first).await.unwrap(), 1);
        let replay = journal.read().await.unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].id, first.id);
    }

    #[tokio::test]
    async fn oversized_journal_and_anchor_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        FileJournal::initialize(dir.path()).unwrap();
        let mut journal = FileJournal::open(dir.path().into(), crypto()).unwrap();
        journal.append(&intent()).await.unwrap();
        journal.max_bytes = 1;
        assert!(journal.read().await.is_err());

        let dir = tempfile::tempdir().unwrap();
        FileJournal::initialize(dir.path()).unwrap();
        let mut anchor = vec![b' '; 4096];
        anchor.extend_from_slice(br#"{"sequence":0,"digest":""}"#);
        std::fs::write(dir.path().join("anchor.json"), anchor).unwrap();
        assert!(FileJournal::open(dir.path().into(), crypto()).is_err());
    }

    #[tokio::test]
    async fn authenticated_tail_recovers_old_anchor_but_tampered_tail_fails() {
        let dir = tempfile::tempdir().unwrap();
        FileJournal::initialize(dir.path()).unwrap();
        let journal = FileJournal::open(dir.path().into(), crypto()).unwrap();
        journal.append(&intent()).await.unwrap();
        let old_anchor = std::fs::read(dir.path().join("anchor.json")).unwrap();
        journal.append(&intent()).await.unwrap();
        let latest_anchor = std::fs::read(dir.path().join("anchor.json")).unwrap();
        drop(journal);

        // Simulate data fsync succeeding before the new anchor was persisted.
        std::fs::write(dir.path().join("anchor.json"), &old_anchor).unwrap();
        let recovered = FileJournal::open(dir.path().into(), crypto()).unwrap();
        assert_eq!(recovered.read().await.unwrap().len(), 2);
        assert_eq!(
            std::fs::read(dir.path().join("anchor.json")).unwrap(),
            latest_anchor
        );
        drop(recovered);

        let bytes = std::fs::read(dir.path().join("control.jsonl")).unwrap();
        let first_end = bytes.iter().position(|b| *b == b'\n').unwrap() + 1;
        let mut tail: Entry = serde_json::from_slice(&bytes[first_end..]).unwrap();
        let first = &tail.payload.ciphertext[..1];
        let replacement = if first == "A" { "B" } else { "A" };
        tail.payload.ciphertext.replace_range(..1, replacement);
        let mut tampered = bytes[..first_end].to_vec();
        tampered.extend_from_slice(&serde_json::to_vec(&tail).unwrap());
        tampered.push(b'\n');
        std::fs::write(dir.path().join("control.jsonl"), tampered).unwrap();
        std::fs::write(dir.path().join("anchor.json"), &old_anchor).unwrap();
        assert!(FileJournal::open(dir.path().into(), crypto()).is_err());
        assert_eq!(
            std::fs::read(dir.path().join("anchor.json")).unwrap(),
            old_anchor
        );
    }

    #[tokio::test]
    async fn only_one_open_instance_may_own_the_journal() {
        let dir = tempfile::tempdir().unwrap();
        FileJournal::initialize(dir.path()).unwrap();
        let journal = FileJournal::open(dir.path().into(), crypto()).unwrap();
        assert!(FileJournal::open(dir.path().into(), crypto()).is_err());
        journal.append(&intent()).await.unwrap();
        drop(journal);
        let reopened = FileJournal::open(dir.path().into(), crypto()).unwrap();
        assert_eq!(reopened.read().await.unwrap().len(), 1);
    }
}
