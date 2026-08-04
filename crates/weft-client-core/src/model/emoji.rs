//! Custom-emoji domain (§9.4) — each namespace's `:name:` → media map. The Rust
//! mirror of `serverHandlers`: `EMOJI` sets an entry, `EMOJI-REMOVED` drops it.
//! Namespace-scoped (keyed by ns id). The markdown-cache invalidation
//! (`clearMdCache`, since a `:name:` render can change) stays a TS side-effect.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::ClientEvent;

/// This domain's state diffs — the mirror applies them onto `Server.emoji`
/// (`store.server(ns).emoji`). Incremental (one entry) to match the event
/// granularity; kinds are distinct from the raw `emoji`/`emoji-removed` events.
#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum EmojiDiff {
    EmojiSet {
        namespace: String,
        name: String,
        media: String,
    },
    EmojiDrop {
        namespace: String,
        name: String,
    },
}

/// The emoji sub-model: ns id → (`:name:` → media). Transient (rebuilt from
/// events; a namespace re-announces its emoji when loaded).
#[derive(Default)]
pub struct Emoji {
    map: BTreeMap<String, BTreeMap<String, String>>,
}

impl Emoji {
    pub fn handle(&mut self, event: &ClientEvent) -> Vec<EmojiDiff> {
        match event {
            ClientEvent::Emoji {
                namespace,
                name,
                media,
            } => self.set(namespace, name, media),
            ClientEvent::EmojiRemoved { namespace, name } => self.drop(namespace, name),
            _ => Vec::new(),
        }
    }

    fn set(&mut self, namespace: &str, name: &str, media: &str) -> Vec<EmojiDiff> {
        let entry = self.map.entry(namespace.to_string()).or_default();

        if entry.get(name).map(String::as_str) == Some(media) {
            return Vec::new(); // unchanged (a re-announce) → no diff
        }

        entry.insert(name.to_string(), media.to_string());
        vec![EmojiDiff::EmojiSet {
            namespace: namespace.to_string(),
            name: name.to_string(),
            media: media.to_string(),
        }]
    }

    fn drop(&mut self, namespace: &str, name: &str) -> Vec<EmojiDiff> {
        let Some(entry) = self.map.get_mut(namespace) else {
            return Vec::new();
        };

        if entry.remove(name).is_none() {
            return Vec::new();
        }

        vec![EmojiDiff::EmojiDrop {
            namespace: namespace.to_string(),
            name: name.to_string(),
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emoji(ns: &str, name: &str, media: &str) -> ClientEvent {
        ClientEvent::Emoji {
            namespace: ns.into(),
            name: name.into(),
            media: media.into(),
        }
    }
    fn removed(ns: &str, name: &str) -> ClientEvent {
        ClientEvent::EmojiRemoved {
            namespace: ns.into(),
            name: name.into(),
        }
    }

    #[test]
    fn set_and_drop_emit_incremental_diffs() {
        let mut e = Emoji::default();
        assert!(matches!(&e.handle(&emoji("n", "party", "blob1"))[0],
            EmojiDiff::EmojiSet { namespace, name, media } if namespace == "n" && name == "party" && media == "blob1"));
        // Re-announce with the same media → no diff.
        assert!(e.handle(&emoji("n", "party", "blob1")).is_empty());
        // Changed media → a fresh set.
        assert!(matches!(
            &e.handle(&emoji("n", "party", "blob2"))[0],
            EmojiDiff::EmojiSet { .. }
        ));
        // Drop it → a drop diff; dropping again → no diff.
        assert!(matches!(&e.handle(&removed("n", "party"))[0],
            EmojiDiff::EmojiDrop { namespace, name } if namespace == "n" && name == "party"));
        assert!(e.handle(&removed("n", "party")).is_empty());
    }
}
