// SPDX-License-Identifier: GPL-2.0-or-later

//! Adding an account and registering it with Evolution Data Server, as a
//! library: the CLI's `setup` and `eds-setup` commands run these, and so
//! can a program that embeds the bridge.

use std::fs;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use base64::Engine;

use crate::config::{
    AccountConfig, AppPaths, AuthProfile, Config, MICROSOFT_OFFICE_CLIENT_ID, SecretBackend,
    default_scopes,
};
use crate::eds;
use crate::graph::GraphClient;
use crate::oauth::{DeviceCode, TokenManager, device_login_with};
use crate::secrets::SecretStore;

/// What adding an account needs to know.
#[derive(Clone, Debug)]
pub struct NewAccount {
    /// The short name the bridge knows it by (no whitespace).
    pub name: String,
    pub email: String,
    pub auth_profile: AuthProfile,
    /// `custom-entra`: the tenant and the app registration.
    pub tenant: Option<String>,
    pub client_id: Option<String>,
    /// `goa`: the GNOME Online Accounts account to take tokens from.
    pub goa_account: Option<String>,
    /// Keep secrets in a mode-0600 file rather than the desktop keyring.
    pub file_secrets: bool,
}

/// Add (or re-add) an account: sign in -- `on_code` shows the device code
/// -- confirm Graph answers, and save the configuration. Returns the
/// account and the address Microsoft knows it by.
pub async fn add_account(
    paths: &AppPaths,
    new: NewAccount,
    on_code: &(dyn Fn(&DeviceCode) + Send + Sync),
) -> Result<(AccountConfig, String)> {
    let (tenant, client_id, scopes) = match new.auth_profile {
        AuthProfile::MicrosoftOffice => {
            if let Some(supplied) = new.client_id.as_deref()
                && supplied.trim() != MICROSOFT_OFFICE_CLIENT_ID
            {
                bail!(
                    "the Microsoft Office identity has a fixed client ID; use the custom-entra profile"
                );
            }
            (
                new.tenant.unwrap_or_else(|| "common".to_owned()),
                MICROSOFT_OFFICE_CLIENT_ID.to_owned(),
                Vec::new(),
            )
        }
        AuthProfile::CustomEntra => (
            new.tenant.unwrap_or_else(|| "organizations".to_owned()),
            new.client_id
                .filter(|id| !id.trim().is_empty())
                .context("the custom-entra profile needs an Entra application (client) ID")?,
            default_scopes(),
        ),
        AuthProfile::Goa => ("common".to_owned(), String::new(), Vec::new()),
    };
    let account = AccountConfig {
        name: new.name.trim().to_owned(),
        email: new.email.trim().to_owned(),
        auth_profile: new.auth_profile,
        tenant: tenant.trim().to_owned(),
        client_id: client_id.trim().to_owned(),
        scopes,
        goa_account: new.goa_account,
    };

    let mut config = if paths.config_file.exists() {
        let existing = Config::load(paths)?;
        if new.file_secrets
            && existing.secrets.backend != SecretBackend::File
            && !existing.accounts.is_empty()
        {
            bail!(
                "refusing to switch an existing multi-account configuration to file secrets; migrate its credentials first"
            );
        }
        existing
    } else {
        Config::default()
    };
    if new.file_secrets {
        config.secrets.backend = SecretBackend::File;
    }
    config.upsert_account(account.clone());
    config.validate()?;

    let store = SecretStore::new(config.secrets.backend, paths);
    // A re-added account keeps the password its mail clients already have.
    let password = store
        .load(&account.name)
        .map(|secrets| secrets.bridge_password)
        .unwrap_or_else(|_| generate_bridge_password());
    let secrets = device_login_with(&account, &store, password, on_code).await?;
    let token_manager = Arc::new(TokenManager::new(account.clone(), store, secrets));
    let profile = GraphClient::new(token_manager).profile().await?;
    config.save(paths)?;
    let address = profile.mail.unwrap_or(profile.user_principal_name);
    Ok((account, address))
}

/// Register an account's calendars (CalDAV) and mail (IMAP/SMTP) with
/// Evolution Data Server, and give EDS its bridge password. The sources go
/// in as files, which the registry may only notice once restarted; a
/// program on the session bus can create them through it instead, from
/// `eds_sources`.
pub async fn register_with_eds(paths: &AppPaths, account_name: &str) -> Result<()> {
    let directory = eds::sources_dir()?;
    let sources = eds_sources(paths, account_name).await?;
    // Children first, so the collection never points at missing sources.
    for (source_uid, contents) in sources.iter().rev() {
        eds::install(&directory, source_uid, contents)?;
    }
    Ok(())
}

/// An account's EDS sources as (UID, key file) pairs, parents first, with
/// its bridge password already stored where EDS looks for it.
pub async fn eds_sources(paths: &AppPaths, account_name: &str) -> Result<Vec<(String, String)>> {
    let config = Config::load(paths)?;
    let account = config
        .find_account(account_name)
        .with_context(|| format!("account {account_name:?} is not configured"))?;
    let uid = eds::source_uid(account);
    let store = SecretStore::new(config.secrets.backend, paths);
    let secrets = store.load(&account.name)?;
    // The From name comes from the directory; the short account name is a
    // fallback when Graph cannot be reached.
    let manager = Arc::new(TokenManager::from_store(account, &store)?);
    let name = match GraphClient::new(manager).profile().await {
        Ok(profile) => profile.display_name.unwrap_or_else(|| account.name.clone()),
        Err(error) => {
            tracing::warn!("could not read the account's name from Microsoft Graph: {error:#}");
            account.name.clone()
        }
    };
    eds::store_password(&uid, &eds::display_name(account), &secrets.bridge_password)?;
    Ok(eds::sources(&config, account, &name))
}

/// Take an account's sources out of Evolution Data Server again.
pub fn unregister_from_eds(paths: &AppPaths, account_name: &str) -> Result<()> {
    let config = Config::load(paths)?;
    let account = config
        .find_account(account_name)
        .with_context(|| format!("account {account_name:?} is not configured"))?;
    let directory = eds::sources_dir()?;
    for (source_uid, _) in eds::sources(&config, account, "") {
        let path = directory.join(format!("{source_uid}.source"));
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("could not remove {}", path.display()))?;
        }
    }
    eds::clear_password(&eds::source_uid(account))
}

/// The password a local mail client signs in to the bridge with.
pub fn generate_bridge_password() -> String {
    let mut bytes = [0_u8; 24];
    rand::fill(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}
