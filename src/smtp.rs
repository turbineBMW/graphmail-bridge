// SPDX-License-Identifier: GPL-2.0-or-later

use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine;
use mail_parser::MessageParser;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::net::{TcpListener, TcpStream};

use crate::service::{AccountRuntime, Runtime};

const MAX_MESSAGE_BYTES: usize = 35 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 64 * 1024;

pub async fn serve(listener: TcpListener, runtime: Arc<Runtime>) -> Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let runtime = runtime.clone();
        tokio::spawn(async move {
            if let Err(error) = handle(stream, runtime).await {
                tracing::warn!(%peer, %error, "SMTP connection closed with an error");
            }
        });
    }
}

async fn handle(stream: TcpStream, runtime: Arc<Runtime>) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    reply(&mut writer, "220 graphmail-bridge ESMTP ready").await?;
    let mut authenticated: Option<Arc<AccountRuntime>> = None;
    let mut has_sender = false;
    let mut recipients = Vec::new();

    loop {
        let Some(line) = read_line(&mut reader).await? else {
            return Ok(());
        };
        let (verb, arguments) = line
            .split_once(' ')
            .map_or((line.as_str(), ""), |(verb, args)| (verb, args.trim()));
        match verb.to_ascii_uppercase().as_str() {
            "EHLO" => {
                writer
                    .write_all(
                        format!(
                            "250-graphmail-bridge\r\n250-AUTH PLAIN LOGIN\r\n250-8BITMIME\r\n250-SIZE {MAX_MESSAGE_BYTES}\r\n250 PIPELINING\r\n"
                        )
                        .as_bytes(),
                    )
                    .await?;
            }
            "HELO" => reply(&mut writer, "250 graphmail-bridge").await?,
            "AUTH" if arguments.to_ascii_uppercase().starts_with("PLAIN") => {
                has_sender = false;
                recipients.clear();
                let initial = arguments.get(5..).unwrap_or_default().trim();
                let encoded = if initial.is_empty() {
                    reply(&mut writer, "334").await?;
                    read_line(&mut reader).await?.unwrap_or_default()
                } else {
                    initial.to_owned()
                };
                match plain_credentials(&encoded) {
                    Some((login, password)) => {
                        authenticated = runtime.authenticate(&login, &password).await;
                        if authenticated.is_some() {
                            reply(&mut writer, "235 2.7.0 Authentication successful").await?;
                        } else {
                            reply(&mut writer, "535 5.7.8 Authentication failed").await?;
                        }
                    }
                    None => reply(&mut writer, "501 5.5.2 Invalid AUTH PLAIN value").await?,
                }
            }
            "AUTH" if arguments.eq_ignore_ascii_case("LOGIN") => {
                has_sender = false;
                recipients.clear();
                reply(&mut writer, "334 VXNlcm5hbWU6").await?;
                let user = read_line(&mut reader).await?.unwrap_or_default();
                reply(&mut writer, "334 UGFzc3dvcmQ6").await?;
                let pass = read_line(&mut reader).await?.unwrap_or_default();
                let credentials = decode_utf8(&user).zip(decode_utf8(&pass));
                authenticated = match credentials {
                    Some((login, password)) => runtime.authenticate(&login, &password).await,
                    None => None,
                };
                if authenticated.is_some() {
                    reply(&mut writer, "235 2.7.0 Authentication successful").await?;
                } else {
                    reply(&mut writer, "535 5.7.8 Authentication failed").await?;
                }
            }
            "MAIL" if authenticated.is_none() => {
                reply(&mut writer, "530 5.7.0 Authentication required").await?
            }
            "MAIL" if arguments.to_ascii_uppercase().starts_with("FROM:") => {
                has_sender = true;
                recipients.clear();
                reply(&mut writer, "250 2.1.0 Sender accepted").await?;
            }
            "RCPT" if has_sender && arguments.to_ascii_uppercase().starts_with("TO:") => {
                if let Some(recipient) = smtp_path(arguments) {
                    recipients.push(recipient);
                    reply(&mut writer, "250 2.1.5 Recipient accepted").await?;
                } else {
                    reply(&mut writer, "501 5.1.3 Invalid recipient").await?;
                }
            }
            "DATA" if authenticated.is_none() => {
                reply(&mut writer, "530 5.7.0 Authentication required").await?
            }
            "DATA" if has_sender && !recipients.is_empty() => {
                let Some(account) = authenticated.clone() else {
                    unreachable!("guarded by the previous arm");
                };
                reply(&mut writer, "354 End data with <CR><LF>.<CR><LF>").await?;
                match read_data(&mut reader).await {
                    Ok(message) => match account
                        .graph
                        .send_mime(&add_missing_envelope_recipients(message, &recipients))
                        .await
                    {
                        Ok(()) => reply(&mut writer, "250 2.0.0 Message accepted").await?,
                        Err(error) => {
                            tracing::warn!(%error, "Graph rejected SMTP message");
                            reply(&mut writer, "451 4.3.0 Microsoft Graph send failed").await?;
                        }
                    },
                    Err(error) => {
                        tracing::warn!(%error, "SMTP message rejected");
                        reply(&mut writer, "552 5.3.4 Message too large").await?;
                    }
                }
                has_sender = false;
                recipients.clear();
            }
            "MAIL" | "RCPT" | "DATA" => {
                reply(&mut writer, "503 5.5.1 Bad sequence of commands").await?
            }
            "RSET" => {
                has_sender = false;
                recipients.clear();
                reply(&mut writer, "250 2.0.0 Reset").await?;
            }
            "NOOP" => reply(&mut writer, "250 2.0.0 OK").await?,
            "QUIT" => {
                reply(&mut writer, "221 2.0.0 Bye").await?;
                return Ok(());
            }
            "STARTTLS" => reply(&mut writer, "454 4.7.0 TLS unavailable on loopback").await?,
            _ => reply(&mut writer, "502 5.5.1 Command not implemented").await?,
        }
    }
}

