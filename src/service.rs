// SPDX-License-Identifier: GPL-2.0-or-later

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tokio::net::TcpListener;
use tokio::sync::broadcast;

use crate::config::{AccountConfig, AppPaths, Config};
use crate::graph::GraphClient;
use crate::oauth::TokenManager;
use crate::secrets::SecretStore;
use crate::store::Store;
use crate::sync::FolderChange;

pub struct AccountRuntime {
    pub config: AccountConfig,
    pub token_manager: Arc<TokenManager>,
    pub graph: GraphClient,
    /// Held while a calendar is synced or written, so a sync never stores a
    /// copy older than a write that raced it.
    pub calendar_lock: tokio::sync::Mutex<()>,
}

pub struct Runtime {
    pub config: Arc<Config>,
    pub store: Arc<Store>,
    /// Folder-change notifications from the sync tasks to idling sessions.
    pub changes: broadcast::Sender<FolderChange>,
    accounts: HashMap<String, Arc<AccountRuntime>>,
    /// Each configured account exactly once (the map above has two keys per account).
    unique_accounts: Vec<Arc<AccountRuntime>>,
}

impl Runtime {
    pub fn load(config: Config, paths: &AppPaths) -> Result<Arc<Self>> {
        let store = SecretStore::new(config.secrets.backend, paths);
        let mut accounts = HashMap::new();
        let mut unique_accounts = Vec::new();
        for account in &config.accounts {
            let token_manager =
                Arc::new(TokenManager::from_store(account, &store).with_context(|| {
                    format!("could not load credentials for account {}", account.name)
                })?);
            let runtime = Arc::new(AccountRuntime {
                config: account.clone(),
                graph: GraphClient::new(token_manager.clone()),
                token_manager,
                calendar_lock: tokio::sync::Mutex::new(()),
            });
            accounts.insert(account.name.to_ascii_lowercase(), runtime.clone());
            accounts.insert(account.email.to_ascii_lowercase(), runtime.clone());
            unique_accounts.push(runtime);
        }
        if accounts.is_empty() {
            bail!("no accounts configured; run `graphmail-bridge setup`");
        }
        let (changes, _) = broadcast::channel(256);
        Ok(Arc::new(Self {
            config: Arc::new(config),
            store: Arc::new(Store::open(&paths.cache_db)?),
            changes,
            accounts,
            unique_accounts,
        }))
    }

    pub fn accounts(&self) -> &[Arc<AccountRuntime>] {
        &self.unique_accounts
    }

    pub async fn authenticate(&self, login: &str, password: &str) -> Option<Arc<AccountRuntime>> {
        let account = self.accounts.get(&login.to_ascii_lowercase())?.clone();
        account
            .token_manager
            .bridge_password_matches(password)
            .await
            .then_some(account)
    }
}

pub async fn serve(runtime: Arc<Runtime>) -> Result<()> {
    let bind = runtime.config.server.bind;
    let imap_address = (bind, runtime.config.server.imap_port);
    let smtp_address = (bind, runtime.config.server.smtp_port);
    let photo_address = (bind, runtime.config.server.photo_port);
    let imap_listener = TcpListener::bind(imap_address)
        .await
        .with_context(|| format!("could not listen on {bind}:{}", imap_address.1))?;
    let smtp_listener = TcpListener::bind(smtp_address)
        .await
        .with_context(|| format!("could not listen on {bind}:{}", smtp_address.1))?;
    let photo_listener = TcpListener::bind(photo_address)
        .await
        .with_context(|| format!("could not listen on {bind}:{}", photo_address.1))?;
    tracing::info!(address = %imap_listener.local_addr()?, "IMAP ready");
    tracing::info!(address = %photo_listener.local_addr()?, "HTTP (photos, CalDAV) ready");
    tracing::info!(address = %smtp_listener.local_addr()?, "SMTP ready");
    for account in runtime.accounts() {
        crate::sync::spawn(runtime.clone(), account.clone());
        crate::calendar::sync::spawn(runtime.clone(), account.clone());
    }

    tokio::try_join!(
        crate::imap::serve(imap_listener, runtime.clone()),
        crate::smtp::serve(smtp_listener, runtime.clone()),
        crate::http::serve(photo_listener, runtime),
    )?;
    Ok(())
}

pub fn install_user_unit(paths: &AppPaths, executable: &Path) -> Result<()> {
    let Some(parent) = paths.user_unit.parent() else {
        bail!("invalid systemd unit path");
    };
    fs::create_dir_all(parent)?;
    let escaped = systemd_escape_path(executable)?;
    let unit = format!(
        "[Unit]\nDescription=Microsoft Graph IMAP/SMTP bridge\nAfter=graphical-session.target\n\n[Service]\nType=simple\nExecStart={escaped} serve\nRestart=on-failure\nRestartSec=5s\nNoNewPrivileges=true\nPrivateTmp=true\nProtectSystem=strict\nProtectControlGroups=true\nProtectKernelModules=true\nProtectKernelTunables=true\nLockPersonality=true\nRestrictSUIDSGID=true\n\n[Install]\nWantedBy=default.target\n"
    );
    fs::write(&paths.user_unit, unit)
        .with_context(|| format!("could not write {}", paths.user_unit.display()))?;
    Ok(())
}

fn systemd_escape_path(path: &Path) -> Result<String> {
    let text = path
        .to_str()
        .context("the executable path is not valid UTF-8")?;
    if text.contains(['\n', '\r']) {
        bail!("invalid newline in executable path");
    }
    Ok(format!(
        "\"{}\"",
        text.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_uses_absolute_executable() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = AppPaths::under(temporary.path());
        install_user_unit(&paths, Path::new("/opt/Graph Mail/graphmail-bridge")).unwrap();
        let unit = fs::read_to_string(paths.user_unit).unwrap();
        assert!(unit.contains("ExecStart=\"/opt/Graph Mail/graphmail-bridge\" serve"));
        assert!(unit.contains("WantedBy=default.target"));
    }
}
