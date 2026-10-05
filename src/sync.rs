// SPDX-License-Identifier: GPL-2.0-or-later

//! Background synchronisation of the local message index with Microsoft
//! Graph delta queries. One task per account walks every folder: the first
//! round lists the folder in full (resumably, page by page), later rounds
//! fetch only what changed since the stored delta link.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::sync::broadcast;

use crate::config::SyncConfig;
use crate::graph::{
    GraphClient, MailFolder, MessageSummary, WELL_KNOWN_FOLDERS, is_resync_required,
};
use crate::service::{AccountRuntime, Runtime};
use crate::store::{Store, StoredFolder, SyncState};

/// Folders polled at the fast interval and listed first.
const HOT_FOLDERS: &[&str] = &["\\Inbox", "\\Sent", "\\Drafts"];
/// Bodies prefetched per cycle when `download_bodies` is on.
const PREFETCH_BATCH: usize = 100;

/// Published on the runtime's broadcast channel whenever a folder's index
/// changed, so idling IMAP sessions can push updates promptly.
#[derive(Clone, Debug)]
pub struct FolderChange {
    pub account: String,
    pub folder_id: String,
}

pub fn spawn(runtime: Arc<Runtime>, account: Arc<AccountRuntime>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run(runtime, account).await;
    })
}

async fn run(runtime: Arc<Runtime>, account: Arc<AccountRuntime>) {
    let config = &runtime.config.sync;
    let name = account.config.name.as_str();
    let folder_poll = Duration::from_secs(config.folder_poll_secs);
    let inbox_poll = Duration::from_secs(config.inbox_poll_secs);
    let mut last_folder_refresh: Option<Instant> = None;
    let mut last_synced: HashMap<String, Instant> = HashMap::new();
    let mut backoff = Duration::from_secs(30);
    tracing::info!(account = name, "mailbox sync started");
    loop {
        let mut failed = false;
        if last_folder_refresh.is_none_or(|at| at.elapsed() >= folder_poll) {
            match refresh_folders(&runtime, &account).await {
                Ok(()) => last_folder_refresh = Some(Instant::now()),
                Err(error) => {
                    tracing::warn!(account = name, %error, "folder list refresh failed");
                    failed = true;
                }
            }
        }
        let folders = match runtime.store.folders(name) {
            Ok(folders) => order_folders(folders),
            Err(error) => {
                tracing::warn!(account = name, %error, "could not read folder list");
                Vec::new()
            }
        };
        for folder in &folders {
            let state = runtime
                .store
                .sync_state(name, &folder.id)
                .unwrap_or_default();
            let interval = if is_hot(folder) {
                inbox_poll
            } else {
                folder_poll
            };
            let due = !state.full_sync_done
                || last_synced
                    .get(&folder.id)
                    .is_none_or(|at| at.elapsed() >= interval);
            if !due {
                continue;
            }
            match sync_folder(&runtime, &account, folder, config).await {
                Ok(()) => {
                    last_synced.insert(folder.id.clone(), Instant::now());
                    if let Err(error) = backfill_sizes(&runtime, &account, folder, config).await {
                        tracing::debug!(account = name, folder = %folder.display_name, %error, "size backfill failed");
                    }
                    if let Err(error) = refresh_pins(&runtime, &account, folder, config).await {
                        tracing::debug!(account = name, folder = %folder.display_name, %error, "pin refresh failed");
                    }
                }
                Err(error) => {
                    failed = true;
                    tracing::warn!(account = name, folder = %folder.display_name, error = format!("{error:#}"), "folder sync failed");
                    let mut state = runtime
                        .store
                        .sync_state(name, &folder.id)
                        .unwrap_or_default();
                    state.last_error = Some(format!("{error:#}"));
                    let _ = runtime.store.save_sync_state(name, &folder.id, &state);
                    // Retry failing folders on the next cycle rather than spinning.
                    last_synced.insert(folder.id.clone(), Instant::now());
                }
            }
        }
        if config.download_bodies
            && let Err(error) = prefetch_bodies(&runtime, &account, &folders, config).await
        {
            tracing::warn!(account = name, %error, "body prefetch failed");
        }
        let pause = if failed {
            let pause = backoff;
            backoff = (backoff * 2).min(Duration::from_secs(900));
            pause
        } else {
            backoff = Duration::from_secs(30);
            inbox_poll
        };
        tokio::time::sleep(pause).await;
    }
}

fn is_hot(folder: &StoredFolder) -> bool {
    folder
        .special_use
        .as_deref()
        .is_some_and(|flag| HOT_FOLDERS.contains(&flag))
}

/// Hot folders first, then the rest smallest-first so most folders become
/// fully usable early in the initial sync.
fn order_folders(mut folders: Vec<StoredFolder>) -> Vec<StoredFolder> {
    folders.sort_by_key(|folder| {
        let hot_rank = HOT_FOLDERS
            .iter()
            .position(|flag| folder.special_use.as_deref() == Some(flag))
            .unwrap_or(HOT_FOLDERS.len());
        (hot_rank, folder.total_item_count)
    });
    folders
}

