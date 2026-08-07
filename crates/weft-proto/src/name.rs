//! Machine identifiers (spec §2.3): accounts, network names, channels,
//! and message targets. All are lowercase ASCII on the wire; parsing
//! case-folds leniently, the stored form is always canonical.

use std::fmt;
use std::str::FromStr;

use ulid::Ulid;

use crate::error::ParseError;

fn invalid(what: &'static str, value: &str) -> ParseError {
    ParseError::Invalid {
        what,
        value: value.to_string(),
    }
}

/// Local account name: `[a-z0-9-_.=+]{1,64}` (§2.3).
///
/// `=` and `+` are here for **Matrix parity**: a bridge adapter mints its
/// puppets' handles from foreign identifiers, and an MXID localpart may contain
/// them (`@alice=bob:matrix.org` → `alice=bob@matrix.org`). A lossy mapping
/// would let two distinct foreign users collide onto one WEFT identity, so the
/// charset covers Matrix's grammar except `/`, which is WEFT's own path
/// separator (`#<ns>/<chan>`, `<origin>/<ulid>`, admin REST paths) — adapters
/// escape that one.
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
                .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'=' | b'+'));
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

/// Sovereign network DNS name, e.g. `weft.example` (§2.1).
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

    /// Namespace segment, if the channel lives inside one. Under v0.13 this is
    /// the namespace ULID (as lowercased text); use [`Self::namespace_id`] to
    /// parse it. Kept as a string accessor for scope coverage + SQL extraction.
    pub fn namespace(&self) -> Option<&str> {
        self.0[1..].split_once('/').map(|(ns, _)| ns)
    }

    /// The namespace's ULID id, if this is a namespaced channel (`#<ns-ulid>/…`).
    pub fn namespace_id(&self) -> Option<NamespaceId> {
        self.namespace().and_then(|ns| ns.parse().ok())
    }

    /// This channel's own ULID id — the last segment (`…/<chan-ulid>` or a bare
    /// top-level `#<chan-ulid>`). `None` if the segment isn't a valid ULID (a
    /// legacy vanity-named channel, tolerated until migration).
    pub fn channel_id(&self) -> Option<ChannelId> {
        let body = &self.0[1..];
        let last = body.rsplit_once('/').map(|(_, c)| c).unwrap_or(body);
        last.parse().ok()
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

/// A group-DM identifier: a server-minted ULID. Addressed on the wire as
/// `&<ulid>` (the `&` sigil is added by [`Target`]; `GroupId` itself is the
/// bare ULID). Group DMs are multi-party conversations with an explicit member
/// list and no namespace (social layer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GroupId(Ulid);

impl GroupId {
    pub fn new(ulid: Ulid) -> Self {
        Self(ulid)
    }
    pub fn ulid(&self) -> Ulid {
        self.0
    }
}

impl FromStr for GroupId {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        // Accept a bare ULID or a `&`-prefixed one (lenient-in).
        let body = s.strip_prefix('&').unwrap_or(s);
        Ulid::from_string(body)
            .map(GroupId)
            .map_err(|_| invalid("group id", s))
    }
}

impl fmt::Display for GroupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Canonical group reference: `&<ULID>` (uppercase Crockford base32).
        write!(f, "&{}", self.0)
    }
}

/// Stable identity of a namespace (v0.13): a server-minted ULID, immutable for
/// the namespace's life. The former single-segment name becomes a mutable
/// [`VanityName`]. Rendered as bare uppercase Crockford base32 (no sigil).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NamespaceId(Ulid);

/// Stable identity of a role (v0.13): a server-minted ULID. The role's name is a
/// mutable display label; ROLE commands address the role by this id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RoleId(Ulid);

/// Stable identity of a channel (v0.13): a server-minted ULID. It is the second
/// segment of a namespaced channel's wire name (`#<ns-ulid>/<chan-ulid>`) or the
/// only segment of a top-level channel (`#<chan-ulid>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChannelId(Ulid);

/// A mutable, human-facing label (§2.3): namespace or channel "vanity" name, one
/// segment `[a-z0-9-_]{1,64}`, per-network (namespaces) or per-namespace
/// (channels) unique. Resolves to a stable ULID id at the wire boundary; the raw
/// WEFT wire carries the ULID, the IRC gateway + clients address by this.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VanityName(String);

