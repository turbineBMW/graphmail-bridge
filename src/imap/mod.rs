// SPDX-License-Identifier: GPL-2.0-or-later

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use mail_parser::{Address, Message, MessageParser, MessagePart, MimeHeaders};
use mailrs_imap_proto::{
    ImapCommand, ParseError, SequenceSet, TaggedCommand, format_bad, format_bye, format_capability,
    format_exists, format_flags, format_list, format_no, format_ok, format_recent, parse_command,
    parse_sequence_set, sequence_set_to_uids,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::net::{TcpListener, TcpStream};

use crate::graph::MessageSummary;
use crate::search::{self, SessionView};
use crate::service::{AccountRuntime, Runtime};
use crate::smtp::read_bounded_line;
use crate::store::StoredFolder;
use crate::sync::fetch_folder_tree;

const CAPABILITIES: &[&str] = &[
    "IMAP4rev1",
    "IDLE",
    "MOVE",
    "NAMESPACE",
    "SPECIAL-USE",
    "ENABLE",
    "UNSELECT",
    "UTF8=ACCEPT",
];
/// Extensions a client may switch on with ENABLE.
const ENABLEABLE: &[&str] = &["UTF8=ACCEPT"];
const MAX_APPEND_BYTES: u32 = 35 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 64 * 1024;
/// Safety-net poll while idling; the sync task normally wakes sessions sooner.
const IDLE_POLL_INTERVAL: Duration = Duration::from_secs(60);
/// Outlook's pin-to-top, exposed as an IMAP keyword. Set from Outlook it
/// arrives through the sync task's pin refresh; set here it is written back
/// to Graph like `\\Flagged`.
pub const PINNED_KEYWORD: &str = "$Pinned";
const PERMANENT_FLAGS: &[&str] = &["\\Seen", "\\Deleted", "\\Flagged", PINNED_KEYWORD];

#[derive(Clone, Debug)]
struct Mailbox {
    id: String,
    name: String,
    child_count: u32,
    total: u32,
    unread: u32,
    special_use: Option<&'static str>,
}

#[derive(Clone, Debug)]
struct SelectedMessage {
    summary: MessageSummary,
    uid: u32,
    deleted: bool,
}

#[derive(Clone, Debug)]
struct SelectedMailbox {
    id: String,
    messages: Vec<SelectedMessage>,
    read_only: bool,
    /// The list came from the local index. Until a folder's first sync has
    /// produced rows, SELECT serves a bounded live listing from Graph instead.
    from_store: bool,
}

#[derive(Default)]
struct Session {
    account: Option<Arc<AccountRuntime>>,
    folders: Vec<Mailbox>,
    selected: Option<SelectedMailbox>,
    /// The client enabled UTF8=ACCEPT, so mailbox names travel as raw UTF-8
    /// instead of modified UTF-7.
    utf8: bool,
}

impl Session {
    fn decode_mailbox_name(&self, wire: &str) -> String {
        if self.utf8 {
            wire.to_owned()
        } else {
            utf7::decode(wire)
        }
    }

    fn encode_mailbox_name(&self, name: &str) -> String {
        if self.utf8 {
            name.to_owned()
        } else {
            utf7::encode(name)
        }
    }
}

pub async fn serve(listener: TcpListener, runtime: Arc<Runtime>) -> Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let runtime = runtime.clone();
        tokio::spawn(async move {
            if let Err(error) = handle(stream, runtime).await {
                tracing::warn!(%peer, %error, "IMAP connection closed with an error");
            }
        });
    }
}

async fn handle(stream: TcpStream, runtime: Arc<Runtime>) -> Result<()> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut session = Session::default();
    writer
        .write_all(
            format!(
                "* OK [CAPABILITY {}] graphmail-bridge ready\r\n",
                CAPABILITIES.join(" ")
            )
            .as_bytes(),
        )
        .await?;

    loop {
        let Some(line) = read_command_line(&mut reader).await? else {
            return Ok(());
        };
        if let Some(tag) = id_command_tag(&line) {
            writer
                .write_all(b"* ID (\"name\" \"graphmail-bridge\" \"version\" \"0.1.0\")\r\n")
                .await?;
            tagged_ok(&mut writer, tag, "ID completed").await?;
            continue;
        }
        let parsed = match parse_command_compat(&line) {
            Ok(command) => command,
            Err(error) => {
                let tag = line.split_whitespace().next().unwrap_or("*");
                writer
                    .write_all(format_bad(tag, &error.to_string()).as_bytes())
                    .await?;
                continue;
            }
        };
        let tag = parsed.tag.clone();
        let should_close = match dispatch(
            parsed.command,
            &tag,
            &mut session,
            &runtime,
            &mut reader,
            &mut writer,
        )
        .await
        {
            Ok(close) => close,
            Err(error) => {
                tracing::warn!(%error, command_tag = %tag, "IMAP command failed");
                writer
                    .write_all(format_no(&tag, &sanitize_error(&error)).as_bytes())
                    .await?;
                false
            }
        };
        if should_close {
            return Ok(());
        }
    }
}

