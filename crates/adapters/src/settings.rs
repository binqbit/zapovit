use crate::crypto::{Keyring, KeyringFile};
use application::{Error, Result};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use std::{collections::BTreeMap, path::PathBuf};
use zeroize::Zeroizing;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub data_mode: String,
    pub journal_dir: PathBuf,
    pub s3_endpoint: String,
    pub s3_region: String,
    pub s3_bucket: String,
    pub health_bind: String,
    pub telegram_api_base: String,
    pub workers: usize,
    pub database_pool: u32,
    #[serde(skip)]
    pub database_url: Zeroizing<String>,
    #[serde(skip)]
    pub telegram_bot_token: Zeroizing<String>,
    #[serde(skip)]
    pub kek_keyring: Zeroizing<String>,
    #[serde(skip)]
    pub verifier_keyring: Zeroizing<String>,
    #[serde(skip)]
    pub journal_keyring: Zeroizing<String>,
    #[serde(skip)]
    pub s3_access_key_id: Zeroizing<String>,
    #[serde(skip)]
    pub s3_secret_access_key: Zeroizing<String>,
}
impl Settings {
    pub fn load() -> Result<Self> {
        Self::from_env(environment)
    }

    fn from_env(mut read: impl FnMut(&str) -> Result<Option<String>>) -> Result<Self> {
        // Read only supported settings; unrelated process and Compose variables
        // must not become application configuration fields.
        let mut builder = config::Config::builder();
        for (name, default) in [
            ("DATA_MODE", "production"),
            ("JOURNAL_DIR", "/var/lib/zapovit/journal"),
            ("S3_ENDPOINT", "http://object-storage:3900"),
            ("S3_REGION", "garage"),
            ("S3_BUCKET", "zapovit"),
            ("HEALTH_BIND", "0.0.0.0:8080"),
            ("TELEGRAM_API_BASE", "https://api.telegram.org"),
            ("WORKERS", "4"),
            ("DATABASE_POOL", "10"),
        ] {
            let value = read(name)?.unwrap_or_else(|| default.into());
            if matches!(name, "WORKERS" | "DATABASE_POOL") {
                value
                    .parse::<usize>()
                    .map_err(|_| invalid_setting(name, "expected_integer"))?;
            }
            builder = builder
                .set_override(name.to_ascii_lowercase(), value)
                .map_err(|_| Error::Config)?;
        }
        let mut settings = builder
            .build()
            .map_err(|_| Error::Config)?
            .try_deserialize::<Self>()
            .map_err(|_| Error::Config)?;
        if !(1..=4).contains(&settings.workers) {
            return Err(invalid_setting("WORKERS", "must_be_between_1_and_4"));
        }
        if !(2..=20).contains(&settings.database_pool) {
            return Err(invalid_setting("DATABASE_POOL", "must_be_between_2_and_20"));
        }
        // Keep secret values out of the general configuration map and its copies.
        for (name, target) in [
            ("DATABASE_URL", &mut settings.database_url),
            ("TELEGRAM_BOT_TOKEN", &mut settings.telegram_bot_token),
            ("KEK_KEYRING", &mut settings.kek_keyring),
            ("VERIFIER_KEYRING", &mut settings.verifier_keyring),
            ("JOURNAL_KEYRING", &mut settings.journal_keyring),
            ("S3_ACCESS_KEY_ID", &mut settings.s3_access_key_id),
            ("S3_SECRET_ACCESS_KEY", &mut settings.s3_secret_access_key),
        ] {
            *target = bounded_secret(name, read(name)?.unwrap_or_default())?;
        }
        Ok(settings)
    }

    pub fn admit_runtime(&self) -> Result<()> {
        // No environment flag can claim an independent cryptographic audit happened.
        if self.data_mode != "synthetic" {
            return Err(invalid_setting("DATA_MODE", "synthetic_mode_required"));
        }
        for (name, value) in [
            ("DATABASE_URL", &self.database_url),
            ("TELEGRAM_BOT_TOKEN", &self.telegram_bot_token),
            ("S3_ACCESS_KEY_ID", &self.s3_access_key_id),
            ("S3_SECRET_ACCESS_KEY", &self.s3_secret_access_key),
        ] {
            if value.is_empty() {
                return Err(invalid_setting(name, "required_value_missing"));
            }
        }
        for (name, value) in [
            ("KEK_KEYRING", &self.kek_keyring),
            ("VERIFIER_KEYRING", &self.verifier_keyring),
            ("JOURNAL_KEYRING", &self.journal_keyring),
        ] {
            keyring(value).map_err(|_| invalid_setting(name, "invalid_keyring"))?;
        }
        Ok(())
    }

    pub fn storage_credentials(&self) -> StorageCredentials {
        StorageCredentials {
            access_key_id: self.s3_access_key_id.to_string(),
            secret_access_key: self.s3_secret_access_key.to_string(),
        }
    }
}

fn environment(name: &str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => Err(Error::Config),
    }
}
fn invalid_setting(setting: &str, reason: &'static str) -> Error {
    // Names come from the explicit settings list, never from secret values.
    tracing::error!(event = "configuration_invalid", setting, reason);
    Error::Config
}
fn bounded_secret(name: &str, value: String) -> Result<Zeroizing<String>> {
    let value = Zeroizing::new(value);
    if value.len() > 65536 {
        return Err(invalid_setting(name, "value_too_large"));
    }
    Ok(value)
}
pub fn read_env_secret(name: &str) -> Result<Zeroizing<String>> {
    let value = bounded_secret(
        name,
        environment(name)?.ok_or_else(|| invalid_setting(name, "required_value_missing"))?,
    )?;
    if value.is_empty() {
        return Err(invalid_setting(name, "required_value_missing"));
    }
    Ok(value)
}
pub fn keyring(value: &str) -> Result<Keyring> {
    Keyring::from_file_data(serde_json::from_str(value).map_err(|_| Error::Config)?)
}

