// SPDX-License-Identifier: GPL-2.0-or-later

use std::fs;
use std::io::IsTerminal;
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use base64::Engine;
use chrono::{DateTime, SecondsFormat, Utc};
use clap::{Parser, Subcommand};
use dialoguer::{Input, Select};
use graphmail_bridge::config::{
    AccountConfig, AppPaths, AuthProfile, Config, MICROSOFT_OFFICE_CLIENT_ID, SecretBackend,
    default_scopes,
};
use graphmail_bridge::graph::GraphClient;
use graphmail_bridge::oauth::{TokenManager, device_login};
use graphmail_bridge::secrets::SecretStore;
use graphmail_bridge::service::{Runtime, install_user_unit};
use graphmail_bridge::store::Store;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    version,
    about = "A lightweight Microsoft Graph to local IMAP/SMTP bridge"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Add an account using a guided Microsoft device-code login.
    Setup {
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        email: Option<String>,
        #[arg(long)]
        client_id: Option<String>,
        /// OAuth identity/protocol profile. The default needs no app registration.
        #[arg(long, value_enum, default_value = "microsoft-office")]
        auth_profile: AuthProfile,
        #[arg(long)]
        tenant: Option<String>,
        /// Use a mode-0600 JSON secret file instead of the desktop keyring.
        #[arg(long)]
        file_secrets: bool,
    },
    /// Repeat Microsoft authorization for an existing account.
    Login {
        /// Account name or email. Optional when only one account is configured.
        account: Option<String>,
    },
    /// Run the foreground IMAP/SMTP bridge.
    Serve,
    /// Verify configuration, credentials, and Graph access.
    Doctor {
        /// Force a refresh-token exchange before checking Graph.
        #[arg(long)]
        refresh: bool,
    },
    /// Show how far the local message index has synced for each folder.
    SyncStatus,
    /// Export a UTC window of Microsoft 365 events as JSON.
    CalendarEvents {
        /// Account name or email. Optional when only one account is configured.
        account: Option<String>,
        #[arg(long)]
        start: DateTime<Utc>,
        #[arg(long)]
        end: DateTime<Utc>,
    },
    /// Print settings to enter in a local mail client.
    ClientConfig {
        /// Account name or email. Optional when only one account is configured.
        account: Option<String>,
    },
    /// Install and optionally start the systemd user service.
    InstallService {
        #[arg(long)]
        no_start: bool,
    },
    /// Disable and remove the systemd user service.
    UninstallService,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
    let cli = Cli::parse();
    let paths = AppPaths::discover()?;
    match cli.command {
        Commands::Setup {
            name,
            email,
            client_id,
            auth_profile,
            tenant,
            file_secrets,
        } => {
            setup(
                &paths,
                name,
                email,
                client_id,
                auth_profile,
                tenant,
                file_secrets,
            )
            .await
        }
        Commands::Login { account } => login(&paths, account.as_deref()).await,
        Commands::Serve => {
            let config = Config::load(&paths)?;
            graphmail_bridge::service::serve(Runtime::load(config, &paths)?).await
        }
        Commands::Doctor { refresh } => doctor(&paths, refresh).await,
        Commands::SyncStatus => sync_status(&paths),
        Commands::CalendarEvents {
            account,
            start,
            end,
        } => calendar_events(&paths, account.as_deref(), start, end).await,
        Commands::ClientConfig { account } => print_client_config(&paths, account.as_deref()),
        Commands::InstallService { no_start } => install_service(&paths, !no_start),
        Commands::UninstallService => uninstall_service(&paths),
    }
}

