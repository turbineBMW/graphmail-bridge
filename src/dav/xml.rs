// SPDX-License-Identifier: GPL-2.0-or-later

//! WebDAV XML: a small namespace-aware tree for request bodies and a writer
//! for `207 Multi-Status` responses.

use anyhow::{Result, bail};
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;

pub const DAV: &str = "DAV:";
pub const CALDAV: &str = "urn:ietf:params:xml:ns:caldav";
pub const CARDDAV: &str = "urn:ietf:params:xml:ns:carddav";
pub const CALSERVER: &str = "http://calendarserver.org/ns/";
pub const APPLE_ICAL: &str = "http://apple.com/ns/ical/";

/// Prefixes declared on every multistatus root.
const PREFIXES: &[(&str, &str)] = &[
    ("D", DAV),
    ("C", CALDAV),
    ("CR", CARDDAV),
    ("CS", CALSERVER),
    ("IC", APPLE_ICAL),
];

/// Request bodies larger than this are refused.
pub const MAX_BODY: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub struct Element {
    pub ns: String,
    pub name: String,
    pub attributes: Vec<(String, String)>,
    pub children: Vec<Element>,
    pub text: String,
}

impl Element {
    pub fn is(&self, ns: &str, name: &str) -> bool {
        self.ns == ns && self.name == name
    }

    pub fn child(&self, ns: &str, name: &str) -> Option<&Element> {
        self.children.iter().find(|child| child.is(ns, name))
    }

    pub fn children_named<'a>(
        &'a self,
        ns: &'a str,
        name: &'a str,
    ) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |child| child.is(ns, name))
    }

    /// The first descendant (depth first) with this name.
    pub fn find(&self, ns: &str, name: &str) -> Option<&Element> {
        for child in &self.children {
            if child.is(ns, name) {
                return Some(child);
            }
            if let Some(found) = child.find(ns, name) {
                return Some(found);
            }
        }
        None
    }

    pub fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Parse a request body into its root element.
pub fn parse(body: &str) -> Result<Element> {
    let mut reader = NsReader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut stack: Vec<Element> = Vec::new();
    loop {
        let (resolved, event) = reader.read_resolved_event()?;
        let ns = match resolved {
            ResolveResult::Bound(namespace) => String::from_utf8_lossy(namespace.0).into_owned(),
            _ => String::new(),
        };
        let is_empty = matches!(event, Event::Empty(_));
        match event {
            Event::Start(start) | Event::Empty(start) => {
                let mut element = Element {
                    ns,
                    name: String::from_utf8_lossy(start.local_name().as_ref()).into_owned(),
                    ..Element::default()
                };
                for attribute in start.attributes().flatten() {
                    element.attributes.push((
                        String::from_utf8_lossy(attribute.key.local_name().as_ref()).into_owned(),
                        attribute.unescape_value()?.into_owned(),
                    ));
                }
                if is_empty {
                    match stack.last_mut() {
                        Some(parent) => parent.children.push(element),
                        None => return Ok(element),
                    }
                } else {
                    stack.push(element);
                }
            }
            Event::End(_) => {
                let Some(element) = stack.pop() else {
                    bail!("unbalanced XML");
                };
                match stack.last_mut() {
                    Some(parent) => parent.children.push(element),
                    None => return Ok(element),
                }
            }
            Event::Text(text) => {
                if let Some(current) = stack.last_mut() {
                    current.text.push_str(&text.xml_content()?);
                }
            }
            Event::CData(data) => {
                if let Some(current) = stack.last_mut() {
                    current.text.push_str(&String::from_utf8_lossy(&data));
                }
            }
            Event::GeneralRef(reference) => {
                if let Some(current) = stack.last_mut() {
                    let entity = format!("&{};", reference.decode()?);
                    current
                        .text
                        .push_str(&quick_xml::escape::unescape(&entity)?);
                }
            }
            Event::Eof => bail!("XML ended before the root element closed"),
            _ => {}
        }
    }
}

pub fn escape(text: &str) -> String {
    quick_xml::escape::escape(text).into_owned()
}

/// A property name: namespace and local name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropName {
    pub ns: String,
    pub name: String,
}

impl PropName {
    pub fn new(ns: &str, name: &str) -> Self {
        Self {
            ns: ns.to_owned(),
            name: name.to_owned(),
        }
    }

    pub fn is(&self, ns: &str, name: &str) -> bool {
        self.ns == ns && self.name == name
    }

    fn open(&self) -> (String, Option<String>) {
        match PREFIXES.iter().find(|(_, uri)| *uri == self.ns) {
            Some((prefix, _)) => (format!("{prefix}:{}", self.name), None),
            None if self.ns.is_empty() => (self.name.clone(), None),
            None => (
                format!("X:{}", self.name),
                Some(format!(" xmlns:X=\"{}\"", escape(&self.ns))),
            ),
        }
    }

    /// `<prefix:name>inner</prefix:name>`, or an empty element.
    pub fn element(&self, inner: &str) -> String {
        let (qualified, declaration) = self.open();
        let declaration = declaration.unwrap_or_default();
        if inner.is_empty() {
            format!("<{qualified}{declaration}/>")
        } else {
            format!("<{qualified}{declaration}>{inner}</{qualified}>")
        }
    }
}

