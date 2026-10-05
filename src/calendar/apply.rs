// SPDX-License-Identifier: GPL-2.0-or-later

//! Carry out a client's PUT or DELETE against Graph, then refresh the
//! stored copy from Graph so the next GET serves what Exchange now holds.

use chrono::{DateTime, Duration, SecondsFormat, Utc};

use super::store::StoredCalendar;
use super::sync::store_event;
use super::write::{Change, ParsedObject, WriteContext, changes, create_body, parse_object};
use crate::service::{AccountRuntime, Runtime};
use crate::timezones;

#[derive(Debug)]
pub enum WriteError {
    /// A precondition (`If-Match` / `If-None-Match`) failed.
    PreconditionFailed,
    /// The object is valid iCalendar but cannot be stored in Exchange.
    Unsupported(String),
    /// The body is not a usable iCalendar object.
    Invalid(String),
    NotFound,
    ReadOnly,
    Graph(anyhow::Error),
}

pub struct Preconditions<'a> {
    /// `If-Match` value, with quotes.
    pub if_match: Option<&'a str>,
    /// `If-None-Match: *`.
    pub if_none_match_any: bool,
}

/// Store a client's object at `href`. Returns whether it was created.
pub async fn put(
    runtime: &Runtime,
    account: &AccountRuntime,
    calendar: &StoredCalendar,
    href: &str,
    body: &str,
    preconditions: &Preconditions<'_>,
) -> Result<bool, WriteError> {
    if !calendar.can_edit {
        return Err(WriteError::ReadOnly);
    }
    let wanted = parse_object(body).map_err(|error| WriteError::Invalid(format!("{error:#}")))?;
    let context = WriteContext {
        account_email: &account.config.email,
        local: timezones::local_zone(),
    };
    let name = account.config.name.as_str();
    let _guard = account.calendar_lock.lock().await;
    let existing = runtime
        .store
        .object(name, &calendar.id, href)
        .map_err(WriteError::Graph)?;
    check(
        existing.as_ref().map(|object| object.etag.as_str()),
        preconditions,
    )?;

    let Some(existing) = existing else {
        let master = wanted.master.as_ref().ok_or_else(|| {
            WriteError::Unsupported("a new event needs its master component".into())
        })?;
        let create = create_body(master, &context)
            .map_err(|error| WriteError::Unsupported(format!("{error:#}")))?;
        let created = account
            .graph
            .create_event(&calendar.id, &create)
            .await
            .map_err(WriteError::Graph)?;
        // Cancelled and moved occurrences are applied to the new series as
        // changes against a copy of it that has neither.
        let mut bare_master = master.clone();
        bare_master
            .properties
            .retain(|property| property.name != "EXDATE");
        let served = ParsedObject {
            uid: wanted.uid.clone(),
            master: Some(bare_master),
            overrides: Vec::new(),
        };
        let pending = changes(&served, &wanted, &context)
            .map_err(|error| WriteError::Unsupported(format!("{error:#}")))?;
        let outcome = apply(account, &created.id, pending).await;
        refresh(runtime, account, calendar, &created.id, href, &wanted.uid).await?;
        outcome?;
        return Ok(true);
    };

    let served = parse_object(&existing.ics).map_err(WriteError::Graph)?;
    if served.uid != wanted.uid {
        return Err(WriteError::Invalid(
            "the UID of an existing object cannot change".into(),
        ));
    }
    let pending = changes(&served, &wanted, &context)
        .map_err(|error| WriteError::Unsupported(format!("{error:#}")))?;
    let outcome = apply(account, &existing.event_id, pending).await;
    refresh(
        runtime,
        account,
        calendar,
        &existing.event_id,
        href,
        &existing.uid,
    )
    .await?;
    outcome?;
    Ok(false)
}

