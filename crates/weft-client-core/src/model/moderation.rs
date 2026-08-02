//! Moderation domain (§6.7) — the mute/ban **deny-list cache**, keyed by scope.
//! The Rust mirror of `moderationHandlers`. The `MODERATED` event carries both
//! sides: `mute`/`ban` add-or-replace an entry, `unmute`/`unban` remove it, `kick`
//! is transient (no cache entry). The posting/join GATE (which covering scopes
//! apply to a channel, and the resulting `can_post`) stays in TS — this owns only
//! the cache, keyed by the scope the event names.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::ClientEvent;

/// One deny-list entry at a scope: a mute or ban on an account.
#[derive(Serialize, Clone)]
pub struct DenyRow {
    pub account: String,
    /// `"mute"` or `"ban"`.
    pub kind: String,
    pub by: Option<String>,
    pub reason: Option<String>,
}

/// This domain's state diff — the mirror sets `store.deny[scope]`. Sent as the
/// scope's whole list (idempotent → a MOD LIST re-fetch / reconnect replaces
/// cleanly).
#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ModDiff {
    Deny { scope: String, rows: Vec<DenyRow> },
}

/// The moderation sub-model: scope → deny rows. Transient (rebuilt from events).
#[derive(Default)]
pub struct Moderation {
    deny: BTreeMap<String, Vec<DenyRow>>,
}

impl Moderation {
    pub fn handle(&mut self, event: &ClientEvent) -> Vec<ModDiff> {
        match event {
            ClientEvent::Moderated { scope, account, action, by, reason } => {
                self.moderated(scope, account, action, by.clone(), reason.clone())
            }
            _ => Vec::new(),
        }
    }

    fn moderated(
        &mut self,
        scope: &str,
        account: &str,
        action: &str,
        by: Option<String>,
        reason: Option<String>,
    ) -> Vec<ModDiff> {
        match action {
            "mute" | "ban" => {
                let rows = self.deny.entry(scope.to_string()).or_default();
                let rec = DenyRow { account: account.to_string(), kind: action.to_string(), by, reason };

                // Re-mute/ban updates `by`/`reason` in place; a new one appends.
                match rows.iter_mut().find(|r| r.account == account && r.kind == action) {
                    Some(existing) => *existing = rec,
                    None => rows.push(rec),
                }

                vec![ModDiff::Deny { scope: scope.to_string(), rows: rows.clone() }]
            }
            "unmute" | "unban" => {
                let kind = if action == "unmute" { "mute" } else { "ban" };
                let Some(rows) = self.deny.get_mut(scope) else { return Vec::new() };

                let before = rows.len();
                rows.retain(|r| !(r.account == account && r.kind == kind));

                if rows.len() == before {
                    return Vec::new(); // nothing matched → no diff
                }

                vec![ModDiff::Deny { scope: scope.to_string(), rows: rows.clone() }]
            }
            // `kick` (and anything else) is transient — no deny-list entry.
            _ => Vec::new(),
        }
    }

    /// Clear a scope's deny list ahead of a `MOD LIST` re-fetch (the `refreshBans`
    /// reset). Emits the now-empty list; the batch response repopulates it.
    pub(super) fn clear(&mut self, scope: &str) -> Vec<ModDiff> {
        self.deny.insert(scope.to_string(), Vec::new());
        vec![ModDiff::Deny { scope: scope.to_string(), rows: Vec::new() }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn moderated(scope: &str, account: &str, action: &str) -> ClientEvent {
        ClientEvent::Moderated {
            scope: scope.into(),
            account: account.into(),
            action: action.into(),
            by: Some("mod".into()),
            reason: None,
        }
    }
    fn rows_of(d: &ModDiff) -> Vec<(&str, &str)> {
        let ModDiff::Deny { rows, .. } = d;
        rows.iter().map(|r| (r.account.as_str(), r.kind.as_str())).collect()
    }

    #[test]
    fn mute_ban_add_and_lift_remove() {
        let mut m = Moderation::default();
        assert_eq!(rows_of(&m.handle(&moderated("ns:x", "alice", "mute"))[0]), vec![("alice", "mute")]);
        assert_eq!(rows_of(&m.handle(&moderated("ns:x", "bob", "ban"))[0]),
                   vec![("alice", "mute"), ("bob", "ban")]);
        // Unmute removes just the matching (account, kind).
        assert_eq!(rows_of(&m.handle(&moderated("ns:x", "alice", "unmute"))[0]), vec![("bob", "ban")]);
    }

    #[test]
    fn re_mute_replaces_in_place_and_kick_is_transient() {
        let mut m = Moderation::default();
        m.handle(&moderated("ns:x", "alice", "mute"));
        // Re-mute updates in place (still one row, same position).
        let d = &m.handle(&moderated("ns:x", "alice", "mute"))[0];
        assert_eq!(rows_of(d), vec![("alice", "mute")]);
        // Kick emits nothing; lifting a non-existent entry emits nothing.
        assert!(m.handle(&moderated("ns:x", "carol", "kick")).is_empty());
        assert!(m.handle(&moderated("ns:x", "ghost", "unban")).is_empty());
    }

    #[test]
    fn clear_empties_the_scope() {
        let mut m = Moderation::default();
        m.handle(&moderated("ns:x", "alice", "mute"));
        let ModDiff::Deny { rows, .. } = &m.clear("ns:x")[0];
        assert!(rows.is_empty());
    }
}
