// SPDX-License-Identifier: GPL-2.0-or-later

//! Graph events to iCalendar objects: one object per single event or per
//! series (master with RRULE/EXDATE plus one override per exception).

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;

use super::model::{Attendee, DateTimeTimeZone, Event, PatternedRecurrence, RecurrencePattern};
use crate::ical::{Component, Property, escape_text};
use crate::timezones;

pub const PRODID: &str = "-//graphmail-bridge//CalDAV//EN";

pub struct RenderContext<'a> {
    /// The UID clients know this object by.
    pub uid: &'a str,
    /// The account's own address, to recognise the user among attendees.
    pub account_email: &'a str,
    /// Zone for events whose own zone Graph does not name usefully.
    pub local: Tz,
}

/// Render a single event, or a series master with its exceptions.
pub fn render(event: &Event, exceptions: &[Event], context: &RenderContext<'_>) -> String {
    let zone = event_zone(event, context.local);
    let mut zones: Vec<Tz> = Vec::new();
    let mut master = vevent(event, zone, context, &mut zones);
    if let Some(recurrence) = &event.recurrence
        && event.is_series_master()
    {
        if let Some(rule) = rrule(recurrence, event, zone) {
            master.push(Property::new("RRULE", rule));
        }
        let start_time = event
            .start
            .as_ref()
            .and_then(graph_instant)
            .map(|start| start.with_timezone(&zone).time())
            .unwrap_or(NaiveTime::MIN);
        for marker in &event.cancelled_occurrences {
            let Some(date) = marker
                .rsplit('.')
                .next()
                .and_then(|date| NaiveDate::parse_from_str(date, "%Y-%m-%d").ok())
            else {
                continue;
            };
            master.push(occurrence_property(
                "EXDATE",
                event.is_all_day,
                date,
                date.and_time(start_time),
                zone,
            ));
        }
    }
    let mut events = vec![master];
    for exception in exceptions {
        let Some(original) = exception
            .original_start
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc))
        else {
            continue;
        };
        let mut component = vevent(exception, zone, context, &mut zones);
        let local = original.with_timezone(&zone).naive_local();
        // All-day originals are stored at midnight UTC by some mailboxes and
        // at local midnight by others; prefer the UTC date when it is one.
        let date = if event.is_all_day && original.time() == NaiveTime::MIN {
            original.date_naive()
        } else {
            local.date()
        };
        let recurrence_id =
            occurrence_property("RECURRENCE-ID", event.is_all_day, date, local, zone);
        component.properties.insert(1, recurrence_id);
        events.push(component);
    }

    let mut calendar = Component::new("VCALENDAR");
    calendar
        .push(Property::new("VERSION", "2.0"))
        .push(Property::new("PRODID", PRODID));
    let year = event
        .start
        .as_ref()
        .and_then(graph_instant)
        .map(|start| start.year())
        .unwrap_or(2026);
    for tz in zones {
        calendar.components.push(timezones::vtimezone(tz, year));
    }
    calendar.components.extend(events);
    calendar.to_string_folded()
}

