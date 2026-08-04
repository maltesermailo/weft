//! Roles domain (§6.5) — per-scope **role definitions** + per-`(account, scope)`
//! **role membership**. The Rust mirror of `rolesHandlers`' role/member bookkeeping.
//!
//! Roles arrive as a batch (a `roles` fetch → `ROLE` events → `BATCH END`): this
//! buffers them (grouped by the event's own `scope`) and, on the role batch's end,
//! emits one `RoleList` per scope that **replaces** that scope's list atomically.
//! `ROLE-MEMBER` is a direct set. So this slice owns the batch *logic* (buffer,
//! sort, scope-route) and the transform to diffs; the data itself lives in the TS
//! mirror (`rolesByScope` / `Server.roles` / `memberRoles`).
//!
//! Scope-keyed like the TS: `ns:<id>` roles go on `Server.roles`, everything else
//! (`*`, `#chan`) under `rolesByScope` — the mirror routes by prefix.
//!
//! NOT here (stays TS): the permission **caps** (`session.caps`, from `CAPS`
//! events — the gate machinery), `grant-info`/grants, and the role-editor actions.
//! `mentionsMe` reads this migrated data and — together with the model's "me" and
//! a model-side copy of these lists — moves with the **messages** slice.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::ClientEvent;

/// A role definition. `caps` is the role's capability list (display/editor only —
/// the client gates on server-resolved `session.caps`, not on this).
#[derive(Serialize, Clone)]
pub struct Role {
    pub id: String,
    pub name: String,
    pub color: String,
    pub caps: Vec<String>,
    pub hoist: bool,
    pub pingable: bool,
    pub position: i32,
}

/// This domain's state diffs.
/// - `RoleList` replaces a scope's whole role list (idempotent → a re-fetch /
///   reconnect replaces cleanly). The mirror routes it to `Server.roles` (ns) or
///   `rolesByScope` (else) and reconstructs `Role` class instances.
/// - `MemberRoles` sets one `(account, scope)`'s role ids.
#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum RoleDiff {
    RoleList {
        scope: String,
        roles: Vec<Role>,
    },
    MemberRoles {
        scope: String,
        account: String,
        roles: Vec<String>,
    },
}

/// The roles sub-model — the streaming-batch state (buffer + window flag) plus a
/// small stored copy of the role definitions + memberships that [`mentions_me`]
/// needs. The mirror still holds its own copy for rendering; this one exists only
/// so the **messages** store can derive a message's `mentioned` flag in the model.
///
/// [`mentions_me`]: Roles::mentions_me
#[derive(Default)]
pub struct Roles {
    // Buffer while a role batch streams, grouped by the ROLE event's own scope.
    role_buf: BTreeMap<String, Vec<Role>>,
    in_role_batch: bool,
    /// Stored role definitions per scope (replaced at each batch's flush).
    roles: BTreeMap<String, Vec<Role>>,
    /// `"account|scope"` → the role ids that account holds (from `ROLE-MEMBER`).
    member_roles: BTreeMap<String, Vec<String>>,
}

/// Split a comma-separated wire list (`""` → empty), used for `caps` and role ids.
fn split_list(s: &str) -> Vec<String> {
    if s.is_empty() {
        Vec::new()
    } else {
        s.split(',').map(str::to_string).collect()
    }
}

impl Roles {
    pub fn handle(&mut self, event: &ClientEvent) -> Vec<RoleDiff> {
        match event {
            ClientEvent::Role {
                scope,
                role,
                color,
                caps,
                hoist,
                pingable,
                position,
                name,
            } => {
                self.role_buf.entry(scope.clone()).or_default().push(Role {
                    id: role.clone(),
                    name: name.clone(),
                    color: color.clone(),
                    caps: split_list(caps),
                    hoist: *hoist,
                    pingable: *pingable,
                    position: *position,
                });

                Vec::new() // buffered; the diff is emitted at the batch's end
            }
            ClientEvent::RoleMember {
                scope,
                account,
                roles,
            } => {
                let ids = split_list(roles);
                self.member_roles
                    .insert(format!("{account}|{scope}"), ids.clone());

                vec![RoleDiff::MemberRoles {
                    scope: scope.clone(),
                    account: account.clone(),
                    roles: ids,
                }]
            }
            // §6.5 role batches are id-prefixed `r…`; mark the window so the matching
            // BATCH END flushes the buffered roles.
            ClientEvent::BatchStart { id } if id.starts_with('r') => {
                self.in_role_batch = true;

                Vec::new()
            }
            ClientEvent::BatchEnd { .. } => self.flush_roles(),
            _ => Vec::new(),
        }
    }