async fn dispatch(
    command: ImapCommand,
    tag: &str,
    session: &mut Session,
    runtime: &Arc<Runtime>,
    reader: &mut BufReader<ReadHalf<TcpStream>>,
    writer: &mut WriteHalf<TcpStream>,
) -> Result<bool> {
    match command {
        ImapCommand::Capability => {
            writer
                .write_all(format_capability(CAPABILITIES).as_bytes())
                .await?;
            tagged_ok(writer, tag, "CAPABILITY completed").await?;
        }
        ImapCommand::Login { username, password } => {
            if let Some(account) = runtime.authenticate(&username, &password).await {
                session.folders = load_folders(&account, runtime).await?;
                session.account = Some(account);
                tagged_ok(writer, tag, "LOGIN completed").await?;
            } else {
                writer
                    .write_all(format_no(tag, "Authentication failed").as_bytes())
                    .await?;
            }
        }
        ImapCommand::Logout => {
            writer
                .write_all(format_bye("Logging out").as_bytes())
                .await?;
            tagged_ok(writer, tag, "LOGOUT completed").await?;
            return Ok(true);
        }
        ImapCommand::List { reference, pattern } | ImapCommand::Lsub { reference, pattern } => {
            let account = require_account(session)?.clone();
            session.folders = load_folders(&account, runtime).await?;
            let pattern = format!(
                "{}{}",
                session.decode_mailbox_name(&reference),
                session.decode_mailbox_name(&pattern)
            );
            for mailbox in &session.folders {
                if !mailbox_pattern_matches(&mailbox.name, &pattern) {
                    continue;
                }
                let children = if mailbox.child_count > 0 {
                    "\\HasChildren"
                } else {
                    "\\HasNoChildren"
                };
                let special = mailbox
                    .special_use
                    .map(|flag| format!(" {flag}"))
                    .unwrap_or_default();
                let flags = format!("{children}{special}");
                let wire_name = session.encode_mailbox_name(&mailbox.name);
                writer
                    .write_all(format_list(&flags, "/", &imap_quote(&wire_name)).as_bytes())
                    .await?;
            }
            tagged_ok(writer, tag, "LIST completed").await?;
        }
        ImapCommand::Select { mailbox } => {
            let name = session.decode_mailbox_name(&mailbox);
            select_mailbox(session, runtime, &name, false, writer).await?;
            tagged_ok(writer, tag, "[READ-WRITE] SELECT completed").await?;
        }
        ImapCommand::Examine { mailbox } => {
            let name = session.decode_mailbox_name(&mailbox);
            select_mailbox(session, runtime, &name, true, writer).await?;
            tagged_ok(writer, tag, "[READ-ONLY] EXAMINE completed").await?;
        }
        ImapCommand::Status { mailbox, .. } => {
            let account = require_account(session)?.clone();
            let name = session.decode_mailbox_name(&mailbox);
            let cached = find_mailbox(&session.folders, &name)?.clone();
            let account_name = account.config.name.as_str();
            // Fully synced folders answer from the local index; otherwise
            // ask Graph, since the stored counts go stale quickly.
            let (total, unread) = if runtime
                .store
                .sync_state(account_name, &cached.id)?
                .full_sync_done
            {
                runtime.store.mailbox_counts(account_name, &cached.id)?
            } else {
                let fresh = account.graph.folder(&cached.id).await?;
                (fresh.total_item_count, fresh.unread_item_count)
            };
            if let Some(entry) = session
                .folders
                .iter_mut()
                .find(|folder| folder.id == cached.id)
            {
                entry.total = total;
                entry.unread = unread;
            }
            let validity = runtime.store.uid_validity(account_name, &cached.id)?;
            let next = runtime.store.uid_next(account_name, &cached.id)?;
            writer
                .write_all(
                    format!(
                        "* STATUS {} (MESSAGES {} UNSEEN {} UIDNEXT {} UIDVALIDITY {})\r\n",
                        imap_quote(&mailbox),
                        total,
                        unread,
                        next,
                        validity
                    )
                    .as_bytes(),
                )
                .await?;
            tagged_ok(writer, tag, "STATUS completed").await?;
        }
        ImapCommand::Fetch {
            sequence,
            attributes,
        } => {
            fetch_messages(session, runtime, &sequence, &attributes, false, writer).await?;
            tagged_ok(writer, tag, "FETCH completed").await?;
        }
        ImapCommand::Store {
            sequence,
            action,
            flags,
        } => {
            store_flags(session, runtime, &sequence, &action, &flags, false, writer).await?;
            tagged_ok(writer, tag, "STORE completed").await?;
        }
        ImapCommand::Search { criteria } => {
            search_messages(session, runtime, &criteria, false, writer).await?;
            tagged_ok(writer, tag, "SEARCH completed").await?;
        }
        ImapCommand::Uid { subcommand } => match *subcommand {
            ImapCommand::Fetch {
                sequence,
                attributes,
            } => {
                fetch_messages(session, runtime, &sequence, &attributes, true, writer).await?;
                tagged_ok(writer, tag, "UID FETCH completed").await?;
            }
            ImapCommand::Store {
                sequence,
                action,
                flags,
            } => {
                store_flags(session, runtime, &sequence, &action, &flags, true, writer).await?;
                tagged_ok(writer, tag, "UID STORE completed").await?;
            }
            ImapCommand::Search { criteria } => {
                search_messages(session, runtime, &criteria, true, writer).await?;
                tagged_ok(writer, tag, "UID SEARCH completed").await?;
            }
            ImapCommand::Copy { sequence, mailbox } => {
                let name = session.decode_mailbox_name(&mailbox);
                copy_or_move(session, runtime, &sequence, &name, true, false, writer).await?;
                tagged_ok(writer, tag, "UID COPY completed").await?;
            }
            ImapCommand::Move { sequence, mailbox } => {
                let name = session.decode_mailbox_name(&mailbox);
                copy_or_move(session, runtime, &sequence, &name, true, true, writer).await?;
                tagged_ok(writer, tag, "UID MOVE completed").await?;
            }
            _ => bail!("unsupported UID subcommand"),
        },
        ImapCommand::Copy { sequence, mailbox } => {
            let name = session.decode_mailbox_name(&mailbox);
            copy_or_move(session, runtime, &sequence, &name, false, false, writer).await?;
            tagged_ok(writer, tag, "COPY completed").await?;
        }
        ImapCommand::Move { sequence, mailbox } => {
            let name = session.decode_mailbox_name(&mailbox);
            copy_or_move(session, runtime, &sequence, &name, false, true, writer).await?;
            tagged_ok(writer, tag, "MOVE completed").await?;
        }
        ImapCommand::Expunge => {
            expunge(session, runtime, writer).await?;
            tagged_ok(writer, tag, "EXPUNGE completed").await?;
        }
        ImapCommand::Close => {
            expunge(session, runtime, writer).await?;
            session.selected = None;
            tagged_ok(writer, tag, "CLOSE completed").await?;
        }
        ImapCommand::Unselect => {
            session.selected = None;
            tagged_ok(writer, tag, "UNSELECT completed").await?;
        }
        ImapCommand::Noop => {
            if session.selected.is_some() {
                refresh_selected(session, runtime, writer).await?;
            }
            tagged_ok(writer, tag, "NOOP completed").await?;
        }
        ImapCommand::Idle => {
            require_account(session)?;
            writer.write_all(b"+ idling\r\n").await?;
            idle(session, runtime, reader, writer).await?;
            tagged_ok(writer, tag, "IDLE terminated").await?;
        }
        ImapCommand::Append {
            mailbox,
            flags,
            literal_size,
        } => {
            if literal_size > MAX_APPEND_BYTES {
                bail!("APPEND literal exceeds the 35 MiB limit");
            }
            let account = require_account(session)?.clone();
            let name = session.decode_mailbox_name(&mailbox);
            let folder = find_mailbox(&session.folders, &name)?.clone();
            writer.write_all(b"+ Ready for literal data\r\n").await?;
            let mut raw = vec![0; literal_size as usize];
            reader.read_exact(&mut raw).await?;
            consume_line_end(reader).await?;
            if folder.special_use == Some("\\Sent") {
                // Graph's sendMail already files a copy in Sent Items, and a
                // MIME upload would only create a second copy marked as a
                // draft. Accept the APPEND so the client does not retry.
                tracing::debug!("ignoring APPEND to the sent folder; Graph keeps its own copy");
                tagged_ok(writer, tag, "APPEND completed (sent copy kept by Graph)").await?;
                return Ok(false);
            }
            let created = account.graph.create_mime(&folder.id, &raw).await?;
            let upper_flags = flags.as_deref().unwrap_or_default().to_ascii_uppercase();
            let seen = upper_flags.contains("\\SEEN");
            let flagged = upper_flags.contains("\\FLAGGED");
            if seen {
                account.graph.set_read(&created.id, true).await?;
            }
            if flagged {
                account.graph.set_flagged(&created.id, true).await?;
            }
            let account_name = account.config.name.as_str();
            runtime.store.upsert_messages(
                account_name,
                &folder.id,
                std::slice::from_ref(&created),
            )?;
            runtime.store.set_local_flags(
                account_name,
                &created.id,
                Some(seen),
                Some(flagged),
                None,
            )?;
            tagged_ok(writer, tag, "APPEND completed").await?;
        }
        ImapCommand::Create { mailbox } => {
            let account = require_account(session)?.clone();
            let name = session.decode_mailbox_name(&mailbox);
            let name = name.trim_end_matches('/');
            let (parent, leaf) = split_mailbox_name(name);
            if leaf.is_empty() {
                bail!("mailbox name must not be empty");
            }
            let parent_id = parent
                .map(|name| find_mailbox(&session.folders, name).map(|folder| folder.id.as_str()))
                .transpose()?;
            account.graph.create_folder(parent_id, leaf).await?;
            session.folders = reload_folders(&account, runtime).await?;
            tagged_ok(writer, tag, "CREATE completed").await?;
        }
        ImapCommand::Delete { mailbox } => {
            let account = require_account(session)?.clone();
            let name = session.decode_mailbox_name(&mailbox);
            let folder = find_mailbox(&session.folders, &name)?.clone();
            if folder.special_use.is_some() {
                bail!("special folders cannot be deleted");
            }
            account.graph.delete_folder(&folder.id).await?;
            if session
                .selected
                .as_ref()
                .is_some_and(|selected| selected.id == folder.id)
            {
                session.selected = None;
            }
            session.folders = reload_folders(&account, runtime).await?;
            tagged_ok(writer, tag, "DELETE completed").await?;
        }
        ImapCommand::Rename { from, to } => {
            let account = require_account(session)?.clone();
            let from = session.decode_mailbox_name(&from);
            let to = session.decode_mailbox_name(&to);
            let old = find_mailbox(&session.folders, &from)?.clone();
            let (old_parent, _) = split_mailbox_name(&from);
            let (new_parent, leaf) = split_mailbox_name(&to);
            if old_parent.map(str::to_ascii_lowercase) != new_parent.map(str::to_ascii_lowercase) {
                bail!("moving a folder to a different parent is not supported by Graph");
            }
            account.graph.rename_folder(&old.id, leaf).await?;
            session.folders = reload_folders(&account, runtime).await?;
            tagged_ok(writer, tag, "RENAME completed").await?;
        }
        ImapCommand::Subscribe { .. } | ImapCommand::Unsubscribe { .. } => {
            tagged_ok(writer, tag, "subscription updated").await?;
        }
        ImapCommand::Namespace => {
            require_account(session)?;
            writer
                .write_all(b"* NAMESPACE ((\"\" \"/\")) NIL NIL\r\n")
                .await?;
            tagged_ok(writer, tag, "NAMESPACE completed").await?;
        }
        ImapCommand::Enable(values) => {
            let enabled: Vec<String> = values
                .iter()
                .map(|value| value.to_ascii_uppercase())
                .filter(|value| ENABLEABLE.contains(&value.as_str()))
                .collect();
            if enabled.iter().any(|value| value == "UTF8=ACCEPT") {
                session.utf8 = true;
            }
            writer
                .write_all(format!("* ENABLED {}\r\n", enabled.join(" ")).as_bytes())
                .await?;
            tagged_ok(writer, tag, "ENABLE completed").await?;
        }
        ImapCommand::Sort { .. } => bail!("SORT is not supported"),
        ImapCommand::GetQuota { .. } | ImapCommand::GetQuotaRoot { .. } => {
            bail!("quota reporting is not supported");
        }
    }
    Ok(false)
}