async fn setup(
    paths: &AppPaths,
    name: Option<String>,
    email: Option<String>,
    client_id: Option<String>,
    auth_profile: AuthProfile,
    tenant: Option<String>,
    file_secrets: bool,
) -> Result<()> {
    match auth_profile {
        AuthProfile::MicrosoftOffice => println!(
            "Compatibility login uses Microsoft's public Office client identity with the\n\
             legacy resource-based Graph device flow. Your tenant audit log will identify\n\
             this sign-in as Microsoft Office. Conditional Access still applies.\n"
        ),
        AuthProfile::CustomEntra => println!(
            "Custom Entra login uses your own public-client app. It needs delegated\n\
             Mail.ReadWrite, Mail.Send, and User.Read permissions with public client\n\
             flows enabled. No client secret is used.\n"
        ),
    }
    let name = prompt(name, "Short account name", Some("work"))?;
    let email = prompt(email, "Microsoft 365 email address", None)?;
    let (tenant, client_id, scopes) = match auth_profile {
        AuthProfile::MicrosoftOffice => {
            if let Some(supplied) = client_id.as_deref()
                && supplied.trim() != MICROSOFT_OFFICE_CLIENT_ID
            {
                bail!(
                    "--client-id cannot override the fixed Microsoft Office identity; use --auth-profile custom-entra"
                );
            }
            (
                tenant.unwrap_or_else(|| "common".to_owned()),
                MICROSOFT_OFFICE_CLIENT_ID.to_owned(),
                Vec::new(),
            )
        }
        AuthProfile::CustomEntra => (
            tenant.unwrap_or_else(|| "organizations".to_owned()),
            prompt(client_id, "Entra application (client) ID", None)?,
            default_scopes(),
        ),
    };
    let account = AccountConfig {
        name: name.trim().to_owned(),
        email: email.trim().to_owned(),
        auth_profile,
        tenant: tenant.trim().to_owned(),
        client_id: client_id.trim().to_owned(),
        scopes,
    };

    let mut config = if paths.config_file.exists() {
        let existing = Config::load(paths)?;
        if file_secrets
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
    if file_secrets {
        config.secrets.backend = SecretBackend::File;
    }
    config.upsert_account(account.clone());
    config.validate()?;

    let store = SecretStore::new(config.secrets.backend, paths);
    let password = generate_bridge_password();
    let secrets = device_login(&account, &store, password).await?;
    let token_manager = Arc::new(TokenManager::new(account.clone(), store, secrets));
    let profile = GraphClient::new(token_manager).profile().await?;
    config.save(paths)?;

    println!(
        "\nAuthorized as {}. Configuration saved to {}.",
        profile
            .mail
            .as_deref()
            .unwrap_or(&profile.user_principal_name),
        paths.config_file.display()
    );
    print_client_config(paths, Some(&account.name))?;
    println!("\nRun `graphmail-bridge install-service` when the settings work.");
    Ok(())
}

async fn login(paths: &AppPaths, account_name: Option<&str>) -> Result<()> {
    let config = Config::load(paths)?;
    let account = resolve_account(&config, account_name)?;
    let store = SecretStore::new(config.secrets.backend, paths);
    let (password, password_is_new) = match store.load(&account.name) {
        Ok(secrets) => (secrets.bridge_password, false),
        Err(error) => {
            eprintln!(
                "warning: existing credentials could not be read ({error:#}); a new bridge password will be generated"
            );
            (generate_bridge_password(), true)
        }
    };
    let secrets = device_login(account, &store, password).await?;
    let token_manager = Arc::new(TokenManager::new(account.clone(), store, secrets));
    let profile = GraphClient::new(token_manager).profile().await?;
    println!(
        "Authorization refreshed for {}.",
        profile
            .mail
            .as_deref()
            .unwrap_or(&profile.user_principal_name)
    );
    if password_is_new {
        println!("\nThe bridge password changed; update your mail client:");
        print_client_config(paths, Some(&account.name))?;
    }
    Ok(())
}

async fn doctor(paths: &AppPaths, refresh: bool) -> Result<()> {
    let config = Config::load(paths)?;
    println!("OK  config: {}", paths.config_file.display());
    println!("OK  loopback bind: {}", config.server.bind);
    println!("OK  secret backend: {:?}", config.secrets.backend);
    let store = SecretStore::new(config.secrets.backend, paths);
    let mut failures = 0usize;
    for account in &config.accounts {
        let result = async {
            let manager = Arc::new(TokenManager::from_store(account, &store)?);
            if refresh {
                manager.refresh_access_token().await?;
            }
            let graph = GraphClient::new(manager);
            let profile = graph.profile().await?;
            let folder_count = graph.folders().await?.len();
            Result::<_>::Ok((profile, folder_count))
        }
        .await;
        match result {
            Ok((profile, folders)) => println!(
                "OK  {}: {} ({:?}, {folders} top-level folders)",
                account.name, profile.user_principal_name, account.auth_profile
            ),
            Err(error) => {
                failures += 1;
                println!("ERR {}: {error:#}", account.name);
            }
        }
    }
    if failures > 0 {
        bail!("{failures} account check(s) failed");
    }
    println!(
        "OK  IMAP 127.0.0.1:{} / SMTP 127.0.0.1:{} / photos http://127.0.0.1:{}/photo",
        config.server.imap_port, config.server.smtp_port, config.server.photo_port
    );
    if paths.cache_db.exists() {
        let store = Store::open(&paths.cache_db)?;
        for account in &config.accounts {
            let states = store.all_sync_states(&account.name)?;
            let done = states
                .iter()
                .filter(|(_, state)| state.full_sync_done)
                .count();
            let messages: u64 = states.iter().map(|(_, state)| state.synced_count).sum();
            println!(
                "OK  {}: local index {done}/{} folders complete, {messages} messages indexed",
                account.name,
                states.len()
            );
        }
    } else {
        println!("--  local index not created yet; it is built when `serve` runs");
    }
    Ok(())
}

async fn calendar_events(
    paths: &AppPaths,
    account_name: Option<&str>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<()> {
    if end <= start {
        bail!("calendar end must be later than start");
    }
    let config = Config::load(paths)?;
    let account = resolve_account(&config, account_name)?;
    let store = SecretStore::new(config.secrets.backend, paths);
    let manager = Arc::new(TokenManager::from_store(account, &store)?);
    let graph = GraphClient::new(manager);
    let start = start.to_rfc3339_opts(SecondsFormat::Secs, true);
    let end = end.to_rfc3339_opts(SecondsFormat::Secs, true);
    let events = graph.calendar_view(&start, &end).await?;
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "account": account.name,
            "start": start,
            "end": end,
            "value": events,
        }))?
    );
    Ok(())
}

