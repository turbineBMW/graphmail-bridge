// SPDX-License-Identifier: GPL-2.0-or-later

use std::sync::Arc;

use base64::Engine;
use graphmail_bridge::config::{
    AccountConfig, AppPaths, AuthProfile, Config, SecretBackend, default_scopes,
};
use graphmail_bridge::secrets::{AccountSecrets, SecretStore};
use graphmail_bridge::service::Runtime;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

fn runtime() -> (tempfile::TempDir, Arc<Runtime>) {
    let temporary = tempfile::tempdir().unwrap();
    let paths = AppPaths::under(temporary.path());
    let account = AccountConfig {
        name: "work".into(),
        email: "me@example.com".into(),
        auth_profile: AuthProfile::CustomEntra,
        tenant: "organizations".into(),
        client_id: "test-client".into(),
        scopes: default_scopes(),
    };
    let mut config = Config::default();
    config.secrets.backend = SecretBackend::File;
    config.accounts.push(account);
    SecretStore::new(SecretBackend::File, &paths)
        .save(
            "work",
            &AccountSecrets {
                bridge_password: "local-secret".into(),
                refresh_token: "unused-refresh-token".into(),
                access_token: None,
                access_token_expires_at: None,
            },
        )
        .unwrap();
    let runtime = Runtime::load(config, &paths).unwrap();
    (temporary, runtime)
}

#[tokio::test]
async fn smtp_greets_and_authenticates_without_graph_access() {
    let (_temporary, runtime) = runtime();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(graphmail_bridge::smtp::serve(listener, runtime));

    let stream = TcpStream::connect(address).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    assert!(read_line(&mut reader).await.starts_with("220 "));
    writer.write_all(b"EHLO test\r\n").await.unwrap();
    let mut capabilities = Vec::new();
    loop {
        let line = read_line(&mut reader).await;
        let done = line.starts_with("250 ");
        capabilities.push(line);
        if done {
            break;
        }
    }
    assert!(
        capabilities
            .iter()
            .any(|line| line.contains("AUTH PLAIN LOGIN"))
    );
    let plain = base64::engine::general_purpose::STANDARD.encode(b"\0work\0local-secret");
    writer
        .write_all(format!("AUTH PLAIN {plain}\r\n").as_bytes())
        .await
        .unwrap();
    assert!(read_line(&mut reader).await.starts_with("235 "));
    writer.write_all(b"QUIT\r\n").await.unwrap();
    assert!(read_line(&mut reader).await.starts_with("221 "));
    task.abort();
}

#[tokio::test]
async fn imap_greets_and_reports_capabilities_without_graph_access() {
    let (_temporary, runtime) = runtime();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(graphmail_bridge::imap::serve(listener, runtime));

    let stream = TcpStream::connect(address).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    assert!(read_line(&mut reader).await.starts_with("* OK "));
    writer.write_all(b"a1 CAPABILITY\r\n").await.unwrap();
    let capabilities = read_line(&mut reader).await;
    assert!(capabilities.contains("IMAP4rev1"));
    assert!(capabilities.contains("MOVE"));
    assert_eq!(read_line(&mut reader).await, "a1 OK CAPABILITY completed");
    writer.write_all(b"a2 LOGOUT\r\n").await.unwrap();
    assert!(read_line(&mut reader).await.starts_with("* BYE "));
    assert_eq!(read_line(&mut reader).await, "a2 OK LOGOUT completed");
    task.abort();
}

