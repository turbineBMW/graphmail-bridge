// SPDX-License-Identifier: GPL-2.0-or-later

//! Just enough RFC 5545 (iCalendar) and RFC 6350 (vCard) to read and write
//! the objects CalDAV and CardDAV clients exchange: nested components,
//! content lines with parameters, line folding, and TEXT escaping.

use anyhow::{Result, bail};

/// Longest line, in octets, before it is folded (RFC 5545 §3.1).
const FOLD_OCTETS: usize = 75;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Component {
    pub name: String,
    pub properties: Vec<Property>,
    pub components: Vec<Component>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Property {
    pub name: String,
    /// Parameter names upper-cased, values unquoted.
    pub params: Vec<(String, String)>,
    /// The raw value, still escaped when it is TEXT.
    pub value: String,
}

impl Component {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_ascii_uppercase(),
            ..Self::default()
        }
    }

    pub fn property(&self, name: &str) -> Option<&Property> {
        self.properties
            .iter()
            .find(|property| property.name.eq_ignore_ascii_case(name))
    }

    pub fn properties<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Property> + 'a {
        self.properties
            .iter()
            .filter(move |property| property.name.eq_ignore_ascii_case(name))
    }

    /// The unescaped TEXT value of the first property called `name`.
    pub fn text(&self, name: &str) -> Option<String> {
        self.property(name).map(Property::text)
    }

    pub fn children<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Component> + 'a {
        self.components
            .iter()
            .filter(move |component| component.name.eq_ignore_ascii_case(name))
    }

    pub fn push(&mut self, property: Property) -> &mut Self {
        self.properties.push(property);
        self
    }

    /// Add a TEXT property, escaping the value; empty values are skipped.
    pub fn push_text(&mut self, name: &str, value: &str) -> &mut Self {
        if !value.is_empty() {
            self.properties.push(Property::text_value(name, value));
        }
        self
    }

    /// Serialize with CRLF line ends and folding.
    pub fn to_string_folded(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        push_folded(out, &format!("BEGIN:{}", self.name));
        for property in &self.properties {
            push_folded(out, &property.to_line());
        }
        for component in &self.components {
            component.write(out);
        }
        push_folded(out, &format!("END:{}", self.name));
    }
}

impl Property {
    pub fn new(name: &str, value: impl Into<String>) -> Self {
        Self {
            name: name.to_ascii_uppercase(),
            params: Vec::new(),
            value: value.into(),
        }
    }

    pub fn text_value(name: &str, value: &str) -> Self {
        Self::new(name, escape_text(value))
    }

    pub fn param(mut self, name: &str, value: impl Into<String>) -> Self {
        self.params.push((name.to_ascii_uppercase(), value.into()));
        self
    }

    pub fn get_param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn text(&self) -> String {
        unescape_text(&self.value)
    }

    fn to_line(&self) -> String {
        let mut line = self.name.clone();
        for (name, value) in &self.params {
            line.push(';');
            line.push_str(name);
            line.push('=');
            // Parameter values cannot be escaped, only quoted; a quote
            // itself has no representation and is dropped.
            let value = value.replace(['"', '\r', '\n'], "");
            if value.contains([':', ';', ',']) {
                line.push('"');
                line.push_str(&value);
                line.push('"');
            } else {
                line.push_str(&value);
            }
        }
        line.push(':');
        line.push_str(&self.value.replace(['\r', '\n'], ""));
        line
    }
}

/// Parse a single top-level component (a `VCALENDAR` or a `VCARD`).
pub fn parse(input: &str) -> Result<Component> {
    let mut stack: Vec<Component> = Vec::new();
    let mut root = None;
    for line in unfold(input) {
        if line.trim().is_empty() {
            continue;
        }
        let property = parse_line(&line)?;
        if property.name == "BEGIN" {
            if root.is_some() {
                bail!("content after the end of the object");
            }
            stack.push(Component::new(&property.value));
        } else if property.name == "END" {
            let Some(component) = stack.pop() else {
                bail!("END:{} without BEGIN", property.value);
            };
            if !component.name.eq_ignore_ascii_case(&property.value) {
                bail!("END:{} closes BEGIN:{}", property.value, component.name);
            }
            match stack.last_mut() {
                Some(parent) => parent.components.push(component),
                None => root = Some(component),
            }
        } else {
            let Some(current) = stack.last_mut() else {
                bail!("property {} outside of any component", property.name);
            };
            current.properties.push(property);
        }
    }
    if !stack.is_empty() {
        bail!("unterminated BEGIN:{}", stack[0].name);
    }
    root.ok_or_else(|| anyhow::anyhow!("no BEGIN line"))
}

fn unfold(input: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in input.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(continuation) = raw.strip_prefix([' ', '\t'])
            && let Some(last) = lines.last_mut()
        {
            last.push_str(continuation);
        } else {
            lines.push(raw.to_owned());
        }
    }
    lines
}

