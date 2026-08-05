//! Per-space bridging bans (bridge-session-protocol §11).
//!
//! weftd tells the provider **once** — `BRIDGING <ns-id> banned|allowed`, when
//! an operator flips it in the admin panel — and keeps no record, so nothing is
//! re-sent on reconnect. The adapter must persist the list and re-apply it, or
//! a restart silently resumes a banned space.
//!
//! This lives in the SDK so every adapter enforces bans the same way instead of
//! re-implementing (and subtly forgetting) them per platform. What "stop
//! bridging" *means* — leaving a Matrix room, ignoring a Discord guild — stays
//! the adapter's; this type only answers "is this space banned?" at the three
//! places every adapter must ask: before asserting, before provisioning, and
//! before ingesting.

use std::collections::BTreeSet;

use weft_proto::{BridgingState, Event};

/// The banned namespaces, by the ULID the adapter minted for them.
///
/// Serializable so the adapter can persist it verbatim — which it must, since
/// weftd never repeats a ban.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct BanList {
    banned: BTreeSet<String>,
}

impl BanList {
    /// Apply a wire event. Returns the namespace and its new banned state when
    /// the event was a `BRIDGING` instruction — the moment to persist the list
    /// and actually stop (or resume) on the foreign side.
    pub fn apply(&mut self, event: &Event) -> Option<(String, bool)> {
        let Event::Bridging { namespace, state } = event else {
            return None;
        };
        let ns = namespace.to_string();
        let banned = *state == BridgingState::Banned;

        if banned {
            self.banned.insert(ns.clone());
        } else {
            self.banned.remove(&ns);
        }

        Some((ns, banned))
    }

    /// Whether bridging this namespace is banned. Ask before asserting it,
    /// before answering a `PROVISION` for it, and before ingesting into it.
    pub fn is_banned(&self, ns_id: &str) -> bool {
        self.banned.contains(ns_id)
    }

    pub fn is_empty(&self) -> bool {
        self.banned.is_empty()
    }

    /// The banned namespace ids — for persisting to whatever store the
    /// adapter chose.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.banned.iter().map(String::as_str)
    }
}

/// Restoring from the adapter's store.
impl FromIterator<String> for BanList {
    fn from_iter<I: IntoIterator<Item = String>>(iter: I) -> Self {
        Self {
            banned: iter.into_iter().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ban_is_stored_until_lifted_and_survives_serde() {
        let mut bans = BanList::default();
        let ns: weft_proto::NamespaceId = "01arz3ndektsv4rrffq69g5fav".parse().unwrap();

        let applied = bans.apply(&Event::Bridging {
            namespace: ns,
            state: BridgingState::Banned,
        });
        assert_eq!(applied, Some((ns.to_string(), true)));
        assert!(bans.is_banned(&ns.to_string()));

        // Persistence is the point: weftd never re-sends a ban, so the list
        // must round-trip through the adapter's own store.
        let json = serde_json::to_string(&bans).unwrap();
        let restored: BanList = serde_json::from_str(&json).unwrap();
        assert!(restored.is_banned(&ns.to_string()));

        let lifted = bans.apply(&Event::Bridging {
            namespace: ns,
            state: BridgingState::Allowed,
        });
        assert_eq!(lifted, Some((ns.to_string(), false)));
        assert!(!bans.is_banned(&ns.to_string()));

        // Anything else is not a bridging instruction.
        assert!(bans.apply(&Event::SyncStart).is_none());
    }
}