/// A namespace reference a client may express as **either** the immutable
/// [`NamespaceId`] **or** a mutable [`VanityName`] (§2.2, v0.13). Used by
/// `NS JOIN`, where a user may type the human name of an *unlisted* namespace
/// (which DISCOVER never surfaced, so the client holds no id for it). The server
/// resolves it to the id at the wire boundary — a stored id first, else the
/// vanity. A lowercase ULID satisfies the vanity charset, so one lenient parse
/// covers both forms; the value is always case-folded to lowercase.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NamespaceRef(String);

impl NamespaceRef {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for NamespaceRef {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        // The vanity charset (lowercase `[a-z0-9-_]{1,64}`) is a superset of a
        // lowercased ULID id, so this accepts both an id and a vanity.
        s.parse::<VanityName>()
            .map(|v| NamespaceRef(v.as_str().to_string()))
            .map_err(|_| invalid("namespace ref", s))
    }
}

impl fmt::Display for NamespaceRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

macro_rules! ulid_id {
    ($ty:ident, $what:literal) => {
        impl $ty {
            pub fn new(ulid: Ulid) -> Self {
                Self(ulid)
            }
            pub fn ulid(&self) -> Ulid {
                self.0
            }
        }
        impl FromStr for $ty {
            type Err = ParseError;
            fn from_str(s: &str) -> Result<Self, ParseError> {
                // Crockford base32 is case-insensitive; the wire form may arrive
                // lowercased (channel segments case-fold), so normalize up first.
                Ulid::from_string(&s.to_ascii_uppercase())
                    .map(Self)
                    .map_err(|_| invalid($what, s))
            }
        }
        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                // v0.13 ids render **lowercase** — they appear inside channel
                // names (`#<ns-id>/<chan-id>`), which case-fold to lowercase, so
                // the id must match that form everywhere (scopes, store keys).
                write!(f, "{}", self.0.to_string().to_ascii_lowercase())
            }
        }
    };
}

ulid_id!(NamespaceId, "namespace id");
ulid_id!(RoleId, "role id");
ulid_id!(ChannelId, "channel id");

impl VanityName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for VanityName {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        let folded = s.to_ascii_lowercase();
        let ok = (1..=64).contains(&folded.len())
            && folded
                .bytes()
                .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_'));
        if ok {
            Ok(VanityName(folded))
        } else {
            Err(invalid("vanity name", s))
        }
    }
}

impl fmt::Display for VanityName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A MSG/MESSAGE destination: `#channel`, `@user` (same-network DM, §9.5), or
/// `&<group>` (a group DM, social layer).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Target {
    Channel(ChannelName),
    /// A DM peer. `network: None` is the same-network form (`@ada`, §9.5) — the
    /// session layer resolves it against its own network. `Some(net)` is a
    /// **cross-network** DM (`@alice@matrix.org`), which also addresses a bridged
    /// (provider-managed) user; the wire form is additive, so `@ada` parses and
    /// serializes exactly as before.
    User {
        account: Account,
        network: Option<NetworkName>,
    },
    Group(GroupId),
}

