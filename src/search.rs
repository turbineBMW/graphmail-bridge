// SPDX-License-Identifier: GPL-2.0-or-later

//! RFC 3501 SEARCH criteria → SQL over the local message index.
//!
//! Text keys use IMAP's case-insensitive substring semantics, evaluated with
//! `instr()` against lower-cased shadow columns the store maintains. Keys the
//! bridge cannot evaluate faithfully are rejected rather than silently matching
//! everything.

use anyhow::{Context, Result, bail};
use chrono::NaiveDate;
use mailrs_imap_proto::{SequenceSet, parse_sequence_set};
use rusqlite::types::Value;

use crate::imap::PINNED_KEYWORD;
use crate::store::SearchSql;

#[derive(Clone, Debug, PartialEq)]
pub enum Criterion {
    All,
    Never,
    Seen(bool),
    Deleted(bool),
    Flagged(bool),
    Draft(bool),
    /// The `$Pinned` keyword: Outlook's pin-to-top.
    Pinned(bool),
    Uid(SequenceSet),
    Seq(SequenceSet),
    From(String),
    To(String),
    Cc(String),
    Bcc(String),
    Subject(String),
    /// BODY: cached body text only.
    Body(String),
    /// TEXT: every header column, the preview, and the cached body text.
    Text(String),
    MessageId(String),
    Before(NaiveDate),
    On(NaiveDate),
    Since(NaiveDate),
    SentBefore(NaiveDate),
    SentOn(NaiveDate),
    SentSince(NaiveDate),
    Larger(u64),
    Smaller(u64),
    Not(Box<Criterion>),
    Or(Box<Criterion>, Box<Criterion>),
    And(Vec<Criterion>),
}

/// Session facts SQL cannot see: which UIDs carry the session-local
/// `\Deleted` flag, and the UID at each sequence position (ascending).
pub struct SessionView<'a> {
    pub deleted_uids: &'a [u32],
    pub seq_to_uid: &'a [u32],
}

pub fn parse(criteria: &str) -> Result<Criterion> {
    let tokens = tokenize(criteria)?;
    let mut cursor = Cursor {
        tokens,
        position: 0,
    };
    let mut keys = Vec::new();
    // CHARSET is only valid at the very start.
    if cursor.peek_atom_eq("CHARSET") {
        cursor.next();
        let charset = cursor.next_string("CHARSET needs a value")?;
        if !matches!(
            charset.to_ascii_uppercase().as_str(),
            "UTF-8" | "US-ASCII" | "UTF8"
        ) {
            bail!("SEARCH charset {charset} is not supported");
        }
    }
    while !cursor.at_end() {
        if cursor.peek_is_close() {
            bail!("unbalanced parenthesis in SEARCH");
        }
        keys.push(parse_key(&mut cursor)?);
    }
    Ok(simplify_and(keys))
}

pub fn to_sql(criterion: &Criterion, view: &SessionView<'_>) -> SearchSql {
    let mut params = Vec::new();
    let where_clause = emit(criterion, view, &mut params);
    SearchSql {
        where_clause,
        params,
    }
}

// ----- tokenizer ------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Open,
    Close,
    Atom(String),
    Quoted(String),
}

fn tokenize(input: &str) -> Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(&character) = chars.peek() {
        match character {
            ' ' | '\t' => {
                chars.next();
            }
            '(' => {
                chars.next();
                tokens.push(Token::Open);
            }
            ')' => {
                chars.next();
                tokens.push(Token::Close);
            }
            '"' => {
                chars.next();
                let mut value = String::new();
                loop {
                    match chars.next() {
                        Some('\\') => match chars.next() {
                            Some(escaped) => value.push(escaped),
                            None => bail!("unterminated escape in SEARCH string"),
                        },
                        Some('"') => break,
                        Some(other) => value.push(other),
                        None => bail!("unterminated quoted string in SEARCH"),
                    }
                }
                tokens.push(Token::Quoted(value));
            }
            '{' => bail!("literal SEARCH arguments are not supported; use quoted strings"),
            _ => {
                let mut value = String::new();
                while let Some(&next) = chars.peek() {
                    if matches!(next, ' ' | '\t' | '(' | ')') {
                        break;
                    }
                    value.push(next);
                    chars.next();
                }
                tokens.push(Token::Atom(value));
            }
        }
    }
    Ok(tokens)
}