#[derive(serde::Serialize, Deserialize)]
pub struct StorageCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
}

pub fn generate_env(output: &std::path::Path) -> Result<()> {
    use std::{fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt};
    fn random(count: usize) -> Result<Zeroizing<Vec<u8>>> {
        let mut bytes = Zeroizing::new(vec![0; count]);
        getrandom::fill(&mut bytes).map_err(|_| Error::Crypto)?;
        Ok(bytes)
    }
    let mut values: BTreeMap<&str, Zeroizing<String>> = BTreeMap::new();
    for name in ["KEK_KEYRING", "VERIFIER_KEYRING", "JOURNAL_KEYRING"] {
        let value = KeyringFile {
            active: "v1".into(),
            keys: [("v1".into(), URL_SAFE_NO_PAD.encode(&*random(32)?))].into(),
        };
        values.insert(
            name,
            Zeroizing::new(serde_json::to_string(&value).map_err(|_| Error::Internal)?),
        );
    }
    let password = Zeroizing::new(hex::encode(&*random(32)?));
    values.insert(
        "DATABASE_URL",
        Zeroizing::new(format!(
            "postgresql://zapovit:{}@db:5432/zapovit",
            password.as_str()
        )),
    );
    values.insert("DATABASE_PASSWORD", password);
    for name in [
        "GARAGE_RPC_SECRET",
        "GARAGE_ADMIN_TOKEN",
        "S3_SECRET_ACCESS_KEY",
    ] {
        values.insert(name, Zeroizing::new(hex::encode(&*random(32)?)));
    }
    values.insert(
        "S3_ACCESS_KEY_ID",
        Zeroizing::new(format!("GK{}", hex::encode(&*random(12)?))),
    );
    let mut content = Zeroizing::new(String::new());
    for line in include_str!("../../../.env.example").lines() {
        if let Some((name, _)) = line.split_once('=')
            && let Some(value) = values.get(name)
        {
            content.push_str(name);
            content.push_str("='");
            content.push_str(value);
            content.push('\'');
        } else {
            content.push_str(line);
        }
        content.push('\n');
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(output)
        .map_err(|_| Error::Storage)?;
    file.write_all(content.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|_| Error::Storage)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(values: &[(&str, &str)]) -> Result<Settings> {
        Settings::from_env(|name| {
            Ok(values
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).into()))
        })
    }

    #[test]
    fn plain_environment_ignores_unrelated_values_and_checks_bounds() {
        let s = settings(&[
            ("WORKERS", "2"),
            ("DATABASE_POOL", "12"),
            ("APP_UID", "10001"),
            ("HOME", "/tmp"),
            ("ZAPOVIT_WORKERS", "invalid"),
        ])
        .unwrap();
        assert_eq!((s.workers, s.database_pool), (2, 12));
        for (key, value) in [
            ("WORKERS", "0"),
            ("WORKERS", "5"),
            ("WORKERS", "bad"),
            ("DATABASE_POOL", "1"),
            ("DATABASE_POOL", "21"),
        ] {
            assert!(settings(&[(key, value)]).is_err());
        }
        assert!(settings(&[]).unwrap().admit_runtime().is_err());
    }

    #[test]
    fn token_comes_from_environment_and_secret_inputs_are_bounded() {
        let s = settings(&[("TELEGRAM_BOT_TOKEN", "123:synthetic")]).unwrap();
        assert_eq!(s.telegram_bot_token.as_str(), "123:synthetic");
        for name in [
            "TELEGRAM_BOT_TOKEN",
            "DATABASE_URL",
            "KEK_KEYRING",
            "VERIFIER_KEYRING",
            "JOURNAL_KEYRING",
            "S3_ACCESS_KEY_ID",
            "S3_SECRET_ACCESS_KEY",
        ] {
            assert!(settings(&[(name, &"x".repeat(65537))]).is_err());
        }
    }

    #[test]
    fn generated_env_contains_valid_independent_keys_and_never_overwrites() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join(".env");
        generate_env(&output).unwrap();
        let text = Zeroizing::new(std::fs::read_to_string(&output).unwrap());
        let mut values: BTreeMap<&str, &str> = text
            .lines()
            .filter(|line| !line.starts_with('#'))
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key, value.trim_matches('\'')))
            .collect();
        let generated =
            Settings::from_env(|name| Ok(values.get(name).map(|value| (*value).into()))).unwrap();
        assert!(generated.admit_runtime().is_err()); // Owner must supply a test bot token.
        values.insert("TELEGRAM_BOT_TOKEN", "123:synthetic");
        let configured =
            Settings::from_env(|name| Ok(values.get(name).map(|value| (*value).into()))).unwrap();
        configured.admit_runtime().unwrap();
        assert_ne!(configured.kek_keyring, configured.verifier_keyring);
        assert_ne!(configured.kek_keyring, configured.journal_keyring);
        assert_eq!(configured.s3_access_key_id.len(), 26);
        assert!(configured.s3_access_key_id.starts_with("GK"));
        assert_eq!(configured.s3_secret_access_key.len(), 64);
        assert!(
            configured
                .database_url
                .contains(values["DATABASE_PASSWORD"])
        );
        assert_eq!(
            std::fs::metadata(&output).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(generate_env(&output).is_err());
        assert_eq!(*text, std::fs::read_to_string(&output).unwrap());
    }
}