/// Which properties a PROPFIND or REPORT asked for.
#[derive(Clone, Debug)]
pub enum Requested {
    /// `allprop` or an empty body.
    All,
    /// `propname`: names only, no values.
    Names,
    Props(Vec<PropName>),
}

impl Requested {
    /// Read the `prop`/`allprop`/`propname` child of a request element.
    pub fn from_request(root: Option<&Element>) -> Self {
        let Some(root) = root else {
            return Self::All;
        };
        if let Some(prop) = root.child(DAV, "prop") {
            return Self::Props(
                prop.children
                    .iter()
                    .map(|child| PropName::new(&child.ns, &child.name))
                    .collect(),
            );
        }
        if root.child(DAV, "propname").is_some() {
            return Self::Names;
        }
        Self::All
    }
}

/// Accumulates `<D:response>` elements into a multistatus body.
pub struct Multistatus {
    body: String,
}

impl Default for Multistatus {
    fn default() -> Self {
        Self::new()
    }
}

impl Multistatus {
    pub fn new() -> Self {
        let mut body = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus");
        for (prefix, uri) in PREFIXES {
            body.push_str(&format!(" xmlns:{prefix}=\"{uri}\""));
        }
        body.push('>');
        Self { body }
    }

    /// A resource's properties: those it has under 200, the rest under 404.
    /// `available` holds every property with its inner XML.
    pub fn props(&mut self, href: &str, requested: &Requested, available: &[(PropName, String)]) {
        let (found, missing): (Vec<String>, Vec<String>) = match requested {
            Requested::All => (
                available
                    .iter()
                    .map(|(name, value)| name.element(value))
                    .collect(),
                Vec::new(),
            ),
            Requested::Names => (
                available.iter().map(|(name, _)| name.element("")).collect(),
                Vec::new(),
            ),
            Requested::Props(names) => {
                let mut found = Vec::new();
                let mut missing = Vec::new();
                for name in names {
                    match available.iter().find(|(candidate, _)| candidate == name) {
                        Some((name, value)) => found.push(name.element(value)),
                        None => missing.push(name.element("")),
                    }
                }
                (found, missing)
            }
        };
        self.body.push_str("<D:response><D:href>");
        self.body.push_str(&escape(href));
        self.body.push_str("</D:href>");
        for (props, status) in [(found, "200 OK"), (missing, "404 Not Found")] {
            if props.is_empty() {
                continue;
            }
            self.body.push_str("<D:propstat><D:prop>");
            for prop in props {
                self.body.push_str(&prop);
            }
            self.body.push_str(&format!(
                "</D:prop><D:status>HTTP/1.1 {status}</D:status></D:propstat>"
            ));
        }
        self.body.push_str("</D:response>");
    }

    /// A response carrying only a status, such as a 404 in a multiget.
    pub fn status(&mut self, href: &str, status: &str) {
        self.body.push_str(&format!(
            "<D:response><D:href>{}</D:href><D:status>HTTP/1.1 {status}</D:status></D:response>",
            escape(href)
        ));
    }

    pub fn sync_token(&mut self, token: &str) {
        self.body
            .push_str(&format!("<D:sync-token>{}</D:sync-token>", escape(token)));
    }

    pub fn finish(mut self) -> String {
        self.body.push_str("</D:multistatus>\n");
        self.body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_propfind_with_foreign_namespaces() {
        let body = r#"<?xml version="1.0"?>
            <propfind xmlns="DAV:" xmlns:cs="http://calendarserver.org/ns/">
              <prop><resourcetype/><cs:getctag/><x:odd xmlns:x="urn:x"/></prop>
            </propfind>"#;
        let root = parse(body).unwrap();
        assert!(root.is(DAV, "propfind"));
        let Requested::Props(names) = Requested::from_request(Some(&root)) else {
            panic!("expected props");
        };
        assert_eq!(
            names,
            [
                PropName::new(DAV, "resourcetype"),
                PropName::new(CALSERVER, "getctag"),
                PropName::new("urn:x", "odd")
            ]
        );
    }

    #[test]
    fn decodes_entities_in_text() {
        let root = parse("<D:href xmlns:D=\"DAV:\">/a?b=1&amp;c=2</D:href>").unwrap();
        assert_eq!(root.text, "/a?b=1&c=2");
    }

    #[test]
    fn writes_found_and_missing_props_separately() {
        let mut multistatus = Multistatus::new();
        multistatus.props(
            "/dav/x/",
            &Requested::Props(vec![
                PropName::new(DAV, "displayname"),
                PropName::new("urn:x", "odd"),
            ]),
            &[(PropName::new(DAV, "displayname"), escape("A & B"))],
        );
        let body = multistatus.finish();
        assert!(body.contains(
            "<D:propstat><D:prop><D:displayname>A &amp; B</D:displayname></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>"
        ));
        assert!(
            body.contains("<X:odd xmlns:X=\"urn:x\"/></D:prop><D:status>HTTP/1.1 404 Not Found")
        );
        assert!(parse(&body).is_ok());
    }
}
