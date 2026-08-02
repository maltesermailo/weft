//! Channels domain — the channel record's scalar metadata **and layout** + the
//! wire-event handler that maintains it, plus the model-side `move_channel`
//! renumber (drag-reorder). The Rust mirror of `sync/channel-handlers.ts` +
//! `channelStore.moveChannel`. Fully self-contained.
//!
//! Owns: `topic`, `restricted` (posting), `view_gated`, `voice`, `vanity`,
//! `category`, `position`. `category`/`position` are the "layout" fields — the
//! model becoming their authority is what the layout+persistence slice is about:
//! the renumber logic lives here now (single source), and persistence rides the
//! `serialize`/`seed` pair (wired by the host). Also owns the channel's identity
//! lifecycle: `channel-renamed` (re-key + persisted-layout re-key) and CHANNEL
//! `deleted` (removal) — the nav / re-subscribe side-effects of both stay in TS —
//! plus the per-namespace category list, the per-channel member roster
//! (`MEMBER join`/`part`), and the typing set (`TYPING`); the roster's
//! cross-domain side-effects (caps / profile / nav / presence) and typing's 6s
//! fallback-expiry timer stay in TS. Still excluded: unread/mention, messages.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::ClientEvent;

/// One channel roster member. `network` is carried raw (not resolved to a
/// local/federated origin) because that resolution needs the session's home
/// network, which the mirror knows — the model stays session-agnostic.
#[derive(Serialize, Clone)]
pub struct RosterMember {
    pub account: String,
    pub network: String,
}

/// A channel's scalar metadata + layout owned by the client core.
#[derive(Default, Clone)]
struct ChannelState {
    // metadata
    voice: bool,
    vanity: String,
    topic: Option<String>,
    restricted: bool,
    view_gated: bool,
    // layout
    category: Option<String>,
    position: i64,
}

/// This domain's state diffs — the TS mirror sets these fields on its `Channel`
/// record. Fields keep snake_case on the wire (variant tag kebab-cased →
/// `kind = "chan-state"`).
#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ChanDiff {
    ChanState {
        name: String,
        voice: bool,
        vanity: String,
        topic: Option<String>,
        restricted: bool,
        view_gated: bool,
        category: Option<String>,
        position: i64,
    },
    /// Re-key the mirror's `Channel` instance `old`→`new` (its unread/mention
    /// tallies ride the instance) and clear the stale vanity. The nav /
    /// re-subscribe side-effects stay in TS.
    ChanRenamed { old: String, new: String },
    /// Drop the mirror's `Channel` instance (CHANNEL DELETE). The nav side-effect
    /// (leaving the deleted view) stays in TS.
    ChanRemoved { name: String },
    /// The namespace's ordered category list — the mirror sets `Server.categories`.
    CatList { ns: String, categories: Vec<String> },
    /// The channel's full member roster — the mirror sets `Channel.members` (it
    /// resolves each member's local/federated origin from its `network`). Sent as
    /// the whole list (idempotent → a reconnect's re-sync replaces cleanly).
    Roster { channel: String, members: Vec<RosterMember> },
    /// The channel's "currently typing" set — the mirror sets `Channel.typers`.
    Typers { channel: String, users: Vec<String> },
}

/// Result of a `move_channel` drag-reorder: the state diffs to apply locally
/// (instant UI) and the `CHANNEL META (channel, key, value)` lines the host must
/// send to the server (the optimistic write — the echo reconciles).
pub struct MoveResult {
    pub diffs: Vec<ChanDiff>,
    pub sends: Vec<(String, String, String)>,
}

/// The persisted per-channel layout (category + position) — the model's cache,
/// stored by the host (localStorage / file) and restored via `seed` on connect
/// for an instant first paint (replaces TS `ensureChannel`'s cache-seeding).
#[derive(Serialize, Deserialize, Default)]
struct LayoutEntry {
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    position: i64,
}

/// The full persisted layout the host stores: per-channel entries + per-namespace
/// category lists. Both fields default, so a partial/older blob still loads (it
/// just seeds less). Supersedes the TS `layoutCache` (`weft:layout`).
#[derive(Serialize, Deserialize, Default)]
struct LayoutBlob {
    #[serde(default)]
    channels: BTreeMap<String, LayoutEntry>,
    #[serde(default)]
    categories: BTreeMap<String, Vec<String>>,
}

/// The channels sub-model + its event handler.
#[derive(Default)]
pub struct Channels {
    map: BTreeMap<String, ChannelState>,
    /// §6.3 per-namespace ordered category list (Discord-style headers). Server-
    /// authoritative (from `NS-META categories`); the model owns the local copy +
    /// the drag-reorder, mirroring `move_channel` for channels.
    categories: BTreeMap<String, Vec<String>>,
    /// §6.3 per-channel member roster (from `MEMBER join`/`part` — incl. the
    /// MEMBERS batch). Transient (not persisted): rebuilt from events each session.
    roster: BTreeMap<String, Vec<RosterMember>>,
    /// §4 per-channel "currently typing" set. Transient. The server never echoes a
    /// user's own typing, so "me" is never here (no session knowledge needed). The
    /// 6s fallback-expiry timer lives host-side (a timer isn't pure-model); it fires
    /// the `typing_stop` command to remove a typer whose `stop` was lost.
    typers: BTreeMap<String, Vec<String>>,
    /// Set when a layout field (category/position/category-list) changed since the
    /// last save.
    dirty: bool,
    /// Channels restored from the layout cache on connect that the server hasn't
    /// re-confirmed with a live event yet. Pruned at SYNC end (§reconciliation) so
    /// a channel deleted or left while we were offline never lingers as a ghost.
    provisional: BTreeSet<String>,
}

