use crate::{
    crypto::CryptoAdapter,
    journal::FileJournal,
    postgres::PgDatabase,
    settings::{Settings, keyring},
    storage::S3Storage,
};
use application::{BlobStore, Database, Error, FileObject, Result};
use domain::Id;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};
use tokio::io::AsyncReadExt;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Object {
    key: String,
    file: String,
    digest: String,
}
async fn storage(settings: &Settings) -> Result<(PgDatabase, S3Storage)> {
    let db = PgDatabase::connect(&settings.database_url, 2).await?;
    let credentials = settings.storage_credentials();
    let storage = S3Storage::new(
        &settings.s3_endpoint,
        &settings.s3_region,
        &settings.s3_bucket,
        &credentials.access_key_id,
        &credentials.secret_access_key,
    )?;
    Ok((db, storage))
}
pub async fn maintenance(settings: &Settings, enabled: bool) -> Result<()> {
    let db = PgDatabase::connect(&settings.database_url, 2).await?;
    set_maintenance(&db, enabled).await
}
pub async fn set_maintenance(db: &PgDatabase, enabled: bool) -> Result<()> {
    // Publish the hold and the mode change together: release workers must never
    // observe maintenance disabled before the restore/backup safety hold exists.
    let mut tx = db.pool.begin().await.map_err(|_| Error::Storage)?;
    let unsafe_exit: bool = sqlx::query_scalar("SELECT restore_required OR backup_session IS NOT NULL FROM maintenance WHERE singleton FOR UPDATE")
        .fetch_one(&mut *tx).await.map_err(|_| Error::Storage)?;
    if !enabled && unsafe_exit {
        return Err(Error::Config);
    }
    sqlx::query("UPDATE maintenance SET enabled=$1,changed_at=clock_timestamp(),backup_session=NULL,backup_started_at=NULL,backup_drained=false,backup_checkpoint=NULL WHERE singleton")
        .bind(enabled)
        .execute(&mut *tx)
        .await
        .map_err(|_| Error::Storage)?;
    if !enabled {
        sqlx::query("UPDATE telegram_cursor SET hold_until=GREATEST(hold_until,floor(extract(epoch from clock_timestamp()))::bigint+86400) WHERE singleton").execute(&mut *tx).await.map_err(|_|Error::Storage)?;
    }
    tx.commit().await.map_err(|_| Error::Storage)
}

