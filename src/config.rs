// SPDX-License-Identifier: GPL-2.0-or-later

use std::fs;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use directories::BaseDirs;
use serde::{Deserialize, Serialize};

use crate::APP_NAME;

pub const MICROSOFT_OFFICE_CLIENT_ID: &str = "d3590ed6-52b3-4102-aeff-aad2292ab01c";
pub const MICROSOFT_GRAPH_RESOURCE: &str = "https://graph.microsoft.com";

#[derive(Clone, Debug)]
pub struct AppPaths {
    pub config_file: PathBuf,
    pub data_dir: PathBuf,
    pub cache_db: PathBuf,
    pub secrets_file: PathBuf,
    pub user_unit: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self> {
        let base = BaseDirs::new().context("could not determine your home directory")?;
        Ok(Self {
            config_file: base.config_dir().join(APP_NAME).join("config.toml"),
            data_dir: base.data_local_dir().join(APP_NAME),
            cache_db: base.data_local_dir().join(APP_NAME).join("cache.sqlite3"),
            secrets_file: base.data_local_dir().join(APP_NAME).join("secrets.json"),
            user_unit: base
                .config_dir()
                .join("systemd/user/graphmail-bridge.service"),
        })
    }

    #[doc(hidden)]
    pub fn under(root: &Path) -> Self {
        Self {
            config_file: root.join("config/config.toml"),
            data_dir: root.join("data"),
            cache_db: root.join("data/cache.sqlite3"),
            secrets_file: root.join("data/secrets.json"),
            user_unit: root.join("config/systemd/user/graphmail-bridge.service"),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub secrets: SecretsConfig,
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
    #[serde(default)]
    pub sync: SyncConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_bind")]
    pub bind: IpAddr,
    #[serde(default = "default_imap_port")]
    pub imap_port: u16,
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,
    /// Loopback HTTP port serving profile pictures (`GET /photo?address=`).
    #[serde(default = "default_photo_port")]
    pub photo_port: u16,
    /// Deprecated: folders are now synced in full. Only used as the size of
    /// the one-off Graph listing served while a folder's first sync runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_limit: Option<usize>,
}

impl ServerConfig {
    pub fn bootstrap_limit(&self) -> usize {
        self.message_limit.unwrap_or(DEFAULT_BOOTSTRAP_LIMIT).max(1)
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            imap_port: default_imap_port(),
            smtp_port: default_smtp_port(),
            photo_port: default_photo_port(),
            message_limit: None,
        }
    }
}

pub const DEFAULT_BOOTSTRAP_LIMIT: usize = 500;

/// Background synchronisation of the local message index.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SyncConfig {
    /// Seconds between delta polls of Inbox, Sent, and Drafts.
    #[serde(default = "default_inbox_poll_secs")]
    pub inbox_poll_secs: u64,
    /// Seconds between delta polls of every other folder (and folder-list refreshes).
    #[serde(default = "default_folder_poll_secs")]
    pub folder_poll_secs: u64,
    /// Messages requested per delta page (1..=500).
    #[serde(default = "default_page_size")]
    pub page_size: u32,
    /// Pause between consecutive delta pages, to stay well under Graph throttling.
    #[serde(default = "default_page_delay_ms")]
    pub page_delay_ms: u64,
    /// Upper bound of the on-disk MIME body cache.
    #[serde(default = "default_body_cache_max_mb")]
    pub body_cache_max_mb: u64,
    /// Prefetch every message body in the background (newest first) once the
    /// index is complete, until the body cache is full.
    #[serde(default)]
    pub download_bodies: bool,
    /// Seconds between checks of every calendar for changed events.
    #[serde(default = "default_calendar_poll_secs")]
    pub calendar_poll_secs: u64,
    /// Events that ended more than this many days ago are not served over
    /// CalDAV (recurring series always are); 0 serves everything.
    #[serde(default = "default_calendar_past_days")]
    pub calendar_past_days: u32,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            inbox_poll_secs: default_inbox_poll_secs(),
            folder_poll_secs: default_folder_poll_secs(),
            page_size: default_page_size(),
            page_delay_ms: default_page_delay_ms(),
            body_cache_max_mb: default_body_cache_max_mb(),
            download_bodies: false,
            calendar_poll_secs: default_calendar_poll_secs(),
            calendar_past_days: default_calendar_past_days(),
        }
    }
}

