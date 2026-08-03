//! Foreign-realm addressing (foreign-bridge framework,
//! `docs/architecture/foreign-bridge-framework.md` §2): the `<scheme>://<realm>/<path>`
//! URI that names a bridged external space/channel/account.
//!
//! weftd core treats a foreign realm and its path as **opaque** — only the
//! per-scheme adapter daemon and the client fully interpret them. This module
//! therefore validates *structure* and a *wire-safe* charset, but imposes none
//! of WEFT's own network/account grammar on the realm or path: foreign things
//! stay foreign (framework §0, §1 decision 1). `matrix://matrix.org/gaming` is a
//! namespace; `.../gaming/general` a channel; a path-less `matrix://matrix.org`
//! names the realm itself (the `REALM ASSERT` binding, framework §3.1).

use std::fmt;
use std::str::FromStr;

use crate::error::ParseError;

/// A foreign-realm URI may not exceed this many bytes. It travels as a single
/// wire param, and a bound keeps the security-critical L0 parser cheap to fuzz.
pub const MAX_FOREIGN_URI_BYTES: usize = 512;

fn invalid(what: &'static str, value: &str) -> ParseError {
    ParseError::Invalid {
        what,
        value: value.to_string(),
    }
}

/// A protocol-adapter scheme: `matrix`, `discord`, … — `[a-z0-9-]{1,32}`,
/// case-folded. Identifies which adapter (and which per-scheme routing entry)
/// owns a foreign URI.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Scheme(String);

impl Scheme {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for Scheme {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        let folded = s.to_ascii_lowercase();
        let ok = (1..=32).contains(&folded.len())
            && folded
                .bytes()
                .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'));

        if ok {
            Ok(Scheme(folded))
        } else {
            Err(invalid("scheme", s))
        }
    }
}

impl fmt::Display for Scheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The external server/instance a foreign URI lives on: `matrix.org`, a Discord
/// guild snowflake, … — `[a-z0-9.-]{1,253}`, case-folded (DNS realms are
/// case-insensitive; numeric snowflakes are unaffected). It becomes a store key
/// and a NETBLOCK key, so the charset stays deliberately narrow and safe.
fn realm_ok(realm: &str) -> bool {
    (1..=253).contains(&realm.len())
        && realm
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-'))
}

/// A single foreign path segment (a space or channel identifier, in the
/// adapter's own terms). Native case is preserved — a foreign identifier may be
/// case-sensitive — and the charset is any printable ASCII except `/` (the
/// segment separator) and space (the wire param separator). Adapters needing
/// non-ASCII names percent-encode them.
fn segment_ok(segment: &str) -> bool {
    (1..=255).contains(&segment.len()) && segment.bytes().all(|b| matches!(b, 0x21..=0x7e))
}

/// A foreign-realm URI: `<scheme>://<realm>/<path>` (framework §2). `path` is the
/// zero-or-more adapter-defined segments after the realm — empty for a realm
/// binding, one for a namespace, two for a channel.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ForeignUri {
    scheme: Scheme,
    realm: String,
    path: Vec<String>,
}

impl ForeignUri {
    pub fn scheme(&self) -> &Scheme {
        &self.scheme
    }

    pub fn realm(&self) -> &str {
        &self.realm
    }

    /// The path segments after the realm (space, channel, …); empty for a
    /// realm-only URI (the `REALM ASSERT` binding).
    pub fn path(&self) -> &[String] {
        &self.path
    }
}

impl FromStr for ForeignUri {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        if s.len() > MAX_FOREIGN_URI_BYTES {
            return Err(invalid("foreign uri", s));
        }

        let (scheme_str, rest) = s.split_once("://").ok_or_else(|| invalid("foreign uri", s))?;
        let scheme: Scheme = scheme_str.parse()?;

        // The realm is the first `/`-delimited element; the remainder are the
        // path segments. An empty segment (a trailing or doubled `/`) is a parse
        // error — strict-out means we never emit one either.
        let mut parts = rest.split('/');
        let realm = parts.next().unwrap_or("").to_ascii_lowercase();

        if !realm_ok(&realm) {
            return Err(invalid("foreign uri", s));
        }

        let mut path = Vec::new();
        for segment in parts {
            if !segment_ok(segment) {
                return Err(invalid("foreign uri", s));
            }

            path.push(segment.to_string());
        }

        Ok(ForeignUri {
            scheme,
            realm,
            path,
        })
    }
}

impl fmt::Display for ForeignUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://{}", self.scheme, self.realm)?;

        for segment in &self.path {
            write!(f, "/{segment}")?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn realm_binding_uri_round_trips() {
        let uri: ForeignUri = "matrix://matrix.org".parse().unwrap();
        assert_eq!(uri.scheme().as_str(), "matrix");
        assert_eq!(uri.realm(), "matrix.org");
        assert!(uri.path().is_empty());
        assert_eq!(uri.to_string(), "matrix://matrix.org");
    }

    #[test]
    fn namespace_uri_round_trips() {
        let uri: ForeignUri = "matrix://matrix.org/gaming".parse().unwrap();
        assert_eq!(uri.path(), ["gaming"]);
        assert_eq!(uri.to_string(), "matrix://matrix.org/gaming");
    }

    #[test]
    fn channel_uri_round_trips() {
        let uri: ForeignUri = "matrix://matrix.org/gaming/general".parse().unwrap();
        assert_eq!(uri.path(), ["gaming", "general"]);
        assert_eq!(uri.to_string(), "matrix://matrix.org/gaming/general");
    }

    #[test]
    fn scheme_and_realm_case_fold_but_path_keeps_native_case() {
        // DNS realms and adapter schemes are case-insensitive; a foreign path
        // segment may be case-sensitive, so it is preserved verbatim.
        let uri: ForeignUri = "Matrix://Matrix.ORG/Gaming/General".parse().unwrap();
        assert_eq!(uri.to_string(), "matrix://matrix.org/Gaming/General");
    }

    #[test]
    fn discord_snowflake_realm_ok() {
        let uri: ForeignUri = "discord://123456789/general".parse().unwrap();
        assert_eq!(uri.scheme().as_str(), "discord");
        assert_eq!(uri.realm(), "123456789");
        assert_eq!(uri.path(), ["general"]);
    }

    #[test]
    fn rejects_malformed() {
        assert!("matrix.org/gaming".parse::<ForeignUri>().is_err()); // no scheme
        assert!("://matrix.org".parse::<ForeignUri>().is_err()); // empty scheme
        assert!("matrix://".parse::<ForeignUri>().is_err()); // empty realm
        assert!("matrix:///gaming".parse::<ForeignUri>().is_err()); // empty realm before path
        assert!("matrix://matrix.org/".parse::<ForeignUri>().is_err()); // trailing slash
        assert!("matrix://matrix.org//general".parse::<ForeignUri>().is_err()); // doubled slash
        assert!("matrix://mat rix.org".parse::<ForeignUri>().is_err()); // space in realm
        assert!("matrix://matrix.org/a b".parse::<ForeignUri>().is_err()); // space in segment
    }

    #[test]
    fn rejects_oversized() {
        let long = format!("matrix://matrix.org/{}", "x".repeat(MAX_FOREIGN_URI_BYTES));
        assert!(long.parse::<ForeignUri>().is_err());
    }
}
