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
pub mod moderation;
pub mod presence;
pub mod reports;
pub mod roles;

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
    // future domains: Ns(namespaces::NsDiff), …
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
        // future domains, one line each:
        // out.extend(self.namespaces.handle(event).into_iter().map(StateDiff::Ns));
        out
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
}