/// Walk the folder tree on Graph and replace the stored list.
async fn refresh_folders(runtime: &Runtime, account: &AccountRuntime) -> Result<()> {
    let folders = fetch_folder_tree(&account.graph).await?;
    let vanished = runtime
        .store
        .replace_folders(&account.config.name, &folders)?;
    for folder_id in vanished {
        notify(runtime, &account.config.name, &folder_id);
    }
    Ok(())
}

/// Every folder in the mailbox, with SPECIAL-USE flags resolved through
/// Graph's locale-independent well-known names.
pub async fn fetch_folder_tree(graph: &GraphClient) -> Result<Vec<StoredFolder>> {
    let mut special_ids: HashMap<String, &'static str> = HashMap::new();
    for (well_known, flag) in WELL_KNOWN_FOLDERS {
        match graph.well_known_folder(well_known).await {
            Ok(folder) => {
                special_ids.insert(folder.id, flag);
            }
            Err(error) => {
                tracing::debug!(%well_known, %error, "well-known folder unavailable");
            }
        }
    }
    let mut folders = Vec::new();
    let mut stack: Vec<MailFolder> = graph.folders().await?;
    while let Some(folder) = stack.pop() {
        if folder.child_folder_count > 0 {
            stack.extend(graph.child_folders(&folder.id).await?);
        }
        let mut stored = StoredFolder::from(&folder);
        stored.special_use = special_ids.get(&folder.id).map(|flag| (*flag).to_owned());
        folders.push(stored);
    }
    Ok(folders)
}

/// Run one delta round for a folder, persisting progress after every page.
async fn sync_folder(
    runtime: &Runtime,
    account: &AccountRuntime,
    folder: &StoredFolder,
    config: &SyncConfig,
) -> Result<()> {
    let store = &runtime.store;
    let name = account.config.name.as_str();
    let mut state = store.sync_state(name, &folder.id)?;
    let initial = GraphClient::messages_delta_url(&folder.id);
    let mut url = state
        .next_link
        .clone()
        .or_else(|| state.delta_link.clone())
        .unwrap_or_else(|| initial.clone());
    let mut tried_plain = false;
    let mut pages = 0usize;
    loop {
        let page = match account
            .graph
            .delta_page::<MessageSummary>(&url, config.page_size)
            .await
        {
            Ok(page) => page,
            Err(error) if is_resync_required(&error) => {
                tracing::warn!(account = name, folder = %folder.display_name, "delta token expired; re-listing folder");
                store.clear_mailbox(name, &folder.id, false)?;
                state = SyncState::default();
                url = initial.clone();
                notify(runtime, name, &folder.id);
                continue;
            }
            Err(error) if !tried_plain && url == initial && rejects_expand(&error) => {
                tracing::info!(
                    account = name,
                    "Graph rejected $expand on delta; syncing without message sizes"
                );
                tried_plain = true;
                url = GraphClient::messages_delta_url_plain(&folder.id);
                continue;
            }
            Err(error) => return Err(error).context("delta page request failed"),
        };
        pages += 1;
        let (removed, upserts): (Vec<_>, Vec<_>) = page
            .value
            .into_iter()
            .partition(|message| message.removed.is_some());
        let changed = !removed.is_empty() || !upserts.is_empty();
        if !upserts.is_empty() {
            store.upsert_messages(name, &folder.id, &upserts)?;
            state.synced_count += upserts.len() as u64;
        }
        if !removed.is_empty() {
            let ids: Vec<String> = removed.into_iter().map(|message| message.id).collect();
            store.remove_messages(name, &folder.id, &ids)?;
        }
        state.last_error = None;
        match (page.next_link, page.delta_link) {
            (Some(next), _) => {
                state.next_link = Some(next.clone());
                store.save_sync_state(name, &folder.id, &state)?;
                if changed {
                    notify(runtime, name, &folder.id);
                }
                url = next;
                tokio::time::sleep(Duration::from_millis(config.page_delay_ms)).await;
            }
            (None, Some(delta)) => {
                let first_completion = !state.full_sync_done;
                state.next_link = None;
                state.delta_link = Some(delta);
                state.full_sync_done = true;
                state.last_sync = Some(chrono::Utc::now().timestamp());
                store.save_sync_state(name, &folder.id, &state)?;
                if changed {
                    notify(runtime, name, &folder.id);
                }
                if first_completion {
                    tracing::info!(account = name, folder = %folder.display_name, messages = state.synced_count, pages, "initial folder sync complete");
                } else if changed {
                    tracing::debug!(account = name, folder = %folder.display_name, "folder changes applied");
                }
                return Ok(());
            }
            (None, None) => bail!("delta page carried neither nextLink nor deltaLink"),
        }
    }
}