async fn read_line(reader: &mut (impl AsyncBufRead + Unpin)) -> Result<Option<String>> {
    let Some(mut bytes) = read_bounded_line(reader, MAX_LINE_BYTES).await? else {
        return Ok(None);
    };
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.pop();
    }
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// Read one line including its terminator, refusing to buffer more than
/// `limit` bytes so a misbehaving local client cannot exhaust memory.
pub(crate) async fn read_bounded_line(
    reader: &mut (impl AsyncBufRead + Unpin),
    limit: usize,
) -> Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let read = reader
        .take(limit as u64 + 1)
        .read_until(b'\n', &mut line)
        .await?;
    if read == 0 {
        return Ok(None);
    }
    if line.len() > limit {
        anyhow::bail!("line exceeds {limit} bytes");
    }
    Ok(Some(line))
}

async fn read_data(reader: &mut (impl AsyncBufRead + Unpin)) -> Result<Vec<u8>> {
    let mut message = Vec::new();
    let mut too_large = false;
    loop {
        let Some(mut line) = read_bounded_line(reader, MAX_LINE_BYTES).await? else {
            anyhow::bail!("connection ended during DATA");
        };
        if line == b".\r\n" || line == b".\n" {
            if too_large {
                anyhow::bail!("message exceeds {MAX_MESSAGE_BYTES} bytes");
            }
            return Ok(message);
        }
        if line.starts_with(b"..") {
            line.remove(0);
        }
        if message.len().saturating_add(line.len()) > MAX_MESSAGE_BYTES {
            too_large = true;
        }
        if !too_large {
            message.extend_from_slice(&line);
        }
    }
}

fn plain_credentials(encoded: &str) -> Option<(String, String)> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let fields: Vec<&[u8]> = decoded.split(|byte| *byte == 0).collect();
    if fields.len() != 3 {
        return None;
    }
    let login = String::from_utf8(fields[1].to_vec()).ok()?;
    let password = String::from_utf8(fields[2].to_vec()).ok()?;
    Some((login, password))
}

fn decode_utf8(encoded: &str) -> Option<String> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    String::from_utf8(decoded).ok()
}

fn smtp_path(arguments: &str) -> Option<String> {
    let value = arguments.split_once(':')?.1.trim_start();
    if let Some(value) = value.strip_prefix('<') {
        return value.split_once('>').map(|(address, _)| address.to_owned());
    }
    value
        .split_ascii_whitespace()
        .next()
        .filter(|address| address.contains('@'))
        .map(str::to_owned)
}