/// The folder list from the local index, walking Graph only when the index
/// has not been populated yet.
async fn load_folders(account: &AccountRuntime, runtime: &Runtime) -> Result<Vec<Mailbox>> {
    let stored = runtime.store.folders(&account.config.name)?;
    if stored.is_empty() {
        return reload_folders(account, runtime).await;
    }
    Ok(mailboxes_from(stored))
}

/// Re-walk the folder tree on Graph (after CREATE/DELETE/RENAME) and update
/// the local index.
async fn reload_folders(account: &AccountRuntime, runtime: &Runtime) -> Result<Vec<Mailbox>> {
    let folders = fetch_folder_tree(&account.graph).await?;
    runtime
        .store
        .replace_folders(&account.config.name, &folders)?;
    Ok(mailboxes_from(folders))
}

/// Build IMAP names (`Parent/Child`) from stored folder rows. Folders whose
/// parent is not in the list are roots.
fn mailboxes_from(stored: Vec<StoredFolder>) -> Vec<Mailbox> {
    let by_id: HashMap<&str, &StoredFolder> = stored
        .iter()
        .map(|folder| (folder.id.as_str(), folder))
        .collect();
    let leaf_name = |folder: &StoredFolder| {
        if folder.special_use.as_deref() == Some("\\Inbox") {
            "INBOX".to_owned()
        } else {
            folder.display_name.replace('/', "-")
        }
    };
    let mut folders = Vec::with_capacity(stored.len());
    for folder in &stored {
        let mut path = vec![leaf_name(folder)];
        let mut parent = folder.parent_id.as_deref().and_then(|id| by_id.get(id));
        let mut depth = 0;
        while let Some(ancestor) = parent {
            path.push(leaf_name(ancestor));
            parent = ancestor.parent_id.as_deref().and_then(|id| by_id.get(id));
            depth += 1;
            if depth > 64 {
                break;
            }
        }
        path.reverse();
        let special_use = folder.special_use.as_deref().and_then(special_use_flag);
        folders.push(Mailbox {
            id: folder.id.clone(),
            name: path.join("/"),
            child_count: folder.child_folder_count,
            total: folder.total_item_count,
            unread: folder.unread_item_count,
            special_use,
        });
    }
    folders.sort_by_key(|folder| folder.name.to_ascii_lowercase());
    folders
}

fn special_use_flag(flag: &str) -> Option<&'static str> {
    crate::graph::WELL_KNOWN_FOLDERS
        .iter()
        .map(|(_, known)| *known)
        .find(|known| *known == flag)
}

/// The messages of a folder, from the local index when it has any rows for
/// the folder, otherwise a bounded live listing from Graph so a freshly
/// installed bridge is usable while the first sync runs.
async fn list_messages(
    account: &AccountRuntime,
    runtime: &Runtime,
    mailbox_id: &str,
    allow_bootstrap: bool,
) -> Result<(Vec<SelectedMessage>, bool)> {
    let account_name = account.config.name.as_str();
    let stored = runtime.store.list_mailbox(account_name, mailbox_id)?;
    let synced = runtime
        .store
        .sync_state(account_name, mailbox_id)?
        .full_sync_done;
    if !stored.is_empty() || synced || !allow_bootstrap {
        let messages = stored
            .into_iter()
            .map(|row| SelectedMessage {
                summary: row.summary,
                uid: row.uid,
                deleted: false,
            })
            .collect();
        return Ok((messages, true));
    }
    let summaries = account
        .graph
        .messages(mailbox_id, runtime.config.server.bootstrap_limit())
        .await?;
    let mut messages = Vec::with_capacity(summaries.len());
    for summary in summaries {
        let uid = runtime
            .store
            .uid_for(account_name, mailbox_id, &summary.id)?;
        messages.push(SelectedMessage {
            summary,
            uid,
            deleted: false,
        });
    }
    Ok((messages, false))
}

async fn select_mailbox(
    session: &mut Session,
    runtime: &Runtime,
    name: &str,
    read_only: bool,
    writer: &mut WriteHalf<TcpStream>,
) -> Result<()> {
    let account = require_account(session)?.clone();
    let mailbox = find_mailbox(&session.folders, name)?.clone();
    // Per RFC 3501 a failed SELECT leaves no mailbox selected.
    session.selected = None;
    let (messages, from_store) = list_messages(&account, runtime, &mailbox.id, true).await?;
    let validity = runtime
        .store
        .uid_validity(&account.config.name, &mailbox.id)?;
    let uid_next = runtime.store.uid_next(&account.config.name, &mailbox.id)?;
    writer
        .write_all(format_flags(PERMANENT_FLAGS).as_bytes())
        .await?;
    writer
        .write_all(format_exists(messages.len() as u32).as_bytes())
        .await?;
    writer.write_all(format_recent(0).as_bytes()).await?;
    writer
        .write_all(
            format!(
                "* OK [PERMANENTFLAGS ({})] Flags permitted\r\n",
                PERMANENT_FLAGS.join(" ")
            )
            .as_bytes(),
        )
        .await?;
    writer
        .write_all(format!("* OK [UIDVALIDITY {validity}] UIDs valid\r\n").as_bytes())
        .await?;
    writer
        .write_all(format!("* OK [UIDNEXT {uid_next}] Predicted next UID\r\n").as_bytes())
        .await?;
    if let Some((index, _)) = messages
        .iter()
        .enumerate()
        .find(|(_, message)| !message.summary.is_read)
    {
        writer
            .write_all(format!("* OK [UNSEEN {}] First unseen\r\n", index + 1).as_bytes())
            .await?;
    }
    session.selected = Some(SelectedMailbox {
        id: mailbox.id,
        messages,
        read_only,
        from_store,
    });
    Ok(())
}

/// Re-read the selected mailbox and tell the client what changed: EXPUNGE
/// for messages that disappeared, FETCH for flag changes, and EXISTS when
/// new messages arrived.
async fn refresh_selected(
    session: &mut Session,
    runtime: &Runtime,
    writer: &mut WriteHalf<TcpStream>,
) -> Result<()> {
    let account = require_account(session)?.clone();
    let selected = require_selected_mut(session)?;
    let account_name = account.config.name.as_str();
    let fresh = if selected.from_store {
        list_messages(&account, runtime, &selected.id, false)
            .await?
            .0
    } else if runtime
        .store
        .sync_state(account_name, &selected.id)?
        .full_sync_done
    {
        // The first sync finished since SELECT: switch this session to the
        // index. Every bootstrap message is in it under the same UID, so the
        // diff below only appends the rest of the history.
        selected.from_store = true;
        list_messages(&account, runtime, &selected.id, false)
            .await?
            .0
    } else {
        // Still bootstrapping: diff against a fresh live listing so the
        // partial index never expunges messages it has not reached yet.
        list_messages(&account, runtime, &selected.id, true)
            .await?
            .0
    };
    let fresh_by_id: HashMap<&str, &SelectedMessage> = fresh
        .iter()
        .map(|message| (message.summary.id.as_str(), message))
        .collect();

    // Removed messages, highest sequence number first so numbering stays valid.
    for index in (0..selected.messages.len()).rev() {
        if !fresh_by_id.contains_key(selected.messages[index].summary.id.as_str()) {
            selected.messages.remove(index);
            writer
                .write_all(format!("* {} EXPUNGE\r\n", index + 1).as_bytes())
                .await?;
        }
    }

    // Flag changes on messages we already know about.
    for (index, message) in selected.messages.iter_mut().enumerate() {
        let Some(current) = fresh_by_id.get(message.summary.id.as_str()) else {
            continue;
        };
        let changed = message.summary.is_read != current.summary.is_read
            || message.summary.is_flagged() != current.summary.is_flagged()
            || message.summary.is_pinned() != current.summary.is_pinned();
        if changed {
            message.summary.is_read = current.summary.is_read;
            message.summary.flag = current.summary.flag.clone();
            message.summary.stored_pinned = Some(current.summary.is_pinned());
            writer
                .write_all(
                    format!(
                        "* {} FETCH (FLAGS {} UID {})\r\n",
                        index + 1,
                        message_flags(message),
                        message.uid
                    )
                    .as_bytes(),
                )
                .await?;
        }
    }

    // New messages are appended so existing sequence numbers are unchanged.
    let known: HashSet<String> = selected
        .messages
        .iter()
        .map(|message| message.summary.id.clone())
        .collect();
    let mut added = false;
    for message in fresh {
        if !known.contains(&message.summary.id) {
            selected.messages.push(message);
            added = true;
        }
    }
    if added {
        writer
            .write_all(format_exists(selected.messages.len() as u32).as_bytes())
            .await?;
        writer.write_all(format_recent(0).as_bytes()).await?;
    }
    Ok(())
}