fn vevent(event: &Event, zone: Tz, context: &RenderContext<'_>, zones: &mut Vec<Tz>) -> Component {
    let mut component = Component::new("VEVENT");
    component.push(Property::new("UID", context.uid));
    let stamp = event
        .last_modified_date_time
        .as_deref()
        .and_then(parse_utc)
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
    component.push(Property::new("DTSTAMP", format_utc(stamp)));
    if let Some(created) = event.created_date_time.as_deref().and_then(parse_utc) {
        component.push(Property::new("CREATED", format_utc(created)));
    }
    component.push(Property::new("LAST-MODIFIED", format_utc(stamp)));

    for (name, value) in [("DTSTART", &event.start), ("DTEND", &event.end)] {
        let Some(value) = value else { continue };
        if event.is_all_day {
            if let Some(date) = graph_naive(value).map(|naive| naive.date()) {
                component.push(
                    Property::new(name, date.format("%Y%m%d").to_string()).param("VALUE", "DATE"),
                );
            }
        } else if let Some(instant) = graph_instant(value) {
            component.push(timed_property(name, instant, zone, zones));
        }
    }

    component.push_text(
        "SUMMARY",
        event.subject.as_deref().unwrap_or_default().trim(),
    );
    if let Some(body) = event
        .body
        .as_ref()
        .and_then(|body| body.content.as_deref())
        .map(str::trim)
    {
        component.push_text("DESCRIPTION", body);
    }
    if let Some(location) = event
        .location
        .as_ref()
        .and_then(|location| location.display_name.as_deref())
    {
        component.push_text("LOCATION", location);
    }
    if let Some(url) = event
        .online_meeting
        .as_ref()
        .and_then(|meeting| meeting.join_url.as_deref())
        .filter(|url| !url.is_empty())
    {
        component.push(Property::new("URL", url.replace(['\r', '\n'], "")));
        component.push_text("X-MICROSOFT-SKYPETEAMSMEETINGURL", url);
    }

    let status = if event.is_cancelled {
        "CANCELLED"
    } else {
        "CONFIRMED"
    };
    component.push(Property::new("STATUS", status));
    let show_as = event.show_as.as_deref().unwrap_or("busy");
    component.push(Property::new(
        "TRANSP",
        if show_as == "free" {
            "TRANSPARENT"
        } else {
            "OPAQUE"
        },
    ));
    component.push(Property::new(
        "X-MICROSOFT-CDO-BUSYSTATUS",
        match show_as {
            "free" => "FREE",
            "tentative" => "TENTATIVE",
            "oof" => "OOF",
            "workingElsewhere" => "WORKINGELSEWHERE",
            _ => "BUSY",
        },
    ));
    let class = match event.sensitivity.as_deref() {
        Some("private" | "personal") => "PRIVATE",
        Some("confidential") => "CONFIDENTIAL",
        _ => "PUBLIC",
    };
    component.push(Property::new("CLASS", class));
    match event.importance.as_deref() {
        Some("high") => {
            component.push(Property::new("PRIORITY", "1"));
        }
        Some("low") => {
            component.push(Property::new("PRIORITY", "9"));
        }
        _ => {}
    }
    if !event.categories.is_empty() {
        let list: Vec<String> = event.categories.iter().map(|c| escape_text(c)).collect();
        component.push(Property::new("CATEGORIES", list.join(",")));
    }

    add_participants(&mut component, event, context);

    if event.is_reminder_on {
        let minutes = event.reminder_minutes_before_start.unwrap_or(15).max(0);
        let mut alarm = Component::new("VALARM");
        let alarm_uid = format!("{}-reminder", context.uid);
        alarm
            .push(Property::new("UID", alarm_uid.clone()))
            .push(Property::new("X-EVOLUTION-ALARM-UID", alarm_uid))
            .push(Property::new("ACTION", "DISPLAY"))
            .push(Property::new("TRIGGER", format!("-PT{minutes}M")));
        alarm.push_text(
            "DESCRIPTION",
            event
                .subject
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or("Reminder"),
        );
        component.components.push(alarm);
    }
    component
}

