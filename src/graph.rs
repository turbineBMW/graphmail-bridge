// SPDX-License-Identifier: GPL-2.0-or-later

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::oauth::TokenManager;

const GRAPH_ROOT: &str = "https://graph.microsoft.com/v1.0";
const IMMUTABLE_ID: &str = "IdType=\"ImmutableId\"";

#[derive(Clone)]
pub struct GraphClient {
    token_manager: Arc<TokenManager>,
    http: reqwest::Client,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserProfile {
    pub display_name: Option<String>,
    pub mail: Option<String>,
    pub user_principal_name: String,
}

/// A profile picture as Graph serves it: raw image bytes and their type.
#[derive(Clone, Debug)]
pub struct Photo {
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// A Microsoft 365 calendar event in the subset used by local calendar clients.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarEvent {
    pub id: String,
    #[serde(default, rename = "iCalUId")]
    pub i_cal_uid: Option<String>,
    #[serde(default)]
    pub subject: Option<String>,
    pub start: GraphDateTime,
    pub end: GraphDateTime,
    #[serde(default)]
    pub is_all_day: bool,
    #[serde(default)]
    pub location: Option<EventLocation>,
    #[serde(default)]
    pub web_link: Option<String>,
    #[serde(default)]
    pub online_meeting: Option<OnlineMeeting>,
    #[serde(default)]
    pub online_meeting_url: Option<String>,
    #[serde(default)]
    pub response_status: Option<ResponseStatus>,
    #[serde(default)]
    pub show_as: Option<String>,
    #[serde(default)]
    pub is_cancelled: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphDateTime {
    pub date_time: String,
    pub time_zone: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventLocation {
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OnlineMeeting {
    #[serde(default)]
    pub join_url: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponseStatus {
    #[serde(default)]
    pub response: Option<String>,
}

/// Photos larger than this are refused rather than buffered; Graph's own
/// pictures are a few hundred KB at most.
pub const MAX_PHOTO_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailFolder {
    pub id: String,
    pub display_name: String,
    pub parent_folder_id: Option<String>,
    #[serde(default)]
    pub child_folder_count: u32,
    #[serde(default)]
    pub total_item_count: u32,
    #[serde(default)]
    pub unread_item_count: u32,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageSummary {
    pub id: String,
    pub subject: Option<String>,
    pub received_date_time: Option<String>,
    pub sent_date_time: Option<String>,
    #[serde(default)]
    pub is_read: bool,
    #[serde(default)]
    pub has_attachments: bool,
    pub internet_message_id: Option<String>,
    #[serde(default)]
    pub is_draft: bool,
    #[serde(default)]
    pub flag: Option<FollowupFlag>,
    #[serde(default)]
    pub from: Option<Recipient>,
    #[serde(default)]
    pub to_recipients: Vec<Recipient>,
    #[serde(default)]
    pub cc_recipients: Vec<Recipient>,
    #[serde(default)]
    pub bcc_recipients: Vec<Recipient>,
    #[serde(default)]
    pub body_preview: Option<String>,
    /// Present on delta entries for messages that were deleted or moved away.
    #[serde(default, rename = "@removed")]
    pub removed: Option<serde_json::Value>,
    /// Size restored from the local store (not part of the Graph payload).
    #[serde(skip)]
    pub stored_size: Option<u64>,
    /// Pin state restored from the local store (not part of the Graph payload).
    #[serde(skip)]
    pub stored_pinned: Option<bool>,
    #[serde(default)]
    pub(crate) single_value_extended_properties: Vec<ExtendedProperty>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Recipient {
    #[serde(default)]
    pub email_address: EmailAddress,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmailAddress {
    pub name: Option<String>,
    pub address: Option<String>,
}

/// One page of a delta round: either more pages follow (`next_link`) or the
/// round is complete and `delta_link` starts the next one.
#[derive(Debug, Deserialize)]
pub struct DeltaPage<T> {
    pub value: Vec<T>,
    #[serde(rename = "@odata.nextLink")]
    pub next_link: Option<String>,
    #[serde(rename = "@odata.deltaLink")]
    pub delta_link: Option<String>,
}

/// Graph answered 410 Gone: the delta token expired and the folder must be
/// re-listed from scratch.
#[derive(Debug)]
pub struct ResyncRequired;

impl std::fmt::Display for ResyncRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Microsoft Graph requires a full resync of this folder")
    }
}

impl std::error::Error for ResyncRequired {}

/// Whether an error chain contains [`ResyncRequired`].
pub fn is_resync_required(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<ResyncRequired>())
}

impl MessageSummary {
    /// Message size in bytes from the `PidTagMessageSize` MAPI property, when
    /// Graph returned it.
    pub fn size(&self) -> Option<u64> {
        if let Some(size) = self.stored_size {
            return Some(size);
        }
        self.single_value_extended_properties
            .iter()
            .find(|property| property.id.eq_ignore_ascii_case(MESSAGE_SIZE_PROPERTY))
            .and_then(|property| property.value.parse().ok())
    }

    pub fn is_flagged(&self) -> bool {
        self.flag
            .as_ref()
            .is_some_and(|flag| flag.flag_status.eq_ignore_ascii_case("flagged"))
    }

    /// Whether Outlook shows the message pinned at the top of its folder,
    /// when the payload says: from the local index, or from the MAPI
    /// property Outlook sets to a far-future sentinel on pin. `None` when
    /// neither is present (a delta entry), so a partial update never
    /// clobbers a known state.
    pub fn pinned(&self) -> Option<bool> {
        if let Some(pinned) = self.stored_pinned {
            return Some(pinned);
        }
        self.single_value_extended_properties
            .iter()
            .find(|property| property.id.eq_ignore_ascii_case(PIN_PROPERTY))
            .map(|property| is_pinned_value(&property.value))
    }

    pub fn is_pinned(&self) -> bool {
        self.pinned().unwrap_or(false)
    }
}

/// Outlook pins a message by setting its renew time to 1 September 4500;
/// anything in that century counts, in case the exact sentinel ever shifts.
fn is_pinned_value(value: &str) -> bool {
    value
        .get(..4)
        .and_then(|year| year.parse::<u32>().ok())
        .is_some_and(|year| year >= PINNED_YEAR)
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FollowupFlag {
    #[serde(default)]
    pub flag_status: String,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct ExtendedProperty {
    id: String,
    value: String,
}

const MESSAGE_SIZE_PROPERTY: &str = "Integer 0xe08";
/// `PidTagRenewTime2`-style read-side pin marker: Outlook derives it from
/// the writable `0x0F01` and OWA sorts on it.
const PIN_PROPERTY: &str = "SystemTime 0xf02";
/// The property a client writes to pin (`4500-09-01T00:00:00Z`) or unpin
/// (delete it); Exchange mirrors the value into [`PIN_PROPERTY`].
const PIN_WRITE_PROPERTY: &str = "SystemTime 0x0F01";
const PINNED_SENTINEL: &str = "4500-09-01T00:00:00Z";
const PINNED_YEAR: u32 = 4500;
/// Extended properties requested with message listings: the MAPI size and
/// the pin marker. Delta requests may reject `$expand` (see `sync`).
const EXPAND_PROPERTIES: &str = "$expand=singleValueExtendedProperties($filter=id%20eq%20'Integer%200x0E08'%20or%20id%20eq%20'SystemTime%200x0F02')";
/// Only messages whose pin marker holds the far-future sentinel.
const PINNED_FILTER: &str = "$filter=singleValueExtendedProperties/Any(ep:%20ep/id%20eq%20'SystemTime%200x0F02'%20and%20cast(ep/value,%20Edm.DateTimeOffset)%20ge%204500-01-01T00:00:00Z)";

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InternetMessageHeader {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MessageHeaders {
    #[serde(default)]
    internet_message_headers: Vec<InternetMessageHeader>,
}

/// Graph well-known folder names that map to IMAP SPECIAL-USE attributes.
pub const WELL_KNOWN_FOLDERS: &[(&str, &str)] = &[
    ("inbox", "\\Inbox"),
    ("sentitems", "\\Sent"),
    ("drafts", "\\Drafts"),
    ("deleteditems", "\\Trash"),
    ("junkemail", "\\Junk"),
    ("archive", "\\Archive"),
];

#[derive(Debug, Deserialize)]
struct GraphPage<T> {
    value: Vec<T>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ContactId {
    id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReadState {
    is_read: bool,
}

#[derive(Serialize)]
struct FlagState<'a> {
    flag: FlagStatus<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FlagStatus<'a> {
    flag_status: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PinState<'a> {
    single_value_extended_properties: [ExtendedPropertyWrite<'a>; 1],
}

/// A `null` value deletes the property, which is how a pin is undone.
#[derive(Serialize)]
struct ExtendedPropertyWrite<'a> {
    id: &'a str,
    value: Option<&'a str>,
}

#[derive(Serialize)]
struct Destination<'a> {
    #[serde(rename = "destinationId")]
    destination_id: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FolderName<'a> {
    display_name: &'a str,
}

impl GraphClient {
    pub fn new(token_manager: Arc<TokenManager>) -> Self {
        Self {
            token_manager,
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(20))
                .timeout(Duration::from_secs(120))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("HTTP client construction should succeed"),
        }
    }

    pub async fn profile(&self) -> Result<UserProfile> {
        self.get_json(&format!(
            "{GRAPH_ROOT}/me?$select=displayName,mail,userPrincipalName"
        ))
        .await
    }

    /// Return every occurrence in a UTC time window. `calendarView` expands
    /// recurring series, and Graph's next links are followed until complete.
    pub async fn calendar_view(&self, start: &str, end: &str) -> Result<Vec<CalendarEvent>> {
        self.get_all(calendar_view_url(start, end)).await
    }

    pub async fn folders(&self) -> Result<Vec<MailFolder>> {
        let url = format!(
            "{GRAPH_ROOT}/me/mailFolders?$top=100&$select=id,displayName,parentFolderId,childFolderCount,totalItemCount,unreadItemCount&includeHiddenFolders=true"
        );
        self.get_all(url).await
    }

    pub async fn child_folders(&self, folder_id: &str) -> Result<Vec<MailFolder>> {
        let url = format!(
            "{GRAPH_ROOT}/me/mailFolders/{}/childFolders?$top=100&$select=id,displayName,parentFolderId,childFolderCount,totalItemCount,unreadItemCount&includeHiddenFolders=true",
            encode_segment(folder_id)
        );
        self.get_all(url).await
    }

    pub async fn folder(&self, folder_id: &str) -> Result<MailFolder> {
        self.well_known_folder(folder_id).await
    }

    pub async fn well_known_folder(&self, name: &str) -> Result<MailFolder> {
        let url = format!(
            "{GRAPH_ROOT}/me/mailFolders/{}?$select=id,displayName,parentFolderId,childFolderCount,totalItemCount,unreadItemCount",
            encode_segment(name)
        );
        self.get_json(&url).await
    }

    pub async fn messages(&self, folder_id: &str, limit: usize) -> Result<Vec<MessageSummary>> {
        let page_size = limit.clamp(1, 1000);
        let url = format!(
            "{GRAPH_ROOT}/me/mailFolders/{}/messages?$top={page_size}&$orderby=receivedDateTime%20desc&$select=id,subject,receivedDateTime,sentDateTime,isRead,hasAttachments,internetMessageId,isDraft,flag&{EXPAND_PROPERTIES}",
            encode_segment(folder_id)
        );
        let mut messages: Vec<MessageSummary> = self.get_limited(url, limit).await?;
        messages.reverse();
        Ok(messages)
    }

    /// The first-request URL of a delta round for a folder. Later pages and
    /// rounds reuse the links Graph hands back, which carry these options.
    pub fn messages_delta_url(folder_id: &str) -> String {
        format!(
            "{GRAPH_ROOT}/me/mailFolders/{}/messages/delta?$orderby=receivedDateTime%20desc&$select=id,subject,receivedDateTime,sentDateTime,isRead,hasAttachments,internetMessageId,isDraft,flag,from,toRecipients,ccRecipients,bccRecipients,bodyPreview&{EXPAND_PROPERTIES}",
            encode_segment(folder_id)
        )
    }

    /// Same as [`Self::messages_delta_url`] without the size property, for
    /// tenants that reject `$expand` on delta.
    pub fn messages_delta_url_plain(folder_id: &str) -> String {
        format!(
            "{GRAPH_ROOT}/me/mailFolders/{}/messages/delta?$orderby=receivedDateTime%20desc&$select=id,subject,receivedDateTime,sentDateTime,isRead,hasAttachments,internetMessageId,isDraft,flag,from,toRecipients,ccRecipients,bccRecipients,bodyPreview",
            encode_segment(folder_id)
        )
    }

    /// First page of a compact `id` + size listing (newest first), for
    /// backfilling sizes that delta responses cannot carry.
    pub fn sizes_url(folder_id: &str) -> String {
        format!(
            "{GRAPH_ROOT}/me/mailFolders/{}/messages?$top=500&$orderby=receivedDateTime%20desc&$select=id&{EXPAND_PROPERTIES}",
            encode_segment(folder_id)
        )
    }

    /// First page of the ids of a folder's pinned messages (newest first).
    pub fn pinned_url(folder_id: &str) -> String {
        format!(
            "{GRAPH_ROOT}/me/mailFolders/{}/messages?$top=500&$orderby=receivedDateTime%20desc&$select=id&{PINNED_FILTER}",
            encode_segment(folder_id)
        )
    }

    /// One page of pinned message ids plus the next page's URL.
    pub async fn pinned_page(&self, url: &str) -> Result<(Vec<String>, Option<String>)> {
        let page: GraphPage<MessageSummary> = self.get_json(url).await?;
        let ids = page.value.into_iter().map(|message| message.id).collect();
        Ok((ids, page.next_link))
    }

    /// One page of `(message id, size)` pairs plus the next page's URL.
    pub async fn sizes_page(&self, url: &str) -> Result<(Vec<(String, u64)>, Option<String>)> {
        let page: GraphPage<MessageSummary> = self.get_json(url).await?;
        let sizes = page
            .value
            .into_iter()
            .filter_map(|message| message.size().map(|size| (message.id, size)))
            .collect();
        Ok((sizes, page.next_link))
    }

    /// Fetch one delta page. Fails with [`ResyncRequired`] in the error chain
    /// when Graph reports the token as expired.
    pub async fn delta_page<T: DeserializeOwned>(
        &self,
        url: &str,
        max_page_size: u32,
    ) -> Result<DeltaPage<T>> {
        let request = self
            .request(Method::GET, url)
            .await?
            .header("Prefer", format!("odata.maxpagesize={max_page_size}"));
        let response = self.send(request).await?;
        decode_json(response).await
    }

    pub async fn mime(&self, message_id: &str) -> Result<Vec<u8>> {
        let url = format!(
            "{GRAPH_ROOT}/me/messages/{}/$value",
            encode_segment(message_id)
        );
        let request = self.request(Method::GET, &url).await?;
        let response = self.send(request).await?;
        Ok(response.bytes().await?.to_vec())
    }

    /// Fetch only the RFC 5322 header block of a message, reconstructed from
    /// Graph's `internetMessageHeaders`. Returns `None` when Graph has no
    /// headers for the item (for example an unsent draft); callers should fall
    /// back to [`GraphClient::mime`] in that case.
    pub async fn headers(&self, message_id: &str) -> Result<Option<Vec<u8>>> {
        let url = format!(
            "{GRAPH_ROOT}/me/messages/{}?$select=internetMessageHeaders",
            encode_segment(message_id)
        );
        let headers: MessageHeaders = self.get_json(&url).await?;
        if headers.internet_message_headers.is_empty() {
            return Ok(None);
        }
        let mut raw = Vec::new();
        for header in headers.internet_message_headers {
            if header.name.is_empty()
                || header.name.contains([':', '\r', '\n'])
                || header.name.as_bytes().iter().any(u8::is_ascii_whitespace)
            {
                continue;
            }
            raw.extend_from_slice(header.name.as_bytes());
            raw.extend_from_slice(b": ");
            raw.extend_from_slice(
                header
                    .value
                    .replace("\r\n", "\n")
                    .replace('\n', "\r\n\t")
                    .as_bytes(),
            );
            raw.extend_from_slice(b"\r\n");
        }
        raw.extend_from_slice(b"\r\n");
        Ok(Some(raw))
    }

    pub async fn send_mime(&self, message: &[u8]) -> Result<()> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(message);
        let request = self
            .request(Method::POST, &format!("{GRAPH_ROOT}/me/sendMail"))
            .await?
            .header(reqwest::header::CONTENT_TYPE, "text/plain")
            .body(encoded);
        self.send(request).await?;
        Ok(())
    }

    pub async fn create_mime(&self, folder_id: &str, message: &[u8]) -> Result<MessageSummary> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(message);
        let url = format!(
            "{GRAPH_ROOT}/me/mailFolders/{}/messages",
            encode_segment(folder_id)
        );
        let request = self
            .request(Method::POST, &url)
            .await?
            .header(reqwest::header::CONTENT_TYPE, "text/plain")
            .body(encoded);
        decode_json(self.send(request).await?).await
    }

    pub async fn set_read(&self, message_id: &str, is_read: bool) -> Result<()> {
        let url = format!("{GRAPH_ROOT}/me/messages/{}", encode_segment(message_id));
        let request = self
            .request(Method::PATCH, &url)
            .await?
            .json(&ReadState { is_read });
        self.send(request).await?;
        Ok(())
    }

    pub async fn set_flagged(&self, message_id: &str, flagged: bool) -> Result<()> {
        let url = format!("{GRAPH_ROOT}/me/messages/{}", encode_segment(message_id));
        let request = self.request(Method::PATCH, &url).await?.json(&FlagState {
            flag: FlagStatus {
                flag_status: if flagged { "flagged" } else { "notFlagged" },
            },
        });
        self.send(request).await?;
        Ok(())
    }

    /// Pin or unpin a message the way Outlook on the web does.
    pub async fn set_pinned(&self, message_id: &str, pinned: bool) -> Result<()> {
        let url = format!("{GRAPH_ROOT}/me/messages/{}", encode_segment(message_id));
        let request = self.request(Method::PATCH, &url).await?.json(&PinState {
            single_value_extended_properties: [ExtendedPropertyWrite {
                id: PIN_WRITE_PROPERTY,
                value: pinned.then_some(PINNED_SENTINEL),
            }],
        });
        self.send(request).await?;
        Ok(())
    }

    pub async fn move_message(&self, message_id: &str, folder_id: &str) -> Result<MessageSummary> {
        let url = format!(
            "{GRAPH_ROOT}/me/messages/{}/move",
            encode_segment(message_id)
        );
        let request = self.request(Method::POST, &url).await?.json(&Destination {
            destination_id: folder_id,
        });
        decode_json(self.send(request).await?).await
    }

    pub async fn copy_message(&self, message_id: &str, folder_id: &str) -> Result<MessageSummary> {
        let url = format!(
            "{GRAPH_ROOT}/me/messages/{}/copy",
            encode_segment(message_id)
        );
        let request = self.request(Method::POST, &url).await?.json(&Destination {
            destination_id: folder_id,
        });
        decode_json(self.send(request).await?).await
    }

    pub async fn delete_message(&self, message_id: &str) -> Result<()> {
        let url = format!("{GRAPH_ROOT}/me/messages/{}", encode_segment(message_id));
        let request = self.request(Method::DELETE, &url).await?;
        self.send(request).await?;
        Ok(())
    }

    pub async fn create_folder(
        &self,
        parent_id: Option<&str>,
        display_name: &str,
    ) -> Result<MailFolder> {
        let url = match parent_id {
            Some(parent) => format!(
                "{GRAPH_ROOT}/me/mailFolders/{}/childFolders",
                encode_segment(parent)
            ),
            None => format!("{GRAPH_ROOT}/me/mailFolders"),
        };
        let request = self
            .request(Method::POST, &url)
            .await?
            .json(&FolderName { display_name });
        decode_json(self.send(request).await?).await
    }

    pub async fn rename_folder(&self, folder_id: &str, display_name: &str) -> Result<MailFolder> {
        let url = format!("{GRAPH_ROOT}/me/mailFolders/{}", encode_segment(folder_id));
        let request = self
            .request(Method::PATCH, &url)
            .await?
            .json(&FolderName { display_name });
        decode_json(self.send(request).await?).await
    }

    pub async fn delete_folder(&self, folder_id: &str) -> Result<()> {
        let url = format!("{GRAPH_ROOT}/me/mailFolders/{}", encode_segment(folder_id));
        let request = self.request(Method::DELETE, &url).await?;
        self.send(request).await?;
        Ok(())
    }

    /// The signed-in user's own profile picture, or `None` when none is set.
    pub async fn my_photo(&self) -> Result<Option<Photo>> {
        self.photo_at(&format!("{GRAPH_ROOT}/me/photo/$value"))
            .await
    }

    /// The profile picture of a user in the same tenant, or `None` when the
    /// address is unknown there or the user has no picture.
    pub async fn user_photo(&self, address: &str) -> Result<Option<Photo>> {
        let url = format!(
            "{GRAPH_ROOT}/users/{}/photo/$value",
            encode_segment(address)
        );
        self.photo_at(&url).await
    }

    /// The picture of the first personal contact with this email address.
    pub async fn contact_photo(&self, address: &str) -> Result<Option<Photo>> {
        let filter = format!(
            "emailAddresses/any(a:a/address eq '{}')",
            address.replace('\'', "''")
        );
        let url = format!(
            "{GRAPH_ROOT}/me/contacts?$select=id&$top=1&$filter={}",
            encode_query(&filter)
        );
        let request = self.request(Method::GET, &url).await?;
        let response = self.send_raw(request).await?;
        if matches!(
            response.status(),
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND | StatusCode::BAD_REQUEST
        ) {
            // Contacts.Read was not granted, or the mailbox has no contacts
            // folder at all; either way there is no picture to be had.
            return Ok(None);
        }
        let page: GraphPage<ContactId> = decode_json(response).await?;
        let Some(contact) = page.value.into_iter().next() else {
            return Ok(None);
        };
        let url = format!(
            "{GRAPH_ROOT}/me/contacts/{}/photo/$value",
            encode_segment(&contact.id)
        );
        self.photo_at(&url).await
    }

    async fn photo_at(&self, url: &str) -> Result<Option<Photo>> {
        let request = self.request(Method::GET, url).await?;
        let response = self.send_raw(request).await?;
        // 404: no such user or no picture. 403/400: Graph refuses to look
        // up other users with this token (consumer accounts, guests). None
        // of those is a fault worth surfacing.
        if matches!(
            response.status(),
            StatusCode::NOT_FOUND | StatusCode::FORBIDDEN | StatusCode::BAD_REQUEST
        ) {
            return Ok(None);
        }
        let response = checked(response).await?;
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("image/jpeg")
            .to_owned();
        if let Some(length) = response.content_length()
            && length > MAX_PHOTO_BYTES as u64
        {
            bail!("profile photo of {length} bytes is too large");
        }
        let bytes = response.bytes().await?;
        if bytes.len() > MAX_PHOTO_BYTES {
            bail!("profile photo of {} bytes is too large", bytes.len());
        }
        if bytes.is_empty() {
            return Ok(None);
        }
        Ok(Some(Photo {
            content_type,
            bytes: bytes.to_vec(),
        }))
    }

    async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        let request = self.request(Method::GET, url).await?;
        decode_json(self.send(request).await?).await
    }

    async fn get_all<T: DeserializeOwned>(&self, mut url: String) -> Result<Vec<T>> {
        let mut values = Vec::new();
        loop {
            let page: GraphPage<T> = self.get_json(&url).await?;
            values.extend(page.value);
            match page.next_link {
                Some(next) => url = next,
                None => return Ok(values),
            }
        }
    }

    async fn get_limited<T: DeserializeOwned>(
        &self,
        mut url: String,
        limit: usize,
    ) -> Result<Vec<T>> {
        let mut values = Vec::with_capacity(limit);
        loop {
            let page: GraphPage<T> = self.get_json(&url).await?;
            values.extend(
                page.value
                    .into_iter()
                    .take(limit.saturating_sub(values.len())),
            );
            if values.len() >= limit {
                return Ok(values);
            }
            match page.next_link {
                Some(next) => url = next,
                None => return Ok(values),
            }
        }
    }

    async fn request(&self, method: Method, url: &str) -> Result<reqwest::RequestBuilder> {
        let access_token = self.token_manager.access_token().await?;
        Ok(self
            .http
            .request(method, url)
            .bearer_auth(access_token)
            .header("Prefer", IMMUTABLE_ID))
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        checked(self.send_raw(request).await?).await
    }

    /// Send with throttling retries but without turning an error status into
    /// a failure, for callers that treat some statuses as ordinary answers.
    async fn send_raw(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        const MAX_RETRIES: usize = 3;
        for attempt in 0..=MAX_RETRIES {
            let response = request
                .try_clone()
                .context("Graph request body cannot be retried")?
                .send()
                .await?;
            let retryable = matches!(
                response.status(),
                StatusCode::TOO_MANY_REQUESTS
                    | StatusCode::SERVICE_UNAVAILABLE
                    | StatusCode::GATEWAY_TIMEOUT
            );
            if !retryable || attempt == MAX_RETRIES {
                return Ok(response);
            }
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .map(|seconds| seconds.clamp(1, 300))
                .unwrap_or_else(|| (1_u64 << attempt).min(15));
            let status = response.status();
            let _ = response.bytes().await;
            tracing::warn!(
                %status,
                retry_after,
                attempt = attempt + 1,
                "Microsoft Graph throttled a request; retrying"
            );
            tokio::time::sleep(Duration::from_secs(retry_after)).await;
        }
        unreachable!("retry loop always returns")
    }
}

async fn decode_json<T: DeserializeOwned>(response: reqwest::Response) -> Result<T> {
    let response = checked(response).await?;
    response
        .json()
        .await
        .context("invalid response from Microsoft Graph")
}

async fn checked(response: reqwest::Response) -> Result<reqwest::Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if status == StatusCode::UNAUTHORIZED {
        bail!("Microsoft Graph authorization failed: {body}");
    }
    if status == StatusCode::GONE {
        return Err(anyhow::Error::new(ResyncRequired)
            .context(format!("Microsoft Graph returned 410: {body}")));
    }
    bail!("Microsoft Graph returned {status}: {body}")
}

fn encode_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Percent-encode a query value, keeping the characters OData filters are
/// made of readable.
fn encode_query(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'~' | b'(' | b')' | b'/' | b':' | b'\'' | b','
            )
        {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn calendar_view_url(start: &str, end: &str) -> String {
    format!(
        "{GRAPH_ROOT}/me/calendarView?startDateTime={}&endDateTime={}&$top=1000&$select=id,iCalUId,subject,start,end,isAllDay,location,webLink,onlineMeeting,onlineMeetingUrl,responseStatus,showAs,isCancelled",
        encode_query(start),
        encode_query(end)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_query_values_for_odata() {
        assert_eq!(
            encode_query("emailAddresses/any(a:a/address eq 'bob@x.y')"),
            "emailAddresses/any(a:a/address%20eq%20'bob%40x.y')"
        );
    }

    #[test]
    fn builds_calendar_view_window() {
        let url = calendar_view_url("2026-09-02T09:00:00Z", "2026-09-03T09:00:00Z");
        assert!(url.contains("startDateTime=2026-09-02T09:00:00Z"));
        assert!(url.contains("endDateTime=2026-09-03T09:00:00Z"));
        assert!(url.contains("$select=id,iCalUId,subject,start,end"));
    }

    #[test]
    fn calendar_event_preserves_graph_field_names() {
        let event: CalendarEvent = serde_json::from_str(
            r#"{"id":"event-1","iCalUId":"ical-1","subject":"Review",
                "start":{"dateTime":"2026-09-02T09:00:00.0000000","timeZone":"UTC"},
                "end":{"dateTime":"2026-09-02T09:30:00.0000000","timeZone":"UTC"},
                "isAllDay":false,"location":{"displayName":"Room 1"}}"#,
        )
        .unwrap();
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["iCalUId"], "ical-1");
        assert_eq!(value["start"]["timeZone"], "UTC");
    }

    #[test]
    fn reads_size_from_extended_property() {
        let summary: MessageSummary = serde_json::from_str(
            r#"{"id":"a","isRead":true,"flag":{"flagStatus":"flagged"},
                "singleValueExtendedProperties":[{"id":"Integer 0xe08","value":"1234"}]}"#,
        )
        .unwrap();
        assert_eq!(summary.size(), Some(1234));
        assert!(summary.is_flagged());
        assert_eq!(summary.pinned(), None, "no pin property in the payload");
        assert!(!summary.is_pinned());
    }

    #[test]
    fn reads_pin_state_from_the_renew_time_sentinel() {
        let pinned: MessageSummary = serde_json::from_str(
            r#"{"id":"a","singleValueExtendedProperties":[
                {"id":"SystemTime 0xf02","value":"4500-09-01T00:00:00Z"}]}"#,
        )
        .unwrap();
        assert_eq!(pinned.pinned(), Some(true));
        let plain: MessageSummary = serde_json::from_str(
            r#"{"id":"b","singleValueExtendedProperties":[
                {"id":"SystemTime 0xf02","value":"2026-08-28T21:03:32Z"}]}"#,
        )
        .unwrap();
        assert_eq!(plain.pinned(), Some(false));
        let mut restored = plain.clone();
        restored.stored_pinned = Some(true);
        assert!(
            restored.is_pinned(),
            "the local index wins over a stale property"
        );
    }

    #[test]
    fn pin_patch_deletes_the_property_to_unpin() {
        let pin = serde_json::to_value(PinState {
            single_value_extended_properties: [ExtendedPropertyWrite {
                id: PIN_WRITE_PROPERTY,
                value: Some(PINNED_SENTINEL),
            }],
        })
        .unwrap();
        assert_eq!(
            pin["singleValueExtendedProperties"][0]["value"],
            "4500-09-01T00:00:00Z"
        );
        let unpin = serde_json::to_value(PinState {
            single_value_extended_properties: [ExtendedPropertyWrite {
                id: PIN_WRITE_PROPERTY,
                value: None,
            }],
        })
        .unwrap();
        assert!(unpin["singleValueExtendedProperties"][0]["value"].is_null());
    }

    #[test]
    fn parses_delta_pages_with_removed_entries() {
        let page: DeltaPage<MessageSummary> = serde_json::from_str(
            r#"{"@odata.deltaLink":"https://graph/delta?$deltatoken=x",
                "value":[
                  {"id":"a","subject":"s","from":{"emailAddress":{"name":"Bob","address":"bob@x"}},
                   "toRecipients":[{"emailAddress":{"address":"me@x"}}]},
                  {"id":"b","@removed":{"reason":"deleted"}}
                ]}"#,
        )
        .unwrap();
        assert!(page.next_link.is_none());
        assert!(page.delta_link.is_some());
        assert_eq!(page.value.len(), 2);
        assert!(page.value[0].removed.is_none());
        assert_eq!(
            page.value[0]
                .from
                .as_ref()
                .unwrap()
                .email_address
                .address
                .as_deref(),
            Some("bob@x")
        );
        assert!(page.value[1].removed.is_some());
    }

    #[test]
    fn resync_marker_is_detectable_through_context() {
        let error = anyhow::Error::new(ResyncRequired).context("outer");
        assert!(is_resync_required(&error));
        assert!(!is_resync_required(&anyhow::anyhow!("other")));
    }

    #[test]
    fn encodes_path_segments_without_plus() {
        assert_eq!(encode_segment("a b=c/d"), "a%20b%3Dc%2Fd");
        assert_eq!(encode_segment("AAMkAD-_="), "AAMkAD-_%3D");
    }
}
