//! Control-plane line grammar (spec §4):
//!
//! ```text
//! @tag1=value;tag2;tag3=value VERB param1 param2 :trailing free text
//! ```
//!
//! Lenient-in, strict-out: [`Line::parse`] tolerates noisy-but-safe input
//! (mixed case, repeated spaces, stray semicolons, any line terminator);
//! [`Line::serialize`] refuses to emit anything the parser would reject.

use std::collections::BTreeMap;

use crate::error::{ParseError, SerializeError};

/// §4: maximum line length in bytes, terminator included.
pub const MAX_LINE_BYTES: usize = 8192;
/// §4: maximum number of tags.
pub const MAX_TAGS: usize = 32;
/// §4: maximum tag key length in bytes.
pub const MAX_TAG_KEY_BYTES: usize = 64;
/// §4: maximum *unescaped* tag value length in bytes.
pub const MAX_TAG_VALUE_BYTES: usize = 1024;
/// §4: maximum number of middle params.
pub const MAX_PARAMS: usize = 15;
/// §3.5: labels are opaque and capped tighter than ordinary tag values.
pub const MAX_LABEL_BYTES: usize = 64;
/// §6.4: at most 10 `attach.N` tags per MSG/MESSAGE.
pub const MAX_ATTACHMENTS: usize = 10;
/// §6.4: reaction emoji (or `:shortcode:`) length cap.
pub const MAX_EMOJI_BYTES: usize = 32;
/// §6.4: HISTORY page size cap; servers clamp, they don't error.
pub const MAX_HISTORY_LIMIT: u32 = 500;

/// Tag map. `BTreeMap` so serialization is deterministic (CLAUDE.md:
/// deterministic output wherever a signature might apply). A flag tag
/// (`@compacted`) is stored with an empty value.
pub type Tags = BTreeMap<String, String>;

/// One parsed control-plane line. This is the raw grammar layer; typed
/// decoding lives in [`crate::command`] / [`crate::event`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Line {
    pub tags: Tags,
    /// Uppercase `[A-Z0-9-]+`, normalized on parse.
    pub verb: String,
    /// Middle params: never empty, no spaces, no leading `:`.
    pub params: Vec<String>,
    /// `Some("")` (empty trailing) and `None` (no trailing) are distinct
    /// and both meaningful (§4).
    pub trailing: Option<String>,
}

fn tag_key_ok(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_TAG_KEY_BYTES
        && key
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'/' | b'-'))
}

fn verb_ok(verb: &str) -> bool {
    !verb.is_empty()
        && verb
            .bytes()
            .all(|b| matches!(b, b'A'..=b'Z' | b'0'..=b'9' | b'-'))
}

/// Escape a raw tag value for the wire (§4 escape table).
pub fn escape_tag_value(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            ';' => out.push_str("\\:"),
            ' ' => out.push_str("\\s"),
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out
}

/// Unescape a wire tag value. Unknown escapes drop the backslash; a
/// dangling trailing backslash is an error (§4).
pub fn unescape_tag_value(raw: &str) -> Result<String, ParseError> {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            None => return Err(ParseError::DanglingEscape),
            Some(':') => out.push(';'),
            Some('s') => out.push(' '),
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some('\\') => out.push('\\'),
            Some(other) => out.push(other),
        }
    }
    Ok(out)
}

