// SPDX-License-Identifier: GPL-2.0-or-later

//! Time zones between Microsoft Graph (Windows names, sometimes IANA) and
//! iCalendar (IANA `TZID`s with `VTIMEZONE` definitions).

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, Offset, TimeZone, Utc};
use chrono_tz::{OffsetName, Tz};

use crate::ical::{Component, Property};
use crate::windows_zones::{IANA_TO_WINDOWS, WINDOWS_TO_IANA};

/// Resolve a zone name from Graph or from a client: an IANA name, a Windows
/// name, or a libical-style TZID such as
/// `/freeassociation.sourceforge.net/America/New_York`.
pub fn resolve(name: &str) -> Option<Tz> {
    let name = name.trim();
    if let Ok(tz) = name.parse::<Tz>() {
        return Some(tz);
    }
    if let Ok(index) = WINDOWS_TO_IANA.binary_search_by(|(windows, _)| windows.cmp(&name)) {
        return WINDOWS_TO_IANA[index].1.parse().ok();
    }
    // Drop leading path segments until an IANA name remains.
    let mut rest = name;
    while let Some((_, tail)) = rest.split_once('/') {
        if let Ok(tz) = tail.parse::<Tz>() {
            return Some(tz);
        }
        rest = tail;
    }
    None
}

/// The Windows name of an IANA zone, for Graph writes that need one.
pub fn windows_name(tz: Tz) -> Option<&'static str> {
    let name = tz.name();
    IANA_TO_WINDOWS
        .binary_search_by(|(iana, _)| iana.cmp(&name))
        .ok()
        .map(|index| IANA_TO_WINDOWS[index].1)
}

/// The machine's zone: `$TZ`, else the `/etc/localtime` link, else UTC.
pub fn local_zone() -> Tz {
    if let Ok(name) = std::env::var("TZ")
        && let Some(tz) = resolve(name.trim_start_matches(':'))
    {
        return tz;
    }
    if let Ok(target) = std::fs::read_link("/etc/localtime")
        && let Some(text) = target.to_str()
        && let Some((_, name)) = text.split_once("zoneinfo/")
        && let Ok(tz) = name.parse()
    {
        return tz;
    }
    Tz::UTC
}

/// A `VTIMEZONE` for `tz`, with yearly rules read off the zone's transitions
/// in `year`. Zones without daylight saving get a single `STANDARD` block.
pub fn vtimezone(tz: Tz, year: i32) -> Component {
    let mut component = Component::new("VTIMEZONE");
    component.push(Property::new("TZID", tz.name()));
    let transitions = transitions_in(tz, year);
    if transitions.len() != 2 {
        let at = Utc
            .with_ymd_and_hms(year, 1, 1, 0, 0, 0)
            .single()
            .unwrap_or_else(Utc::now);
        let offset = tz.offset_from_utc_datetime(&at.naive_utc());
        let seconds = offset.fix().local_minus_utc();
        let mut standard = Component::new("STANDARD");
        standard
            .push(Property::new("DTSTART", "19700101T000000"))
            .push(Property::new("TZOFFSETFROM", format_offset(seconds)))
            .push(Property::new("TZOFFSETTO", format_offset(seconds)));
        if let Some(abbreviation) = offset.abbreviation() {
            standard.push(Property::new("TZNAME", abbreviation));
        }
        component.components.push(standard);
        return component;
    }
    for transition in transitions {
        let kind = if transition.to > transition.from {
            "DAYLIGHT"
        } else {
            "STANDARD"
        };
        let local = transition.at.naive_utc() + Duration::seconds(transition.from as i64);
        let date = local.date();
        let (ordinal, weekday) = weekday_rule(date);
        let first = nth_weekday_1970(date.month(), ordinal, date.weekday())
            .unwrap_or(date)
            .and_time(local.time());
        let mut observance = Component::new(kind);
        observance
            .push(Property::new("DTSTART", format_local(first)))
            .push(Property::new(
                "RRULE",
                format!(
                    "FREQ=YEARLY;BYMONTH={};BYDAY={ordinal}{weekday}",
                    date.month()
                ),
            ))
            .push(Property::new(
                "TZOFFSETFROM",
                format_offset(transition.from),
            ))
            .push(Property::new("TZOFFSETTO", format_offset(transition.to)));
        if let Some(name) = transition.name {
            observance.push(Property::new("TZNAME", name));
        }
        component.components.push(observance);
    }
    component
}

struct Transition {
    at: DateTime<Utc>,
    from: i32,
    to: i32,
    name: Option<String>,
}

/// Offset changes during `year`, found day by day and then hour by hour.
fn transitions_in(tz: Tz, year: i32) -> Vec<Transition> {
    let offset_at = |at: DateTime<Utc>| {
        tz.offset_from_utc_datetime(&at.naive_utc())
            .fix()
            .local_minus_utc()
    };
    let Some(start) = Utc.with_ymd_and_hms(year, 1, 1, 0, 0, 0).single() else {
        return Vec::new();
    };
    let mut transitions = Vec::new();
    let mut previous = offset_at(start);
    for day in 1..=366 {
        let at = start + Duration::days(day);
        if at.year() != year {
            break;
        }
        let current = offset_at(at);
        if current == previous {
            continue;
        }
        let mut moment = at - Duration::days(1);
        while offset_at(moment) == previous {
            moment += Duration::minutes(15);
        }
        transitions.push(Transition {
            at: moment,
            from: previous,
            to: current,
            name: tz
                .offset_from_utc_datetime(&moment.naive_utc())
                .abbreviation()
                .map(str::to_owned),
        });
        previous = current;
    }
    transitions
}

