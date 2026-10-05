// SPDX-License-Identifier: GPL-2.0-or-later

//! A tiny loopback HTTP service that hands a mail client the profile
//! pictures Microsoft Graph knows about, so an IMAP client can show Outlook
//! avatars without owning a Graph token itself.
//!
//! One endpoint: `GET /photo?address=<email>` with HTTP Basic credentials
//! equal to the bridge's IMAP login. The account's own address maps to
//! `/me/photo`; anything else is looked up as a tenant user, then as a
//! personal contact. `200` carries the image, `404` means nobody has one.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use crate::graph::Photo;
use crate::service::{AccountRuntime, Runtime};

const MAX_LINE_BYTES: usize = 8 * 1024;
const MAX_HEADERS: usize = 64;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long an answer is reused before Graph is asked again. Pictures change
/// rarely; the negative answer is the common one and the one worth caching.
const HIT_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MISS_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const MAX_CACHED: usize = 2000;

/// `(account, address)` to what Graph said, and when.
type CacheKey = (String, String);
type CacheEntry = (Instant, Option<Photo>);

#[derive(Default)]
pub struct PhotoCache {
    entries: Mutex<HashMap<CacheKey, CacheEntry>>,
}

impl PhotoCache {
    async fn get(&self, key: &(String, String)) -> Option<Option<Photo>> {
        let entries = self.entries.lock().await;
        let (stored, photo) = entries.get(key)?;
        let ttl = if photo.is_some() { HIT_TTL } else { MISS_TTL };
        (stored.elapsed() < ttl).then(|| photo.clone())
    }

    async fn put(&self, key: (String, String), photo: Option<Photo>) {
        let mut entries = self.entries.lock().await;
        if entries.len() >= MAX_CACHED {
            entries.clear();
        }
        entries.insert(key, (Instant::now(), photo));
    }
}

pub async fn serve(listener: TcpListener, runtime: Arc<Runtime>) -> Result<()> {
    let cache = Arc::new(PhotoCache::default());
    loop {
        let (stream, peer) = listener.accept().await?;
        let runtime = runtime.clone();
        let cache = cache.clone();
        tokio::spawn(async move {
            let handled = tokio::time::timeout(REQUEST_TIMEOUT, handle(stream, runtime, cache));
            match handled.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::debug!(%peer, %error, "photo request failed");
                }
                Err(_) => tracing::debug!(%peer, "photo request timed out"),
            }
        });
    }
}

struct Request {
    target: String,
    authorization: Option<String>,
}

async fn handle(stream: TcpStream, runtime: Arc<Runtime>, cache: Arc<PhotoCache>) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let request = match read_request(&mut reader).await? {
        Some(request) => request,
        None => return respond(&mut writer, 400, "text/plain", b"bad request").await,
    };
    let (path, query) = request
        .target
        .split_once('?')
        .unwrap_or((request.target.as_str(), ""));
    if path != "/photo" {
        return respond(&mut writer, 404, "text/plain", b"not found").await;
    }
    let Some((login, password)) = request.authorization.as_deref().and_then(basic_credentials)
    else {
        return unauthorized(&mut writer).await;
    };
    let Some(account) = runtime.authenticate(&login, &password).await else {
        return unauthorized(&mut writer).await;
    };
    let Some(address) = query_value(query, "address")
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| value.contains('@') && !value.contains(['/', '?', '#', '\r', '\n']))
    else {
        return respond(
            &mut writer,
            400,
            "text/plain",
            b"missing or invalid address",
        )
        .await;
    };

    let key = (account.config.name.to_ascii_lowercase(), address.clone());
    let photo = match cache.get(&key).await {
        Some(cached) => cached,
        None => match lookup(&account, &address).await {
            Ok(photo) => {
                cache.put(key, photo.clone()).await;
                photo
            }
            Err(error) => {
                tracing::warn!(
                    account = %account.config.name,
                    %address,
                    %error,
                    "could not fetch a profile photo from Microsoft Graph"
                );
                return respond(
                    &mut writer,
                    502,
                    "text/plain",
                    b"Microsoft Graph lookup failed",
                )
                .await;
            }
        },
    };
    match photo {
        Some(photo) => respond(&mut writer, 200, &photo.content_type, &photo.bytes).await,
        None => respond(&mut writer, 404, "text/plain", b"no photo").await,
    }
}