/// ORGANIZER and ATTENDEE lines. A meeting is anything with attendees or
/// organised by someone else; plain appointments get neither.
fn add_participants(component: &mut Component, event: &Event, context: &RenderContext<'_>) {
    let organizer = event
        .organizer
        .as_ref()
        .and_then(|organizer| organizer.email_address.address.as_deref())
        .map(str::to_owned);
    let organized_elsewhere = !event.is_organizer && organizer.is_some();
    if event.attendees.is_empty() && !organized_elsewhere {
        return;
    }
    let own_response = event
        .response_status
        .as_ref()
        .and_then(|status| status.response.as_deref());
    if let (Some(address), Some(recipient)) = (&organizer, &event.organizer) {
        let mut property = Property::new("ORGANIZER", format!("mailto:{address}"));
        if let Some(name) = recipient
            .email_address
            .name
            .as_deref()
            .filter(|n| !n.is_empty())
        {
            property = property.param("CN", name);
        }
        component.push(property);
    }
    let mut listed_self = false;
    for attendee in &event.attendees {
        let Some(address) = attendee.email_address.address.as_deref() else {
            continue;
        };
        let is_self = address.eq_ignore_ascii_case(context.account_email);
        listed_self |= is_self;
        let response = if is_self {
            own_response
        } else {
            attendee
                .status
                .as_ref()
                .and_then(|status| status.response.as_deref())
        };
        component.push(attendee_property(
            attendee,
            address,
            response,
            event.response_requested,
        ));
    }
    if !listed_self && organized_elsewhere {
        let me = Attendee {
            kind: Some("required".into()),
            ..Attendee::default()
        };
        component.push(attendee_property(
            &me,
            context.account_email,
            own_response,
            event.response_requested,
        ));
    }
}

fn attendee_property(
    attendee: &Attendee,
    address: &str,
    response: Option<&str>,
    response_requested: bool,
) -> Property {
    let mut property = Property::new("ATTENDEE", format!("mailto:{address}"));
    if let Some(name) = attendee
        .email_address
        .name
        .as_deref()
        .filter(|n| !n.is_empty())
    {
        property = property.param("CN", name);
    }
    let (cutype, role) = match attendee.kind.as_deref() {
        Some("optional") => ("INDIVIDUAL", "OPT-PARTICIPANT"),
        Some("resource") => ("RESOURCE", "NON-PARTICIPANT"),
        _ => ("INDIVIDUAL", "REQ-PARTICIPANT"),
    };
    let partstat = partstat(response);
    property = property
        .param("CUTYPE", cutype)
        .param("ROLE", role)
        .param("PARTSTAT", partstat);
    if partstat == "NEEDS-ACTION" && response_requested {
        property = property.param("RSVP", "TRUE");
    }
    property
}

pub fn partstat(response: Option<&str>) -> &'static str {
    match response {
        Some("accepted" | "organizer") => "ACCEPTED",
        Some("tentativelyAccepted") => "TENTATIVE",
        Some("declined") => "DECLINED",
        _ => "NEEDS-ACTION",
    }
}

/// The zone a series repeats in, falling back to the machine's zone when
/// Graph names a custom or unknown one.
pub fn event_zone(event: &Event, fallback: Tz) -> Tz {
    event
        .original_start_time_zone
        .as_deref()
        .and_then(timezones::resolve)
        .or_else(|| {
            event
                .recurrence
                .as_ref()
                .and_then(|recurrence| recurrence.range.recurrence_time_zone.as_deref())
                .and_then(timezones::resolve)
        })
        .unwrap_or(fallback)
}

fn timed_property(name: &str, instant: DateTime<Utc>, zone: Tz, zones: &mut Vec<Tz>) -> Property {
    if zone == Tz::UTC {
        return Property::new(name, format_utc(instant));
    }
    if !zones.contains(&zone) {
        zones.push(zone);
    }
    let local = instant.with_timezone(&zone).naive_local();
    Property::new(name, timezones::format_local(local)).param("TZID", zone.name())
}

/// EXDATE or RECURRENCE-ID in the same form as the series' DTSTART.
fn occurrence_property(
    name: &str,
    all_day: bool,
    date: NaiveDate,
    local: NaiveDateTime,
    zone: Tz,
) -> Property {
    if all_day {
        return Property::new(name, date.format("%Y%m%d").to_string()).param("VALUE", "DATE");
    }
    if zone == Tz::UTC {
        return Property::new(name, local.format("%Y%m%dT%H%M%SZ").to_string());
    }
    Property::new(name, timezones::format_local(local)).param("TZID", zone.name())
}