struct Cursor {
    tokens: Vec<Token>,
    position: usize,
}

impl Cursor {
    fn at_end(&self) -> bool {
        self.position >= self.tokens.len()
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.position).cloned();
        self.position += 1;
        token
    }

    fn peek_is_close(&self) -> bool {
        matches!(self.peek(), Some(Token::Close))
    }

    fn peek_atom_eq(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Token::Atom(atom)) if atom.eq_ignore_ascii_case(word))
    }

    /// The next token as a string argument (atom or quoted).
    fn next_string(&mut self, context: &str) -> Result<String> {
        match self.next() {
            Some(Token::Atom(value)) | Some(Token::Quoted(value)) => Ok(value),
            _ => bail!("{context}"),
        }
    }
}

// ----- parser ---------------------------------------------------------------

fn parse_key(cursor: &mut Cursor) -> Result<Criterion> {
    let token = cursor.next().context("unexpected end of SEARCH criteria")?;
    let word = match token {
        Token::Open => {
            let mut group = Vec::new();
            loop {
                match cursor.peek() {
                    None => bail!("unbalanced parenthesis in SEARCH"),
                    Some(Token::Close) => {
                        cursor.next();
                        break;
                    }
                    _ => group.push(parse_key(cursor)?),
                }
            }
            return Ok(simplify_and(group));
        }
        Token::Close => bail!("unbalanced parenthesis in SEARCH"),
        Token::Quoted(_) => bail!("SEARCH key must be an atom"),
        Token::Atom(word) => word.to_ascii_uppercase(),
    };
    Ok(match word.as_str() {
        "ALL" | "OLD" | "UNANSWERED" => Criterion::All,
        "RECENT" | "NEW" | "ANSWERED" => Criterion::Never,
        "SEEN" => Criterion::Seen(true),
        "UNSEEN" => Criterion::Seen(false),
        "DELETED" => Criterion::Deleted(true),
        "UNDELETED" => Criterion::Deleted(false),
        "FLAGGED" => Criterion::Flagged(true),
        "UNFLAGGED" => Criterion::Flagged(false),
        "DRAFT" => Criterion::Draft(true),
        "UNDRAFT" => Criterion::Draft(false),
        "KEYWORD" => {
            let flag = cursor.next_string("KEYWORD needs a flag")?;
            if flag.eq_ignore_ascii_case(PINNED_KEYWORD) {
                Criterion::Pinned(true)
            } else {
                Criterion::Never
            }
        }
        "UNKEYWORD" => {
            let flag = cursor.next_string("UNKEYWORD needs a flag")?;
            if flag.eq_ignore_ascii_case(PINNED_KEYWORD) {
                Criterion::Pinned(false)
            } else {
                Criterion::All
            }
        }
        "NOT" => Criterion::Not(Box::new(parse_key(cursor)?)),
        "OR" => {
            let left = parse_key(cursor)?;
            let right = parse_key(cursor)?;
            Criterion::Or(Box::new(left), Box::new(right))
        }
        "UID" => {
            let set = cursor.next_string("UID search key needs a set")?;
            Criterion::Uid(parse_sequence_set(&set).map_err(anyhow::Error::msg)?)
        }
        "FROM" => Criterion::From(cursor.next_string("FROM needs a string")?),
        "TO" => Criterion::To(cursor.next_string("TO needs a string")?),
        "CC" => Criterion::Cc(cursor.next_string("CC needs a string")?),
        "BCC" => Criterion::Bcc(cursor.next_string("BCC needs a string")?),
        "SUBJECT" => Criterion::Subject(cursor.next_string("SUBJECT needs a string")?),
        "BODY" => Criterion::Body(cursor.next_string("BODY needs a string")?),
        "TEXT" => Criterion::Text(cursor.next_string("TEXT needs a string")?),
        "HEADER" => {
            let field = cursor.next_string("HEADER needs a field name")?;
            let value = cursor.next_string("HEADER needs a value")?;
            match field.to_ascii_uppercase().as_str() {
                "FROM" => Criterion::From(value),
                "TO" => Criterion::To(value),
                "CC" => Criterion::Cc(value),
                "BCC" => Criterion::Bcc(value),
                "SUBJECT" => Criterion::Subject(value),
                "MESSAGE-ID" => Criterion::MessageId(value),
                other => bail!("SEARCH HEADER {other} is not supported by this bridge"),
            }
        }
        "BEFORE" => Criterion::Before(parse_date(&cursor.next_string("BEFORE needs a date")?)?),
        "ON" => Criterion::On(parse_date(&cursor.next_string("ON needs a date")?)?),
        "SINCE" => Criterion::Since(parse_date(&cursor.next_string("SINCE needs a date")?)?),
        "SENTBEFORE" => {
            Criterion::SentBefore(parse_date(&cursor.next_string("SENTBEFORE needs a date")?)?)
        }
        "SENTON" => Criterion::SentOn(parse_date(&cursor.next_string("SENTON needs a date")?)?),
        "SENTSINCE" => {
            Criterion::SentSince(parse_date(&cursor.next_string("SENTSINCE needs a date")?)?)
        }
        "LARGER" => Criterion::Larger(parse_number(&cursor.next_string("LARGER needs a number")?)?),
        "SMALLER" => Criterion::Smaller(parse_number(
            &cursor.next_string("SMALLER needs a number")?,
        )?),
        other
            if other.starts_with(|character: char| character.is_ascii_digit()) || other == "*" =>
        {
            Criterion::Seq(parse_sequence_set(other).map_err(anyhow::Error::msg)?)
        }
        other => bail!("SEARCH key {other} is not supported by this bridge"),
    })
}

