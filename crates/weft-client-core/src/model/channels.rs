//! Channels domain — the channel record's scalar metadata **and layout** + the
//! wire-event handler that maintains it, plus the model-side `move_channel`
//! renumber (drag-reorder). The Rust mirror of `sync/channel-handlers.ts` +
//! `channelStore.moveChannel`. Fully self-contained.
//!
//! Owns: `topic`, `restricted` (posting), `view_gated`, `voice`, `vanity`,
//! `category`, `position`. `category`/`position` are the "layout" fields — the
//! model becoming their authority is what the layout+persistence slice is about:
//! the renumber logic lives here now (single source), and persistence rides the
//! `serialize`/`seed` pair (wired by the host). Still excluded: `deleted`/
//! `channel-renamed` (TS instance re-key + nav), unread/typing, roster, messages.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ClientEvent;

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

/// The channels sub-model + its event handler.
#[derive(Default)]
pub struct Channels {
    map: BTreeMap<String, ChannelState>,
    /// Set when a layout field (category/position) changed since the last save.
    dirty: bool,
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
            _ => Vec::new(),
        }
    }

    // §6.3 CHANNEL META — one `key=value` at a time. `deleted` (TS removal + nav)
    // and unknown keys yield no diff and pass through to TS.
    fn chanmeta(&mut self, channel: &str, key: &str, value: &str) -> Vec<ChanDiff> {
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

    // ---- layout persistence (the model's cache; the host does the I/O) ----

    /// Serialize the per-channel layout (category + position) for the host to
    /// persist. Only real (`#`) channels — DMs/groups have no layout.
    pub fn serialize(&self) -> String {
        let layout: BTreeMap<&str, LayoutEntry> = self
            .map
            .iter()
            .filter(|(name, _)| name.starts_with('#'))
            .map(|(name, ch)| (name.as_str(), LayoutEntry { category: ch.category.clone(), position: ch.position }))
            .collect();
        serde_json::to_string(&layout).unwrap_or_default()
    }

    /// Restore the cached layout on connect and emit a diff per channel so the TS
    /// mirror paints the last-known order instantly (before the server re-sends).
    pub fn seed(&mut self, blob: &str) -> Vec<ChanDiff> {
        let layout: BTreeMap<String, LayoutEntry> = serde_json::from_str(blob).unwrap_or_default();
        let mut diffs = Vec::new();
        for (name, entry) in layout {
            let ch = self.map.entry(name.clone()).or_default();
            ch.category = entry.category;
            ch.position = entry.position;
            diffs.push(self.snapshot(&name));
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
    // (name, category, position) for a snapshot diff.
    fn cat_pos(d: &ChanDiff) -> (&str, Option<&str>, i64) {
        let ChanDiff::ChanState { name, category, position, .. } = d;
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
            r.diffs.iter().find_map(|d| {
                let ChanDiff::ChanState { name: n, position, .. } = d;
                (n == name).then_some(*position)
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
            r.diffs.iter().find(|d| matches!(d, ChanDiff::ChanState { name, .. } if name == "#n/a")).unwrap();
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
    fn metadata_still_works_alongside_layout() {
        let mut ch = Channels::default();
        let ChanDiff::ChanState { topic, restricted, view_gated, voice, .. } = {
            ch.handle(&chanmeta("#n/c", "topic", "hi"));
            ch.handle(&chanmeta("#n/c", "posting", "restricted"));
            ch.handle(&chanmeta("#n/c", "view-gated", "true"));
            one(ch.handle(&layout("#n/c", None, 0, "voice", "")))
        };
        assert_eq!(topic.as_deref(), Some("hi"));
        assert!(restricted && view_gated && voice);
    }
}