/// Wait for the client's DONE while pushing mailbox changes: the sync task
/// signals folder updates, and a slow poll covers anything it missed.
async fn idle(
    session: &mut Session,
    runtime: &Runtime,
    reader: &mut BufReader<ReadHalf<TcpStream>>,
    writer: &mut WriteHalf<TcpStream>,
) -> Result<()> {
    let mut interval = tokio::time::interval(IDLE_POLL_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await;
    let mut changes = runtime.changes.subscribe();
    let account_name = session
        .account
        .as_ref()
        .map(|account| account.config.name.clone())
        .unwrap_or_default();
    loop {
        tokio::select! {
            change = changes.recv() => {
                let relevant = match change {
                    Ok(change) => {
                        change.account == account_name
                            && session
                                .selected
                                .as_ref()
                                .is_some_and(|selected| selected.id == change.folder_id)
                    }
                    // Lagged: something changed, just refresh.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => true,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => false,
                };
                if relevant
                    && session.selected.is_some()
                    && let Err(error) = refresh_selected(session, runtime, writer).await
                {
                    tracing::warn!(%error, "IDLE refresh failed");
                }
            }
            // fill_buf only peeks, so cancelling it loses nothing.
            ready = reader.fill_buf() => {
                if ready?.is_empty() {
                    bail!("connection closed while idling");
                }
                let done = read_command_line(reader).await?.unwrap_or_default();
                if !done.trim().eq_ignore_ascii_case("DONE") {
                    bail!("expected DONE while idling");
                }
                return Ok(());
            }
            _ = interval.tick() => {
                if session.selected.is_some()
                    && let Err(error) = refresh_selected(session, runtime, writer).await
                {
                    tracing::warn!(%error, "IDLE refresh failed");
                }
            }
        }
    }
}

/// What a FETCH needs from Graph: nothing, just the headers, or the whole
/// MIME message.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Need {
    Nothing,
    Headers,
    Mime,
}

async fn fetch_messages(
    session: &mut Session,
    runtime: &Runtime,
    sequence: &str,
    attributes: &str,
    uid_mode: bool,
    writer: &mut WriteHalf<TcpStream>,
) -> Result<()> {
    let account = require_account(session)?.clone();
    let account_name = account.config.name.as_str();
    let body_cache_max = runtime.config.sync.body_cache_max_bytes();
    let selected = require_selected_mut(session)?;
    let indices = selected_indices(selected, sequence, uid_mode)?;

    let upper = attributes.to_ascii_uppercase();
    let macro_name = upper.trim().trim_matches(['(', ')']);
    let macro_all = macro_name == "ALL";
    let macro_fast = macro_name == "FAST";
    let macro_full = macro_name == "FULL";
    let tokens: HashSet<&str> = upper
        .split(|character: char| character.is_ascii_whitespace() || matches!(character, '(' | ')'))
        .filter(|value| !value.is_empty())
        .collect();
    let wants_uid = uid_mode || tokens.contains("UID");
    let wants_flags = tokens.contains("FLAGS") || macro_all || macro_fast || macro_full;
    let wants_date = tokens.contains("INTERNALDATE") || macro_all || macro_fast || macro_full;
    let wants_size = tokens.contains("RFC822.SIZE") || macro_all || macro_fast || macro_full;
    let wants_envelope = tokens.contains("ENVELOPE") || macro_all || macro_full;
    let wants_bodystructure = tokens.contains("BODYSTRUCTURE");
    let wants_body_descriptor = tokens.contains("BODY") || macro_full;
    let body_requests = parse_body_requests(attributes);

    let mut need = Need::Nothing;
    if wants_envelope {
        need = need.max(Need::Headers);
    }
    if wants_bodystructure || wants_body_descriptor {
        need = Need::Mime;
    }
    for request in &body_requests {
        need = need.max(request.kind.need());
    }

    for index in indices {
        let message = &mut selected.messages[index];
        let sequence_number = index + 1;
        let mut raw: Option<Vec<u8>> = None;
        let mut headers: Option<Vec<u8>> = None;
        let mut per_message_need = need;
        if wants_size && message.summary.size().is_none() {
            per_message_need = Need::Mime;
        }
        match per_message_need {
            Need::Mime => {
                raw = Some(
                    cached_mime(
                        runtime,
                        &account,
                        account_name,
                        &message.summary.id,
                        body_cache_max,
                    )
                    .await?,
                );
            }
            Need::Headers => {
                // A cached body answers header requests without a round trip.
                if let Some(mime) = runtime.store.body_get(account_name, &message.summary.id)? {
                    raw = Some(mime);
                } else {
                    match account.graph.headers(&message.summary.id).await? {
                        Some(block) => headers = Some(block),
                        None => {
                            raw = Some(
                                cached_mime(
                                    runtime,
                                    &account,
                                    account_name,
                                    &message.summary.id,
                                    body_cache_max,
                                )
                                .await?,
                            );
                        }
                    }
                }
            }
            Need::Nothing => {}
        }
        // Header block used for ENVELOPE and header sections.
        let header_block: Option<Vec<u8>> = match (&headers, &raw) {
            (Some(block), _) => Some(block.clone()),
            (None, Some(mime)) => Some(body_section(mime, &BodyKind::Header)),
            (None, None) => None,
        };

        let mut fields = Vec::new();
        if wants_uid {
            fields.push(format!("UID {}", message.uid));
        }
        if wants_flags {
            fields.push(format!("FLAGS {}", message_flags(message)));
        }
        if wants_date {
            fields.push(format!(
                "INTERNALDATE {}",
                imap_quote(&internal_date(&message.summary))
            ));
        }
        if wants_size {
            let size = message
                .summary
                .size()
                .or_else(|| raw.as_ref().map(|bytes| bytes.len() as u64))
                .unwrap_or(0);
            fields.push(format!("RFC822.SIZE {size}"));
        }
        if wants_envelope {
            fields.push(format!(
                "ENVELOPE {}",
                envelope(header_block.as_deref().unwrap_or_default())
            ));
        }
        if wants_bodystructure {
            let bytes = raw.as_deref().unwrap_or_default();
            fields.push(format!("BODYSTRUCTURE {}", body_structure(bytes)));
        }
        if wants_body_descriptor {
            let bytes = raw.as_deref().unwrap_or_default();
            fields.push(format!("BODY {}", body_structure(bytes)));
        }

        writer
            .write_all(format!("* {sequence_number} FETCH (").as_bytes())
            .await?;
        if !fields.is_empty() {
            writer.write_all(fields.join(" ").as_bytes()).await?;
        }
        let mut mark_seen = false;
        for (position, request) in body_requests.iter().enumerate() {
            if !fields.is_empty() || position > 0 {
                writer.write_all(b" ").await?;
            }
            let mut data = match (&request.kind, &raw, &header_block) {
                (kind, _, Some(block)) if kind.need() == Need::Headers => body_section(block, kind),
                (kind, Some(mime), _) => body_section(mime, kind),
                _ => Vec::new(),
            };
            if request.start < data.len() {
                let end = request.length.map_or(data.len(), |length| {
                    request.start.saturating_add(length).min(data.len())
                });
                data = data[request.start..end].to_vec();
            } else {
                data.clear();
            }
            let key = if request.partial {
                format!("{}<{}>", request.key, request.start)
            } else {
                request.key.clone()
            };
            writer
                .write_all(format!("{key} {{{}}}\r\n", data.len()).as_bytes())
                .await?;
            writer.write_all(&data).await?;
            if !request.peek {
                mark_seen = true;
            }
        }
        if mark_seen && !message.summary.is_read && !selected.read_only {
            account.graph.set_read(&message.summary.id, true).await?;
            runtime.store.set_local_flags(
                account_name,
                &message.summary.id,
                Some(true),
                None,
                None,
            )?;
            message.summary.is_read = true;
            if !wants_flags {
                writer
                    .write_all(format!(" FLAGS {}", message_flags(message)).as_bytes())
                    .await?;
            }
        }
        writer.write_all(b")\r\n").await?;
    }
    Ok(())
}

/// The full MIME of a message, served from the body cache when present and
/// cached after a Graph download otherwise.
async fn cached_mime(
    runtime: &Runtime,
    account: &AccountRuntime,
    account_name: &str,
    message_id: &str,
    body_cache_max: u64,
) -> Result<Vec<u8>> {
    if let Some(mime) = runtime.store.body_get(account_name, message_id)? {
        return Ok(mime);
    }
    let mime = account.graph.mime(message_id).await?;
    if let Err(error) = runtime
        .store
        .body_put(account_name, message_id, &mime, body_cache_max)
    {
        tracing::warn!(%error, "could not cache message body");
    }
    Ok(mime)
}

