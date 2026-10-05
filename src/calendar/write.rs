// SPDX-License-Identifier: GPL-2.0-or-later

//! iCalendar objects from CalDAV clients to Graph writes.
//!
//! A client always PUTs the whole object, and much of what Graph holds has
//! no exact iCalendar form (HTML bodies, Teams details). So an update is
//! sent as the difference between the client's object and the copy last
//! served: only fields the user actually changed reach Graph.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc, Weekday};
use chrono_tz::Tz;
use serde_json::{Map, Value, json};

use crate::ical::{self, Component, Property};
use crate::timezones;

/// What a client sent for one object: the master component (absent when a
/// client only holds overrides) and its overrides.
#[derive(Debug)]
pub struct ParsedObject {
    pub uid: String,
    pub master: Option<Component>,
    pub overrides: Vec<Component>,
}

pub struct WriteContext<'a> {
    pub account_email: &'a str,
    /// Zone for floating times and all-day events.
    pub local: Tz,
}

/// Graph calls an update turns into, in order.
#[derive(Debug, PartialEq)]
pub enum Change {
    /// PATCH the master (or single event).
    Patch(Value),
    /// Answer the invitation: `accept`, `tentativelyAccept` or `decline`.
    Respond(&'static str),
    /// PATCH the occurrence originally starting at this instant.
    PatchOccurrence(DateTime<Utc>, Value),
    RespondOccurrence(DateTime<Utc>, &'static str),
    /// Cancel the occurrence originally starting at this instant.
    DeleteOccurrence(DateTime<Utc>),
}

pub fn parse_object(text: &str) -> Result<ParsedObject> {
    let calendar = ical::parse(text)?;
    if calendar.name != "VCALENDAR" {
        bail!("expected a VCALENDAR object");
    }
    if calendar.children("VTODO").next().is_some() || calendar.children("VJOURNAL").next().is_some()
    {
        bail!("only events can be stored in Microsoft 365 calendars");
    }
    let events: Vec<&Component> = calendar.children("VEVENT").collect();
    let Some(first) = events.first() else {
        bail!("the object holds no VEVENT");
    };
    let uid = first
        .property("UID")
        .map(|property| property.value.clone())
        .filter(|uid| !uid.is_empty())
        .context("VEVENT without UID")?;
    let mut master = None;
    let mut overrides = Vec::new();
    for event in events {
        if event
            .property("UID")
            .map(|property| property.value.as_str())
            != Some(uid.as_str())
        {
            bail!("an object must hold one UID");
        }
        if event.property("RECURRENCE-ID").is_some() {
            overrides.push(event.clone());
        } else if master.replace(event.clone()).is_some() {
            bail!("two master VEVENTs with the same UID");
        }
    }
    Ok(ParsedObject {
        uid,
        master,
        overrides,
    })
}

/// The Graph event body for a new event in a calendar the user owns.
pub fn create_body(master: &Component, context: &WriteContext<'_>) -> Result<Value> {
    let mut fields = fields(master, context, true)?;
    if !organized_by_other(master, context) {
        fields.insert("attendees".into(), attendees(master, context));
    }
    Ok(Value::Object(fields))
}

/// The Graph calls that turn the `served` object into the `wanted` one.
pub fn changes(
    served: &ParsedObject,
    wanted: &ParsedObject,
    context: &WriteContext<'_>,
) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    let (Some(old), Some(new)) = (&served.master, &wanted.master) else {
        bail!("an existing event cannot lose its master component");
    };
    let organizer = !organized_by_other(old, context);
    let patch = diff(old, new, context, organizer, true)?;
    if !patch.is_empty() {
        changes.push(Change::Patch(Value::Object(patch)));
    }
    if let Some(action) = response_change(old, new, context) {
        changes.push(Change::Respond(action));
    }

    let old_exdates = exdates(old, context)?;
    for instant in exdates(new, context)? {
        if !old_exdates.contains(&instant) {
            changes.push(Change::DeleteOccurrence(instant));
        }
    }
    for override_component in &wanted.overrides {
        let instant = recurrence_instant(override_component, context)?;
        let baseline = served
            .overrides
            .iter()
            .find(|candidate| {
                recurrence_instant(candidate, context).is_ok_and(|other| other == instant)
            })
            .cloned()
            .unwrap_or_else(|| occurrence_of(old, override_component));
        let patch = diff(&baseline, override_component, context, organizer, false)?;
        if !patch.is_empty() {
            changes.push(Change::PatchOccurrence(instant, Value::Object(patch)));
        }
        if let Some(action) = response_change(&baseline, override_component, context) {
            changes.push(Change::RespondOccurrence(instant, action));
        }
    }
    Ok(changes)
}

/// Fields that differ between two versions of a component, as a PATCH.
fn diff(
    old: &Component,
    new: &Component,
    context: &WriteContext<'_>,
    organizer: bool,
    is_master: bool,
) -> Result<Map<String, Value>> {
    let mut before = fields(old, context, is_master)?;
    let mut after = fields(new, context, is_master)?;
    if organizer {
        before.insert("attendees".into(), attendees(old, context));
        after.insert("attendees".into(), attendees(new, context));
    } else {
        // Only the organizer can move or rename a meeting; an attendee's
        // own copy keeps its reminder, categories and free/busy.
        for key in [
            "subject",
            "body",
            "start",
            "end",
            "isAllDay",
            "location",
            "recurrence",
            "sensitivity",
            "importance",
        ] {
            before.remove(key);
            after.remove(key);
        }
    }
    let mut patch = Map::new();
    for (key, value) in after {
        if before.get(&key) != Some(&value) {
            patch.insert(key, value);
        }
    }
    // Times travel together so Graph never sees an end before the start.
    if patch.contains_key("start") || patch.contains_key("end") || patch.contains_key("isAllDay") {
        let full = fields(new, context, is_master)?;
        for key in ["start", "end", "isAllDay"] {
            if let Some(value) = full.get(key) {
                patch.insert(key.into(), value.clone());
            }
        }
    }
    Ok(patch)
}

/// Every writable field of a component, always present so a removal shows
/// up as a difference.
fn fields(
    component: &Component,
    context: &WriteContext<'_>,
    is_master: bool,
) -> Result<Map<String, Value>> {
    let mut fields = Map::new();
    fields.insert(
        "subject".into(),
        json!(component.text("SUMMARY").unwrap_or_default().trim()),
    );
    fields.insert(
        "body".into(),
        json!({
            "contentType": "text",
            "content": component.text("DESCRIPTION").unwrap_or_default(),
        }),
    );
    fields.insert(
        "location".into(),
        json!({ "displayName": component.text("LOCATION").unwrap_or_default() }),
    );
    let (start, end, all_day) = times(component, context)?;
    fields.insert("start".into(), start);
    fields.insert("end".into(), end);
    fields.insert("isAllDay".into(), json!(all_day));
    fields.insert("showAs".into(), json!(show_as(component)));
    fields.insert(
        "sensitivity".into(),
        json!(match component
            .property("CLASS")
            .map(|p| p.value.to_ascii_uppercase())
            .as_deref()
        {
            Some("PRIVATE") => "private",
            Some("CONFIDENTIAL") => "confidential",
            _ => "normal",
        }),
    );
    fields.insert(
        "importance".into(),
        json!(match component
            .property("PRIORITY")
            .and_then(|p| p.value.trim().parse::<u8>().ok())
        {
            Some(1..=4) => "high",
            Some(6..=9) => "low",
            _ => "normal",
        }),
    );
    let categories: Vec<String> = component
        .properties("CATEGORIES")
        .flat_map(|property| ical::split_value(&property.value, ','))
        .map(|category| ical::unescape_text(&category))
        .filter(|category| !category.is_empty())
        .collect();
    fields.insert("categories".into(), json!(categories));
    let reminder = component.children("VALARM").filter_map(alarm_minutes).min();
    fields.insert("isReminderOn".into(), json!(reminder.is_some()));
    if let Some(minutes) = reminder {
        fields.insert("reminderMinutesBeforeStart".into(), json!(minutes));
    }
    if is_master {
        let recurrence = match component.property("RRULE") {
            Some(rule) => recurrence(&rule.value, component, context)?,
            None => Value::Null,
        };
        fields.insert("recurrence".into(), recurrence);
    }
    Ok(fields)
}

fn show_as(component: &Component) -> &'static str {
    match component
        .property("X-MICROSOFT-CDO-BUSYSTATUS")
        .map(|property| property.value.to_ascii_uppercase())
        .as_deref()
    {
        Some("FREE") => return "free",
        Some("TENTATIVE") => return "tentative",
        Some("OOF") => return "oof",
        Some("WORKINGELSEWHERE") => return "workingElsewhere",
        Some("BUSY") => return "busy",
        _ => {}
    }
    match component
        .property("TRANSP")
        .map(|property| property.value.to_ascii_uppercase())
    {
        Some(value) if value == "TRANSPARENT" => "free",
        _ => "busy",
    }
}