    fn flush_roles(&mut self) -> Vec<RoleDiff> {
        if !self.in_role_batch {
            return Vec::new();
        }

        self.in_role_batch = false;

        std::mem::take(&mut self.role_buf)
            .into_iter()
            .map(|(scope, mut roles)| {
                // Keep position order (the server sorts, but be safe).
                roles.sort_by(|a, b| {
                    a.position
                        .cmp(&b.position)
                        .then_with(|| a.name.cmp(&b.name))
                });

                self.roles.insert(scope.clone(), roles.clone());

                RoleDiff::RoleList { scope, roles }
            })
            .collect()
    }

    /// Port of the TS `session.mentionsMe`: does `body` ping `me` directly,
    /// `@everyone`/`@here`, or a **pingable** role `me` holds at `ns`'s scope?
    /// (`ns` empty → the network scope `*`.) The messages store calls this through
    /// `AppState` to derive a message's `mentioned` flag for the unread tally.
    pub fn mentions_me(&self, me: &str, body: &str, ns: &str) -> bool {
        if me.is_empty() {
            return false;
        }

        if mentions_token(body, me)
            || mentions_token(body, "everyone")
            || mentions_token(body, "here")
        {
            return true;
        }

        let scope = if ns.is_empty() {
            "*".to_string()
        } else {
            format!("ns:{ns}")
        };
        let Some(mine) = self.member_roles.get(&format!("{me}|{scope}")) else {
            return false;
        };

        self.roles.get(&scope).is_some_and(|roles| {
            roles
                .iter()
                .any(|r| r.pingable && mine.contains(&r.id) && mentions_token(body, &r.name))
        })
    }
}