pub async fn begin_backup(settings: &Settings) -> Result<Id> {
    begin_backup_on_db(&PgDatabase::connect(&settings.database_url, 2).await?).await
}
pub async fn begin_backup_on_db(db: &PgDatabase) -> Result<Id> {
    // The database dump itself must retain a restore gate. Only this live
    // session can clear it after snapshot+archive completion; the dumped row
    // has no checkpoint and cannot be completed as if it were the live session.
    let session = Id::new_v4();
    let result = sqlx::query("UPDATE maintenance SET enabled=true,changed_at=clock_timestamp(),backup_session=$1,backup_started_at=clock_timestamp(),backup_drained=false,backup_checkpoint=NULL,restore_required=true WHERE singleton AND NOT enabled AND NOT restore_required AND backup_session IS NULL")
        .bind(session).execute(&db.pool).await.map_err(|_| Error::Storage)?;
    if result.rows_affected() != 1 {
        return Err(Error::Config);
    }
    Ok(session)
}
pub async fn wait_for_drain(settings: &Settings, session: Id, timeout: Duration) -> Result<()> {
    let db = PgDatabase::connect(&settings.database_url, 2).await?;
    wait_for_drain_on_db(&db, session, timeout).await
}
pub async fn wait_for_drain_on_db(db: &PgDatabase, session: Id, timeout: Duration) -> Result<()> {
    if timeout.is_zero() || timeout > Duration::from_secs(600) {
        return Err(Error::Config);
    }
    tokio::time::timeout(timeout, async {
        loop {
            let mut tx = db.pool.begin().await.map_err(|_| Error::Storage)?;
            let owned: bool = sqlx::query_scalar("SELECT enabled AND COALESCE(backup_session=$1,false) FROM maintenance WHERE singleton FOR UPDATE")
                .bind(session).fetch_one(&mut *tx).await.map_err(|_| Error::Storage)?;
            if !owned { return Err(Error::Config); }
            // New worker claims and uploads are excluded by maintenance. Existing
            // I/O has bounded timeouts; recent pending uploads retain a conservative
            // two-minute fence even if a worker disappeared before bookkeeping.
            let drained: bool = sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM outbox WHERE state IN ('claimed','dispatching') AND (data->>'lease_until')::bigint > floor(extract(epoch from clock_timestamp()))::bigint) AND NOT EXISTS(SELECT 1 FROM file_objects WHERE state='pending' AND (data->>'created_at')::bigint > floor(extract(epoch from clock_timestamp()))::bigint-$1)")
                .bind(application::MAX_BLOB_OPERATION_SECS as i64 + 10)
                .fetch_one(&mut *tx).await.map_err(|_| Error::Storage)?;
            if drained {
                sqlx::query("UPDATE maintenance SET backup_drained=true WHERE singleton").execute(&mut *tx).await.map_err(|_| Error::Storage)?;
                return tx.commit().await.map_err(|_| Error::Storage);
            }
            tx.commit().await.map_err(|_| Error::Storage)?;
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }).await.map_err(|_| Error::Storage)?
}
fn journal_crypto(settings: &Settings) -> Result<Arc<dyn application::Crypto>> {
    Ok(Arc::new(CryptoAdapter::new(
        keyring(&settings.journal_keyring)?,
        keyring(&settings.journal_keyring)?,
    )))
}
pub async fn snapshot_journal(settings: &Settings, session: Id, directory: &Path) -> Result<()> {
    let db = PgDatabase::connect(&settings.database_url, 2).await?;
    let mut tx = db.pool.begin().await.map_err(|_| Error::Storage)?;
    let ready: bool = sqlx::query_scalar("SELECT enabled AND COALESCE(backup_session=$1,false) AND backup_drained FROM maintenance WHERE singleton FOR UPDATE")
        .bind(session).fetch_one(&mut *tx).await.map_err(|_| Error::Storage)?;
    if !ready {
        return Err(Error::Config);
    }
    let checkpoint = FileJournal::snapshot(
        settings.journal_dir.clone(),
        directory.into(),
        journal_crypto(settings)?,
    )
    .await?;
    sqlx::query("UPDATE maintenance SET backup_checkpoint=$1 WHERE singleton")
        .bind(sqlx::types::Json(checkpoint))
        .execute(&mut *tx)
        .await
        .map_err(|_| Error::Storage)?;
    tx.commit().await.map_err(|_| Error::Storage)
}
pub async fn complete_backup(settings: &Settings, session: Id) -> Result<()> {
    complete_backup_on_db(
        &PgDatabase::connect(&settings.database_url, 2).await?,
        session,
    )
    .await
}
pub async fn complete_backup_on_db(db: &PgDatabase, session: Id) -> Result<()> {
    // Deliberately leave every existing operational hold untouched.
    let result = sqlx::query("UPDATE maintenance SET enabled=false,changed_at=clock_timestamp(),backup_session=NULL,backup_started_at=NULL,backup_drained=false,backup_checkpoint=NULL,restore_required=false,last_backup_at=clock_timestamp() WHERE singleton AND enabled AND backup_session=$1 AND backup_drained AND backup_checkpoint IS NOT NULL")
        .bind(session).execute(&db.pool).await.map_err(|_| Error::Storage)?;
    if result.rows_affected() != 1 {
        return Err(Error::Config);
    }
    Ok(())
}
pub async fn require_restore(settings: &Settings) -> Result<()> {
    require_restore_on_db(&PgDatabase::connect(&settings.database_url, 2).await?).await
}
pub async fn require_restore_on_db(db: &PgDatabase) -> Result<()> {
    sqlx::query("UPDATE maintenance SET enabled=true,changed_at=clock_timestamp(),restore_required=true,restore_checkpoint=NULL,backup_session=NULL,backup_started_at=NULL,backup_drained=false,backup_checkpoint=NULL WHERE singleton")
        .execute(&db.pool).await.map_err(|_| Error::Storage)?;
    Ok(())
}
pub async fn verify_restore(settings: &Settings, directory: &Path, witness: &Path) -> Result<()> {
    if std::fs::canonicalize(directory).map_err(|_| Error::Storage)?
        != std::fs::canonicalize(&settings.journal_dir).map_err(|_| Error::Storage)?
    {
        return Err(Error::Config);
    }
    let db = PgDatabase::connect(&settings.database_url, 2).await?;
    let mut tx = db.pool.begin().await.map_err(|_| Error::Storage)?;
    let held: bool = sqlx::query_scalar(
        "SELECT enabled AND restore_required FROM maintenance WHERE singleton FOR UPDATE",
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(|_| Error::Storage)?;
    if !held {
        return Err(Error::Config);
    }
    let checkpoint =
        FileJournal::verify_checkpoint(directory.into(), witness.into(), journal_crypto(settings)?)
            .await?;
    sqlx::query(
        "UPDATE maintenance SET restore_required=false,restore_checkpoint=$1 WHERE singleton",
    )
    .bind(sqlx::types::Json(checkpoint))
    .execute(&mut *tx)
    .await
    .map_err(|_| Error::Storage)?;
    tx.commit().await.map_err(|_| Error::Storage)
}
pub async fn export_objects(settings: &Settings, directory: &Path) -> Result<()> {
    let (db, storage) = storage(settings).await?;
    export_objects_from(&db, &storage, directory).await
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestPage {
    file: String,
    digest: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PagedManifest {
    version: u8,
    objects: u64,
    pages: Vec<ManifestPage>,
}
enum ImportPage {
    Legacy(Vec<Object>),
    Verified(ManifestPage),
}

pub async fn export_objects_from(
    db: &PgDatabase,
    storage: &dyn BlobStore,
    directory: &Path,
) -> Result<()> {
    let drained: bool = sqlx::query_scalar("SELECT enabled AND backup_session IS NOT NULL AND backup_drained FROM maintenance WHERE singleton")
        .fetch_one(&db.pool).await.map_err(|_| Error::Storage)?;
    if !drained {
        return Err(Error::Config);
    }
    std::fs::create_dir(directory).map_err(|_| Error::Storage)?;
    let mut cursor: Option<Id> = None;
    let mut scanned_pages = 0;
    let mut manifest = PagedManifest {
        version: 2,
        objects: 0,
        pages: Vec::new(),
    };
    loop {
        let rows: Vec<(Id, serde_json::Value)> = sqlx::query_as(
            "SELECT id,data FROM file_objects WHERE ($1::uuid IS NULL OR id>$1) ORDER BY id LIMIT 250",
        )
        .bind(cursor)
        .fetch_all(&db.pool)
        .await
        .map_err(|_| Error::Storage)?;
        if rows.is_empty() {
            break;
        }
        scanned_pages += 1;
        if scanned_pages > 4096 {
            return Err(Error::Config);
        }
        let mut page = Vec::new();
        for (id, raw) in rows {
            let object: FileObject = serde_json::from_value(raw).map_err(|_| Error::Storage)?;
            if object.id != id {
                return Err(Error::Storage);
            }
            cursor = Some(object.id);
            if !storage.exists(&object.key).await? {
                if matches!(object.state.as_str(), "pending" | "gc" | "deleted") {
                    continue;
                }
                return Err(Error::Storage);
            }
            let bytes = storage.get(&object.key, 15 * 1024 * 1024).await?;
            let digest = hex::encode(Sha256::digest(&bytes));
            if !object.digest.is_empty() && object.digest != digest {
                return Err(Error::Crypto);
            }
            let file = object.id.simple().to_string();
            tokio::fs::write(directory.join(&file), bytes)
                .await
                .map_err(|_| Error::Storage)?;
            page.push(Object {
                key: object.key,
                file,
                digest,
            });
        }
        if !page.is_empty() {
            let file = format!("manifest-{:04}.json", manifest.pages.len());
            let bytes = serde_json::to_vec(&page).map_err(|_| Error::Internal)?;
            manifest.objects += page.len() as u64;
            manifest.pages.push(ManifestPage {
                file: file.clone(),
                digest: hex::encode(Sha256::digest(&bytes)),
            });
            tokio::fs::write(directory.join(file), bytes)
                .await
                .map_err(|_| Error::Storage)?;
        }
    }
    tokio::fs::write(
        directory.join("manifest.json"),
        serde_json::to_vec(&manifest).map_err(|_| Error::Internal)?,
    )
    .await
    .map_err(|_| Error::Storage)
}
pub async fn import_objects(settings: &Settings, directory: &Path) -> Result<()> {
    let (db, storage) = storage(settings).await?;
    import_objects_into(&db, &storage, directory).await
}
pub async fn import_objects_into(
    db: &PgDatabase,
    storage: &dyn BlobStore,
    directory: &Path,
) -> Result<()> {
    let mut tx = db.begin().await?;
    if tx.writes_ready().await? {
        return Err(Error::Config);
    }
    tx.commit().await?;
    let expected_live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM file_objects WHERE state NOT IN ('pending','gc','deleted')",
    )
    .fetch_one(&db.pool)
    .await
    .map_err(|_| Error::Storage)?;
    let bytes = read_bounded(&directory.join("manifest.json"), 8 * 1024 * 1024).await?;
    let mut pages = Vec::new();
    let legacy: Option<Vec<Object>>;
    let expected_count: u64;
    if bytes.first() == Some(&b'[') {
        let objects: Vec<Object> = serde_json::from_slice(&bytes).map_err(|_| Error::Config)?;
        if objects.len() > 10_000 {
            return Err(Error::Config);
        }
        expected_count = objects.len() as u64;
        legacy = Some(objects);
    } else {
        let manifest: PagedManifest = serde_json::from_slice(&bytes).map_err(|_| Error::Config)?;
        if manifest.version != 2 || manifest.pages.len() > 4096 {
            return Err(Error::Config);
        }
        expected_count = manifest.objects;
        pages = manifest.pages;
        legacy = None;
    }
    let mut last_id = None;
    let mut count = 0_u64;
    let mut live = 0_i64;
    let inputs = legacy
        .into_iter()
        .map(ImportPage::Legacy)
        .chain(pages.into_iter().map(ImportPage::Verified));
    for (index, input) in inputs.enumerate() {
        let objects = match input {
            // Legacy exports are bounded by their original 10,000-object contract.
            ImportPage::Legacy(objects) => objects,
            ImportPage::Verified(page) => {
                if page.file != format!("manifest-{index:04}.json") {
                    return Err(Error::Config);
                }
                let bytes = read_bounded(&directory.join(&page.file), 256 * 1024).await?;
                if hex::encode(Sha256::digest(&bytes)) != page.digest {
                    return Err(Error::Crypto);
                }
                let objects: Vec<Object> =
                    serde_json::from_slice(&bytes).map_err(|_| Error::Config)?;
                if objects.is_empty() || objects.len() > 250 {
                    return Err(Error::Config);
                }
                objects
            }
        };
        for object in objects {
            let id = Id::parse_str(&object.file).map_err(|_| Error::Config)?;
            if id.simple().to_string() != object.file || last_id.is_some_and(|last| id <= last) {
                return Err(Error::Config);
            }
            last_id = Some(id);
            let raw: serde_json::Value =
                sqlx::query_scalar("SELECT data FROM file_objects WHERE id=$1")
                    .bind(id)
                    .fetch_one(&db.pool)
                    .await
                    .map_err(|_| Error::Storage)?;
            let recorded: FileObject = serde_json::from_value(raw).map_err(|_| Error::Storage)?;
            validate_manifest(
                std::slice::from_ref(&object),
                std::slice::from_ref(&recorded),
            )?;
            if !matches!(recorded.state.as_str(), "pending" | "gc" | "deleted") {
                live += 1;
            }
            let bytes = read_bounded(&directory.join(&object.file), 15 * 1024 * 1024).await?;
            if hex::encode(Sha256::digest(&bytes)) != object.digest {
                return Err(Error::Crypto);
            }
            storage.put(&object.key, &bytes).await?;
            count += 1;
        }
    }
    if count != expected_count || live != expected_live {
        return Err(Error::Config);
    }
    Ok(())
}

fn validate_manifest(objects: &[Object], ledger: &[FileObject]) -> Result<()> {
    if objects.len() > 10_000 {
        return Err(Error::Config);
    }
    let mut expected: BTreeMap<_, _> = ledger
        .iter()
        .map(|object| (object.id.simple().to_string(), object))
        .collect();
    for object in objects {
        let recorded = expected.remove(&object.file).ok_or(Error::Config)?;
        if object.key != recorded.key
            || !(object.key.starts_with("draft/") || object.key.starts_with("sealed/"))
            || object.digest.len() != 64
            || !object.digest.bytes().all(|b| b.is_ascii_hexdigit())
            || (!recorded.digest.is_empty() && object.digest != recorded.digest)
        {
            return Err(Error::Config);
        }
    }
    // Pending/deleted objects may legitimately be absent. Every live encrypted
    // reference in the restored database must have a manifest entry.
    if expected
        .values()
        .any(|object| !matches!(object.state.as_str(), "pending" | "gc" | "deleted"))
    {
        return Err(Error::Config);
    }
    Ok(())
}

async fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|_| Error::Storage)?;
    if !metadata.is_file() || metadata.len() > limit as u64 {
        return Err(Error::Config);
    }
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|_| Error::Storage)?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| Error::Storage)?;
    if bytes.len() > limit {
        return Err(Error::Config);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn backup_reads_are_bounded_and_reject_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("object");
        std::fs::write(&path, [42; 32]).unwrap();
        assert!(read_bounded(&path, 31).await.is_err());
        assert_eq!(read_bounded(&path, 32).await.unwrap(), [42; 32]);
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_bounded(&link, 32).await.is_err());
    }
    #[test]
    fn manifest_requires_exact_live_ledger_references() {
        let id = domain::Id::new_v4();
        let ledger = vec![FileObject {
            id,
            plan_id: domain::Id::new_v4(),
            draft_id: domain::Id::new_v4(),
            operation_id: domain::Id::new_v4(),
            key: format!("sealed/{id}"),
            state: "sealed".into(),
            size: 16,
            digest: "ab".repeat(32),
            created_at: 0,
            due_at: i64::MAX,
        }];
        assert!(validate_manifest(&[], &ledger).is_err());
        let mut manifest = vec![Object {
            key: ledger[0].key.clone(),
            file: id.simple().to_string(),
            digest: ledger[0].digest.clone(),
        }];
        validate_manifest(&manifest, &ledger).unwrap();
        manifest[0].key = "sealed/different".into();
        assert!(validate_manifest(&manifest, &ledger).is_err());
        manifest[0].key = ledger[0].key.clone();
        manifest[0].digest = "cd".repeat(32);
        assert!(validate_manifest(&manifest, &ledger).is_err());
        manifest[0].digest = ledger[0].digest.clone();
        manifest[0].file = "../outside".into();
        assert!(validate_manifest(&manifest, &ledger).is_err());
    }
}