impl Line {
    /// Parse one line (with or without its terminator). Lenient-in.
    pub fn parse(input: &str) -> Result<Line, ParseError> {
        let s = input
            .strip_suffix("\r\n")
            .or_else(|| input.strip_suffix('\n'))
            .or_else(|| input.strip_suffix('\r'))
            .unwrap_or(input);
        // Limit applies to the line body: `serialize` may emit exactly
        // MAX_LINE_BYTES, and the transport's terminator must not push the
        // round trip over the edge.
        if s.len() > MAX_LINE_BYTES {
            return Err(ParseError::LineTooLong { len: s.len() });
        }
        if s.bytes().any(|b| b == b'\r' || b == b'\n') {
            return Err(ParseError::EmbeddedLineBreak);
        }

        let mut tags = Tags::new();
        let mut rest = s;
        if let Some(r) = rest.strip_prefix('@') {
            // Tags without a following verb are not a line.
            let (tag_str, r) = r.split_once(' ').ok_or(ParseError::EmptyLine)?;
            parse_tags(tag_str, &mut tags)?;
            rest = r;
        }

        let mut verb: Option<String> = None;
        let mut params = Vec::new();
        let mut trailing = None;
        let mut cur = rest;
        loop {
            cur = cur.trim_start_matches(' '); // lenient: collapse space runs between tokens
            if cur.is_empty() {
                break;
            }
            // Only params (not the verb) may introduce a trailing.
            if verb.is_some() {
                if let Some(t) = cur.strip_prefix(':') {
                    trailing = Some(t.to_string());
                    break;
                }
            }
            let (tok, r) = cur.split_once(' ').unwrap_or((cur, ""));
            match &verb {
                None => {
                    let v = tok.to_ascii_uppercase(); // lenient: fold verb case
                    if !verb_ok(&v) {
                        return Err(ParseError::BadVerb {
                            verb: tok.to_string(),
                        });
                    }
                    verb = Some(v);
                }
                Some(_) => params.push(tok.to_string()),
            }
            cur = r;
        }
        if params.len() > MAX_PARAMS {
            return Err(ParseError::TooManyParams);
        }
        Ok(Line {
            tags,
            verb: verb.ok_or(ParseError::EmptyLine)?,
            params,
            trailing,
        })
    }

    /// Serialize without a line terminator (the transport frames lines).
    /// Strict-out: every constraint the parser enforces is checked here.
    pub fn serialize(&self) -> Result<String, SerializeError> {
        if !verb_ok(&self.verb) {
            return Err(SerializeError::BadVerb {
                verb: self.verb.clone(),
            });
        }
        if self.tags.len() > MAX_TAGS {
            return Err(SerializeError::TooManyTags);
        }
        if self.params.len() > MAX_PARAMS {
            return Err(SerializeError::TooManyParams);
        }

        let mut out = String::new();
        if !self.tags.is_empty() {
            out.push('@');
            for (i, (key, value)) in self.tags.iter().enumerate() {
                if !tag_key_ok(key) {
                    return Err(SerializeError::BadTagKey { key: key.clone() });
                }
                if value.len() > MAX_TAG_VALUE_BYTES {
                    return Err(SerializeError::TagValueTooLong { key: key.clone() });
                }
                if i > 0 {
                    out.push(';');
                }
                out.push_str(key);
                if !value.is_empty() {
                    out.push('=');
                    out.push_str(&escape_tag_value(value));
                }
            }
            out.push(' ');
        }
        out.push_str(&self.verb);

        for param in &self.params {
            let bad = |reason| SerializeError::BadParam {
                param: param.clone(),
                reason,
            };
            if param.is_empty() {
                return Err(bad("empty"));
            }
            if param.contains(' ') {
                return Err(bad("contains space"));
            }
            if param.starts_with(':') {
                return Err(bad("leading colon"));
            }
            if param.bytes().any(|b| b == b'\r' || b == b'\n') {
                return Err(bad("contains CR/LF"));
            }
            out.push(' ');
            out.push_str(param);
        }

        if let Some(trailing) = &self.trailing {
            if trailing.bytes().any(|b| b == b'\r' || b == b'\n') {
                return Err(SerializeError::BadTrailing);
            }
            out.push_str(" :");
            out.push_str(trailing);
        }

        if out.len() > MAX_LINE_BYTES {
            return Err(SerializeError::LineTooLong);
        }
        debug_assert!(Line::parse(&out).is_ok(), "strict-out violated: {out:?}");
        Ok(out)
    }
}

