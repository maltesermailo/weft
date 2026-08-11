//! Power levels ↔ capabilities (matrix.md §10, protocol doc §7).
//!
//! weftd carries no notion of a level: it speaks capabilities, and **the
//! adapter owns the mapping** — this module is that decision, in one place.
//! The translation is deliberately coarse (three tiers), because it is lossy
//! in both directions and pretending otherwise would invent authority:
//!
//! | tier | level | capabilities |
//! |---|---|---|
//! | admin | 90 | `ns-admin` (which implies the rest weftd-side) |
//! | moderator | 50 | `mute,ban,kick,delete-any` |
//! | member | 0 | — |
//! | muted | -1 | a §6.7 mute — below `events_default`, so the homeserver
//!   refuses their messages. Outbound only: an inbound negative level reads as
//!   "no tier" and revokes caps, not as a WEFT mute. |
//!
//! The bot sits at 100, above every mapped tier — §9: bridge-created rooms
//! are bridge-controlled.

use std::collections::BTreeMap;

pub const BOT_LEVEL: i64 = 100;
pub const ADMIN_LEVEL: i64 = 90;
pub const MOD_LEVEL: i64 = 50;

/// What a §6.7 mute writes. The only level we ever set below zero, which is what
/// makes lifting one safe: a negative level is ours to clear, anything else was
/// set by something we must not overwrite (see `Bridge::lift_mute_outbound`).
pub const MUTED_LEVEL: i64 = -1;

pub const MOD_CAPS: &str = "mute,ban,kick,delete-any";
pub const ADMIN_CAPS: &str = "ns-admin";

/// The level a WEFT grant implies — keyed on the *strongest* capability in
/// the granted set, since a grant states what was given, not a total.
pub fn level_for_grant(caps: &str) -> i64 {
    let caps: Vec<&str> = caps.split(',').map(str::trim).collect();

    if caps.contains(&"ns-admin") {
        return ADMIN_LEVEL;
    }
    if caps
        .iter()
        .any(|c| matches!(*c, "mute" | "ban" | "kick" | "delete-any"))
    {
        return MOD_LEVEL;
    }
    0
}

/// The capabilities a Matrix level implies. `None` = no tier reached — the
/// caller revokes rather than grants.
pub fn caps_for_level(level: i64) -> Option<&'static str> {
    if level >= ADMIN_LEVEL {
        return Some(ADMIN_CAPS);
    }
    if level >= MOD_LEVEL {
        return Some(MOD_CAPS);
    }
    None
}

/// The users whose level changed between two `m.room.power_levels` `users`
/// maps — including users dropped from the map (their new level is the
/// default, 0).
pub fn diff_users(old: &BTreeMap<String, i64>, new: &BTreeMap<String, i64>) -> Vec<(String, i64)> {
    let mut changed = Vec::new();

    for (user, level) in new {
        if old.get(user) != Some(level) {
            changed.push((user.clone(), *level));
        }
    }
    for user in old.keys() {
        if !new.contains_key(user) {
            changed.push((user.clone(), 0));
        }
    }

    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_map_to_tiers_and_back() {
        assert_eq!(level_for_grant("ns-admin"), ADMIN_LEVEL);
        assert_eq!(level_for_grant("mute,ban,kick,delete-any"), MOD_LEVEL);
        assert_eq!(level_for_grant("ban"), MOD_LEVEL);
        assert_eq!(
            level_for_grant("send,react"),
            0,
            "plain member caps are no tier"
        );

        assert_eq!(caps_for_level(100), Some(ADMIN_CAPS));
        assert_eq!(caps_for_level(90), Some(ADMIN_CAPS));
        assert_eq!(caps_for_level(50), Some(MOD_CAPS));
        assert_eq!(caps_for_level(49), None);
        assert_eq!(caps_for_level(0), None);
    }

    #[test]
    fn level_diffs_catch_raises_drops_and_removals() {
        let old: BTreeMap<String, i64> = [("@a:x".into(), 50), ("@b:x".into(), 90)]
            .into_iter()
            .collect();
        let new: BTreeMap<String, i64> = [("@a:x".into(), 90), ("@c:x".into(), 50)]
            .into_iter()
            .collect();

        let mut diff = diff_users(&old, &new);
        diff.sort();
        assert_eq!(
            diff,
            [
                ("@a:x".to_string(), 90), // raised
                ("@b:x".to_string(), 0),  // removed from the map = default
                ("@c:x".to_string(), 50), // new moderator
            ]
        );

        assert!(diff_users(&new, &new).is_empty(), "no change, no work");
    }
}
