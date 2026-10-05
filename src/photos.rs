// SPDX-License-Identifier: GPL-2.0-or-later

//! Profile pictures Microsoft Graph knows about, so an IMAP client can show
//! Outlook avatars without owning a Graph token itself. Served by the
//! loopback HTTP server as `GET /photo?address=<email>`.
//!
//! The account's own address maps to `/me/photo`; anything else is looked
//! up as a tenant user, then as a personal contact.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::Mutex;

use crate::graph::Photo;
use crate::service::AccountRuntime;

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

/// The picture for `address`, from the cache or from Graph.
pub async fn photo(
    account: &AccountRuntime,
    cache: &PhotoCache,
    address: &str,
) -> Result<Option<Photo>> {
    let key = (account.config.name.to_ascii_lowercase(), address.to_owned());
    if let Some(cached) = cache.get(&key).await {
        return Ok(cached);
    }
    let photo = lookup(account, address).await?;
    cache.put(key, photo.clone()).await;
    Ok(photo)
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