async fn lookup(account: &AccountRuntime, address: &str) -> Result<Option<Photo>> {
    if address.eq_ignore_ascii_case(&account.config.email) {
        return account.graph.my_photo().await;
    }
    if let Some(photo) = account.graph.user_photo(address).await? {
        return Ok(Some(photo));
    }
    account.graph.contact_photo(address).await
}

async fn read_request(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
) -> Result<Option<Request>> {
    let Some(line) = read_line(reader).await? else {
        return Ok(None);
    };
    let mut parts = line.split_ascii_whitespace();
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Ok(None);
    };
    if !method.eq_ignore_ascii_case("GET") || !version.starts_with("HTTP/1.") {
        return Ok(None);
    }
    let mut authorization = None;
    for _ in 0..MAX_HEADERS {
        let Some(line) = read_line(reader).await? else {
            return Ok(None);
        };
        if line.is_empty() {
            return Ok(Some(Request {
                target: target.to_owned(),
                authorization,
            }));
        }
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("authorization")
        {
            authorization = Some(value.trim().to_owned());
        }
    }
    Ok(None)
}

async fn read_line(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
) -> Result<Option<String>> {
    let mut buffer = Vec::new();
    let mut limited = reader.take(MAX_LINE_BYTES as u64);
    let read = limited.read_until(b'\n', &mut buffer).await?;
    if read == 0 {
        return Ok(None);
    }
    if !buffer.ends_with(b"\n") {
        anyhow::bail!("request line too long");
    }
    let text = String::from_utf8(buffer).context("request is not UTF-8")?;
    Ok(Some(text.trim_end_matches(['\r', '\n']).to_owned()))
}

fn basic_credentials(header: &str) -> Option<(String, String)> {
    let (scheme, encoded) = header.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (login, password) = text.split_once(':')?;
    Some((login.to_owned(), password.to_owned()))
}

/// The first `name=value` pair in a query string, percent-decoded.
pub fn query_value(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        (key == name).then(|| percent_decode(value))
    })
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = &value[index + 1..index + 3];
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn unauthorized(writer: &mut tokio::net::tcp::OwnedWriteHalf) -> Result<()> {
    writer
        .write_all(
            b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"graphmail-bridge\"\r\nContent-Type: text/plain\r\nContent-Length: 12\r\nConnection: close\r\n\r\nunauthorized",
        )
        .await?;
    writer.shutdown().await?;
    Ok(())
}

async fn respond(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        502 => "Bad Gateway",
        _ => "Error",
    };
    let cache_control = if status == 200 || status == 404 {
        "max-age=86400"
    } else {
        "no-store"
    };
    let content_type = if content_type.contains(['\r', '\n']) {
        "application/octet-stream"
    } else {
        content_type
    };
    writer
        .write_all(
            format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: {cache_control}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .await?;
    writer.write_all(body).await?;
    writer.shutdown().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_query_values() {
        assert_eq!(
            query_value("x=1&address=bob%40example.com&y", "address").as_deref(),
            Some("bob@example.com")
        );
        assert_eq!(query_value("a=b", "address"), None);
        assert_eq!(percent_decode("a+b%2"), "a b%2");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn parses_basic_credentials() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("work:s3cret:x");
        assert_eq!(
            basic_credentials(&format!("Basic {encoded}")),
            Some(("work".into(), "s3cret:x".into()))
        );
        assert_eq!(basic_credentials("Bearer abc"), None);
    }
}
