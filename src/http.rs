// SPDX-License-Identifier: GPL-2.0-or-later

//! The loopback HTTP server: profile photos at `/photo` and CalDAV under
//! `/dav/`. Every request is authenticated with HTTP Basic and the bridge's
//! own credentials (the same as IMAP).

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use base64::Engine;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{self, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;

use crate::photos::{PhotoCache, photo};
use crate::service::{AccountRuntime, Runtime};

pub type HttpResponse = Response<Full<Bytes>>;

/// Clients get this long to send a request's headers.
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn serve(listener: TcpListener, runtime: Arc<Runtime>) -> Result<()> {
    let photos = Arc::new(PhotoCache::default());
    loop {
        let (stream, peer) = listener.accept().await?;
        let runtime = runtime.clone();
        let photos = photos.clone();
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |request| {
                let runtime = runtime.clone();
                let photos = photos.clone();
                async move { Ok::<_, Infallible>(route(request, &runtime, &photos).await) }
            });
            let served = hyper::server::conn::http1::Builder::new()
                .title_case_headers(true)
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_TIMEOUT)
                .serve_connection(TokioIo::new(stream), service)
                .await;
            if let Err(error) = served {
                tracing::debug!(%peer, %error, "HTTP connection ended with an error");
            }
        });
    }
}

async fn route(request: Request<Incoming>, runtime: &Runtime, photos: &PhotoCache) -> HttpResponse {
    let path = request.uri().path().to_owned();
    if path == "/photo" {
        return match authenticate(&request, runtime).await {
            Some(account) => photo_response(&request, &account, photos).await,
            None => unauthorized(),
        };
    }
    if path.starts_with("/.well-known/caldav") || path.starts_with("/.well-known/carddav") {
        let mut response = text(StatusCode::MOVED_PERMANENTLY, "see /dav/");
        response
            .headers_mut()
            .insert(header::LOCATION, HeaderValue::from_static("/dav/"));
        return response;
    }
    if path == "/dav" || path.starts_with("/dav/") {
        let Some(account) = authenticate(&request, runtime).await else {
            return unauthorized();
        };
        let (parts, body) = request.into_parts();
        let body = match Limited::new(body, crate::dav::xml::MAX_BODY)
            .collect()
            .await
        {
            Ok(collected) => collected.to_bytes(),
            Err(_) => return text(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
        };
        return crate::dav::handle(runtime, &account, &parts, body).await;
    }
    text(StatusCode::NOT_FOUND, "not found")
}

async fn authenticate(
    request: &Request<Incoming>,
    runtime: &Runtime,
) -> Option<Arc<AccountRuntime>> {
    let (login, password) = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(basic_credentials)?;
    runtime.authenticate(&login, &password).await
}

async fn photo_response(
    request: &Request<Incoming>,
    account: &AccountRuntime,
    photos: &PhotoCache,
) -> HttpResponse {
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return text(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    }
    let query = request.uri().query().unwrap_or("");
    let Some(address) = query_value(query, "address")
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| value.contains('@') && !value.contains(['/', '?', '#', '\r', '\n']))
    else {
        return text(StatusCode::BAD_REQUEST, "missing or invalid address");
    };
    let mut response = match photo(account, photos, &address).await {
        Ok(Some(photo)) => {
            let content_type = HeaderValue::from_str(&photo.content_type)
                .unwrap_or(HeaderValue::from_static("application/octet-stream"));
            let mut response = Response::new(Full::new(Bytes::from(photo.bytes)));
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, content_type);
            response
        }
        Ok(None) => text(StatusCode::NOT_FOUND, "no photo"),
        Err(error) => {
            tracing::warn!(
                account = %account.config.name,
                %address,
                %error,
                "could not fetch a profile photo from Microsoft Graph"
            );
            let mut response = text(StatusCode::BAD_GATEWAY, "Microsoft Graph lookup failed");
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            return response;
        }
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("max-age=86400"),
    );
    response
}

pub fn text(status: StatusCode, body: &str) -> HttpResponse {
    let mut response = Response::new(Full::new(Bytes::from(body.to_owned())));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

fn unauthorized() -> HttpResponse {
    let mut response = text(StatusCode::UNAUTHORIZED, "unauthorized");
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"graphmail-bridge\""),
    );
    response
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
        (key == name).then(|| percent_decode(&value.replace('+', " ")))
    })
}

/// Decode `%XX` escapes; invalid escapes are kept as they are.
pub fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(hex) = value.get(index + 1..index + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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
        assert_eq!(query_value("q=a+b%2", "q").as_deref(), Some("a b%2"));
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("a%40b.ics"), "a@b.ics");
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
