// SPDX-License-Identifier: GPL-2.0-or-later

//! Register a bridge account with Evolution Data Server as a WebDAV
//! collection, so EDS discovers its calendars over CalDAV and every EDS
//! client (GNOME Calendar, Evolution, Era, the shell's clock) shows them.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use directories::BaseDirs;

use crate::config::{AccountConfig, Config};

/// The `.source` file name (without extension) EDS knows the account by.
pub fn source_uid(account: &AccountConfig) -> String {
    let name: String = account
        .name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!("graphmail-bridge-{name}")
}

/// How EDS clients list the account.
pub fn display_name(account: &AccountConfig) -> String {
    format!("{} (Microsoft 365)", account.name)
}

pub fn sources_dir() -> Result<PathBuf> {
    let base = BaseDirs::new().context("could not determine your home directory")?;
    Ok(base.config_dir().join("evolution/sources"))
}

/// The collection source: EDS discovers the calendars below `/dav/` itself
/// and authenticates as the account's email with the bridge password.
pub fn source_file(config: &Config, account: &AccountConfig) -> String {
    let url = format!(
        "http://{}:{}/dav/",
        config.server.bind, config.server.photo_port
    );
    let display_name = display_name(account);
    format!(
        "[Data Source]\n\
         DisplayName={display_name}\n\
         Enabled=true\n\
         Parent=\n\
         \n\
         [Collection]\n\
         BackendName=webdav\n\
         Identity={email}\n\
         CalendarEnabled=true\n\
         ContactsEnabled=false\n\
         MailEnabled=false\n\
         CalendarUrl={url}\n\
         ContactsUrl=\n\
         AllowSourcesRename=false\n\
         \n\
         [Authentication]\n\
         Host=\n\
         Port=0\n\
         User={email}\n\
         Method=plain/password\n\
         RememberPassword=true\n\
         ProxyUid=system-proxy\n\
         \n\
         [Security]\n\
         Method=none\n\
         \n\
         [WebDAV Backend]\n\
         SslTrust=\n\
         AvoidIfmatch=false\n",
        email = keyfile_value(&account.email),
        display_name = keyfile_value(&display_name),
    )
}

/// GKeyFile escapes for a value on one line.
fn keyfile_value(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

pub fn install(directory: &Path, uid: &str, contents: &str) -> Result<PathBuf> {
    fs::create_dir_all(directory)
        .with_context(|| format!("could not create {}", directory.display()))?;
    let path = directory.join(format!("{uid}.source"));
    let temporary = directory.join(format!(".{uid}.source.tmp"));
    fs::write(&temporary, contents)
        .with_context(|| format!("could not write {}", temporary.display()))?;
    fs::rename(&temporary, &path)?;
    Ok(path)
}

/// Store the password where EDS looks for a source's credentials: libsecret,
/// matched on `e-source-uid` alone.
pub fn store_password(uid: &str, display_name: &str, password: &str) -> Result<()> {
    let mut child = Command::new("secret-tool")
        .args([
            "store",
            &format!("--label=Evolution Data Source “{display_name}”"),
            "e-source-uid",
            uid,
            "eds-origin",
            "evolution-data-server",
        ])
        .stdin(Stdio::piped())
        .spawn()
        .context("could not run secret-tool (part of libsecret)")?;
    child
        .stdin
        .take()
        .context("secret-tool has no stdin")?
        .write_all(password.as_bytes())?;
    if !child.wait()?.success() {
        bail!("secret-tool could not store the password");
    }
    Ok(())
}

pub fn clear_password(uid: &str) -> Result<()> {
    let status = Command::new("secret-tool")
        .args(["clear", "e-source-uid", uid])
        .status()
        .context("could not run secret-tool (part of libsecret)")?;
    if !status.success() {
        bail!("secret-tool could not remove the password");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuthProfile, default_scopes};

    fn account() -> AccountConfig {
        AccountConfig {
            name: "Work Mail".into(),
            email: "me@example.com".into(),
            auth_profile: AuthProfile::CustomEntra,
            tenant: "organizations".into(),
            client_id: "client".into(),
            scopes: default_scopes(),
        }
    }

    #[test]
    fn uid_is_a_plain_file_name() {
        assert_eq!(source_uid(&account()), "graphmail-bridge-work-mail");
    }

    #[test]
    fn source_points_eds_at_the_dav_root() {
        let text = source_file(&Config::default(), &account());
        assert!(text.contains("BackendName=webdav\n"));
        assert!(text.contains("CalendarUrl=http://127.0.0.1:1180/dav/\n"));
        assert!(text.contains("Identity=me@example.com\n"));
        assert!(text.contains("User=me@example.com\n"));
        assert!(text.contains("[Security]\nMethod=none\n"));
        assert!(text.contains("DisplayName=Work Mail (Microsoft 365)\n"));
    }

    #[test]
    fn install_replaces_the_file_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let path = install(directory.path(), "x", "one").unwrap();
        install(directory.path(), "x", "two").unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "two");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
