// SPDX-License-Identifier: GPL-2.0-or-later

//! The Microsoft Graph calendar resources the CalDAV side reads and writes.

use serde::{Deserialize, Deserializer, Serialize};

/// Event fields requested from Graph. `cancelledOccurrences` is only
/// returned by a GET of one series master, so it is requested separately.
pub const EVENT_FIELDS: &str = "id,iCalUId,changeKey,type,seriesMasterId,subject,body,start,end,isAllDay,location,attendees,organizer,isOrganizer,responseStatus,responseRequested,showAs,sensitivity,categories,isReminderOn,reminderMinutesBeforeStart,recurrence,isCancelled,onlineMeeting,createdDateTime,lastModifiedDateTime,originalStart,originalStartTimeZone,originalEndTimeZone,importance";

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Calendar {
    pub id: String,
    pub name: String,
    #[serde(deserialize_with = "null_default")]
    pub hex_color: String,
    pub can_edit: bool,
    pub is_default_calendar: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Event {
    pub id: String,
    #[serde(rename = "iCalUId")]
    pub i_cal_uid: Option<String>,
    pub change_key: Option<String>,
    /// `singleInstance`, `occurrence`, `exception` or `seriesMaster`.
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub series_master_id: Option<String>,
    pub subject: Option<String>,
    pub body: Option<ItemBody>,
    pub start: Option<DateTimeTimeZone>,
    pub end: Option<DateTimeTimeZone>,
    pub is_all_day: bool,
    pub location: Option<Location>,
    #[serde(deserialize_with = "null_default")]
    pub attendees: Vec<Attendee>,
    pub organizer: Option<Recipient>,
    pub is_organizer: bool,
    pub response_status: Option<ResponseStatus>,
    pub response_requested: bool,
    pub show_as: Option<String>,
    pub sensitivity: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub categories: Vec<String>,
    pub is_reminder_on: bool,
    pub reminder_minutes_before_start: Option<i64>,
    pub recurrence: Option<PatternedRecurrence>,
    pub is_cancelled: bool,
    pub online_meeting: Option<OnlineMeeting>,
    pub created_date_time: Option<String>,
    pub last_modified_date_time: Option<String>,
    /// UTC start an occurrence or exception had in the series pattern.
    pub original_start: Option<String>,
    pub original_start_time_zone: Option<String>,
    pub original_end_time_zone: Option<String>,
    pub importance: Option<String>,
    /// `OID.<master id>.<yyyy-mm-dd>` per cancelled occurrence.
    #[serde(deserialize_with = "null_default")]
    pub cancelled_occurrences: Vec<String>,
}

impl Event {
    pub fn is_series_master(&self) -> bool {
        self.kind.as_deref() == Some("seriesMaster")
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ItemBody {
    pub content_type: Option<String>,
    pub content: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DateTimeTimeZone {
    pub date_time: String,
    pub time_zone: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Location {
    pub display_name: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EmailAddress {
    pub name: Option<String>,
    pub address: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Recipient {
    pub email_address: EmailAddress,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Attendee {
    pub email_address: EmailAddress,
    /// `required`, `optional` or `resource`.
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub status: Option<ResponseStatus>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ResponseStatus {
    /// `none`, `organizer`, `tentativelyAccepted`, `accepted`, `declined`
    /// or `notResponded`.
    pub response: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OnlineMeeting {
    pub join_url: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct PatternedRecurrence {
    pub pattern: RecurrencePattern,
    pub range: RecurrenceRange,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct RecurrencePattern {
    /// `daily`, `weekly`, `absoluteMonthly`, `relativeMonthly`,
    /// `absoluteYearly` or `relativeYearly`.
    #[serde(rename = "type")]
    pub kind: String,
    pub interval: u32,
    #[serde(skip_serializing_if = "is_zero")]
    pub month: u32,
    #[serde(skip_serializing_if = "is_zero")]
    pub day_of_month: u32,
    #[serde(
        deserialize_with = "null_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub days_of_week: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_day_of_week: Option<String>,
    /// `first` … `fourth` or `last`; meaningful for the relative types only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct RecurrenceRange {
    /// `endDate`, `noEnd` or `numbered`.
    #[serde(rename = "type")]
    pub kind: String,
    pub start_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_date: Option<String>,
    #[serde(skip_serializing_if = "is_zero")]
    pub number_of_occurrences: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recurrence_time_zone: Option<String>,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// Graph sends explicit `null` for empty collections and strings.
fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tolerates_nulls_from_graph() {
        let event: Event = serde_json::from_str(
            r#"{"id":"e","attendees":null,"categories":null,"recurrence":null,
                "start":{"dateTime":"2026-09-08T18:00:00.0000000","timeZone":"UTC"}}"#,
        )
        .unwrap();
        assert!(event.attendees.is_empty());
        assert!(event.recurrence.is_none());
        let calendar: Calendar =
            serde_json::from_str(r#"{"id":"c","name":"Birthdays","hexColor":null}"#).unwrap();
        assert_eq!(calendar.hex_color, "");
    }

    #[test]
    fn recurrence_writes_only_meaningful_fields() {
        let recurrence = PatternedRecurrence {
            pattern: RecurrencePattern {
                kind: "daily".into(),
                interval: 1,
                ..Default::default()
            },
            range: RecurrenceRange {
                kind: "noEnd".into(),
                start_date: Some("2026-10-05".into()),
                ..Default::default()
            },
        };
        let value = serde_json::to_value(recurrence).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "pattern": {"type": "daily", "interval": 1},
                "range": {"type": "noEnd", "startDate": "2026-10-05"}
            })
        );
    }
}