/// The RRULE for a Graph recurrence, or `None` for a pattern it cannot
/// express.
pub fn rrule(recurrence: &PatternedRecurrence, event: &Event, zone: Tz) -> Option<String> {
    let pattern = &recurrence.pattern;
    let mut parts = Vec::new();
    let frequency = match pattern.kind.as_str() {
        "daily" => "DAILY",
        "weekly" => "WEEKLY",
        "absoluteMonthly" | "relativeMonthly" => "MONTHLY",
        "absoluteYearly" | "relativeYearly" => "YEARLY",
        _ => return None,
    };
    parts.push(format!("FREQ={frequency}"));
    if pattern.interval > 1 {
        parts.push(format!("INTERVAL={}", pattern.interval));
    }
    match pattern.kind.as_str() {
        "weekly" => {
            parts.push(format!("BYDAY={}", day_codes(pattern)?.join(",")));
            if let Some(first) = pattern.first_day_of_week.as_deref().and_then(day_code) {
                parts.push(format!("WKST={first}"));
            }
        }
        "absoluteMonthly" => parts.push(format!("BYMONTHDAY={}", pattern.day_of_month)),
        "absoluteYearly" => {
            parts.push(format!("BYMONTH={}", pattern.month));
            parts.push(format!("BYMONTHDAY={}", pattern.day_of_month));
        }
        "relativeMonthly" | "relativeYearly" => {
            if pattern.kind == "relativeYearly" {
                parts.push(format!("BYMONTH={}", pattern.month));
            }
            parts.extend(relative_days(pattern)?);
        }
        _ => {}
    }
    let range = &recurrence.range;
    match range.kind.as_str() {
        "numbered" if range.number_of_occurrences > 0 => {
            parts.push(format!("COUNT={}", range.number_of_occurrences));
        }
        "endDate" => {
            let end = NaiveDate::parse_from_str(range.end_date.as_deref()?, "%Y-%m-%d").ok()?;
            if event.is_all_day {
                parts.push(format!("UNTIL={}", end.format("%Y%m%d")));
            } else {
                let last_moment = end.and_hms_opt(23, 59, 59)?;
                let until = zone
                    .from_local_datetime(&last_moment)
                    .latest()?
                    .with_timezone(&Utc);
                parts.push(format!("UNTIL={}", format_utc(until)));
            }
        }
        _ => {}
    }
    Some(parts.join(";"))
}

fn relative_days(pattern: &RecurrencePattern) -> Option<Vec<String>> {
    let position = match pattern.index.as_deref().unwrap_or("first") {
        "first" => 1,
        "second" => 2,
        "third" => 3,
        "fourth" => 4,
        "last" => -1,
        _ => return None,
    };
    let days = day_codes(pattern)?;
    Some(if days.len() == 1 {
        vec![format!("BYDAY={position}{}", days[0])]
    } else {
        vec![
            format!("BYDAY={}", days.join(",")),
            format!("BYSETPOS={position}"),
        ]
    })
}

fn day_codes(pattern: &RecurrencePattern) -> Option<Vec<&'static str>> {
    let days: Vec<&'static str> = pattern
        .days_of_week
        .iter()
        .filter_map(|day| day_code(day))
        .collect();
    (!days.is_empty()).then_some(days)
}

pub fn day_code(day: &str) -> Option<&'static str> {
    Some(match day.to_ascii_lowercase().as_str() {
        "monday" => "MO",
        "tuesday" => "TU",
        "wednesday" => "WE",
        "thursday" => "TH",
        "friday" => "FR",
        "saturday" => "SA",
        "sunday" => "SU",
        _ => return None,
    })
}

/// A Graph date-time as a wall-clock value in its own zone.
pub fn graph_naive(value: &DateTimeTimeZone) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(&value.date_time, "%Y-%m-%dT%H:%M:%S%.f").ok()
}