/// Minutes before the start of a display or audio alarm relative to it.
fn alarm_minutes(alarm: &Component) -> Option<i64> {
    let trigger = alarm.property("TRIGGER")?;
    if trigger
        .get_param("RELATED")
        .is_some_and(|related| related.eq_ignore_ascii_case("END"))
        || trigger
            .get_param("VALUE")
            .is_some_and(|value| value.eq_ignore_ascii_case("DATE-TIME"))
    {
        return None;
    }
    let seconds = parse_duration(&trigger.value)?;
    Some((-seconds).max(0) / 60)
}

/// Start, end and all-day flag as Graph `dateTimeTimeZone` values.
fn times(component: &Component, context: &WriteContext<'_>) -> Result<(Value, Value, bool)> {
    let start_property = component
        .property("DTSTART")
        .context("VEVENT without DTSTART")?;
    let start = parse_time(start_property, context)?;
    let end = match component.property("DTEND") {
        Some(property) => parse_time(property, context)?,
        None => match component
            .property("DURATION")
            .and_then(|property| parse_duration(&property.value))
        {
            Some(seconds) => start.plus(Duration::seconds(seconds)),
            None if start.is_date() => start.plus(Duration::days(1)),
            None => start.clone(),
        },
    };
    let all_day = start.is_date();
    Ok((
        start.to_graph(context.local),
        end.to_graph(context.local),
        all_day,
    ))
}

