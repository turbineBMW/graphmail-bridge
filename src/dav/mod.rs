// SPDX-License-Identifier: GPL-2.0-or-later

//! CalDAV (RFC 4791) over the account's Microsoft 365 calendars, with the
//! CalendarServer CTag and RFC 6578 sync-collection, shaped by what
//! Evolution Data Server's WebDAV collection and CalDAV backends request.
//!
//! Layout, per authenticated account:
//!
//! ```text
//! /dav/                                  service root
//! /dav/<account>/                        principal
//! /dav/<account>/calendars/              calendar home
//! /dav/<account>/calendars/<slug>/       one Graph calendar
//! /dav/<account>/calendars/<slug>/<obj>  one event or series (.ics)
//! ```

pub mod xml;

use bytes::Bytes;
use http_body_util::Full;
use hyper::header::{self, HeaderValue};
use hyper::http::request::Parts;
use hyper::{Method, Response, StatusCode};

use self::xml::{
    APPLE_ICAL, CALDAV, CALSERVER, DAV, Element, Multistatus, PropName, Requested, escape,
};
use crate::calendar::apply::{self, Preconditions, WriteError};
use crate::calendar::store::{StoredCalendar, StoredObject};
use crate::http::{HttpResponse, percent_decode, text};
use crate::service::{AccountRuntime, Runtime};

const DAV_CAPABILITIES: &str = "1, 3, calendar-access";
const ALLOW: &str = "OPTIONS, GET, HEAD, PUT, DELETE, PROPFIND, REPORT";
const ICS_TYPE: &str = "text/calendar; charset=utf-8";
const SYNC_TOKEN_PREFIX: &str = "https://graphmail-bridge.invalid/sync/";

/// A resource a path names.
enum Resource {
    Root,
    Principal,
    CalendarHome,
    Calendar(StoredCalendar),
    /// An object in a calendar; `None` when nothing is stored there yet.
    Object(StoredCalendar, String, Option<Box<StoredObject>>),
}

