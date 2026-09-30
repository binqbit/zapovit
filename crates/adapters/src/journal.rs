//! The journal lives outside the database backup. Never truncate it during restore.
use application::{ControlIntent, ControlJournal, Crypto, Envelope, Error, Result};
use async_trait::async_trait;
use domain::Id;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalCheckpoint {
    pub sequence: u64,
    pub digest: String,
}
type Anchor = JournalCheckpoint;

#[derive(Clone, PartialEq, Eq)]
struct Stamp(u64, u64, u64, i64, i64, i64, i64);
fn stamp(path: &Path) -> Result<Stamp> {
    let m = std::fs::symlink_metadata(path).map_err(|_| Error::Storage)?;
    if !m.is_file() {
        return Err(Error::Storage);
    }
    Ok(Stamp(
        m.dev(),
        m.ino(),
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec(),
    ))
}
struct JournalState {
    anchor: Anchor,
    bytes: u64,
    operations: BTreeMap<Id, (u64, String)>,
    stamp: Stamp,
    replica_stamp: Option<Stamp>,
    replica_anchor: Option<Anchor>,
}
pub struct FileJournal {
    root: PathBuf,
    crypto: Arc<dyn Crypto>,
    gate: Arc<Mutex<Option<JournalState>>>,
    max_bytes: u64,
    io_timeout: Duration,
    file_lock: Arc<File>,
    replica: Option<(PathBuf, Arc<File>)>,
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
        // Writable handles support exclusive locks on network filesystems that
        // implement flock using byte-range locks; append needs write access anyway.
        let file_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.join("control.jsonl"))
            .map_err(|_| Error::Storage)?;
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
            gate: Arc::new(Mutex::new(None)),
            max_bytes: MAX_JOURNAL_BYTES,
            io_timeout: Duration::from_secs(15),
            file_lock: Arc::new(file_lock),
            replica: None,
        };
        let (records, anchor, bytes) = this.load()?;
        *this.gate.try_lock().map_err(|_| Error::Internal)? = Some(Self::state(
            &this.root,
            &this.crypto,
            records,
            anchor,
            bytes,
        )?);
        Ok(this)
    }
    /// A replica is an operator-provisioned durable mount. This API verifies and
    /// fsyncs both histories; it cannot establish that two paths are on independent hosts.
    pub fn open_with_replica(
        root: PathBuf,
        replica: Option<PathBuf>,
        crypto: Arc<dyn Crypto>,
        allow_initialize: bool,
    ) -> Result<Self> {
        let mut this = Self::open_or_initialize(root, crypto, allow_initialize)?;
        if let Some(replica) = replica {
            if replica == this.root {
                return Err(Error::Config);
            }
            let empty = std::fs::read_dir(&replica)
                .map(|mut d| d.next().is_none())
                .unwrap_or(!replica.exists());
            let current = this.gate.try_lock().map_err(|_| Error::Internal)?;
            if empty && current.as_ref().is_some_and(|s| s.anchor.sequence == 0) && allow_initialize
            {
                Self::initialize(&replica)?;
            }
            drop(current);
            if std::fs::canonicalize(&replica).map_err(|_| Error::Storage)?
                == std::fs::canonicalize(&this.root).map_err(|_| Error::Storage)?
            {
                return Err(Error::Config);
            }
            let lock = Arc::new(
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(replica.join("control.jsonl"))
                    .map_err(|_| Error::Storage)?,
            );
            lock.try_lock().map_err(|_| Error::Config)?;
            this.replica = Some((replica, lock));
            let mut guard = this.gate.try_lock().map_err(|_| Error::Internal)?;
            Self::sync_replica(
                &this.root,
                this.replica.as_ref(),
                &this.crypto,
                this.max_bytes,
                guard.as_mut().ok_or(Error::Internal)?,
            )?;
        }
        Ok(this)
    }
    fn state(
        root: &Path,
        crypto: &Arc<dyn Crypto>,
        records: Vec<ControlIntent>,
        anchor: Anchor,
        bytes: u64,
    ) -> Result<JournalState> {
        let mut operations = BTreeMap::new();
        for (index, record) in records.iter().enumerate() {
            let plain = Zeroizing::new(serde_json::to_vec(record).map_err(|_| Error::Internal)?);
            if operations
                .insert(record.id, (index as u64 + 1, crypto.digest(&plain)))
                .is_some()
            {
                return Err(Error::Crypto);
            }
        }
        Ok(JournalState {
            anchor,
            bytes,
            operations,
            stamp: stamp(&root.join("control.jsonl"))?,
            replica_stamp: None,
            replica_anchor: None,
        })
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
        Self::load_at(&self.root, &self.crypto, self.max_bytes)
    }
    fn load_at(
        root: &Path,
        crypto: &Arc<dyn Crypto>,
        max_bytes: u64,
    ) -> Result<(Vec<ControlIntent>, Anchor, u64)> {
        let bytes = Self::read_bounded(&root.join("control.jsonl"), max_bytes)?;
        let anchor = Self::anchor(root)?;
        let (records, latest) = Self::verify_bytes(crypto, &bytes, &anchor)?;
        if latest.sequence > anchor.sequence {
            Self::write_anchor(root, &latest)?;
        }
        Ok((records, latest, bytes.len() as u64))
    }
    fn anchor(root: &Path) -> Result<Anchor> {
        serde_json::from_slice(&Self::read_bounded(
            &root.join("anchor.json"),
            MAX_ANCHOR_BYTES,
        )?)
        .map_err(|_| Error::Storage)
    }
    fn verify_bytes(
        crypto: &Arc<dyn Crypto>,
        bytes: &[u8],
        anchor: &Anchor,
    ) -> Result<(Vec<ControlIntent>, Anchor)> {
        if !bytes.is_empty() && bytes.last() != Some(&b'\n') {
            return Err(Error::Storage);
        }
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
            let plain = crypto.unwrap(&context, entry.operation, &entry.payload)?;
            let intent: ControlIntent =
                serde_json::from_slice(&plain).map_err(|_| Error::Crypto)?;
            if intent.id != entry.operation {
                return Err(Error::Crypto);
            }
            latest = Anchor {
                sequence: entry.sequence,
                digest: crypto.digest(line),
            };
            if anchor.sequence == latest.sequence {
                anchor_seen = anchor.digest == latest.digest;
            }
            records.push(intent);
        }
        if !anchor_seen {
            return Err(Error::Storage);
        }
        Ok((records, latest))
    }
    fn sync_replica(
        root: &Path,
        replica: Option<&(PathBuf, Arc<File>)>,
        crypto: &Arc<dyn Crypto>,
        max_bytes: u64,
        state: &mut JournalState,
    ) -> Result<()> {
        let Some((replica, _lock)) = replica else {
            return Ok(());
        };
        let replica_path = replica.join("control.jsonl");
        let actual = stamp(&replica_path)?;
        let actual_anchor = Self::anchor(replica)?;
        if let Some(expected) = &state.replica_stamp {
            if *expected != actual || state.replica_anchor.as_ref() != Some(&actual_anchor) {
                return Err(Error::Storage);
            }
        } else {
            let remote = Self::read_bounded(&replica_path, max_bytes)?;
            Self::verify_bytes(crypto, &remote, &actual_anchor)?;
            let local = Self::read_bounded(&root.join("control.jsonl"), max_bytes)?;
            if !local.starts_with(&remote) {
                return Err(Error::Storage);
            }
        }
        if actual.2 > state.bytes {
            return Err(Error::Storage);
        }
        if actual.2 < state.bytes {
            let mut source = File::open(root.join("control.jsonl")).map_err(|_| Error::Storage)?;
            source
                .seek(SeekFrom::Start(actual.2))
                .map_err(|_| Error::Storage)?;
            let mut tail = Vec::new();
            source
                .take(state.bytes - actual.2)
                .read_to_end(&mut tail)
                .map_err(|_| Error::Storage)?;
            if tail.len() as u64 != state.bytes - actual.2 {
                return Err(Error::Storage);
            }
            let mut target = OpenOptions::new()
                .append(true)
                .open(&replica_path)
                .map_err(|_| Error::Storage)?;
            target
                .write_all(&tail)
                .and_then(|()| target.sync_all())
                .map_err(|_| Error::Storage)?;
        }
        Self::write_anchor(replica, &state.anchor)?;
        state.replica_stamp = Some(stamp(&replica_path)?);
        state.replica_anchor = Some(state.anchor.clone());
        Ok(())
    }
    /// Capture an authenticated prefix without taking the live writer's OS lock.
    /// Reading the anchor first permits a newer complete tail; a partial append
    /// is rejected and the caller may retry. The source is never modified.
    pub async fn snapshot(
        root: PathBuf,
        destination: PathBuf,
        crypto: Arc<dyn Crypto>,
    ) -> Result<JournalCheckpoint> {
        tokio::task::spawn_blocking(move || {
            let mut captured = None;
            for _ in 0..8 {
                let anchor = Self::anchor(&root)?;
                let bytes = Self::read_bounded(&root.join("control.jsonl"), MAX_JOURNAL_BYTES)?;
                if let Ok((_, latest)) = Self::verify_bytes(&crypto, &bytes, &anchor) {
                    captured = Some((bytes, latest));
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            let (bytes, latest) = captured.ok_or(Error::Storage)?;
            std::fs::create_dir(&destination).map_err(|_| Error::Storage)?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination.join("control.jsonl"))
                .map_err(|_| Error::Storage)?;
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(|_| Error::Storage)?;
            Self::write_anchor(&destination, &latest)?;
            Ok(latest)
        })
        .await
        .map_err(|_| Error::Internal)?
    }
    /// The witness must come from the current independent replica, never from
    /// the content backup being restored. Matching proves integrity against that
    /// supplied checkpoint; independent freshness remains an operator obligation.
    pub async fn verify_checkpoint(
        root: PathBuf,
        witness: PathBuf,
        crypto: Arc<dyn Crypto>,
    ) -> Result<JournalCheckpoint> {
        tokio::task::spawn_blocking(move || {
            let bytes = Self::read_bounded(&root.join("control.jsonl"), MAX_JOURNAL_BYTES)?;
            let (_, latest) = Self::verify_bytes(&crypto, &bytes, &Self::anchor(&root)?)?;
            let expected: Anchor =
                serde_json::from_slice(&Self::read_bounded(&witness, MAX_ANCHOR_BYTES)?)
                    .map_err(|_| Error::Storage)?;
            if latest != expected {
                return Err(Error::Storage);
            }
            Ok(latest)
        })
        .await
        .map_err(|_| Error::Internal)?
    }
}