/// A Graph date-time as an instant.
pub fn graph_instant(value: &DateTimeTimeZone) -> Option<DateTime<Utc>> {
    let naive = graph_naive(value)?;
    let zone = timezones::resolve(&value.time_zone).unwrap_or(Tz::UTC);
    Some(
        zone.from_local_datetime(&naive)
            .earliest()?
            .with_timezone(&Utc),
    )
}

fn parse_utc(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

pub fn format_utc(value: DateTime<Utc>) -> String {
    value.format("%Y%m%dT%H%M%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::model::{
        EmailAddress, ItemBody, Recipient, RecurrenceRange, ResponseStatus,
    };
    use crate::ical::parse;

    fn at(date_time: &str) -> Option<DateTimeTimeZone> {
        Some(DateTimeTimeZone {
            date_time: date_time.into(),
            time_zone: "UTC".into(),
        })
    }

    fn context() -> RenderContext<'static> {
        RenderContext {
            uid: "uid-1",
            account_email: "me@example.com",
            local: Tz::America__New_York,
        }
    }

    fn weekly_series() -> Event {
        Event {
            id: "master".into(),
            kind: Some("seriesMaster".into()),
            subject: Some("Standup".into()),
            start: at("2026-09-08T13:30:00.0000000"),
            end: at("2026-09-08T14:00:00.0000000"),
            original_start_time_zone: Some("Eastern Standard Time".into()),
            last_modified_date_time: Some("2026-09-01T12:00:00.1234567Z".into()),
            recurrence: Some(PatternedRecurrence {
                pattern: RecurrencePattern {
                    kind: "weekly".into(),
                    interval: 2,
                    days_of_week: vec!["tuesday".into()],
                    first_day_of_week: Some("sunday".into()),
                    index: Some("first".into()),
                    ..Default::default()
                },
                range: RecurrenceRange {
                    kind: "endDate".into(),
                    start_date: Some("2026-09-08".into()),
                    end_date: Some("2026-12-15".into()),
                    recurrence_time_zone: Some("Eastern Standard Time".into()),
                    ..Default::default()
                },
            }),
            cancelled_occurrences: vec!["OID.master.2026-09-22".into()],
            ..Default::default()
        }
    }

    #[test]
    fn series_render_in_their_own_zone_with_rules_and_exdates() {
        let mut exception = weekly_series();
        exception.kind = Some("exception".into());
        exception.recurrence = None;
        exception.subject = Some("Standup (moved)".into());
        exception.original_start = Some("2026-10-06T13:30:00Z".into());
        exception.start = at("2026-10-06T15:00:00.0000000");
        exception.end = at("2026-10-06T15:30:00.0000000");
        let text = render(&weekly_series(), &[exception], &context());
        let calendar = parse(&text).unwrap();
        assert_eq!(calendar.children("VTIMEZONE").count(), 1);
        let events: Vec<_> = calendar.children("VEVENT").collect();
        assert_eq!(events.len(), 2);
        let master = events[0];
        let start = master.property("DTSTART").unwrap();
        assert_eq!(start.get_param("TZID"), Some("America/New_York"));
        assert_eq!(start.value, "20260908T093000");
        assert_eq!(
            master.property("RRULE").unwrap().value,
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=TU;WKST=SU;UNTIL=20261216T045959Z"
        );
        assert_eq!(master.property("EXDATE").unwrap().value, "20260922T093000");
        let moved = events[1];
        assert_eq!(moved.property("UID").unwrap().value, "uid-1");
        assert_eq!(
            moved.property("RECURRENCE-ID").unwrap().value,
            "20261006T093000"
        );
        assert_eq!(moved.property("DTSTART").unwrap().value, "20261006T110000");
    }

    #[test]
    fn all_day_events_use_dates() {
        let event = Event {
            id: "e".into(),
            kind: Some("singleInstance".into()),
            subject: Some("Holiday".into()),
            is_all_day: true,
            start: at("2026-10-12T00:00:00.0000000"),
            end: at("2026-10-13T00:00:00.0000000"),
            show_as: Some("free".into()),
            ..Default::default()
        };
        let calendar = parse(&render(&event, &[], &context())).unwrap();
        let vevent = calendar.children("VEVENT").next().unwrap();
        let start = vevent.property("DTSTART").unwrap();
        assert_eq!(
            (start.value.as_str(), start.get_param("VALUE")),
            ("20261012", Some("DATE"))
        );
        assert_eq!(vevent.property("DTEND").unwrap().value, "20261013");
        assert_eq!(vevent.property("TRANSP").unwrap().value, "TRANSPARENT");
        assert_eq!(calendar.children("VTIMEZONE").count(), 0);
    }

    #[test]
    fn invitations_list_the_user_with_their_response() {
        let event = Event {
            id: "e".into(),
            subject: Some("Review".into()),
            start: at("2026-10-07T18:00:00.0000000"),
            end: at("2026-10-07T19:00:00.0000000"),
            original_start_time_zone: Some("UTC".into()),
            organizer: Some(Recipient {
                email_address: EmailAddress {
                    name: Some("Boss, The".into()),
                    address: Some("boss@example.com".into()),
                },
            }),
            response_status: Some(ResponseStatus {
                response: Some("tentativelyAccepted".into()),
            }),
            response_requested: true,
            body: Some(ItemBody {
                content_type: Some("text".into()),
                content: Some("Agenda; items, here\r\n".into()),
            }),
            is_reminder_on: true,
            reminder_minutes_before_start: Some(10),
            ..Default::default()
        };
        let text = render(&event, &[], &context());
        let calendar = parse(&text).unwrap();
        let vevent = calendar.children("VEVENT").next().unwrap();
        assert_eq!(
            vevent.property("DTSTART").unwrap().value,
            "20261007T180000Z"
        );
        let organizer = vevent.property("ORGANIZER").unwrap();
        assert_eq!(organizer.get_param("CN"), Some("Boss, The"));
        let me = vevent.property("ATTENDEE").unwrap();
        assert_eq!(me.value, "mailto:me@example.com");
        assert_eq!(me.get_param("PARTSTAT"), Some("TENTATIVE"));
        assert_eq!(vevent.text("DESCRIPTION").unwrap(), "Agenda; items, here");
        let alarm = vevent.children("VALARM").next().unwrap();
        assert_eq!(alarm.property("TRIGGER").unwrap().value, "-PT10M");
    }

    #[test]
    fn relative_patterns_use_ordinals_or_setpos() {
        let mut event = weekly_series();
        let recurrence = event.recurrence.as_mut().unwrap();
        recurrence.pattern = RecurrencePattern {
            kind: "relativeMonthly".into(),
            interval: 1,
            days_of_week: vec!["thursday".into()],
            index: Some("last".into()),
            ..Default::default()
        };
        recurrence.range = RecurrenceRange {
            kind: "numbered".into(),
            number_of_occurrences: 6,
            ..Default::default()
        };
        let zone = Tz::America__New_York;
        let rule = rrule(event.recurrence.as_ref().unwrap(), &event, zone).unwrap();
        assert_eq!(rule, "FREQ=MONTHLY;BYDAY=-1TH;COUNT=6");
        let recurrence = event.recurrence.as_mut().unwrap();
        recurrence.pattern.days_of_week = vec!["monday".into(), "friday".into()];
        recurrence.pattern.index = Some("second".into());
        let rule = rrule(event.recurrence.as_ref().unwrap(), &event, zone).unwrap();
        assert_eq!(rule, "FREQ=MONTHLY;BYDAY=MO,FR;BYSETPOS=2;COUNT=6");
    }
}
