//! Social domain — the durable social graph: friends + group DMs. The Rust twin of
//! the persistent-state half of `socialHandlers`. **Calls** (the real-time
//! ring/active/group-call state + their LiveKit media side-effects) stay TS, as do
//! the side-effects here: the friend-request toast, `channelStore.ensure`/`delete`
//! for a group, and the self-leave navigation.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::ClientEvent;

/// A group DM (the value; the `&id` is the map key), mirroring TS `GroupInfo`.
#[derive(Serialize, Clone)]
pub struct GroupInfo {
    pub name: Option<String>,
    pub members: Vec<String>,
}

/// This domain's state diffs — the mirror applies them onto `store.social`'s
/// `friends` / `groups` SvelteMaps.
#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SocialDiff {
    FriendSet { user: String, state: String },
    FriendDrop { user: String },
    GroupSet { id: String, name: Option<String>, members: Vec<String> },
    GroupDrop { id: String },
}

/// The social sub-model: the friend graph + group DMs, plus the session ref (for
/// self-leave detection). Transient (re-fetched on connect).
#[derive(Default)]
pub struct Social {
    me: String, // "account@network"
    friends: BTreeMap<String, String>,
    groups: BTreeMap<String, GroupInfo>,
}

impl Social {
    pub fn handle(&mut self, event: &ClientEvent) -> Vec<SocialDiff> {
        match event {
            ClientEvent::Connected { network, account } => {
                self.me = format!("{account}@{network}");

                Vec::new()
            }
            ClientEvent::Friend { user, state } => {
                self.friends.insert(user.clone(), state.clone());

                vec![SocialDiff::FriendSet { user: user.clone(), state: state.clone() }]
            }
            ClientEvent::FriendRemoved { user } => {
                self.friends.remove(user);

                vec![SocialDiff::FriendDrop { user: user.clone() }]
            }
            ClientEvent::Group { id, name, members } => {
                self.groups.insert(id.clone(), GroupInfo { name: name.clone(), members: members.clone() });

                vec![SocialDiff::GroupSet { id: id.clone(), name: name.clone(), members: members.clone() }]
            }
            ClientEvent::GroupMember { group, user, action } => self.group_member(group, user, action),
            _ => Vec::new(),
        }
    }

    // §group-DM membership: join adds (deduped), a non-join by **me** drops the
    // group (the TS nav/channel cleanup rides the same event), any other part
    // removes that member. No-op (no diff) when unknown / nothing changed.
    fn group_member(&mut self, group: &str, user: &str, action: &str) -> Vec<SocialDiff> {
        if !self.groups.contains_key(group) {
            return Vec::new();
        }

        if action != "join" && user == self.me {
            self.groups.remove(group);

            return vec![SocialDiff::GroupDrop { id: group.to_string() }];
        }

        let g = self.groups.get_mut(group).unwrap();

        let changed = if action == "join" {
            if g.members.iter().any(|m| m == user) {
                false
            } else {
                g.members.push(user.to_string());
                true
            }
        } else {
            let before = g.members.len();
            g.members.retain(|m| m != user);
            g.members.len() != before
        };

        if !changed {
            return Vec::new();
        }

        vec![SocialDiff::GroupSet { id: group.to_string(), name: g.name.clone(), members: g.members.clone() }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connected() -> ClientEvent {
        ClientEvent::Connected { network: "home".into(), account: "me".into() }
    }
    fn group_member(group: &str, user: &str, action: &str) -> ClientEvent {
        ClientEvent::GroupMember { group: group.into(), user: user.into(), action: action.into() }
    }
    fn members(d: &SocialDiff) -> Vec<&str> {
        let SocialDiff::GroupSet { members, .. } = d else { panic!("expected GroupSet") };
        members.iter().map(String::as_str).collect()
    }

    #[test]
    fn friend_set_and_removed() {
        let mut s = Social::default();
        assert!(matches!(&s.handle(&ClientEvent::Friend { user: "a@n".into(), state: "incoming".into() })[0],
            SocialDiff::FriendSet { user, state } if user == "a@n" && state == "incoming"));
        assert!(matches!(&s.handle(&ClientEvent::FriendRemoved { user: "a@n".into() })[0],
            SocialDiff::FriendDrop { user } if user == "a@n"));
    }

    #[test]
    fn group_member_join_part_and_self_leave() {
        let mut s = Social::default();
        s.handle(&connected());
        s.handle(&ClientEvent::Group { id: "&g".into(), name: None, members: vec!["me@home".into()] });

        // Join adds (deduped); a duplicate join → no diff.
        assert_eq!(members(&s.handle(&group_member("&g", "bob@home", "join"))[0]), vec!["me@home", "bob@home"]);
        assert!(s.handle(&group_member("&g", "bob@home", "join")).is_empty());
        // Another member's part removes just them.
        assert_eq!(members(&s.handle(&group_member("&g", "bob@home", "part"))[0]), vec!["me@home"]);
        // *My* part drops the whole group.
        assert!(matches!(&s.handle(&group_member("&g", "me@home", "part"))[0],
            SocialDiff::GroupDrop { id } if id == "&g"));
        // A part in an unknown group → no diff.
        assert!(s.handle(&group_member("&gone", "x@home", "part")).is_empty());
    }
}