fn parse_line(line: &str) -> Result<Property> {
    // Find the ':' that ends the name and parameters, skipping quoted
    // parameter values, which may contain ':'.
    let mut in_quotes = false;
    let mut split = None;
    for (index, character) in line.char_indices() {
        match character {
            '"' => in_quotes = !in_quotes,
            ':' if !in_quotes => {
                split = Some(index);
                break;
            }
            _ => {}
        }
    }
    let Some(split) = split else {
        bail!("content line without ':': {line:?}");
    };
    let (head, value) = (&line[..split], &line[split + 1..]);
    let mut parts = split_unquoted(head, ';').into_iter();
    let name = parts.next().unwrap_or_default().trim().to_ascii_uppercase();
    if name.is_empty() {
        bail!("content line without a name: {line:?}");
    }
    // A vCard property may carry a group prefix ("item1.EMAIL"); keep only
    // the name, since nothing here relies on groups.
    let name = name.rsplit('.').next().unwrap_or(&name).to_owned();
    let mut params = Vec::new();
    for part in parts {
        let (key, value) = part.split_once('=').unwrap_or((part.as_str(), ""));
        params.push((key.trim().to_ascii_uppercase(), value.replace('"', "")));
    }
    Ok(Property {
        name,
        params,
        value: value.to_owned(),
    })
}

fn split_unquoted(text: &str, separator: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for character in text.chars() {
        if character == '"' {
            in_quotes = !in_quotes;
        }
        if character == separator && !in_quotes {
            parts.push(std::mem::take(&mut current));
        } else {
            current.push(character);
        }
    }
    parts.push(current);
    parts
}

fn push_folded(out: &mut String, line: &str) {
    let mut width = 0;
    for character in line.chars() {
        let size = character.len_utf8();
        if width + size > FOLD_OCTETS {
            out.push_str("\r\n ");
            width = 1;
        }
        out.push(character);
        width += size;
    }
    out.push_str("\r\n");
}

pub fn escape_text(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    let value = value.replace("\r\n", "\n");
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            ';' => escaped.push_str("\\;"),
            ',' => escaped.push_str("\\,"),
            '\n' => escaped.push_str("\\n"),
            '\r' => {}
            other => escaped.push(other),
        }
    }
    escaped
}

pub fn unescape_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match characters.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// Split a structured or list value on unescaped `separator`, keeping the
/// pieces escaped.
pub fn split_value(value: &str, separator: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            current.push('\\');
            current.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == separator {
            parts.push(std::mem::take(&mut current));
        } else {
            current.push(character);
        }
    }
    if escaped {
        current.push('\\');
    }
    parts.push(current);
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_components_with_folding_and_quoted_params() {
        let input = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:abc\r\n\
                     SUMMARY:Plan\\, then\r\n  ship\r\n\
                     ATTENDEE;CN=\"Doe; Jane\";PARTSTAT=ACCEPTED:mailto:jane@example.com\r\n\
                     END:VEVENT\r\nEND:VCALENDAR\r\n";
        let calendar = parse(input).unwrap();
        assert_eq!(calendar.name, "VCALENDAR");
        let event = calendar.children("VEVENT").next().unwrap();
        assert_eq!(event.text("SUMMARY").unwrap(), "Plan, then ship");
        let attendee = event.property("attendee").unwrap();
        assert_eq!(attendee.get_param("cn"), Some("Doe; Jane"));
        assert_eq!(attendee.value, "mailto:jane@example.com");
    }

    #[test]
    fn rejects_mismatched_components() {
        assert!(parse("BEGIN:VCALENDAR\nBEGIN:VEVENT\nEND:VCALENDAR\n").is_err());
        assert!(parse("BEGIN:VCALENDAR\n").is_err());
    }

    #[test]
    fn folds_long_lines_on_character_boundaries() {
        let mut event = Component::new("VEVENT");
        event.push_text("SUMMARY", &"é".repeat(60));
        let text = event.to_string_folded();
        for line in text.split("\r\n") {
            assert!(line.len() <= FOLD_OCTETS, "{} octets", line.len());
        }
        let round = parse(&text).unwrap();
        assert_eq!(round.text("SUMMARY").unwrap(), "é".repeat(60));
    }

    #[test]
    fn text_escaping_round_trips() {
        let original = "a;b,c\\d\nnext";
        assert_eq!(unescape_text(&escape_text(original)), original);
    }

    #[test]
    fn quotes_parameter_values_that_need_it() {
        let property = Property::new("ATTENDEE", "mailto:a@b").param("CN", "Doe, Jane");
        assert_eq!(property.to_line(), "ATTENDEE;CN=\"Doe, Jane\":mailto:a@b");
    }

    #[test]
    fn splits_structured_values_on_unescaped_separators() {
        assert_eq!(
            split_value("Doe;Jane\\;Ann;;", ';'),
            ["Doe", "Jane\\;Ann", "", ""]
        );
    }

    #[test]
    fn keeps_vcard_names_without_group_prefix() {
        let card = parse("BEGIN:VCARD\nitem1.EMAIL;TYPE=work:a@b\nEND:VCARD\n").unwrap();
        assert_eq!(card.property("EMAIL").unwrap().value, "a@b");
    }
}
