//! Invites domain (§6.5) — the invite **list** for a scope. The Rust mirror of the
//! reducer's invite-batch flush + the revoke list-drop. The list streams via the
//! `il`-prefixed BATCH (INVITE-INFO rows) and flushes atomically on its end; an
//! `INVITED` with `max-uses=0` (a revoke echo) drops one. The create-screen state
//! (link / id / open flags / scope), the mint link, and the re-fetch stay TS (UI).

use serde::Serialize;

use crate::ClientEvent;

/// One live invite in the §6.5 invites menu. Snake_case fields mirror the TS
/// `InviteInfo` the list renders.
#[derive(Serialize, Clone)]
pub struct InviteInfo {
    pub scope: String,
    pub invite_id: String,
    pub creator: String,
    pub uses_left: Option<u32>,
    pub used: u32,
    pub expiry: Option<u64>,
}

/// This domain's state diff — the mirror sets `store.invites.list`. Sent as the
/// whole list (idempotent → a re-fetch / revoke replaces cleanly).
#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum InviteDiff {
    InviteList { invites: Vec<InviteInfo> },
}

/// The invites sub-model: the last-fetched scope's list + the streaming-batch
/// state. Transient (fetched on demand).
#[derive(Default)]
pub struct Invites {
    list: Vec<InviteInfo>,
    // Buffer while an invite-list batch streams; flushed to `list` on its end.
    buf: Vec<InviteInfo>,
    in_batch: bool,
}

impl Invites {
    pub fn handle(&mut self, event: &ClientEvent) -> Vec<InviteDiff> {
        match event {
            ClientEvent::InviteInfo { scope, invite_id, creator, uses_left, used, expiry } => {
                self.buf.push(InviteInfo {
                    scope: scope.clone(),
                    invite_id: invite_id.clone(),
                    creator: creator.clone(),
                    uses_left: *uses_left,
                    used: *used,
                    expiry: *expiry,
                });

                Vec::new() // buffered; the diff is emitted at the batch's end
            }
            // §6.5 invite-list batches are id-prefixed `il`; mark the window so the
            // matching BATCH END flushes the buffered invites.
            ClientEvent::BatchStart { id } if id.starts_with("il") => {
                self.in_batch = true;

                Vec::new()
            }
            ClientEvent::BatchEnd { .. } => self.flush(),
            // §6.5 a revoke echo (INVITED … max-uses=0) drops the invite from the list.
            ClientEvent::Invited { invite_id, max_uses, .. } if *max_uses == Some(0) => {
                self.revoke(invite_id)
            }
            _ => Vec::new(),
        }
    }

    fn flush(&mut self) -> Vec<InviteDiff> {
        if !self.in_batch {
            return Vec::new();
        }

        self.in_batch = false;
        self.list = std::mem::take(&mut self.buf);

        vec![InviteDiff::InviteList { invites: self.list.clone() }]
    }

    fn revoke(&mut self, invite_id: &str) -> Vec<InviteDiff> {
        let before = self.list.len();
        self.list.retain(|i| i.invite_id != invite_id);

        if self.list.len() == before {
            return Vec::new();
        }

        vec![InviteDiff::InviteList { invites: self.list.clone() }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invite_info(scope: &str, id: &str) -> ClientEvent {
        ClientEvent::InviteInfo {
            scope: scope.into(),
            invite_id: id.into(),
            creator: "alice".into(),
            uses_left: Some(5),
            used: 0,
            expiry: None,
        }
    }
    fn batch_start(id: &str) -> ClientEvent {
        ClientEvent::BatchStart { id: id.into() }
    }
    fn batch_end() -> ClientEvent {
        ClientEvent::BatchEnd { id: "il1".into(), truncated: false }
    }
    fn revoked(id: &str) -> ClientEvent {
        ClientEvent::Invited { scope: "ns:x".into(), invite_id: id.into(), link: None, max_uses: Some(0) }
    }
    fn ids(diffs: &[InviteDiff]) -> Vec<&str> {
        let InviteDiff::InviteList { invites } = &diffs[0];
        invites.iter().map(|i| i.invite_id.as_str()).collect()
    }

    #[test]
    fn list_batch_buffers_then_flushes_on_end() {
        let mut inv = Invites::default();
        assert!(inv.handle(&batch_start("il1")).is_empty());
        assert!(inv.handle(&invite_info("ns:x", "i1")).is_empty()); // buffered, no diff
        assert!(inv.handle(&invite_info("ns:x", "i2")).is_empty());
        assert_eq!(ids(&inv.handle(&batch_end())), vec!["i1", "i2"]);
    }

    #[test]
    fn non_invite_batch_end_does_not_flush() {
        let mut inv = Invites::default();
        // A roles batch end (no `il` start) must not flush an empty invite list.
        assert!(inv.handle(&ClientEvent::BatchEnd { id: "r1".into(), truncated: false }).is_empty());
    }

    #[test]
    fn refetch_replaces_the_list() {
        let mut inv = Invites::default();
        inv.handle(&batch_start("il1"));
        inv.handle(&invite_info("ns:x", "i1"));
        inv.handle(&invite_info("ns:x", "i2"));
        inv.handle(&batch_end());

        // A re-fetch carrying only one invite replaces (not merges).
        inv.handle(&batch_start("il2"));
        inv.handle(&invite_info("ns:x", "i3"));
        assert_eq!(ids(&inv.handle(&batch_end())), vec!["i3"]);
    }

    #[test]
    fn revoke_drops_and_mint_is_a_noop() {
        let mut inv = Invites::default();
        inv.handle(&batch_start("il1"));
        inv.handle(&invite_info("ns:x", "i1"));
        inv.handle(&invite_info("ns:x", "i2"));
        inv.handle(&batch_end());

        // INVITED with max-uses=0 drops just that invite.
        assert_eq!(ids(&inv.handle(&revoked("i1"))), vec!["i2"]);
        // Revoking an unknown invite → no diff.
        assert!(inv.handle(&revoked("ghost")).is_empty());
        // A mint echo (max-uses > 0) doesn't touch the list.
        let mint = ClientEvent::Invited {
            scope: "ns:x".into(), invite_id: "i9".into(), link: Some("weft://x".into()), max_uses: Some(1),
        };
        assert!(inv.handle(&mint).is_empty());
    }
}
