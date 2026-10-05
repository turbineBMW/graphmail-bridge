// SPDX-License-Identifier: GPL-2.0-or-later

//! Background synchronisation of each account's calendars into the local
//! store. Graph has no per-calendar delta in v1.0, but a listing of event
//! ids and change keys is cheap; only events whose key changed are fetched.
//! A change to one occurrence of a series changes the master's key too.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{NaiveDate, Utc};

use super::model::{Calendar, Event};
use super::render::{RenderContext, render};
use super::store::{ObjectWrite, slug};
use crate::service::{AccountRuntime, Runtime};
use crate::timezones;

/// Bump when rendering changes, so every stored object is rebuilt.
pub const RENDER_VERSION: i64 = 2;
/// More changed events than this are fetched with one listing instead of
/// one request each.
const BULK_THRESHOLD: usize = 20;
/// How far ahead exceptions of open-ended series are looked for.
const FUTURE_DAYS: i64 = 2 * 365;

pub fn spawn(runtime: Arc<Runtime>, account: Arc<AccountRuntime>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move { run(runtime, account).await })
}

async fn run(runtime: Arc<Runtime>, account: Arc<AccountRuntime>) {
    let name = account.config.name.as_str();
    let poll = Duration::from_secs(runtime.config.sync.calendar_poll_secs);
    let mut backoff = Duration::from_secs(30);
    tracing::info!(account = name, "calendar sync started");
    loop {
        let pause = match sync_account(&runtime, &account).await {
            Ok(()) => {
                backoff = Duration::from_secs(30);
                poll
            }
            Err(error) => {
                tracing::warn!(
                    account = name,
                    error = format!("{error:#}"),
                    "calendar sync failed"
                );
                let pause = backoff;
                backoff = (backoff * 2).min(Duration::from_secs(900));
                pause
            }
        };
        tokio::time::sleep(pause).await;
    }
}

/// Refresh the calendar list and every calendar's events once.
pub async fn sync_account(runtime: &Runtime, account: &AccountRuntime) -> Result<()> {
    let name = account.config.name.as_str();
    let calendars = account
        .graph
        .calendars()
        .await
        .context("could not list calendars")?;
    runtime.store.replace_calendars(name, &calendars)?;
    for calendar in &calendars {
        let _guard = account.calendar_lock.lock().await;
        match sync_calendar(runtime, account, calendar).await {
            Ok(()) => runtime
                .store
                .set_calendar_sync_result(name, &calendar.id, None)?,
            Err(error) => {
                let text = format!("{error:#}");
                tracing::warn!(account = name, calendar = %calendar.name, error = %text, "calendar sync failed");
                runtime
                    .store
                    .set_calendar_sync_result(name, &calendar.id, Some(&text))?;
            }
        }
    }
    Ok(())
}

async fn sync_calendar(
    runtime: &Runtime,
    account: &AccountRuntime,
    calendar: &Calendar,
) -> Result<()> {
    let store = &runtime.store;
    let name = account.config.name.as_str();
    let since = since(runtime.config.sync.calendar_past_days);
    let keys = account
        .graph
        .calendar_event_keys(&calendar.id, since.as_deref())
        .await?;
    let stored = store.object_versions(name, &calendar.id)?;
    let listed: HashSet<&str> = keys.iter().map(|key| key.id.as_str()).collect();
    let removed: Vec<String> = stored
        .keys()
        .filter(|id| !listed.contains(id.as_str()))
        .cloned()
        .collect();
    store.remove_objects(name, &calendar.id, &removed)?;
    let stale: HashSet<&str> = keys
        .iter()
        .filter(|key| {
            stored.get(&key.id).is_none_or(|(change_key, version)| {
                *change_key != key.change_key || *version != RENDER_VERSION
            })
        })
        .map(|key| key.id.as_str())
        .collect();
    if stale.is_empty() && removed.is_empty() {
        return Ok(());
    }
    let events = if stale.len() > BULK_THRESHOLD {
        account
            .graph
            .calendar_events(&calendar.id, since.as_deref())
            .await?
            .into_iter()
            .filter(|event| stale.contains(event.id.as_str()))
            .collect()
    } else {
        let mut events = Vec::with_capacity(stale.len());
        for id in &stale {
            if let Some(event) = account.graph.event(id).await? {
                events.push(event);
            }
        }
        events
    };
    let fetched = events.len();
    for event in events {
        store_event(runtime, account, &calendar.id, event, None).await?;
    }
    tracing::info!(
        account = name,
        calendar = %calendar.name,
        updated = fetched,
        removed = removed.len(),
        "calendar changes applied"
    );
    Ok(())
}

