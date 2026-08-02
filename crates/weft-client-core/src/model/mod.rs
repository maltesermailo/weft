//! Client-core application model + the `reduce` dispatcher (model migration,
//! `docs/architecture/client-core-model-migration.md`).
//!
//! **Clear separation, three layers:**
//! - *Codec* (`crate` root): `ClientEvent` = the wire vocabulary; `build_*` = out.
//! - *Model* (this module): `AppState` owns one sub-state per migrated domain and
//!   emits its own [`StateDiff`] vocabulary — never reusing `ClientEvent`.
//! - *Per-domain handlers* (submodules like [`channels`]): each domain owns its
//!   state struct, its wire-event handler, and its diff enum — the Rust mirror of
//!   the TS per-domain `*Handlers` maps (`sync/channel-handlers.ts`, …).
//!
//! `reduce` is the registry: it offers each inbound wire event to every domain
//! handler and collects the diffs, exactly like the TS reducer's `domainHandlers`.
//! Pure — no I/O, WASM-safe.

pub mod channels;
pub mod emoji;
pub mod federation;
pub mod invites;
pub mod messages;
pub mod moderation;
pub mod namespaces;
pub mod presence;
pub mod reports;
pub mod roles;
pub mod social;
pub mod threads;

use std::collections::BTreeSet;

use serde::Serialize;

use crate::ClientEvent;

/// The model's output vocabulary — a state change the TS mirror applies. Kept
/// distinct from the wire [`ClientEvent`] (codec/model separation). Each domain
/// owns its own diff enum; this only aggregates them. `untagged` → the inner
/// domain diff (which carries its own `kind` tag) reaches TS verbatim, so the TS
/// mirror routes on `kind` exactly as it does for wire events.
#[derive(Serialize, Clone)]
#[serde(untagged)]
pub enum StateDiff {
    Chan(channels::ChanDiff),
    Presence(presence::PresenceDiff),
    Mod(moderation::ModDiff),
    Report(reports::ReportDiff),
    Emoji(emoji::EmojiDiff),
    Role(roles::RoleDiff),
    Msg(messages::MsgDiff),
    Invite(invites::InviteDiff),
    Federation(federation::FederationDiff),
    Social(social::SocialDiff),
    Thread(threads::ThreadDiff),
    Ns(namespaces::NsDiff),
}