/// A DTSTART/DTEND/RECURRENCE-ID/EXDATE value.
#[derive(Clone, Debug, PartialEq)]
enum Moment {
    Date(NaiveDate),
    Utc(DateTime<Utc>),
    Zoned(NaiveDateTime, Tz),
}

impl Moment {
    fn is_date(&self) -> bool {
        matches!(self, Self::Date(_))
    }

    fn plus(&self, duration: Duration) -> Self {
        match self {
            Self::Date(date) => Self::Date(*date + Duration::days(duration.num_days().max(1))),
            Self::Utc(instant) => Self::Utc(*instant + duration),
            Self::Zoned(local, tz) => Self::Zoned(*local + duration, *tz),
        }
    }

    fn instant(&self, local: Tz) -> Option<DateTime<Utc>> {
        match self {
            Self::Date(date) => local
                .from_local_datetime(&date.and_hms_opt(0, 0, 0)?)
                .earliest()
                .map(|value| value.with_timezone(&Utc)),
            Self::Utc(instant) => Some(*instant),
            Self::Zoned(naive, tz) => tz
                .from_local_datetime(naive)
                .earliest()
                .map(|value| value.with_timezone(&Utc)),
        }
    }

    fn zone(&self, local: Tz) -> Tz {
        match self {
            Self::Zoned(_, tz) => *tz,
            Self::Utc(_) => Tz::UTC,
            Self::Date(_) => local,
        }
    }

    fn to_graph(&self, local: Tz) -> Value {
        let (naive, tz) = match self {
            Self::Date(date) => (date.and_hms_opt(0, 0, 0).unwrap_or_default(), local),
            Self::Utc(instant) => (instant.naive_utc(), Tz::UTC),
            Self::Zoned(naive, tz) => (*naive, *tz),
        };
        match graph_zone_name(tz) {
            Some(name) => json!({
                "dateTime": naive.format("%Y-%m-%dT%H:%M:%S").to_string(),
                "timeZone": name,
            }),
            None => {
                let utc = tz
                    .from_local_datetime(&naive)
                    .earliest()
                    .map(|value| value.naive_utc())
                    .unwrap_or(naive);
                json!({
                    "dateTime": utc.format("%Y-%m-%dT%H:%M:%S").to_string(),
                    "timeZone": "UTC",
                })
            }
        }
    }
}

/// A zone name Graph accepts: Windows names always work.
fn graph_zone_name(tz: Tz) -> Option<&'static str> {
    if tz == Tz::UTC {
        return Some("UTC");
    }
    timezones::windows_name(tz)
}