fn simplify_and(mut keys: Vec<Criterion>) -> Criterion {
    match keys.len() {
        0 => Criterion::All,
        1 => keys.remove(0),
        _ => Criterion::And(keys),
    }
}

/// IMAP date: `d-Mon-yyyy` with a one- or two-digit day.
fn parse_date(value: &str) -> Result<NaiveDate> {
    let mut parts = value.trim().splitn(3, '-');
    let (Some(day), Some(month), Some(year)) = (parts.next(), parts.next(), parts.next()) else {
        bail!("invalid SEARCH date {value:?}");
    };
    let day: u32 = day
        .parse()
        .with_context(|| format!("invalid SEARCH date {value:?}"))?;
    let year: i32 = year
        .parse()
        .with_context(|| format!("invalid SEARCH date {value:?}"))?;
    let month = match month.to_ascii_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => bail!("invalid SEARCH date {value:?}"),
    };
    NaiveDate::from_ymd_opt(year, month, day)
        .with_context(|| format!("invalid SEARCH date {value:?}"))
}

fn parse_number(value: &str) -> Result<u64> {
    value
        .parse()
        .with_context(|| format!("invalid SEARCH number {value:?}"))
}

// ----- SQL emission ---------------------------------------------------------

fn emit(criterion: &Criterion, view: &SessionView<'_>, params: &mut Vec<Value>) -> String {
    match criterion {
        Criterion::All => "1=1".into(),
        Criterion::Never => "0=1".into(),
        Criterion::Seen(value) => format!("is_read={}", i32::from(*value)),
        Criterion::Flagged(value) => format!("flagged={}", i32::from(*value)),
        Criterion::Pinned(value) => format!("COALESCE(pinned, 0)={}", i32::from(*value)),
        Criterion::Draft(value) => format!("is_draft={}", i32::from(*value)),
        Criterion::Deleted(value) => {
            let set = uid_list(view.deleted_uids);
            if *value { set } else { format!("NOT ({set})") }
        }
        Criterion::Uid(set) => uid_set_sql(set, view.seq_to_uid.last().copied().unwrap_or(0)),
        Criterion::Seq(set) => seq_set_sql(set, view.seq_to_uid),
        Criterion::From(term) => substring("from_lc", term, params),
        Criterion::To(term) => substring("to_lc", term, params),
        Criterion::Cc(term) => substring("cc_lc", term, params),
        Criterion::Bcc(term) => substring("bcc_lc", term, params),
        Criterion::Subject(term) => substring("subject_lc", term, params),
        Criterion::MessageId(term) => substring("lower(internet_message_id)", term, params),
        Criterion::Body(term) => body_sql(term, params),
        Criterion::Text(term) => {
            let columns = [
                "subject_lc",
                "from_lc",
                "to_lc",
                "cc_lc",
                "bcc_lc",
                "preview_lc",
            ]
            .iter()
            .map(|column| substring(column, term, params))
            .collect::<Vec<_>>();
            format!("({} OR {})", columns.join(" OR "), body_sql(term, params))
        }
        Criterion::Before(date) => format!("received_at < {}", day_start(*date)),
        Criterion::Since(date) => format!("received_at >= {}", day_start(*date)),
        Criterion::On(date) => format!(
            "(received_at >= {} AND received_at < {})",
            day_start(*date),
            day_start(*date) + 86_400
        ),
        Criterion::SentBefore(date) => format!("sent_at < {}", day_start(*date)),
        Criterion::SentSince(date) => format!("sent_at >= {}", day_start(*date)),
        Criterion::SentOn(date) => format!(
            "(sent_at >= {} AND sent_at < {})",
            day_start(*date),
            day_start(*date) + 86_400
        ),
        Criterion::Larger(size) => format!("size > {size}"),
        Criterion::Smaller(size) => format!("size < {size}"),
        Criterion::Not(inner) => format!("NOT ({})", emit(inner, view, params)),
        Criterion::Or(left, right) => format!(
            "({} OR {})",
            emit(left, view, params),
            emit(right, view, params)
        ),
        Criterion::And(items) => {
            let parts = items
                .iter()
                .map(|item| emit(item, view, params))
                .collect::<Vec<_>>();
            format!("({})", parts.join(" AND "))
        }
    }
}