fn sync_status(paths: &AppPaths) -> Result<()> {
    let config = Config::load(paths)?;
    if !paths.cache_db.exists() {
        println!("No local index yet; run `graphmail-bridge serve` to start syncing.");
        return Ok(());
    }
    let store = Store::open(&paths.cache_db)?;
    for account in &config.accounts {
        println!("{}:", account.name);
        let mut states = store.all_sync_states(&account.name)?;
        if states.is_empty() {
            println!("  (no folders synced yet)");
        }
        states.sort_by(|a, b| {
            a.0.display_name
                .to_lowercase()
                .cmp(&b.0.display_name.to_lowercase())
        });
        for (folder, state) in states {
            let (local, unseen) = store.mailbox_counts(&account.name, &folder.id)?;
            let status = if state.full_sync_done {
                "synced".to_owned()
            } else if state.next_link.is_some() {
                "initial sync in progress".to_owned()
            } else {
                "pending".to_owned()
            };
            let last = state
                .last_sync
                .and_then(|ts| chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0))
                .map(|ts| ts.format("%Y-%m-%d %H:%M UTC").to_string())
                .unwrap_or_else(|| "never".to_owned());
            println!(
                "  {:<32} {:<24} local {local:>6} (unseen {unseen:>5})  remote {:>6}  last {last}",
                folder.display_name, status, folder.total_item_count
            );
            if let Some(error) = state.last_error {
                println!("      last error: {error}");
            }
        }
    }
    let (bytes, rows) = store.body_cache_stats()?;
    println!(
        "body cache: {rows} messages, {:.1} MiB of {} MiB",
        bytes as f64 / (1024.0 * 1024.0),
        config.sync.body_cache_max_mb
    );
    Ok(())
}

