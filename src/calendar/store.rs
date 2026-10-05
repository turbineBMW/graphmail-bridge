// SPDX-License-Identifier: GPL-2.0-or-later

//! Calendar rows in the local store: one row per Graph calendar, one object
//! per single event or series, and tombstones for deleted objects. Every
//! change bumps the calendar's `modseq`, which serves as its CTag.

use std::collections::HashMap;

use anyhow::Result;
use rusqlite::{OptionalExtension, Transaction, params};

use super::model::Calendar;
use crate::store::Store;

#[derive(Clone, Debug)]
pub struct StoredCalendar {
    pub id: String,
    /// The calendar's URL segment.
    pub slug: String,
    pub name: String,
    pub color: Option<String>,
    pub can_edit: bool,
    pub is_default: bool,
    pub modseq: i64,
}

#[derive(Clone, Debug)]
pub struct StoredObject {
    pub calendar_id: String,
    /// The object's file name in its collection, percent-encoded exactly as
    /// clients see it.
    pub href: String,
    pub event_id: String,
    pub uid: String,
    pub change_key: Option<String>,
    pub graph_json: String,
    pub exceptions_json: String,
    pub ics: String,
    pub etag: String,
    pub render_version: i64,
}

/// A calendar's sync state, for status output.
#[derive(Clone, Debug)]
pub struct CalendarStatus {
    pub name: String,
    pub can_edit: bool,
    pub objects: u64,
    pub last_sync: Option<i64>,
    pub last_error: Option<String>,
}

/// An object to store; its etag is derived from `ics`.
pub struct ObjectWrite<'a> {
    pub calendar_id: &'a str,
    pub href: &'a str,
    pub event_id: &'a str,
    pub uid: &'a str,
    pub change_key: Option<&'a str>,
    pub graph_json: &'a str,
    pub exceptions_json: &'a str,
    pub ics: &'a str,
    pub render_version: i64,
}

const OBJECT_COLUMNS: &str = "calendar_id, href, event_id, uid, change_key, graph_json, exceptions_json, ics, etag, render_version";

