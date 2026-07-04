//! Machine identifiers (spec §2.3): accounts, network names, channels,
//! and message targets. All are lowercase ASCII on the wire; parsing
//! case-folds leniently, the stored form is always canonical.

use std::fmt;
use std::str::FromStr;

use crate::error::ParseError;

fn invalid(what: &'static str, value: &str) -> ParseError {
    ParseError::Invalid {
        what,
        value: value.to_string(),
    }
}

/// Local account name: `[a-z0-9-_.]{1,64}` (§2.3).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Account(String);

impl Account {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for Account {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        let folded = s.to_ascii_lowercase();
        let ok = (1..=64).contains(&folded.len())
            && folded
                .bytes()
                .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.'));
        if ok {
            Ok(Account(folded))
        } else {
            Err(invalid("account", s))
        }
    }
}

impl fmt::Display for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Sovereign network DNS name, e.g. `hda.example` (§2.1).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NetworkName(String);

impl NetworkName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for NetworkName {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        fn label_ok(label: &str) -> bool {
            (1..=63).contains(&label.len())
                && label
                    .bytes()
                    .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'))
                && !label.starts_with('-')
                && !label.ends_with('-')
        }
        let folded = s.to_ascii_lowercase();
        if !folded.is_empty() && folded.len() <= 253 && folded.split('.').all(label_ok) {
            Ok(NetworkName(folded))
        } else {
            Err(invalid("network name", s))
        }
    }
}

impl fmt::Display for NetworkName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Fully qualified user: `user@network` (§2.1).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UserRef {
    pub account: Account,
    pub network: NetworkName,
}

impl UserRef {
    pub fn new(account: Account, network: NetworkName) -> Self {
        Self { account, network }
    }
}

impl FromStr for UserRef {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        let (account, network) = s
            .split_once('@')
            .ok_or_else(|| invalid("user reference", s))?;
        Ok(UserRef {
            account: account.parse()?,
            network: network.parse()?,
        })
    }
}

impl fmt::Display for UserRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.account, self.network)
    }
}

/// Namespace name: one segment `[a-z0-9-_]+` (§2.3), no `#`, no `/`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NamespaceName(String);

impl NamespaceName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for NamespaceName {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        let folded = s.to_ascii_lowercase();
        let ok = !folded.is_empty()
            && folded.len() <= 64
            && folded
                .bytes()
                .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_'));
        if ok {
            Ok(NamespaceName(folded))
        } else {
            Err(invalid("namespace", s))
        }
    }
}

impl fmt::Display for NamespaceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Channel name with leading `#`: `#general` or `#ns/general` — one
/// namespace level, no nesting; ≤200 bytes total; segments `[a-z0-9-_]+`
/// (§2.1, §2.3).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChannelName(String);

impl ChannelName {
    /// Full wire form including `#` (and namespace if any).
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Namespace segment, if the channel lives inside one.
    pub fn namespace(&self) -> Option<&str> {
        self.0[1..].split_once('/').map(|(ns, _)| ns)
    }
}

impl FromStr for ChannelName {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        fn segment_ok(seg: &str) -> bool {
            !seg.is_empty()
                && seg
                    .bytes()
                    .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_'))
        }
        let folded = s.to_ascii_lowercase();
        let ok = folded.len() <= 200
            && folded.strip_prefix('#').is_some_and(|body| {
                let segments: Vec<&str> = body.split('/').collect();
                (1..=2).contains(&segments.len()) && segments.iter().copied().all(segment_ok)
            });
        if ok {
            Ok(ChannelName(folded))
        } else {
            Err(invalid("channel", s))
        }
    }
}

impl fmt::Display for ChannelName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A MSG/MESSAGE destination: `#channel` or `@user` (same-network DM, §9.5).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Target {
    Channel(ChannelName),
    User(Account),
}

impl FromStr for Target {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        if s.starts_with('#') {
            Ok(Target::Channel(s.parse()?))
        } else if let Some(user) = s.strip_prefix('@') {
            Ok(Target::User(user.parse()?))
        } else {
            Err(invalid("target", s))
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Channel(channel) => channel.fmt(f),
            Target::User(account) => write!(f, "@{account}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_validation_and_folding() {
        assert_eq!("Ada_99.x".parse::<Account>().unwrap().as_str(), "ada_99.x");
        assert!("".parse::<Account>().is_err());
        assert!("has space".parse::<Account>().is_err());
        assert!("ümläut".parse::<Account>().is_err());
        assert!("x".repeat(65).parse::<Account>().is_err());
    }

    #[test]
    fn network_name_validation() {
        assert!("hda.example".parse::<NetworkName>().is_ok());
        assert!("localhost".parse::<NetworkName>().is_ok());
        assert!("".parse::<NetworkName>().is_err());
        assert!("-bad.example".parse::<NetworkName>().is_err());
        assert!("double..dot".parse::<NetworkName>().is_err());
    }

    #[test]
    fn user_ref_round_trips() {
        let user: UserRef = "jannik@hda.example".parse().unwrap();
        assert_eq!(user.to_string(), "jannik@hda.example");
        assert!("no-at-sign".parse::<UserRef>().is_err());
    }

    #[test]
    fn channel_names() {
        assert_eq!(
            "#General".parse::<ChannelName>().unwrap().as_str(),
            "#general"
        );
        let ns: ChannelName = "#gaming/general".parse().unwrap();
        assert_eq!(ns.namespace(), Some("gaming"));
        assert!("general".parse::<ChannelName>().is_err()); // missing '#'
        assert!("#a/b/c".parse::<ChannelName>().is_err()); // no nesting
        assert!("#".parse::<ChannelName>().is_err());
        assert!(format!("#{}", "x".repeat(200))
            .parse::<ChannelName>()
            .is_err());
    }

    #[test]
    fn targets() {
        assert!(matches!(
            "#general".parse::<Target>(),
            Ok(Target::Channel(_))
        ));
        let dm: Target = "@ada".parse().unwrap();
        assert_eq!(dm.to_string(), "@ada");
        assert!("plain".parse::<Target>().is_err());
    }
}
