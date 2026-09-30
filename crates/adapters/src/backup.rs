use crate::{postgres::PgDatabase, settings::Settings, storage::S3Storage};
use application::{BlobStore, Database, Error, FileObject, Result, list};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path};
use tokio::io::AsyncReadExt;

#[derive(Serialize, Deserialize)]
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
    sqlx::query("UPDATE maintenance SET enabled=$1,changed_at=clock_timestamp() WHERE singleton")
        .bind(enabled)
        .execute(&mut *tx)
        .await
        .map_err(|_| Error::Storage)?;
    if !enabled {
        sqlx::query("UPDATE telegram_cursor SET hold_until=GREATEST(hold_until,floor(extract(epoch from clock_timestamp()))::bigint+86400) WHERE singleton").execute(&mut *tx).await.map_err(|_|Error::Storage)?;
    }
    tx.commit().await.map_err(|_| Error::Storage)
}
pub async fn export_objects(settings: &Settings, directory: &Path) -> Result<()> {
    let (db, storage) = storage(settings).await?;
    let mut tx = db.begin().await?;
    if tx.writes_ready().await? {
        return Err(Error::Config);
    }
    let objects = list::<FileObject>(&mut *tx, None).await?;
    tx.commit().await?;
    std::fs::create_dir(directory).map_err(|_| Error::Storage)?;
    let mut manifest = Vec::new();
    for object in objects {
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
        manifest.push(Object {
            key: object.key,
            file,
            digest,
        });
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
    let mut tx = db.begin().await?;
    if tx.writes_ready().await? {
        return Err(Error::Config);
    }
    let ledger = list::<FileObject>(&mut *tx, None).await?;
    tx.commit().await?;
    let bytes = read_bounded(&directory.join("manifest.json"), 8 * 1024 * 1024).await?;
    let objects: Vec<Object> = serde_json::from_slice(&bytes).map_err(|_| Error::Config)?;
    validate_manifest(&objects, &ledger)?;
    for object in objects {
        let bytes = read_bounded(&directory.join(&object.file), 15 * 1024 * 1024).await?;
        if hex::encode(Sha256::digest(&bytes)) != object.digest {
            return Err(Error::Crypto);
        }
        storage.put(&object.key, &bytes).await?;
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
