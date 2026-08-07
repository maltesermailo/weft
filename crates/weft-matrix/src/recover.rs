//! Rebuilding the daemon's state from Matrix (owner requirement 2026-08-06).
//!
//! The question this answers: **what happens when the daemon's database is
//! deleted?** The answer is "almost nothing is lost", and the reason is worth
//! stating, because it shapes the whole design:
//!
//! - **Structure ids are deterministic.** `ident::stable_ulid` derives a
//!   namespace/channel ULID from the Matrix room id, and weftd *pins* what we
//!   mint — so re-asserting a room reproduces the same WEFT objects rather than
//!   orphaning them. Nothing about the mapping needs storing to be recovered.
//! - **Matrix is a database we already have.** Which rooms we bridge, who is in
//!   them, the power levels, which room is whose DM — all of it is room state we
//!   can read back.
//! - **Ingested msgids are deterministic too** (`ident::msgid_for` from the
//!   event id + its timestamp), and the ids we mint on the *other* side are
//!   stamped onto the Matrix events we send (`dev.weft.msgid`). So the link map
//!   is rebuildable, on demand rather than eagerly — a mutation naming an
//!   unknown event resolves by reading that one event.
//!
//! Exactly one thing cannot be derived: the **bridging ban list**. weftd tells
//! us once and deliberately keeps no record (bridge-session-protocol §11), and
//! Matrix has no opinion about it. So it lives in the bot's Matrix **account
//! data** — the adapter's own durable notebook, which survives our database
//! precisely because it is not in it.
//!
//! What still needs a human: nothing, in the normal case. The manual
//! attachments exist for the abnormal one — a puppet or DM room whose marker
//! state is missing because it was created by an older build.

use serde_json::json;

use crate::bridge::Bridge;
use crate::store::{Projection, Room, Space};

/// The state event marking a bridged Space, and the account-data key holding
/// the ban list. Namespaced under a domain we own, per Matrix convention.
pub const SPACE_MARKER: &str = "dev.weft.space";
pub const DM_MARKER: &str = "dev.weft.dm";
pub const BANS_KEY: &str = "dev.weft.bans";
/// Account data listing the Spaces we *consume*, keyed by room id.
///
/// The marker for a consumed Space cannot live in the room's state: a state event
/// needs power level 50 and the bot joins someone else's Space at 0, so the write is
/// refused every time (`M_FORBIDDEN … user_level (0) < send_level (50)`). Account
/// data is the bot's own and always writable, which is why the ban list lives there
/// too. A *projected* Space is different — we created it and we are its admin — so
/// that marker stays in room state, where it is visible and portable.
pub const CONSUMED_KEY: &str = "dev.weft.consumed";
/// The field carrying a WEFT msgid on an event we sent.
pub const MSGID_FIELD: &str = "dev.weft.msgid";

/// What a recovery pass found, for the operator who asked for it.
#[derive(Debug, Default, PartialEq)]
pub struct Recovered {
    pub spaces: usize,
    pub rooms: usize,
    pub projections: usize,
    pub categories: usize,
    pub dms: usize,
    pub puppets: usize,
    pub bans: usize,
    /// Rooms we are joined to but could not classify — the honest residue.
    pub unclaimed: Vec<String>,
}

impl std::fmt::Display for Recovered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} consumed space(s), {} room(s), {} projection(s), {} category sub-space(s), \
             {} DM(s), {} puppet(s), {} ban(s)",
            self.spaces,
            self.rooms,
            self.projections,
            self.categories,
            self.dms,
            self.puppets,
            self.bans
        )?;
        if !self.unclaimed.is_empty() {
            write!(f, "; {} room(s) unclaimed", self.unclaimed.len())?;
        }
        Ok(())
    }
}

