//! The bridge core: one task, two inbound streams, no shared state.
//!
//! Everything the daemon *does* funnels through here — weftd's stream (the
//! `weft-appservice` `Incoming`s) on one side, the homeserver's transaction
//! pushes on the other. Single-tasked on purpose: the store has one writer,
//! ordering within each side is preserved, and there is no lock to hold wrong.
//!
//! The flows implemented are the MVP set (plan slice 10): provisioning,
//! structure assertion, bidirectional messages/edits/deletes/reactions, and §8
//! membership mapping. Moderation and power levels are slice 11; media,
//! typing, DMs and HISTORY backfill are deferred and logged when they knock.

use std::collections::BTreeSet;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use weft_appservice::{ChannelAssertion, Incoming, NamespaceAssertion, Realm};
use weft_proto::{Command, Event, MemberAction};

use crate::asapi::Txn;
use crate::hs::Hs;
use crate::ident;
use crate::store::{Room, Space, Store};

pub struct Bridge {
    pub realm: Realm,
    pub hs: Hs,
    pub store: Store,
    /// The companion homeserver's server name — puppets live under it.
    pub domain: String,
    pub puppet_prefix: String,
    pub bot_localpart: String,
}

impl Bridge {
    /// The dispatch loop. Ends when either stream does.
    pub async fn run(
        mut self,
        mut incoming: mpsc::Receiver<Incoming>,
        // Borrowed: the transaction stream outlives any one weftd session —
        // the AS server keeps feeding it across reconnects.
        txns: &mut mpsc::Receiver<Txn>,
    ) -> anyhow::Result<()> {
        loop {
            tokio::select! {
                inc = incoming.recv() => {
                    let Some(inc) = inc else { break };
                    self.on_incoming(inc).await;
                }
                txn = txns.recv() => {
                    let Some(txn) = txn else { break };
                    for event in txn.events {
                        self.on_matrix_event(event).await;
                    }
                }
            }
        }

        Ok(())
    }

    // ---- weftd → us -------------------------------------------------------

    pub async fn on_incoming(&mut self, inc: Incoming) {
        match inc {
            Incoming::Event(Event::Provision { uri, job }) => {
                let ok = self.provision(&uri.to_string()).await.unwrap_or_else(|e| {
                    warn!(%uri, "provisioning failed: {e:#}");
                    false
                });
                let _ = self.realm.provisioned(&job, ok).await;
            }
            Incoming::Event(bridging @ Event::Bridging { .. }) => {
                // The row is the enforcement: weftd never re-sends a ban.
                if let Some((ns, banned)) = self.store.apply_bridging(&bridging).await {
                    info!(ns, banned, "bridging instruction from the operator");
                }
            }
            Incoming::Event(Event::Message(m)) => {
                if let Err(e) = self.relay_message(&m).await {
                    warn!(msgid = %m.msgid, "relay to Matrix failed: {e:#}");
                }
            }
            Incoming::Event(Event::Edited {
                user,
                msgid,
                edit_of,
                body,
                ..
            }) => {
                if let Err(e) = self
                    .relay_edit(
                        &user.to_string(),
                        &msgid.to_string(),
                        &edit_of.to_string(),
                        &body,
                    )
                    .await
                {
                    warn!(%edit_of, "edit relay to Matrix failed: {e:#}");
                }
            }
            Incoming::Event(Event::Deleted { msgid, by, .. }) => {
                let by = by.map(|u| u.to_string());
                if let Err(e) = self.relay_delete(by.as_deref(), &msgid.to_string()).await {
                    warn!(%msgid, "delete relay to Matrix failed: {e:#}");
                }
            }
            Incoming::Event(Event::Reaction {
                msgid,
                emoji,
                op,
                by,
                ..
            }) => {
                let add = op == weft_proto::ReactionOp::Add;
                if let Err(e) = self
                    .relay_reaction(&by.to_string(), &msgid.to_string(), &emoji, add)
                    .await
                {
                    warn!(%msgid, "reaction relay to Matrix failed: {e:#}");
                }
            }
            // Structure acks: weftd pinning the ids we minted. Nothing to do.
            Incoming::Event(Event::NsMeta { .. } | Event::ChannelLayout { .. }) => {}
            Incoming::Event(other) => debug!(?other, "unhandled weftd event"),

            Incoming::Command {
                as_user,
                as_ulid,
                command,
            } => {
                self.on_weftd_request(as_user, as_ulid, command).await;
            }
        }
    }