impl SyncConfig {
    pub fn body_cache_max_bytes(&self) -> u64 {
        self.body_cache_max_mb.saturating_mul(1024 * 1024)
    }
}

const fn default_inbox_poll_secs() -> u64 {
    60
}

const fn default_folder_poll_secs() -> u64 {
    600
}

const fn default_page_size() -> u32 {
    300
}

const fn default_page_delay_ms() -> u64 {
    250
}

const fn default_calendar_poll_secs() -> u64 {
    300
}

const fn default_calendar_past_days() -> u32 {
    365
}

const fn default_body_cache_max_mb() -> u64 {
    2048
}

fn default_bind() -> IpAddr {
    IpAddr::V4(Ipv4Addr::LOCALHOST)
}

const fn default_imap_port() -> u16 {
    1143
}

const fn default_photo_port() -> u16 {
    1180
}

const fn default_smtp_port() -> u16 {
    1025
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SecretBackend {
    #[default]
    Keyring,
    File,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SecretsConfig {
    #[serde(default)]
    pub backend: SecretBackend,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccountConfig {
    pub name: String,
    pub email: String,
    #[serde(default)]
    pub auth_profile: AuthProfile,
    #[serde(default = "default_tenant")]
    pub tenant: String,
    pub client_id: String,
    #[serde(default = "default_scopes")]
    pub scopes: Vec<String>,
    /// For the `goa` profile: the GNOME Online Accounts account whose
    /// Microsoft 365 token this account uses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goa_account: Option<String>,
}

/// Selects the Microsoft OAuth protocol and application identity.
///
/// `CustomEntra` remains the deserialization default so existing configuration
/// files keep their original v2/scopes behavior. The setup command explicitly
/// defaults new accounts to `MicrosoftOffice` compatibility mode.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum AuthProfile {
    MicrosoftOffice,
    #[default]
    CustomEntra,
    /// The token of a GNOME Online Accounts Microsoft 365 account, for a
    /// tenant that allows GOA's own app. No login of the bridge's own.
    Goa,
}

fn default_tenant() -> String {
    "organizations".to_owned()
}

pub fn default_scopes() -> Vec<String> {
    [
        "openid",
        "profile",
        "offline_access",
        "https://graph.microsoft.com/User.Read",
        "https://graph.microsoft.com/Mail.ReadWrite",
        "https://graph.microsoft.com/Mail.Send",
        "https://graph.microsoft.com/Calendars.ReadWrite",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

impl Config {
    pub fn load(paths: &AppPaths) -> Result<Self> {
        let raw = fs::read_to_string(&paths.config_file).with_context(|| {
            format!(
                "could not read {}; run `graphmail-bridge setup` first",
                paths.config_file.display()
            )
        })?;
        let config: Self = toml::from_str(&raw)
            .with_context(|| format!("invalid TOML in {}", paths.config_file.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn save(&self, paths: &AppPaths) -> Result<()> {
        self.validate()?;
        if let Some(parent) = paths.config_file.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }
        fs::create_dir_all(&paths.data_dir)
            .with_context(|| format!("could not create {}", paths.data_dir.display()))?;

        let serialized = toml::to_string_pretty(self).context("could not serialize config")?;
        let temp = paths.config_file.with_extension("toml.tmp");
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&temp)
            .with_context(|| format!("could not create {}", temp.display()))?;
        file.write_all(serialized.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp, &paths.config_file)?;
        fs::set_permissions(&paths.config_file, fs::Permissions::from_mode(0o600))?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if !self.server.bind.is_loopback() {
            bail!(
                "refusing non-loopback bind address {}; TLS is not implemented",
                self.server.bind
            );
        }
        if self.server.imap_port == self.server.smtp_port
            || self.server.imap_port == self.server.photo_port
            || self.server.smtp_port == self.server.photo_port
        {
            bail!("IMAP, SMTP and photo ports must all differ");
        }
        if self.server.message_limit.is_some() {
            tracing::warn!(
                "server.message_limit is deprecated: folders are synced in full; the value only sizes the bootstrap listing"
            );
        }
        if !(1..=500).contains(&self.sync.page_size) {
            bail!("sync.page_size must be between 1 and 500");
        }
        if self.sync.inbox_poll_secs < 15
            || self.sync.folder_poll_secs < 15
            || self.sync.calendar_poll_secs < 15
        {
            bail!("sync poll intervals must be at least 15 seconds");
        }
        if self.sync.body_cache_max_mb < 64 {
            bail!("sync.body_cache_max_mb must be at least 64");
        }

        let mut names = std::collections::HashSet::new();
        let mut emails = std::collections::HashSet::new();
        for account in &self.accounts {
            if account.name.trim().is_empty() || account.name.chars().any(char::is_whitespace) {
                bail!("account names must be non-empty and contain no whitespace");
            }
            if !account.email.contains('@') {
                bail!("account {} has an invalid email address", account.name);
            }
            if account.auth_profile == AuthProfile::Goa {
                if account
                    .goa_account
                    .as_deref()
                    .is_none_or(|id| id.trim().is_empty())
                {
                    bail!(
                        "account {} uses the goa profile without a GOA account",
                        account.name
                    );
                }
            } else if account.client_id.trim().is_empty() {
                bail!("account {} has no Entra client ID", account.name);
            }
            if account.auth_profile != AuthProfile::Goa && account.tenant.trim().is_empty() {
                bail!("account {} has no Entra tenant", account.name);
            }
            match account.auth_profile {
                AuthProfile::MicrosoftOffice if account.client_id != MICROSOFT_OFFICE_CLIENT_ID => {
                    bail!(
                        "account {} uses the microsoft-office profile with an unexpected client ID",
                        account.name
                    );
                }
                AuthProfile::CustomEntra if account.scopes.is_empty() => {
                    bail!("account {} has no delegated OAuth scopes", account.name);
                }
                _ => {}
            }
            if !names.insert(account.name.to_ascii_lowercase()) {
                bail!("duplicate account name {}", account.name);
            }
            if !emails.insert(account.email.to_ascii_lowercase()) {
                bail!("duplicate account email {}", account.email);
            }
        }
        Ok(())
    }

    pub fn find_account(&self, login: &str) -> Option<&AccountConfig> {
        self.accounts.iter().find(|account| {
            account.name.eq_ignore_ascii_case(login) || account.email.eq_ignore_ascii_case(login)
        })
    }

    pub fn upsert_account(&mut self, account: AccountConfig) {
        if let Some(existing) = self
            .accounts
            .iter_mut()
            .find(|item| item.name.eq_ignore_ascii_case(&account.name))
        {
            *existing = account;
        } else {
            self.accounts.push(account);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> AccountConfig {
        AccountConfig {
            name: "work".into(),
            email: "me@example.com".into(),
            auth_profile: AuthProfile::CustomEntra,
            tenant: "organizations".into(),
            client_id: "client-id".into(),
            scopes: default_scopes(),
            goa_account: None,
        }
    }

    #[test]
    fn config_round_trips_securely() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths::under(temp.path());
        let mut config = Config::default();
        config.upsert_account(account());
        config.save(&paths).unwrap();

        let loaded = Config::load(&paths).unwrap();
        assert_eq!(loaded.accounts, config.accounts);
        assert_eq!(
            fs::metadata(paths.config_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn rejects_remote_bind() {
        let mut config = Config::default();
        config.server.bind = "0.0.0.0".parse().unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn old_accounts_default_to_custom_entra() {
        let account: AccountConfig = toml::from_str(
            r#"
name = "work"
email = "me@example.com"
client_id = "existing-client"
"#,
        )
        .unwrap();
        assert_eq!(account.auth_profile, AuthProfile::CustomEntra);
    }

    #[test]
    fn office_profile_requires_the_office_client_id() {
        let mut config = Config::default();
        let mut account = account();
        account.auth_profile = AuthProfile::MicrosoftOffice;
        config.accounts.push(account);
        assert!(config.validate().is_err());
    }
}