impl Store {
    /// Mirror the account's calendar list. Calendars that vanished are
    /// dropped with everything in them.
    pub fn replace_calendars(&self, account: &str, calendars: &[Calendar]) -> Result<()> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        for calendar in calendars {
            let color = normalize_color(&calendar.hex_color);
            transaction.execute(
                "INSERT INTO calendars (account, calendar_id, slug, name, color, can_edit, is_default)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(account, calendar_id) DO UPDATE SET
                    name=excluded.name, color=excluded.color,
                    can_edit=excluded.can_edit, is_default=excluded.is_default",
                params![
                    account,
                    calendar.id,
                    slug(&calendar.id),
                    calendar.name,
                    color,
                    calendar.can_edit,
                    calendar.is_default_calendar
                ],
            )?;
        }
        let existing: Vec<String> = {
            let mut statement =
                transaction.prepare("SELECT calendar_id FROM calendars WHERE account=?1")?;
            let rows = statement.query_map(params![account], |row| row.get(0))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        for id in existing {
            if !calendars.iter().any(|calendar| calendar.id == id) {
                transaction.execute(
                    "DELETE FROM calendars WHERE account=?1 AND calendar_id=?2",
                    params![account, id],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn calendars(&self, account: &str) -> Result<Vec<StoredCalendar>> {
        let connection = self.lock();
        let mut statement = connection.prepare(
            "SELECT calendar_id, slug, name, color, can_edit, is_default, modseq
             FROM calendars WHERE account=?1 ORDER BY is_default DESC, name",
        )?;
        let rows = statement.query_map(params![account], calendar_row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn calendar_status(&self, account: &str) -> Result<Vec<CalendarStatus>> {
        let connection = self.lock();
        let mut statement = connection.prepare(
            "SELECT name, can_edit, last_sync, last_error,
                    (SELECT COUNT(*) FROM calendar_objects o
                     WHERE o.account=c.account AND o.calendar_id=c.calendar_id)
             FROM calendars c WHERE account=?1 ORDER BY is_default DESC, name",
        )?;
        let rows = statement.query_map(params![account], |row| {
            Ok(CalendarStatus {
                name: row.get(0)?,
                can_edit: row.get(1)?,
                last_sync: row.get(2)?,
                last_error: row.get(3)?,
                objects: row.get::<_, i64>(4)? as u64,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn calendar_by_slug(&self, account: &str, slug: &str) -> Result<Option<StoredCalendar>> {
        let connection = self.lock();
        Ok(connection
            .query_row(
                "SELECT calendar_id, slug, name, color, can_edit, is_default, modseq
                 FROM calendars WHERE account=?1 AND slug=?2",
                params![account, slug],
                calendar_row,
            )
            .optional()?)
    }

    pub fn set_calendar_sync_result(
        &self,
        account: &str,
        calendar_id: &str,
        error: Option<&str>,
    ) -> Result<()> {
        let connection = self.lock();
        connection.execute(
            "UPDATE calendars SET last_sync=CASE WHEN ?3 IS NULL THEN ?4 ELSE last_sync END,
                                  last_error=?3
             WHERE account=?1 AND calendar_id=?2",
            params![account, calendar_id, error, chrono::Utc::now().timestamp()],
        )?;
        Ok(())
    }

    /// `event id -> (change key, render version)` for a calendar's objects.
    pub fn object_versions(
        &self,
        account: &str,
        calendar_id: &str,
    ) -> Result<HashMap<String, (Option<String>, i64)>> {
        let connection = self.lock();
        let mut statement = connection.prepare(
            "SELECT event_id, change_key, render_version FROM calendar_objects
             WHERE account=?1 AND calendar_id=?2",
        )?;
        let rows = statement.query_map(params![account, calendar_id], |row| {
            Ok((row.get(0)?, (row.get(1)?, row.get(2)?)))
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn objects(&self, account: &str, calendar_id: &str) -> Result<Vec<StoredObject>> {
        let connection = self.lock();
        let mut statement = connection.prepare(&format!(
            "SELECT {OBJECT_COLUMNS} FROM calendar_objects
             WHERE account=?1 AND calendar_id=?2 ORDER BY href"
        ))?;
        let rows = statement.query_map(params![account, calendar_id], object_row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn object(
        &self,
        account: &str,
        calendar_id: &str,
        href: &str,
    ) -> Result<Option<StoredObject>> {
        let connection = self.lock();
        Ok(connection
            .query_row(
                &format!(
                    "SELECT {OBJECT_COLUMNS} FROM calendar_objects
                     WHERE account=?1 AND calendar_id=?2 AND href=?3"
                ),
                params![account, calendar_id, href],
                object_row,
            )
            .optional()?)
    }

    pub fn object_by_event(&self, account: &str, event_id: &str) -> Result<Option<StoredObject>> {
        let connection = self.lock();
        Ok(connection
            .query_row(
                &format!(
                    "SELECT {OBJECT_COLUMNS} FROM calendar_objects
                     WHERE account=?1 AND event_id=?2"
                ),
                params![account, event_id],
                object_row,
            )
            .optional()?)
    }

    /// Insert or replace an object. The calendar's modseq is bumped only
    /// when the served iCalendar text actually changed. Returns whether it
    /// did.
    pub fn put_object(&self, account: &str, object: &ObjectWrite<'_>) -> Result<bool> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        let previous: Option<(String, String, String, i64)> = transaction
            .query_row(
                "SELECT calendar_id, href, ics, modseq FROM calendar_objects
                 WHERE account=?1 AND event_id=?2",
                params![account, object.event_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let moved = previous.as_ref().filter(|(calendar_id, href, _, _)| {
            calendar_id != object.calendar_id || href != object.href
        });
        let changed = previous.is_none()
            || moved.is_some()
            || previous
                .as_ref()
                .is_some_and(|(_, _, ics, _)| ics != object.ics);
        let modseq = match &previous {
            Some((_, _, _, modseq)) if !changed => *modseq,
            _ => bump_modseq(&transaction, account, object.calendar_id)?,
        };
        if let Some((old_calendar, old_href, _, _)) = moved {
            let tombstone_modseq = if old_calendar == object.calendar_id {
                modseq
            } else {
                bump_modseq(&transaction, account, old_calendar)?
            };
            transaction.execute(
                "INSERT OR REPLACE INTO calendar_tombstones (account, calendar_id, href, modseq)
                 VALUES (?1, ?2, ?3, ?4)",
                params![account, old_calendar, old_href, tombstone_modseq],
            )?;
        }
        transaction.execute(
            "DELETE FROM calendar_objects WHERE account=?1 AND event_id=?2",
            params![account, object.event_id],
        )?;
        transaction.execute(
            "INSERT OR REPLACE INTO calendar_objects (account, calendar_id, href, event_id, uid,
                change_key, graph_json, exceptions_json, ics, etag, render_version, modseq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                account,
                object.calendar_id,
                object.href,
                object.event_id,
                object.uid,
                object.change_key,
                object.graph_json,
                object.exceptions_json,
                object.ics,
                etag(object.ics),
                object.render_version,
                modseq
            ],
        )?;
        transaction.execute(
            "DELETE FROM calendar_tombstones WHERE account=?1 AND calendar_id=?2 AND href=?3",
            params![account, object.calendar_id, object.href],
        )?;
        transaction.commit()?;
        Ok(changed)
    }

    /// Remove objects by Graph event id, leaving tombstones behind.
    pub fn remove_objects(
        &self,
        account: &str,
        calendar_id: &str,
        event_ids: &[String],
    ) -> Result<()> {
        if event_ids.is_empty() {
            return Ok(());
        }
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        let modseq = bump_modseq(&transaction, account, calendar_id)?;
        for event_id in event_ids {
            let href: Option<String> = transaction
                .query_row(
                    "SELECT href FROM calendar_objects
                     WHERE account=?1 AND calendar_id=?2 AND event_id=?3",
                    params![account, calendar_id, event_id],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(href) = href else { continue };
            transaction.execute(
                "DELETE FROM calendar_objects WHERE account=?1 AND event_id=?2",
                params![account, event_id],
            )?;
            transaction.execute(
                "INSERT OR REPLACE INTO calendar_tombstones (account, calendar_id, href, modseq)
                 VALUES (?1, ?2, ?3, ?4)",
                params![account, calendar_id, href, modseq],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Objects changed and hrefs removed after `modseq`, for sync-collection.
    pub fn calendar_changes_since(
        &self,
        account: &str,
        calendar_id: &str,
        modseq: i64,
    ) -> Result<(Vec<StoredObject>, Vec<String>)> {
        let connection = self.lock();
        let mut statement = connection.prepare(&format!(
            "SELECT {OBJECT_COLUMNS} FROM calendar_objects
             WHERE account=?1 AND calendar_id=?2 AND modseq>?3 ORDER BY href"
        ))?;
        let changed = statement
            .query_map(params![account, calendar_id, modseq], object_row)?
            .collect::<std::result::Result<_, _>>()?;
        let mut statement = connection.prepare(
            "SELECT href FROM calendar_tombstones
             WHERE account=?1 AND calendar_id=?2 AND modseq>?3 ORDER BY href",
        )?;
        let removed = statement
            .query_map(params![account, calendar_id, modseq], |row| row.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        Ok((changed, removed))
    }
}

fn bump_modseq(transaction: &Transaction<'_>, account: &str, calendar_id: &str) -> Result<i64> {
    Ok(transaction.query_row(
        "UPDATE calendars SET modseq=modseq+1 WHERE account=?1 AND calendar_id=?2
         RETURNING modseq",
        params![account, calendar_id],
        |row| row.get(0),
    )?)
}

fn calendar_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredCalendar> {
    Ok(StoredCalendar {
        id: row.get(0)?,
        slug: row.get(1)?,
        name: row.get(2)?,
        color: row.get(3)?,
        can_edit: row.get(4)?,
        is_default: row.get(5)?,
        modseq: row.get(6)?,
    })
}

fn object_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredObject> {
    Ok(StoredObject {
        calendar_id: row.get(0)?,
        href: row.get(1)?,
        event_id: row.get(2)?,
        uid: row.get(3)?,
        change_key: row.get(4)?,
        graph_json: row.get(5)?,
        exceptions_json: row.get(6)?,
        ics: row.get(7)?,
        etag: row.get(8)?,
        render_version: row.get(9)?,
    })
}

/// A short, stable, URL-safe name for a Graph id.
pub fn slug(id: &str) -> String {
    format!("{:016x}", fnv64(id.as_bytes()))
}

/// A strong entity tag for a served object.
pub fn etag(content: &str) -> String {
    format!("\"{:016x}\"", fnv64(content.as_bytes()))
}

fn fnv64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// `#RRGGBB` or nothing: EDS ignores any other form.
fn normalize_color(color: &str) -> Option<String> {
    let hex = color.strip_prefix('#')?;
    (hex.len() >= 6 && hex[..6].bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| format!("#{}", hex[..6].to_ascii_uppercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("cache.sqlite3")).unwrap();
        (directory, store)
    }

    fn calendar(id: &str) -> Calendar {
        Calendar {
            id: id.into(),
            name: format!("Cal {id}"),
            hex_color: "#058039".into(),
            can_edit: true,
            is_default_calendar: id == "a",
        }
    }

    fn write<'a>(event_id: &'a str, href: &'a str, ics: &'a str) -> ObjectWrite<'a> {
        ObjectWrite {
            calendar_id: "a",
            href,
            event_id,
            uid: event_id,
            change_key: Some("ck"),
            graph_json: "{}",
            exceptions_json: "[]",
            ics,
            render_version: 1,
        }
    }

    #[test]
    fn modseq_tracks_real_changes_and_deletions() {
        let (_directory, store) = store();
        store.replace_calendars("work", &[calendar("a")]).unwrap();
        let slug = store.calendars("work").unwrap()[0].slug.clone();
        assert!(
            store
                .put_object("work", &write("e1", "e1.ics", "v1"))
                .unwrap()
        );
        assert!(
            !store
                .put_object("work", &write("e1", "e1.ics", "v1"))
                .unwrap()
        );
        let calendar = store.calendar_by_slug("work", &slug).unwrap().unwrap();
        assert_eq!(calendar.modseq, 1);
        assert_eq!(calendar.color.as_deref(), Some("#058039"));

        assert!(
            store
                .put_object("work", &write("e2", "e2.ics", "v1"))
                .unwrap()
        );
        store.remove_objects("work", "a", &["e1".into()]).unwrap();
        let (changed, removed) = store.calendar_changes_since("work", "a", 1).unwrap();
        assert_eq!(
            changed.iter().map(|o| o.href.as_str()).collect::<Vec<_>>(),
            ["e2.ics"]
        );
        assert_eq!(removed, ["e1.ics"]);
        assert!(store.object("work", "a", "e1.ics").unwrap().is_none());
        let kept = store.object_by_event("work", "e2").unwrap().unwrap();
        assert_eq!(kept.etag, etag("v1"));
    }

    #[test]
    fn vanished_calendars_take_their_objects_along() {
        let (_directory, store) = store();
        store
            .replace_calendars("work", &[calendar("a"), calendar("b")])
            .unwrap();
        store
            .put_object("work", &write("e1", "e1.ics", "v1"))
            .unwrap();
        store.replace_calendars("work", &[calendar("b")]).unwrap();
        assert_eq!(store.calendars("work").unwrap().len(), 1);
        assert!(store.object_by_event("work", "e1").unwrap().is_none());
    }

    #[test]
    fn colors_are_normalized_for_eds() {
        assert_eq!(normalize_color("#a1b2c3ff").as_deref(), Some("#A1B2C3"));
        assert_eq!(normalize_color(""), None);
        assert_eq!(normalize_color("lightGreen"), None);
    }
}
