//! Messages domain (§9) — the message **store** (design:
//! `docs/architecture/client-core-model-migration.md`, messages capstone).
//!
//! Rust owns the per-channel ordered buffer and the ordering-sensitive **mutation
//! semantics** — local-echo → ack reconciliation, edit, redact, react — and (later)
//! unread/mention derivation + modseq/gap bookkeeping. TS owns the *render window*
//! (scroll/anchor/heights/day-dividers/grouping) and adds presentation the store
//! doesn't carry (the render `key`, `ts`/`time` — the store has **no clock**, so
//! the TS window derives those from the message `id` / arrival).
//!
//! Diffs are **thin live-tail** deltas; bulk message bodies never stream here —
//! they enter TS via the `messages_range` pull (M3). Scoping tail diffs to *open*
//! channels + `UnreadChanged` for background channels is M3 too.
//!
//! **M1 (this file): the isolated store + semantics + diffs + a `range` reader,
//! unit-tested and NOT wired into `reduce`** — the app stays on the TS message path
//! until the M4 cutover. History-batch messages are skipped (the pull path owns
//! older history).

use std::collections::BTreeMap;

use serde::Serialize;

use crate::ClientEvent;

/// One stored aggregate reaction (emoji → count + whether *I* reacted).
#[derive(Serialize, Clone, Default, PartialEq, Debug)]
pub struct Reaction {
    pub count: i64,
    pub mine: bool,
}

/// A stored message. `id` is the server msgid, or a client `local:<n>` id for a
/// still-`pending` local echo (swapped to the server id on ack/reconcile). The
/// store carries no `ts`/`key` — the TS window derives those from `id`.
#[derive(Serialize, Clone, Default, PartialEq, Debug)]
pub struct Msg {
    pub id: String,
    pub author: String,
    pub body: String,
    pub system: bool,
    pub own: bool,
    pub edited: bool,
    pub md: bool,
    pub reply_to: Option<String>,
    pub thread: Option<String>,
    pub bridged: bool,
    /// The sender's network (always) — the TS window derives foreign `@net`/`who`.
    pub network: String,
    pub attachments: Vec<String>,
    /// The optimistic-send label; matched to reconcile the local echo with its ack.
    pub label: Option<String>,
    pub reactions: BTreeMap<String, Reaction>,
    pub pending: bool,
    pub failed: bool,
}

/// A channel's unread tally. Model-derived (the *truth*); the TS render layer
/// decides whether to *show* it (muted scopes / the active channel stay silent).
#[derive(Serialize, Clone, Copy, Default, PartialEq, Debug)]
pub struct Unread {
    pub count: i64,
    pub mentions: i64,
}

/// Thin live-tail diffs — the only per-message push. `MsgUpdated` identifies the
/// target by its **current** `id` (which may differ from `msg.id` on a
/// local→server ack). `UnreadChanged` is the cheap derived diff a *background*
/// channel gets instead of the message body (the subscription split, M3).
#[derive(Serialize, Clone, PartialEq, Debug)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum MsgDiff {
    MsgAppended { channel: String, msg: Msg },
    MsgUpdated { channel: String, id: String, msg: Msg },
    MsgRemoved { channel: String, id: String },
    UnreadChanged { channel: String, count: i64, mentions: i64 },
}

/// The message store: per-channel ordered buffers, plus session identity for
/// `mine`/`own` and federation labeling.
#[derive(Default)]
pub struct Messages {
    home: String,
    me: String,
    buffers: BTreeMap<String, Vec<Msg>>,
    /// Per-channel unread tally (server `UNREAD-COUNTS` snapshot + live increments;
    /// reset on `MARKED`). The model's authoritative count; display gating is TS.
    unread: BTreeMap<String, Unread>,
    /// Monotonic counter for `local:<n>` optimistic-echo ids.
    local_seq: u64,
}