/// Delta responses cannot carry the MAPI size property, so page the compact
/// list endpoint (newest first) until every indexed message has a size. New
/// arrivals sit on the first page, so steady-state cost is one request.
async fn backfill_sizes(
    runtime: &Runtime,
    account: &AccountRuntime,
    folder: &StoredFolder,
    config: &SyncConfig,
) -> Result<()> {
    let store = &runtime.store;
    let name = account.config.name.as_str();
    if !store.sync_state(name, &folder.id)?.full_sync_done {
        return Ok(());
    }
    let mut remaining = store.missing_size_count(name, &folder.id)?;
    if remaining == 0 {
        return Ok(());
    }
    let mut url = GraphClient::sizes_url(&folder.id);
    loop {
        let (sizes, next) = account.graph.sizes_page(&url).await?;
        let changed = store.set_sizes(name, &sizes)? as u64;
        remaining = remaining.saturating_sub(changed);
        match next {
            Some(next) if remaining > 0 => {
                url = next;
                tokio::time::sleep(Duration::from_millis(config.page_delay_ms)).await;
            }
            _ => return Ok(()),
        }
    }
}

/// Delta responses cannot carry the pin marker either, and a pin changes
/// nothing else about a message, so ask Graph for the folder's pinned set
/// each cycle (one request: pins are few) and mirror it into the index.
async fn refresh_pins(
    runtime: &Runtime,
    account: &AccountRuntime,
    folder: &StoredFolder,
    config: &SyncConfig,
) -> Result<()> {
    let store = &runtime.store;
    let name = account.config.name.as_str();
    if !store.sync_state(name, &folder.id)?.full_sync_done {
        return Ok(());
    }
    let mut pinned = Vec::new();
    let mut url = GraphClient::pinned_url(&folder.id);
    loop {
        let (ids, next) = account.graph.pinned_page(&url).await?;
        pinned.extend(ids);
        match next {
            Some(next) => {
                url = next;
                tokio::time::sleep(Duration::from_millis(config.page_delay_ms)).await;
            }
            None => break,
        }
    }
    if store.set_pinned_messages(name, &folder.id, &pinned)? {
        tracing::debug!(account = name, folder = %folder.display_name, pinned = pinned.len(), "pinned set changed");
        notify(runtime, name, &folder.id);
    }
    Ok(())
}

fn rejects_expand(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}");
    text.contains("400") && text.to_ascii_lowercase().contains("expand")
}

/// Download bodies newest-first for fully synced folders until the cache is
/// near its cap, a bounded batch per cycle so polling keeps its cadence.
async fn prefetch_bodies(
    runtime: &Runtime,
    account: &AccountRuntime,
    folders: &[StoredFolder],
    config: &SyncConfig,
) -> Result<()> {
    let store: &Store = &runtime.store;
    let name = account.config.name.as_str();
    let cap = config.body_cache_max_bytes();
    let mut remaining = PREFETCH_BATCH;
    for folder in folders {
        if remaining == 0 {
            break;
        }
        if !store.sync_state(name, &folder.id)?.full_sync_done {
            continue;
        }
        let ids = store.uncached_message_ids(name, &folder.id, remaining)?;
        for id in ids {
            let (used, _) = store.body_cache_stats()?;
            if used >= cap / 100 * 95 {
                tracing::info!(account = name, "body cache is full; prefetch paused");
                return Ok(());
            }
            match account.graph.mime(&id).await {
                Ok(mime) => store.body_put(name, &id, &mime, cap)?,
                Err(error) => {
                    tracing::debug!(account = name, %error, "body prefetch skipped a message")
                }
            }
            remaining = remaining.saturating_sub(1);
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    Ok(())
}

fn notify(runtime: &Runtime, account: &str, folder_id: &str) {
    // Nobody idling is not an error.
    let _ = runtime.changes.send(FolderChange {
        account: account.to_owned(),
        folder_id: folder_id.to_owned(),
    });
}

/// Subscribe to folder change notifications.
pub fn subscribe(runtime: &Runtime) -> broadcast::Receiver<FolderChange> {
    runtime.changes.subscribe()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(id: &str, special: Option<&str>, total: u32) -> StoredFolder {
        StoredFolder {
            id: id.into(),
            display_name: id.into(),
            parent_id: None,
            child_folder_count: 0,
            total_item_count: total,
            unread_item_count: 0,
            special_use: special.map(str::to_owned),
        }
    }

    #[test]
    fn hot_folders_come_first_then_smallest() {
        let ordered = order_folders(vec![
            folder("big", None, 5000),
            folder("sent", Some("\\Sent"), 100),
            folder("small", None, 3),
            folder("inbox", Some("\\Inbox"), 9000),
            folder("junk", Some("\\Junk"), 1),
        ]);
        let ids: Vec<&str> = ordered.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["inbox", "sent", "junk", "small", "big"]);
    }

    #[test]
    fn expand_rejection_is_detected_from_error_text() {
        assert!(rejects_expand(&anyhow::anyhow!(
            "Microsoft Graph returned 400 Bad Request: The query parameter '$expand' is not supported"
        )));
        assert!(!rejects_expand(&anyhow::anyhow!(
            "Microsoft Graph returned 503"
        )));
    }
}