fn print_client_config(paths: &AppPaths, account_name: Option<&str>) -> Result<()> {
    let config = Config::load(paths)?;
    let account = resolve_account(&config, account_name)?;
    let secrets = SecretStore::new(config.secrets.backend, paths).load(&account.name)?;
    println!(
        "\nMail client settings\n\
         Username: {}\n\
         Password: {}\n\
         IMAP:     127.0.0.1:{}  Security: None\n\
         SMTP:     127.0.0.1:{}  Security: None  Authentication: Password\n\
         Photos:   http://127.0.0.1:{}/photo?address=<email>  (HTTP Basic, same credentials)",
        account.email,
        secrets.bridge_password,
        config.server.imap_port,
        config.server.smtp_port,
        config.server.photo_port
    );
    Ok(())
}

fn install_service(paths: &AppPaths, start: bool) -> Result<()> {
    let executable = std::env::current_exe().context("could not locate this executable")?;
    if !executable.is_absolute() {
        bail!("the executable path must be absolute");
    }
    Config::load(paths)?;
    install_user_unit(paths, &executable)?;
    systemctl(&["daemon-reload"])?;
    if start {
        systemctl(&["enable", "--now", "graphmail-bridge.service"])?;
        println!("Installed and started graphmail-bridge.service.");
    } else {
        println!("Installed {}.", paths.user_unit.display());
    }
    Ok(())
}

fn uninstall_service(paths: &AppPaths) -> Result<()> {
    let _ = systemctl(&["disable", "--now", "graphmail-bridge.service"]);
    if paths.user_unit.exists() {
        fs::remove_file(&paths.user_unit)
            .with_context(|| format!("could not remove {}", paths.user_unit.display()))?;
    }
    systemctl(&["daemon-reload"])?;
    println!("Removed graphmail-bridge.service. Account data was preserved.");
    Ok(())
}

fn systemctl(arguments: &[&str]) -> Result<()> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(arguments)
        .status()
        .context("could not run systemctl --user")?;
    if !status.success() {
        bail!("systemctl --user {} failed", arguments.join(" "));
    }
    Ok(())
}

/// Pick the account a command should act on: the requested one, the only
/// configured one, or an interactive choice when several exist.
fn resolve_account<'a>(config: &'a Config, requested: Option<&str>) -> Result<&'a AccountConfig> {
    if let Some(name) = requested {
        return config
            .find_account(name)
            .with_context(|| format!("account {name:?} is not configured"));
    }
    match config.accounts.as_slice() {
        [] => bail!("no accounts are configured; run `graphmail-bridge setup`"),
        [only] => Ok(only),
        accounts => {
            let names = accounts
                .iter()
                .map(|account| account.name.as_str())
                .collect::<Vec<_>>();
            if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
                bail!(
                    "several accounts are configured; name one of: {}",
                    names.join(", ")
                );
            }
            let labels = accounts
                .iter()
                .map(|account| format!("{} <{}>", account.name, account.email))
                .collect::<Vec<_>>();
            let chosen = Select::new()
                .with_prompt("Which account?")
                .items(&labels)
                .default(0)
                .interact()
                .context("account selection failed")?;
            Ok(&accounts[chosen])
        }
    }
}

fn prompt(value: Option<String>, label: &str, default: Option<&str>) -> Result<String> {
    if let Some(value) = value {
        return Ok(value);
    }
    let mut input = Input::<String>::new().with_prompt(label);
    if let Some(default) = default {
        input = input.default(default.to_owned());
    }
    input.interact_text().context("setup prompt failed")
}

fn generate_bridge_password() -> String {
    let mut bytes = [0_u8; 24];
    rand::fill(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}
