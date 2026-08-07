//! Identity and id mapping: Matrix ↔ WEFT.
//!
//! Two rules govern everything here:
//!
//! - **Injective, or not at all.** A lossy mapping merges two people into one
//!   identity, which is an impersonation bug, not a display blemish. Anything
//!   that cannot be mapped injectively is refused (`None`), never approximated.
//! - **Deterministic ids.** Structure ids (spaces, rooms) derive from the
//!   Matrix id, so re-provisioning after a restart — or on another weftd —
//!   reproduces the same namespace instead of orphaning every stored
//!   reference. Message ids carry the event's real timestamp, because the
//!   multi-origin replica is ordered by ULID time.

use sha2::{Digest, Sha256};

/// Bytes a WEFT local account may contain (§2.3) minus `=`, which is the
/// escape character.
fn plain(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'+')
}

/// Escape a Matrix localpart into the WEFT account charset.
///
/// `=xx` (lowercase hex) per escaped byte — the reason `=` is in the account
/// grammar at all. Injective because `=` itself is always escaped.
pub fn escape_localpart(localpart: &str) -> String {
    let mut out = String::with_capacity(localpart.len());

    for b in localpart.bytes() {
        if plain(b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("={b:02x}"));
        }
    }

    out
}

/// The inverse of [`escape_localpart`]: `=xx` → the byte. Anything malformed
/// (`=` without two hex digits) is a mapping we never produced — `None`.
pub fn unescape_localpart(escaped: &str) -> Option<String> {
    let bytes = escaped.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'=' {
            let hex = bytes.get(i + 1..i + 3)?;
            let hex = std::str::from_utf8(hex).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }

    String::from_utf8(out).ok()
}

/// A **foreign** WEFT handle (`carol=40x@kde.org`) back to its MXID — the
/// inverse of [`weft_user`], for addressing them on the Matrix side (power
/// levels, bans). `None` for a handle we could not have produced.
pub fn mxid_of_weft_user(user: &str) -> Option<String> {
    let (account, network) = user.split_once('@')?;
    let localpart = unescape_localpart(account)?;

    Some(format!("@{localpart}:{network}"))
}

/// `@carol:kde.org` → `carol@kde.org` (WEFT `user@network` form).
///
/// `None` when the identity cannot be represented injectively: a server name
/// with a port or IP literal (not a valid WEFT network name), or a localpart
/// that escapes past the 64-char account limit — truncating would collide.
pub fn weft_user(mxid: &ruma::UserId) -> Option<String> {
    let server = mxid.server_name();
    if server.port().is_some() || server.is_ip_literal() {
        return None;
    }

    let account = escape_localpart(mxid.localpart());
    if account.is_empty() || account.len() > 64 {
        return None;
    }

    Some(format!("{account}@{}", server.host()))
}

/// The Matrix puppet of one of our users: `@<prefix><account-ulid>:<domain>`.
///
/// Keyed by the account **ULID** (owner directive 2026-08-06) — the stable
/// identity — never by the account name, which is a mutable vanity label: a
/// name-keyed puppet would be orphaned by a rename. A ULID is lowercase
/// Crockford base32, safely inside Matrix's localpart grammar, and trivially
/// injective under a reserved prefix.
pub fn puppet_localpart(prefix: &str, ulid: &str) -> String {
    format!("{prefix}{ulid}")
}

pub fn puppet_mxid(prefix: &str, ulid: &str, domain: &str) -> String {
    format!("@{}:{domain}", puppet_localpart(prefix, ulid))
}

/// Is this Matrix user one of ours — the bridge bot or a puppet? Their events
/// must never be re-ingested: they *are* the relay of a WEFT event.
pub fn is_our_mxid(mxid: &ruma::UserId, prefix: &str, domain: &str, bot_localpart: &str) -> bool {
    mxid.server_name().host() == domain
        && (mxid.localpart() == bot_localpart || mxid.localpart().starts_with(prefix))
}