/// `#<ns>/<chan>` → `<ns>`, else `""` (top-level / DM / group).
fn ns_of(name: &str) -> &str {
    name.strip_prefix('#')
        .and_then(|rest| rest.split_once('/'))
        .map(|(ns, _)| ns)
        .unwrap_or("")
}

impl Channels {
    /// Handle the channel events this domain owns; return the resulting diffs.
    /// Mirrors `channel-handlers.ts`.
    pub fn handle(&mut self, event: &ClientEvent) -> Vec<ChanDiff> {
        match event {
            ClientEvent::Chanmeta { channel, key, value } => self.chanmeta(channel, key, value),
            ClientEvent::ChannelLayout { channel, category, position, channel_kind, vanity } => {
                vec![self.layout(channel, category.clone(), *position, channel_kind, vanity)]
            }
            ClientEvent::ChannelRenamed { old, new } => self.renamed(old, new),
            // §6.3 channel roster: MEMBER join/part (incl. the MEMBERS batch).
            ClientEvent::Member { channel, user, network, action, .. } => {
                self.member(channel, user, network, action)
            }
            // §4 typing set (the host owns the fallback-expiry timer).
            ClientEvent::Typing { channel, user, state } => self.typing(channel, user, state),
            // §6.3 the namespace's category list rides NS-META (server-authoritative);
            // adopt it (the other NS-META fields stay TS-owned via the raw event).
            ClientEvent::NsMeta { id, categories, .. } => self.set_categories(id, categories.clone()),
            // SYNC end = the server has finished enumerating the visible channels
            // (a CHANNEL-LAYOUT per one). Any cache-seeded channel still provisional
            // is gone server-side → prune it.
            ClientEvent::SyncEnd { .. } => self.reconcile_seed(),
            _ => Vec::new(),
        }
    }

    // §6.3 maintain a channel's roster. Join adds (deduped by account); part
    // removes. Emits the full list so the mirror just sets `Channel.members`. A
    // no-op (no diff) when nothing changed — a duplicate join (MEMBERS re-fetch)
    // or a part of someone absent. The channel's own removal (self-part → leave)
    // is a TS side-effect that needs the session's "me"; the model just drops the
    // member here and lets the (instance-gone) roster diff no-op in the mirror.
    fn member(&mut self, channel: &str, user: &str, network: &str, action: &str) -> Vec<ChanDiff> {
        let members = self.roster.entry(channel.to_string()).or_default();

        let changed = if action == "join" {
            if members.iter().any(|m| m.account == user) {
                false
            } else {
                members.push(RosterMember { account: user.to_string(), network: network.to_string() });
                true
            }
        } else {
            let before = members.len();
            members.retain(|m| m.account != user);
            members.len() != before
        };

        if !changed {
            return Vec::new();
        }

        vec![ChanDiff::Roster { channel: channel.to_string(), members: members.clone() }]
    }

    // §4 maintain a channel's typing set. `start` adds (deduped), anything else
    // removes. No-op (no diff) when unchanged. Also the target of the host's
    // `typing_stop` expiry command. Never holds "me" — self-typing isn't echoed.
    pub(super) fn typing(&mut self, channel: &str, user: &str, state: &str) -> Vec<ChanDiff> {
        let users = self.typers.entry(channel.to_string()).or_default();

        let changed = if state == "start" {
            if users.iter().any(|u| u == user) {
                false
            } else {
                users.push(user.to_string());
                true
            }
        } else {
            let before = users.len();
            users.retain(|u| u != user);
            users.len() != before
        };

        if !changed {
            return Vec::new();
        }

        vec![ChanDiff::Typers { channel: channel.to_string(), users: users.clone() }]
    }

    // §6.3 adopt the server-authoritative category list for a namespace. A no-op
    // (no diff, no dirty) when unchanged — NS-META fires for every ns update, most
    // of which don't touch categories.
    fn set_categories(&mut self, ns: &str, categories: Vec<String>) -> Vec<ChanDiff> {
        if self.categories.get(ns).map(Vec::as_slice) == Some(categories.as_slice()) {
            return Vec::new();
        }

        self.categories.insert(ns.to_string(), categories.clone());
        self.dirty = true;

        vec![ChanDiff::CatList { ns: ns.to_string(), categories }]
    }