    /// weftd asking us to act — membership requests and local users' mutations
    /// of realm-minted messages (bridge-session-protocol §6, §8).
    ///
    /// The actor arrives as name **and ULID**; everything here keys on the
    /// ULID (the stable identity — names are mutable vanity labels) and keeps
    /// the name only for attribution strings.
    async fn on_weftd_request(
        &mut self,
        as_user: Option<String>,
        as_ulid: Option<String>,
        command: Command,
    ) {
        let Some(user) = as_user else {
            debug!(?command, "request without @as — ignored");
            return;
        };
        let Some(ulid) = as_ulid else {
            // A relay without the identity tag cannot be puppeted safely —
            // keying by name would orphan the puppet on a rename.
            warn!(user, ?command, "request without @ulid — dropped");
            return;
        };
        let account = user.split('@').next().unwrap_or(&user).to_string();

        match command {
            Command::NsJoin { ns } => {
                if let Err(e) = self
                    .puppet_join_namespace(&ulid, &account, &ns.to_string())
                    .await
                {
                    warn!(user, "NS JOIN relay failed: {e:#}");
                }
            }
            Command::NsLeave { ns } => {
                if let Err(e) = self
                    .puppet_leave_namespace(&ulid, &account, &ns.to_string())
                    .await
                {
                    warn!(user, "NS LEAVE relay failed: {e:#}");
                }
            }
            Command::React { msgid, emoji } => {
                if let Err(e) = self
                    .local_reaction(&user, &ulid, &account, &msgid.to_string(), &emoji, true)
                    .await
                {
                    warn!(user, %msgid, "relayed REACT failed: {e:#}");
                }
            }
            Command::Unreact { msgid, emoji } => {
                if let Err(e) = self
                    .local_reaction(&user, &ulid, &account, &msgid.to_string(), &emoji, false)
                    .await
                {
                    warn!(user, %msgid, "relayed UNREACT failed: {e:#}");
                }
            }
            Command::Delete { msgid } => {
                if let Err(e) = self
                    .local_delete(&user, &ulid, &account, &msgid.to_string())
                    .await
                {
                    warn!(user, %msgid, "relayed DELETE failed: {e:#}");
                }
            }
            // HISTORY backfill is deferred with the rest of the scrollback
            // work; saying so beats silently eating the request.
            Command::History { .. } => debug!("HISTORY backfill not implemented yet (deferred)"),
            other => debug!(?other, "unhandled weftd request"),
        }
    }

    // ---- provisioning (PROVISION → resolve + join + enumerate + assert) ----