/// `(ordinal, weekday)` for an RRULE BYDAY: the n-th weekday of the month,
/// or -1 when it is the last one.
fn weekday_rule(date: NaiveDate) -> (i32, &'static str) {
    let ordinal = if date.day() + 7 > days_in_month(date) {
        -1
    } else {
        ((date.day() - 1) / 7 + 1) as i32
    };
    (ordinal, weekday_code(date.weekday()))
}

fn nth_weekday_1970(month: u32, ordinal: i32, weekday: chrono::Weekday) -> Option<NaiveDate> {
    if ordinal > 0 {
        NaiveDate::from_weekday_of_month_opt(1970, month, weekday, ordinal as u8)
    } else {
        let mut date = NaiveDate::from_ymd_opt(1970, month, 1)?;
        let last = days_in_month(date);
        date = date.with_day(last)?;
        while date.weekday() != weekday {
            date = date.pred_opt()?;
        }
        Some(date)
    }
}

fn days_in_month(date: NaiveDate) -> u32 {
    let (year, month) = (date.year(), date.month());
    let next = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)
    };
    next.and_then(|next| next.pred_opt())
        .map(|last| last.day())
        .unwrap_or(31)
}

pub fn weekday_code(weekday: chrono::Weekday) -> &'static str {
    match weekday {
        chrono::Weekday::Mon => "MO",
        chrono::Weekday::Tue => "TU",
        chrono::Weekday::Wed => "WE",
        chrono::Weekday::Thu => "TH",
        chrono::Weekday::Fri => "FR",
        chrono::Weekday::Sat => "SA",
        chrono::Weekday::Sun => "SU",
    }
}

fn format_offset(seconds: i32) -> String {
    let sign = if seconds < 0 { '-' } else { '+' };
    let seconds = seconds.abs();
    format!("{sign}{:02}{:02}", seconds / 3600, (seconds % 3600) / 60)
}

pub fn format_local(value: NaiveDateTime) -> String {
    value.format("%Y%m%dT%H%M%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_windows_iana_and_libical_names() {
        assert_eq!(
            resolve("Eastern Standard Time"),
            Some(Tz::America__New_York)
        );
        assert_eq!(resolve("America/New_York"), Some(Tz::America__New_York));
        assert_eq!(
            resolve("/freeassociation.sourceforge.net/Tzfile/Europe/Berlin"),
            Some(Tz::Europe__Berlin)
        );
        assert_eq!(resolve("UTC"), Some(Tz::UTC));
        assert_eq!(resolve("Nowhere Standard Time"), None);
    }

    #[test]
    fn maps_iana_back_to_windows() {
        assert_eq!(
            windows_name(Tz::America__New_York),
            Some("Eastern Standard Time")
        );
        assert_eq!(
            windows_name(Tz::America__Toronto),
            Some("Eastern Standard Time")
        );
    }

    #[test]
    fn tables_are_sorted_for_binary_search() {
        assert!(WINDOWS_TO_IANA.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert!(IANA_TO_WINDOWS.windows(2).all(|pair| pair[0].0 < pair[1].0));
    }

    #[test]
    fn new_york_rules_match_the_us_dst_law() {
        let text = vtimezone(Tz::America__New_York, 2026).to_string_folded();
        assert!(text.contains("TZID:America/New_York"));
        assert!(text.contains("BEGIN:DAYLIGHT\r\nDTSTART:19700308T020000\r\nRRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=2SU\r\nTZOFFSETFROM:-0500\r\nTZOFFSETTO:-0400"));
        assert!(text.contains("BEGIN:STANDARD\r\nDTSTART:19701101T020000\r\nRRULE:FREQ=YEARLY;BYMONTH=11;BYDAY=1SU\r\nTZOFFSETFROM:-0400\r\nTZOFFSETTO:-0500"));
    }

    #[test]
    fn europe_uses_last_sunday_rules() {
        let text = vtimezone(Tz::Europe__Berlin, 2026).to_string_folded();
        assert!(text.contains("BYMONTH=3;BYDAY=-1SU"));
        assert!(text.contains("BYMONTH=10;BYDAY=-1SU"));
        assert!(text.contains("DTSTART:19700329T020000"));
        assert!(text.contains("DTSTART:19701025T030000"));
    }

    #[test]
    fn zones_without_dst_have_one_observance() {
        let text = vtimezone(Tz::Asia__Tokyo, 2026).to_string_folded();
        assert!(text.contains("TZOFFSETFROM:+0900\r\nTZOFFSETTO:+0900"));
        assert!(!text.contains("DAYLIGHT"));
    }
}