async fn store_flags(
    session: &mut Session,
    runtime: &Runtime,
    sequence: &str,
    action: &str,
    flags: &str,
    uid_mode: bool,
    writer: &mut WriteHalf<TcpStream>,
) -> Result<()> {
    let account = require_account(session)?.clone();
    let account_name = account.config.name.as_str();
    let selected = require_selected_mut(session)?;
    if selected.read_only {
        bail!("mailbox is read-only");
    }
    let indices = selected_indices(selected, sequence, uid_mode)?;
    let upper_flags = flags.to_ascii_uppercase();
    let upper_action = action.to_ascii_uppercase();
    let add = upper_action.starts_with('+');
    let remove = upper_action.starts_with('-');
    let replace = !add && !remove;
    let silent = upper_action.ends_with(".SILENT");
    let has_seen = upper_flags.contains("\\SEEN");
    let has_deleted = upper_flags.contains("\\DELETED");
    let has_flagged = upper_flags.contains("\\FLAGGED");
    let has_pinned = upper_flags.contains(&PINNED_KEYWORD.to_ascii_uppercase());
    for index in indices {
        let message = &mut selected.messages[index];

        let target_seen = if has_seen {
            Some(!remove)
        } else if replace {
            Some(false)
        } else {
            None
        };
        if let Some(value) = target_seen
            && value != message.summary.is_read
        {
            account.graph.set_read(&message.summary.id, value).await?;
            runtime.store.set_local_flags(
                account_name,
                &message.summary.id,
                Some(value),
                None,
                None,
            )?;
            message.summary.is_read = value;
        }

        let target_flagged = if has_flagged {
            Some(!remove)
        } else if replace {
            Some(false)
        } else {
            None
        };
        if let Some(value) = target_flagged
            && value != message.summary.is_flagged()
        {
            account
                .graph
                .set_flagged(&message.summary.id, value)
                .await?;
            runtime.store.set_local_flags(
                account_name,
                &message.summary.id,
                None,
                Some(value),
                None,
            )?;
            message.summary.flag = Some(crate::graph::FollowupFlag {
                flag_status: if value { "flagged" } else { "notFlagged" }.to_owned(),
            });
        }

        let target_pinned = if has_pinned {
            Some(!remove)
        } else if replace {
            Some(false)
        } else {
            None
        };
        if let Some(value) = target_pinned
            && value != message.summary.is_pinned()
        {
            account.graph.set_pinned(&message.summary.id, value).await?;
            runtime.store.set_local_flags(
                account_name,
                &message.summary.id,
                None,
                None,
                Some(value),
            )?;
            message.summary.stored_pinned = Some(value);
        }

        if has_deleted {
            message.deleted = !remove;
        } else if replace {
            message.deleted = false;
        }
        if !silent {
            writer
                .write_all(
                    format!(
                        "* {} FETCH (FLAGS {} UID {})\r\n",
                        index + 1,
                        message_flags(message),
                        message.uid
                    )
                    .as_bytes(),
                )
                .await?;
        }
    }
    Ok(())
}

async fn search_messages(
    session: &Session,
    runtime: &Runtime,
    criteria: &str,
    uid_mode: bool,
    writer: &mut WriteHalf<TcpStream>,
) -> Result<()> {
    let account = require_account(session)?;
    let selected = require_selected(session)?;
    let criterion = search::parse(criteria)?;
    let seq_to_uid: Vec<u32> = selected
        .messages
        .iter()
        .map(|message| message.uid)
        .collect();
    let deleted_uids: Vec<u32> = selected
        .messages
        .iter()
        .filter(|message| message.deleted)
        .map(|message| message.uid)
        .collect();
    let view = SessionView {
        deleted_uids: &deleted_uids,
        seq_to_uid: &seq_to_uid,
    };
    let sql = search::to_sql(&criterion, &view);
    let matched: HashSet<u32> = if selected.from_store {
        let store = runtime.store.clone();
        let account_name = account.config.name.clone();
        let mailbox_id = selected.id.clone();
        tokio::task::spawn_blocking(move || store.search(&account_name, &mailbox_id, &sql))
            .await
            .context("search task failed")??
            .into_iter()
            .collect()
    } else {
        // Bootstrap listings are not indexed yet; evaluate in memory via a
        // throwaway index of just this session's messages.
        bail!("SEARCH is available once this folder's first sync completes; try again shortly")
    };
    let mut values = Vec::new();
    for (index, message) in selected.messages.iter().enumerate() {
        if matched.contains(&message.uid) {
            values.push(if uid_mode {
                message.uid.to_string()
            } else {
                (index + 1).to_string()
            });
        }
    }
    writer
        .write_all(format!("* SEARCH {}\r\n", values.join(" ")).as_bytes())
        .await?;
    Ok(())
}

async fn copy_or_move(
    session: &mut Session,
    runtime: &Runtime,
    sequence: &str,
    mailbox: &str,
    uid_mode: bool,
    move_messages: bool,
    writer: &mut WriteHalf<TcpStream>,
) -> Result<()> {
    let account = require_account(session)?.clone();
    let account_name = account.config.name.as_str();
    let destination = find_mailbox(&session.folders, mailbox)?.clone();
    let selected = require_selected_mut(session)?;
    if selected.read_only && move_messages {
        bail!("mailbox is read-only");
    }
    if destination.id == selected.id {
        bail!("source and destination mailbox are the same");
    }
    let indices = selected_indices(selected, sequence, uid_mode)?;
    // Process from the highest sequence number down so that each EXPUNGE we
    // emit leaves the remaining indices valid, even if Graph fails midway.
    for index in indices.into_iter().rev() {
        let message_id = selected.messages[index].summary.id.clone();
        if move_messages {
            account
                .graph
                .move_message(&message_id, &destination.id)
                .await
                .with_context(|| format!("could not move message {}", index + 1))?;
            runtime.store.remove_messages(
                account_name,
                &selected.id,
                std::slice::from_ref(&message_id),
            )?;
            selected.messages.remove(index);
            writer
                .write_all(format!("* {} EXPUNGE\r\n", index + 1).as_bytes())
                .await?;
        } else {
            account
                .graph
                .copy_message(&message_id, &destination.id)
                .await
                .with_context(|| format!("could not copy message {}", index + 1))?;
        }
    }
    Ok(())
}

async fn expunge(
    session: &mut Session,
    runtime: &Runtime,
    writer: &mut WriteHalf<TcpStream>,
) -> Result<()> {
    let account = require_account(session)?.clone();
    let account_name = account.config.name.as_str();
    let selected = require_selected_mut(session)?;
    if selected.read_only {
        bail!("mailbox is read-only");
    }
    for index in (0..selected.messages.len()).rev() {
        if selected.messages[index].deleted {
            let message_id = selected.messages[index].summary.id.clone();
            account.graph.delete_message(&message_id).await?;
            runtime.store.remove_messages(
                account_name,
                &selected.id,
                std::slice::from_ref(&message_id),
            )?;
            selected.messages.remove(index);
            writer
                .write_all(format!("* {} EXPUNGE\r\n", index + 1).as_bytes())
                .await?;
        }
    }
    Ok(())
}

fn selected_indices(
    selected: &SelectedMailbox,
    sequence: &str,
    uid_mode: bool,
) -> Result<Vec<usize>> {
    let set = parse_sequence_set(sequence).map_err(anyhow::Error::msg)?;
    if uid_mode {
        let max_uid = selected
            .messages
            .iter()
            .map(|message| message.uid)
            .max()
            .unwrap_or(0);
        let mut wanted: HashSet<u32> = sequence_set_to_uids(&set, max_uid).into_iter().collect();
        // RFC 3501: `n:*` always includes the highest existing UID, even when
        // n is larger than it.
        if max_uid > 0 && range_from_reaches_star(&set) {
            wanted.insert(max_uid);
        }
        Ok(selected
            .messages
            .iter()
            .enumerate()
            .filter_map(|(index, message)| wanted.contains(&message.uid).then_some(index))
            .collect())
    } else {
        Ok(sequence_set_to_uids(&set, selected.messages.len() as u32)
            .into_iter()
            .map(|sequence| sequence as usize - 1)
            .collect())
    }
}

fn range_from_reaches_star(set: &SequenceSet) -> bool {
    match set {
        SequenceSet::RangeFrom(_) | SequenceSet::All => true,
        SequenceSet::List(sets) => sets.iter().any(range_from_reaches_star),
        SequenceSet::Single(_) | SequenceSet::Range(..) => false,
    }
}

fn require_account(session: &Session) -> Result<&Arc<AccountRuntime>> {
    session
        .account
        .as_ref()
        .context("authenticate with LOGIN first")
}

fn require_selected(session: &Session) -> Result<&SelectedMailbox> {
    session.selected.as_ref().context("select a mailbox first")
}