    /// Resolve, join and assert one space. Returns whether to answer
    /// `PROVISION-OK`.
    pub async fn provision(&mut self, uri: &str) -> anyhow::Result<bool> {
        let Some(space_ref) = ident::SpaceRef::parse(uri) else {
            return Ok(false);
        };
        let space_uri = ident::SpaceRef {
            room: None,
            ..space_ref.clone()
        }
        .uri();

        let (space_room, servers) = self.hs.resolve_alias(&space_ref.alias()).await?;
        let ns_id = ident::stable_ulid(&space_room);

        // The operator banned this space: refuse to provision, uniformly.
        if self.store.state.bans.is_banned(&ns_id) {
            info!(uri, "provision refused — space is banned from bridging");
            return Ok(false);
        }

        self.hs.join(&space_room, &servers, None).await?;
        let state = self.hs.state(&space_room).await?;

        let title = state_str(&state, "m.room.name", "name");
        let description = state_str(&state, "m.room.topic", "topic");

        self.realm
            .assert_namespace(&NamespaceAssertion {
                uri: &space_uri,
                id: &ns_id,
                visibility: weft_proto::Visibility::Public,
                title: title.as_deref(),
                description: description.as_deref(),
                icon: None,
                // Matrix is a levels realm: the native roles editor is hidden;
                // the Power Levels surface arrives with slice 11.
                authority: Some(weft_proto::Authority::Levels),
                settings_disabled: &["roles"],
            })
            .await?;

        // The space's children, in order-key order (design doc §6).
        let mut children: Vec<(String, String)> = state
            .iter()
            .filter(|ev| ev["type"] == "m.space.child")
            .filter(|ev| {
                ev["content"]["via"]
                    .as_array()
                    .is_some_and(|via| !via.is_empty())
            })
            .filter_map(|ev| {
                let room = ev["state_key"].as_str()?.to_string();
                let order = ev["content"]["order"].as_str().unwrap_or("").to_string();
                Some((order, room))
            })
            .collect();
        children.sort();

        let mut space = Space {
            ns_id: ns_id.clone(),
            room_id: space_room.clone(),
            uri: space_uri.clone(),
            ..Space::default()
        };

        for (position, (_, child)) in children.iter().enumerate() {
            match self
                .provision_room(&space_ref, &ns_id, child, position as i64)
                .await
            {
                Ok(Some((room, members))) => {
                    for member in members {
                        space
                            .member_rooms
                            .entry(member)
                            .or_default()
                            .insert(child.clone());
                    }
                    space.rooms.insert(child.clone(), room);
                }
                // Unbridgeable (encrypted, unjoinable): absent, not fatal —
                // the same treatment the IRC gateway gives e2ee (§13).
                Ok(None) => {}
                Err(e) => warn!(child, "room provisioning failed: {e:#}"),
            }
        }

        // A space with no bridgeable rooms still provisions: Spaces exist
        // without chats, and they map exactly like an empty WEFT namespace
        // (owner directive 2026-08-06). Rooms added later arrive by
        // re-assertion. (This also keeps the answer consistent — the namespace
        // was already asserted above, so refusing here would leave it behind
        // anyway.)

        // State the membership we can see. Additive statements, not a SYNC
        // window — full-replace covers *every* namespace we govern, which is
        // exactly wrong while provisioning space #2 of 2.
        for user in space.member_rooms.keys() {
            let _ = self.realm.member(&ns_id, user, MemberAction::Join).await;
        }

        self.store.save_space(space).await;
        Ok(true)
    }

    /// Join + assert one child room. `None` = deliberately not bridgeable.
    async fn provision_room(
        &mut self,
        space_ref: &ident::SpaceRef,
        ns_id: &str,
        room_id: &str,
        position: i64,
    ) -> anyhow::Result<Option<(Room, BTreeSet<String>)>> {
        self.hs.join(room_id, &[], None).await?;
        let state = self.hs.state(room_id).await?;

        // Locked decision 7: an encrypted room gets no channel, ever.
        if state.iter().any(|ev| ev["type"] == "m.room.encryption") {
            debug!(room_id, "encrypted room — not bridged (invariant 8)");
            return Ok(None);
        }

        let chan_id = ident::stable_ulid(room_id);
        let vanity = state_str(&state, "m.room.name", "name")
            .map(|name| vanity_of(&name))
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| chan_id.clone());

        // The room's URI segment is its channel ULID: unique and stable by
        // construction, where a name-derived segment could collide or churn.
        let uri = space_ref.room_uri(&chan_id);
        let channel = self
            .realm
            .assert_channel(&ChannelAssertion {
                uri: &uri,
                id: &chan_id,
                namespace_id: ns_id,
                vanity: &vanity,
                position,
                kind: weft_proto::ChannelKind::Text,
                category: None,
            })
            .await?;

        let members = state
            .iter()
            .filter(|ev| ev["type"] == "m.room.member")
            .filter(|ev| ev["content"]["membership"] == "join")
            .filter_map(|ev| ev["state_key"].as_str())
            .filter_map(|mxid| self.foreign_user(mxid))
            .collect();

