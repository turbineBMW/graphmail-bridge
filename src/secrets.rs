// SPDX-License-Identifier: GPL-2.0-or-later

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::KEYRING_SERVICE;
use crate::config::{AppPaths, SecretBackend};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccountSecrets {
    pub bridge_password: String,
    pub refresh_token: String,
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub access_token_expires_at: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct SecretStore {
    backend: SecretBackend,
    file: std::path::PathBuf,
}

impl SecretStore {
    pub fn new(backend: SecretBackend, paths: &AppPaths) -> Self {
        Self {
            backend,
            file: paths.secrets_file.clone(),
        }
    }

    pub fn backend(&self) -> SecretBackend {
        self.backend
    }

    pub fn load(&self, account: &str) -> Result<AccountSecrets> {
        match self.backend {
            SecretBackend::Keyring => {
                let entry = keyring::Entry::new(KEYRING_SERVICE, account)
                    .context("could not open the desktop keyring")?;
                let raw = entry.get_password().with_context(|| {
                    format!("no credentials found in the keyring for account {account}")
                })?;
                serde_json::from_str(&raw).context("keyring credential is corrupt")
            }
            SecretBackend::File => {
                let entries = read_file_entries(&self.file)?;
                entries
                    .get(account)
                    .cloned()
                    .with_context(|| format!("no credentials found for account {account}"))
            }
        }
    }

    pub fn save(&self, account: &str, secrets: &AccountSecrets) -> Result<()> {
        match self.backend {
            SecretBackend::Keyring => {
                let entry = keyring::Entry::new(KEYRING_SERVICE, account)
                    .context("could not open the desktop keyring")?;
                let raw = serde_json::to_string(secrets)?;
                entry
                    .set_password(&raw)
                    .context("could not store credentials in the desktop keyring")
            }
            SecretBackend::File => {
                let mut entries = if self.file.exists() {
                    read_file_entries(&self.file)?
                } else {
                    BTreeMap::new()
                };
                entries.insert(account.to_owned(), secrets.clone());
                write_file_entries(&self.file, &entries)
            }
        }
    }

    pub fn remove(&self, account: &str) -> Result<()> {
        match self.backend {
            SecretBackend::Keyring => {
                let entry = keyring::Entry::new(KEYRING_SERVICE, account)?;
                entry
                    .delete_credential()
                    .context("could not delete credential")
            }
            SecretBackend::File => {
                let mut entries = read_file_entries(&self.file)?;
                entries.remove(account);
                write_file_entries(&self.file, &entries)
            }
        }
    }
}

fn read_file_entries(path: &Path) -> Result<BTreeMap<String, AccountSecrets>> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("could not read secret file {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("secret file {} is corrupt", path.display()))
}

fn write_file_entries(path: &Path, entries: &BTreeMap<String, AccountSecrets>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("json.tmp");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&temp)
        .with_context(|| format!("could not create {}", temp.display()))?;
    file.write_all(serde_json::to_string_pretty(entries)?.as_bytes())?;
    file.sync_all()?;
    fs::rename(temp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_store_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::under(temp.path());
        let store = SecretStore::new(SecretBackend::File, &paths);
        let value = AccountSecrets {
            bridge_password: "local-only".into(),
            refresh_token: "refresh".into(),
            access_token: Some("access".into()),
            access_token_expires_at: Some(123),
        };
        store.save("work", &value).unwrap();
        assert_eq!(store.load("work").unwrap().refresh_token, "refresh");
        store.remove("work").unwrap();
        assert!(store.load("work").is_err());
    }
}
