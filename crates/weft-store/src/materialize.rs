//! §12.1 materialization: event rows → the compacted wire form.
//!
//! One principle, one function: batches carry **one MESSAGE per surviving
//! message** (final body, `edited=` count, `edited-at=`), per-emoji
//! `REACTIONS` summaries, and `DELETED` tombstones. Never `EDITED` chains,
//! never reaction ping-pong (security invariant 10). Both storage backends
//! share this — it is deliberately pure and heavily unit-tested here
//! rather than reimplemented per backend.

use std::collections::HashMap;

use weft_proto::{MsgId, MsgMeta, Ulid, UserRef};

use crate::types::{EventKind, EventRecord};

/// §7 REACTIONS: `by=` lists at most this many actors; `count` stays
/// authoritative.
pub const MAX_REACTION_ACTORS: usize = 20;

/// Per-emoji reaction summary for one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionSummary {
    pub emoji: String,
    pub count: u64,
    /// First `MAX_REACTION_ACTORS` actors, in reaction order.
    pub actors: Vec<UserRef>,
}

/// One entry of a HISTORY batch. Variant sizes are lopsided by nature
/// (a full message vs a tombstone); items only live inside one HISTORY
/// response, so boxing would buy nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum HistoryItem {
    Message {
        msgid: MsgId,
        sender: UserRef,
        /// Final body — the last edit's, or the original's.
        body: String,
        meta: MsgMeta,
        /// `(edit count, unix ms of final edit)` when edited.
        edited: Option<(u64, u64)>,
        reactions: Vec<ReactionSummary>,
    },
    /// A deleted message: the tombstone is the sole survivor (§12.1).
    Tombstone { msgid: MsgId, by: UserRef },
}

/// Materialize a page. `roots` ascending; `children` in any order (sorted
/// here by msgid = event order, which decides which edit is "final" and
/// whether a reaction ends net-added).
pub fn materialize(roots: Vec<EventRecord>, mut children: Vec<EventRecord>) -> Vec<HistoryItem> {
    children.sort_by(|a, b| a.msgid.cmp(&b.msgid));
    let mut by_root: HashMap<Ulid, Vec<EventRecord>> = HashMap::new();
    for child in children {
        by_root.entry(child.root.ulid()).or_default().push(child);
    }

    roots
        .into_iter()
        .map(|root| {
            let children = by_root.remove(&root.msgid.ulid()).unwrap_or_default();
            materialize_one(root, children)
        })
        .collect()
}