fn require_selected_mut(session: &mut Session) -> Result<&mut SelectedMailbox> {
    session.selected.as_mut().context("select a mailbox first")
}

fn find_mailbox<'a>(folders: &'a [Mailbox], name: &str) -> Result<&'a Mailbox> {
    let name = name.trim_end_matches('/');
    folders
        .iter()
        .find(|folder| folder.name.eq_ignore_ascii_case(name))
        .with_context(|| format!("mailbox {name:?} does not exist"))
}

fn split_mailbox_name(name: &str) -> (Option<&str>, &str) {
    name.rsplit_once('/')
        .map_or((None, name), |(parent, leaf)| (Some(parent), leaf))
}

fn parse_command_compat(line: &str) -> std::result::Result<TaggedCommand, ParseError> {
    let Some((tag, rest)) = line.trim().split_once(' ') else {
        return parse_command(line);
    };
    let (verb, arguments) = rest.split_once(' ').unwrap_or((rest, ""));
    if !verb.eq_ignore_ascii_case("STATUS") {
        return parse_command(line);
    }
    let (mailbox, items) = parse_first_imap_argument(arguments)?;
    if items.trim().is_empty() {
        return Err(ParseError::MissingArgument("status items".into()));
    }
    Ok(TaggedCommand {
        tag: tag.to_owned(),
        command: ImapCommand::Status {
            mailbox,
            items: items.trim().to_owned(),
        },
    })
}

fn parse_first_imap_argument(input: &str) -> std::result::Result<(String, &str), ParseError> {
    let input = input.trim_start();
    if let Some(quoted) = input.strip_prefix('"') {
        let mut escaped = false;
        let mut value = String::new();
        for (index, character) in quoted.char_indices() {
            if escaped {
                value.push(character);
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                return Ok((value, &quoted[index + character.len_utf8()..]));
            } else {
                value.push(character);
            }
        }
        Err(ParseError::MissingArgument(
            "unterminated mailbox name".into(),
        ))
    } else {
        let (value, rest) = input.split_once(' ').unwrap_or((input, ""));
        Ok((value.to_owned(), rest))
    }
}

fn id_command_tag(line: &str) -> Option<&str> {
    let (tag, rest) = line.trim().split_once(' ')?;
    let verb = rest.split_ascii_whitespace().next()?;
    verb.eq_ignore_ascii_case("ID").then_some(tag)
}

fn mailbox_pattern_matches(name: &str, pattern: &str) -> bool {
    fn matches(
        name: &[char],
        pattern: &[char],
        name_index: usize,
        pattern_index: usize,
        memo: &mut HashSet<(usize, usize)>,
    ) -> bool {
        if !memo.insert((name_index, pattern_index)) {
            return false;
        }
        let Some(token) = pattern.get(pattern_index) else {
            return name_index == name.len();
        };
        match token {
            '*' => (name_index..=name.len())
                .any(|next| matches(name, pattern, next, pattern_index + 1, memo)),
            '%' => {
                matches(name, pattern, name_index, pattern_index + 1, memo)
                    || (name_index < name.len()
                        && name[name_index] != '/'
                        && matches(name, pattern, name_index + 1, pattern_index, memo))
            }
            literal => {
                name.get(name_index)
                    .is_some_and(|character| character.eq_ignore_ascii_case(literal))
                    && matches(name, pattern, name_index + 1, pattern_index + 1, memo)
            }
        }
    }
    matches(
        &name.chars().collect::<Vec<_>>(),
        &pattern.chars().collect::<Vec<_>>(),
        0,
        0,
        &mut HashSet::new(),
    )
}

fn message_flags(message: &SelectedMessage) -> String {
    let mut flags = Vec::new();
    if message.summary.is_read {
        flags.push("\\Seen");
    }
    if message.deleted {
        flags.push("\\Deleted");
    }
    if message.summary.is_flagged() {
        flags.push("\\Flagged");
    }
    if message.summary.is_draft {
        flags.push("\\Draft");
    }
    if message.summary.is_pinned() {
        flags.push(PINNED_KEYWORD);
    }
    format!("({})", flags.join(" "))
}

fn internal_date(summary: &MessageSummary) -> String {
    summary
        .received_date_time
        .as_deref()
        .or(summary.sent_date_time.as_deref())
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.format("%d-%b-%Y %H:%M:%S %z").to_string())
        .unwrap_or_else(|| {
            DateTime::<Utc>::from_timestamp(0, 0)
                .unwrap()
                .format("%d-%b-%Y %H:%M:%S +0000")
                .to_string()
        })
}

fn envelope(raw: &[u8]) -> String {
    let Some(message) = MessageParser::default().parse(raw) else {
        return "(NIL NIL NIL NIL NIL NIL NIL NIL NIL NIL)".to_owned();
    };
    let from = format_addresses(message.from());
    let sender = format_addresses(message.sender().or(message.from()));
    let reply_to = format_addresses(message.reply_to().or(message.from()));
    format!(
        "({} {} {} {} {} {} {} {} {} {})",
        nstring(message.date().map(|date| date.to_rfc822()).as_deref()),
        nstring(message.subject()),
        from,
        sender,
        reply_to,
        format_addresses(message.to()),
        format_addresses(message.cc()),
        format_addresses(message.bcc()),
        nstring(message.in_reply_to().as_text()),
        nstring(message.message_id()),
    )
}

fn format_addresses(addresses: Option<&Address<'_>>) -> String {
    let Some(addresses) = addresses else {
        return "NIL".to_owned();
    };
    let values = addresses
        .iter()
        .filter_map(|address| {
            let email = address.address()?;
            let (mailbox, host) = email.rsplit_once('@').unwrap_or((email, ""));
            Some(format!(
                "({} NIL {} {})",
                nstring(address.name()),
                nstring(Some(mailbox)),
                nstring(Some(host))
            ))
        })
        .collect::<Vec<_>>();
    if values.is_empty() {
        "NIL".to_owned()
    } else {
        format!("({})", values.join(" "))
    }
}

#[derive(Debug)]
struct BodyRequest {
    key: String,
    kind: BodyKind,
    peek: bool,
    start: usize,
    length: Option<usize>,
    partial: bool,
}

#[derive(Debug)]
enum BodyKind {
    Full,
    Header,
    HeaderFields(HashSet<String>),
    HeaderFieldsNot(HashSet<String>),
    Text,
    Part {
        path: Vec<usize>,
        section: PartSection,
    },
}

impl BodyKind {
    fn need(&self) -> Need {
        match self {
            Self::Header | Self::HeaderFields(_) | Self::HeaderFieldsNot(_) => Need::Headers,
            Self::Full | Self::Text | Self::Part { .. } => Need::Mime,
        }
    }
}

#[derive(Debug)]
enum PartSection {
    Content,
    Mime,
    Header,
    Text,
}

/// Parse every body-section item in a FETCH attribute list, in order.
fn parse_body_requests(attributes: &str) -> Vec<BodyRequest> {
    let upper = attributes.to_ascii_uppercase();
    let mut requests = Vec::new();
    let mut cursor = 0;
    while cursor < upper.len() {
        let rest = &upper[cursor..];
        let Some(offset) = rest.find("BODY") else {
            break;
        };
        let start = cursor + offset;
        let after = &upper[start + 4..];
        let (peek, bracket) = if let Some(stripped) = after.strip_prefix(".PEEK[") {
            (true, Some(stripped))
        } else if let Some(stripped) = after.strip_prefix('[') {
            (false, Some(stripped))
        } else {
            (false, None)
        };
        let Some(inside) = bracket else {
            cursor = start + 4;
            continue;
        };
        let Some(close) = inside.find(']') else {
            break;
        };
        let section = &inside[..close];
        let mut tail = inside[close + 1..].trim_start();
        let (mut offset_value, mut length, mut partial) = (0, None, false);
        if let Some(spec) = tail.strip_prefix('<')
            && let Some((inner, remainder)) = spec.split_once('>')
        {
            let mut parts = inner.split('.');
            offset_value = parts
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            length = parts.next().and_then(|value| value.parse().ok());
            partial = true;
            tail = remainder;
        }
        if let Some(kind) = parse_section(section.trim()) {
            requests.push(BodyRequest {
                key: format!("BODY[{section}]"),
                kind,
                peek,
                start: offset_value,
                length,
                partial,
            });
        }
        cursor = upper.len() - tail.len();
    }

    for token in upper
        .split(|character: char| character.is_ascii_whitespace() || matches!(character, '(' | ')'))
    {
        match token {
            "RFC822" => requests.push(BodyRequest::simple("RFC822", BodyKind::Full, false)),
            "RFC822.HEADER" => {
                requests.push(BodyRequest::simple("RFC822.HEADER", BodyKind::Header, true));
            }
            "RFC822.TEXT" => {
                requests.push(BodyRequest::simple("RFC822.TEXT", BodyKind::Text, false))
            }
            _ => {}
        }
    }
    requests
}