#[async_trait]
impl ControlJournal for FileJournal {
    async fn append(&self, intent: &ControlIntent) -> Result<u64> {
        let deadline = tokio::time::Instant::now() + self.io_timeout;
        let mut guard = tokio::time::timeout_at(deadline, self.gate.clone().lock_owned())
            .await
            .map_err(|_| Error::Storage)?;
        let root = self.root.clone();
        let crypto = self.crypto.clone();
        let replica = self.replica.clone();
        let file_lock = self.file_lock.clone();
        let limit = self.max_bytes;
        let intent = intent.clone();
        let operation = tokio::task::spawn_blocking(move || {
            let _keep_lock = file_lock;
            let state = guard.as_mut().ok_or(Error::Storage)?;
            // The OS lock excludes cooperative writers. Metadata and checkpoint
            // comparison reject replacement, truncation and out-of-band writes.
            if stamp(&root.join("control.jsonl"))? != state.stamp
                || Self::anchor(&root)? != state.anchor
            {
                return Err(Error::Storage);
            }
            let plain = Zeroizing::new(serde_json::to_vec(&intent).map_err(|_| Error::Internal)?);
            let fingerprint = crypto.digest(&plain);
            if let Some((sequence, previous)) = state.operations.get(&intent.id) {
                if previous != &fingerprint {
                    return Err(Error::Crypto);
                }
                let sequence = *sequence;
                Self::sync_replica(&root, replica.as_ref(), &crypto, limit, state)?;
                return Ok(sequence);
            }
            let sequence = state.anchor.sequence.checked_add(1).ok_or(Error::Storage)?;
            let payload = crypto.wrap(
                &format!("journal/{sequence}/{}", state.anchor.digest),
                intent.id,
                &plain,
            )?;
            let entry = Entry {
                sequence,
                previous: state.anchor.digest.clone(),
                operation: intent.id,
                payload,
            };
            let mut bytes = serde_json::to_vec(&entry).map_err(|_| Error::Internal)?;
            let digest = crypto.digest(&bytes);
            bytes.push(b'\n');
            let total = state
                .bytes
                .checked_add(bytes.len() as u64)
                .filter(|size| *size <= limit)
                .ok_or(Error::Storage)?;
            let mut file = OpenOptions::new()
                .append(true)
                .open(root.join("control.jsonl"))
                .map_err(|_| Error::Storage)?;
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(|_| Error::Storage)?;
            let anchor = Anchor { sequence, digest };
            Self::write_anchor(&root, &anchor)?;
            state.bytes = total;
            state.anchor = anchor;
            state.stamp = stamp(&root.join("control.jsonl"))?;
            state.operations.insert(intent.id, (sequence, fingerprint));
            // Never acknowledge until the configured replica is durable as well.
            // On replica failure, the staged application scope stays paused.
            Self::sync_replica(&root, replica.as_ref(), &crypto, limit, state)?;
            Ok(sequence)
        });
        // A blocked filesystem cannot be safely cancelled. Its task keeps the
        // serialization guard; timeout never acknowledges the control, and a
        // later retry must verify both durable copies before acknowledging it.
        tokio::time::timeout_at(deadline, operation)
            .await
            .map_err(|_| Error::Storage)?
            .map_err(|_| Error::Internal)?
    }
    async fn read(&self) -> Result<Vec<ControlIntent>> {
        let guard = self.gate.clone().lock_owned().await;
        let root = self.root.clone();
        let crypto = self.crypto.clone();
        let file_lock = self.file_lock.clone();
        let limit = self.max_bytes;
        tokio::task::spawn_blocking(move || {
            let _keep_lock = file_lock;
            let state = guard.as_ref().ok_or(Error::Storage)?;
            if stamp(&root.join("control.jsonl"))? != state.stamp
                || Self::anchor(&root)? != state.anchor
            {
                return Err(Error::Storage);
            }
            Self::load_at(&root, &crypto, limit).map(|v| v.0)
        })
        .await
        .map_err(|_| Error::Internal)?
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

    #[tokio::test]
    async fn live_snapshot_is_consistent_and_requires_current_restore_witness() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("live");
        let journal = FileJournal::open_or_initialize(root.clone(), crypto(), true).unwrap();
        journal.append(&intent()).await.unwrap();
        let snapshot = dir.path().join("snapshot");
        let checkpoint = FileJournal::snapshot(root.clone(), snapshot.clone(), crypto())
            .await
            .unwrap();
        assert_eq!(checkpoint.sequence, 1);
        FileJournal::verify_checkpoint(snapshot.clone(), root.join("anchor.json"), crypto())
            .await
            .unwrap();
        journal.append(&intent()).await.unwrap();
        assert!(
            FileJournal::verify_checkpoint(snapshot, root.join("anchor.json"), crypto())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn replica_failure_never_acknowledges_and_retry_preserves_one_record() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("live");
        let replica = dir.path().join("replica");
        let journal =
            FileJournal::open_with_replica(root.clone(), Some(replica.clone()), crypto(), true)
                .unwrap();
        journal.append(&intent()).await.unwrap();
        let anchor = std::fs::read(replica.join("anchor.json")).unwrap();
        std::fs::remove_file(replica.join("anchor.json")).unwrap();
        let record = intent();
        assert!(journal.append(&record).await.is_err());
        std::fs::write(replica.join("anchor.json"), &anchor).unwrap();
        assert_eq!(journal.append(&record).await.unwrap(), 2);
        assert_eq!(journal.append(&record).await.unwrap(), 2);
        assert_eq!(
            std::fs::read(root.join("control.jsonl")).unwrap(),
            std::fs::read(replica.join("control.jsonl")).unwrap()
        );
        assert_eq!(journal.read().await.unwrap().len(), 2);
        let first_line = std::fs::read(root.join("control.jsonl"))
            .unwrap()
            .split_inclusive(|b| *b == b'\n')
            .next()
            .unwrap()
            .to_vec();
        drop(journal);
        // A content snapshot cannot roll back a newer independent replica.
        std::fs::write(root.join("control.jsonl"), first_line).unwrap();
        std::fs::write(root.join("anchor.json"), anchor).unwrap();
        assert!(FileJournal::open_with_replica(root, Some(replica), crypto(), false).is_err());
    }

    #[tokio::test]
    async fn cached_append_rejects_external_history_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let journal = FileJournal::open_or_initialize(dir.path().into(), crypto(), true).unwrap();
        journal.append(&intent()).await.unwrap();
        let path = dir.path().join("control.jsonl");
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, bytes).unwrap();
        assert!(journal.append(&intent()).await.is_err());
    }

    #[tokio::test]
    async fn a_busy_journal_has_a_bounded_response_without_acknowledgement() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal =
            FileJournal::open_or_initialize(dir.path().into(), crypto(), true).unwrap();
        journal.io_timeout = Duration::from_millis(25);
        let held = journal.gate.lock().await;
        let record = intent();
        assert!(journal.append(&record).await.is_err());
        drop(held);
        assert!(journal.read().await.unwrap().is_empty());
        assert_eq!(journal.append(&record).await.unwrap(), 1);
    }

    #[tokio::test]
    #[ignore = "synthetic filesystem performance probe; run explicitly and retain its JSON output"]
    async fn journal_append_scale_probe() {
        let dir = tempfile::tempdir().unwrap();
        let journal = FileJournal::open_with_replica(
            dir.path().join("primary"),
            Some(dir.path().join("replica")),
            crypto(),
            true,
        )
        .unwrap();
        let mut early = Vec::new();
        let mut late = Vec::new();
        let began = std::time::Instant::now();
        for index in 0..10_000 {
            let at = std::time::Instant::now();
            journal.append(&intent()).await.unwrap();
            let micros = at.elapsed().as_micros() as u64;
            if index < 100 {
                early.push(micros);
            }
            if index >= 9_900 {
                late.push(micros);
            }
        }
        early.sort_unstable();
        late.sort_unstable();
        println!(
            "{}",
            serde_json::json!({
                "probe":"journal_append_scale", "records":10000,
                "replicas":2, "same_host_synthetic":true,
                "seconds":began.elapsed().as_secs_f64(),
                "journal_bytes":std::fs::metadata(dir.path().join("primary/control.jsonl")).unwrap().len(),
                "first_100_p50_us":early[49], "first_100_p95_us":early[94],
                "last_100_p50_us":late[49], "last_100_p95_us":late[94]
            })
        );
        assert_eq!(journal.read().await.unwrap().len(), 10_000);
    }
}