fn parse_time(property: &Property, context: &WriteContext<'_>) -> Result<Moment> {
    let value = property.value.trim();
    let is_date = property
        .get_param("VALUE")
        .is_some_and(|kind| kind.eq_ignore_ascii_case("DATE"))
        || (value.len() == 8 && value.bytes().all(|byte| byte.is_ascii_digit()));
    if is_date {
        let date = NaiveDate::parse_from_str(value, "%Y%m%d")
            .with_context(|| format!("invalid date {value:?}"))?;
        return Ok(Moment::Date(date));
    }
    if let Some(utc) = value.strip_suffix('Z') {
        let naive = NaiveDateTime::parse_from_str(utc, "%Y%m%dT%H%M%S")
            .with_context(|| format!("invalid date-time {value:?}"))?;
        return Ok(Moment::Utc(Utc.from_utc_datetime(&naive)));
    }
    let naive = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S")
        .with_context(|| format!("invalid date-time {value:?}"))?;
    let tz = property
        .get_param("TZID")
        .and_then(timezones::resolve)
        .unwrap_or(context.local);
    Ok(Moment::Zoned(naive, tz))
}

/// An RFC 5545 duration in seconds (negative for `-P…`).
fn parse_duration(text: &str) -> Option<i64> {
    let text = text.trim();
    let (sign, rest) = match text.as_bytes().first()? {
        b'-' => (-1, &text[1..]),
        b'+' => (1, &text[1..]),
        _ => (1, text),
    };
    let rest = rest.strip_prefix('P')?;
    let mut seconds = 0i64;
    let mut number = String::new();
    let mut in_time = false;
    for character in rest.chars() {
        match character {
            'T' => in_time = true,
            '0'..='9' => number.push(character),
            unit => {
                let value: i64 = number.parse().ok()?;
                number.clear();
                seconds += value
                    * match (unit, in_time) {
                        ('W', false) => 7 * 86_400,
                        ('D', false) => 86_400,
                        ('H', true) => 3_600,
                        ('M', true) => 60,
                        ('S', true) => 1,
                        _ => return None,
                    };
            }
        }
    }
    number.is_empty().then_some(sign * seconds)
}