fn parse_section(section: &str) -> Option<BodyKind> {
    let field_list = |section: &str| -> HashSet<String> {
        section
            .split_once('(')
            .and_then(|(_, rest)| rest.rsplit_once(')').map(|(names, _)| names))
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    };
    if section.starts_with("HEADER.FIELDS.NOT") {
        Some(BodyKind::HeaderFieldsNot(field_list(section)))
    } else if section.starts_with("HEADER.FIELDS") {
        Some(BodyKind::HeaderFields(field_list(section)))
    } else if section == "HEADER" {
        Some(BodyKind::Header)
    } else if section == "TEXT" {
        Some(BodyKind::Text)
    } else if section.is_empty() {
        Some(BodyKind::Full)
    } else if section.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        parse_part_section(section)
    } else {
        None
    }
}

impl BodyRequest {
    fn simple(key: &str, kind: BodyKind, peek: bool) -> Self {
        Self {
            key: key.to_owned(),
            kind,
            peek,
            start: 0,
            length: None,
            partial: false,
        }
    }
}

fn body_section(raw: &[u8], kind: &BodyKind) -> Vec<u8> {
    let boundary = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| (position + 4, 4))
        .or_else(|| {
            raw.windows(2)
                .position(|window| window == b"\n\n")
                .map(|position| (position + 2, 2))
        });
    match kind {
        BodyKind::Full => raw.to_vec(),
        BodyKind::Header => boundary.map_or_else(|| raw.to_vec(), |(end, _)| raw[..end].to_vec()),
        BodyKind::Text => boundary.map_or_else(Vec::new, |(end, _)| raw[end..].to_vec()),
        BodyKind::HeaderFields(names) => {
            let header = boundary.map_or(raw, |(end, separator)| &raw[..end - separator]);
            filter_headers(header, names, false)
        }
        BodyKind::HeaderFieldsNot(names) => {
            let header = boundary.map_or(raw, |(end, separator)| &raw[..end - separator]);
            filter_headers(header, names, true)
        }
        BodyKind::Part { path, section } => part_section(raw, path, section),
    }
}

fn parse_part_section(section: &str) -> Option<BodyKind> {
    let mut tokens = section.split('.').collect::<Vec<_>>();
    let section = match tokens.last().copied() {
        Some("MIME") => {
            tokens.pop();
            PartSection::Mime
        }
        Some("HEADER") => {
            tokens.pop();
            PartSection::Header
        }
        Some("TEXT") => {
            tokens.pop();
            PartSection::Text
        }
        _ => PartSection::Content,
    };
    let path = tokens
        .into_iter()
        .map(str::parse::<usize>)
        .collect::<std::result::Result<Vec<_>, _>>()
        .ok()?;
    if path.is_empty() || path.contains(&0) {
        return None;
    }
    Some(BodyKind::Part { path, section })
}

fn part_section(raw: &[u8], path: &[usize], section: &PartSection) -> Vec<u8> {
    let Some(message) = MessageParser::default().parse(raw) else {
        return Vec::new();
    };
    let Some(part) = find_part(&message, path) else {
        return Vec::new();
    };
    let header_start = part.raw_header_offset() as usize;
    let body_start = part.raw_body_offset() as usize;
    let end = part.raw_end_offset() as usize;
    match section {
        PartSection::Mime | PartSection::Header => raw
            .get(header_start..body_start)
            .unwrap_or_default()
            .to_vec(),
        PartSection::Content | PartSection::Text => {
            raw.get(body_start..end).unwrap_or_default().to_vec()
        }
    }
}

fn find_part<'a>(message: &'a Message<'_>, path: &[usize]) -> Option<&'a MessagePart<'a>> {
    let root = message.root_part();
    let mut part = if let Some(children) = root.sub_parts() {
        message.part(*children.get(path[0] - 1)?)?
    } else if path[0] == 1 {
        root
    } else {
        return None;
    };
    for number in &path[1..] {
        let children = part.sub_parts()?;
        part = message.part(*children.get(*number - 1)?)?;
    }
    Some(part)
}

fn body_structure(raw: &[u8]) -> String {
    MessageParser::default()
        .parse(raw)
        .map_or_else(|| "NIL".to_owned(), |message| structure_part(&message, 0))
}

fn structure_part(message: &Message<'_>, part_id: u32) -> String {
    let Some(part) = message.part(part_id) else {
        return "NIL".to_owned();
    };
    if let Some(children) = part.sub_parts() {
        let child_structures = children
            .iter()
            .map(|child| structure_part(message, *child))
            .collect::<Vec<_>>()
            .join(" ");
        let subtype = part
            .content_type()
            .and_then(|content_type| content_type.c_subtype.as_deref())
            .unwrap_or("mixed");
        return format!(
            "({child_structures} {} {} {} NIL NIL)",
            imap_quote(&subtype.to_ascii_uppercase()),
            content_parameters(part),
            content_disposition(part)
        );
    }

    let content_type = part.content_type();
    let type_name = content_type
        .map(|value| value.c_type.as_ref())
        .unwrap_or(if part.is_text() {
            "text"
        } else {
            "application"
        });
    let subtype = content_type
        .and_then(|value| value.c_subtype.as_deref())
        .unwrap_or(if part.is_text_html() {
            "html"
        } else if part.is_text() {
            "plain"
        } else {
            "octet-stream"
        });
    let encoding = part.content_transfer_encoding().unwrap_or("7bit");
    let body_start = part.raw_body_offset() as usize;
    let body_end = part.raw_end_offset() as usize;
    let body = message
        .raw_message
        .get(body_start..body_end)
        .unwrap_or_default();
    let lines = body.iter().filter(|byte| **byte == b'\n').count();
    let text_lines = if type_name.eq_ignore_ascii_case("text") {
        format!(" {lines}")
    } else {
        String::new()
    };
    format!(
        "({} {} {} {} {} {} {}{} NIL {} NIL NIL)",
        imap_quote(&type_name.to_ascii_uppercase()),
        imap_quote(&subtype.to_ascii_uppercase()),
        content_parameters(part),
        nstring(part.content_id()),
        nstring(part.content_description()),
        imap_quote(&encoding.to_ascii_uppercase()),
        body.len(),
        text_lines,
        content_disposition(part),
    )
}

fn content_parameters(part: &MessagePart<'_>) -> String {
    let Some(attributes) = part
        .content_type()
        .and_then(|content_type| content_type.attributes.as_ref())
    else {
        return "NIL".to_owned();
    };
    if attributes.is_empty() {
        return "NIL".to_owned();
    }
    let values = attributes
        .iter()
        .map(|attribute| {
            format!(
                "{} {}",
                imap_quote(&attribute.name.to_ascii_uppercase()),
                imap_quote(&attribute.value)
            )
        })
        .collect::<Vec<_>>();
    format!("({})", values.join(" "))
}

fn content_disposition(part: &MessagePart<'_>) -> String {
    let Some(disposition) = part.content_disposition() else {
        return "NIL".to_owned();
    };
    let attributes = disposition
        .attributes
        .as_ref()
        .filter(|attributes| !attributes.is_empty())
        .map(|attributes| {
            let values = attributes
                .iter()
                .map(|attribute| {
                    format!(
                        "{} {}",
                        imap_quote(&attribute.name.to_ascii_uppercase()),
                        imap_quote(&attribute.value)
                    )
                })
                .collect::<Vec<_>>();
            format!("({})", values.join(" "))
        })
        .unwrap_or_else(|| "NIL".to_owned());
    format!(
        "({} {attributes})",
        imap_quote(&disposition.c_type.to_ascii_uppercase())
    )
}

fn filter_headers(raw: &[u8], names: &HashSet<String>, invert: bool) -> Vec<u8> {
    let text = String::from_utf8_lossy(raw);
    let normalized = text.replace("\r\n", "\n");
    let mut result = String::new();
    let mut include = false;
    for line in normalized.lines() {
        if !line.starts_with([' ', '\t']) {
            let name = line
                .split_once(':')
                .map(|(name, _)| name.trim().to_ascii_uppercase());
            let listed = name.as_ref().is_some_and(|name| names.contains(name));
            include = name.is_some() && listed != invert;
        }
        if include {
            result.push_str(line);
            result.push_str("\r\n");
        }
    }
    result.push_str("\r\n");
    result.into_bytes()
}

fn nstring(value: Option<&str>) -> String {
    value.map_or_else(|| "NIL".to_owned(), imap_quote)
}

fn imap_quote(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace(['\r', '\n'], " ")
    )
}