/// Fetch what a series needs besides its master (cancellations and
/// exceptions), render the object and store it. Keeps the href and UID a
/// client already knows the object by; `identity` names them for an
/// object a client just created.
pub async fn store_event(
    runtime: &Runtime,
    account: &AccountRuntime,
    calendar_id: &str,
    mut event: Event,
    identity: Option<(&str, &str)>,
) -> Result<()> {
    let name = account.config.name.as_str();
    let mut exceptions = Vec::new();
    if event.is_series_master() {
        event.cancelled_occurrences = account.graph.cancelled_occurrences(&event.id).await?;
        if let Some((start, end)) = exception_window(&event, runtime.config.sync.calendar_past_days)
        {
            exceptions = account
                .graph
                .instances(&event.id, &start, &end)
                .await?
                .into_iter()
                .filter(|instance| instance.kind.as_deref() == Some("exception"))
                .collect();
        }
    }
    let existing = runtime.store.object_by_event(name, &event.id)?;
    let (href, uid) = match (identity, existing) {
        (Some((href, uid)), _) => (href.to_owned(), uid.to_owned()),
        (None, Some(object)) => (object.href, object.uid),
        (None, None) => new_identity(&event),
    };
    let ics = render(
        &event,
        &exceptions,
        &RenderContext {
            uid: &uid,
            account_email: &account.config.email,
            local: timezones::local_zone(),
        },
    );
    runtime.store.put_object(
        name,
        &ObjectWrite {
            calendar_id,
            href: &href,
            event_id: &event.id,
            uid: &uid,
            change_key: event.change_key.as_deref(),
            graph_json: &serde_json::to_string(&event)?,
            exceptions_json: &serde_json::to_string(&exceptions)?,
            ics: &ics,
            render_version: RENDER_VERSION,
        },
    )?;
    Ok(())
}

/// The href and UID of an object first seen on Graph: the iCalendar UID
/// Exchange assigned, which is hex and therefore URL-safe.
fn new_identity(event: &Event) -> (String, String) {
    let uid = event.i_cal_uid.clone().unwrap_or_else(|| event.id.clone());
    let safe = uid
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    let stem = if safe { uid.clone() } else { slug(&uid) };
    (format!("{stem}.ics"), uid)
}

/// The listing filter's lower bound, `None` to list everything.
fn since(past_days: u32) -> Option<String> {
    (past_days > 0).then(|| {
        (Utc::now() - chrono::Duration::days(i64::from(past_days)))
            .format("%Y-%m-%dT00:00:00")
            .to_string()
    })
}

/// The UTC window in which a series' exceptions are looked for.
fn exception_window(event: &Event, past_days: u32) -> Option<(String, String)> {
    let range = &event.recurrence.as_ref()?.range;
    let parse = |value: &Option<String>| {
        value
            .as_deref()
            .and_then(|value| NaiveDate::parse_from_str(value, "%Y-%m-%d").ok())
    };
    let today = Utc::now().date_naive();
    let mut start = parse(&range.start_date).unwrap_or(today) - chrono::Duration::days(1);
    if past_days > 0 {
        start = start.max(today - chrono::Duration::days(i64::from(past_days)));
    }
    let end = match range.kind.as_str() {
        "endDate" => parse(&range.end_date)? + chrono::Duration::days(2),
        _ => today + chrono::Duration::days(FUTURE_DAYS),
    };
    (start < end).then(|| {
        (
            format!("{}T00:00:00Z", start.format("%Y-%m-%d")),
            format!("{}T00:00:00Z", end.format("%Y-%m-%d")),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exchange_uids_become_hrefs_directly() {
        let event = Event {
            id: "AAkA+/x=".into(),
            i_cal_uid: Some("040000008200E00074C5B7101A82E008".into()),
            ..Default::default()
        };
        assert_eq!(
            new_identity(&event),
            (
                "040000008200E00074C5B7101A82E008.ics".to_owned(),
                "040000008200E00074C5B7101A82E008".to_owned()
            )
        );
        let odd = Event {
            id: "x".into(),
            i_cal_uid: Some("a/b@c".into()),
            ..Default::default()
        };
        let (href, uid) = new_identity(&odd);
        assert_eq!(uid, "a/b@c");
        assert!(href.ends_with(".ics") && !href.contains('/'));
    }
}