impl Bridge {
    /// Rebuild everything derivable from Matrix. Idempotent: it re-derives
    /// rather than appends, so running it on a healthy daemon changes nothing
    /// and running it on an empty one restores the bridge.
    pub async fn recover(&mut self) -> anyhow::Result<Recovered> {
        let mut found = Recovered::default();

        // The ban list first: it gates everything else, and re-bridging a
        // banned space while restoring would be the worst possible order.
        if let Some(bans) = self.recover_bans().await? {
            found.bans = bans;
        }

        // The consumed-Space records first: they come from the bot's account data,
        // not from room state, because a Space we merely joined will not let us write
        // state into it. Without this pass every consumed Space classifies as an
        // unclaimed child room, and its channels never find their namespace.
        let consumed = self.consumed_spaces().await.unwrap_or_default();

        let rooms = self.hs.joined_rooms().await?;
        // Two passes: Spaces define the containers, so they must exist before a
        // child room can be filed under one.
        let mut children: Vec<(String, Vec<serde_json::Value>)> = Vec::new();

        for room_id in rooms {
            let state = match self.hs.state(&room_id).await {
                Ok(state) => state,
                Err(e) => {
                    tracing::warn!(room_id, "could not read room state: {e:#}");
                    found.unclaimed.push(room_id);
                    continue;
                }
            };

            // A room named in the account-data record IS a consumed Space, whatever
            // its state says.
            if let Some(record) = consumed.get(&room_id) {
                let (Some(ns_id), Some(uri)) = (record["ns"].as_str(), record["uri"].as_str())
                else {
                    continue;
                };
                self.store
                    .save_space(Space {
                        ns_id: ns_id.to_string(),
                        room_id: room_id.clone(),
                        uri: uri.to_string(),
                        ..Space::default()
                    })
                    .await;
                found.spaces += 1;
                continue;
            }

            match self.classify(&room_id, &state).await {
                Some(Classified::Space) => found.spaces += 1,
                Some(Classified::Projection) => found.projections += 1,
                Some(Classified::Dm) => found.dms += 1,
                // Anything else is a child: it needs its Space to exist first,
                // so it waits for the second pass.
                _ => children.push((room_id, state)),
            }
        }

        for (room_id, state) in children {
            match self.recover_child(&room_id, &state).await {
                Some(Classified::Room) => found.rooms += 1,
                Some(Classified::Category) => found.categories += 1,
                _ => found.unclaimed.push(room_id.clone()),
            }

            // Puppets and levels are per-room facts, wherever the room landed.
            found.puppets += self.recover_puppets(&state).await;
            self.recover_levels(&room_id, &state).await;
        }

        Ok(found)
    }

    /// The Spaces we consume, from the bot's account data: `room_id → {ns, uri}`.
    ///
    /// The authoritative record for a consumed Space. Room state cannot hold it —
    /// see [`CONSUMED_KEY`].
    async fn consumed_spaces(
        &self,
    ) -> anyhow::Result<std::collections::BTreeMap<String, serde_json::Value>> {
        let bot = format!("@{}:{}", self.bot_localpart, self.domain);
        let Some(data) = self.hs.account_data(&bot, CONSUMED_KEY).await? else {
            return Ok(Default::default());
        };

        Ok(data["spaces"]
            .as_object()
            .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default())
    }