#[tokio::test]
async fn smtp_survives_reauthentication_mid_transaction() {
    let (_temporary, runtime) = runtime();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(graphmail_bridge::smtp::serve(listener, runtime));

    let stream = TcpStream::connect(address).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    read_line(&mut reader).await;
    let plain = base64::engine::general_purpose::STANDARD.encode(b"\0work\0local-secret");
    writer
        .write_all(format!("AUTH PLAIN {plain}\r\n").as_bytes())
        .await
        .unwrap();
    assert!(read_line(&mut reader).await.starts_with("235 "));
    writer
        .write_all(b"MAIL FROM:<me@example.com>\r\n")
        .await
        .unwrap();
    assert!(read_line(&mut reader).await.starts_with("250 "));
    writer
        .write_all(b"RCPT TO:<you@example.com>\r\n")
        .await
        .unwrap();
    assert!(read_line(&mut reader).await.starts_with("250 "));
    let wrong = base64::engine::general_purpose::STANDARD.encode(b"\0work\0wrong");
    writer
        .write_all(format!("AUTH PLAIN {wrong}\r\n").as_bytes())
        .await
        .unwrap();
    assert!(read_line(&mut reader).await.starts_with("535 "));
    // Previously this panicked the connection task; it must answer instead.
    writer.write_all(b"DATA\r\n").await.unwrap();
    assert!(read_line(&mut reader).await.starts_with("530 "));
    writer.write_all(b"QUIT\r\n").await.unwrap();
    assert!(read_line(&mut reader).await.starts_with("221 "));
    task.abort();
}

#[tokio::test]
async fn smtp_rejects_oversized_lines_without_buffering_them() {
    let (_temporary, runtime) = runtime();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(graphmail_bridge::smtp::serve(listener, runtime));

    let stream = TcpStream::connect(address).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    read_line(&mut reader).await;
    let huge = vec![b'A'; 70 * 1024];
    let _ = writer.write_all(&huge).await;
    let _ = writer.write_all(b"\r\n").await;
    // The server closes the connection after refusing the line.
    let mut rest = String::new();
    reader.read_line(&mut rest).await.unwrap();
    assert!(rest.is_empty());
    task.abort();
}

#[tokio::test]
async fn imap_enable_only_confirms_supported_extensions() {
    let (_temporary, runtime) = runtime();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(graphmail_bridge::imap::serve(listener, runtime));

    let stream = TcpStream::connect(address).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    read_line(&mut reader).await;
    writer
        .write_all(b"a1 ENABLE CONDSTORE QRESYNC UTF8=ACCEPT\r\n")
        .await
        .unwrap();
    assert_eq!(read_line(&mut reader).await, "* ENABLED UTF8=ACCEPT");
    assert_eq!(read_line(&mut reader).await, "a1 OK ENABLE completed");
    writer.write_all(b"a2 SELECT INBOX\r\n").await.unwrap();
    assert!(read_line(&mut reader).await.starts_with("a2 NO "));
    task.abort();
}

async fn read_line(reader: &mut (impl AsyncBufReadExt + Unpin)) -> String {
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    line.trim_end_matches(['\r', '\n']).to_owned()
}

#[tokio::test]
async fn photo_endpoint_requires_bridge_credentials() {
    let (_temporary, runtime) = runtime();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(graphmail_bridge::http::serve(listener, runtime));

    async fn get(address: std::net::SocketAddr, request: &str) -> String {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut stream, &mut response)
            .await
            .unwrap();
        response
    }

    let anonymous = get(
        address,
        "GET /photo?address=bob%40example.com HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(anonymous.starts_with("HTTP/1.1 401 "), "{anonymous}");
    assert!(
        anonymous
            .to_ascii_lowercase()
            .contains("www-authenticate: basic")
    );

    let wrong = base64::engine::general_purpose::STANDARD.encode("work:nope");
    let denied = get(
        address,
        &format!(
            "GET /photo?address=bob%40example.com HTTP/1.1\r\nAuthorization: Basic {wrong}\r\nConnection: close\r\n\r\n"
        ),
    )
    .await;
    assert!(denied.starts_with("HTTP/1.1 401 "), "{denied}");

    let right = base64::engine::general_purpose::STANDARD.encode("me@example.com:local-secret");
    let missing = get(
        address,
        &format!(
            "GET /photo HTTP/1.1\r\nAuthorization: Basic {right}\r\nConnection: close\r\n\r\n"
        ),
    )
    .await;
    assert!(missing.starts_with("HTTP/1.1 400 "), "{missing}");

    let elsewhere = get(
        address,
        &format!(
            "GET /other HTTP/1.1\r\nAuthorization: Basic {right}\r\nConnection: close\r\n\r\n"
        ),
    )
    .await;
    assert!(elsewhere.starts_with("HTTP/1.1 404 "), "{elsewhere}");
    task.abort();
}
