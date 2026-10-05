// SPDX-License-Identifier: GPL-2.0-or-later

//! Local SQLite store: stable IMAP UIDs, the synced message index, folder
//! list, per-folder delta-sync state, and a size-capped MIME body cache.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::DateTime;
use mail_parser::MessageParser;
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::graph::{FollowupFlag, MailFolder, MessageSummary, Recipient};

const SCHEMA_VERSION: i64 = 3;

pub struct Store {
    connection: Mutex<Connection>,
}

/// A message row from the local index, ready to be served over IMAP.
#[derive(Clone, Debug)]
pub struct StoredMessage {
    pub uid: u32,
    pub summary: MessageSummary,
}

/// A folder row from the local index.
#[derive(Clone, Debug)]
pub struct StoredFolder {
    pub id: String,
    pub display_name: String,
    pub parent_id: Option<String>,
    pub child_folder_count: u32,
    pub total_item_count: u32,
    pub unread_item_count: u32,
    /// IMAP SPECIAL-USE attribute such as `\Inbox`, when known.
    pub special_use: Option<String>,
}

/// Delta-sync bookkeeping for one folder.
#[derive(Clone, Debug, Default)]
pub struct SyncState {
    /// The `@odata.deltaLink` to start the next incremental round from.
    pub delta_link: Option<String>,
    /// The `@odata.nextLink` of a round still in flight; a restart resumes here.
    pub next_link: Option<String>,
    pub full_sync_done: bool,
    pub last_sync: Option<i64>,
    pub synced_count: u64,
    pub last_error: Option<String>,
}