/// The recurrence for an RRULE, or an error naming what Graph cannot hold.
fn recurrence(rule: &str, component: &Component, context: &WriteContext<'_>) -> Result<Value> {
    let parts: Vec<(String, String)> = rule
        .split(';')
        .filter_map(|part| part.split_once('='))
        .map(|(key, value)| (key.to_ascii_uppercase(), value.to_ascii_uppercase()))
        .collect();
    let get = |key: &str| {
        parts
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    for (key, _) in &parts {
        if !matches!(
            key.as_str(),
            "FREQ"
                | "INTERVAL"
                | "BYDAY"
                | "BYMONTHDAY"
                | "BYMONTH"
                | "BYSETPOS"
                | "COUNT"
                | "UNTIL"
                | "WKST"
        ) {
            bail!("Microsoft 365 cannot repeat by {key}");
        }
    }
    let start_property = component
        .property("DTSTART")
        .context("VEVENT without DTSTART")?;
    let start = parse_time(start_property, context)?;
    let zone = start.zone(context.local);
    let start_date = match &start {
        Moment::Date(date) => *date,
        Moment::Utc(instant) => instant.date_naive(),
        Moment::Zoned(naive, _) => naive.date(),
    };
    let interval: u32 = get("INTERVAL")
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);
    let by_day: Vec<(Option<i32>, Weekday)> = match get("BYDAY") {
        Some(list) => list.split(',').map(parse_by_day).collect::<Result<_>>()?,
        None => Vec::new(),
    };
    let day_names = |days: &[(Option<i32>, Weekday)]| -> Vec<&'static str> {
        days.iter().map(|(_, day)| weekday_name(*day)).collect()
    };
    let month: Option<u32> = get("BYMONTH").map(|value| value.parse()).transpose()?;
    let month_day: Option<i32> = get("BYMONTHDAY").map(|value| value.parse()).transpose()?;
    let set_position: Option<i32> = get("BYSETPOS").map(|value| value.parse()).transpose()?;
    let mut pattern = Map::new();
    pattern.insert("interval".into(), json!(interval));
    let relative = |pattern: &mut Map<String, Value>, kind: &str| -> Result<()> {
        let ordinals: Vec<i32> = by_day.iter().filter_map(|(ordinal, _)| *ordinal).collect();
        let position = match (set_position, ordinals.as_slice()) {
            (Some(position), []) => position,
            (None, [ordinal]) if by_day.len() == 1 => *ordinal,
            _ => bail!("Microsoft 365 cannot hold this monthly or yearly rule"),
        };
        pattern.insert("type".into(), json!(kind));
        pattern.insert("daysOfWeek".into(), json!(day_names(&by_day)));
        pattern.insert("index".into(), json!(week_index(position)?));
        Ok(())
    };
    match get("FREQ").context("RRULE without FREQ")? {
        "DAILY" if by_day.is_empty() => {
            pattern.insert("type".into(), json!("daily"));
        }
        // "Every weekday" arrives as a daily rule limited to some days.
        "DAILY" | "WEEKLY" => {
            let days = if by_day.is_empty() {
                vec![weekday_name(start_date.weekday())]
            } else {
                day_names(&by_day)
            };
            pattern.insert("type".into(), json!("weekly"));
            pattern.insert("daysOfWeek".into(), json!(days));
            let first = match get("WKST") {
                Some(code) => weekday_name(parse_weekday(code)?),
                None => "sunday",
            };
            pattern.insert("firstDayOfWeek".into(), json!(first));
        }
        "MONTHLY" if by_day.is_empty() => {
            let day = month_day.unwrap_or(start_date.day() as i32);
            if day < 1 {
                bail!("Microsoft 365 cannot repeat on days counted from the month's end");
            }
            pattern.insert("type".into(), json!("absoluteMonthly"));
            pattern.insert("dayOfMonth".into(), json!(day));
        }
        "MONTHLY" => relative(&mut pattern, "relativeMonthly")?,
        "YEARLY" => {
            pattern.insert("month".into(), json!(month.unwrap_or(start_date.month())));
            if by_day.is_empty() {
                let day = month_day.unwrap_or(start_date.day() as i32);
                if day < 1 {
                    bail!("Microsoft 365 cannot repeat on days counted from the month's end");
                }
                pattern.insert("type".into(), json!("absoluteYearly"));
                pattern.insert("dayOfMonth".into(), json!(day));
            } else {
                relative(&mut pattern, "relativeYearly")?;
            }
        }
        other => bail!("Microsoft 365 cannot repeat {other}"),
    }

    let mut range = Map::new();
    range.insert(
        "startDate".into(),
        json!(start_date.format("%Y-%m-%d").to_string()),
    );
    if let Some(name) = graph_zone_name(zone) {
        range.insert("recurrenceTimeZone".into(), json!(name));
    }
    if let Some(count) = get("COUNT") {
        range.insert("type".into(), json!("numbered"));
        range.insert("numberOfOccurrences".into(), json!(count.parse::<u32>()?));
    } else if let Some(until) = get("UNTIL") {
        let property = Property::new("UNTIL", until);
        let end_date = match parse_time(&property, context)? {
            Moment::Date(date) => date,
            Moment::Utc(instant) => instant.with_timezone(&zone).date_naive(),
            Moment::Zoned(naive, _) => naive.date(),
        };
        range.insert("type".into(), json!("endDate"));
        range.insert(
            "endDate".into(),
            json!(end_date.format("%Y-%m-%d").to_string()),
        );
    } else {
        range.insert("type".into(), json!("noEnd"));
    }
    Ok(json!({ "pattern": pattern, "range": range }))
}

fn parse_by_day(entry: &str) -> Result<(Option<i32>, Weekday)> {
    let entry = entry.trim();
    let split = entry.len().saturating_sub(2);
    let (ordinal, code) = entry.split_at(split);
    let ordinal = if ordinal.is_empty() {
        None
    } else {
        Some(ordinal.trim_start_matches('+').parse::<i32>()?)
    };
    Ok((ordinal, parse_weekday(code)?))
}

fn parse_weekday(code: &str) -> Result<Weekday> {
    Ok(match code {
        "MO" => Weekday::Mon,
        "TU" => Weekday::Tue,
        "WE" => Weekday::Wed,
        "TH" => Weekday::Thu,
        "FR" => Weekday::Fri,
        "SA" => Weekday::Sat,
        "SU" => Weekday::Sun,
        other => bail!("unknown weekday {other:?}"),
    })
}

fn weekday_name(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "monday",
        Weekday::Tue => "tuesday",
        Weekday::Wed => "wednesday",
        Weekday::Thu => "thursday",
        Weekday::Fri => "friday",
        Weekday::Sat => "saturday",
        Weekday::Sun => "sunday",
    }
}