/// This bridge's MXID namespace on the companion homeserver.
///
/// The three fields are not independent settings that happen to sit together:
/// they define one namespace, and every id in it is derived from all three —
/// puppets are `@<prefix><ulid>:<domain>`, the bot is `@<bot>:<domain>`, and
/// "is this ours?" is the same question read backwards. Holding them apart meant
/// each caller re-derived the answer, and the bot MXID in particular was
/// hand-formatted in four places.
///
/// The free functions above stay: this binds them, it does not replace them.
#[derive(Debug, Clone)]
pub struct MatrixIdentity {
    domain: String,
    puppet_prefix: String,
    bot_localpart: String,
}

impl MatrixIdentity {
    pub fn new(
        domain: impl Into<String>,
        puppet_prefix: impl Into<String>,
        bot_localpart: impl Into<String>,
    ) -> Self {
        Self {
            domain: domain.into(),
            puppet_prefix: puppet_prefix.into(),
            bot_localpart: bot_localpart.into(),
        }
    }

    /// The companion homeserver's server name — also what we advertise as a
    /// `via` server for rooms we create.
    pub fn domain(&self) -> &str {
        &self.domain
    }

    pub fn puppet_prefix(&self) -> &str {
        &self.puppet_prefix
    }

    pub fn bot_localpart(&self) -> &str {
        &self.bot_localpart
    }

    /// The appservice bot's MXID.
    pub fn bot_mxid(&self) -> String {
        self.mxid(&self.bot_localpart)
    }

    /// A localpart on our homeserver as a full MXID.
    pub fn mxid(&self, localpart: &str) -> String {
        format!("@{localpart}:{}", self.domain)
    }

    /// The puppet localpart / MXID for one of our accounts, keyed by ULID.
    pub fn puppet_localpart(&self, ulid: &str) -> String {
        puppet_localpart(&self.puppet_prefix, ulid)
    }

    pub fn puppet_mxid(&self, ulid: &str) -> String {
        puppet_mxid(&self.puppet_prefix, ulid, &self.domain)
    }

    /// Is this Matrix user one of ours (bot or puppet)? Their events are the
    /// relay of a WEFT event and must never be re-ingested.
    pub fn is_ours(&self, mxid: &ruma::UserId) -> bool {
        is_our_mxid(mxid, &self.puppet_prefix, &self.domain, &self.bot_localpart)
    }

    /// The account ULID this MXID is the puppet of, or `None` if it is not one of
    /// our puppets (a foreign user, or our own bot).
    pub fn puppet_ulid<'a>(&self, mxid: &'a ruma::UserId) -> Option<&'a str> {
        if mxid.server_name().host() != self.domain {
            return None;
        }

        mxid.localpart().strip_prefix(self.puppet_prefix.as_str())
    }
}

/// A parsed `matrix://<realm>/<space>[/<room>]` foreign URI.
#[derive(Debug, Clone, PartialEq)]
pub struct SpaceRef {
    pub realm: String,
    pub space: String,
    pub room: Option<String>,
}

impl SpaceRef {
    /// Parse the daemon's own scheme's URIs. weftd routes on the scheme, so
    /// anything else reaching us is a bug upstream — refused, not guessed at.
    pub fn parse(uri: &str) -> Option<Self> {
        let rest = uri.strip_prefix("matrix://")?;
        let mut parts = rest.split('/');
        let realm = parts.next()?.to_string();
        let space = parts.next()?.to_string();
        let room = parts.next().map(str::to_string);

        if realm.is_empty() || space.is_empty() || parts.next().is_some() {
            return None;
        }

        Some(Self { realm, space, room })
    }

    /// The Matrix alias this space resolves through: `#<space>:<realm>`.
    pub fn alias(&self) -> String {
        format!("#{}:{}", self.space, self.realm)
    }

    pub fn uri(&self) -> String {
        match &self.room {
            Some(room) => format!("matrix://{}/{}/{}", self.realm, self.space, room),
            None => format!("matrix://{}/{}", self.realm, self.space),
        }
    }

    /// The URI of a room under this space.
    pub fn room_uri(&self, room: &str) -> String {
        format!("matrix://{}/{}/{room}", self.realm, self.space)
    }
}