/// The client-core model: one field per migrated domain. Grows one sub-state per
/// slice; S0 owns only [`channels`].
#[derive(Default)]
pub struct AppState {
    pub channels: channels::Channels,
    pub presence: presence::Presence,
    pub moderation: moderation::Moderation,
    pub reports: reports::Reports,
    pub emoji: emoji::Emoji,
    pub roles: roles::Roles,
    pub invites: invites::Invites,
    pub federation: federation::Federation,
    pub social: social::Social,
    pub threads: threads::Threads,
    pub namespaces: namespaces::Namespaces,
    pub messages: messages::Messages,
    /// Channels the frontend has declared **open** — the two-tier subscription
    /// scope. Message-body diffs (`MsgAppended`/`MsgUpdated`/`MsgRemoved`) push only
    /// for these; every other channel gets just the cheap `UnreadChanged`. Empty by
    /// default → minimal IPC until the frontend opens a channel. The sentinel `"*"`
    /// means **all channels** (the frontend's emit-all mode before scoping lands).
    open: BTreeSet<String>,
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Offer one inbound wire event to every domain handler and collect the
    /// resulting state diffs. Each domain handles the kinds it owns and ignores
    /// the rest (mirrors the TS `domainHandlers` spread). Un-migrated events yield
    /// no diffs; the sink glue (S1) still forwards the raw event to TS so its
    /// remaining handlers/side-effects fire.
    pub fn reduce(&mut self, event: &ClientEvent) -> Vec<StateDiff> {
        let mut out = Vec::new();
        out.extend(self.channels.handle(event).into_iter().map(StateDiff::Chan));
        out.extend(self.presence.handle(event).into_iter().map(StateDiff::Presence));
        out.extend(self.moderation.handle(event).into_iter().map(StateDiff::Mod));
        out.extend(self.reports.handle(event).into_iter().map(StateDiff::Report));
        out.extend(self.emoji.handle(event).into_iter().map(StateDiff::Emoji));
        out.extend(self.roles.handle(event).into_iter().map(StateDiff::Role));
        out.extend(self.invites.handle(event).into_iter().map(StateDiff::Invite));
        out.extend(self.federation.handle(event).into_iter().map(StateDiff::Federation));
        out.extend(self.social.handle(event).into_iter().map(StateDiff::Social));
        out.extend(self.threads.handle(event).into_iter().map(StateDiff::Thread));
        out.extend(self.namespaces.handle(event).into_iter().map(StateDiff::Ns));

        // Messages (capstone store): non-cross-domain mutations via `handle`; a
        // live MESSAGE additionally needs the cross-domain `mentioned` flag, so it's
        // ingested here. Both are subscription-scoped before reaching the frontend.
        let msg_diffs = self.messages.handle(event);
        out.extend(self.scope_msgs(msg_diffs));

        // A live MESSAGE is ingested; **history-batch messages are not** — older
        // history (and search / pins / thread views) is owned by the pull path /
        // the frontend's own backfill, and ingesting it here would pollute the
        // live buffer + falsely bump unread.
        if let ClientEvent::Message { target, body, own, history: false, .. } = event {
            let me = self.messages.me().to_string();
            let mentioned = !*own && self.roles.mentions_me(&me, body, channel_ns(target));

            let diffs = self.messages.ingest(event, mentioned);
            out.extend(self.scope_msgs(diffs));
        }

        out
    }

    /// Apply the two-tier subscription scope to freshly produced message diffs:
    /// `UnreadChanged` always flows (the cheap background push); the body diffs
    /// flow only for channels the frontend declared open.
    fn scope_msgs(&self, diffs: Vec<messages::MsgDiff>) -> Vec<StateDiff> {
        use messages::MsgDiff;

        diffs
            .into_iter()
            .filter(|d| match d {
                MsgDiff::UnreadChanged { .. } => true,
                MsgDiff::MsgAppended { channel, .. }
                | MsgDiff::MsgUpdated { channel, .. }
                | MsgDiff::MsgRemoved { channel, .. } => {
                    self.open.contains(channel) || self.open.contains("*")
                }
            })
            .map(StateDiff::Msg)
            .collect()
    }

    // ---- layout: persistence + the model-side drag-reorder (host-invoked) ----

    /// Restore the persisted channel layout on connect; returns diffs the host
    /// emits so the mirror paints the cached order instantly.
    pub fn seed_layout(&mut self, blob: &str) -> Vec<StateDiff> {
        self.channels.seed(blob).into_iter().map(StateDiff::Chan).collect()
    }

    /// The serialized layout iff it changed since the last call — the host saves
    /// it after a reduce.
    pub fn take_dirty_layout(&mut self) -> Option<String> {
        self.channels.take_dirty()
    }

    /// Drag-reorder a channel (the `move_channel` command): returns the state
    /// diffs to emit (instant UI) and the `CHANNEL META (channel, key, value)`
    /// writes the host must send to the server.
    pub fn move_channel(
        &mut self,
        ns: &str,
        drag: &str,
        target_cat: &str,
        anchor: Option<&str>,
        after: bool,
    ) -> (Vec<StateDiff>, Vec<(String, String, String)>) {
        let r = self.channels.move_channel(ns, drag, target_cat, anchor, after);
        (r.diffs.into_iter().map(StateDiff::Chan).collect(), r.sends)
    }