    /// Drag-reorder a namespace's category list: move `drag` to `target`'s slot
    /// (dropping on the implicit top group → the end). Returns the diff (instant UI
    /// via the mirror) + the `NS META <ns> categories <list>` write. Mirrors the TS
    /// `moveCategory`.
    pub fn move_category(&mut self, ns: &str, drag: &str, target: &str) -> MoveResult {
        let empty = MoveResult { diffs: Vec::new(), sends: Vec::new() };

        if drag == target || drag.is_empty() {
            return empty;
        }

        let mut cats = self.categories.get(ns).cloned().unwrap_or_default();

        let Some(from) = cats.iter().position(|c| c == drag) else {
            return empty;
        };

        cats.remove(from);
        let to = cats.iter().position(|c| c == target).unwrap_or(cats.len());
        cats.insert(to, drag.to_string());

        self.categories.insert(ns.to_string(), cats.clone());
        self.dirty = true;

        MoveResult {
            diffs: vec![ChanDiff::CatList { ns: ns.to_string(), categories: cats.clone() }],
            sends: vec![(ns.to_string(), "categories".into(), cats.join(","))],
        }
    }

    // §6.3 CHANNEL META — one `key=value` at a time. `deleted` removes the channel
    // (the nav side-effect stays in TS); unknown keys yield no diff and pass
    // through to TS.
    fn chanmeta(&mut self, channel: &str, key: &str, value: &str) -> Vec<ChanDiff> {
        if key == "deleted" {
            return self.deleted(channel);
        }

        self.provisional.remove(channel); // a live metadata event confirms it exists
        let ch = self.map.entry(channel.to_string()).or_default();

        match key {
            "topic" => ch.topic = Some(value.to_string()),
            "posting" => ch.restricted = value == "restricted",
            "view-gated" => ch.view_gated = value == "true",
            "category" => ch.category = if value.is_empty() { None } else { Some(value.to_string()) },
            "position" => ch.position = value.parse().unwrap_or(0), // parse failure → 0
            _ => return Vec::new(),
        }

        if key == "category" || key == "position" {
            self.dirty = true; // layout changed → the host re-persists
        }

        vec![self.snapshot(channel)]
    }

    // §7 CHANNEL-LAYOUT — the full layout tuple.
    fn layout(
        &mut self,
        channel: &str,
        category: Option<String>,
        position: i64,
        channel_kind: &str,
        vanity: &str,
    ) -> ChanDiff {
        self.provisional.remove(channel); // the server's live CHANNEL-LAYOUT confirms it
        let ch = self.map.entry(channel.to_string()).or_default();

        ch.category = category;
        ch.position = position;
        ch.voice = channel_kind == "voice";

        if !vanity.is_empty() {
            ch.vanity = vanity.to_string(); // empty must NOT clear an existing vanity
        }

        self.dirty = true; // category/position (layout) changed
        self.snapshot(channel)
    }

    // §6.3 CHANNEL-RENAMED — re-key the channel's state `old`→`new`. The server
    // sends no live layout on rename, so clear the stale vanity (the display falls
    // back to the new wire name's slug until a later layout sets it). Idempotent:
    // the event arrives as a broadcast plus a labeled copy to the initiator, so a
    // second call (with `old` already gone) is a harmless re-emit.
    fn renamed(&mut self, old: &str, new: &str) -> Vec<ChanDiff> {
        if let Some(mut state) = self.map.remove(old) {
            state.vanity.clear();
            self.map.insert(new.to_string(), state);
            self.dirty = true; // the persisted layout is name-keyed → re-key + re-save
        }

        if let Some(members) = self.roster.remove(old) {
            self.roster.insert(new.to_string(), members); // roster follows the re-key
        }

        if let Some(users) = self.typers.remove(old) {
            self.typers.insert(new.to_string(), users);
        }

        // A rename is a live event → both names are confirmed (never provisional).
        self.provisional.remove(old);
        self.provisional.remove(new);

        vec![ChanDiff::ChanRenamed { old: old.to_string(), new: new.to_string() }]
    }

    // §6.3 CHANNEL DELETE — drop the channel from the model. Always tells the
    // mirror to remove its instance (which may exist even when the model has no
    // metadata for it — e.g. a channel with only messages).
    fn deleted(&mut self, channel: &str) -> Vec<ChanDiff> {
        self.provisional.remove(channel);
        self.roster.remove(channel);
        self.typers.remove(channel);

        if self.map.remove(channel).is_some() {
            self.dirty = true; // the persisted layout set shrank → re-save
        }

        vec![ChanDiff::ChanRemoved { name: channel.to_string() }]
    }

    // SYNC-end reconciliation: every channel still `provisional` was cache-seeded
    // but got no live event during the sync → it's gone (deleted / left / hidden
    // while we were offline). Drop it from the model + tell the mirror to remove
    // its instance, so the instant-paint cache can never strand a ghost.
    fn reconcile_seed(&mut self) -> Vec<ChanDiff> {
        let stale: Vec<String> = std::mem::take(&mut self.provisional).into_iter().collect();

        if !stale.is_empty() {
            self.dirty = true; // the persisted layout set shrank → re-save cleaned
        }

        stale
            .into_iter()
            .map(|name| {
                self.map.remove(&name);
                ChanDiff::ChanRemoved { name }
            })
            .collect()
    }