fn week_index(position: i32) -> Result<&'static str> {
    Ok(match position {
        1 => "first",
        2 => "second",
        3 => "third",
        4 => "fourth",
        -1 => "last",
        other => bail!("Microsoft 365 cannot repeat on occurrence {other} of a month"),
    })
}

/// Attendees other than the user, as Graph wants them.
fn attendees(component: &Component, context: &WriteContext<'_>) -> Value {
    let organizer = component.property("ORGANIZER").and_then(mail_address);
    let list: Vec<Value> = component
        .properties("ATTENDEE")
        .filter_map(|property| {
            let address = mail_address(property)?;
            if address.eq_ignore_ascii_case(context.account_email)
                || organizer
                    .as_deref()
                    .is_some_and(|organizer| organizer.eq_ignore_ascii_case(&address))
            {
                return None;
            }
            let cutype = property.get_param("CUTYPE").unwrap_or("INDIVIDUAL");
            let role = property.get_param("ROLE").unwrap_or("REQ-PARTICIPANT");
            let kind =
                if cutype.eq_ignore_ascii_case("RESOURCE") || cutype.eq_ignore_ascii_case("ROOM") {
                    "resource"
                } else if role.eq_ignore_ascii_case("OPT-PARTICIPANT")
                    || role.eq_ignore_ascii_case("NON-PARTICIPANT")
                {
                    "optional"
                } else {
                    "required"
                };
            let mut email = json!({ "address": address });
            if let Some(name) = property.get_param("CN").filter(|name| !name.is_empty()) {
                email["name"] = json!(name);
            }
            Some(json!({ "emailAddress": email, "type": kind }))
        })
        .collect();
    Value::Array(list)
}

fn mail_address(property: &Property) -> Option<String> {
    let value = property.value.trim();
    let address = value
        .get(..7)
        .filter(|scheme| scheme.eq_ignore_ascii_case("mailto:"))
        .map(|_| &value[7..])
        .unwrap_or(value);
    address.contains('@').then(|| address.to_owned())
}

/// Whether someone other than the user organises this event.
fn organized_by_other(component: &Component, context: &WriteContext<'_>) -> bool {
    component
        .property("ORGANIZER")
        .and_then(mail_address)
        .is_some_and(|organizer| !organizer.eq_ignore_ascii_case(context.account_email))
}

/// The user's PARTSTAT as a Graph response action, when it changed.
fn response_change(
    old: &Component,
    new: &Component,
    context: &WriteContext<'_>,
) -> Option<&'static str> {
    if !organized_by_other(new, context) {
        return None;
    }
    let own = |component: &Component| {
        component
            .properties("ATTENDEE")
            .find(|property| {
                mail_address(property)
                    .is_some_and(|address| address.eq_ignore_ascii_case(context.account_email))
            })
            .and_then(|property| property.get_param("PARTSTAT"))
            .map(str::to_ascii_uppercase)
    };
    let (before, after) = (own(old), own(new));
    if before == after {
        return None;
    }
    match after.as_deref() {
        Some("ACCEPTED") => Some("accept"),
        Some("TENTATIVE") => Some("tentativelyAccept"),
        Some("DECLINED") => Some("decline"),
        _ => None,
    }
}

fn exdates(component: &Component, context: &WriteContext<'_>) -> Result<Vec<DateTime<Utc>>> {
    let mut instants = Vec::new();
    for property in component.properties("EXDATE") {
        for value in property.value.split(',') {
            let mut single = property.clone();
            single.value = value.to_owned();
            if let Some(instant) = occurrence_instant(&single, component, context)? {
                instants.push(instant);
            }
        }
    }
    Ok(instants)
}

fn recurrence_instant(component: &Component, context: &WriteContext<'_>) -> Result<DateTime<Utc>> {
    let property = component
        .property("RECURRENCE-ID")
        .context("override without RECURRENCE-ID")?;
    occurrence_instant(property, component, context)?.context("RECURRENCE-ID names no instant")
}

/// The UTC instant an EXDATE or RECURRENCE-ID names. Graph identifies
/// occurrences by their original UTC start; for all-day series that is
/// midnight of the date in the master's zone.
fn occurrence_instant(
    property: &Property,
    component: &Component,
    context: &WriteContext<'_>,
) -> Result<Option<DateTime<Utc>>> {
    let moment = parse_time(property, context)?;
    if let Moment::Date(date) = moment {
        let zone = component
            .property("DTSTART")
            .and_then(|start| parse_time(start, context).ok())
            .map(|start| start.zone(context.local))
            .unwrap_or(context.local);
        return Ok(
            Moment::Zoned(date.and_hms_opt(0, 0, 0).unwrap_or_default(), zone)
                .instant(context.local),
        );
    }
    Ok(moment.instant(context.local))
}