    /// Drag-reorder a namespace's category list (the `move_category` command):
    /// returns the diff to emit (instant UI) + the `NS META <ns> categories <list>`
    /// write the host must send.
    pub fn move_category(
        &mut self,
        ns: &str,
        drag: &str,
        target: &str,
    ) -> (Vec<StateDiff>, Vec<(String, String, String)>) {
        let r = self.channels.move_category(ns, drag, target);
        (r.diffs.into_iter().map(StateDiff::Chan).collect(), r.sends)
    }

    /// §4 remove a typer whose fallback-expiry timer fired host-side (its `stop`
    /// was lost). Local only — no server write; returns the diff to emit.
    pub fn typing_stop(&mut self, channel: &str, user: &str) -> Vec<StateDiff> {
        self.channels.typing(channel, user, "stop").into_iter().map(StateDiff::Chan).collect()
    }

    /// §6.7 clear a scope's deny list ahead of a `MOD LIST` re-fetch (the
    /// `refreshBans` reset). Local only — the batch response repopulates it.
    pub fn mod_refresh(&mut self, scope: &str) -> Vec<StateDiff> {
        self.moderation.clear(scope).into_iter().map(StateDiff::Mod).collect()
    }

    /// §6.7 clear the report queue ahead of an on-demand re-fetch (the reports
    /// modal's open reset). Local only.
    pub fn reports_clear(&mut self) -> Vec<StateDiff> {
        self.reports.clear().into_iter().map(StateDiff::Report).collect()
    }

    // ---- messages: the two-tier IPC surface (subscription + local echo + pull) ----

    /// Declare the set of **open** channels — the subscription scope. Only these
    /// receive message-body diffs; every other channel gets just `UnreadChanged`.
    /// Replaces the whole set (the host re-sends it whenever the open view changes).
    /// Pass `["*"]` for emit-all (every channel gets body diffs).
    pub fn set_open_channels(&mut self, channels: Vec<String>) {
        self.open = channels.into_iter().collect();
    }

    /// §9 optimistic send: insert a `pending` local echo and return its diff for an
    /// instant render. The server echo (own + matching label) reconciles it to the
    /// server id on ingest; the host still builds + sends the wire `MSG` itself.
    pub fn send_message(&mut self, channel: &str, label: &str, body: &str, md: bool) -> Vec<StateDiff> {
        let (_id, diffs) = self.messages.insert_pending(channel, label, body, md);

        self.scope_msgs(diffs)
    }

    /// The **pull** half of the two-tier IPC: up to `limit` messages in `channel`
    /// ending before `before` (exclusive, else newest). The frontend's window cache
    /// fills from this — bulk bodies never stream over the push path.
    pub fn messages_range(&self, channel: &str, before: Option<&str>, limit: usize) -> Vec<messages::Msg> {
        self.messages.range(channel, before, limit)
    }
}