fn parse_tags(tag_str: &str, tags: &mut Tags) -> Result<(), ParseError> {
    for item in tag_str.split(';') {
        if item.is_empty() {
            continue; // lenient: stray semicolons
        }
        let (key, raw_value) = item.split_once('=').unwrap_or((item, ""));
        let key = key.to_ascii_lowercase(); // lenient: fold key case
        if !tag_key_ok(&key) {
            return Err(ParseError::BadTagKey { key });
        }
        let value = unescape_tag_value(raw_value)?;
        if value.len() > MAX_TAG_VALUE_BYTES {
            return Err(ParseError::TagValueTooLong { key });
        }
        tags.insert(key, value); // lenient: duplicate key — last wins
    }
    if tags.len() > MAX_TAGS {
        return Err(ParseError::TooManyTags);
    }
    Ok(())
}

// ---- shared helpers for typed decoders (command.rs / event.rs) ----

/// §3.5: read the `label` tag. Empty label is treated as absent.
pub(crate) fn label_from_tags(tags: &Tags) -> Result<Option<String>, ParseError> {
    match tags.get("label") {
        None => Ok(None),
        Some(v) if v.is_empty() => Ok(None),
        Some(v) if v.len() > MAX_LABEL_BYTES => Err(ParseError::LabelTooLong),
        Some(v) => Ok(Some(v.clone())),
    }
}

/// §3.5: write the `label` tag.
pub(crate) fn write_label(tags: &mut Tags, label: Option<&str>) -> Result<(), SerializeError> {
    if let Some(label) = label {
        if label.is_empty() {
            return Err(SerializeError::Unrepresentable("empty label"));
        }
        if label.len() > MAX_LABEL_BYTES {
            return Err(SerializeError::LabelTooLong);
        }
        tags.insert("label".to_string(), label.to_string());
    }
    Ok(())
}

/// Cursor over a line's middle params for verb decoders.
pub(crate) struct Args<'a> {
    line: &'a Line,
    verb: &'static str,
    next: usize,
}

impl<'a> Args<'a> {
    pub(crate) fn new(line: &'a Line, verb: &'static str) -> Self {
        Self {
            line,
            verb,
            next: 0,
        }
    }