impl Messages {
    /// Wire dispatch for events that need **no cross-domain input**. A live
    /// `MESSAGE` is *not* here — it needs `mentioned` (from the roles domain), so
    /// `AppState` computes that and calls [`ingest`](Self::ingest). History-batch
    /// messages are skipped (the pull path owns older history).
    pub fn handle(&mut self, event: &ClientEvent) -> Vec<MsgDiff> {
        match event {
            ClientEvent::Connected { network, account } => {
                self.home = network.clone();
                self.me = account.clone();

                Vec::new()
            }
            ClientEvent::Edited { target, edit_of, body, .. } => self.edit(target, edit_of, body),
            ClientEvent::Deleted { target, msgid } => self.redact(target, msgid),
            ClientEvent::Reaction { target, msgid, emoji, op, by } => {
                self.react(target, msgid, emoji, op, by)
            }
            // §9.7 read-marker sync from another device → the channel is caught up.
            ClientEvent::Marked { channel, .. } => self.mark_read(channel),
            // §6.3 server-authoritative tally (login/SYNC snapshot, cross-device).
            ClientEvent::UnreadCounts { channel, unread, mentions } => {
                self.set_unread(channel, *unread as i64, *mentions as i64)
            }
            _ => Vec::new(),
        }
    }

    /// The session account (from `Connected`) — `AppState` reads it to derive a
    /// message's `mentioned` flag via the roles domain.
    pub fn me(&self) -> &str {
        &self.me
    }

    /// Optimistic send: insert a `pending` local echo. Returns its `local:<n>` id
    /// (the host keeps it to reconcile) + the append diff.
    pub fn insert_pending(&mut self, channel: &str, label: &str, body: &str, md: bool) -> (String, Vec<MsgDiff>) {
        self.local_seq += 1;
        let id = format!("local:{}", self.local_seq);

        let msg = Msg {
            id: id.clone(),
            author: self.me.clone(),
            body: body.to_string(),
            own: true,
            md,
            network: self.home.clone(),
            label: Some(label.to_string()),
            pending: true,
            ..Default::default()
        };

        self.buffers.entry(channel.to_string()).or_default().push(msg.clone());
        (id.clone(), vec![MsgDiff::MsgAppended { channel: channel.to_string(), msg }])
    }

    /// Mark a pending echo failed (send error) so the UI can show a retry.
    pub fn fail_pending(&mut self, channel: &str, id: &str) -> Vec<MsgDiff> {
        let Some(buf) = self.buffers.get_mut(channel) else { return Vec::new() };
        let Some(m) = buf.iter_mut().find(|m| m.id == id && m.pending) else { return Vec::new() };

        m.failed = true;
        m.pending = false;

        vec![MsgDiff::MsgUpdated { channel: channel.to_string(), id: id.to_string(), msg: m.clone() }]
    }

    /// Ingest a live `MESSAGE`: reconcile our own echo (by label) with its pending,
    /// else upsert by id (a re-delivery — keep accumulated reactions), else append.
    /// A fresh non-own append also **bumps the unread tally** (`mentioned` — a
    /// mention of me — additionally bumps mentions; the caller derives it from the
    /// roles domain). Returns the message diff, plus `UnreadChanged` when it moved.
    pub fn ingest(&mut self, event: &ClientEvent, mentioned: bool) -> Vec<MsgDiff> {
        let ClientEvent::Message {
            target, sender, network, msgid, body, attachments, system, own, edited, reply_to, thread, md, label, ..
        } = event
        else {
            return Vec::new();
        };

        let Some(channel) = channel_key(target, sender, *own) else { return Vec::new() };
        let is_system = system.is_some();

        let msg = Msg {
            id: msgid.clone(),
            author: sender.clone(),
            body: match system.as_deref() {
                Some(kind) => {
                    let who = if network != &self.home { format!("{sender}@{network}") } else { sender.clone() };

                    system_line(&who, kind)
                }
                None => body.clone(),
            },
            system: is_system,
            own: *own && !is_system,
            edited: *edited,
            md: *md && !is_system,
            reply_to: reply_to.clone(),
            thread: thread.clone(),
            bridged: network != &self.home,
            network: network.clone(),
            attachments: attachments.clone(),
            label: label.clone(),
            reactions: BTreeMap::new(),
            pending: false,
            failed: false,
        };

        let buf = self.buffers.entry(channel.clone()).or_default();

        // §3.5/§11.13 reconcile: our echoed copy (by label) replaces the pending.
        if msg.own {
            if let Some(label) = &msg.label {
                if let Some(idx) = buf.iter().position(|m| m.pending && m.label.as_deref() == Some(label.as_str())) {
                    let old_id = buf[idx].id.clone();
                    buf[idx] = msg.clone();

                    return vec![MsgDiff::MsgUpdated { channel, id: old_id, msg }];
                }
            }
        }

        // Upsert by id (re-delivery / offline edit) — keep accumulated reactions.
        if let Some(idx) = buf.iter().position(|m| m.id == msg.id) {
            let mut msg = msg;
            msg.reactions = buf[idx].reactions.clone();
            buf[idx] = msg.clone();

            return vec![MsgDiff::MsgUpdated { channel, id: msg.id.clone(), msg }];
        }

        buf.push(msg.clone());
        let mut out = vec![MsgDiff::MsgAppended { channel: channel.clone(), msg }];

        // A fresh message from someone else increments the unread tally (a mention
        // additionally the mention tally). Own messages never count as unread.
        if !*own {
            let u = self.unread.entry(channel.clone()).or_default();
            u.count += 1;

            if mentioned {
                u.mentions += 1;
            }

            out.push(MsgDiff::UnreadChanged { channel, count: u.count, mentions: u.mentions });
        }

        out
    }