    // ---- layout persistence (the model's cache; the host does the I/O) ----

    /// Serialize the per-channel layout (category + position) for the host to
    /// persist. Only **namespaced** channels (`#<ns>/<chan>`) — DMs/groups and
    /// top-level channels have no category/position layout (and top-level channels
    /// get no CHANNEL-LAYOUT to reconcile against, so seeding them would ghost).
    pub fn serialize(&self) -> String {
        let channels = self
            .map
            .iter()
            .filter(|(name, _)| !ns_of(name).is_empty())
            .map(|(name, ch)| (name.clone(), LayoutEntry { category: ch.category.clone(), position: ch.position }))
            .collect();
        let blob = LayoutBlob { channels, categories: self.categories.clone() };

        serde_json::to_string(&blob).unwrap_or_default()
    }

    /// Restore the cached layout on connect and emit a diff per channel so the TS
    /// mirror paints the last-known order instantly (before the server re-sends).
    /// Each seeded channel is **provisional** — a live event confirms it, else the
    /// SYNC-end reconcile prunes it (so a stale cache can't paint a ghost).
    pub fn seed(&mut self, blob: &str) -> Vec<ChanDiff> {
        let blob: LayoutBlob = serde_json::from_str(blob).unwrap_or_default();
        let mut diffs = Vec::new();

        for (name, entry) in blob.channels {
            if ns_of(&name).is_empty() {
                continue; // defensive: an old cache may hold non-namespaced entries
            }

            let ch = self.map.entry(name.clone()).or_default();
            ch.category = entry.category;
            ch.position = entry.position;

            self.provisional.insert(name.clone());
            diffs.push(self.snapshot(&name));
        }

        // Category lists paint instantly too; NS-META overwrites the whole list (a
        // stale one is simply replaced), so these need no provisional reconcile.
        for (ns, categories) in blob.categories {
            self.categories.insert(ns.clone(), categories.clone());
            diffs.push(ChanDiff::CatList { ns, categories });
        }

        diffs
    }

    /// The serialized layout iff it changed since the last call (clears the flag).
    /// The host saves it after a reduce; `None` means nothing to persist.
    pub fn take_dirty(&mut self) -> Option<String> {
        if self.dirty {
            self.dirty = false;
            Some(self.serialize())
        } else {
            None
        }
    }

    /// Discord-style drag-reorder (was `channelStore.moveChannel`, now model-side):
    /// move `drag` into `target_cat` at `anchor` (before/after), then renumber that
    /// category so positions stay stable + ordered. Returns the diffs to apply and
    /// the `CHANNEL META` writes the host must send. `""` `target_cat` = the bare
    /// (uncategorized) top-level group.
    pub fn move_channel(
        &mut self,
        ns: &str,
        drag: &str,
        target_cat: &str,
        anchor: Option<&str>,
        after: bool,
    ) -> MoveResult {
        if !self.map.contains_key(drag) {
            return MoveResult { diffs: Vec::new(), sends: Vec::new() };
        }

        let mut sends = Vec::new();
        let mut changed = vec![drag.to_string()]; // drag always re-snapshotted

        // Set the dragged channel's category (optimistic) + queue the write.
        self.map.get_mut(drag).unwrap().category =
            if target_cat.is_empty() { None } else { Some(target_cat.to_string()) };
        sends.push((drag.to_string(), "category".into(), target_cat.to_string()));
        self.dirty = true; // layout changed → re-persist

        // The ordered list of the target category's channels (excluding drag).
        let mut list: Vec<String> = self
            .map
            .iter()
            .filter(|(name, ch)| {
                name.starts_with('#')
                    && ns_of(name) == ns
                    && ch.category.as_deref().unwrap_or("") == target_cat
                    && name.as_str() != drag
            })
            .map(|(name, _)| name.clone())
            .collect();
        list.sort_by(|a, b| self.map[a].position.cmp(&self.map[b].position).then_with(|| a.cmp(b)));

        // Insert drag at the anchor (default: end; `after` → past the anchor).
        let mut at = anchor
            .and_then(|a| list.iter().position(|n| n == a))
            .map(|i| i as i64)
            .unwrap_or(-1);

        if at < 0 {
            at = list.len() as i64;
        } else if after {
            at += 1;
        }

        list.insert(at as usize, drag.to_string());

        // Renumber 0..n; emit a write + diff only for channels whose position changed.
        for (i, name) in list.iter().enumerate() {
            let i = i as i64;

            if self.map[name].position != i {
                self.map.get_mut(name).unwrap().position = i;
                sends.push((name.clone(), "position".into(), i.to_string()));

                if name != drag {
                    changed.push(name.clone());
                }
            }
        }

        MoveResult { diffs: changed.iter().map(|n| self.snapshot(n)).collect(), sends }
    }