fn substring(column: &str, term: &str, params: &mut Vec<Value>) -> String {
    params.push(Value::from(term.to_lowercase()));
    format!("instr(COALESCE({column}, ''), ?) > 0")
}

fn body_sql(term: &str, params: &mut Vec<Value>) -> String {
    params.push(Value::from(term.to_lowercase()));
    "EXISTS (SELECT 1 FROM body_cache b WHERE b.account = messages.account \
     AND b.message_id = messages.message_id AND instr(COALESCE(b.body_text, ''), ?) > 0)"
        .into()
}

fn day_start(date: NaiveDate) -> i64 {
    date.and_hms_opt(0, 0, 0)
        .map(|value| value.and_utc().timestamp())
        .unwrap_or(0)
}

fn uid_list(uids: &[u32]) -> String {
    if uids.is_empty() {
        return "0=1".into();
    }
    let list = uids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!("uid IN ({list})")
}

/// UID set → range predicates. `n:*` reaches the highest existing UID even
/// when `n` is above it (RFC 3501).
fn uid_set_sql(set: &SequenceSet, max_uid: u32) -> String {
    match set {
        SequenceSet::Single(n) => format!("uid = {n}"),
        SequenceSet::Range(start, end) => {
            format!("uid BETWEEN {} AND {}", start.min(end), start.max(end))
        }
        SequenceSet::RangeFrom(start) => {
            if *start > max_uid {
                format!("uid = {max_uid}")
            } else {
                format!("uid >= {start}")
            }
        }
        SequenceSet::All => "1=1".into(),
        SequenceSet::List(sets) => {
            let parts = sets
                .iter()
                .map(|set| uid_set_sql(set, max_uid))
                .collect::<Vec<_>>();
            format!("({})", parts.join(" OR "))
        }
    }
}