/// A deterministic ULID for a structure object (space, room, role), derived
/// from its Matrix id. Same room ⇒ same ULID, across restarts and stores.
///
/// Built through `from_parts` rather than from the hash wholesale: a raw u128
/// overflows the 48-bit timestamp field, and such a value does **not** survive
/// a parse round trip — weftd would store a different id than the one we minted
/// and every map keyed on ours would miss. The timestamp bits carry hash
/// material too (structure ids are not time-ordered), so collisions stay as
/// unlikely as the 128 bits allow.
pub fn stable_ulid(matrix_id: &str) -> String {
    let hash = Sha256::digest(matrix_id.as_bytes());
    let mut ts = [0u8; 8];
    ts[2..].copy_from_slice(&hash[..6]); // 48 bits, the field's full width
    let timestamp = u64::from_be_bytes(ts);
    let random = u128::from_be_bytes(hash[6..22].try_into().expect("16 bytes"));

    ulid::Ulid::from_parts(timestamp, random)
        .to_string()
        .to_ascii_lowercase()
}

/// The msgid we mint for a Matrix event: `<realm>/<ulid>`.
///
/// The ULID's **time bits are the event's `origin_server_ts`** — the replica
/// is multi-origin and ordered by ULID time, so a hash-random timestamp would
/// scramble every conversation. The random bits derive from the event id, so
/// the same event always maps to the same msgid (idempotent re-ingestion)
/// without consulting any stored map.
pub fn msgid_for(realm: &str, event_id: &str, origin_server_ts_ms: u64) -> String {
    let hash = Sha256::digest(event_id.as_bytes());
    let rand = u128::from_be_bytes(hash[..16].try_into().expect("16 bytes"));
    let ulid = ulid::Ulid::from_parts(origin_server_ts_ms, rand);

    format!("{realm}/{}", ulid.to_string().to_ascii_lowercase())
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    fn identity() -> MatrixIdentity {
        MatrixIdentity::new("weft.example", "weft_", "weftbot")
    }

    fn mxid(raw: &str) -> &ruma::UserId {
        raw.try_into().expect("a valid MXID")
    }

    #[test]
    fn the_bot_and_puppets_share_one_namespace() {
        let id = identity();

        assert_eq!(id.bot_mxid(), "@weftbot:weft.example");
        assert_eq!(id.puppet_localpart("01abc"), "weft_01abc");
        assert_eq!(id.puppet_mxid("01abc"), "@weft_01abc:weft.example");
        // Every id is derived from all three fields, so the round trip holds.
        assert_eq!(
            id.puppet_ulid(mxid(&id.puppet_mxid("01abc"))),
            Some("01abc")
        );
    }

    #[test]
    fn our_own_ids_are_recognised_as_ours() {
        // This is what keeps a relay from being re-ingested: our bot's and
        // puppets' events *are* WEFT events already.
        let id = identity();

        assert!(id.is_ours(mxid("@weftbot:weft.example")));
        assert!(id.is_ours(mxid("@weft_01abc:weft.example")));
    }

    #[test]
    fn a_foreign_user_is_never_ours_even_with_our_localpart_shape() {
        let id = identity();

        assert!(!id.is_ours(mxid("@carol:kde.org")));
        // The prefix alone must not be enough — on someone else's homeserver it
        // is just a name, and treating it as ours would drop a real user's
        // traffic as if it were our own echo.
        assert!(!id.is_ours(mxid("@weft_01abc:kde.org")));
        assert_eq!(id.puppet_ulid(mxid("@weft_01abc:kde.org")), None);
    }

    #[test]
    fn the_bot_is_ours_but_is_not_a_puppet() {
        // `puppet_ulid` answers "which account is this the puppet of", and the
        // bot is the puppet of nobody — a caller that conflated the two would
        // attribute bridge traffic to an account.
        let id = identity();

        assert!(id.is_ours(mxid("@weftbot:weft.example")));
        assert_eq!(id.puppet_ulid(mxid("@weftbot:weft.example")), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_is_injective_and_stays_in_the_account_charset() {
        // The dangerous pair: distinct localparts that a lossy mapping merges.
        let a = escape_localpart("Alice");
        let b = escape_localpart("alice");
        assert_ne!(a, b);
        assert_eq!(b, "alice");

        // `=` always escapes, so `=41lice` cannot collide with escaped `Alice`.
        assert_ne!(escape_localpart("=41lice"), a);

        for c in [a, escape_localpart("weird/{user}!")] {
            assert!(
                c.bytes().all(|b| plain(b) || b == b'='),
                "outside the account charset: {c}"
            );
        }
    }

    #[test]
    fn escaping_round_trips_through_unescape() {
        for lp in ["alice", "Alice", "weird/{user}!", "=41", "a=b", "über"] {
            assert_eq!(
                unescape_localpart(&escape_localpart(lp)).as_deref(),
                Some(lp),
                "round trip of {lp:?}"
            );
        }

        let mxid: &ruma::UserId = "@Weird User:kde.org".try_into().unwrap();
        let handle = weft_user(mxid).unwrap();
        assert_eq!(
            mxid_of_weft_user(&handle).as_deref(),
            Some("@Weird User:kde.org"),
            "the WEFT handle addresses the original MXID"
        );
    }

    #[test]
    fn mxids_map_to_weft_users_or_are_refused() {
        let carol: &ruma::UserId = "@carol:kde.org".try_into().unwrap();
        assert_eq!(weft_user(carol).as_deref(), Some("carol@kde.org"));

        // A port or IP literal is not a WEFT network name — refused, not bent.
        let ported: &ruma::UserId = "@x:kde.org:8448".try_into().unwrap();
        assert_eq!(weft_user(ported), None);

        // Escaping past the 64-char account limit would need truncation, and
        // truncation collides. Refused.
        let long: String = format!("@{}:kde.org", "Ä".repeat(20));
        let long: &ruma::UserId = long.as_str().try_into().unwrap();
        assert_eq!(weft_user(long), None);
    }

    #[test]
    fn space_uris_parse_and_round_trip() {
        let space = SpaceRef::parse("matrix://matrix.org/gaming").unwrap();
        assert_eq!(space.alias(), "#gaming:matrix.org");
        assert_eq!(space.room, None);
        assert_eq!(space.uri(), "matrix://matrix.org/gaming");
        assert_eq!(
            space.room_uri("general"),
            "matrix://matrix.org/gaming/general"
        );

        let room = SpaceRef::parse("matrix://matrix.org/gaming/general").unwrap();
        assert_eq!(room.room.as_deref(), Some("general"));

        assert_eq!(SpaceRef::parse("discord://x/y"), None);
        assert_eq!(SpaceRef::parse("matrix://matrix.org"), None);
        assert_eq!(SpaceRef::parse("matrix://m/a/b/c"), None);
    }

    #[test]
    fn minted_ids_survive_a_parse_round_trip() {
        // The ids we mint are pinned by weftd, which parses them — an id that
        // re-encodes differently would leave every map keyed on ours missing.
        for id in ["!space:kde.org", "!gen:kde.org", "!zzz:matrix.org", "$ev"] {
            let minted = stable_ulid(id);
            let parsed: ulid::Ulid = minted.parse().expect("a canonical ULID");
            assert_eq!(
                parsed.to_string().to_ascii_lowercase(),
                minted,
                "round trip of {id}"
            );
        }

        let msgid = msgid_for("kde.org", "$ev", 1_722_000_000_000);
        let ulid_part = msgid.split('/').nth(1).unwrap();
        let parsed: ulid::Ulid = ulid_part.parse().expect("a canonical ULID");
        assert_eq!(parsed.to_string().to_ascii_lowercase(), ulid_part);
    }

    #[test]
    fn ids_are_deterministic_and_msgids_carry_real_time() {
        assert_eq!(stable_ulid("!room:kde.org"), stable_ulid("!room:kde.org"));
        assert_ne!(stable_ulid("!a:kde.org"), stable_ulid("!b:kde.org"));

        let ts = 1_722_000_000_000u64;
        let m = msgid_for("matrix.org", "$event1", ts);
        assert_eq!(m, msgid_for("matrix.org", "$event1", ts), "idempotent");

        let ulid: ulid::Ulid = m.split('/').nth(1).unwrap().parse().unwrap();
        assert_eq!(ulid.timestamp_ms(), ts, "ordered by real event time");
    }
}