pub async fn handle(
    runtime: &Runtime,
    account: &AccountRuntime,
    request: &Parts,
    body: Bytes,
) -> HttpResponse {
    let path = request.uri.path();
    let resource = match resolve(runtime, account, path) {
        Ok(Some(resource)) => resource,
        Ok(None) => return text(StatusCode::NOT_FOUND, "not found"),
        Err(error) => return server_error(error),
    };
    let method = request.method.as_str();
    let result = match method {
        "OPTIONS" => Ok(options()),
        "PROPFIND" => propfind(runtime, account, &resource, request, &body),
        "REPORT" => report(runtime, account, &resource, &body),
        "GET" | "HEAD" => Ok(get(&resource, request.method == Method::HEAD)),
        "PUT" => put(runtime, account, resource, request, &body).await,
        "DELETE" => delete(runtime, account, resource, request).await,
        _ => Ok(text(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")),
    };
    match result {
        Ok(mut response) => {
            response
                .headers_mut()
                .insert("DAV", HeaderValue::from_static(DAV_CAPABILITIES));
            tracing::debug!(
                account = %account.config.name,
                method,
                path,
                status = response.status().as_u16(),
                "CalDAV request"
            );
            response
        }
        Err(error) => server_error(error),
    }
}

fn server_error(error: anyhow::Error) -> HttpResponse {
    tracing::warn!(error = format!("{error:#}"), "CalDAV request failed");
    text(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

fn resolve(
    runtime: &Runtime,
    account: &AccountRuntime,
    path: &str,
) -> anyhow::Result<Option<Resource>> {
    let segments: Vec<&str> = path
        .trim_start_matches("/dav")
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    let name = account.config.name.as_str();
    let Some((first, rest)) = segments.split_first() else {
        return Ok(Some(Resource::Root));
    };
    if !percent_decode(first).eq_ignore_ascii_case(name) {
        return Ok(None);
    }
    Ok(match rest {
        [] => Some(Resource::Principal),
        ["calendars"] => Some(Resource::CalendarHome),
        ["calendars", slug] => runtime
            .store
            .calendar_by_slug(name, slug)?
            .map(Resource::Calendar),
        ["calendars", slug, object] => {
            let Some(calendar) = runtime.store.calendar_by_slug(name, slug)? else {
                return Ok(None);
            };
            let stored = runtime
                .store
                .object(name, &calendar.id, object)?
                .map(Box::new);
            Some(Resource::Object(calendar, (*object).to_owned(), stored))
        }
        _ => None,
    })
}

fn options() -> HttpResponse {
    let mut response = text(StatusCode::OK, "");
    response
        .headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static(ALLOW));
    response
}

// ----- paths -----------------------------------------------------------------

fn account_segment(account: &AccountRuntime) -> String {
    let mut encoded = String::new();
    for byte in account.config.name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn principal_href(account: &AccountRuntime) -> String {
    format!("/dav/{}/", account_segment(account))
}

fn home_href(account: &AccountRuntime) -> String {
    format!("/dav/{}/calendars/", account_segment(account))
}

fn calendar_href(account: &AccountRuntime, calendar: &StoredCalendar) -> String {
    format!("{}{}/", home_href(account), calendar.slug)
}

fn object_href(account: &AccountRuntime, calendar: &StoredCalendar, name: &str) -> String {
    format!("{}{name}", calendar_href(account, calendar))
}

fn href(inner: &str) -> String {
    format!("<D:href>{}</D:href>", escape(inner))
}

fn sync_token(calendar: &StoredCalendar) -> String {
    format!("{SYNC_TOKEN_PREFIX}{}", calendar.modseq)
}

// ----- properties ------------------------------------------------------------

fn prop(ns: &str, name: &str, value: impl Into<String>) -> (PropName, String) {
    (PropName::new(ns, name), value.into())
}

/// Properties every resource of the account shares.
fn account_props(account: &AccountRuntime) -> Vec<(PropName, String)> {
    let principal = href(&principal_href(account));
    vec![
        prop(DAV, "current-user-principal", principal.clone()),
        prop(DAV, "principal-URL", principal.clone()),
        prop(DAV, "owner", principal),
        prop(CALDAV, "calendar-home-set", href(&home_href(account))),
        prop(
            CALDAV,
            "calendar-user-address-set",
            href(&format!("mailto:{}", account.config.email)),
        ),
    ]
}

fn privileges(writable: bool) -> String {
    let mut privileges = vec!["read", "read-current-user-privilege-set"];
    if writable {
        privileges.extend(["write", "write-content", "bind", "unbind"]);
    }
    privileges
        .iter()
        .map(|privilege| format!("<D:privilege><D:{privilege}/></D:privilege>"))
        .collect()
}

fn resource_props(
    account: &AccountRuntime,
    resource: &Resource,
    calendar_order: usize,
) -> Vec<(PropName, String)> {
    let mut props = account_props(account);
    match resource {
        Resource::Root | Resource::CalendarHome => {
            props.push(prop(DAV, "resourcetype", "<D:collection/>"));
            props.push(prop(DAV, "current-user-privilege-set", privileges(false)));
        }
        Resource::Principal => {
            props.push(prop(DAV, "resourcetype", "<D:collection/><D:principal/>"));
            props.push(prop(DAV, "displayname", escape(&account.config.email)));
            props.push(prop(DAV, "current-user-privilege-set", privileges(false)));
        }
        Resource::Calendar(calendar) => {
            props.extend(calendar_props(calendar, calendar_order));
        }
        Resource::Object(_, _, Some(object)) => props.extend(object_props(object)),
        Resource::Object(_, _, None) => {}
    }
    props
}

fn calendar_props(calendar: &StoredCalendar, order: usize) -> Vec<(PropName, String)> {
    let mut props = vec![
        prop(DAV, "resourcetype", "<D:collection/><C:calendar/>"),
        prop(DAV, "displayname", escape(&calendar.name)),
        prop(
            DAV,
            "current-user-privilege-set",
            privileges(calendar.can_edit),
        ),
        prop(
            DAV,
            "supported-report-set",
            [
                "C:calendar-multiget",
                "C:calendar-query",
                "D:sync-collection",
            ]
            .iter()
            .map(|report| {
                format!("<D:supported-report><D:report><{report}/></D:report></D:supported-report>")
            })
            .collect::<String>(),
        ),
        prop(DAV, "sync-token", escape(&sync_token(calendar))),
        prop(DAV, "getetag", format!("\"{}\"", calendar.modseq)),
        prop(CALSERVER, "getctag", calendar.modseq.to_string()),
        prop(
            CALDAV,
            "supported-calendar-component-set",
            "<C:comp name=\"VEVENT\"/>",
        ),
        prop(
            CALDAV,
            "supported-calendar-data",
            "<C:calendar-data content-type=\"text/calendar\" version=\"2.0\"/>",
        ),
        prop(
            CALDAV,
            "calendar-description",
            escape(&format!("Microsoft 365 calendar {}", calendar.name)),
        ),
        prop(APPLE_ICAL, "calendar-order", order.to_string()),
    ];
    if let Some(color) = &calendar.color {
        props.push(prop(APPLE_ICAL, "calendar-color", escape(color)));
    }
    props
}

fn object_props(object: &StoredObject) -> Vec<(PropName, String)> {
    vec![
        prop(DAV, "resourcetype", ""),
        prop(DAV, "getetag", escape(&object.etag)),
        prop(
            DAV,
            "getcontenttype",
            "text/calendar; charset=utf-8; component=VEVENT",
        ),
        prop(DAV, "getcontentlength", object.ics.len().to_string()),
    ]
}

/// Object properties plus `calendar-data` when it was asked for by name.
fn object_props_with_data(object: &StoredObject, requested: &Requested) -> Vec<(PropName, String)> {
    let mut props = object_props(object);
    if let Requested::Props(names) = requested
        && names.iter().any(|name| name.is(CALDAV, "calendar-data"))
    {
        props.push(prop(CALDAV, "calendar-data", escape(&object.ics)));
    }
    props
}

// ----- PROPFIND --------------------------------------------------------------

fn propfind(
    runtime: &Runtime,
    account: &AccountRuntime,
    resource: &Resource,
    request: &Parts,
    body: &[u8],
) -> anyhow::Result<HttpResponse> {
    let root = match parse_body(body) {
        Ok(root) => root,
        Err(response) => return Ok(*response),
    };
    let requested = Requested::from_request(root.as_ref());
    let depth_one = request
        .headers
        .get("Depth")
        .and_then(|value| value.to_str().ok())
        .is_none_or(|depth| depth.trim() != "0");
    let name = account.config.name.as_str();
    let calendars = runtime.store.calendars(name)?;
    let order_of = |calendar: &StoredCalendar| {
        calendars
            .iter()
            .position(|candidate| candidate.id == calendar.id)
            .unwrap_or(0)
    };
    let mut multistatus = Multistatus::new();
    match resource {
        Resource::Root => {
            multistatus.props("/dav/", &requested, &resource_props(account, resource, 0));
            if depth_one {
                multistatus.props(
                    &principal_href(account),
                    &requested,
                    &resource_props(account, &Resource::Principal, 0),
                );
            }
        }
        Resource::Principal => {
            multistatus.props(
                &principal_href(account),
                &requested,
                &resource_props(account, resource, 0),
            );
            if depth_one {
                multistatus.props(
                    &home_href(account),
                    &requested,
                    &resource_props(account, &Resource::CalendarHome, 0),
                );
            }
        }
        Resource::CalendarHome => {
            multistatus.props(
                &home_href(account),
                &requested,
                &resource_props(account, resource, 0),
            );
            if depth_one {
                for (order, calendar) in calendars.iter().enumerate() {
                    multistatus.props(
                        &calendar_href(account, calendar),
                        &requested,
                        &resource_props(account, &Resource::Calendar(calendar.clone()), order),
                    );
                }
            }
        }
        Resource::Calendar(calendar) => {
            multistatus.props(
                &calendar_href(account, calendar),
                &requested,
                &resource_props(account, resource, order_of(calendar)),
            );
            if depth_one {
                for object in runtime.store.objects(name, &calendar.id)? {
                    multistatus.props(
                        &object_href(account, calendar, &object.href),
                        &requested,
                        &object_props_with_data(&object, &requested),
                    );
                }
            }
        }
        Resource::Object(calendar, object_name, Some(object)) => {
            multistatus.props(
                &object_href(account, calendar, object_name),
                &requested,
                &object_props_with_data(object, &requested),
            );
        }
        Resource::Object(_, _, None) => return Ok(text(StatusCode::NOT_FOUND, "not found")),
    }
    Ok(multistatus_response(multistatus))
}

// ----- REPORT ----------------------------------------------------------------

fn report(
    runtime: &Runtime,
    account: &AccountRuntime,
    resource: &Resource,
    body: &[u8],
) -> anyhow::Result<HttpResponse> {
    let Resource::Calendar(calendar) = resource else {
        return Ok(text(
            StatusCode::FORBIDDEN,
            "REPORT is supported on calendars only",
        ));
    };
    let root = match parse_body(body) {
        Ok(Some(root)) => root,
        Ok(None) => return Ok(text(StatusCode::BAD_REQUEST, "REPORT needs a body")),
        Err(response) => return Ok(*response),
    };
    let requested = Requested::from_request(Some(&root));
    let name = account.config.name.as_str();
    let mut multistatus = Multistatus::new();
    if root.is(CALDAV, "calendar-multiget") {
        let prefix = calendar_href(account, calendar);
        for requested_href in root.children_named(DAV, "href") {
            let target = requested_href.text.trim();
            let object_name = target
                .strip_prefix(&prefix)
                .or_else(|| target.rsplit_once('/').map(|(_, name)| name))
                .unwrap_or(target);
            match runtime.store.object(name, &calendar.id, object_name)? {
                Some(object) => multistatus.props(
                    target,
                    &requested,
                    &object_props_with_data(&object, &requested),
                ),
                None => multistatus.status(target, "404 Not Found"),
            }
        }
    } else if root.is(CALDAV, "calendar-query") {
        // Only events are stored, and returning more than a time-range
        // filter asks for is allowed for clients that filter themselves:
        // EDS runs a second, unbounded query anyway.
        if query_wants_events(&root) {
            for object in runtime.store.objects(name, &calendar.id)? {
                multistatus.props(
                    &object_href(account, calendar, &object.href),
                    &requested,
                    &object_props_with_data(&object, &requested),
                );
            }
        }
    } else if root.is(DAV, "sync-collection") {
        let token = root
            .child(DAV, "sync-token")
            .map(|token| token.text.trim().to_owned())
            .unwrap_or_default();
        let since = if token.is_empty() {
            0
        } else {
            match token
                .strip_prefix(SYNC_TOKEN_PREFIX)
                .and_then(|value| value.parse::<i64>().ok())
                .filter(|value| *value <= calendar.modseq)
            {
                Some(since) => since,
                None => return Ok(dav_error(StatusCode::FORBIDDEN, "<D:valid-sync-token/>")),
            }
        };
        let (changed, removed) = runtime
            .store
            .calendar_changes_since(name, &calendar.id, since)?;
        for object in changed {
            multistatus.props(
                &object_href(account, calendar, &object.href),
                &requested,
                &object_props_with_data(&object, &requested),
            );
        }
        if since > 0 {
            for removed_href in removed {
                multistatus.status(
                    &object_href(account, calendar, &removed_href),
                    "404 Not Found",
                );
            }
        }
        multistatus.sync_token(&sync_token(calendar));
    } else {
        return Ok(dav_error(StatusCode::FORBIDDEN, "<D:supported-report/>"));
    }
    Ok(multistatus_response(multistatus))
}

/// Whether a calendar-query's filter can match a VEVENT.
fn query_wants_events(root: &Element) -> bool {
    let Some(filter) = root.child(CALDAV, "filter") else {
        return true;
    };
    let Some(calendar_filter) = filter.child(CALDAV, "comp-filter") else {
        return true;
    };
    let mut components = calendar_filter
        .children_named(CALDAV, "comp-filter")
        .peekable();
    if components.peek().is_none() {
        return true;
    }
    components.any(|component| {
        component
            .attribute("name")
            .is_some_and(|name| name.eq_ignore_ascii_case("VEVENT"))
    })
}

// ----- GET / PUT / DELETE ----------------------------------------------------

fn get(resource: &Resource, head: bool) -> HttpResponse {
    let Resource::Object(_, _, Some(object)) = resource else {
        return text(StatusCode::NOT_FOUND, "not found");
    };
    let body = if head {
        Bytes::new()
    } else {
        Bytes::from(object.ics.clone())
    };
    let mut response = Response::new(Full::new(body));
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(ICS_TYPE));
    if let Ok(etag) = HeaderValue::from_str(&object.etag) {
        headers.insert(header::ETAG, etag);
    }
    response
}

async fn put(
    runtime: &Runtime,
    account: &AccountRuntime,
    resource: Resource,
    request: &Parts,
    body: &[u8],
) -> anyhow::Result<HttpResponse> {
    let Resource::Object(calendar, object_name, _) = resource else {
        return Ok(text(
            StatusCode::METHOD_NOT_ALLOWED,
            "PUT objects into a calendar",
        ));
    };
    let Ok(body) = std::str::from_utf8(body) else {
        return Ok(text(StatusCode::BAD_REQUEST, "the body is not UTF-8"));
    };
    let preconditions = preconditions(request);
    let outcome = apply::put(
        runtime,
        account,
        &calendar,
        &object_name,
        body,
        &Preconditions {
            if_match: preconditions.0.as_deref(),
            if_none_match_any: preconditions.1,
        },
    )
    .await;
    Ok(match outcome {
        // No ETag: what Graph stored differs from what was sent, so the
        // client must GET it back (RFC 4791 §5.3.4).
        Ok(true) => text(StatusCode::CREATED, ""),
        Ok(false) => text(StatusCode::NO_CONTENT, ""),
        Err(error) => write_error(account, error),
    })
}

async fn delete(
    runtime: &Runtime,
    account: &AccountRuntime,
    resource: Resource,
    request: &Parts,
) -> anyhow::Result<HttpResponse> {
    let Resource::Object(calendar, object_name, _) = resource else {
        return Ok(text(StatusCode::FORBIDDEN, "only events can be deleted"));
    };
    let preconditions = preconditions(request);
    let outcome = apply::delete(
        runtime,
        account,
        &calendar,
        &object_name,
        &Preconditions {
            if_match: preconditions.0.as_deref(),
            if_none_match_any: preconditions.1,
        },
    )
    .await;
    Ok(match outcome {
        Ok(()) => text(StatusCode::NO_CONTENT, ""),
        Err(error) => write_error(account, error),
    })
}

/// `(If-Match, If-None-Match: *)`.
fn preconditions(request: &Parts) -> (Option<String>, bool) {
    let header = |name: &str| {
        request
            .headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let if_none_match_any = header("If-None-Match").is_some_and(|value| value.trim() == "*");
    (header("If-Match"), if_none_match_any)
}

fn write_error(account: &AccountRuntime, error: WriteError) -> HttpResponse {
    match error {
        WriteError::PreconditionFailed => {
            text(StatusCode::PRECONDITION_FAILED, "precondition failed")
        }
        WriteError::NotFound => text(StatusCode::NOT_FOUND, "not found"),
        WriteError::ReadOnly => text(StatusCode::FORBIDDEN, "this calendar is read-only"),
        WriteError::Invalid(message) => {
            tracing::info!(account = %account.config.name, %message, "rejected an invalid calendar object");
            dav_error_text(
                StatusCode::BAD_REQUEST,
                "<C:valid-calendar-data/>",
                &message,
            )
        }
        WriteError::Unsupported(message) => {
            tracing::info!(account = %account.config.name, %message, "rejected a calendar object Exchange cannot hold");
            dav_error_text(
                StatusCode::FORBIDDEN,
                "<C:valid-calendar-object-resource/>",
                &message,
            )
        }
        WriteError::Graph(error) => {
            tracing::warn!(account = %account.config.name, error = format!("{error:#}"), "calendar write failed");
            text(
                StatusCode::BAD_GATEWAY,
                &format!("Microsoft Graph refused the change: {error:#}"),
            )
        }
    }
}

// ----- helpers ---------------------------------------------------------------

/// The parsed body, `None` when empty, or a 400 response.
fn parse_body(body: &[u8]) -> Result<Option<Element>, Box<HttpResponse>> {
    let Ok(text_body) = std::str::from_utf8(body) else {
        return Err(Box::new(text(
            StatusCode::BAD_REQUEST,
            "the body is not UTF-8",
        )));
    };
    if text_body.trim().is_empty() {
        return Ok(None);
    }
    xml::parse(text_body).map(Some).map_err(|error| {
        Box::new(text(
            StatusCode::BAD_REQUEST,
            &format!("invalid XML: {error:#}"),
        ))
    })
}

fn multistatus_response(multistatus: Multistatus) -> HttpResponse {
    let mut response = Response::new(Full::new(Bytes::from(multistatus.finish())));
    *response.status_mut() = StatusCode::MULTI_STATUS;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/xml; charset=utf-8"),
    );
    response
}

fn dav_error(status: StatusCode, condition: &str) -> HttpResponse {
    dav_error_text(status, condition, "")
}

/// An RFC 4918 `<D:error>` body naming the failed precondition.
fn dav_error_text(status: StatusCode, condition: &str, message: &str) -> HttpResponse {
    let description = if message.is_empty() {
        String::new()
    } else {
        format!(
            "<D:responsedescription>{}</D:responsedescription>",
            escape(message)
        )
    };
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:error xmlns:D=\"DAV:\" xmlns:C=\"{CALDAV}\">{condition}{description}</D:error>\n"
    );
    let mut response = Response::new(Full::new(Bytes::from(body)));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/xml; charset=utf-8"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_queries_for_other_components_match_nothing() {
        let todo = xml::parse(
            r#"<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
                 <D:prop><D:getetag/></D:prop>
                 <C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VTODO"/></C:comp-filter></C:filter>
               </C:calendar-query>"#,
        )
        .unwrap();
        assert!(!query_wants_events(&todo));
        let events = xml::parse(
            r#"<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
                 <C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VEVENT">
                   <C:time-range start="20260901T000000Z" end="20261101T000000Z"/>
                 </C:comp-filter></C:comp-filter></C:filter>
               </C:calendar-query>"#,
        )
        .unwrap();
        assert!(query_wants_events(&events));
    }
}