async fn read_command_line(reader: &mut BufReader<ReadHalf<TcpStream>>) -> Result<Option<String>> {
    let Some(mut bytes) = read_bounded_line(reader, MAX_LINE_BYTES)
        .await
        .context("IMAP command line is too long")?
    else {
        return Ok(None);
    };
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.pop();
    }
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

async fn consume_line_end(reader: &mut BufReader<ReadHalf<TcpStream>>) -> Result<()> {
    let mut first = [0; 1];
    reader.read_exact(&mut first).await?;
    if first[0] == b'\r' {
        let mut second = [0; 1];
        reader.read_exact(&mut second).await?;
        if second[0] != b'\n' {
            bail!("invalid APPEND literal terminator");
        }
    } else if first[0] != b'\n' {
        bail!("invalid APPEND literal terminator");
    }
    Ok(())
}

async fn tagged_ok(writer: &mut WriteHalf<TcpStream>, tag: &str, message: &str) -> Result<()> {
    writer.write_all(format_ok(tag, message).as_bytes()).await?;
    Ok(())
}

fn sanitize_error(error: &anyhow::Error) -> String {
    format!("{error:#}")
        .replace(['\r', '\n'], " ")
        .chars()
        .take(300)
        .collect()
}

/// RFC 3501 section 5.1.3 modified UTF-7 for mailbox names.
mod utf7 {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+,";

    pub fn encode(name: &str) -> String {
        let mut output = String::with_capacity(name.len());
        let mut pending: Vec<u16> = Vec::new();
        let flush = |pending: &mut Vec<u16>, output: &mut String| {
            if pending.is_empty() {
                return;
            }
            let bytes: Vec<u8> = pending.iter().flat_map(|unit| unit.to_be_bytes()).collect();
            output.push('&');
            output.push_str(&base64_encode(&bytes));
            output.push('-');
            pending.clear();
        };
        for character in name.chars() {
            if character == '&' {
                flush(&mut pending, &mut output);
                output.push_str("&-");
            } else if (' '..='~').contains(&character) {
                flush(&mut pending, &mut output);
                output.push(character);
            } else {
                let mut units = [0_u16; 2];
                pending.extend_from_slice(character.encode_utf16(&mut units));
            }
        }
        flush(&mut pending, &mut output);
        output
    }

    pub fn decode(wire: &str) -> String {
        let mut output = String::with_capacity(wire.len());
        let mut rest = wire;
        while let Some(index) = rest.find('&') {
            output.push_str(&rest[..index]);
            let after = &rest[index + 1..];
            let Some(end) = after.find('-') else {
                output.push_str(&rest[index..]);
                return output;
            };
            let encoded = &after[..end];
            if encoded.is_empty() {
                output.push('&');
            } else {
                match base64_decode(encoded) {
                    Some(bytes) if bytes.len() % 2 == 0 => {
                        let units: Vec<u16> = bytes
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|pair| u16::from_be_bytes(*pair))
                            .collect();
                        output.push_str(&String::from_utf16_lossy(&units));
                    }
                    _ => output.push_str(&rest[index..index + 1 + end + 1]),
                }
            }
            rest = &after[end + 1..];
        }
        output.push_str(rest);
        output
    }

    fn base64_encode(bytes: &[u8]) -> String {
        let mut output = String::new();
        for chunk in bytes.chunks(3) {
            let mut buffer = [0_u8; 3];
            buffer[..chunk.len()].copy_from_slice(chunk);
            let value = u32::from_be_bytes([0, buffer[0], buffer[1], buffer[2]]);
            let count = chunk.len() + 1;
            for position in 0..count {
                let shift = 18 - 6 * position;
                output.push(ALPHABET[((value >> shift) & 0x3F) as usize] as char);
            }
        }
        output
    }

    fn base64_decode(text: &str) -> Option<Vec<u8>> {
        let mut output = Vec::new();
        let mut buffer = 0_u32;
        let mut bits = 0;
        for byte in text.bytes() {
            let value = ALPHABET.iter().position(|item| *item == byte)? as u32;
            buffer = (buffer << 6) | value;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                output.push(((buffer >> bits) & 0xFF) as u8);
            }
        }
        Some(output)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn round_trips_international_names() {
            for name in [
                "INBOX",
                "Entwürfe",
                "Éléments envoyés",
                "A&B",
                "日本語/Sub",
                "&",
            ] {
                let encoded = encode(name);
                assert!(encoded.is_ascii(), "{encoded}");
                assert_eq!(decode(&encoded), name, "{encoded}");
            }
            assert_eq!(encode("A&B"), "A&-B");
            assert_eq!(encode("Entwürfe"), "Entw&APw-rfe");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_requested_headers() {
        let raw = b"From: a@example.com\r\nSubject: Hi\r\n\tthere\r\nTo: b@example.com\r\n\r\nBody";
        let names = HashSet::from(["SUBJECT".to_owned(), "TO".to_owned()]);
        assert_eq!(
            String::from_utf8(body_section(raw, &BodyKind::HeaderFields(names.clone()))).unwrap(),
            "Subject: Hi\r\n\tthere\r\nTo: b@example.com\r\n\r\n"
        );
        assert_eq!(
            String::from_utf8(body_section(raw, &BodyKind::HeaderFieldsNot(names))).unwrap(),
            "From: a@example.com\r\n\r\n"
        );
    }

    #[test]
    fn parses_partial_body_request() {
        let requests = parse_body_requests("(UID BODY.PEEK[HEADER]<10.20>)");
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert!(request.peek);
        assert_eq!(request.start, 10);
        assert_eq!(request.length, Some(20));
        assert_eq!(request.key, "BODY[HEADER]");
    }

    #[test]
    fn parses_multiple_body_sections() {
        let requests = parse_body_requests(
            "(FLAGS BODY.PEEK[HEADER.FIELDS (From Subject)] BODY[TEXT] RFC822.SIZE)",
        );
        assert_eq!(requests.len(), 2);
        assert!(requests[0].peek);
        assert!(matches!(requests[0].kind, BodyKind::HeaderFields(_)));
        assert_eq!(requests[0].key, "BODY[HEADER.FIELDS (FROM SUBJECT)]");
        assert!(!requests[1].peek);
        assert!(matches!(requests[1].kind, BodyKind::Text));
        assert!(parse_body_requests("(UID RFC822.SIZE BODYSTRUCTURE)").is_empty());
    }

    #[test]
    fn finds_mime_parts_and_formats_structure() {
        let raw = b"From: a@example.com\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nHello\r\n--x\r\nContent-Type: application/pdf\r\nContent-Transfer-Encoding: base64\r\n\r\nYWJj\r\n--x--\r\n";
        let structure = body_structure(raw);
        assert!(structure.contains("\"MIXED\""));
        assert!(structure.contains("\"TEXT\" \"PLAIN\""));
        assert!(structure.contains("\"APPLICATION\" \"PDF\""));
        let request = parse_body_requests("BODY.PEEK[1.MIME]").remove(0);
        let part = body_section(raw, &request.kind);
        assert!(
            String::from_utf8(part)
                .unwrap()
                .contains("Content-Type: text/plain")
        );
    }

    #[test]
    fn quotes_mailbox_names() {
        assert_eq!(imap_quote("A \\\" B"), "\"A \\\\\\\" B\"");
    }

    #[test]
    fn parses_status_with_a_quoted_mailbox() {
        let command = parse_command_compat("a1 STATUS \"Sent Items\" (MESSAGES UIDNEXT)").unwrap();
        assert!(matches!(
            command.command,
            ImapCommand::Status { mailbox, .. } if mailbox == "Sent Items"
        ));
    }

    #[test]
    fn matches_imap_mailbox_patterns() {
        assert!(mailbox_pattern_matches("Projects/Rust", "*"));
        assert!(mailbox_pattern_matches("Projects", "%"));
        assert!(!mailbox_pattern_matches("Projects/Rust", "%"));
        assert!(mailbox_pattern_matches("Projects/Rust", "Projects/%"));
        assert!(mailbox_pattern_matches("INBOX", "inbox"));
    }

    #[test]
    fn uid_range_to_star_includes_highest_uid() {
        let summary: MessageSummary = serde_json::from_str(r#"{"id":"a"}"#).unwrap();
        let selected = SelectedMailbox {
            id: "box".into(),
            messages: vec![
                SelectedMessage {
                    summary: summary.clone(),
                    uid: 3,
                    deleted: false,
                },
                SelectedMessage {
                    summary,
                    uid: 9,
                    deleted: false,
                },
            ],
            read_only: false,
            from_store: true,
        };
        assert_eq!(selected_indices(&selected, "100:*", true).unwrap(), vec![1]);
        assert_eq!(
            selected_indices(&selected, "1:*", true).unwrap(),
            vec![0, 1]
        );
        assert_eq!(selected_indices(&selected, "2", false).unwrap(), vec![1]);
    }
}