        Ok(Some((
            Room {
                chan_id,
                channel,
                uri,
            },
            members,
        )))
    }

    /// A remote MXID as a WEFT user — `None` for our own bot/puppets (their
    /// events are relays, never originals) and for unmappable identities.
    fn foreign_user(&self, mxid: &str) -> Option<String> {
        let parsed: &ruma::UserId = mxid.try_into().ok()?;

        if ident::is_our_mxid(
            parsed,
            &self.puppet_prefix,
            &self.domain,
            &self.bot_localpart,
        ) {
            return None;
        }

        let user = ident::weft_user(parsed);
        if user.is_none() {
            debug!(mxid, "unmappable identity — skipped");
        }

        user
    }

    // ---- Matrix → WEFT (transaction events) --------------------------------

    pub async fn on_matrix_event(&mut self, ev: Value) {
        let Some(room_id) = ev["room_id"].as_str().map(String::from) else {
            return;
        };
        let Some(sender) = ev["sender"].as_str().map(String::from) else {
            return;
        };
        let event_id = ev["event_id"].as_str().unwrap_or_default().to_string();
        let ts = ev["origin_server_ts"].as_u64().unwrap_or_default();

        // Our own puppets' and bot's events are the *relay* of WEFT traffic —
        // re-ingesting them would echo every message back to its author.
        let Some(weft_sender) = self.foreign_user(&sender) else {
            return;
        };

        let Some((room, space)) = self.store.state.channel_of_room(&room_id) else {
            return; // an unmapped room (the space room itself, or noise)
        };
        let (channel, ns_id, realm) = (
            room.channel.clone(),
            space.ns_id.clone(),
            realm_of_uri(&space.uri),
        );

        // The operator banned this space: nothing crosses, either direction.
        if self.store.state.bans.is_banned(&ns_id) {
            return;
        }

        match ev["type"].as_str().unwrap_or_default() {
            "m.room.message" => {
                let content = &ev["content"];

                // An edit travels as m.replace with the full new content.
                let relates = &content["m.relates_to"];
                if relates["rel_type"] == "m.replace" {
                    let Some(root) = relates["event_id"]
                        .as_str()
                        .and_then(|id| self.store.state.links.msgid_of(id))
                        .map(String::from)
                    else {
                        return; // an edit of something we never bridged
                    };
                    let Some(body) = content["m.new_content"]["body"].as_str() else {
                        return;
                    };
                    let Ok(root) = root.parse() else { return };

                    let minted = ident::msgid_for(&realm, &event_id, ts);
                    if let Err(e) = self.realm.edit(&weft_sender, &minted, &root, body).await {
                        warn!(event_id, "edit ingestion failed: {e:#}");
                        return;
                    }
                    self.store.link(&event_id, &minted, &room_id).await;
                    return;
                }

                let Some(body) = content["body"].as_str() else {
                    return;
                };
                let minted = ident::msgid_for(&realm, &event_id, ts);
                if let Err(e) = self
                    .realm
                    .message(&weft_sender, &minted, &channel, body)
                    .await
                {
                    warn!(event_id, "message ingestion failed: {e:#}");
                    return;
                }
                self.store.link(&event_id, &minted, &room_id).await;
            }
            "m.reaction" => {
                let relates = &ev["content"]["m.relates_to"];
                if relates["rel_type"] != "m.annotation" {
                    return;
                }
                let Some(root) = relates["event_id"]
                    .as_str()
                    .and_then(|id| self.store.state.links.msgid_of(id))
                    .map(String::from)
                else {
                    return;
                };
                let Some(key) = relates["key"].as_str() else {
                    return;
                };
                let Ok(root_id) = root.parse() else { return };

                if let Err(e) = self.realm.react(&weft_sender, &root_id, key, true).await {
                    warn!(event_id, "reaction ingestion failed: {e:#}");
                    return;
                }
                self.store
                    .reaction_add(
                        &event_id,
                        crate::store::Reaction {
                            root,
                            key: key.to_string(),
                            by: weft_sender,
                        },
                    )
                    .await;
            }
            "m.room.redaction" => {
                // Room v11 moved `redacts` into content; accept both homes.
                let Some(redacts) = ev["redacts"]
                    .as_str()
                    .or_else(|| ev["content"]["redacts"].as_str())
                    .map(String::from)
                else {
                    return;
                };

                if let Some(r) = self.store.reaction_take(&redacts).await {
                    // Redacting a reaction is an unreact — by the reactor,
                    // whoever redacted (a mod removing it acts on their behalf).
                    let Ok(root) = r.root.parse() else { return };
                    if let Err(e) = self.realm.react(&r.by, &root, &r.key, false).await {
                        warn!(event_id, "unreact ingestion failed: {e:#}");
                    }
                    return;
                }

                let Some(root) = self.store.state.links.msgid_of(&redacts).map(String::from) else {
                    return;
                };
                let Ok(root) = root.parse() else { return };
                if let Err(e) = self.realm.delete(&weft_sender, &root).await {
                    warn!(event_id, "delete ingestion failed: {e:#}");
                }
            }
            "m.room.member" => {
                let Some(subject) = ev["state_key"].as_str() else {
                    return;
                };
                let Some(subject) = self.foreign_user(subject) else {
                    return;
                };
                let joined = ev["content"]["membership"] == "join";

                self.member_change(
                    &space_uri_of(&self.store.state, &room_id),
                    &ns_id,
                    &room_id,
                    &subject,
                    joined,
                )
                .await;
            }
            other => debug!(other, "unbridged Matrix event type"),
        }
    }

    /// §8 membership mapping: [`Space::member_joined`]/[`Space::member_left`]
    /// decide whether this room op is a namespace transition; only a
    /// transition is stated.
    async fn member_change(
        &mut self,
        space_uri: &str,
        ns_id: &str,
        room_id: &str,
        user: &str,
        joined: bool,
    ) {
        let Some(space) = self.store.state.spaces.get_mut(space_uri) else {
            return;
        };

        let action = if joined {
            space.member_joined(user, room_id)
        } else {
            space.member_left(user, room_id)
        };
        self.store
            .persist_member_room(space_uri, user, room_id, joined)
            .await;

        if let Some(action) = action {
            if let Err(e) = self.realm.member(ns_id, user, action).await {
                warn!(user, "membership statement failed: {e:#}");
            }
        }
    }

    // ---- WEFT → Matrix (relayed local events) -------------------------------

    async fn relay_message(&mut self, m: &weft_proto::MessageEvent) -> anyhow::Result<()> {
        let weft_proto::Target::Channel(channel) = &m.target else {
            return Ok(()); // DMs are v2
        };
        let Some((room_id, space)) = self.store.state.room_of_channel(&channel.to_string()) else {
            return Ok(());
        };
        let (room_id, ns_id) = (room_id.to_string(), space.ns_id.clone());

        if self.store.state.bans.is_banned(&ns_id) {
            return Ok(());
        }

        let account = m.sender.account.to_string();
        let Some(puppet) = self.puppet_of_account(&account) else {
            warn!(
                account,
                "no puppet — user never reached this bridge via a membership relay"
            );
            return Ok(());
        };
        let event_id = self
            .hs
            .send(
                &room_id,
                "m.room.message",
                json!({ "msgtype": "m.text", "body": m.body }),
                &txn_of(&m.msgid.to_string()),
                Some(&puppet),
            )
            .await?;

        self.store
            .link(&event_id, &m.msgid.to_string(), &room_id)
            .await;
        Ok(())
    }

    async fn relay_edit(
        &mut self,
        user: &str,
        msgid: &str,
        edit_of: &str,
        body: &str,
    ) -> anyhow::Result<()> {
        let Some(at) = self.store.state.links.event_of(edit_of).cloned() else {
            return Ok(());
        };
        let (room_id, orig_event) = (at.room, at.event);

        let account = account_of(user);
        let Some(puppet) = self.puppet_of_account(&account) else {
            warn!(account, "no puppet for relayed edit — skipped");
            return Ok(());
        };
        let event_id = self
            .hs
            .send(
                &room_id,
                "m.room.message",
                json!({
                    "msgtype": "m.text",
                    // The fallback body for clients that don't render edits.
                    "body": format!("* {body}"),
                    "m.new_content": { "msgtype": "m.text", "body": body },
                    "m.relates_to": { "rel_type": "m.replace", "event_id": orig_event },
                }),
                &txn_of(msgid),
                Some(&puppet),
            )
            .await?;

        self.store.link(&event_id, msgid, &room_id).await;
        Ok(())
    }

    async fn relay_delete(&mut self, by: Option<&str>, msgid: &str) -> anyhow::Result<()> {
        let Some(at) = self.store.state.links.event_of(msgid).cloned() else {
            return Ok(());
        };
        let (room_id, event_id) = (at.room, at.event);

        // The author's puppet redacts their own message; anything else (an
        // operator's admin-panel delete, or an unknown author) is the bot's
        // moderation act.
        let puppet = by.and_then(|by| self.puppet_of_account(&account_of(by)));

        self.hs
            .redact(
                &room_id,
                &event_id,
                None,
                &txn_of(&format!("del-{msgid}")),
                puppet.as_deref(),
            )
            .await?;
        Ok(())
    }

    async fn relay_reaction(
        &mut self,
        by: &str,
        root: &str,
        emoji: &str,
        add: bool,
    ) -> anyhow::Result<()> {
        let account = account_of(by);
        let reaction = crate::store::Reaction {
            root: root.to_string(),
            key: emoji.to_string(),
            by: account.clone(),
        };

        if !add {
            let Some(event_id) = self.store.sent_take(&reaction).await else {
                return Ok(());
            };
            let Some(room_id) = self
                .store
                .state
                .links
                .event_of(root)
                .map(|at| at.room.clone())
            else {
                return Ok(());
            };
            let Some(puppet) = self.puppet_of_account(&account) else {
                return Ok(());
            };
            self.hs
                .redact(
                    &room_id,
                    &event_id,
                    None,
                    &txn_of(&format!("unreact-{event_id}")),
                    Some(&puppet),
                )
                .await?;
            return Ok(());
        }

        let Some(at) = self.store.state.links.event_of(root).cloned() else {
            return Ok(());
        };
        let (room_id, orig_event) = (at.room, at.event);
        let Some(puppet) = self.puppet_of_account(&account) else {
            warn!(account, "no puppet for relayed reaction — skipped");
            return Ok(());
        };
        let event_id = self
            .hs
            .send(
                &room_id,
                "m.reaction",
                json!({ "m.relates_to": {
                    "rel_type": "m.annotation",
                    "event_id": orig_event,
                    "key": emoji,
                }}),
                &txn_of(&format!("react-{root}-{account}")),
                Some(&puppet),
            )
            .await?;

        self.store.sent_note(reaction, event_id).await;
        Ok(())
    }

    // ---- local users acting inside the realm (§8's asks + §6 membership) ---

    async fn puppet_join_namespace(
        &mut self,
        ulid: &str,
        account: &str,
        ns_id: &str,
    ) -> anyhow::Result<()> {
        let Some(space) = self.store.state.space_of_ns(ns_id) else {
            return Ok(());
        };
        let rooms: Vec<String> = space.rooms.keys().cloned().collect();
        let user = format!("{account}@{}", self.realm.network());

        if self.store.state.bans.is_banned(ns_id) {
            return Ok(());
        }

        let puppet = self.ensure_puppet(ulid, account).await?;
        let rooms_total = rooms.len();
        let mut any = false;

        for room in rooms {
            match self.hs.join(&room, &[], Some(&puppet)).await {
                Ok(_) => any = true,
                Err(e) => warn!(room, account, "puppet join failed: {e:#}"),
            }
        }

        // The statement is what makes the membership true weftd-side — sent
        // only once the foreign side actually has them (§6). An **empty**
        // space is the degenerate case: there is nothing to join foreign-side
        // (Matrix space-join is cosmetic, §8), so the membership is true the
        // moment we say so — without this, an empty namespace could never be
        // joined at all.
        if any || rooms_total == 0 {
            self.realm.member(ns_id, &user, MemberAction::Join).await?;
        }
        Ok(())
    }

    async fn puppet_leave_namespace(
        &mut self,
        ulid: &str,
        account: &str,
        ns_id: &str,
    ) -> anyhow::Result<()> {
        let Some(space) = self.store.state.space_of_ns(ns_id) else {
            return Ok(());
        };
        let rooms: Vec<String> = space.rooms.keys().cloned().collect();
        let user = format!("{account}@{}", self.realm.network());

        let puppet = self.ensure_puppet(ulid, account).await?;
        for room in rooms {
            if let Err(e) = self.hs.leave(&room, Some(&puppet)).await {
                debug!(room, account, "puppet leave failed: {e:#}");
            }
        }

        self.realm.member(ns_id, &user, MemberAction::Part).await?;
        Ok(())
    }

    /// A local user's reaction on a realm-minted message: perform it as their
    /// puppet, then **confirm it back through ingestion attributed to them** —
    /// the protocol's return path (bridge-session-protocol §8).
    #[allow(clippy::too_many_arguments)]
    async fn local_reaction(
        &mut self,
        user: &str,
        ulid: &str,
        account: &str,
        root: &str,
        emoji: &str,
        add: bool,
    ) -> anyhow::Result<()> {
        let Some(at) = self.store.state.links.event_of(root).cloned() else {
            return Ok(());
        };
        let (room_id, orig_event) = (at.room, at.event);
        let puppet = self.ensure_puppet(ulid, account).await?;
        let reaction = crate::store::Reaction {
            root: root.to_string(),
            key: emoji.to_string(),
            by: account.to_string(),
        };

        if add {
            let event_id = self
                .hs
                .send(
                    &room_id,
                    "m.reaction",
                    json!({ "m.relates_to": {
                        "rel_type": "m.annotation",
                        "event_id": orig_event,
                        "key": emoji,
                    }}),
                    &txn_of(&format!("react-{root}-{account}")),
                    Some(&puppet),
                )
                .await?;
            self.store.sent_note(reaction, event_id).await;
        } else {
            let Some(event_id) = self.store.sent_take(&reaction).await else {
                return Ok(());
            };
            self.hs
                .redact(
                    &room_id,
                    &event_id,
                    None,
                    &txn_of(&format!("unreact-{event_id}")),
                    Some(&puppet),
                )
                .await?;
        }

        let root_id = root.parse()?;
        self.realm.react(user, &root_id, emoji, add).await
    }

    async fn local_delete(
        &mut self,
        user: &str,
        ulid: &str,
        account: &str,
        root: &str,
    ) -> anyhow::Result<()> {
        let Some(at) = self.store.state.links.event_of(root).cloned() else {
            return Ok(());
        };
        let (room_id, event_id) = (at.room, at.event);

        let puppet = self.ensure_puppet(ulid, account).await?;
        self.hs
            .redact(
                &room_id,
                &event_id,
                None,
                &txn_of(&format!("del-{root}")),
                Some(&puppet),
            )
            .await?;

        let root_id = root.parse()?;
        self.realm.delete(user, &root_id).await
    }

    /// The puppet MXID for one of our users, **keyed by account ULID** (owner
    /// directive 2026-08-06): the localpart derives from the ULID, so a rename
    /// changes nothing on the Matrix side; the name rides along for display.
    /// Registered on first sight.
    async fn ensure_puppet(&mut self, ulid: &str, account: &str) -> anyhow::Result<String> {
        if let Some(user) = self.store.state.users.by_ulid(ulid) {
            let localpart = user.localpart.clone();

            // A rename: same identity, re-point the name index.
            if user.account != account {
                self.store.note_user(ulid, account, &localpart).await;
            }

            return Ok(format!("@{localpart}:{}", self.domain));
        }

        let localpart = ident::puppet_localpart(&self.puppet_prefix, ulid);
        self.hs.ensure_registered(&localpart).await?;
        self.store.note_user(ulid, account, &localpart).await;

        Ok(format!("@{localpart}:{}", self.domain))
    }

    /// Resolve a puppet by wire **name** — for the fan-out events, which carry
    /// no ULID. A miss means this user never reached the bridge through a
    /// membership relay (the only door in), so there is nothing to speak as.
    fn puppet_of_account(&self, account: &str) -> Option<String> {
        self.store
            .state
            .users
            .by_account(account)
            .map(|(_, user)| format!("@{}:{}", user.localpart, self.domain))
    }
}