/// Sequence-number set → UID range predicates via the session's ordering.
fn seq_set_sql(set: &SequenceSet, seq_to_uid: &[u32]) -> String {
    let count = seq_to_uid.len() as u32;
    if count == 0 {
        return "0=1".into();
    }
    let uid_at = |sequence: u32| seq_to_uid[(sequence.clamp(1, count) - 1) as usize];
    match set {
        SequenceSet::Single(n) => {
            if *n == 0 || *n > count {
                "0=1".into()
            } else {
                format!("uid = {}", uid_at(*n))
            }
        }
        SequenceSet::Range(start, end) => {
            let (low, high) = (*start.min(end), *start.max(end));
            if low > count {
                // `100:*`-style ranges beyond the end still include the last message.
                return format!("uid = {}", uid_at(count));
            }
            format!("uid BETWEEN {} AND {}", uid_at(low), uid_at(high))
        }
        SequenceSet::RangeFrom(start) => {
            if *start > count {
                format!("uid = {}", uid_at(count))
            } else {
                format!("uid >= {}", uid_at(*start))
            }
        }
        SequenceSet::All => "1=1".into(),
        SequenceSet::List(sets) => {
            let parts = sets
                .iter()
                .map(|set| seq_set_sql(set, seq_to_uid))
                .collect::<Vec<_>>();
            format!("({})", parts.join(" OR "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sql(criteria: &str) -> SearchSql {
        let criterion = parse(criteria).unwrap();
        let view = SessionView {
            deleted_uids: &[4],
            seq_to_uid: &[2, 4, 7, 9],
        };
        to_sql(&criterion, &view)
    }

    #[test]
    fn rejects_unknown_keys_and_literals() {
        assert!(parse("SMELLS bad").is_err());
        assert!(parse("HEADER X-Custom value").is_err());
        assert!(parse("FROM {3}").is_err());
        assert!(parse("(UNSEEN").is_err());
    }

    #[test]
    fn flag_and_set_keys_translate() {
        let out = sql("UNSEEN UNDELETED UID 5:*");
        assert_eq!(
            out.where_clause,
            "(is_read=0 AND NOT (uid IN (4)) AND uid >= 5)"
        );
        assert!(out.params.is_empty());
        assert_eq!(sql("UID 100:*").where_clause, "uid = 9");
        assert_eq!(sql("2:3").where_clause, "uid BETWEEN 4 AND 7");
        assert_eq!(sql("100:*").where_clause, "uid = 9");
        assert_eq!(sql("ALL").where_clause, "1=1");
        assert_eq!(sql("RECENT").where_clause, "0=1");
    }

    #[test]
    fn text_keys_are_case_insensitive_substrings() {
        let out = sql("FROM \"Bob Smith\" SUBJECT hello");
        assert_eq!(
            out.where_clause,
            "(instr(COALESCE(from_lc, ''), ?) > 0 AND instr(COALESCE(subject_lc, ''), ?) > 0)"
        );
        assert_eq!(
            out.params,
            vec![
                Value::from("bob smith".to_owned()),
                Value::from("hello".to_owned())
            ]
        );
        let text = sql("TEXT x");
        assert!(text.where_clause.contains("preview_lc"));
        assert!(text.where_clause.contains("body_cache"));
        assert_eq!(text.params.len(), 7);
    }

    #[test]
    fn dates_are_utc_day_boundaries() {
        assert_eq!(
            sql("SINCE 1-Jan-2020").where_clause,
            "received_at >= 1577836800"
        );
        assert_eq!(
            sql("ON \"02-Jan-2020\"").where_clause,
            "(received_at >= 1577923200 AND received_at < 1578009600)"
        );
        assert_eq!(
            sql("SENTBEFORE 1-Feb-2021").where_clause,
            "sent_at < 1612137600"
        );
        assert!(parse("SINCE 31-Feb-2020").is_err());
    }

    #[test]
    fn boolean_operators_nest() {
        let out = sql("NOT (OR FLAGGED DRAFT) LARGER 1000");
        assert_eq!(
            out.where_clause,
            "(NOT ((flagged=1 OR is_draft=1)) AND size > 1000)"
        );
        let header = sql("HEADER Message-ID <abc@x>");
        assert_eq!(
            header.where_clause,
            "instr(COALESCE(lower(internet_message_id), ''), ?) > 0"
        );
    }

    #[test]
    fn charset_prefix_is_accepted() {
        assert_eq!(sql("CHARSET UTF-8 SEEN").where_clause, "is_read=1");
        assert!(parse("CHARSET KOI8-R SEEN").is_err());
    }
}