/// The master as it looks at one occurrence, as the baseline for a new
/// override: same fields, the occurrence's times, no rule.
fn occurrence_of(master: &Component, override_component: &Component) -> Component {
    let mut occurrence = master.clone();
    occurrence
        .properties
        .retain(|property| !matches!(property.name.as_str(), "RRULE" | "EXDATE" | "RDATE"));
    if let (Some(start), Some(recurrence_id)) = (
        master.property("DTSTART"),
        override_component.property("RECURRENCE-ID"),
    ) {
        let duration = master.property("DTEND").and_then(|end| {
            let parse = |value: &str| {
                NaiveDateTime::parse_from_str(value.trim_end_matches('Z'), "%Y%m%dT%H%M%S").ok()
            };
            Some(parse(&end.value)? - parse(&start.value)?)
        });
        let mut new_start = recurrence_id.clone();
        new_start.name = "DTSTART".into();
        if let (Some(duration), Some(begin)) = (
            duration,
            NaiveDateTime::parse_from_str(
                recurrence_id.value.trim_end_matches('Z'),
                "%Y%m%dT%H%M%S",
            )
            .ok(),
        ) {
            let mut new_end = new_start.clone();
            new_end.name = "DTEND".into();
            new_end.value = format!(
                "{}{}",
                (begin + duration).format("%Y%m%dT%H%M%S"),
                if recurrence_id.value.ends_with('Z') {
                    "Z"
                } else {
                    ""
                }
            );
            occurrence
                .properties
                .retain(|property| property.name != "DTEND");
            occurrence.properties.push(new_end);
        }
        occurrence
            .properties
            .retain(|property| property.name != "DTSTART");
        occurrence.properties.push(new_start);
    }
    occurrence
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> WriteContext<'static> {
        WriteContext {
            account_email: "me@example.com",
            local: Tz::America__New_York,
        }
    }

    fn object(events: &str) -> ParsedObject {
        parse_object(&format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{events}END:VCALENDAR\r\n"
        ))
        .unwrap()
    }

    const SERIES: &str = "BEGIN:VEVENT\r\nUID:u1\r\nSUMMARY:Standup\r\n\
        DTSTART;TZID=America/New_York:20261006T093000\r\n\
        DTEND;TZID=America/New_York:20261006T100000\r\n\
        RRULE:FREQ=WEEKLY;BYDAY=TU;UNTIL=20261216T045959Z\r\n\
        BEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT10M\r\nEND:VALARM\r\nEND:VEVENT\r\n";

    #[test]
    fn creates_series_with_windows_zones_and_reminders() {
        let parsed = object(SERIES);
        let body = create_body(parsed.master.as_ref().unwrap(), &context()).unwrap();
        assert_eq!(body["subject"], "Standup");
        assert_eq!(
            body["start"],
            json!({"dateTime": "2026-10-06T09:30:00", "timeZone": "Eastern Standard Time"})
        );
        assert_eq!(body["reminderMinutesBeforeStart"], 10);
        assert_eq!(body["isReminderOn"], true);
        assert_eq!(
            body["recurrence"],
            json!({
                "pattern": {"interval": 1, "type": "weekly", "daysOfWeek": ["tuesday"], "firstDayOfWeek": "sunday"},
                "range": {"startDate": "2026-10-06", "recurrenceTimeZone": "Eastern Standard Time",
                          "type": "endDate", "endDate": "2026-12-15"}
            })
        );
        assert_eq!(body["attendees"], json!([]));
    }

    #[test]
    fn updates_send_only_what_changed() {
        let served = object(SERIES);
        let wanted = object(&SERIES.replace("SUMMARY:Standup", "SUMMARY:Daily sync"));
        let changes = changes(&served, &wanted, &context()).unwrap();
        assert_eq!(changes, [Change::Patch(json!({"subject": "Daily sync"}))]);
    }

    #[test]
    fn moving_an_event_sends_both_times() {
        let served = object(SERIES);
        let wanted = object(&SERIES.replace(
            "DTEND;TZID=America/New_York:20261006T100000",
            "DTEND;TZID=America/New_York:20261006T103000",
        ));
        let changes = changes(&served, &wanted, &context()).unwrap();
        let [Change::Patch(patch)] = changes.as_slice() else {
            panic!("{changes:?}");
        };
        assert_eq!(patch["end"]["dateTime"], "2026-10-06T10:30:00");
        assert_eq!(patch["start"]["dateTime"], "2026-10-06T09:30:00");
    }

    #[test]
    fn exdates_and_overrides_become_occurrence_changes() {
        let served = object(SERIES);
        let with_exdate = SERIES.replace(
            "RRULE",
            "EXDATE;TZID=America/New_York:20261013T093000\r\nRRULE",
        );
        let moved = "BEGIN:VEVENT\r\nUID:u1\r\nRECURRENCE-ID;TZID=America/New_York:20261020T093000\r\n\
            SUMMARY:Standup\r\nDTSTART;TZID=America/New_York:20261020T110000\r\n\
            DTEND;TZID=America/New_York:20261020T113000\r\n\
            BEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT10M\r\nEND:VALARM\r\nEND:VEVENT\r\n";
        let wanted = object(&format!("{with_exdate}{moved}"));
        let changes = changes(&served, &wanted, &context()).unwrap();
        let cancelled = Utc.with_ymd_and_hms(2026, 10, 13, 13, 30, 0).unwrap();
        let original = Utc.with_ymd_and_hms(2026, 10, 20, 13, 30, 0).unwrap();
        assert_eq!(changes[0], Change::DeleteOccurrence(cancelled));
        let Change::PatchOccurrence(instant, patch) = &changes[1] else {
            panic!("{changes:?}");
        };
        assert_eq!(*instant, original);
        assert_eq!(patch["start"]["dateTime"], "2026-10-20T11:00:00");
        assert!(patch.get("subject").is_none(), "{patch}");
    }

    #[test]
    fn attendees_answer_instead_of_editing() {
        let invitation = "BEGIN:VEVENT\r\nUID:u2\r\nSUMMARY:Review\r\n\
            DTSTART:20261007T180000Z\r\nDTEND:20261007T190000Z\r\n\
            ORGANIZER:mailto:boss@example.com\r\n\
            ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@example.com\r\nEND:VEVENT\r\n";
        let served = object(invitation);
        let wanted = object(
            &invitation
                .replace("NEEDS-ACTION", "ACCEPTED")
                .replace("SUMMARY:Review", "SUMMARY:Renamed locally"),
        );
        assert_eq!(
            changes(&served, &wanted, &context()).unwrap(),
            [Change::Respond("accept")]
        );
    }

    #[test]
    fn rejects_rules_graph_cannot_hold() {
        let master = object(&SERIES.replace(
            "RRULE:FREQ=WEEKLY;BYDAY=TU;UNTIL=20261216T045959Z",
            "RRULE:FREQ=HOURLY",
        ))
        .master
        .unwrap();
        assert!(create_body(&master, &context()).is_err());
        let last_day = object(&SERIES.replace(
            "RRULE:FREQ=WEEKLY;BYDAY=TU;UNTIL=20261216T045959Z",
            "RRULE:FREQ=MONTHLY;BYMONTHDAY=-1",
        ))
        .master
        .unwrap();
        assert!(create_body(&last_day, &context()).is_err());
    }

    #[test]
    fn all_day_events_and_weekday_rules() {
        let master = object(
            "BEGIN:VEVENT\r\nUID:u3\r\nSUMMARY:Off\r\nDTSTART;VALUE=DATE:20261012\r\n\
             RRULE:FREQ=DAILY;BYDAY=MO,TU,WE,TH,FR;COUNT=5\r\nEND:VEVENT\r\n",
        )
        .master
        .unwrap();
        let body = create_body(&master, &context()).unwrap();
        assert_eq!(body["isAllDay"], true);
        assert_eq!(body["end"]["dateTime"], "2026-10-13T00:00:00");
        assert_eq!(body["recurrence"]["pattern"]["type"], "weekly");
        assert_eq!(body["recurrence"]["range"]["numberOfOccurrences"], 5);
    }

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration("-PT15M"), Some(-900));
        assert_eq!(parse_duration("P1DT2H"), Some(93_600));
        assert_eq!(parse_duration("P1W"), Some(604_800));
        assert_eq!(parse_duration("PT"), Some(0));
        assert_eq!(parse_duration("15M"), None);
    }
}