    /// §9.7 a channel is caught up (`MARKED`, or the local view marked it read):
    /// zero the tally. No-op (no diff) if already zero.
    fn mark_read(&mut self, channel: &str) -> Vec<MsgDiff> {
        let u = self.unread.entry(channel.to_string()).or_default();

        if u.count == 0 && u.mentions == 0 {
            return Vec::new();
        }

        *u = Unread::default();
        vec![MsgDiff::UnreadChanged { channel: channel.to_string(), count: 0, mentions: 0 }]
    }

    /// §6.3 adopt the server-authoritative tally (the login/SYNC snapshot + the
    /// cross-device push). No-op when unchanged.
    fn set_unread(&mut self, channel: &str, count: i64, mentions: i64) -> Vec<MsgDiff> {
        let next = Unread { count, mentions };

        if self.unread.get(channel) == Some(&next) {
            return Vec::new();
        }

        self.unread.insert(channel.to_string(), next);
        vec![MsgDiff::UnreadChanged { channel: channel.to_string(), count, mentions }]
    }

    fn edit(&mut self, target: &str, edit_of: &str, body: &str) -> Vec<MsgDiff> {
        let Some(buf) = self.buffers.get_mut(target) else { return Vec::new() };
        let Some(m) = buf.iter_mut().find(|m| m.id == edit_of) else { return Vec::new() };

        m.body = body.to_string();
        m.edited = true;

        vec![MsgDiff::MsgUpdated { channel: target.to_string(), id: edit_of.to_string(), msg: m.clone() }]
    }

    fn redact(&mut self, target: &str, msgid: &str) -> Vec<MsgDiff> {
        let Some(buf) = self.buffers.get_mut(target) else { return Vec::new() };

        let before = buf.len();
        buf.retain(|m| m.id != msgid);

        if buf.len() == before {
            return Vec::new();
        }

        vec![MsgDiff::MsgRemoved { channel: target.to_string(), id: msgid.to_string() }]
    }

    fn react(&mut self, target: &str, msgid: &str, emoji: &str, op: &str, by: &str) -> Vec<MsgDiff> {
        let me = self.me.clone();
        let Some(buf) = self.buffers.get_mut(target) else { return Vec::new() };
        let Some(m) = buf.iter_mut().find(|m| m.id == msgid) else { return Vec::new() };

        apply_reaction(&mut m.reactions, emoji, op, by, &me);
        vec![MsgDiff::MsgUpdated { channel: target.to_string(), id: msgid.to_string(), msg: m.clone() }]
    }

    /// The window for `messages_range`: up to `limit` messages, ending before
    /// `before` (exclusive) if given, else the newest. Newest-last order.
    pub fn range(&self, channel: &str, before: Option<&str>, limit: usize) -> Vec<Msg> {
        let Some(buf) = self.buffers.get(channel) else { return Vec::new() };

        let end = match before {
            Some(id) => buf.iter().position(|m| m.id == id).unwrap_or(buf.len()),
            None => buf.len(),
        };

        let start = end.saturating_sub(limit);
        buf[start..end].to_vec()
    }
}