pub async fn delete(
    runtime: &Runtime,
    account: &AccountRuntime,
    calendar: &StoredCalendar,
    href: &str,
    preconditions: &Preconditions<'_>,
) -> Result<(), WriteError> {
    if !calendar.can_edit {
        return Err(WriteError::ReadOnly);
    }
    let name = account.config.name.as_str();
    let _guard = account.calendar_lock.lock().await;
    let existing = runtime
        .store
        .object(name, &calendar.id, href)
        .map_err(WriteError::Graph)?
        .ok_or(WriteError::NotFound)?;
    check(Some(&existing.etag), preconditions)?;
    account
        .graph
        .delete_event(&existing.event_id)
        .await
        .map_err(WriteError::Graph)?;
    runtime
        .store
        .remove_objects(name, &calendar.id, &[existing.event_id])
        .map_err(WriteError::Graph)?;
    Ok(())
}

fn check(current: Option<&str>, preconditions: &Preconditions<'_>) -> Result<(), WriteError> {
    if preconditions.if_none_match_any && current.is_some() {
        return Err(WriteError::PreconditionFailed);
    }
    if let Some(expected) = preconditions.if_match
        && expected.trim() != "*"
        && current != Some(expected.trim())
    {
        return Err(WriteError::PreconditionFailed);
    }
    if preconditions.if_match.is_some() && current.is_none() {
        return Err(WriteError::PreconditionFailed);
    }
    Ok(())
}

async fn apply(
    account: &AccountRuntime,
    event_id: &str,
    pending: Vec<Change>,
) -> Result<(), WriteError> {
    let graph = &account.graph;
    for change in pending {
        let result = match change {
            Change::Patch(patch) => graph.update_event(event_id, &patch).await,
            Change::Respond(action) => graph.respond_to_event(event_id, action).await,
            Change::PatchOccurrence(instant, patch) => {
                let id = occurrence(account, event_id, instant).await?;
                graph.update_event(&id, &patch).await
            }
            Change::RespondOccurrence(instant, action) => {
                let id = occurrence(account, event_id, instant).await?;
                graph.respond_to_event(&id, action).await
            }
            Change::DeleteOccurrence(instant) => {
                let id = occurrence(account, event_id, instant).await?;
                graph.delete_event(&id).await
            }
        };
        result.map_err(WriteError::Graph)?;
    }
    Ok(())
}

/// The Graph id of the occurrence of a series originally at `instant`.
async fn occurrence(
    account: &AccountRuntime,
    master_id: &str,
    instant: DateTime<Utc>,
) -> Result<String, WriteError> {
    let format = |at: DateTime<Utc>| at.to_rfc3339_opts(SecondsFormat::Secs, true);
    let instances = account
        .graph
        .instances(
            master_id,
            &format(instant - Duration::days(1)),
            &format(instant + Duration::days(1)),
        )
        .await
        .map_err(WriteError::Graph)?;
    instances
        .into_iter()
        .find(|candidate| {
            candidate
                .original_start
                .as_deref()
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .is_some_and(|start| start == instant)
        })
        .map(|found| found.id)
        .ok_or_else(|| {
            WriteError::Unsupported(format!(
                "the series has no occurrence at {}",
                format(instant)
            ))
        })
}

/// Re-read an event from Graph and store it under the client's href and UID.
async fn refresh(
    runtime: &Runtime,
    account: &AccountRuntime,
    calendar: &StoredCalendar,
    event_id: &str,
    href: &str,
    uid: &str,
) -> Result<(), WriteError> {
    let event = account
        .graph
        .event(event_id)
        .await
        .map_err(WriteError::Graph)?
        .ok_or(WriteError::NotFound)?;
    store_event(runtime, account, &calendar.id, event, Some((href, uid)))
        .await
        .map_err(WriteError::Graph)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preconditions_follow_rfc_7232() {
        let none = Preconditions {
            if_match: None,
            if_none_match_any: false,
        };
        assert!(check(None, &none).is_ok());
        let create_only = Preconditions {
            if_match: None,
            if_none_match_any: true,
        };
        assert!(check(None, &create_only).is_ok());
        assert!(check(Some("\"a\""), &create_only).is_err());
        let matching = Preconditions {
            if_match: Some("\"a\""),
            if_none_match_any: false,
        };
        assert!(check(Some("\"a\""), &matching).is_ok());
        assert!(check(Some("\"b\""), &matching).is_err());
        assert!(check(None, &matching).is_err());
    }
}