    /// The ban list from the bot's account data — the one thing Matrix does not
    /// otherwise know and weftd will never repeat.
    async fn recover_bans(&mut self) -> anyhow::Result<Option<usize>> {
        let bot = format!("@{}:{}", self.bot_localpart, self.domain);
        let Some(data) = self.hs.account_data(&bot, BANS_KEY).await? else {
            return Ok(None);
        };
        let banned: Vec<String> = data["banned"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|b| b.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let count = banned.len();
        for ns in banned {
            self.store
                .apply_bridging(&weft_proto::Event::Bridging {
                    namespace: match ns.parse() {
                        Ok(ns) => ns,
                        Err(_) => continue,
                    },
                    state: weft_proto::BridgingState::Banned,
                })
                .await;
        }

        Ok(Some(count))
    }

    /// Persist the ban list where it survives our database. Called on every
    /// `BRIDGING` instruction, since weftd sends each exactly once.
    pub async fn persist_bans(&self) {
        let bot = format!("@{}:{}", self.bot_localpart, self.domain);
        let banned: Vec<&str> = self.store.state.bans.iter().collect();

        if let Err(e) = self
            .hs
            .set_account_data(&bot, BANS_KEY, json!({ "banned": banned }))
            .await
        {
            // Loud: this is the one piece of state with nowhere else to live.
            tracing::error!("could not persist the bridging bans: {e:#}");
        }
    }

    /// A top-level room: a consumed Space, a projected Space, or a DM.
    async fn classify(&mut self, room_id: &str, state: &[serde_json::Value]) -> Option<Classified> {
        if let Some(marker) = state_content(state, DM_MARKER) {
            let (Some(account), Some(mxid)) = (marker["account"].as_str(), marker["mxid"].as_str())
            else {
                return None;
            };
            self.store.save_dm_room(account, mxid, room_id).await;
            return Some(Classified::Dm);
        }

        let marker = state_content(state, SPACE_MARKER)?;
        let ns_id = marker["ns"].as_str()?.to_string();

        match marker["kind"].as_str()? {
            "consumed" => {
                let uri = marker["uri"].as_str()?.to_string();
                self.store
                    .save_space(Space {
                        ns_id,
                        room_id: room_id.to_string(),
                        uri,
                        ..Space::default()
                    })
                    .await;
                Some(Classified::Space)
            }
            "projected" => {
                self.store.save_projection(&ns_id, room_id).await;
                Some(Classified::Projection)
            }
            _ => None,
        }
    }

    /// A room under a Space: a channel's room, or a category sub-space.
    async fn recover_child(
        &mut self,
        room_id: &str,
        state: &[serde_json::Value],
    ) -> Option<Classified> {
        let parent = state
            .iter()
            .find(|ev| ev["type"] == "m.space.parent")
            .and_then(|ev| ev["state_key"].as_str())?
            .to_string();

        // A sub-space under a projected Space is a category.
        if state
            .iter()
            .any(|ev| ev["type"] == "m.room.create" && ev["content"]["type"] == "m.space")
        {
            let (ns_id, _) = self.projection_of_space(&parent)?;
            let name = state_content(state, "m.room.name")?["name"]
                .as_str()?
                .to_string();
            self.store
                .save_projected_category(&ns_id, &name, room_id)
                .await;
            return Some(Classified::Category);
        }

        // Otherwise a channel's room. Its ids are deterministic, which is what
        // makes this recoverable at all.
        let chan_id = crate::ident::stable_ulid(room_id);

        if let Some((ns_id, _)) = self.projection_of_space(&parent) {
            let channel = format!("#{ns_id}/{chan_id}");
            self.store
                .save_projected_room(&ns_id, &channel, room_id)
                .await;
            return Some(Classified::Room);
        }

        // A consumed space: rebuild the room entry, ids and URI alike.
        let (space_uri, ns_id) = self.consumed_of_space(&parent)?;
        let space_ref = crate::ident::SpaceRef::parse(&space_uri)?;
        let uri = space_ref.room_uri(&chan_id);
        let channel = format!("#{ns_id}/{chan_id}");

        let mut space = self.store.state.spaces.get(&space_uri)?.clone();
        space.rooms.insert(
            room_id.to_string(),
            Room {
                chan_id,
                channel,
                uri,
            },
        );
        self.store.save_space(space).await;

        Some(Classified::Room)
    }

    /// Puppets from a room's membership: the localpart *is* the account ULID
    /// (that is why they are keyed by it), so a puppet is self-describing. The
    /// display name carries the account label; a missing one is filled by the
    /// next relay that names them.
    async fn recover_puppets(&mut self, state: &[serde_json::Value]) -> usize {
        let mut found = 0;

        for ev in state.iter().filter(|ev| ev["type"] == "m.room.member") {
            let Some(mxid) = ev["state_key"].as_str() else {
                continue;
            };
            let Some((localpart, server)) = mxid.strip_prefix('@').and_then(|m| m.split_once(':'))
            else {
                continue;
            };
            if server != self.domain {
                continue; // a real Matrix user, not one of ours
            }
            // The remainder after the prefix **is** the account ULID: that is
            // how the localpart was built (`ident::puppet_localpart`), so this
            // is a spelling change, not a lookup — there is nothing here that
            // could have been lost with the database.
            let Some(ulid) = localpart.strip_prefix(&self.puppet_prefix) else {
                if localpart != self.bot_localpart {
                    // On our domain (so ours, by the appservice namespace) yet
                    // not named like a puppet: almost certainly a
                    // `puppet_prefix` changed under an existing deployment,
                    // which orphans every puppet created before it. Say so —
                    // skipping quietly would look like a clean recovery.
                    tracing::warn!(
                        mxid,
                        prefix = %self.puppet_prefix,
                        "a user on our domain does not match the puppet prefix — \
                         orphaned by a prefix change?"
                    );
                }
                continue;
            };
            if self.store.state.users.by_ulid(ulid).is_some() {
                continue;
            }

            // The label is all the display name carries. Absent ⇒ fall back to
            // the ULID: attribution runs on the id, so a missing label costs
            // only the name-index lookup, which the next relay refills.
            let account = ev["content"]["displayname"].as_str().unwrap_or(ulid);
            self.store
                .note_user(ulid, account, &format!("{}{ulid}", self.puppet_prefix))
                .await;
            found += 1;
        }

        found
    }

    /// The power-level baseline: the live map *is* the baseline, so reading it
    /// back is the whole recovery — and it prevents a restored daemon from
    /// re-translating every existing level as if it had just changed.
    async fn recover_levels(&mut self, room_id: &str, state: &[serde_json::Value]) {
        let Some(content) = state_content(state, "m.room.power_levels") else {
            return;
        };
        let users = content["users"]
            .as_object()
            .map(|m| {
                m.iter()
                    .filter_map(|(u, l)| l.as_i64().map(|l| (u.clone(), l)))
                    .collect()
            })
            .unwrap_or_default();

        self.store.set_room_levels(room_id, users).await;
    }

    fn projection_of_space(&self, space_room: &str) -> Option<(String, Projection)> {
        self.store
            .state
            .projections
            .iter()
            .find(|(_, p)| p.space_room == space_room)
            .map(|(ns, p)| (ns.clone(), p.clone()))
    }

    fn consumed_of_space(&self, space_room: &str) -> Option<(String, String)> {
        self.store
            .state
            .spaces
            .values()
            .find(|s| s.room_id == space_room)
            .map(|s| (s.uri.clone(), s.ns_id.clone()))
    }
}

#[derive(Debug, PartialEq)]
enum Classified {
    Space,
    Projection,
    Room,
    Category,
    Dm,
}

/// One state event's content by type (empty state key).
fn state_content<'a>(
    state: &'a [serde_json::Value],
    event_type: &str,
) -> Option<&'a serde_json::Value> {
    state
        .iter()
        .find(|ev| ev["type"] == event_type && ev["state_key"] == "")
        .map(|ev| &ev["content"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recovery_summary_reads_as_a_sentence() {
        let found = Recovered {
            spaces: 1,
            rooms: 3,
            dms: 2,
            puppets: 4,
            bans: 1,
            unclaimed: vec!["!mystery:kde.org".into()],
            ..Recovered::default()
        };
        let text = found.to_string();

        assert!(text.contains("1 consumed space(s)"), "{text}");
        assert!(text.contains("3 room(s)"), "{text}");
        // The residue is reported, never hidden: an unclaimed room is the
        // operator's cue that something needs attaching by hand.
        assert!(text.contains("1 room(s) unclaimed"), "{text}");

        assert!(!Recovered::default().to_string().contains("unclaimed"));
    }

    #[test]
    fn state_lookup_wants_the_empty_state_key() {
        let state = vec![
            serde_json::json!({ "type": "m.room.name", "state_key": "", "content": { "name": "X" } }),
            serde_json::json!({ "type": "m.room.member", "state_key": "@a:b", "content": {} }),
        ];
        assert_eq!(state_content(&state, "m.room.name").unwrap()["name"], "X");
        // A member event is per-user state, never the room's own.
        assert!(state_content(&state, "m.room.member").is_none());
    }
}
