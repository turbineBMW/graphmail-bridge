// SPDX-License-Identifier: GPL-2.0-or-later

//! Register a bridge account with Evolution Data Server: a WebDAV collection,
//! so EDS discovers its calendars over CalDAV and every EDS client (GNOME
//! Calendar, Evolution, Era, the shell's clock) shows them, plus the IMAP
//! account, identity and SMTP transport below it, so mail clients that read
//! their accounts from EDS find the bridge like any other account.

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

/// The account's sources as `(uid, keyfile)`: the collection first, then
/// its mail account, identity and transport. `name` is the person's name
/// for the From header.
pub fn sources(config: &Config, account: &AccountConfig, name: &str) -> Vec<(String, String)> {
    let uid = source_uid(account);
    let bind = config.server.bind;
    let url = format!("http://{bind}:{}/dav/", config.server.photo_port);
    let display_name = keyfile_value(&display_name(account));
    let email = keyfile_value(&account.email);
    let name = keyfile_value(name);
    let (mail_uid, identity_uid, transport_uid) = (
        format!("{uid}-mail"),
        format!("{uid}-identity"),
        format!("{uid}-transport"),
    );
    let collection = format!(
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
         MailEnabled=true\n\
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
         AvoidIfmatch=false\n"
    );
    // An empty IMAP method is EDS's plain password (LOGIN), the only one
    // the bridge's IMAP server offers.
    let mail = format!(
        "[Data Source]\n\
         DisplayName={display_name}\n\
         Enabled=true\n\
         Parent={uid}\n\
         \n\
         [Mail Account]\n\
         BackendName=imapx\n\
         IdentityUid={identity_uid}\n\
         \n\
         [Authentication]\n\
         Host={bind}\n\
         Port={imap_port}\n\
         User={email}\n\
         Method=\n\
         RememberPassword=true\n\
         ProxyUid=system-proxy\n\
         \n\
         [Security]\n\
         Method=none\n\
         \n\
         [Imapx Backend]\n\
         UseIdle=true\n",
        imap_port = config.server.imap_port,
    );
    // Graph files a copy of everything sent, so clients must not append
    // one to Sent themselves.
    let identity = format!(
        "[Data Source]\n\
         DisplayName={display_name}\n\
         Enabled=true\n\
         Parent={uid}\n\
         \n\
         [Mail Identity]\n\
         Address={email}\n\
         Name={name}\n\
         \n\
         [Mail Submission]\n\
         TransportUid={transport_uid}\n\
         UseSentFolder=false\n"
    );
    let transport = format!(
        "[Data Source]\n\
         DisplayName={display_name}\n\
         Enabled=true\n\
         Parent={uid}\n\
         \n\
         [Authentication]\n\
         Host={bind}\n\
         Port={smtp_port}\n\
         User={email}\n\
         Method=PLAIN\n\
         RememberPassword=true\n\
         ProxyUid=system-proxy\n\
         \n\
         [Security]\n\
         Method=none\n\
         \n\
         [Mail Transport]\n\
         BackendName=smtp\n",
        smtp_port = config.server.smtp_port,
    );
    vec![
        (uid, collection),
        (mail_uid, mail),
        (identity_uid, identity),
        (transport_uid, transport),
    ]
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
            goa_account: None,
        }
    }

    #[test]
    fn uid_is_a_plain_file_name() {
        assert_eq!(source_uid(&account()), "graphmail-bridge-work-mail");
    }

    #[test]
    fn sources_cover_calendars_and_mail() {
        let sources = sources(&Config::default(), &account(), "Jane Doe");
        let uids: Vec<&str> = sources.iter().map(|(uid, _)| uid.as_str()).collect();
        assert_eq!(
            uids,
            [
                "graphmail-bridge-work-mail",
                "graphmail-bridge-work-mail-mail",
                "graphmail-bridge-work-mail-identity",
                "graphmail-bridge-work-mail-transport"
            ]
        );
        let (collection, mail, identity, transport) =
            (&sources[0].1, &sources[1].1, &sources[2].1, &sources[3].1);
        assert!(collection.contains("BackendName=webdav\n"));
        assert!(collection.contains("CalendarUrl=http://127.0.0.1:1180/dav/\n"));
        assert!(collection.contains("MailEnabled=true\n"));
        assert!(collection.contains("DisplayName=Work Mail (Microsoft 365)\n"));
        assert!(mail.contains("Parent=graphmail-bridge-work-mail\n"));
        assert!(mail.contains("IdentityUid=graphmail-bridge-work-mail-identity\n"));
        assert!(mail.contains("Host=127.0.0.1\nPort=1143\n"));
        assert!(identity.contains("Address=me@example.com\nName=Jane Doe\n"));
        assert!(identity.contains("TransportUid=graphmail-bridge-work-mail-transport\n"));
        assert!(transport.contains("Port=1025\n"));
        for text in [collection, mail, transport] {
            assert!(text.contains("[Security]\nMethod=none\n"));
        }
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
