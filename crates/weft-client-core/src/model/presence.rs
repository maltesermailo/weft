//! Presence domain (§7) — each account's live presence status. The Rust mirror of
//! `accountHandlers.presence`. A trivial map, but its own module: presence is
//! **global account identity**, not a channel concern — so it's the first domain
//! that isn't [`channels`](super::channels), demonstrating the multi-domain shape.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::ClientEvent;

/// This domain's state diff — the mirror sets `Account.presence`. The kind is
/// **`acct-presence`**, deliberately distinct from the raw `presence` wire event,
/// so the model's diff and the wire event never collide in the TS handler map.
#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PresenceDiff {
    AcctPresence { account: String, status: String },
}

/// The presence sub-model: account handle → last-known status. Transient (rebuilt
/// from events each session; reset on connect).
#[derive(Default)]
pub struct Presence {
    map: BTreeMap<String, String>,
}

impl Presence {
    /// Handle the presence events this domain owns; return the resulting diffs.
    pub fn handle(&mut self, event: &ClientEvent) -> Vec<PresenceDiff> {
        match event {
            ClientEvent::Presence { user, status } => self.set(user, status),
            _ => Vec::new(),
        }
    }

    // §7 record an account's presence. No-op (no diff) when unchanged — presence
    // is re-announced on join/reconnect, so the same status arrives repeatedly.
    fn set(&mut self, account: &str, status: &str) -> Vec<PresenceDiff> {
        if self.map.get(account).map(String::as_str) == Some(status) {
            return Vec::new();
        }

        self.map.insert(account.to_string(), status.to_string());
        vec![PresenceDiff::AcctPresence {
            account: account.to_string(),
            status: status.to_string(),
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn presence(user: &str, status: &str) -> ClientEvent {
        ClientEvent::Presence {
            user: user.into(),
            status: status.into(),
        }
    }

    #[test]
    fn presence_set_and_dedup() {
        let mut p = Presence::default();
        let diffs = p.handle(&presence("alice", "online"));
        assert_eq!(diffs.len(), 1);
        let PresenceDiff::AcctPresence { account, status } = &diffs[0];
        assert_eq!((account.as_str(), status.as_str()), ("alice", "online"));

        // Same status re-announced → no diff.
        assert!(p.handle(&presence("alice", "online")).is_empty());
        // A change emits again.
        assert_eq!(p.handle(&presence("alice", "away")).len(), 1);
    }

    #[test]
    fn ignores_other_events() {
        let mut p = Presence::default();
        assert!(p
            .handle(&ClientEvent::Closed { reason: "x".into() })
            .is_empty());
    }
}
