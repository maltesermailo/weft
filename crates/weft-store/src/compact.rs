//! §12.1 compaction: the storage rewrite that runs after the audit window.
//!
//! Like materialization, the semantics are ONE pure function
//! ([`compaction_plan`]) shared by every backend: given a message's rows
//! and the audit cutoff, decide which rows to drop. Backends fetch rows,
//! call the plan, delete — they never reimplement the rules.
//!
//! The audit-window promise (§12.1): "what did it say before?" stays
//! answerable for `compact-after` after the content became stale. So a
//! superseded edit body is droppable only once its *successor* has aged
//! past the cutoff, and a deleted message's content only once the *delete*
//! has.

use std::collections::HashMap;

use weft_proto::Ulid;

use crate::types::{EventKind, EventRecord};

/// Rows (by event ulid) that compaction may delete for one root's family.
/// `root_family` = the root row plus all its children, any order.
pub fn compaction_plan(root_family: &[EventRecord], cutoff_ms: u64) -> Vec<Ulid> {
    let mut rows: Vec<&EventRecord> = root_family.iter().collect();
    rows.sort_by(|a, b| a.msgid.cmp(&b.msgid));

    // Deleted, and the deletion has left the audit window: tombstone only —
    // drop everything else, content is gone for good (§12.1).
    if let Some(delete) = rows
        .iter()
        .find(|r| matches!(r.kind, EventKind::Delete) && r.at_ms() < cutoff_ms)
    {
        let keep = delete.msgid.ulid();
        return rows
            .iter()
            .filter(|r| r.msgid.ulid() != keep)
            .map(|r| r.msgid.ulid())
            .collect();
    }

    let mut drops = Vec::new();

    // Edits: drop E_i iff a successor edit exists whose timestamp has left
    // the window — E_i's body stopped being auditable-relevant then.
    let edits: Vec<&&EventRecord> = rows
        .iter()
        .filter(|r| matches!(r.kind, EventKind::Edit { .. }))
        .collect();
    for (i, edit) in edits.iter().enumerate() {
        if edits[i + 1..].iter().any(|next| next.at_ms() < cutoff_ms) {
            drops.push(edit.msgid.ulid());
        }
    }

    // Reactions per (emoji, actor): the prefix of ops older than the
    // cutoff collapses to its net effect — one add row, or nothing.
    let mut per_actor: HashMap<(String, String), Vec<&&EventRecord>> = HashMap::new();
    for row in rows
        .iter()
        .filter(|r| matches!(r.kind, EventKind::React { .. }) && r.at_ms() < cutoff_ms)
    {
        let EventKind::React { emoji, .. } = &row.kind else {
            unreachable!()
        };
        per_actor
            .entry((emoji.clone(), row.sender.to_string()))
            .or_default()
            .push(row);
    }
    for (_, ops) in per_actor {
        let Some(last) = ops.last() else { continue };
        let EventKind::React { add, .. } = &last.kind else {
            unreachable!()
        };
        // Net-add: keep only the final add. Net-remove: ping-pong that
        // cancelled entirely — drop the whole old prefix (§12.1).
        let keep = if *add { Some(last.msgid.ulid()) } else { None };
        for op in ops {
            if Some(op.msgid.ulid()) != keep {
                drops.push(op.msgid.ulid());
            }
        }
    }

    drops
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Scope;
    use weft_proto::{MsgId, MsgMeta, UserRef};

    fn user(name: &str) -> UserRef {
        format!("{name}@test.example").parse().unwrap()
    }

    fn msgid(at_ms: u64) -> MsgId {
        format!("test.example/{}", Ulid::from_parts(at_ms, at_ms as u128))
            .parse()
            .unwrap()
    }

    fn record(at_ms: u64, root_ms: u64, sender: &str, kind: EventKind) -> EventRecord {
        EventRecord {
            scope: Scope::Channel("#t".parse().unwrap()),
            msgid: msgid(at_ms),
            root: msgid(root_ms),
            sender: user(sender),
            kind,
        }
    }

    fn message(at_ms: u64) -> EventRecord {
        record(
            at_ms,
            at_ms,
            "ada",
            EventKind::Message {
                body: "original".into(),
                meta: MsgMeta::default(),
            },
        )
    }

    fn edit(at_ms: u64, root: u64) -> EventRecord {
        record(
            at_ms,
            root,
            "ada",
            EventKind::Edit {
                body: format!("v{at_ms}"),
            },
        )
    }

    fn react(at_ms: u64, root: u64, who: &str, add: bool) -> EventRecord {
        record(
            at_ms,
            root,
            who,
            EventKind::React {
                emoji: "👍".into(),
                add,
            },
        )
    }

    fn ulids(records: &[&EventRecord]) -> Vec<Ulid> {
        records.iter().map(|r| r.msgid.ulid()).collect()
    }

    #[test]
    fn fresh_families_are_untouchable() {
        // Everything inside the audit window: nothing drops.
        let family = [message(100), edit(200, 100), react(300, 100, "bob", true)];
        assert!(compaction_plan(&family, 100).is_empty());
    }

    #[test]
    fn superseded_edits_drop_once_their_successor_ages_out() {
        let e1 = edit(200, 100);
        let e2 = edit(300, 100);
        let e3 = edit(900, 100); // successor still in window
        let family = [message(100), e1.clone(), e2.clone(), e3];
        // cutoff 400: e2 (at 300) has aged out → e1 is droppable; e2 stays
        // because ITS successor (e3, 900) is still in the window.
        assert_eq!(compaction_plan(&family, 400), ulids(&[&e1]));
        // cutoff 1000: e3 aged out too → e1 and e2 both drop; e3 (final) stays.
        let plan = compaction_plan(&family, 1_000);
        assert_eq!(plan.len(), 2);
        assert!(!plan.contains(&msgid(900).ulid()), "final edit survives");
    }

    #[test]
    fn cancelled_reaction_pairs_vanish_net_adds_keep_one() {
        let a1 = react(200, 100, "bob", true);
        let a2 = react(300, 100, "bob", false); // bob cancelled
        let a3 = react(400, 100, "eve", true);
        let a4 = react(500, 100, "eve", false);
        let a5 = react(600, 100, "eve", true); // eve nets add
        let family = [
            message(100),
            a1.clone(),
            a2.clone(),
            a3.clone(),
            a4.clone(),
            a5.clone(),
        ];
        let plan = compaction_plan(&family, 1_000);
        // bob's pair gone entirely; eve keeps only the final add.
        assert!(plan.contains(&a1.msgid.ulid()));
        assert!(plan.contains(&a2.msgid.ulid()));
        assert!(plan.contains(&a3.msgid.ulid()));
        assert!(plan.contains(&a4.msgid.ulid()));
        assert!(!plan.contains(&a5.msgid.ulid()));
    }

    #[test]
    fn recent_reaction_ops_are_left_alone() {
        let a1 = react(200, 100, "bob", true);
        let a2 = react(900, 100, "bob", false); // still in window
        let family = [message(100), a1, a2];
        // Only ops older than the cutoff join the prefix; bob's old add
        // nets "added" *within the prefix*, so it stays as the single row.
        assert!(compaction_plan(&family, 400).is_empty());
    }

    #[test]
    fn old_deleted_messages_keep_only_the_tombstone() {
        let del = record(400, 100, "mod", EventKind::Delete);
        let family = [
            message(100),
            edit(200, 100),
            react(300, 100, "bob", true),
            del.clone(),
        ];
        let plan = compaction_plan(&family, 1_000);
        assert_eq!(plan.len(), 3, "everything but the tombstone");
        assert!(!plan.contains(&del.msgid.ulid()));
    }

    #[test]
    fn recent_deletes_preserve_the_family_for_audit() {
        // Delete still inside the window: moderators can still ask what it said.
        let del = record(900, 100, "mod", EventKind::Delete);
        let family = [message(100), edit(200, 100), del];
        assert!(compaction_plan(&family, 400).is_empty());
    }
}