    fn snapshot(&self, channel: &str) -> ChanDiff {
        let ch = self.map.get(channel).cloned().unwrap_or_default();
        ChanDiff::ChanState {
            name: channel.to_string(),
            voice: ch.voice,
            vanity: ch.vanity,
            topic: ch.topic,
            restricted: ch.restricted,
            view_gated: ch.view_gated,
            category: ch.category,
            position: ch.position,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chanmeta(channel: &str, key: &str, value: &str) -> ClientEvent {
        ClientEvent::Chanmeta { channel: channel.into(), key: key.into(), value: value.into() }
    }
    fn layout(channel: &str, category: Option<&str>, position: i64, kind: &str, vanity: &str) -> ClientEvent {
        ClientEvent::ChannelLayout {
            channel: channel.into(),
            category: category.map(Into::into),
            position,
            channel_kind: kind.into(),
            vanity: vanity.into(),
        }
    }
    fn one(diffs: Vec<ChanDiff>) -> ChanDiff {
        assert_eq!(diffs.len(), 1);
        diffs.into_iter().next().unwrap()
    }
    fn renamed(old: &str, new: &str) -> ClientEvent {
        ClientEvent::ChannelRenamed { old: old.into(), new: new.into() }
    }
    fn sync_end() -> ClientEvent {
        ClientEvent::SyncEnd { cursor: "c1".into() }
    }
    fn removed_names(diffs: &[ChanDiff]) -> Vec<&str> {
        diffs.iter().filter_map(|d| match d {
            ChanDiff::ChanRemoved { name } => Some(name.as_str()),
            _ => None,
        }).collect()
    }
    // (name, category, position) for a snapshot diff.
    fn cat_pos(d: &ChanDiff) -> (&str, Option<&str>, i64) {
        let ChanDiff::ChanState { name, category, position, .. } = d else {
            panic!("expected a ChanState diff");
        };
        (name, category.as_deref(), *position)
    }

    #[test]
    fn chanmeta_and_layout_set_category_and_position() {
        let mut ch = Channels::default();
        assert_eq!(cat_pos(&one(ch.handle(&chanmeta("#n/c", "category", "Text")))), ("#n/c", Some("Text"), 0));
        assert_eq!(cat_pos(&one(ch.handle(&chanmeta("#n/c", "position", "4")))), ("#n/c", Some("Text"), 4));
        // empty category clears to None; layout overwrites both.
        assert_eq!(cat_pos(&one(ch.handle(&chanmeta("#n/c", "category", "")))), ("#n/c", None, 4));
        assert_eq!(cat_pos(&one(ch.handle(&layout("#n/c", Some("Voice"), 2, "voice", "")))), ("#n/c", Some("Voice"), 2));
    }

    #[test]
    fn move_within_category_renumbers_and_reports_sends() {
        let mut ch = Channels::default();
        // Three channels in category "General" at positions 0,1,2.
        for (c, p) in [("#n/a", 0), ("#n/b", 1), ("#n/c", 2)] {
            ch.handle(&layout(c, Some("General"), p, "text", ""));
        }
        // Drag `c` to the front (before `a`).
        let r = ch.move_channel("n", "#n/c", "General", Some("#n/a"), false);
        // New order a,b get bumped to 1,2; c → 0.
        let pos = |name: &str| {
            r.diffs.iter().find_map(|d| match d {
                ChanDiff::ChanState { name: n, position, .. } if n == name => Some(*position),
                _ => None,
            })
        };
        assert_eq!(pos("#n/c"), Some(0));
        assert_eq!(pos("#n/a"), Some(1));
        assert_eq!(pos("#n/b"), Some(2));
        // Sends: c category (unchanged "General") + the three position writes.
        assert!(r.sends.contains(&("#n/c".into(), "category".into(), "General".into())));
        assert!(r.sends.contains(&("#n/c".into(), "position".into(), "0".into())));
        assert!(r.sends.contains(&("#n/a".into(), "position".into(), "1".into())));
        assert!(r.sends.contains(&("#n/b".into(), "position".into(), "2".into())));
    }

    #[test]
    fn move_across_categories_sets_category_and_appends() {
        let mut ch = Channels::default();
        ch.handle(&layout("#n/a", Some("A"), 0, "text", ""));
        ch.handle(&layout("#n/b", Some("B"), 0, "text", ""));
        // Move `a` into category B (no anchor → append at end).
        let r = ch.move_channel("n", "#n/a", "B", None, false);
        let ChanDiff::ChanState { category, position, .. } =
            r.diffs.iter().find(|d| matches!(d, ChanDiff::ChanState { name, .. } if name == "#n/a")).unwrap()
        else {
            panic!("expected a ChanState diff");
        };
        assert_eq!(category.as_deref(), Some("B"));
        assert_eq!(*position, 1); // after b (which is at 0)
        assert!(r.sends.contains(&("#n/a".into(), "category".into(), "B".into())));
    }

    #[test]
    fn move_only_reports_channels_whose_position_actually_changed() {
        let mut ch = Channels::default();
        ch.handle(&layout("#n/a", Some("G"), 0, "text", ""));
        ch.handle(&layout("#n/b", Some("G"), 1, "text", ""));
        // "Move" b after a — it's already there; positions don't change.
        let r = ch.move_channel("n", "#n/b", "G", Some("#n/a"), true);
        // Only b's category write is guaranteed; no position write for a (unchanged).
        assert!(!r.sends.iter().any(|(c, k, _)| c == "#n/a" && k == "position"));
    }

    #[test]
    fn move_ignores_channels_in_other_namespaces() {
        let mut ch = Channels::default();
        ch.handle(&layout("#n/a", Some("G"), 0, "text", ""));
        ch.handle(&layout("#other/x", Some("G"), 0, "text", "")); // same cat name, different ns
        let r = ch.move_channel("n", "#n/a", "G", None, false);
        // The other-ns channel must not be renumbered/sent.
        assert!(!r.sends.iter().any(|(c, _, _)| c == "#other/x"));
    }

    #[test]
    fn unknown_drag_is_a_noop() {
        let mut ch = Channels::default();
        let r = ch.move_channel("n", "#n/missing", "G", None, false);
        assert!(r.diffs.is_empty() && r.sends.is_empty());
    }

    #[test]
    fn serialize_seed_roundtrip() {
        let mut ch = Channels::default();
        ch.handle(&layout("#n/a", Some("G"), 2, "text", ""));
        ch.handle(&layout("#n/b", None, 0, "voice", ""));
        let blob = ch.serialize();

        let mut fresh = Channels::default();
        let diffs = fresh.seed(&blob);
        assert_eq!(diffs.len(), 2); // one diff per channel → mirror paints instantly
        assert_eq!(cat_pos(&fresh.snapshot("#n/a")), ("#n/a", Some("G"), 2));
        assert_eq!(cat_pos(&fresh.snapshot("#n/b")), ("#n/b", None, 0));
        assert!(fresh.take_dirty().is_none()); // a restore is not a change
    }

    #[test]
    fn only_layout_changes_dirty_the_cache() {
        let mut ch = Channels::default();
        ch.handle(&chanmeta("#n/c", "topic", "hi"));
        assert!(ch.take_dirty().is_none()); // topic isn't layout
        ch.handle(&chanmeta("#n/c", "position", "1"));
        assert!(ch.take_dirty().is_some()); // position is layout
        assert!(ch.take_dirty().is_none()); // cleared
    }

    #[test]
    fn serialize_excludes_dms_and_groups() {
        let mut ch = Channels::default();
        ch.handle(&chanmeta("#n/c", "category", "G"));
        ch.map.entry("@dm".into()).or_default().position = 5; // not a real channel
        let blob = ch.serialize();
        assert!(blob.contains("#n/c"));
        assert!(!blob.contains("@dm"));
    }

    #[test]
    fn rename_rekeys_state_clears_vanity_and_reports_diff() {
        let mut ch = Channels::default();
        ch.handle(&layout("#n/old", Some("G"), 3, "voice", "General"));
        let diff = one(ch.handle(&renamed("#n/old", "#n/new")));
        assert!(matches!(&diff, ChanDiff::ChanRenamed { old, new } if old == "#n/old" && new == "#n/new"));

        // The state moved to the new key, keeping layout/kind but dropping vanity.
        let ChanDiff::ChanState { vanity, category, position, voice, .. } = ch.snapshot("#n/new") else {
            panic!("expected a ChanState diff");
        };
        assert_eq!(vanity, ""); // stale vanity cleared → display falls back to the slug
        assert_eq!(category.as_deref(), Some("G"));
        assert_eq!(position, 3);
        assert!(voice);
        assert!(!ch.map.contains_key("#n/old")); // old key gone

        // Persistence follows the re-key: the blob now carries #n/new, not #n/old.
        assert!(ch.take_dirty().is_some());
        let blob = ch.serialize();
        assert!(blob.contains("#n/new") && !blob.contains("#n/old"));
    }

    #[test]
    fn rename_is_idempotent() {
        let mut ch = Channels::default();
        ch.handle(&layout("#n/old", Some("G"), 0, "text", ""));
        ch.handle(&renamed("#n/old", "#n/new"));
        // The labeled-copy re-emit: `old` is already gone — still a harmless diff.
        let diff = one(ch.handle(&renamed("#n/old", "#n/new")));
        assert!(matches!(diff, ChanDiff::ChanRenamed { .. }));
        assert!(ch.map.contains_key("#n/new"));
    }

    #[test]
    fn delete_removes_state_and_reports_removed() {
        let mut ch = Channels::default();
        ch.handle(&layout("#n/c", Some("G"), 1, "text", ""));
        let diff = one(ch.handle(&chanmeta("#n/c", "deleted", "")));
        assert!(matches!(&diff, ChanDiff::ChanRemoved { name } if name == "#n/c"));
        assert!(!ch.map.contains_key("#n/c"));
        assert!(ch.take_dirty().is_some()); // layout set shrank → re-persist
    }

    #[test]
    fn delete_of_untracked_channel_still_reports_removed() {
        let mut ch = Channels::default();
        // Never had metadata for it (e.g. a messages-only channel) — the mirror
        // still needs to drop its instance, and this must not create an entry.
        let diff = one(ch.handle(&chanmeta("#n/ghost", "deleted", "")));
        assert!(matches!(diff, ChanDiff::ChanRemoved { .. }));
        assert!(!ch.map.contains_key("#n/ghost"));
    }

    #[test]
    fn syncend_prunes_unconfirmed_seed_but_keeps_confirmed() {
        let mut ch = Channels::default();
        // Cache from a previous session: two namespaced channels.
        let blob = {
            let mut src = Channels::default();
            src.handle(&layout("#n/keep", Some("G"), 0, "text", ""));
            src.handle(&layout("#n/gone", Some("G"), 1, "text", ""));
            src.serialize()
        };
        let seeded = ch.seed(&blob);
        assert_eq!(seeded.len(), 2); // instant paint for both

        // The server re-confirms only `keep` (a live CHANNEL-LAYOUT); `gone` was
        // deleted while we were offline, so no event arrives for it.
        ch.handle(&layout("#n/keep", Some("G"), 0, "text", "keep"));

        // SYNC end reconciles: `gone` is pruned, `keep` survives.
        let pruned = ch.handle(&sync_end());
        assert_eq!(removed_names(&pruned), vec!["#n/gone"]);
        assert!(ch.map.contains_key("#n/keep"));
        assert!(!ch.map.contains_key("#n/gone"));
        // The cleaned layout no longer carries the ghost.
        assert!(ch.take_dirty().is_some());
        assert!(!ch.serialize().contains("#n/gone"));
    }

    #[test]
    fn seed_skips_non_namespaced_and_syncend_is_noop_when_all_confirmed() {
        let mut ch = Channels::default();
        // A stale cache holding a top-level channel (no ns) — not seeded (it gets
        // no CHANNEL-LAYOUT to reconcile against, so seeding would strand it).
        let diffs = ch.seed(r##"{"#top":{"category":null,"position":0}}"##);
        assert!(diffs.is_empty());
        assert!(!ch.map.contains_key("#top"));
        // Nothing provisional → SYNC end prunes nothing.
        assert!(ch.handle(&sync_end()).is_empty());
    }

    fn ns_meta(id: &str, categories: &[&str]) -> ClientEvent {
        ClientEvent::NsMeta {
            id: id.into(),
            name: id.into(),
            visibility: "public".into(),
            owner: None,
            title: None,
            description: None,
            recovery_set: false,
            recovery_eta: None,
            recovery_rung: None,
            categories: categories.iter().map(|s| s.to_string()).collect(),
            federation: false,
        }
    }
    fn cat_list(d: &ChanDiff) -> Option<(&str, Vec<&str>)> {
        match d {
            ChanDiff::CatList { ns, categories } => {
                Some((ns, categories.iter().map(String::as_str).collect()))
            }
            _ => None,
        }
    }

    #[test]
    fn ns_meta_adopts_categories_and_is_a_noop_when_unchanged() {
        let mut ch = Channels::default();
        let d = one(ch.handle(&ns_meta("n", &["Text", "Voice"])));
        assert_eq!(cat_list(&d), Some(("n", vec!["Text", "Voice"])));
        // Same list again (an unrelated NS-META update) → no diff, no dirty.
        assert!(ch.take_dirty().is_some()); // the first adoption dirtied
        assert!(ch.handle(&ns_meta("n", &["Text", "Voice"])).is_empty());
        assert!(ch.take_dirty().is_none());
    }

    #[test]
    fn move_category_reorders_and_reports_the_ns_meta_write() {
        let mut ch = Channels::default();
        ch.handle(&ns_meta("n", &["A", "B", "C"]));
        // Drag C before B.
        let r = ch.move_category("n", "C", "B");
        assert_eq!(cat_list(&r.diffs[0]), Some(("n", vec!["A", "C", "B"])));
        assert_eq!(r.sends, vec![("n".into(), "categories".into(), "A,C,B".into())]);
    }

    #[test]
    fn move_category_drop_on_implicit_group_goes_to_end_and_unknown_is_noop() {
        let mut ch = Channels::default();
        ch.handle(&ns_meta("n", &["A", "B", "C"]));
        // Drop A on the bare top group ("" target) → append at end.
        let r = ch.move_category("n", "A", "");
        assert_eq!(cat_list(&r.diffs[0]), Some(("n", vec!["B", "C", "A"])));
        // Dragging the bare group itself, or an unknown category, is a no-op.
        assert!(ch.move_category("n", "", "A").diffs.is_empty());
        assert!(ch.move_category("n", "Ghost", "A").diffs.is_empty());
    }

    #[test]
    fn categories_persist_and_seed() {
        let mut ch = Channels::default();
        ch.handle(&layout("#n/a", Some("A"), 0, "text", ""));
        ch.handle(&ns_meta("n", &["A", "B"]));
        let blob = ch.serialize();

        let mut fresh = Channels::default();
        let diffs = fresh.seed(&blob);
        // One channel snapshot + one category list restored for instant paint.
        assert!(diffs.iter().any(|d| cat_list(d) == Some(("n", vec!["A", "B"]))));
        // A matching NS-META afterward is a no-op (already seeded).
        assert!(fresh.handle(&ns_meta("n", &["A", "B"])).is_empty());
    }

    fn member(channel: &str, user: &str, network: &str, action: &str) -> ClientEvent {
        ClientEvent::Member {
            channel: channel.into(),
            user: user.into(),
            network: network.into(),
            action: action.into(),
            count: None,
        }
    }
    // (account, network) pairs of a roster diff.
    fn roster_of(d: &ChanDiff) -> Option<Vec<(&str, &str)>> {
        match d {
            ChanDiff::Roster { members, .. } => {
                Some(members.iter().map(|m| (m.account.as_str(), m.network.as_str())).collect())
            }
            _ => None,
        }
    }

    #[test]
    fn member_join_and_part_maintain_the_roster() {
        let mut ch = Channels::default();
        assert_eq!(roster_of(&one(ch.handle(&member("#n/c", "alice", "home", "join")))),
                   Some(vec![("alice", "home")]));
        assert_eq!(roster_of(&one(ch.handle(&member("#n/c", "bob", "peer", "join")))),
                   Some(vec![("alice", "home"), ("bob", "peer")]));
        // A part removes just that member.
        assert_eq!(roster_of(&one(ch.handle(&member("#n/c", "alice", "home", "part")))),
                   Some(vec![("bob", "peer")]));
    }

    #[test]
    fn duplicate_join_and_absent_part_emit_no_diff() {
        let mut ch = Channels::default();
        ch.handle(&member("#n/c", "alice", "home", "join"));
        // MEMBERS re-fetch re-announces alice → deduped, no diff.
        assert!(ch.handle(&member("#n/c", "alice", "home", "join")).is_empty());
        // Parting someone who isn't here → no diff.
        assert!(ch.handle(&member("#n/c", "ghost", "home", "part")).is_empty());
    }

    #[test]
    fn roster_follows_rename_and_clears_on_delete() {
        let mut ch = Channels::default();
        ch.handle(&member("#n/old", "alice", "home", "join"));
        ch.handle(&renamed("#n/old", "#n/new"));
        // A part under the new name works → the roster moved with the re-key.
        assert_eq!(roster_of(&one(ch.handle(&member("#n/new", "alice", "home", "part")))), Some(vec![]));

        ch.handle(&member("#n/x", "bob", "home", "join"));
        ch.handle(&chanmeta("#n/x", "deleted", ""));
        // After delete, a re-join starts a fresh roster (old entry was dropped).
        assert_eq!(roster_of(&one(ch.handle(&member("#n/x", "carol", "home", "join")))),
                   Some(vec![("carol", "home")]));
    }

    fn typing(channel: &str, user: &str, state: &str) -> ClientEvent {
        ClientEvent::Typing { channel: channel.into(), user: user.into(), state: state.into() }
    }
    fn typers_of(d: &ChanDiff) -> Option<Vec<&str>> {
        match d {
            ChanDiff::Typers { users, .. } => Some(users.iter().map(String::as_str).collect()),
            _ => None,
        }
    }

    #[test]
    fn typing_start_and_stop_maintain_the_set() {
        let mut ch = Channels::default();
        assert_eq!(typers_of(&one(ch.handle(&typing("#n/c", "alice", "start")))), Some(vec!["alice"]));
        assert_eq!(typers_of(&one(ch.handle(&typing("#n/c", "bob", "start")))), Some(vec!["alice", "bob"]));
        // A duplicate start (re-sent) is a no-op; stop removes.
        assert!(ch.handle(&typing("#n/c", "alice", "start")).is_empty());
        assert_eq!(typers_of(&one(ch.handle(&typing("#n/c", "alice", "stop")))), Some(vec!["bob"]));
        // Stopping someone absent is a no-op.
        assert!(ch.handle(&typing("#n/c", "alice", "stop")).is_empty());
    }

    #[test]
    fn typing_stop_command_removes_the_typer() {
        let mut st = crate::model::AppState::new();
        st.reduce(&typing("#n/c", "alice", "start"));
        // The host's expiry timer fires → the command removes alice locally.
        let diffs = st.typing_stop("#n/c", "alice");
        assert_eq!(diffs.len(), 1);
        // Alice already gone → the command is a no-op.
        assert!(st.typing_stop("#n/c", "alice").is_empty());
    }

    #[test]
    fn metadata_still_works_alongside_layout() {
        let mut ch = Channels::default();
        let ChanDiff::ChanState { topic, restricted, view_gated, voice, .. } = ({
            ch.handle(&chanmeta("#n/c", "topic", "hi"));
            ch.handle(&chanmeta("#n/c", "posting", "restricted"));
            ch.handle(&chanmeta("#n/c", "view-gated", "true"));
            one(ch.handle(&layout("#n/c", None, 0, "voice", "")))
        }) else {
            panic!("expected a ChanState diff");
        };
        assert_eq!(topic.as_deref(), Some("hi"));
        assert!(restricted && view_gated && voice);
    }
}