/// A parameterised SQL fragment produced by the IMAP SEARCH translator.
#[derive(Clone, Debug)]
pub struct SearchSql {
    pub where_clause: String,
    pub params: Vec<Value>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut connection = Connection::open(path)
            .with_context(|| format!("could not open store {}", path.display()))?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;
             PRAGMA busy_timeout=5000;",
        )?;
        migrate(&mut connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.connection.lock().expect("store mutex poisoned")
    }

    // ----- UID allocation ---------------------------------------------------

    pub fn uid_validity(&self, account: &str, mailbox_id: &str) -> Result<u32> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        ensure_mailbox(&transaction, account, mailbox_id)?;
        let value = transaction.query_row(
            "SELECT uid_validity FROM mailboxes WHERE account=?1 AND graph_id=?2",
            params![account, mailbox_id],
            |row| row.get::<_, u32>(0),
        )?;
        transaction.commit()?;
        Ok(value)
    }

    pub fn uid_for(&self, account: &str, mailbox_id: &str, message_id: &str) -> Result<u32> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        let uid = allocate_uid(&transaction, account, mailbox_id, message_id)?;
        transaction.commit()?;
        Ok(uid)
    }

    pub fn uid_next(&self, account: &str, mailbox_id: &str) -> Result<u32> {
        let _ = self.uid_validity(account, mailbox_id)?;
        let connection = self.lock();
        Ok(connection.query_row(
            "SELECT next_uid FROM mailboxes WHERE account=?1 AND graph_id=?2",
            params![account, mailbox_id],
            |row| row.get(0),
        )?)
    }

    // ----- message index ----------------------------------------------------

    /// Insert or update message rows, allocating UIDs for new ones. Fields
    /// absent from a partial payload keep their stored value.
    pub fn upsert_messages(
        &self,
        account: &str,
        mailbox_id: &str,
        batch: &[MessageSummary],
    ) -> Result<Vec<u32>> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        let mut uids = Vec::with_capacity(batch.len());
        for summary in batch {
            let uid = allocate_uid(&transaction, account, mailbox_id, &summary.id)?;
            uids.push(uid);
            let from_name = summary
                .from
                .as_ref()
                .and_then(|recipient| recipient.email_address.name.clone());
            let from_addr = summary
                .from
                .as_ref()
                .and_then(|recipient| recipient.email_address.address.clone());
            let from_text = summary.from.as_ref().map(std::slice::from_ref);
            let from_list = from_text.map(format_recipients);
            let to_list = non_empty(format_recipients(&summary.to_recipients));
            let cc_list = non_empty(format_recipients(&summary.cc_recipients));
            let bcc_list = non_empty(format_recipients(&summary.bcc_recipients));
            let flagged = summary.is_flagged();
            transaction.execute(
                "INSERT INTO messages (
                    account, mailbox_id, message_id, uid, subject, from_name, from_addr,
                    to_list, cc_list, bcc_list, received_raw, sent_raw, received_at, sent_at,
                    is_read, is_draft, flagged, has_attachments, internet_message_id, size,
                    body_preview, subject_lc, from_lc, to_lc, cc_lc, bcc_lc, preview_lc, pinned)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                         ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28)
                 ON CONFLICT(account, mailbox_id, message_id) DO UPDATE SET
                    subject = COALESCE(excluded.subject, subject),
                    from_name = COALESCE(excluded.from_name, from_name),
                    from_addr = COALESCE(excluded.from_addr, from_addr),
                    to_list = COALESCE(excluded.to_list, to_list),
                    cc_list = COALESCE(excluded.cc_list, cc_list),
                    bcc_list = COALESCE(excluded.bcc_list, bcc_list),
                    received_raw = COALESCE(excluded.received_raw, received_raw),
                    sent_raw = COALESCE(excluded.sent_raw, sent_raw),
                    received_at = COALESCE(excluded.received_at, received_at),
                    sent_at = COALESCE(excluded.sent_at, sent_at),
                    is_read = excluded.is_read,
                    is_draft = excluded.is_draft,
                    flagged = excluded.flagged,
                    has_attachments = excluded.has_attachments,
                    internet_message_id = COALESCE(excluded.internet_message_id, internet_message_id),
                    size = COALESCE(excluded.size, size),
                    body_preview = COALESCE(excluded.body_preview, body_preview),
                    subject_lc = COALESCE(excluded.subject_lc, subject_lc),
                    from_lc = COALESCE(excluded.from_lc, from_lc),
                    to_lc = COALESCE(excluded.to_lc, to_lc),
                    cc_lc = COALESCE(excluded.cc_lc, cc_lc),
                    bcc_lc = COALESCE(excluded.bcc_lc, bcc_lc),
                    preview_lc = COALESCE(excluded.preview_lc, preview_lc),
                    pinned = COALESCE(excluded.pinned, pinned)",
                params![
                    account,
                    mailbox_id,
                    summary.id,
                    uid,
                    summary.subject,
                    from_name,
                    from_addr,
                    to_list,
                    cc_list,
                    bcc_list,
                    summary.received_date_time,
                    summary.sent_date_time,
                    summary.received_date_time.as_deref().and_then(epoch_seconds),
                    summary.sent_date_time.as_deref().and_then(epoch_seconds),
                    summary.is_read,
                    summary.is_draft,
                    flagged,
                    summary.has_attachments,
                    summary.internet_message_id,
                    summary.size().map(|size| size as i64),
                    summary.body_preview,
                    summary.subject.as_deref().map(lowercase),
                    from_list.as_deref().map(lowercase),
                    to_list.as_deref().map(lowercase),
                    cc_list.as_deref().map(lowercase),
                    bcc_list.as_deref().map(lowercase),
                    summary.body_preview.as_deref().map(lowercase),
                    summary.pinned(),
                ],
            )?;
        }
        transaction.commit()?;
        Ok(uids)
    }

    /// Remove messages from a folder's index, releasing their UID mapping and
    /// cached body. Returns how many rows were removed.
    pub fn remove_messages(
        &self,
        account: &str,
        mailbox_id: &str,
        ids: &[String],
    ) -> Result<usize> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        let mut removed = 0;
        for id in ids {
            removed += transaction.execute(
                "DELETE FROM messages WHERE account=?1 AND mailbox_id=?2 AND message_id=?3",
                params![account, mailbox_id, id],
            )?;
            transaction.execute(
                "DELETE FROM message_uids WHERE account=?1 AND mailbox_id=?2 AND message_id=?3",
                params![account, mailbox_id, id],
            )?;
            transaction.execute(
                "DELETE FROM body_cache WHERE account=?1 AND message_id=?2",
                params![account, id],
            )?;
        }
        transaction.commit()?;
        Ok(removed)
    }

    /// Every indexed message in a folder, oldest UID first.
    pub fn list_mailbox(&self, account: &str, mailbox_id: &str) -> Result<Vec<StoredMessage>> {
        let connection = self.lock();
        let mut statement = connection.prepare_cached(
            "SELECT message_id, uid, subject, from_name, from_addr, received_raw, sent_raw,
                    is_read, is_draft, flagged, has_attachments, internet_message_id, size,
                    body_preview, pinned
             FROM messages WHERE account=?1 AND mailbox_id=?2 ORDER BY uid",
        )?;
        let rows = statement.query_map(params![account, mailbox_id], |row| {
            let flagged: bool = row.get(9)?;
            let from_name: Option<String> = row.get(3)?;
            let from_addr: Option<String> = row.get(4)?;
            let from = (from_name.is_some() || from_addr.is_some()).then_some(Recipient {
                email_address: crate::graph::EmailAddress {
                    name: from_name,
                    address: from_addr,
                },
            });
            Ok(StoredMessage {
                uid: row.get(1)?,
                summary: MessageSummary {
                    id: row.get(0)?,
                    subject: row.get(2)?,
                    received_date_time: row.get(5)?,
                    sent_date_time: row.get(6)?,
                    is_read: row.get(7)?,
                    has_attachments: row.get(10)?,
                    internet_message_id: row.get(11)?,
                    is_draft: row.get(8)?,
                    flag: Some(FollowupFlag {
                        flag_status: if flagged { "flagged" } else { "notFlagged" }.to_owned(),
                    }),
                    from,
                    to_recipients: Vec::new(),
                    cc_recipients: Vec::new(),
                    bcc_recipients: Vec::new(),
                    body_preview: row.get(13)?,
                    removed: None,
                    stored_size: row.get::<_, Option<i64>>(12)?.map(|size| size as u64),
                    stored_pinned: Some(row.get::<_, Option<bool>>(14)?.unwrap_or(false)),
                    single_value_extended_properties: Vec::new(),
                },
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// How many indexed messages in a folder have no size yet.
    pub fn missing_size_count(&self, account: &str, mailbox_id: &str) -> Result<u64> {
        let connection = self.lock();
        let count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM messages WHERE account=?1 AND mailbox_id=?2 AND size IS NULL",
            params![account, mailbox_id],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as u64)
    }

    /// Fill in sizes for messages that lack one; returns how many rows changed.
    pub fn set_sizes(&self, account: &str, sizes: &[(String, u64)]) -> Result<usize> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        let mut changed = 0;
        for (message_id, size) in sizes {
            changed += transaction.execute(
                "UPDATE messages SET size=?3 WHERE account=?1 AND message_id=?2 AND size IS NULL",
                params![account, message_id, *size as i64],
            )?;
        }
        transaction.commit()?;
        Ok(changed)
    }

    /// `(messages, unseen)` counts for a folder's index.
    pub fn mailbox_counts(&self, account: &str, mailbox_id: &str) -> Result<(u32, u32)> {
        let connection = self.lock();
        Ok(connection.query_row(
            "SELECT COUNT(*), COALESCE(SUM(CASE WHEN is_read THEN 0 ELSE 1 END), 0)
             FROM messages WHERE account=?1 AND mailbox_id=?2",
            params![account, mailbox_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?)
    }

    /// Mirror a flag change the bridge already pushed to Graph so the client
    /// sees it before the next delta round.
    pub fn set_local_flags(
        &self,
        account: &str,
        message_id: &str,
        is_read: Option<bool>,
        flagged: Option<bool>,
        pinned: Option<bool>,
    ) -> Result<()> {
        let connection = self.lock();
        if let Some(value) = is_read {
            connection.execute(
                "UPDATE messages SET is_read=?3 WHERE account=?1 AND message_id=?2",
                params![account, message_id, value],
            )?;
        }
        if let Some(value) = flagged {
            connection.execute(
                "UPDATE messages SET flagged=?3 WHERE account=?1 AND message_id=?2",
                params![account, message_id, value],
            )?;
        }
        if let Some(value) = pinned {
            connection.execute(
                "UPDATE messages SET pinned=?3 WHERE account=?1 AND message_id=?2",
                params![account, message_id, value],
            )?;
        }
        Ok(())
    }

    /// Make `pinned_ids` exactly the pinned messages of a folder. Returns
    /// whether any row changed, so idle sessions are only woken when needed.
    pub fn set_pinned_messages(
        &self,
        account: &str,
        mailbox_id: &str,
        pinned_ids: &[String],
    ) -> Result<bool> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        let mut changed = 0usize;
        {
            let mut pin = transaction.prepare_cached(
                "UPDATE messages SET pinned=1
                 WHERE account=?1 AND mailbox_id=?2 AND message_id=?3 AND COALESCE(pinned, 0)=0",
            )?;
            for message_id in pinned_ids {
                changed += pin.execute(params![account, mailbox_id, message_id])?;
            }
        }
        let keep = if pinned_ids.is_empty() {
            String::new()
        } else {
            let placeholders = (0..pinned_ids.len())
                .map(|index| format!("?{}", index + 3))
                .collect::<Vec<_>>()
                .join(", ");
            format!(" AND message_id NOT IN ({placeholders})")
        };
        let mut values: Vec<Value> = vec![account.to_owned().into(), mailbox_id.to_owned().into()];
        values.extend(pinned_ids.iter().map(|id| Value::from(id.clone())));
        changed += transaction.execute(
            &format!(
                "UPDATE messages SET pinned=0 WHERE account=?1 AND mailbox_id=?2 AND pinned=1{keep}"
            ),
            rusqlite::params_from_iter(values),
        )?;
        transaction.commit()?;
        Ok(changed > 0)
    }

    /// Drop every indexed message of a folder (for a forced resync). UIDs stay
    /// allocated so a re-listed message keeps its number, unless
    /// `bump_validity` is set, which starts the mailbox over.
    pub fn clear_mailbox(
        &self,
        account: &str,
        mailbox_id: &str,
        bump_validity: bool,
    ) -> Result<()> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        transaction.execute(
            "DELETE FROM body_cache WHERE account=?1 AND message_id IN
                (SELECT message_id FROM messages WHERE account=?1 AND mailbox_id=?2)",
            params![account, mailbox_id],
        )?;
        transaction.execute(
            "DELETE FROM messages WHERE account=?1 AND mailbox_id=?2",
            params![account, mailbox_id],
        )?;
        if bump_validity {
            transaction.execute(
                "DELETE FROM message_uids WHERE account=?1 AND mailbox_id=?2",
                params![account, mailbox_id],
            )?;
            transaction.execute(
                "UPDATE mailboxes SET uid_validity=?3, next_uid=1
                 WHERE account=?1 AND graph_id=?2",
                params![account, mailbox_id, new_uid_validity()],
            )?;
        }
        transaction.execute(
            "UPDATE sync_state SET delta_link=NULL, next_link=NULL, full_sync_done=0,
                    synced_count=0 WHERE account=?1 AND folder_id=?2",
            params![account, mailbox_id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Run a translated SEARCH against a folder; returns matching UIDs ascending.
    pub fn search(&self, account: &str, mailbox_id: &str, sql: &SearchSql) -> Result<Vec<u32>> {
        let connection = self.lock();
        let query = format!(
            "SELECT uid FROM messages WHERE account=?1 AND mailbox_id=?2 AND ({}) ORDER BY uid",
            sql.where_clause
        );
        let mut statement = connection.prepare(&query)?;
        let mut values: Vec<Value> = vec![
            Value::from(account.to_owned()),
            Value::from(mailbox_id.to_owned()),
        ];
        values.extend(sql.params.iter().cloned());
        let rows = statement.query_map(rusqlite::params_from_iter(values), |row| {
            row.get::<_, u32>(0)
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    // ----- sync state -------------------------------------------------------

    pub fn sync_state(&self, account: &str, folder_id: &str) -> Result<SyncState> {
        let connection = self.lock();
        Ok(connection
            .query_row(
                "SELECT delta_link, next_link, full_sync_done, last_sync, synced_count, last_error
                 FROM sync_state WHERE account=?1 AND folder_id=?2",
                params![account, folder_id],
                |row| {
                    Ok(SyncState {
                        delta_link: row.get(0)?,
                        next_link: row.get(1)?,
                        full_sync_done: row.get(2)?,
                        last_sync: row.get(3)?,
                        synced_count: row.get::<_, i64>(4)? as u64,
                        last_error: row.get(5)?,
                    })
                },
            )
            .optional()?
            .unwrap_or_default())
    }

    pub fn save_sync_state(&self, account: &str, folder_id: &str, state: &SyncState) -> Result<()> {
        let connection = self.lock();
        connection.execute(
            "INSERT INTO sync_state (account, folder_id, delta_link, next_link, full_sync_done,
                                     last_sync, synced_count, last_error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(account, folder_id) DO UPDATE SET
                delta_link=excluded.delta_link, next_link=excluded.next_link,
                full_sync_done=excluded.full_sync_done, last_sync=excluded.last_sync,
                synced_count=excluded.synced_count, last_error=excluded.last_error",
            params![
                account,
                folder_id,
                state.delta_link,
                state.next_link,
                state.full_sync_done,
                state.last_sync,
                state.synced_count as i64,
                state.last_error,
            ],
        )?;
        Ok(())
    }

    /// `(folder, state)` for every folder of an account, for status output.
    pub fn all_sync_states(&self, account: &str) -> Result<Vec<(StoredFolder, SyncState)>> {
        let folders = self.folders(account)?;
        let mut out = Vec::with_capacity(folders.len());
        for folder in folders {
            let state = self.sync_state(account, &folder.id)?;
            out.push((folder, state));
        }
        Ok(out)
    }

    // ----- folders ----------------------------------------------------------

    /// Replace the account's folder list with `folders`. Folders that vanished
    /// have their index and sync state dropped.
    pub fn replace_folders(&self, account: &str, folders: &[StoredFolder]) -> Result<Vec<String>> {
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        let existing: Vec<String> = {
            let mut statement =
                transaction.prepare("SELECT folder_id FROM folders WHERE account=?1")?;
            let rows = statement.query_map(params![account], |row| row.get::<_, String>(0))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        let mut vanished = Vec::new();
        for id in existing {
            if !folders.iter().any(|folder| folder.id == id) {
                transaction.execute(
                    "DELETE FROM body_cache WHERE account=?1 AND message_id IN
                        (SELECT message_id FROM messages WHERE account=?1 AND mailbox_id=?2)",
                    params![account, id],
                )?;
                transaction.execute(
                    "DELETE FROM messages WHERE account=?1 AND mailbox_id=?2",
                    params![account, id],
                )?;
                transaction.execute(
                    "DELETE FROM sync_state WHERE account=?1 AND folder_id=?2",
                    params![account, id],
                )?;
                transaction.execute(
                    "DELETE FROM folders WHERE account=?1 AND folder_id=?2",
                    params![account, id],
                )?;
                vanished.push(id);
            }
        }
        for folder in folders {
            transaction.execute(
                "INSERT INTO folders (account, folder_id, display_name, parent_id,
                                      child_folder_count, total_item_count, unread_item_count,
                                      special_use)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(account, folder_id) DO UPDATE SET
                    display_name=excluded.display_name, parent_id=excluded.parent_id,
                    child_folder_count=excluded.child_folder_count,
                    total_item_count=excluded.total_item_count,
                    unread_item_count=excluded.unread_item_count,
                    special_use=excluded.special_use",
                params![
                    account,
                    folder.id,
                    folder.display_name,
                    folder.parent_id,
                    folder.child_folder_count,
                    folder.total_item_count,
                    folder.unread_item_count,
                    folder.special_use,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(vanished)
    }

    pub fn folders(&self, account: &str) -> Result<Vec<StoredFolder>> {
        let connection = self.lock();
        let mut statement = connection.prepare_cached(
            "SELECT folder_id, display_name, parent_id, child_folder_count, total_item_count,
                    unread_item_count, special_use
             FROM folders WHERE account=?1",
        )?;
        let rows = statement.query_map(params![account], |row| {
            Ok(StoredFolder {
                id: row.get(0)?,
                display_name: row.get(1)?,
                parent_id: row.get(2)?,
                child_folder_count: row.get(3)?,
                total_item_count: row.get(4)?,
                unread_item_count: row.get(5)?,
                special_use: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    // ----- body cache -------------------------------------------------------

    pub fn body_get(&self, account: &str, message_id: &str) -> Result<Option<Vec<u8>>> {
        let connection = self.lock();
        let body: Option<Vec<u8>> = connection
            .query_row(
                "SELECT mime FROM body_cache WHERE account=?1 AND message_id=?2",
                params![account, message_id],
                |row| row.get(0),
            )
            .optional()?;
        if body.is_some() {
            connection.execute(
                "UPDATE body_cache SET last_access=?3 WHERE account=?1 AND message_id=?2",
                params![account, message_id, now()],
            )?;
        }
        Ok(body)
    }

    pub fn body_cached(&self, account: &str, message_id: &str) -> Result<bool> {
        let connection = self.lock();
        Ok(connection
            .query_row(
                "SELECT 1 FROM body_cache WHERE account=?1 AND message_id=?2",
                params![account, message_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Cache a message's MIME, then evict least-recently-used bodies until the
    /// cache fits in `max_bytes`. Bodies larger than the cap are not stored.
    pub fn body_put(
        &self,
        account: &str,
        message_id: &str,
        mime: &[u8],
        max_bytes: u64,
    ) -> Result<()> {
        if mime.len() as u64 > max_bytes {
            return Ok(());
        }
        let body_text = extract_body_text(mime);
        let mut connection = self.lock();
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO body_cache (account, message_id, mime, body_text, size, last_access)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(account, message_id) DO UPDATE SET
                mime=excluded.mime, body_text=excluded.body_text, size=excluded.size,
                last_access=excluded.last_access",
            params![
                account,
                message_id,
                mime,
                body_text,
                mime.len() as i64,
                now()
            ],
        )?;
        let total: i64 =
            transaction.query_row("SELECT COALESCE(SUM(size), 0) FROM body_cache", [], |row| {
                row.get(0)
            })?;
        let mut excess = (total.max(0) as u64).saturating_sub(max_bytes);
        if excess > 0 {
            // Walk least-recently-used rows and drop just enough to fit.
            let victims: Vec<(i64, i64)> = {
                let mut statement = transaction.prepare(
                    "SELECT rowid, size FROM body_cache WHERE NOT (account=?1 AND message_id=?2)
                     ORDER BY last_access ASC",
                )?;
                let rows = statement.query_map(params![account, message_id], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?;
                rows.collect::<std::result::Result<Vec<_>, _>>()?
            };
            for (rowid, size) in victims {
                if excess == 0 {
                    break;
                }
                transaction.execute("DELETE FROM body_cache WHERE rowid=?1", params![rowid])?;
                excess = excess.saturating_sub(size.max(0) as u64);
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// `(bytes, rows)` currently held by the body cache.
    pub fn body_cache_stats(&self) -> Result<(u64, u64)> {
        let connection = self.lock();
        let (bytes, rows): (i64, i64) = connection.query_row(
            "SELECT COALESCE(SUM(size), 0), COUNT(*) FROM body_cache",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok((bytes.max(0) as u64, rows.max(0) as u64))
    }

    /// Message ids in a folder without a cached body, newest first.
    pub fn uncached_message_ids(
        &self,
        account: &str,
        mailbox_id: &str,
        limit: usize,
    ) -> Result<Vec<String>> {
        let connection = self.lock();
        let mut statement = connection.prepare_cached(
            "SELECT m.message_id FROM messages m
             LEFT JOIN body_cache b ON b.account = m.account AND b.message_id = m.message_id
             WHERE m.account=?1 AND m.mailbox_id=?2 AND b.message_id IS NULL
             ORDER BY m.received_at DESC LIMIT ?3",
        )?;
        let rows = statement.query_map(params![account, mailbox_id, limit as i64], |row| {
            row.get::<_, String>(0)
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Reclaim file space after large evictions.
    pub fn vacuum_incremental(&self) -> Result<()> {
        let connection = self.lock();
        connection.execute_batch("PRAGMA incremental_vacuum;")?;
        Ok(())
    }
}

fn migrate(connection: &mut Connection) -> Result<()> {
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version >= SCHEMA_VERSION {
        return Ok(());
    }
    // auto_vacuum must be chosen before any table exists to take effect on a
    // fresh file; on an existing file it is a no-op until VACUUM, which is fine.
    connection.execute_batch("PRAGMA auto_vacuum=INCREMENTAL;")?;
    let transaction = connection.transaction()?;
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS mailboxes (
             account TEXT NOT NULL,
             graph_id TEXT NOT NULL,
             uid_validity INTEGER NOT NULL,
             next_uid INTEGER NOT NULL DEFAULT 1,
             PRIMARY KEY (account, graph_id)
         );
         CREATE TABLE IF NOT EXISTS message_uids (
             account TEXT NOT NULL,
             mailbox_id TEXT NOT NULL,
             message_id TEXT NOT NULL,
             uid INTEGER NOT NULL,
             PRIMARY KEY (account, mailbox_id, message_id),
             UNIQUE (account, mailbox_id, uid),
             FOREIGN KEY (account, mailbox_id)
               REFERENCES mailboxes(account, graph_id) ON DELETE CASCADE
         );
         CREATE TABLE IF NOT EXISTS messages (
             account TEXT NOT NULL,
             mailbox_id TEXT NOT NULL,
             message_id TEXT NOT NULL,
             uid INTEGER NOT NULL,
             subject TEXT, from_name TEXT, from_addr TEXT,
             to_list TEXT, cc_list TEXT, bcc_list TEXT,
             received_raw TEXT, sent_raw TEXT,
             received_at INTEGER, sent_at INTEGER,
             is_read INTEGER NOT NULL DEFAULT 0,
             is_draft INTEGER NOT NULL DEFAULT 0,
             flagged INTEGER NOT NULL DEFAULT 0,
             has_attachments INTEGER NOT NULL DEFAULT 0,
             internet_message_id TEXT,
             size INTEGER,
             pinned INTEGER,
             body_preview TEXT,
             subject_lc TEXT, from_lc TEXT, to_lc TEXT, cc_lc TEXT, bcc_lc TEXT, preview_lc TEXT,
             PRIMARY KEY (account, mailbox_id, message_id)
         );
         CREATE INDEX IF NOT EXISTS messages_order ON messages(account, mailbox_id, uid);
         CREATE INDEX IF NOT EXISTS messages_received ON messages(account, mailbox_id, received_at);
         CREATE TABLE IF NOT EXISTS folders (
             account TEXT NOT NULL,
             folder_id TEXT NOT NULL,
             display_name TEXT NOT NULL,
             parent_id TEXT,
             child_folder_count INTEGER NOT NULL DEFAULT 0,
             total_item_count INTEGER NOT NULL DEFAULT 0,
             unread_item_count INTEGER NOT NULL DEFAULT 0,
             special_use TEXT,
             PRIMARY KEY (account, folder_id)
         );
         CREATE TABLE IF NOT EXISTS sync_state (
             account TEXT NOT NULL,
             folder_id TEXT NOT NULL,
             delta_link TEXT,
             next_link TEXT,
             full_sync_done INTEGER NOT NULL DEFAULT 0,
             last_sync INTEGER,
             synced_count INTEGER NOT NULL DEFAULT 0,
             last_error TEXT,
             PRIMARY KEY (account, folder_id)
         );
         CREATE TABLE IF NOT EXISTS body_cache (
             account TEXT NOT NULL,
             message_id TEXT NOT NULL,
             mime BLOB NOT NULL,
             body_text TEXT,
             size INTEGER NOT NULL,
             last_access INTEGER NOT NULL,
             PRIMARY KEY (account, message_id)
         );
         CREATE INDEX IF NOT EXISTS body_cache_lru ON body_cache(last_access);
         CREATE TABLE IF NOT EXISTS calendars (
             account TEXT NOT NULL,
             calendar_id TEXT NOT NULL,
             slug TEXT NOT NULL,
             name TEXT NOT NULL,
             color TEXT,
             can_edit INTEGER NOT NULL DEFAULT 0,
             is_default INTEGER NOT NULL DEFAULT 0,
             modseq INTEGER NOT NULL DEFAULT 0,
             last_sync INTEGER,
             last_error TEXT,
             PRIMARY KEY (account, calendar_id),
             UNIQUE (account, slug)
         );
         CREATE TABLE IF NOT EXISTS calendar_objects (
             account TEXT NOT NULL,
             calendar_id TEXT NOT NULL,
             href TEXT NOT NULL,
             event_id TEXT NOT NULL,
             uid TEXT NOT NULL,
             change_key TEXT,
             graph_json TEXT NOT NULL,
             exceptions_json TEXT NOT NULL,
             ics TEXT NOT NULL,
             etag TEXT NOT NULL,
             render_version INTEGER NOT NULL,
             modseq INTEGER NOT NULL,
             PRIMARY KEY (account, calendar_id, href),
             UNIQUE (account, event_id),
             FOREIGN KEY (account, calendar_id)
               REFERENCES calendars(account, calendar_id) ON DELETE CASCADE
         );
         CREATE TABLE IF NOT EXISTS calendar_tombstones (
             account TEXT NOT NULL,
             calendar_id TEXT NOT NULL,
             href TEXT NOT NULL,
             modseq INTEGER NOT NULL,
             PRIMARY KEY (account, calendar_id, href),
             FOREIGN KEY (account, calendar_id)
               REFERENCES calendars(account, calendar_id) ON DELETE CASCADE
         );",
    )?;
    if version == 1 {
        // v2: Outlook pin state; NULL until the first pin refresh reads it.
        transaction.execute_batch("ALTER TABLE messages ADD COLUMN pinned INTEGER;")?;
    }
    // v3 only adds the calendar tables, which the batch above creates.
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

fn ensure_mailbox(transaction: &Transaction<'_>, account: &str, mailbox_id: &str) -> Result<()> {
    transaction.execute(
        "INSERT OR IGNORE INTO mailboxes(account, graph_id, uid_validity)
         VALUES (?1, ?2, ?3)",
        params![account, mailbox_id, new_uid_validity()],
    )?;
    Ok(())
}

fn allocate_uid(
    transaction: &Transaction<'_>,
    account: &str,
    mailbox_id: &str,
    message_id: &str,
) -> Result<u32> {
    ensure_mailbox(transaction, account, mailbox_id)?;
    if let Some(uid) = transaction
        .query_row(
            "SELECT uid FROM message_uids
             WHERE account=?1 AND mailbox_id=?2 AND message_id=?3",
            params![account, mailbox_id, message_id],
            |row| row.get::<_, u32>(0),
        )
        .optional()?
    {
        return Ok(uid);
    }
    let uid = transaction.query_row(
        "SELECT next_uid FROM mailboxes WHERE account=?1 AND graph_id=?2",
        params![account, mailbox_id],
        |row| row.get::<_, u32>(0),
    )?;
    transaction.execute(
        "INSERT INTO message_uids(account, mailbox_id, message_id, uid)
         VALUES (?1, ?2, ?3, ?4)",
        params![account, mailbox_id, message_id, uid],
    )?;
    transaction.execute(
        "UPDATE mailboxes SET next_uid=?3 WHERE account=?1 AND graph_id=?2",
        params![account, mailbox_id, uid.saturating_add(1)],
    )?;
    Ok(uid)
}

fn new_uid_validity() -> u32 {
    u32::try_from(now().max(1)).unwrap_or(u32::MAX)
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn epoch_seconds(rfc3339: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(rfc3339)
        .ok()
        .map(|value| value.timestamp())
}

fn lowercase(value: &str) -> String {
    value.to_lowercase()
}

fn non_empty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

/// `Name <addr>, Name <addr>` — the shape IMAP clients search against.
pub fn format_recipients(recipients: &[Recipient]) -> String {
    recipients
        .iter()
        .map(|recipient| {
            let name = recipient.email_address.name.as_deref().unwrap_or_default();
            let address = recipient
                .email_address
                .address
                .as_deref()
                .unwrap_or_default();
            match (name.is_empty(), address.is_empty()) {
                (true, _) => address.to_owned(),
                (false, true) => name.to_owned(),
                (false, false) => format!("{name} <{address}>"),
            }
        })
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Decoded, lower-cased text of every text part, for BODY/TEXT searches.
fn extract_body_text(mime: &[u8]) -> Option<String> {
    let message = MessageParser::default().parse(mime)?;
    let mut text = String::new();
    let mut index = 0;
    while let Some(part) = message.body_text(index) {
        text.push_str(&part);
        text.push('\n');
        index += 1;
    }
    if text.trim().is_empty() {
        // HTML-only messages: strip tags crudely so words remain searchable.
        if let Some(html) = message.body_html(0) {
            text = strip_tags(&html);
        }
    }
    let text = text.to_lowercase();
    (!text.trim().is_empty()).then_some(text)
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for character in html.chars() {
        match character {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(character),
            _ => {}
        }
    }
    out
}

impl From<&MailFolder> for StoredFolder {
    fn from(folder: &MailFolder) -> Self {
        Self {
            id: folder.id.clone(),
            display_name: folder.display_name.clone(),
            parent_id: folder.parent_folder_id.clone(),
            child_folder_count: folder.child_folder_count,
            total_item_count: folder.total_item_count,
            unread_item_count: folder.unread_item_count,
            special_use: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let temporary = tempfile::tempdir().unwrap();
        let store = Store::open(&temporary.path().join("cache.sqlite3")).unwrap();
        (temporary, store)
    }

    fn summary(json: &str) -> MessageSummary {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn uids_are_stable_and_scoped_to_mailbox() {
        let (_temporary, store) = store();
        let one = store.uid_for("work", "inbox", "message-a").unwrap();
        assert_eq!(store.uid_for("work", "inbox", "message-a").unwrap(), one);
        assert_eq!(
            store.uid_for("work", "inbox", "message-b").unwrap(),
            one + 1
        );
        assert_eq!(store.uid_for("work", "archive", "message-a").unwrap(), 1);
        assert!(store.uid_validity("work", "inbox").unwrap() > 0);
    }

    #[test]
    fn migrates_a_v0_uid_cache_in_place() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("cache.sqlite3");
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE mailboxes (
                         account TEXT NOT NULL, graph_id TEXT NOT NULL,
                         uid_validity INTEGER NOT NULL, next_uid INTEGER NOT NULL DEFAULT 1,
                         PRIMARY KEY (account, graph_id));
                     CREATE TABLE message_uids (
                         account TEXT NOT NULL, mailbox_id TEXT NOT NULL,
                         message_id TEXT NOT NULL, uid INTEGER NOT NULL,
                         PRIMARY KEY (account, mailbox_id, message_id),
                         UNIQUE (account, mailbox_id, uid),
                         FOREIGN KEY (account, mailbox_id)
                           REFERENCES mailboxes(account, graph_id) ON DELETE CASCADE);
                     INSERT INTO mailboxes VALUES ('work', 'inbox', 1700000000, 42);
                     INSERT INTO message_uids VALUES ('work', 'inbox', 'old-message', 41);",
                )
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.uid_validity("work", "inbox").unwrap(), 1_700_000_000);
        assert_eq!(store.uid_for("work", "inbox", "old-message").unwrap(), 41);
        assert_eq!(store.uid_for("work", "inbox", "new-message").unwrap(), 42);
        // Synced rows adopt the UIDs the old cache had handed out.
        let uids = store
            .upsert_messages("work", "inbox", &[summary(r#"{"id":"old-message"}"#)])
            .unwrap();
        assert_eq!(uids, vec![41]);
        assert!(
            store.list_mailbox("work", "inbox").unwrap()[0]
                .summary
                .flag
                .is_some()
        );
    }

    #[test]
    fn pin_state_survives_partial_updates_and_follows_the_pinned_set() {
        let (_temporary, store) = store();
        let messages = [
            summary(r#"{"id":"a","subject":"A"}"#),
            summary(r#"{"id":"b","subject":"B"}"#),
            summary(
                r#"{"id":"c","subject":"C","singleValueExtendedProperties":[
                    {"id":"SystemTime 0xf02","value":"4500-09-01T00:00:00Z"}]}"#,
            ),
        ];
        store.upsert_messages("work", "inbox", &messages).unwrap();
        let pinned = |store: &Store| -> Vec<String> {
            store
                .list_mailbox("work", "inbox")
                .unwrap()
                .into_iter()
                .filter(|row| row.summary.is_pinned())
                .map(|row| row.summary.id)
                .collect()
        };
        assert_eq!(pinned(&store), vec!["c"]);

        assert!(
            store
                .set_pinned_messages("work", "inbox", &["a".to_owned()])
                .unwrap()
        );
        assert_eq!(pinned(&store), vec!["a"]);
        assert!(
            !store
                .set_pinned_messages("work", "inbox", &["a".to_owned()])
                .unwrap(),
            "no change reported when the set already matches"
        );

        // A delta entry without the property (marked read) keeps the pin.
        store
            .upsert_messages("work", "inbox", &[summary(r#"{"id":"a","isRead":true}"#)])
            .unwrap();
        assert_eq!(pinned(&store), vec!["a"]);

        store
            .set_local_flags("work", "b", None, None, Some(true))
            .unwrap();
        assert_eq!(pinned(&store), vec!["a", "b"]);
        assert!(store.set_pinned_messages("work", "inbox", &[]).unwrap());
        assert!(pinned(&store).is_empty());
    }

    #[test]
    fn upsert_is_idempotent_and_keeps_fields_on_partial_updates() {
        let (_temporary, store) = store();
        let full = summary(
            r#"{"id":"a","subject":"Hello World","isRead":false,
                "from":{"emailAddress":{"name":"Bob","address":"bob@example.com"}},
                "toRecipients":[{"emailAddress":{"address":"me@example.com"}}],
                "receivedDateTime":"2020-01-02T03:04:05Z","bodyPreview":"Preview"}"#,
        );
        let uids = store
            .upsert_messages("work", "inbox", std::slice::from_ref(&full))
            .unwrap();
        assert_eq!(uids, vec![1]);
        let again = store.upsert_messages("work", "inbox", &[full]).unwrap();
        assert_eq!(again, vec![1]);

        let partial = summary(r#"{"id":"a","isRead":true}"#);
        store.upsert_messages("work", "inbox", &[partial]).unwrap();
        let rows = store.list_mailbox("work", "inbox").unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].summary.is_read);
        assert_eq!(rows[0].summary.subject.as_deref(), Some("Hello World"));
        assert_eq!(
            rows[0].summary.received_date_time.as_deref(),
            Some("2020-01-02T03:04:05Z")
        );
        assert_eq!(store.mailbox_counts("work", "inbox").unwrap(), (1, 0));
        assert_eq!(store.missing_size_count("work", "inbox").unwrap(), 1);
        assert_eq!(
            store.set_sizes("work", &[("a".to_owned(), 1234)]).unwrap(),
            1
        );
        assert_eq!(store.set_sizes("work", &[("a".to_owned(), 1)]).unwrap(), 0);
        assert_eq!(
            store.list_mailbox("work", "inbox").unwrap()[0]
                .summary
                .size(),
            Some(1234)
        );
        assert_eq!(store.missing_size_count("work", "inbox").unwrap(), 0);
    }

    #[test]
    fn removal_releases_uid_and_body() {
        let (_temporary, store) = store();
        store
            .upsert_messages(
                "work",
                "inbox",
                &[summary(r#"{"id":"a"}"#), summary(r#"{"id":"b"}"#)],
            )
            .unwrap();
        store
            .body_put("work", "a", b"Subject: x\r\n\r\nbody", 1 << 20)
            .unwrap();
        assert!(store.body_cached("work", "a").unwrap());
        assert_eq!(
            store
                .remove_messages("work", "inbox", &["a".to_owned()])
                .unwrap(),
            1
        );
        assert!(!store.body_cached("work", "a").unwrap());
        assert_eq!(store.list_mailbox("work", "inbox").unwrap().len(), 1);
        // A re-appearing message gets a fresh UID, as IMAP requires.
        assert_eq!(store.uid_for("work", "inbox", "a").unwrap(), 3);
    }

    #[test]
    fn body_cache_evicts_least_recently_used() {
        let (_temporary, store) = store();
        let body = vec![b'x'; 1000];
        for id in ["a", "b", "c"] {
            store.body_put("work", id, &body, 2500).unwrap();
        }
        let (bytes, rows) = store.body_cache_stats().unwrap();
        assert!(rows <= 2, "rows={rows}");
        assert!(bytes <= 2500);
        assert!(!store.body_cached("work", "a").unwrap());
        assert!(store.body_cached("work", "c").unwrap());
        // Oversized bodies are skipped rather than thrashing the cache.
        store.body_put("work", "big", &vec![0; 5000], 2500).unwrap();
        assert!(!store.body_cached("work", "big").unwrap());
    }

    #[test]
    fn body_text_is_decoded_for_search() {
        let mime = b"Subject: t\r\nContent-Type: text/plain\r\nContent-Transfer-Encoding: base64\r\n\r\nSGVsbG8gU2VhcmNoYWJsZSBXb3JsZA==\r\n";
        assert_eq!(
            extract_body_text(mime).as_deref(),
            Some("hello searchable world\n")
        );
    }

    #[test]
    fn folder_replacement_drops_vanished_folders() {
        let (_temporary, store) = store();
        let a = StoredFolder {
            id: "a".into(),
            display_name: "A".into(),
            parent_id: None,
            child_folder_count: 0,
            total_item_count: 0,
            unread_item_count: 0,
            special_use: None,
        };
        let mut b = a.clone();
        b.id = "b".into();
        store.replace_folders("work", &[a.clone(), b]).unwrap();
        store
            .upsert_messages("work", "b", &[summary(r#"{"id":"m"}"#)])
            .unwrap();
        let vanished = store.replace_folders("work", &[a]).unwrap();
        assert_eq!(vanished, vec!["b".to_owned()]);
        assert!(store.list_mailbox("work", "b").unwrap().is_empty());
        assert_eq!(store.folders("work").unwrap().len(), 1);
    }

    #[test]
    fn sync_state_round_trips() {
        let (_temporary, store) = store();
        assert!(!store.sync_state("work", "inbox").unwrap().full_sync_done);
        let state = SyncState {
            delta_link: Some("https://delta".into()),
            next_link: None,
            full_sync_done: true,
            last_sync: Some(42),
            synced_count: 7,
            last_error: None,
        };
        store.save_sync_state("work", "inbox", &state).unwrap();
        let loaded = store.sync_state("work", "inbox").unwrap();
        assert_eq!(loaded.delta_link.as_deref(), Some("https://delta"));
        assert!(loaded.full_sync_done);
        assert_eq!(loaded.synced_count, 7);
        store.clear_mailbox("work", "inbox", false).unwrap();
        assert!(!store.sync_state("work", "inbox").unwrap().full_sync_done);
    }
}