    pub(crate) fn opt(&mut self) -> Option<&'a str> {
        let param = self.line.params.get(self.next)?;
        self.next += 1;
        Some(param.as_str())
    }

    pub(crate) fn req(&mut self, what: &'static str) -> Result<&'a str, ParseError> {
        self.opt().ok_or(ParseError::MissingParam {
            verb: self.verb,
            what,
        })
    }

    pub(crate) fn trailing_opt(&self) -> Option<String> {
        self.line.trailing.clone()
    }

    pub(crate) fn trailing_req(&self, what: &'static str) -> Result<&'a str, ParseError> {
        self.line
            .trailing
            .as_deref()
            .ok_or(ParseError::MissingParam {
                verb: self.verb,
                what,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal() {
        let line = Line::parse("PING").unwrap();
        assert_eq!(line.verb, "PING");
        assert!(line.tags.is_empty() && line.params.is_empty() && line.trailing.is_none());
    }

    #[test]
    fn parse_full_line() {
        let line = Line::parse("@a=1;flag MSG #general :hello  world").unwrap();
        assert_eq!(line.verb, "MSG");
        assert_eq!(line.tags.get("a").map(String::as_str), Some("1"));
        assert_eq!(line.tags.get("flag").map(String::as_str), Some(""));
        assert_eq!(line.params, vec!["#general"]);
        // Inner spaces of the trailing are preserved verbatim.
        assert_eq!(line.trailing.as_deref(), Some("hello  world"));
    }

    #[test]
    fn tag_escaping_round_trips() {
        let raw = "a;b c\\d\r\ne";
        let escaped = escape_tag_value(raw);
        assert_eq!(escaped, "a\\:b\\sc\\\\d\\r\\ne");
        assert_eq!(unescape_tag_value(&escaped).unwrap(), raw);
    }

    #[test]
    fn unknown_escape_drops_backslash() {
        assert_eq!(unescape_tag_value("a\\xb").unwrap(), "axb");
    }

    #[test]
    fn dangling_escape_is_error() {
        assert_eq!(unescape_tag_value("abc\\"), Err(ParseError::DanglingEscape));
    }

    #[test]
    fn empty_trailing_distinct_from_absent() {
        assert_eq!(
            Line::parse("MSG #a :").unwrap().trailing.as_deref(),
            Some("")
        );
        assert_eq!(Line::parse("MSG #a").unwrap().trailing, None);
    }

    #[test]
    fn lenient_case_spaces_and_terminators() {
        let line = Line::parse("  msg   #a  :body\r\n").unwrap();
        assert_eq!(line.verb, "MSG");
        assert_eq!(line.params, vec!["#a"]);
        assert_eq!(line.trailing.as_deref(), Some("body"));
    }

    #[test]
    fn limits_enforced_on_parse() {
        let long = "PING ".to_string() + &"x".repeat(MAX_LINE_BYTES);
        assert!(matches!(
            Line::parse(&long),
            Err(ParseError::LineTooLong { .. })
        ));

        let tags: Vec<String> = (0..MAX_TAGS + 1).map(|i| format!("k{i}")).collect();
        let line = format!("@{} PING", tags.join(";"));
        assert_eq!(Line::parse(&line), Err(ParseError::TooManyTags));

        let line = format!("PING {}", vec!["p"; MAX_PARAMS + 1].join(" "));
        assert_eq!(Line::parse(&line), Err(ParseError::TooManyParams));

        let line = format!("@k={} PING", "v".repeat(MAX_TAG_VALUE_BYTES + 1));
        assert!(matches!(
            Line::parse(&line),
            Err(ParseError::TagValueTooLong { .. })
        ));

        assert!(matches!(
            Line::parse("@K!=v PING"),
            Err(ParseError::BadTagKey { .. })
        ));
        assert!(matches!(
            Line::parse("P!NG"),
            Err(ParseError::BadVerb { .. })
        ));
        assert_eq!(Line::parse(""), Err(ParseError::EmptyLine));
        assert_eq!(Line::parse("PI\rNG x"), Err(ParseError::EmbeddedLineBreak));
    }

    #[test]
    fn strict_out_refuses_what_parse_rejects() {
        let ok = Line {
            verb: "PING".into(),
            ..Default::default()
        };

        let mut line = ok.clone();
        line.params = vec!["has space".into()];
        assert!(matches!(
            line.serialize(),
            Err(SerializeError::BadParam { .. })
        ));

        let mut line = ok.clone();
        line.params = vec![":leading".into()];
        assert!(matches!(
            line.serialize(),
            Err(SerializeError::BadParam { .. })
        ));

        let mut line = ok.clone();
        line.verb = "ping".into(); // parser normalizes; serializer refuses non-canonical
        assert!(matches!(
            line.serialize(),
            Err(SerializeError::BadVerb { .. })
        ));

        let mut line = ok.clone();
        line.tags.insert("BAD".into(), String::new());
        assert!(matches!(
            line.serialize(),
            Err(SerializeError::BadTagKey { .. })
        ));

        let mut line = ok.clone();
        line.trailing = Some("a\nb".into());
        assert_eq!(line.serialize(), Err(SerializeError::BadTrailing));

        let mut line = ok;
        line.trailing = Some("x".repeat(MAX_LINE_BYTES));
        assert_eq!(line.serialize(), Err(SerializeError::LineTooLong));
    }

    #[test]
    fn max_length_line_survives_terminated_round_trip() {
        // The transport appends a terminator; that must never make our own
        // maximal output unparseable (strict-out).
        let line = Line {
            verb: "MSG".into(),
            params: vec!["#a".into()],
            trailing: Some("x".repeat(MAX_LINE_BYTES - "MSG #a :".len())),
            ..Default::default()
        };
        let out = line.serialize().unwrap();
        assert_eq!(out.len(), MAX_LINE_BYTES);
        assert_eq!(Line::parse(&format!("{out}\r\n")).unwrap(), line);
    }

    #[test]
    fn line_round_trip_is_canonical() {
        let input = "@b=2;a=1;flag MSG #general extra :hi there";
        let line = Line::parse(input).unwrap();
        let out = line.serialize().unwrap();
        // Canonical form sorts tags (BTreeMap) — reparse must be identical.
        assert_eq!(out, "@a=1;b=2;flag MSG #general extra :hi there");
        assert_eq!(Line::parse(&out).unwrap(), line);
    }
}
