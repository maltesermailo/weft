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
pub fn stable_ulid(matrix_id: &str) -> String {
    let hash = Sha256::digest(matrix_id.as_bytes());
    let n = u128::from_be_bytes(hash[..16].try_into().expect("16 bytes"));

    ulid::Ulid::from(n).to_string().to_ascii_lowercase()
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