fn add_missing_envelope_recipients(message: Vec<u8>, recipients: &[String]) -> Vec<u8> {
    let parsed = MessageParser::default().parse(&message);
    let mut header_addresses = Vec::new();
    if let Some(parsed) = &parsed {
        for addresses in [parsed.to(), parsed.cc(), parsed.bcc()]
            .into_iter()
            .flatten()
        {
            header_addresses.extend(
                addresses
                    .iter()
                    .filter_map(|address| address.address().map(str::to_ascii_lowercase)),
            );
        }
    }
    let missing = recipients
        .iter()
        .filter(|recipient| {
            !header_addresses
                .iter()
                .any(|header| header.eq_ignore_ascii_case(recipient))
        })
        .cloned()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return message;
    }
    let Some(header_end) = message
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .or_else(|| message.windows(2).position(|window| window == b"\n\n"))
    else {
        return message;
    };
    let newline = if message.get(header_end..header_end + 4) == Some(b"\r\n\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut result =
        Vec::with_capacity(message.len() + missing.iter().map(String::len).sum::<usize>() + 8);
    let header = &message[..header_end];
    if let Some(bcc_end) = existing_bcc_end(header) {
        // Extend the existing Bcc header rather than emitting a second one.
        result.extend_from_slice(&header[..bcc_end]);
        result.extend_from_slice(format!(", {}", missing.join(", ")).as_bytes());
        result.extend_from_slice(&header[bcc_end..]);
    } else {
        result.extend_from_slice(header);
        result.extend_from_slice(format!("{newline}Bcc: {}", missing.join(", ")).as_bytes());
    }
    result.extend_from_slice(&message[header_end..]);
    result
}

/// Byte offset of the end of an existing `Bcc:` header (including folded
/// continuation lines, excluding its final line break), if there is one.
fn existing_bcc_end(header: &[u8]) -> Option<usize> {
    let mut offset = 0;
    let mut in_bcc = false;
    let mut end = None;
    for line in header.split(|byte| *byte == b'\n') {
        let trimmed = line.strip_suffix(b"\r").unwrap_or(line);
        let continuation = trimmed
            .first()
            .is_some_and(|byte| matches!(byte, b' ' | b'\t'));
        if !continuation {
            if in_bcc {
                break;
            }
            in_bcc = trimmed.len() >= 4 && trimmed[..4].eq_ignore_ascii_case(b"bcc:");
        }
        if in_bcc {
            end = Some(offset + trimmed.len());
        }
        offset += line.len() + 1;
    }
    end
}

async fn reply(writer: &mut (impl AsyncWrite + Unpin), line: &str) -> Result<()> {
    writer
        .write_all(format!("{line}\r\n").as_bytes())
        .await
        .context("could not write SMTP response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_auth_plain() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(b"\0work\0secret");
        assert_eq!(
            plain_credentials(&encoded),
            Some(("work".into(), "secret".into()))
        );
    }

    #[test]
    fn extends_existing_bcc_header() {
        let message =
            b"From: me@example.com\r\nBcc: one@example.com\r\nTo: you@example.com\r\n\r\nbody"
                .to_vec();
        let result = add_missing_envelope_recipients(message, &["two@example.com".into()]);
        let text = String::from_utf8(result).unwrap();
        assert!(text.contains("Bcc: one@example.com, two@example.com\r\nTo:"));
        assert_eq!(text.matches("Bcc:").count(), 1);
    }

    #[test]
    fn preserves_smtp_only_bcc_recipients() {
        let message =
            b"From: me@example.com\r\nTo: you@example.com\r\nSubject: hi\r\n\r\nbody".to_vec();
        let result = add_missing_envelope_recipients(
            message,
            &["you@example.com".into(), "hidden@example.com".into()],
        );
        let text = String::from_utf8(result).unwrap();
        assert!(text.contains("\r\nBcc: hidden@example.com\r\n\r\n"));
        assert!(!text.contains("Bcc: you@example.com"));
    }
}