fn materialize_one(root: EventRecord, children: Vec<EventRecord>) -> HistoryItem {
    let EventKind::Message { body, meta } = root.kind else {
        unreachable!("roots() returns Message rows only");
    };

    let mut final_body = body;
    let mut edits: u64 = 0;
    let mut edited_at = 0;
    // (emoji, actor) → net-added? Last op wins (§6.4: REACT idempotent).
    let mut reactions: HashMap<(String, String), bool> = HashMap::new();
    // Actor arrival order per emoji, for the `by=` list.
    let mut arrival: Vec<(String, UserRef)> = Vec::new();

    for child in children {
        match child.kind {
            EventKind::Delete => {
                // Tombstone wins over everything; content is gone (§12.1).
                return HistoryItem::Tombstone {
                    msgid: root.msgid,
                    by: child.sender,
                };
            }
            EventKind::Edit { body } => {
                final_body = body;
                edits += 1;
                edited_at = child.msgid.timestamp_ms();
            }
            EventKind::React { emoji, add } => {
                let key = (emoji.clone(), child.sender.to_string());
                if add && !reactions.get(&key).copied().unwrap_or(false) {
                    arrival.push((emoji.clone(), child.sender.clone()));
                }
                reactions.insert(key, add);
            }
            EventKind::Message { .. } => {
                // A Message row can never be a child of another root.
                unreachable!("children() never returns roots");
            }
        }
    }

    // Fold surviving reactions into per-emoji summaries, arrival-ordered.
    let mut summaries: Vec<ReactionSummary> = Vec::new();
    for (emoji, actor) in arrival {
        if !reactions
            .get(&(emoji.clone(), actor.to_string()))
            .copied()
            .unwrap_or(false)
        {
            continue; // later removed — cancelled pairs vanish (§12.1)
        }
        match summaries.iter_mut().find(|s| s.emoji == emoji) {
            Some(summary) => {
                if summary.actors.iter().any(|a| a == &actor) {
                    continue; // add-remove-add: already counted once
                }
                summary.count += 1;
                if summary.actors.len() < MAX_REACTION_ACTORS {
                    summary.actors.push(actor);
                }
            }
            None => summaries.push(ReactionSummary {
                emoji,
                count: 1,
                actors: vec![actor],
            }),
        }
    }

    HistoryItem::Message {
        msgid: root.msgid,
        sender: root.sender,
        body: final_body,
        meta,
        edited: (edits > 0).then_some((edits, edited_at)),
        reactions: summaries,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Scope;

    fn user(name: &str) -> UserRef {
        format!("{name}@test.example").parse().unwrap()
    }

    fn msgid(seq: u64) -> MsgId {
        format!(
            "test.example/{}",
            Ulid::from_parts(1_000 + seq, seq as u128)
        )
        .parse()
        .unwrap()
    }

    fn root(seq: u64, body: &str) -> EventRecord {
        EventRecord {
            scope: Scope::Channel("#t".parse().unwrap()),
            msgid: msgid(seq),
            root: msgid(seq),
            sender: user("ada"),
            kind: EventKind::Message {
                body: body.to_string(),
                meta: MsgMeta::default(),
            },
        }
    }

    fn child(seq: u64, of: u64, sender: &str, kind: EventKind) -> EventRecord {
        EventRecord {
            scope: Scope::Channel("#t".parse().unwrap()),
            msgid: msgid(seq),
            root: msgid(of),
            sender: user(sender),
            kind,
        }
    }

    fn edit(seq: u64, of: u64, body: &str) -> EventRecord {
        child(
            seq,
            of,
            "ada",
            EventKind::Edit {
                body: body.to_string(),
            },
        )
    }

    fn react(seq: u64, of: u64, who: &str, emoji: &str, add: bool) -> EventRecord {
        child(
            seq,
            of,
            who,
            EventKind::React {
                emoji: emoji.to_string(),
                add,
            },
        )
    }

    #[test]
    fn untouched_message_passes_through() {
        let items = materialize(vec![root(1, "hi")], vec![]);
        let [HistoryItem::Message {
            body,
            edited: None,
            reactions,
            ..
        }] = items.as_slice()
        else {
            panic!("{items:?}");
        };
        assert_eq!(body, "hi");
        assert!(reactions.is_empty());
    }

    #[test]
    fn edits_collapse_to_final_body_with_count() {
        // §12.1: original + final only; intermediates gone; count survives.
        let items = materialize(
            vec![root(1, "v1")],
            vec![edit(2, 1, "v2"), edit(3, 1, "v3"), edit(4, 1, "v4")],
        );
        let [HistoryItem::Message { body, edited, .. }] = items.as_slice() else {
            panic!("{items:?}");
        };
        assert_eq!(body, "v4");
        assert_eq!(*edited, Some((3, msgid(4).timestamp_ms())));
    }

    #[test]
    fn out_of_order_children_still_pick_the_latest_edit() {
        let items = materialize(
            vec![root(1, "v1")],
            vec![edit(3, 1, "final"), edit(2, 1, "middle")], // shuffled
        );
        let [HistoryItem::Message { body, .. }] = items.as_slice() else {
            panic!();
        };
        assert_eq!(body, "final");
    }

    #[test]
    fn cancelled_reactions_vanish_and_re_adds_count_once() {
        // §12.1: add/remove pairs drop; add-remove-add nets one.
        let items = materialize(
            vec![root(1, "hi")],
            vec![
                react(2, 1, "ada", "👍", true),
                react(3, 1, "bob", "👍", true),
                react(4, 1, "ada", "👍", false), // ada cancels
                react(5, 1, "eve", "🔥", true),
                react(6, 1, "eve", "🔥", false), // eve cancels 🔥 entirely
                react(7, 1, "bob", "👍", false),
                react(8, 1, "bob", "👍", true), // bob re-adds
            ],
        );
        let [HistoryItem::Message { reactions, .. }] = items.as_slice() else {
            panic!("{items:?}");
        };
        assert_eq!(
            reactions.len(),
            1,
            "cancelled emoji must vanish: {reactions:?}"
        );
        assert_eq!(reactions[0].emoji, "👍");
        assert_eq!(reactions[0].count, 1);
        assert_eq!(reactions[0].actors, vec![user("bob")]);
    }

    #[test]
    fn duplicate_adds_are_idempotent() {
        let items = materialize(
            vec![root(1, "hi")],
            vec![
                react(2, 1, "ada", "👍", true),
                react(3, 1, "ada", "👍", true), // §6.4: idempotent
            ],
        );
        let [HistoryItem::Message { reactions, .. }] = items.as_slice() else {
            panic!();
        };
        assert_eq!(reactions[0].count, 1);
    }

    #[test]
    fn actor_list_caps_at_twenty_but_count_is_authoritative() {
        let children: Vec<EventRecord> = (0..25)
            .map(|i| react(2 + i, 1, &format!("u{i}"), "👍", true))
            .collect();
        let items = materialize(vec![root(1, "hi")], children);
        let [HistoryItem::Message { reactions, .. }] = items.as_slice() else {
            panic!();
        };
        assert_eq!(reactions[0].count, 25);
        assert_eq!(reactions[0].actors.len(), MAX_REACTION_ACTORS);
    }

    #[test]
    fn delete_leaves_only_the_tombstone() {
        // §12.1: the DELETED event is the sole survivor — edits, body, and
        // reactions are unrecoverable from a batch (invariant 10).
        let items = materialize(
            vec![root(1, "secret"), root(10, "kept")],
            vec![
                edit(2, 1, "more secret"),
                react(3, 1, "bob", "👀", true),
                child(4, 1, "mod", EventKind::Delete),
            ],
        );
        let [HistoryItem::Tombstone { msgid: gone, by }, HistoryItem::Message { body, .. }] =
            items.as_slice()
        else {
            panic!("{items:?}");
        };
        assert_eq!(gone, &msgid(1));
        assert_eq!(by, &user("mod"));
        assert_eq!(body, "kept");
    }

    #[test]
    fn structural_meta_survives_materialization() {
        // Replies/threads unaffected (§12.1): tags live on surviving events.
        let mut record = root(1, "reply body");
        let EventKind::Message { meta, .. } = &mut record.kind else {
            unreachable!()
        };
        meta.reply_to = Some(msgid(0));
        meta.thread = Some(msgid(0));
        let items = materialize(vec![record], vec![edit(2, 1, "edited reply")]);
        let [HistoryItem::Message { meta, body, .. }] = items.as_slice() else {
            panic!();
        };
        assert_eq!(body, "edited reply");
        assert_eq!(meta.reply_to, Some(msgid(0)));
        assert_eq!(meta.thread, Some(msgid(0)));
    }
}