/// Case-insensitive `@token\b` test (the TS mention regex, compared literally).
/// A match needs the char after `@token` to be a non-word char (`[A-Za-z0-9_]`)
/// or end-of-string — the `\b` word boundary.
fn mentions_token(body: &str, token: &str) -> bool {
    if token.is_empty() {
        return false;
    }

    let body = body.to_lowercase();
    let needle = format!("@{}", token.to_lowercase());

    let mut from = 0;
    while let Some(rel) = body[from..].find(&needle) {
        let end = from + rel + needle.len();
        let boundary = match body[end..].chars().next() {
            Some(c) => !(c.is_ascii_alphanumeric() || c == '_'),
            None => true,
        };

        if boundary {
            return true;
        }

        from = end;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(scope: &str, id: &str, name: &str, position: i32) -> ClientEvent {
        ClientEvent::Role {
            scope: scope.into(),
            role: id.into(),
            color: "#fff".into(),
            caps: "send,react".into(),
            hoist: false,
            pingable: true,
            position,
            name: name.into(),
        }
    }
    fn member(scope: &str, account: &str, roles: &str) -> ClientEvent {
        ClientEvent::RoleMember {
            scope: scope.into(),
            account: account.into(),
            roles: roles.into(),
        }
    }
    fn batch_start(id: &str) -> ClientEvent {
        ClientEvent::BatchStart { id: id.into() }
    }
    fn batch_end() -> ClientEvent {
        ClientEvent::BatchEnd {
            id: "r1".into(),
            truncated: false,
        }
    }
    fn role_list<'a>(diffs: &'a [RoleDiff], scope: &str) -> Option<Vec<&'a str>> {
        diffs.iter().find_map(|d| match d {
            RoleDiff::RoleList { scope: s, roles } if s == scope => {
                Some(roles.iter().map(|r| r.name.as_str()).collect())
            }
            _ => None,
        })
    }

    #[test]
    fn role_batch_buffers_then_flushes_sorted_on_end() {
        let mut r = Roles::default();
        assert!(r.handle(&batch_start("r1")).is_empty());
        // Out-of-order positions — the flush sorts them.
        assert!(r.handle(&role("ns:x", "id-b", "Mods", 1)).is_empty()); // buffered, no diff
        assert!(r.handle(&role("ns:x", "id-a", "Admins", 0)).is_empty());
        assert_eq!(
            role_list(&r.handle(&batch_end()), "ns:x"),
            Some(vec!["Admins", "Mods"])
        );
    }

    #[test]
    fn refetch_replaces_and_caps_parse() {
        let mut r = Roles::default();
        r.handle(&batch_start("r1"));
        r.handle(&role("ns:x", "id-a", "Admins", 0));
        r.handle(&role("ns:x", "id-b", "Mods", 1));
        r.handle(&batch_end());
        // A re-fetch carrying only one role → the RoleList replaces (not merges).
        r.handle(&batch_start("r2"));
        r.handle(&role("ns:x", "id-a", "Admins", 0));
        let diffs = r.handle(&batch_end());
        assert_eq!(role_list(&diffs, "ns:x"), Some(vec!["Admins"]));
        let RoleDiff::RoleList { roles, .. } = &diffs[0] else {
            panic!()
        };
        assert_eq!(roles[0].caps, vec!["send", "react"]); // comma list parsed
    }

    #[test]
    fn non_role_batch_end_does_not_flush() {
        let mut r = Roles::default();
        assert!(r.handle(&batch_end()).is_empty()); // no `r…` start → nothing buffered/flushed
    }

    #[test]
    fn member_roles_set_directly() {
        let mut r = Roles::default();
        let diffs = r.handle(&member("ns:x", "alice", "id-a,id-b"));
        assert!(matches!(&diffs[0],
            RoleDiff::MemberRoles { account, roles, .. } if account == "alice" && roles == &["id-a", "id-b"]));
        // Empty → cleared list.
        let RoleDiff::MemberRoles { roles, .. } = &r.handle(&member("ns:x", "alice", ""))[0] else {
            panic!()
        };
        assert!(roles.is_empty());
    }

    #[test]
    fn mentions_me_direct_and_everyone_here() {
        let r = Roles::default();
        assert!(r.mentions_me("alice", "hey @alice look", ""));
        assert!(r.mentions_me("alice", "@ALICE (case-insensitive)", ""));
        assert!(r.mentions_me("alice", "ping @everyone now", "n"));
        assert!(r.mentions_me("alice", "@here quick", "n"));
        // Word boundary: `@alicexyz` is not a mention of `alice`.
        assert!(!r.mentions_me("alice", "mail @alicexyz today", ""));
        // Not mentioned at all.
        assert!(!r.mentions_me("alice", "just chatting", ""));
        // No "me" → never.
        assert!(!r.mentions_me("", "@everyone", ""));
    }

    #[test]
    fn mentions_me_pingable_role_i_hold() {
        let mut r = Roles::default();
        // Two roles at ns:x — Mods pingable, Muted not.
        r.handle(&batch_start("r1"));
        r.handle(&ClientEvent::Role {
            scope: "ns:x".into(),
            role: "id-mods".into(),
            color: "#fff".into(),
            caps: "".into(),
            hoist: false,
            pingable: true,
            position: 0,
            name: "Mods".into(),
        });
        r.handle(&ClientEvent::Role {
            scope: "ns:x".into(),
            role: "id-muted".into(),
            color: "#fff".into(),
            caps: "".into(),
            hoist: false,
            pingable: false,
            position: 1,
            name: "Muted".into(),
        });
        r.handle(&batch_end());
        r.handle(&member("ns:x", "alice", "id-mods"));

        // `ns` maps to the `ns:<ns>` scope; alice holds pingable Mods → mentioned.
        assert!(r.mentions_me("alice", "hey @Mods help", "x"));
        // A non-pingable role I hold does not ping.
        assert!(!r.mentions_me("alice", "the @Muted list", "x"));
        // A pingable role I do NOT hold does not ping me.
        assert!(!r.mentions_me("bob", "@Mods", "x"));
        // Right role name, wrong scope (no stored roles at `*`) → no ping.
        assert!(!r.mentions_me("alice", "@Mods", ""));
    }
}