/// §9 conversation key: channel by name, group DM by id, 1:1 DM by the **other**
/// party (both sides → one conversation). `None` for an unrecognized target.
fn channel_key(target: &str, sender: &str, own: bool) -> Option<String> {
    if target.starts_with('#') || target.starts_with('&') {
        return Some(target.to_string());
    }

    target.strip_prefix('@').map(|peer| format!("@{}", if own { peer } else { sender }))
}

fn system_line(who: &str, kind: &str) -> String {
    match kind {
        "join" => format!("{who} joined"),
        "part" => format!("{who} left"),
        "welcome" => format!("👋 Welcome, {who}!"),
        other => format!("{who} {other}"),
    }
}

/// Port of the TS `applyReaction`: `add`/`remove` an emoji by `by`, tracking the
/// aggregate count + whether *I* (`me`) reacted; drop the entry at count 0.
fn apply_reaction(reactions: &mut BTreeMap<String, Reaction>, emoji: &str, op: &str, by: &str, me: &str) {
    let r = reactions.entry(emoji.to_string()).or_default();

    if op == "add" {
        r.count += 1;

        if by == me {
            r.mine = true;
        }
    } else {
        r.count -= 1;

        if by == me {
            r.mine = false;
        }
    }

    if r.count <= 0 {
        reactions.remove(emoji);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connected() -> ClientEvent {
        ClientEvent::Connected { network: "home".into(), account: "me".into() }
    }
    #[allow(clippy::too_many_arguments)]
    fn message(target: &str, sender: &str, network: &str, msgid: &str, body: &str, own: bool, label: Option<&str>) -> ClientEvent {
        ClientEvent::Message {
            target: target.into(), sender: sender.into(), network: network.into(), msgid: msgid.into(),
            body: body.into(), attachments: Vec::new(), system: None, own, history: false, edited: false,
            reply_to: None, thread: None, md: false, label: label.map(Into::into),
        }
    }

    #[test]
    fn ingest_appends_keyed_and_bridged() {
        let mut m = Messages::default();
        m.handle(&connected());
        let d = m.ingest(&message("#n/c", "alice", "home", "01a", "hi", false, None), false);
        assert!(matches!(&d[0], MsgDiff::MsgAppended { channel, msg } if channel == "#n/c" && msg.body == "hi" && !msg.bridged));
        // DM keys by the other party.
        let d = m.ingest(&message("@me", "bob", "home", "01b", "yo", false, None), false);
        assert!(matches!(&d[0], MsgDiff::MsgAppended { channel, .. } if channel == "@bob"));
    }

    #[test]
    fn local_echo_reconciles_with_its_ack() {
        let mut m = Messages::default();
        m.handle(&connected());
        let (local_id, d) = m.insert_pending("#n/c", "L1", "hey", false);
        assert!(matches!(&d[0], MsgDiff::MsgAppended { msg, .. } if msg.pending && msg.own && msg.id == local_id));
        // The server echo (own + same label) UPDATES the pending in place → server id.
        let d = m.ingest(&message("#n/c", "me", "home", "01srv", "hey", true, Some("L1")), false);
        assert!(matches!(&d[0], MsgDiff::MsgUpdated { id, msg, .. } if id == &local_id && msg.id == "01srv" && !msg.pending));
        // No duplicate: the buffer holds exactly one.
        assert_eq!(m.range("#n/c", None, 50).len(), 1);
    }

    #[test]
    fn upsert_by_id_keeps_reactions() {
        let mut m = Messages::default();
        m.handle(&connected());
        m.ingest(&message("#n/c", "alice", "home", "01a", "hi", false, None), false);
        m.react("#n/c", "01a", "👍", "add", "bob");
        // A re-delivery of the same msgid replaces body but keeps the reaction.
        let d = m.ingest(&message("#n/c", "alice", "home", "01a", "hi (edited)", false, None), false);
        let MsgDiff::MsgUpdated { msg, .. } = &d[0] else { panic!() };
        assert_eq!(msg.body, "hi (edited)");
        assert_eq!(msg.reactions.get("👍").unwrap().count, 1);
    }

    #[test]
    fn edit_redact_react_semantics() {
        let mut m = Messages::default();
        m.handle(&connected());
        m.ingest(&message("#n/c", "alice", "home", "01a", "hi", false, None), false);
        // edit
        let d = m.handle(&ClientEvent::Edited { target: "#n/c".into(), sender: "alice".into(), edit_of: "01a".into(), body: "hello".into() });
        assert!(matches!(&d[0], MsgDiff::MsgUpdated { msg, .. } if msg.body == "hello" && msg.edited));
        // react: mine toggles when it's me
        m.handle(&ClientEvent::Reaction { target: "#n/c".into(), msgid: "01a".into(), emoji: "❤".into(), op: "add".into(), by: "me".into() });
        let d = m.range("#n/c", None, 1);
        assert!(d[0].reactions.get("❤").unwrap().mine);
        // remove reaction drops it at 0
        m.handle(&ClientEvent::Reaction { target: "#n/c".into(), msgid: "01a".into(), emoji: "❤".into(), op: "remove".into(), by: "me".into() });
        assert!(m.range("#n/c", None, 1)[0].reactions.is_empty());
        // redact removes the message
        let d = m.handle(&ClientEvent::Deleted { target: "#n/c".into(), msgid: "01a".into() });
        assert!(matches!(&d[0], MsgDiff::MsgRemoved { id, .. } if id == "01a"));
        assert!(m.range("#n/c", None, 50).is_empty());
    }

    fn unread(diffs: &[MsgDiff]) -> Option<(i64, i64)> {
        diffs.iter().find_map(|d| match d {
            MsgDiff::UnreadChanged { count, mentions, .. } => Some((*count, *mentions)),
            _ => None,
        })
    }

    #[test]
    fn unread_bumps_on_others_messages_and_mentions() {
        let mut m = Messages::default();
        m.handle(&connected());
        // Someone else's message → unread 1, no mention.
        assert_eq!(unread(&m.ingest(&message("#n/c", "alice", "home", "01a", "hi", false, None), false)), Some((1, 0)));
        // A mention (caller passes `mentioned`) → unread 2, mention 1.
        assert_eq!(unread(&m.ingest(&message("#n/c", "alice", "home", "01b", "@me hi", false, None), true)), Some((2, 1)));
        // My own message never bumps unread (no UnreadChanged emitted).
        assert!(unread(&m.ingest(&message("#n/c", "me", "home", "01c", "yo", true, None), false)).is_none());
    }

    #[test]
    fn marked_clears_and_unread_counts_sets_authoritative() {
        let mut m = Messages::default();
        m.handle(&connected());
        m.ingest(&message("#n/c", "alice", "home", "01a", "hi", false, None), false);
        // MARKED (caught up) zeroes the tally.
        assert_eq!(unread(&m.handle(&ClientEvent::Marked { channel: "#n/c".into(), msgid: "01a".into() })), Some((0, 0)));
        // Already zero → no diff.
        assert!(m.handle(&ClientEvent::Marked { channel: "#n/c".into(), msgid: "01a".into() }).is_empty());
        // UNREAD-COUNTS overrides with the server tally.
        assert_eq!(unread(&m.handle(&ClientEvent::UnreadCounts { channel: "#n/c".into(), unread: 5, mentions: 2 })), Some((5, 2)));
        // Same tally again → no diff.
        assert!(m.handle(&ClientEvent::UnreadCounts { channel: "#n/c".into(), unread: 5, mentions: 2 }).is_empty());
    }

    #[test]
    fn range_paginates_before_a_cursor() {
        let mut m = Messages::default();
        m.handle(&connected());
        for i in 0..5 {
            m.ingest(&message("#n/c", "a", "home", &format!("0{i}"), "x", false, None), false);
        }
        // Newest 2.
        let newest = m.range("#n/c", None, 2);
        assert_eq!(newest.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), vec!["03", "04"]);
        // The 2 before "03".
        let older = m.range("#n/c", Some("03"), 2);
        assert_eq!(older.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), vec!["01", "02"]);
    }
}