impl FromStr for Target {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        if s.starts_with('#') {
            Ok(Target::Channel(s.parse()?))
        } else if let Some(user) = s.strip_prefix('@') {
            // After the sigil, a remaining `@` splits account from network:
            // `@ada` = same-network, `@alice@matrix.org` = cross-network.
            match user.split_once('@') {
                Some((account, network)) => Ok(Target::User {
                    account: account.parse()?,
                    network: Some(network.parse()?),
                }),
                None => Ok(Target::User {
                    account: user.parse()?,
                    network: None,
                }),
            }
        } else if s.starts_with('&') {
            Ok(Target::Group(s.parse()?))
        } else {
            Err(invalid("target", s))
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Channel(channel) => channel.fmt(f),
            Target::User {
                account,
                network: Some(network),
            } => write!(f, "@{account}@{network}"),
            Target::User {
                account,
                network: None,
            } => write!(f, "@{account}"),
            Target::Group(group) => group.fmt(f), // GroupId already renders `&<ulid>`
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
    fn account_covers_matrix_localparts() {
        // A bridge puppet keeps the foreign localpart verbatim, so distinct
        // MXIDs stay distinct accounts (no lossy collision).
        assert_eq!(
            "alice=bob".parse::<Account>().unwrap().as_str(),
            "alice=bob"
        );
        assert_eq!(
            "a+b.c_d-e".parse::<Account>().unwrap().as_str(),
            "a+b.c_d-e"
        );
        assert_ne!(
            "alice=bob".parse::<Account>().unwrap(),
            "alicebob".parse::<Account>().unwrap()
        );

        // `/` stays out — it is WEFT's own path separator.
        assert!("alice/bob".parse::<Account>().is_err());
    }

    #[test]
    fn network_name_validation() {
        assert!("weft.example".parse::<NetworkName>().is_ok());
        assert!("localhost".parse::<NetworkName>().is_ok());
        assert!("".parse::<NetworkName>().is_err());
        assert!("-bad.example".parse::<NetworkName>().is_err());
        assert!("double..dot".parse::<NetworkName>().is_err());
    }

    #[test]
    fn dm_targets_round_trip_both_forms() {
        // §9.5 same-network form — byte-identical to before the network field.
        let local: Target = "@ada".parse().unwrap();
        assert_eq!(
            local,
            Target::User {
                account: "ada".parse().unwrap(),
                network: None
            }
        );
        assert_eq!(local.to_string(), "@ada");

        // Cross-network / bridged form: the remainder after the sigil splits.
        let foreign: Target = "@alice@matrix.org".parse().unwrap();
        assert_eq!(
            foreign,
            Target::User {
                account: "alice".parse().unwrap(),
                network: Some("matrix.org".parse().unwrap())
            }
        );
        assert_eq!(foreign.to_string(), "@alice@matrix.org");

        assert!("@ada@".parse::<Target>().is_err()); // empty network
        assert!("@@matrix.org".parse::<Target>().is_err()); // empty account
    }

    #[test]
    fn user_ref_round_trips() {
        let user: UserRef = "jannik@weft.example".parse().unwrap();
        assert_eq!(user.to_string(), "jannik@weft.example");
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
    fn ulid_ids_round_trip() {
        let u = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
        // Renders lowercase (v0.13, to match channel-name folding); parse is
        // case-insensitive so either case round-trips to the same id.
        let ns = NamespaceId::new(u);
        assert_eq!(ns.to_string(), "01arz3ndektsv4rrffq69g5fav");
        assert_eq!(ns.to_string().parse::<NamespaceId>().unwrap(), ns);
        assert_eq!(
            "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse::<NamespaceId>().unwrap(),
            ns
        );
        assert_eq!(u, RoleId::new(u).ulid());
        assert!("not-a-ulid".parse::<RoleId>().is_err());
    }

    #[test]
    fn vanity_names() {
        assert_eq!(
            "My-Server_1".parse::<VanityName>().unwrap().as_str(),
            "my-server_1"
        );
        assert!("".parse::<VanityName>().is_err());
        assert!("has space".parse::<VanityName>().is_err());
        assert!("x".repeat(65).parse::<VanityName>().is_err());
    }

    #[test]
    fn channel_ulid_segments() {
        // v0.13 wire form: `#<ns-ulid>/<chan-ulid>` (case-folded to lowercase).
        let ns = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
        let ch = Ulid::from_string("01BX5ZZKBKACTAV9WEVGEMMVRZ").unwrap();
        let wire = format!("#{ns}/{ch}");
        let name: ChannelName = wire.parse().unwrap();
        assert_eq!(name.namespace_id(), Some(NamespaceId::new(ns)));
        assert_eq!(name.channel_id(), Some(ChannelId::new(ch)));

        // Top-level channel: only segment is the channel ULID, no namespace.
        let top: ChannelName = format!("#{ch}").parse().unwrap();
        assert_eq!(top.namespace_id(), None);
        assert_eq!(top.channel_id(), Some(ChannelId::new(ch)));

        // A legacy vanity-named channel still parses as a ChannelName but has no
        // ULID id (tolerated until migration rewrites it).
        let legacy: ChannelName = "#gaming/general".parse().unwrap();
        assert_eq!(legacy.channel_id(), None);
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