/// A state event's content field, by event type.
fn state_str(state: &[Value], event_type: &str, field: &str) -> Option<String> {
    state
        .iter()
        .find(|ev| ev["type"] == event_type && ev["state_key"] == "")
        .and_then(|ev| ev["content"][field].as_str())
        .map(String::from)
}

/// A room name as a channel vanity: lowercase, runs of anything unusable
/// collapse to one `-`, trimmed at the edges.
fn vanity_of(name: &str) -> String {
    let mut out = String::new();

    for c in name.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '.') {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }

    out.trim_end_matches('-').chars().take(32).collect()
}

/// A WEFT msgid as a Matrix transaction id (idempotency key): the `/` is the
/// only byte that needs replacing to be path-safe.
fn txn_of(msgid: &str) -> String {
    msgid.replace('/', "_")
}

fn account_of(user: &str) -> String {
    user.split('@').next().unwrap_or(user).to_string()
}

fn realm_of_uri(uri: &str) -> String {
    ident::SpaceRef::parse(uri)
        .map(|s| s.realm)
        .unwrap_or_default()
}

fn space_uri_of(state: &crate::store::State, room_id: &str) -> String {
    state
        .channel_of_room(room_id)
        .map(|(_, space)| space.uri.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vanities_survive_matrix_room_names() {
        assert_eq!(vanity_of("General Chat"), "general-chat");
        assert_eq!(vanity_of("Café ☕ Talk"), "caf-talk");
        assert_eq!(vanity_of("🔥"), "");
    }

    #[test]
    fn txn_ids_are_stable_per_msgid() {
        assert_eq!(txn_of("kde.org/01abc"), "kde.org_01abc");
        assert_eq!(txn_of("kde.org/01abc"), txn_of("kde.org/01abc"));
    }
}