/// The namespace of a message target for mention-scope resolution: `#<ns>/<chan>`
/// → `<ns>`, else `""` (top-level channel / DM / group → the `*` network scope).
fn channel_ns(target: &str) -> &str {
    target
        .strip_prefix('#')
        .and_then(|rest| rest.split_once('/'))
        .map(|(ns, _)| ns)
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_routes_to_the_owning_domain_and_ignores_the_rest() {
        let mut st = AppState::new();

        // A channels event produces a channels diff.
        let diffs = st.reduce(&ClientEvent::Chanmeta {
            channel: "#n/c".into(),
            key: "topic".into(),
            value: "hi".into(),
        });
        assert!(matches!(diffs.as_slice(), [StateDiff::Chan(_)]));

        // A presence event routes to the presence domain.
        let diffs = st.reduce(&ClientEvent::Presence {
            user: "a".into(),
            status: "online".into(),
        });
        assert!(matches!(diffs.as_slice(), [StateDiff::Presence(_)]));

        // An event no migrated domain owns produces nothing (TS still gets it raw).
        let diffs = st.reduce(&ClientEvent::Closed { reason: "bye".into() });
        assert!(diffs.is_empty());
    }

    fn msg(target: &str, sender: &str, msgid: &str, body: &str, own: bool) -> ClientEvent {
        ClientEvent::Message {
            target: target.into(), sender: sender.into(), network: "home".into(), msgid: msgid.into(),
            body: body.into(), attachments: Vec::new(), system: None, own, history: false, edited: false,
            reply_to: None, thread: None, md: false, label: None,
        }
    }

    #[test]
    fn message_scopes_body_to_open_channels_and_derives_mentions() {
        let mut st = AppState::new();
        st.reduce(&ClientEvent::Connected { network: "home".into(), account: "me".into() });

        // A background (not-open) channel: only the cheap UnreadChanged flows.
        let diffs = st.reduce(&msg("#n/c", "alice", "01a", "hi", false));
        assert!(matches!(diffs.as_slice(),
            [StateDiff::Msg(messages::MsgDiff::UnreadChanged { channel, count, mentions })]
            if channel == "#n/c" && *count == 1 && *mentions == 0));

        // Open it → the next message carries the body diff too, and `@me` mentions.
        st.set_open_channels(vec!["#n/c".into()]);
        let diffs = st.reduce(&msg("#n/c", "alice", "01b", "@me hi", false));
        assert!(diffs.iter().any(|d| matches!(d, StateDiff::Msg(messages::MsgDiff::MsgAppended { .. }))));
        assert!(diffs.iter().any(|d| matches!(d,
            StateDiff::Msg(messages::MsgDiff::UnreadChanged { mentions, .. }) if *mentions == 1)));
    }

    #[test]
    fn send_message_echoes_and_range_reads_the_buffer() {
        let mut st = AppState::new();
        st.reduce(&ClientEvent::Connected { network: "home".into(), account: "me".into() });
        st.set_open_channels(vec!["#n/c".into()]);

        let diffs = st.send_message("#n/c", "L1", "hey", false);
        assert!(matches!(&diffs[0],
            StateDiff::Msg(messages::MsgDiff::MsgAppended { msg, .. }) if msg.pending && msg.own));

        // The pull path sees the echo; a server ack (own + label) reconciles it.
        assert_eq!(st.messages_range("#n/c", None, 50).len(), 1);
        st.reduce(&ClientEvent::Message {
            target: "#n/c".into(), sender: "me".into(), network: "home".into(), msgid: "01srv".into(),
            body: "hey".into(), attachments: Vec::new(), system: None, own: true, history: false,
            edited: false, reply_to: None, thread: None, md: false, label: Some("L1".into()),
        });

        let range = st.messages_range("#n/c", None, 50);
        assert_eq!(range.len(), 1); // reconciled in place, not duplicated
        assert_eq!(range[0].id, "01srv");
    }

    #[test]
    fn history_messages_are_not_ingested() {
        let mut st = AppState::new();
        st.reduce(&ClientEvent::Connected { network: "home".into(), account: "me".into() });
        st.set_open_channels(vec!["*".into()]);

        // A history-batch message (search / pins / thread / backfill) must not
        // enter the live buffer or bump unread — the frontend owns older history.
        let hist = ClientEvent::Message {
            target: "#n/c".into(), sender: "alice".into(), network: "home".into(), msgid: "01a".into(),
            body: "old".into(), attachments: Vec::new(), system: None, own: false, history: true,
            edited: false, reply_to: None, thread: None, md: false, label: None,
        };
        assert!(st.reduce(&hist).is_empty());
        assert!(st.messages_range("#n/c", None, 50).is_empty());
    }

    #[test]
    fn open_star_emits_body_for_every_channel() {
        let mut st = AppState::new();
        st.reduce(&ClientEvent::Connected { network: "home".into(), account: "me".into() });
        st.set_open_channels(vec!["*".into()]); // emit-all

        // A channel never individually opened still gets the body diff.
        let diffs = st.reduce(&msg("#n/other", "alice", "01a", "hi", false));
        assert!(diffs.iter().any(|d| matches!(d, StateDiff::Msg(messages::MsgDiff::MsgAppended { .. }))));
    }
}
