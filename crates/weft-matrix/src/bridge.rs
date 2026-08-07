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
use weft_proto::{Command, Event, MemberAction, ToastKind};

use crate::asapi::Txn;
use crate::hs::Hs;
use crate::ident;
use crate::ident::MatrixIdentity;
use crate::pending::PendingByLabel;
use crate::store::{EventRef, Room, Space, Store};

pub struct Bridge {
    pub realm: Realm,
    pub hs: Hs,
    pub store: Store,
    /// Our MXID namespace on the companion homeserver: the bot, the puppets, and
    /// the "is this ours?" test that keeps a relay from being re-ingested.
    pub identity: MatrixIdentity,
    /// Projected structure buffered between `CHANNEL-LAYOUT` and its `POLICY`
    /// (the §3 rules need the policy, which travels as a separate event).
    pub pending_layouts: std::collections::HashMap<String, PendingLayout>,
    /// Injections awaiting their labeled echo (§3.5): the Matrix event + room
    /// the minted id must link back to.
    pub pending_injections: PendingByLabel<EventRef>,
    /// §10 revert: attributed acts awaiting weftd's verdict. An `ERR` echoing
    /// the label means WEFT refused — undo the foreign-side change and notice
    /// the actor.
    pub pending_acts: PendingByLabel<PendingAct>,
    /// Open management flows by view-id: which action, on what, for whom.
    pub flows: std::collections::HashMap<String, Flow>,
    /// weftd's HTTP media plane, when configured. `None` ⇒ media is not
    /// bridged, and the daemon says so once rather than per message.
    pub weft_media: Option<crate::media::WeftMedia>,
    /// Blobs downloaded from Matrix and waiting for their upload grant
    /// (`STREAM ACCEPT`).
    pub pending_uploads: PendingByLabel<PendingUpload>,
    /// A monotonic suffix for DM transaction ids. Deliberately *not* one of the
    /// registries above: it parks nothing and resolves nothing, it only has to
    /// keep two DMs in the same second distinct (a DM carries no msgid to key
    /// on). It used to share the upload counter, which coupled two unrelated
    /// sequences for no reason.
    pub dm_txn: u64,
    /// MXIDs allowed to drive the `!weft` console. Empty ⇒ disabled.
    pub admins: Vec<String>,
    /// weftd's local-membership statement, accumulating between its `BATCH
    /// START` and `BATCH END` (ns id → the local accounts it still holds).
    /// Consumed by the reconcile on `BATCH END`; see [`Bridge::reconcile_local_membership`].
    pub local_roster: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
}

/// A Matrix attachment already in hand, waiting for weftd's upload grant so it
/// can be posted and attached to the message it belongs to.
pub struct PendingUpload {
    pub bytes: Vec<u8>,
    pub mime: String,
    /// Everything needed to send the message once the blob has a hash.
    pub sender: String,
    pub channel: String,
    pub body: String,
    pub msgid: String,
    pub event_id: String,
    pub room_id: String,
}

/// An open management flow (slice 11) — the context its next step acts on.
#[derive(Debug, Clone)]
pub struct Flow {
    pub action: String,
    /// The invoking WEFT user; every wire command the flow issues is theirs.
    pub invoker: String,
    /// What the action was invoked on (`ctx_ref`): a namespace, channel or member.
    pub ctx_ref: String,
}

/// A foreign-side change we already made, and how to undo it if WEFT refuses
/// the corresponding WEFT-side act (§10: "revert + notice").
#[derive(Debug, Clone)]
pub enum PendingAct {
    /// A power level was set; restore the previous one (0 = remove).
    Level {
        room: String,
        mxid: String,
        previous: i64,
        actor: String,
    },
    /// A member was banned/kicked. `mxid` is `None` when they have no Matrix
    /// identity to restore — there is then nothing to undo, but the refusal is
    /// still worth reporting, so the act is still parked.
    Membership {
        room: String,
        mxid: Option<String>,
        was_banned: bool,
        actor: String,
    },
}

/// How many events one backfill page fetches. Matrix's `/messages` is
/// paginated far below WEFT's `MAX_HISTORY_LIMIT` (500), and backfill is
/// demand-driven — the client scrolls again for the next window.
const BACKFILL_PAGE: u32 = 50;

/// How long a mirrored typing indicator lives. Matrix expires it server-side,
/// so a `stop` that never arrives (a dropped session) still clears.
const TYPING_TTL_MS: u64 = 20_000;

/// A projected channel's layout, waiting for its retention policy.
pub struct PendingLayout {
    pub vanity: String,
    pub kind: weft_proto::ChannelKind,
    pub position: i64,
    /// The channel's category, if any — its room is parented under that
    /// category's sub-space rather than the top Space (matrix.md §6).
    pub category: Option<String>,
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
            Incoming::Event {
                event,
                label,
                actor_ulid,
            } => self.on_weftd_event(event, label, actor_ulid).await,
            Incoming::Invoke {
                view_id,
                action,
                ctx_ref,
                invoker,
                ..
            } => {
                self.on_invoke(&view_id, &action, ctx_ref.as_deref(), invoker.as_deref())
                    .await
            }
            Incoming::Step {
                view_id,
                button,
                values,
                closed,
            } => {
                self.on_step(&view_id, button.as_deref(), &values, closed)
                    .await
            }
            Incoming::Command {
                as_user,
                as_ulid,
                command,
            } => self.on_weftd_request(as_user, as_ulid, command).await,
        }
    }

    async fn on_weftd_event(
        &mut self,
        event: Event,
        label: Option<String>,
        actor_ulid: Option<String>,
    ) {
        match event {
            Event::Provision { uri, job } => {
                let ok = self.provision(&uri.to_string()).await.unwrap_or_else(|e| {
                    warn!(%uri, "provisioning failed: {e:#}");
                    false
                });
                let _ = self.realm.provisioned(&job, ok).await;
            }
            bridging @ Event::Bridging { .. } => {
                // The row is the enforcement: weftd never re-sends a ban.
                if let Some((ns, banned)) = self.store.apply_bridging(&bridging).await {
                    info!(ns, banned, "bridging instruction from the operator");
                    // weftd sends each ban exactly once and keeps no record, so
                    // ours must outlive our own database (§11).
                    self.persist_bans().await;
                }
            }
            Event::Message(m) => {
                // A labeled echo of our own projected injection: the home
                // minted it, the label correlates, the msgid links (§3.5).
                if let Some(label) = label {
                    if self.link_injection_echo(&label, &m.msgid.to_string()).await {
                        return;
                    }
                }
                if let Err(e) = self.relay_message(&m, actor_ulid.as_deref()).await {
                    warn!(msgid = %m.msgid, "relay to Matrix failed: {e:#}");
                }
            }
            Event::Edited {
                user,
                msgid,
                edit_of,
                body,
                ..
            } => {
                if let Some(label) = label {
                    if self.link_injection_echo(&label, &msgid.to_string()).await {
                        return;
                    }
                }
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
            Event::Deleted { msgid, by, .. } => {
                let by = by.map(|u| u.to_string());
                if let Err(e) = self.relay_delete(by.as_deref(), &msgid.to_string()).await {
                    warn!(%msgid, "delete relay to Matrix failed: {e:#}");
                }
            }
            Event::Reaction {
                msgid,
                emoji,
                op,
                by,
                ..
            } => {
                let add = op == weft_proto::ReactionOp::Add;
                if let Err(e) = self
                    .relay_reaction(&by.to_string(), &msgid.to_string(), &emoji, add)
                    .await
                {
                    warn!(%msgid, "reaction relay to Matrix failed: {e:#}");
                }
            }
            // Outbound projection: weftd describes a projected native
            // namespace (bridges= set, no origin) — mirror it as a Space.
            Event::NsMeta {
                id,
                vanity,
                title,
                categories,
                bridges,
                origin: None,
                ..
            } if bridges.iter().any(|b| b.as_str() == "matrix") => {
                let ns_id = id.to_string();
                if let Err(e) = self
                    .ensure_projection(&ns_id, &vanity.to_string(), title.as_deref())
                    .await
                {
                    warn!(ns = %id, "projecting the Space failed: {e:#}");
                    return;
                }
                // Categories are ns-level and ordered; each becomes a
                // sub-space (matrix.md §6, locked decision 4).
                if let Err(e) = self.ensure_categories(&ns_id, &categories).await {
                    warn!(ns = %id, "projecting the categories failed: {e:#}");
                }
            }
            Event::ChannelLayout {
                channel,
                vanity,
                kind,
                position,
                category,
                origin: None,
                ..
            } => {
                // Buffered until its POLICY arrives — the §3 projection rules
                // need the retention policy, which travels separately.
                self.pending_layouts.insert(
                    channel.to_string(),
                    PendingLayout {
                        vanity,
                        kind,
                        position,
                        category,
                    },
                );
            }
            Event::Policy { channel, policy } => {
                if let Some(layout) = self.pending_layouts.remove(&channel.to_string()) {
                    if let Err(e) = self
                        .ensure_projected_room(&channel.to_string(), layout, policy)
                        .await
                    {
                        warn!(%channel, "projecting the room failed: {e:#}");
                    }
                }
            }
            // §15 one of our members is typing in a bridged channel — mirror it
            // as their puppet's typing EDU. The event names its own user, so
            // there is no `@as` here.
            Event::Typing {
                channel,
                user,
                state,
            } => {
                let user = user.to_string();
                self.relay_typing(&user, actor_ulid.as_deref(), &channel.to_string(), state)
                    .await;
            }
            // §13 the upload grant for a blob we are holding.
            Event::StreamAccept { token } => {
                if let Some(label) = label {
                    self.finish_attachment_upload(&label, &token).await;
                }
            }
            // §10: a refusal of one of our attributed acts — undo the
            // foreign-side change and notice the actor.
            Event::Err(err) => {
                if let Some(label) = label {
                    self.revert_act(&label, &err.text).await;
                } else {
                    debug!(code = %err.code, text = %err.text, "unlabeled ERR from weftd");
                }
            }
            // weftd stating its own local membership of the spaces we consume —
            // the half of the reconcile only it can know (§7a). Rows accumulate;
            // the `ni…` BATCH END is what says the statement is complete.
            Event::NsMemberInfo {
                namespace, user, ..
            } => {
                self.local_roster
                    .entry(namespace.to_string())
                    .or_default()
                    .insert(user.account.to_string());
            }
            Event::BatchEnd { id, .. } if id.starts_with("ni") => {
                self.reconcile_local_membership().await;
            }
            // Structure acks for replicas we asserted ourselves: nothing to do.
            Event::NsMeta { .. } | Event::ChannelLayout { .. } => {}
            other => debug!(?other, "unhandled weftd event"),
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
        // Authority relays (a WEFT-side GRANT/REVOKE inside a bridged
        // namespace) arrive bare — weftd tells us the fact; the level is ours
        // to compute (§7: the adapter owns the mapping).
        match &command {
            Command::Grant {
                subject,
                scope,
                caps,
                ..
            } => {
                let (subject, scope, caps) = (subject.clone(), scope.clone(), caps.clone());
                let level = crate::levels::level_for_grant(&caps);
                self.apply_level_outbound(&scope, &subject, as_ulid.as_deref(), level)
                    .await;
                return;
            }
            Command::Revoke { subject, scope, .. } => {
                let (subject, scope) = (subject.clone(), scope.clone());
                self.apply_level_outbound(&scope, &subject, as_ulid.as_deref(), 0)
                    .await;
                return;
            }
            // Backfill is a request about a *channel*, not on anyone's behalf
            // (protocol doc §8), so it carries no `@as` either.
            Command::History {
                target: weft_proto::Target::Channel(channel),
                before,
                limit,
                ..
            } => {
                let (channel, before, limit) = (channel.to_string(), before.clone(), *limit);
                if let Err(e) = self.backfill(&channel, before.as_ref(), limit).await {
                    warn!(channel, "backfill failed: {e:#}");
                }
                return;
            }
            _ => {}
        }

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
            // §5 a local user's DM to one of the realm's users: weftd stored and
            // echoed it locally, and hands us the copy for the only route that
            // can reach them.
            Command::Msg {
                target:
                    weft_proto::Target::User {
                        account: peer,
                        network: Some(network),
                    },
                body,
                ..
            } => {
                let peer = format!("{peer}@{network}");
                if let Err(e) = self
                    .relay_dm(&ulid, &account, &peer, body.as_deref().unwrap_or_default())
                    .await
                {
                    warn!(user, peer, "DM relay failed: {e:#}");
                }
            }
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
        //
        // `via` is carried through, not just filtered on: joining a room by ID is
        // only possible with server hints (`?server_name=`), because an ID says
        // nothing about who has the room. Dropping them worked for local rooms —
        // our own homeserver already knows those — and failed every child of a
        // *remote* space, which is the interesting case.
        let mut children: Vec<(String, String, Vec<String>)> = state
            .iter()
            .filter(|ev| ev["type"] == "m.space.child")
            .filter_map(|ev| {
                let via: Vec<String> = ev["content"]["via"]
                    .as_array()?
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect();
                if via.is_empty() {
                    return None; // §space spec: a child without `via` is not joinable
                }

                let room = ev["state_key"].as_str()?.to_string();
                let order = ev["content"]["order"].as_str().unwrap_or("").to_string();
                Some((order, room, via))
            })
            .collect();
        children.sort();

        let mut space = Space {
            ns_id: ns_id.clone(),
            room_id: space_room.clone(),
            uri: space_uri.clone(),
            ..Space::default()
        };

        for (position, (_, child, via)) in children.iter().enumerate() {
            match self
                .provision_room(&space_ref, &ns_id, child, via, position as i64)
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

        // Record the consumed Space so recovery can tell it from a projected one —
        // the two are restored differently (its channels come from the realm's
        // assertions, ours from weftd's structure).
        //
        // In the bot's ACCOUNT DATA, not the room's state: a state event needs power
        // level 50 and we join someone else's Space at 0, so the state write is
        // refused every single time. Account data is ours and always writable, which
        // is why the ban list lives there as well.
        if let Err(e) = self
            .record_consumed_space(&space_room, &ns_id, &space_uri)
            .await
        {
            warn!(space_room, "could not record the consumed Space: {e:#}");
        }

        // Best effort on top: where we *do* have the power (a Space whose admin
        // promoted the bot), the marker in room state is visible and portable. A
        // refusal here is the normal case and says nothing, so it is not a warning.
        if let Err(e) = self
            .hs
            .put_state(
                &space_room,
                crate::recover::SPACE_MARKER,
                "",
                json!({ "kind": "consumed", "ns": ns_id, "uri": space_uri }),
            )
            .await
        {
            // Best-effort: a space we cannot mark still bridges, it just needs
            // Expected without power level 50 — the account-data record above is
            // what recovery actually reads.
            debug!(
                space_room,
                "no room-state marker on the consumed Space: {e:#}"
            );
        }

        self.store.save_space(space).await;
        Ok(true)
    }

    /// Send as a puppet, joining the room first if the puppet isn't in it yet.
    ///
    /// A puppet joins a namespace's rooms when its user joins the namespace — but the
    /// set of rooms changes afterwards. A room added to a Space later, a namespace
    /// joined while the Space was still empty, a puppet minted after the fact: each
    /// leaves a puppet outside a room it now needs to speak in, and Matrix answers
    /// `M_FORBIDDEN … not in room`. Rather than enumerate the orderings, recover from
    /// the one condition they all produce.
    async fn send_as_puppet(
        &mut self,
        room_id: &str,
        puppet: &str,
        content: Value,
        txn: &str,
    ) -> anyhow::Result<String> {
        match self
            .hs
            .send(
                room_id,
                "m.room.message",
                content.clone(),
                txn,
                Some(puppet),
            )
            .await
        {
            Ok(event_id) => Ok(event_id),
            Err(e) if format!("{e:#}").contains("not in room") => {
                info!(room_id, puppet, "puppet was not in the room — joining");
                // The Space first: a restricted room admits Space members, so
                // joining the child directly is refused with "you do not belong
                // to any of the required rooms/spaces" — a *different* 403 than
                // the one we are recovering from, and the reason this recovery
                // used to fail on exactly the rooms it was written for.
                if let Some((_, space)) = self.store.state.channel_of_room(room_id) {
                    let (space_room, space_uri) = (space.room_id.clone(), space.uri.clone());
                    if let Err(e) = self
                        .puppet_join_space(&space_room, &space_uri, puppet)
                        .await
                    {
                        debug!(space_room, puppet, "puppet space join failed: {e:#}");
                    }
                }

                let via = self.room_via(room_id).await;
                self.hs.join(room_id, &via, Some(puppet)).await?;

                self.hs
                    .send(room_id, "m.room.message", content, txn, Some(puppet))
                    .await
            }
            Err(e) => Err(e),
        }
    }

    /// Servers to join `room` through, from its `m.space.child` event in the Space.
    ///
    /// A room ID cannot be joined without them once the room is on another server —
    /// and a v12+ room ID carries no server part at all, so there is nothing to fall
    /// back on. The bot is in the Space, so reading the child event is always allowed.
    async fn room_via(&self, room: &str) -> Vec<String> {
        let Some((_, space)) = self.store.state.channel_of_room(room) else {
            return Vec::new();
        };

        self.hs
            .get_state(&space.room_id, "m.space.child", room)
            .await
            .ok()
            .flatten()
            .and_then(|c| c["via"].as_array().cloned())
            .map(|via| {
                via.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Add this Space to the bot's account-data list of consumed Spaces.
    ///
    /// Read-modify-write: account data is one document per key, so appending means
    /// merging with what is there. Keyed by room id, which is what recovery walks.
    async fn record_consumed_space(
        &self,
        space_room: &str,
        ns_id: &str,
        space_uri: &str,
    ) -> anyhow::Result<()> {
        let bot = self.identity.bot_mxid();
        let mut spaces = self
            .hs
            .account_data(&bot, crate::recover::CONSUMED_KEY)
            .await?
            .and_then(|d| d.get("spaces").cloned())
            .and_then(|s| s.as_object().cloned())
            .unwrap_or_default();

        spaces.insert(
            space_room.to_string(),
            json!({ "ns": ns_id, "uri": space_uri }),
        );

        self.hs
            .set_account_data(
                &bot,
                crate::recover::CONSUMED_KEY,
                json!({ "spaces": spaces }),
            )
            .await
    }

    /// A child added to (or removed from) a consumed Space, live.
    ///
    /// `m.space.child` with a non-empty `via` adds; empty content removes (the
    /// Matrix spec's tombstone for a child). The added room is provisioned exactly
    /// as it would have been during `provision`, so a Space that was empty when it
    /// was consumed still grows channels.
    async fn on_space_child(&mut self, space_room: &str, ev: &Value) {
        let Some(space) = self
            .store
            .state
            .spaces
            .values()
            .find(|s| s.room_id == space_room)
            .cloned()
        else {
            return; // not a space we consume
        };

        // Same gate the rest of the ingest path applies.
        if self.store.state.bans.is_banned(&space.ns_id) {
            return;
        }

        let Some(child) = ev["state_key"].as_str() else {
            return;
        };
        let via: Vec<String> = ev["content"]["via"]
            .as_array()
            .map(|via| {
                via.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        let mut space = space;

        if via.is_empty() {
            // Removed from the space. Stop tracking it; the channel itself is
            // weftd's to retire, and a re-add re-provisions cleanly.
            if space.rooms.remove(child).is_some() {
                info!(child, ns = %space.ns_id, "child removed from consumed Space");
                self.store.save_space(space).await;
            }
            return;
        }

        if space.rooms.contains_key(child) {
            return; // already bridged — an order change, or a repeated statement
        }

        let Some(space_ref) = ident::SpaceRef::parse(&space.uri) else {
            return;
        };
        // Appended: the child's `order` orders it among siblings, but a channel's
        // position only has to be stable and distinct, and re-asserting every
        // sibling to make room would be a lot of churn for a cosmetic ordering.
        let position = space.rooms.len() as i64;

        match self
            .provision_room(&space_ref, &space.ns_id.clone(), child, &via, position)
            .await
        {
            Ok(Some((room, members))) => {
                for member in members {
                    space
                        .member_rooms
                        .entry(member)
                        .or_default()
                        .insert(child.to_string());
                }
                space.rooms.insert(child.to_string(), room);

                let ns_id = space.ns_id.clone();
                let users: Vec<String> = space.member_rooms.keys().cloned().collect();
                self.store.save_space(space).await;

                for user in users {
                    let _ = self.realm.member(&ns_id, &user, MemberAction::Join).await;
                }

                info!(child, ns = %ns_id, "child added to consumed Space — bridged");
            }
            // Encrypted or unjoinable: absent, not fatal (locked decision 7).
            Ok(None) => {}
            Err(e) => warn!(child, "provisioning an added child failed: {e:#}"),
        }
    }

    /// Join + assert one child room. `None` = deliberately not bridgeable.
    async fn provision_room(
        &mut self,
        space_ref: &ident::SpaceRef,
        ns_id: &str,
        room_id: &str,
        // The `via` servers from the space's `m.space.child` event. Required to join
        // by room ID at all when the room is not on our own homeserver.
        via: &[String],
        position: i64,
    ) -> anyhow::Result<Option<(Room, BTreeSet<String>)>> {
        self.hs.join(room_id, via, None).await?;
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

        if self.identity.is_ours(parsed) {
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

        // An operator's console line is a command wherever it is typed — and it
        // is checked before any room mapping, so `recover` works precisely when
        // the mappings are gone.
        if self.admins.iter().any(|a| a == &sender) {
            if let Some(body) = ev["content"]["body"].as_str() {
                if let Some(command) = crate::admin::parse(body) {
                    self.run_console(&room_id, command).await;
                    return;
                }
            }
        }

        // A bridged DM room is neither a replica nor a projection: it carries
        // one conversation, stored in the ordinary DM scope.
        if let Some((account, mxid)) = self
            .store
            .state
            .dm_of_room(&room_id)
            .map(|(a, m)| (a.to_string(), m.to_string()))
        {
            self.on_dm_event(&ev, &account, &mxid).await;
            return;
        }

        // A projected room takes the injection path — the home mints there,
        // so it is a different wire shape from replica ingestion.
        if let Some((channel, ns_id)) = self
            .store
            .state
            .channel_of_projected_room(&room_id)
            .map(|(c, n)| (c.to_string(), n.to_string()))
        {
            self.on_projected_matrix_event(&ev, &room_id, &channel, &ns_id, &weft_sender)
                .await;
            return;
        }

        // A `m.space.child` arrives in the SPACE room, which is never a mapped
        // channel — so it has to be handled before the lookup below drops it as
        // noise. This is how a room added to a consumed Space after provisioning
        // becomes a channel; without it the Space stays as it was at provision time,
        // which for an empty Space means forever.
        if ev["type"] == "m.space.child" {
            self.on_space_child(&room_id, &ev).await;
            return;
        }

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

                // An attachment must exist on our side *before* the message
                // references it, so the blob round trip comes first and the
                // MSG is sent when the grant lands (§12).
                if let Some((mxc, mime, _)) = crate::media::attachment_of(content) {
                    self.begin_attachment_upload(
                        &mxc,
                        &mime,
                        crate::media::PendingParts {
                            sender: weft_sender,
                            channel,
                            body: body.to_string(),
                            msgid: minted,
                            event_id,
                            room_id,
                        },
                    )
                    .await;
                    return;
                }

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
            "m.room.power_levels" => {
                let (room_id, ns_id, sender) =
                    (room_id.clone(), ns_id.clone(), weft_sender.clone());
                self.on_power_levels_event(&ev, &room_id, &ns_id, &sender)
                    .await;
            }
            "m.room.member" => {
                let (chan, ns, sender) = (channel.clone(), ns_id.clone(), weft_sender.clone());
                self.on_member_moderation(&ev, &chan, &ns, &sender).await;

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

    /// A Matrix user's event in a **projected** room: the injection path.
    /// The home mints, so a post carries no msgid — only a label whose echo
    /// links the minted id back (§3.5); mutations name home-minted roots.
    async fn on_projected_matrix_event(
        &mut self,
        ev: &Value,
        room_id: &str,
        channel: &str,
        ns_id: &str,
        weft_sender: &str,
    ) {
        if self.store.state.bans.is_banned(ns_id) {
            return;
        }
        let event_id = ev["event_id"].as_str().unwrap_or_default().to_string();

        match ev["type"].as_str().unwrap_or_default() {
            "m.room.message" => {
                let content = &ev["content"];

                let relates = &content["m.relates_to"];
                if relates["rel_type"] == "m.replace" {
                    let Some(root) = relates["event_id"]
                        .as_str()
                        .and_then(|id| self.store.state.links.msgid_of(id))
                        .map(String::from)
                    else {
                        return;
                    };
                    let Some(body) = content["m.new_content"]["body"].as_str() else {
                        return;
                    };
                    let Ok(root) = root.parse() else { return };

                    let label = self.mint_injection_label(&event_id, room_id);
                    if let Err(e) = self
                        .realm
                        .inject_edit(weft_sender, &root, body, &label)
                        .await
                    {
                        warn!(event_id, "projected edit injection failed: {e:#}");
                        self.pending_injections.forget(&label);
                    }
                    return;
                }

                let Some(body) = content["body"].as_str() else {
                    return;
                };
                let label = self.mint_injection_label(&event_id, room_id);
                if let Err(e) = self
                    .realm
                    .inject_message(weft_sender, channel, body, &label)
                    .await
                {
                    warn!(event_id, "projected injection failed: {e:#}");
                    self.pending_injections.forget(&label);
                }
            }
            // Reactions and redactions carry no minted id anywhere, so the
            // replica helpers apply verbatim — the roots are home-minted and
            // resolved through the same links.
            "m.reaction" => {
                let relates = &ev["content"]["m.relates_to"];
                if relates["rel_type"] != "m.annotation" {
                    return;
                }
                let (Some(root), Some(key)) = (
                    relates["event_id"]
                        .as_str()
                        .and_then(|id| self.store.state.links.msgid_of(id))
                        .map(String::from),
                    relates["key"].as_str(),
                ) else {
                    return;
                };
                let Ok(root_id) = root.parse() else { return };

                if let Err(e) = self.realm.react(weft_sender, &root_id, key, true).await {
                    warn!(event_id, "projected reaction failed: {e:#}");
                    return;
                }
                self.store
                    .reaction_add(
                        &event_id,
                        crate::store::Reaction {
                            root,
                            key: key.to_string(),
                            by: weft_sender.to_string(),
                        },
                    )
                    .await;
            }
            "m.room.redaction" => {
                let Some(redacts) = ev["redacts"]
                    .as_str()
                    .or_else(|| ev["content"]["redacts"].as_str())
                    .map(String::from)
                else {
                    return;
                };

                if let Some(r) = self.store.reaction_take(&redacts).await {
                    let Ok(root) = r.root.parse() else { return };
                    if let Err(e) = self.realm.react(&r.by, &root, &r.key, false).await {
                        warn!(event_id, "projected unreact failed: {e:#}");
                    }
                    return;
                }

                let Some(root) = self.store.state.links.msgid_of(&redacts).map(String::from) else {
                    return;
                };
                let Ok(root) = root.parse() else { return };
                if let Err(e) = self.realm.delete(weft_sender, &root).await {
                    warn!(event_id, "projected delete failed: {e:#}");
                }
            }
            "m.room.power_levels" => {
                self.on_power_levels_event(ev, room_id, ns_id, weft_sender)
                    .await;
            }
            "m.room.member" => {
                // Moderation of a puppet translates (§10); everything else is
                // roster flow.
                self.on_member_moderation(ev, channel, ns_id, weft_sender)
                    .await;

                let Some(subject) = ev["state_key"].as_str() else {
                    return;
                };
                let Some(subject) = self.foreign_user(subject) else {
                    return;
                };
                let joined = ev["content"]["membership"] == "join";

                let Some(projection) = self.store.state.projections.get_mut(ns_id) else {
                    return;
                };
                let action = if joined {
                    projection.member_joined(&subject, room_id)
                } else {
                    projection.member_left(&subject, room_id)
                };

                // First projected-room join IS the namespace join (§8, run in
                // the outbound sense); weftd accepts the statement because the
                // namespace is flagged for our scheme.
                if let Some(action) = action {
                    if let Err(e) = self.realm.member(ns_id, &subject, action).await {
                        warn!(subject, "projected membership statement failed: {e:#}");
                    }
                }
            }
            other => debug!(other, "unbridged Matrix event type (projected room)"),
        }
    }

    /// A fresh injection label, parked with what its echo must link.
    fn mint_injection_label(&mut self, event_id: &str, room_id: &str) -> String {
        self.pending_injections.park(EventRef {
            room: room_id.to_string(),
            event: event_id.to_string(),
        })
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

    async fn relay_message(
        &mut self,
        m: &weft_proto::MessageEvent,
        actor_ulid: Option<&str>,
    ) -> anyhow::Result<()> {
        let weft_proto::Target::Channel(channel) = &m.target else {
            return Ok(()); // DMs are v2
        };

        // A foreign-sender event on our session originated on the Matrix side
        // (only we can put foreign senders there) — relaying it back would
        // echo every bridged message into its own room.
        if m.sender.network.as_str() != self.realm.network() {
            return Ok(());
        }

        // Consumed replica or outbound projection — one relay, two maps.
        let channel = channel.to_string();
        let (room_id, ns_id) =
            if let Some((room, space)) = self.store.state.room_of_channel(&channel) {
                (room.to_string(), space.ns_id.clone())
            } else if let Some((ns, room)) = self.store.state.projected_room_of_channel(&channel) {
                (room.to_string(), ns.to_string())
            } else {
                return Ok(());
            };

        if self.store.state.bans.is_banned(&ns_id) {
            return Ok(());
        }

        let account = m.sender.account.to_string();
        // The stamped ULID registers the puppet on first sight; the name index
        // covers events that predate the stamp.
        let puppet = match actor_ulid {
            Some(ulid) => self.ensure_puppet(ulid, &account).await?,
            None => match self.puppet_of_account(&account) {
                Some(puppet) => puppet,
                None => {
                    warn!(account, "no puppet and no ulid= on the event — skipped");
                    return Ok(());
                }
            },
        };
        let content = json!({
            "msgtype": "m.text",
            "body": m.body,
            // The id we minted, carried on the event itself: this is what makes the
            // link map rebuildable after a database loss (see `recover`) — an
            // ingested message's id is already derivable, ours would not be.
            crate::recover::MSGID_FIELD: m.msgid.to_string(),
        });
        let event_id = self
            .send_as_puppet(&room_id, &puppet, content, &txn_of(&m.msgid.to_string()))
            .await?;

        self.store
            .link(&event_id, &m.msgid.to_string(), &room_id)
            .await;

        // §12 each blob becomes its own Matrix event — one attachment per event
        // is all Matrix carries.
        self.relay_attachments(&room_id, &puppet, &m.msgid.to_string(), &m.meta.attachments)
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
        let (space_room, space_uri) = (space.room_id.clone(), space.uri.clone());
        let user = format!("{account}@{}", self.realm.network());

        if self.store.state.bans.is_banned(ns_id) {
            return Ok(());
        }

        let puppet = self.ensure_puppet(ulid, account).await?;

        // Joining the namespace here **is** joining the Space there. Not
        // decoration: a restricted child room (`join_rule: restricted`)
        // authorizes by Space membership, so a puppet outside the Space is
        // refused every single child with `M_FORBIDDEN … do not belong to any of
        // the required rooms/spaces`. So the Space comes first — but its failure
        // must not *abort* the join, or one unjoinable Space costs the user the
        // whole namespace, including rooms they could still have entered.
        let space_joined = match self
            .puppet_join_space(&space_room, &space_uri, &puppet)
            .await
        {
            Ok(()) => true,
            Err(e) => {
                warn!(
                    space_room,
                    account,
                    "puppet could not join the Space — restricted children will refuse it: {e:#}"
                );

                false
            }
        };

        let rooms_total = rooms.len();
        let mut rooms_joined = false;

        for room in rooms {
            // With `via`: a room on another server cannot be joined by ID alone.
            let via = self.room_via(&room).await;
            match self.hs.join(&room, &via, Some(&puppet)).await {
                Ok(_) => rooms_joined = true,
                // A single unjoinable channel is not a failed namespace join —
                // it may be invite-only, or restricted to a different Space.
                Err(e) => warn!(room, account, "puppet join failed: {e:#}"),
            }
        }

        // The statement is what makes the membership true weftd-side (§6), and it
        // is only honest once the user is *somewhere* foreign-side: in the Space,
        // or in one of its rooms, or there is nothing to be in (an empty space —
        // without which an empty namespace could never be joined at all).
        if !space_joined && !rooms_joined && rooms_total > 0 {
            warn!(
                account,
                ns = %ns_id,
                "namespace join asserts nothing — the foreign side refused the Space and every room"
            );

            return Ok(());
        }

        self.realm.member(ns_id, &user, MemberAction::Join).await?;

        Ok(())
    }

    /// Retire the puppets of users who are no longer members weftd-side.
    ///
    /// weftd applies an `NS LEAVE` whether or not we are connected, and its pushes
    /// are live-only, so one that happened while this daemon was down never
    /// reached us: the puppet stays in the Space and its rooms, and Matrix users
    /// keep seeing a member who left. The statement weftd sends on registration is
    /// the whole of what it holds, so anyone joined foreign-side and *absent* from
    /// it has left — including the case where a namespace's local roster is now
    /// empty, which is why an unmentioned namespace reconciles against an empty
    /// set rather than being skipped.
    ///
    /// Only our own puppets are touched. A foreign member of the space is not ours
    /// to remove, and the bot has to stay to keep reading the Space.
    async fn reconcile_local_membership(&mut self) {
        let statement = std::mem::take(&mut self.local_roster);
        let spaces: Vec<(String, String, Vec<String>)> = self
            .store
            .state
            .spaces
            .values()
            .map(|space| {
                (
                    space.ns_id.clone(),
                    space.room_id.clone(),
                    space.rooms.keys().cloned().collect(),
                )
            })
            .collect();

        for (ns_id, space_room, rooms) in spaces {
            let members = statement.get(&ns_id);
            let Ok(state) = self.hs.state(&space_room).await else {
                continue; // a Space we cannot read is one we cannot reconcile
            };

            let departed: Vec<(String, String)> = state
                .iter()
                .filter(|ev| ev["type"] == "m.room.member")
                .filter(|ev| ev["content"]["membership"] == "join")
                .filter_map(|ev| ev["state_key"].as_str())
                .filter_map(|mxid| {
                    let account = self.puppet_account_of(mxid)?;
                    let still_a_member = members.is_some_and(|m| m.contains(&account));

                    (!still_a_member).then(|| (mxid.to_string(), account))
                })
                .collect();

            for (puppet, account) in departed {
                info!(
                    account,
                    ns = %ns_id,
                    "local member left while we were away — retiring their puppet"
                );

                for room in &rooms {
                    if let Err(e) = self.hs.leave(room, Some(&puppet)).await {
                        debug!(room, account, "reconcile room leave failed: {e:#}");
                    }
                }
                if let Err(e) = self.hs.leave(&space_room, Some(&puppet)).await {
                    debug!(space_room, account, "reconcile space leave failed: {e:#}");
                }
            }
        }
    }

    /// Join a puppet to a Space, resolving the servers to join through.
    ///
    /// A Space is joined by ID like any room, so it needs `via` servers when it
    /// lives on another homeserver — and a v12+ room ID carries no server part to
    /// fall back on. The alias is the one handle that resolves to both.
    async fn puppet_join_space(
        &self,
        space_room: &str,
        space_uri: &str,
        puppet: &str,
    ) -> anyhow::Result<()> {
        let via = match ident::SpaceRef::parse(space_uri) {
            Some(space_ref) => self
                .hs
                .resolve_alias(&space_ref.alias())
                .await
                .map(|(_, servers)| servers)
                .unwrap_or_default(),
            None => Vec::new(),
        };

        self.hs.join(space_room, &via, Some(puppet)).await?;

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
        let space_room = space.room_id.clone();
        let user = format!("{account}@{}", self.realm.network());

        let puppet = self.ensure_puppet(ulid, account).await?;
        for room in rooms {
            if let Err(e) = self.hs.leave(&room, Some(&puppet)).await {
                debug!(room, account, "puppet leave failed: {e:#}");
            }
        }

        // Symmetric with the join: the Space membership *is* the namespace
        // membership foreign-side, so leaving it behind would leave the user
        // still in the Space — and still able to re-enter its restricted rooms.
        if let Err(e) = self.hs.leave(&space_room, Some(&puppet)).await {
            debug!(space_room, account, "puppet space leave failed: {e:#}");
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

    // ---- the operator console (owner requirement 2026-08-06) ---------------

    /// Run one `!weft` command and answer in the room it was typed in.
    ///
    /// Every answer goes back as an `m.notice` — an operator who asks a question
    /// gets a reply even when the command failed, because a console that
    /// sometimes says nothing is worse than no console.
    async fn run_console(&mut self, room_id: &str, command: Result<crate::admin::Command, String>) {
        use crate::admin::Command;

        let reply = match command {
            Err(usage) => usage,
            Ok(Command::Help) => crate::admin::HELP.to_string(),
            Ok(Command::Status) => self.console_status(),
            Ok(Command::Recover) => match self.recover().await {
                Ok(found) => format!("recovered {found}"),
                Err(e) => format!("recovery failed: {e:#}"),
            },
            Ok(Command::AttachPuppet {
                mxid,
                ulid,
                account,
            }) => {
                self.console_attach_puppet(&mxid, &ulid, account.as_deref())
                    .await
            }
            Ok(Command::AttachDm { account, mxid }) => {
                self.console_attach_dm(room_id, &account, &mxid).await
            }
        };

        let txn = txn_of(&format!("console-{}", self.next_dm_txn()));
        if let Err(e) = self
            .hs
            .send(
                room_id,
                "m.room.message",
                json!({ "msgtype": "m.notice", "body": reply }),
                &txn,
                None,
            )
            .await
        {
            warn!(room_id, "console reply failed: {e:#}");
        }
    }

    fn console_status(&self) -> String {
        let state = &self.store.state;
        let rooms: usize = state.spaces.values().map(|s| s.rooms.len()).sum();
        let projected: usize = state.projections.values().map(|p| p.rooms.len()).sum();

        format!(
            "consumed: {} space(s), {rooms} room(s)\n             projected: {} namespace(s), {projected} room(s)\n             DMs: {}   puppets: {}   links: {}\n             banned from bridging: {}",
            state.spaces.len(),
            state.projections.len(),
            state.dm_rooms.len(),
            state.users.iter().count(),
            state.links.len(),
            state.bans.iter().count(),
        )
    }

    /// Re-point a puppet by hand. The ULID is the identity, so that is what is
    /// recorded; the name is a label the next relay would fix anyway.
    async fn console_attach_puppet(
        &mut self,
        mxid: &str,
        ulid: &str,
        account: Option<&str>,
    ) -> String {
        let Some(localpart) = mxid
            .strip_prefix('@')
            .and_then(|m| m.split_once(':'))
            .filter(|(_, server)| *server == self.identity.domain())
            .map(|(local, _)| local.to_string())
        else {
            return format!(
                "{mxid} is not a puppet on {} — nothing to attach",
                self.identity.domain()
            );
        };

        let account = account.unwrap_or(ulid);
        self.store.note_user(ulid, account, &localpart).await;

        format!("attached {mxid} to {account} ({ulid})")
    }

    /// Re-point the room the command was typed in as a DM. Deliberately uses
    /// *this* room rather than taking a room id: an operator can see which room
    /// they are in, and a mistyped id would silently hijack another conversation.
    async fn console_attach_dm(&mut self, room_id: &str, account: &str, mxid: &str) -> String {
        if let Err(e) = self
            .hs
            .put_state(
                room_id,
                crate::recover::DM_MARKER,
                "",
                json!({ "account": account, "mxid": mxid }),
            )
            .await
        {
            return format!("could not mark the room: {e:#}");
        }
        self.store.save_dm_room(account, mxid, room_id).await;

        format!("attached this room as the DM between {account} and {mxid}")
    }

    // ---- DMs and typing (matrix.md §15, protocol doc §5) -------------------

    /// WEFT → Matrix: carry a local user's DM into a Matrix DM room, opening
    /// one on first use. Sent as their puppet, so it reads as coming from them.
    async fn relay_dm(
        &mut self,
        ulid: &str,
        account: &str,
        peer: &str,
        body: &str,
    ) -> anyhow::Result<()> {
        let Some(mxid) = ident::mxid_of_weft_user(peer) else {
            anyhow::bail!("{peer} has no Matrix identity");
        };
        let puppet = self.ensure_puppet(ulid, account).await?;

        let room = match self
            .store
            .state
            .dm_rooms
            .get(&(account.to_string(), mxid.clone()))
        {
            Some(room) => room.clone(),
            None => {
                // `is_direct` + the invite is what makes clients render it as a
                // DM rather than a tiny room; created **as the puppet**, so the
                // conversation belongs to the two of them.
                let room = self
                    .hs
                    .create_room_as(
                        json!({
                            "is_direct": true,
                            "preset": "trusted_private_chat",
                            "invite": [mxid],
                        }),
                        Some(&puppet),
                    )
                    .await?;
                // Same idea as the Space marker: a DM room says whose it is,
                // so it can be re-attached without a database.
                if let Err(e) = self
                    .hs
                    .put_state(
                        &room,
                        crate::recover::DM_MARKER,
                        "",
                        json!({ "account": account, "mxid": mxid }),
                    )
                    .await
                {
                    warn!(room, "could not mark the DM room: {e:#}");
                }

                self.store.save_dm_room(account, &mxid, &room).await;
                room
            }
        };

        let txn = txn_of(&format!("dm-{account}-{}", self.next_dm_txn()));
        self.hs
            .send(
                &room,
                "m.room.message",
                json!({ "msgtype": "m.text", "body": body }),
                &txn,
                Some(&puppet),
            )
            .await?;

        Ok(())
    }

    /// A Matrix message in a bridged **DM** room: ingest it as an ordinary WEFT
    /// DM (`Scope::Dm`), keyed by the realm's msgid like any other ingest.
    async fn on_dm_event(&mut self, ev: &Value, account: &str, mxid: &str) {
        if ev["type"] != "m.room.message" {
            return; // v1 bridges DM text; edits/reactions ride the channel path
        }
        let (Some(event_id), Some(body)) = (
            ev["event_id"].as_str().map(String::from),
            ev["content"]["body"].as_str(),
        ) else {
            return;
        };
        if self.store.state.links.msgid_of(&event_id).is_some() {
            return; // already ingested
        }
        let Some(sender) = ev["sender"].as_str().filter(|s| *s == mxid) else {
            return; // our own puppet's copy, or a third party in a "DM"
        };
        let Some(weft_sender) = self.foreign_user(sender) else {
            return;
        };

        let realm = weft_sender
            .split('@')
            .nth(1)
            .unwrap_or_default()
            .to_string();
        let ts = ev["origin_server_ts"].as_u64().unwrap_or_default();
        let minted = ident::msgid_for(&realm, &event_id, ts);

        if let Err(e) = self.realm.dm(&weft_sender, &minted, account, body).await {
            warn!(event_id, "DM ingestion failed: {e:#}");
            return;
        }

        let room = ev["room_id"].as_str().unwrap_or_default().to_string();
        self.store.link(&event_id, &minted, &room).await;
    }

    /// §15 mirror a local member's typing as their puppet's typing EDU.
    async fn relay_typing(
        &mut self,
        user: &str,
        ulid: Option<&str>,
        channel: &str,
        state: weft_proto::TypingState,
    ) {
        let Some(room) = self.room_of_channel(channel) else {
            return;
        };
        let account = account_of(user);
        let puppet = match ulid {
            Some(ulid) => self.ensure_puppet(ulid, &account).await.ok(),
            None => self.puppet_of_account(&account),
        };
        let Some(puppet) = puppet else {
            return; // nobody to type as
        };

        let typing = state == weft_proto::TypingState::Start;
        if let Err(e) = self
            .hs
            .typing(
                &room,
                &puppet,
                typing,
                if typing { TYPING_TTL_MS } else { 0 },
            )
            .await
        {
            debug!(channel, "typing relay failed: {e:#}");
        }
    }

    /// A monotonic suffix so two DMs in the same second get distinct txn ids
    /// (a DM carries no msgid we could key on — weftd minted it locally).
    fn next_dm_txn(&mut self) -> u64 {
        self.dm_txn += 1;
        self.dm_txn
    }

    // ---- media (matrix.md §12) ---------------------------------------------

    /// Matrix → WEFT: download the blob, then ask weftd for an upload grant.
    /// The message waits — a reference to a blob weftd does not hold yet would
    /// render as a broken attachment.
    async fn begin_attachment_upload(
        &mut self,
        mxc: &str,
        mime: &str,
        parts: crate::media::PendingParts,
    ) {
        if self.weft_media.is_none() {
            warn!("media is not bridged ([weft] media_url unset) — attachment dropped");
            return;
        }

        let (bytes, served_mime) = match self.hs.download_mxc(mxc).await {
            Ok(blob) => blob,
            Err(e) => {
                warn!(mxc, "attachment download failed: {e:#}");
                return;
            }
        };
        // Trust the event's declared mime over the transport's when both exist
        // — the sender described the file, the server may have guessed.
        let mime = if mime == "application/octet-stream" {
            served_mime
        } else {
            mime.to_string()
        };

        let bytes_len = bytes.len() as u64;
        let label = self.pending_uploads.park(PendingUpload {
            bytes,
            mime: mime.clone(),
            sender: parts.sender,
            channel: parts.channel,
            body: parts.body,
            msgid: parts.msgid,
            event_id: parts.event_id,
            room_id: parts.room_id,
        });

        if let Err(e) = self.realm.offer_media(&mime, bytes_len, &label).await {
            warn!("media offer failed: {e:#}");
            self.pending_uploads.forget(&label);
        }
    }

    /// The grant arrived: post the bytes, then send the message that references
    /// them. A failure at either step drops the whole message rather than
    /// leaving one that points at nothing.
    async fn finish_attachment_upload(&mut self, label: &str, token: &str) {
        let Some(pending) = self.pending_uploads.take(label) else {
            return; // not ours (weftd labels its answer with our offer's label)
        };
        let Some(media) = self.weft_media.clone() else {
            return;
        };

        let reference = match media.upload(token, pending.bytes, &pending.mime).await {
            Ok(reference) => reference,
            Err(e) => {
                warn!(event_id = %pending.event_id, "blob upload failed: {e:#}");
                return;
            }
        };

        if let Err(e) = self
            .realm
            .message_with_attachments(
                &pending.sender,
                &pending.msgid,
                &pending.channel,
                &pending.body,
                vec![reference],
            )
            .await
        {
            warn!(event_id = %pending.event_id, "attachment message failed: {e:#}");
            return;
        }

        self.store
            .link(&pending.event_id, &pending.msgid, &pending.room_id)
            .await;
    }

    /// WEFT → Matrix: mirror a message's attachments as their own Matrix
    /// events, one per blob (Matrix carries one attachment per event).
    ///
    /// Uploaded to the companion homeserver once and referenced by `mxc://`, so
    /// remote homeservers fetch it through ordinary Matrix media federation.
    async fn relay_attachments(
        &mut self,
        room_id: &str,
        puppet: &str,
        msgid: &str,
        attachments: &[String],
    ) {
        let Some(media) = self.weft_media.clone() else {
            if !attachments.is_empty() {
                warn!("media is not bridged ([weft] media_url unset) — attachments dropped");
            }
            return;
        };

        for (index, reference) in attachments.iter().enumerate() {
            let Some(hash) = crate::media::weft_hash(reference) else {
                continue; // not a blob reference we understand
            };

            let bytes = match media.fetch(hash).await {
                Ok(bytes) => bytes,
                Err(e) => {
                    warn!(hash, "blob fetch failed: {e:#}");
                    continue;
                }
            };
            // weftd does not report a mime on the fetch, so sniff the two cases
            // a chat actually cares about and fall back to a download.
            let mime = sniff_mime(&bytes);
            let mxc = match self.hs.upload_media(bytes, mime, hash).await {
                Ok(mxc) => mxc,
                Err(e) => {
                    warn!(hash, "media upload to Matrix failed: {e:#}");
                    continue;
                }
            };

            if let Err(e) = self
                .hs
                .send(
                    room_id,
                    "m.room.message",
                    json!({
                        "msgtype": crate::media::msgtype_for(mime),
                        "body": hash,
                        "url": mxc,
                        "info": { "mimetype": mime },
                    }),
                    &txn_of(&format!("att-{msgid}-{index}")),
                    Some(puppet),
                )
                .await
            {
                warn!(hash, "attachment event failed: {e:#}");
            }
        }
    }

    // ---- backfill (protocol doc §8) ----------------------------------------

    /// Answer weftd's `HISTORY` for a replica channel by **replaying the window
    /// as ordinary ingestion** — there is no separate backfill ingress.
    ///
    /// Two properties make the replay safe to repeat:
    ///
    /// - **Deterministic msgids.** `ident::msgid_for` derives the id from the
    ///   event id and its `origin_server_ts`, so replaying an event yields the
    ///   id it already has. A window fetched twice cannot fork a message.
    /// - **Oldest-first.** Matrix pages backwards (newest first); the replica is
    ///   ordered by ULID time, so the page is reversed before it is sent.
    ///
    /// Events already linked are skipped: they are in the store, and re-sending
    /// them would ask the channel actor to ingest what it has.
    async fn backfill(
        &mut self,
        channel: &str,
        before: Option<&weft_proto::MsgId>,
        limit: Option<u32>,
    ) -> anyhow::Result<()> {
        // Only a consumed replica has a foreign scrollback to fetch; a
        // projected channel's history is the home's own (it minted every id),
        // so weftd never asks — but be explicit rather than rely on that.
        let Some((room_id, space)) = self.store.state.room_of_channel(channel) else {
            return Ok(());
        };
        let (room_id, ns_id, realm) = (
            room_id.to_string(),
            space.ns_id.clone(),
            realm_of_uri(&space.uri),
        );

        if self.store.state.bans.is_banned(&ns_id) {
            return Ok(());
        }

        // Anchor the page at the oldest message we hold; without one, start at
        // the live end (a channel we have nothing for yet).
        let from = match before {
            Some(msgid) => {
                let Some(at) = self.store.state.links.event_of(&msgid.to_string()) else {
                    // We do not know that id — nothing to anchor on, and
                    // guessing would replay an arbitrary window.
                    debug!(%msgid, "backfill anchor is unknown — skipped");
                    return Ok(());
                };
                self.hs.token_at_event(&room_id, &at.event.clone()).await?
            }
            None => None,
        };

        // Matrix caps a page far below WEFT's 500; asking for more just wastes
        // the round trip, and the client scrolls again for the next window.
        let limit = limit.unwrap_or(BACKFILL_PAGE).min(BACKFILL_PAGE);
        let mut chunk = self
            .hs
            .messages_back(&room_id, from.as_deref(), limit)
            .await?;
        chunk.reverse();

        let mut replayed = 0usize;
        for ev in chunk {
            let (Some(event_id), Some(sender)) = (
                ev["event_id"].as_str().map(String::from),
                ev["sender"].as_str().map(String::from),
            ) else {
                continue;
            };
            if ev["type"] != "m.room.message" {
                continue; // v1 replays messages; edits/reactions ride live
            }
            if self.store.state.links.msgid_of(&event_id).is_some() {
                continue; // already ingested
            }
            // Our own puppets' events are WEFT-origin already (relayed out);
            // ingesting them would author our users under the realm.
            let Some(weft_sender) = self.foreign_user(&sender) else {
                continue;
            };
            let Some(body) = ev["content"]["body"].as_str() else {
                continue;
            };

            let ts = ev["origin_server_ts"].as_u64().unwrap_or_default();
            let minted = ident::msgid_for(&realm, &event_id, ts);
            if let Err(e) = self
                .realm
                .message(&weft_sender, &minted, channel, body)
                .await
            {
                warn!(event_id, "backfill replay failed: {e:#}");
                break;
            }

            self.store.link(&event_id, &minted, &room_id).await;
            replayed += 1;
        }

        info!(channel, replayed, "backfilled a window");
        Ok(())
    }

    // ---- authority: capabilities here, power levels there (§10) ------------

    /// A WEFT grant/revoke in a bridged namespace → the mapped Matrix level,
    /// written into every room of that namespace's space (replica or
    /// projection alike).
    async fn apply_level_outbound(
        &mut self,
        scope: &str,
        subject: &str,
        subject_ulid: Option<&str>,
        level: i64,
    ) {
        let Some(ns_id) = scope.strip_prefix("ns:") else {
            return; // only namespace authority maps onto a space (§10)
        };

        // Consumed space or outbound projection — collect its rooms.
        let mut rooms: Vec<String> = Vec::new();
        if let Some(space) = self.store.state.space_of_ns(ns_id) {
            rooms.extend(space.rooms.keys().cloned());
            rooms.push(space.room_id.clone());
        } else if let Some(p) = self.store.state.projections.get(ns_id) {
            rooms.extend(p.rooms.values().cloned());
            rooms.push(p.space_room.clone());
        } else {
            return;
        }

        // The subject on the Matrix side: a foreign handle addresses its real
        // MXID; a local account addresses their puppet — registered here if
        // this is the first we hear of them (the relay carries their ULID
        // precisely so a grant need not wait for their first message).
        let mxid = if subject.contains('@') {
            ident::mxid_of_weft_user(subject)
        } else {
            match subject_ulid {
                Some(ulid) => self.ensure_puppet(ulid, subject).await.ok(),
                None => self.puppet_of_account(subject),
            }
        };
        let Some(mxid) = mxid else {
            warn!(
                subject,
                "no Matrix identity for the grant subject — skipped"
            );
            return;
        };

        for room in rooms {
            if let Err(e) = self.set_room_level(&room, &mxid, level).await {
                warn!(room, mxid, level, "power-level write failed: {e:#}");
            }
        }
    }

    /// Read-modify-write one room's `m.room.power_levels` users map, and move
    /// our own diff baseline with it (so the echo of our write is a no-op).
    async fn set_room_level(&mut self, room: &str, mxid: &str, level: i64) -> anyhow::Result<()> {
        let mut content = self
            .hs
            .get_state(room, "m.room.power_levels", "")
            .await?
            .unwrap_or_else(|| json!({}));

        let users = content["users"].as_object().cloned().unwrap_or_default();
        let mut users = users;
        if level == 0 {
            users.remove(mxid);
        } else {
            users.insert(mxid.to_string(), json!(level));
        }
        content["users"] = Value::Object(users.clone());

        self.hs
            .put_state(room, "m.room.power_levels", "", content)
            .await?;

        let baseline: std::collections::BTreeMap<String, i64> = users
            .iter()
            .filter_map(|(u, l)| l.as_i64().map(|l| (u.clone(), l)))
            .collect();
        self.store.set_room_levels(room, baseline).await;
        Ok(())
    }

    /// An inbound `m.room.power_levels` event: diff against the baseline and
    /// translate each change into the acting moderator's attributed
    /// GRANT/REVOKE — weftd honors them iff WEFT granted *them* the authority
    /// (§10: no side-channel authority).
    async fn on_power_levels_event(&mut self, ev: &Value, room_id: &str, ns_id: &str, actor: &str) {
        let new: std::collections::BTreeMap<String, i64> = ev["content"]["users"]
            .as_object()
            .map(|m| {
                m.iter()
                    .filter_map(|(u, l)| l.as_i64().map(|l| (u.clone(), l)))
                    .collect()
            })
            .unwrap_or_default();
        let old = self
            .store
            .state
            .room_levels
            .get(room_id)
            .cloned()
            .unwrap_or_default();

        let scope = format!("ns:{ns_id}");
        for (mxid, level) in crate::levels::diff_users(&old, &new) {
            let Some(subject) = self.weft_subject_of_mxid(&mxid) else {
                continue; // the bot, or an unmappable identity
            };

            // §10: label the act so a refusal comes back correlated, and park
            // the undo — the Matrix state changed *before* WEFT agreed.
            let label = self.park_act(PendingAct::Level {
                room: room_id.to_string(),
                mxid: mxid.clone(),
                previous: old.get(&mxid).copied().unwrap_or(0),
                actor: actor.to_string(),
            });

            // Revoke-then-grant makes the translation deterministic: the new
            // tier's caps replace whatever the old tier held. Only the act that
            // can be *refused* carries the label — a revoke of nothing cannot.
            let caps = crate::levels::caps_for_level(level);
            if let Err(e) = self
                .realm
                .revoke_as(
                    actor,
                    &subject,
                    &scope,
                    None,
                    caps.is_none().then_some(&*label),
                )
                .await
            {
                warn!(subject, "revoke_as failed: {e:#}");
                continue;
            }
            if let Some(caps) = caps {
                if let Err(e) = self
                    .realm
                    .grant_as(actor, &subject, &scope, caps, Some(&label))
                    .await
                {
                    warn!(subject, "grant_as failed: {e:#}");
                }
            }
        }

        self.store.set_room_levels(room_id, new).await;
    }

    /// The WEFT subject a Matrix id maps to: a puppet → its bare local
    /// account, a foreign MXID → the escaped handle, the bot → nothing.
    fn weft_subject_of_mxid(&self, mxid: &str) -> Option<String> {
        let parsed: &ruma::UserId = mxid.try_into().ok()?;

        if parsed.server_name().host() == self.identity.domain()
            && parsed.localpart() == self.identity.bot_localpart()
        {
            return None;
        }
        if let Some(ulid) = self.identity.puppet_ulid(parsed) {
            // Puppets are ULID-keyed; the subject is the bare account.
            return self
                .store
                .state
                .users
                .by_ulid(ulid)
                .map(|u| u.account.clone());
        }

        ident::weft_user(parsed)
    }

    /// An inbound moderation act on one of **our** users' puppets: a Matrix
    /// ban/kick of a foreign user stays Matrix-internal (their membership
    /// event updates the roster), but against a puppet it is the §10
    /// translation — the attributed BAN/KICK, checked against the actor's
    /// WEFT grants.
    async fn on_member_moderation(&mut self, ev: &Value, channel: &str, ns_id: &str, actor: &str) {
        let Some(target) = ev["state_key"].as_str() else {
            return;
        };
        let sender = ev["sender"].as_str().unwrap_or_default();
        if sender == target {
            return; // an ordinary self leave/join, not moderation
        }
        let Some(account) = self.puppet_account_of(target) else {
            return; // not a puppet — roster flows handle it
        };
        let membership = ev["content"]["membership"].as_str().unwrap_or_default();
        let reason = ev["content"]["reason"].as_str();

        let label = self.park_act(PendingAct::Membership {
            room: ev["room_id"].as_str().unwrap_or_default().to_string(),
            mxid: Some(target.to_string()),
            was_banned: membership == "ban",
            actor: actor.to_string(),
        });

        let result = match membership {
            "ban" => {
                self.realm
                    .ban_as(
                        actor,
                        &format!("ns:{ns_id}"),
                        &account,
                        reason,
                        true,
                        Some(&label),
                    )
                    .await
            }
            "leave" => {
                self.realm
                    .kick_as(actor, channel, &account, reason, Some(&label))
                    .await
            }
            _ => return,
        };
        if let Err(e) = result {
            warn!(account, membership, "moderation relay failed: {e:#}");
        }
    }

    // ---- management flows (slice 11) ---------------------------------------

    /// A routed `PLUGIN INVOKE`: open the flow's first view. The invoker is
    /// remembered because every command the flow later issues is **theirs**
    /// (`@as`), never the service's.
    pub async fn on_invoke(
        &mut self,
        view_id: &str,
        action: &str,
        ctx_ref: Option<&str>,
        invoker: Option<&str>,
    ) {
        let ctx = self.realm.ctx_for(view_id);
        let Some(invoker) = invoker else {
            let _ = ctx
                .toast(ToastKind::Error, "weftd did not name the invoker")
                .await;
            return;
        };
        let ctx_ref = ctx_ref.unwrap_or_default();
        self.flows.insert(
            view_id.to_string(),
            Flow {
                action: action.to_string(),
                invoker: invoker.to_string(),
                ctx_ref: ctx_ref.to_string(),
            },
        );

        let opened = match action {
            "power-levels" => self.open_power_levels(&ctx, ctx_ref).await,
            "create-room" => {
                let projected = self.store.state.projections.contains_key(ctx_ref);
                let consumed = self.store.state.space_of_ns(ctx_ref).is_some();
                if !projected && !consumed {
                    let _ = ctx
                        .toast(ToastKind::Error, "this namespace is not bridged")
                        .await;
                    self.flows.remove(view_id);
                    return;
                }

                ctx.view(&crate::actions::create_room_view(projected)).await
            }
            "create-subspace" => {
                if !self.store.state.projections.contains_key(ctx_ref) {
                    // A consumed space's structure is the realm's to describe
                    // (§4): its categories arrive on the assertions, so there
                    // is nothing for us to add here.
                    let _ = ctx
                        .toast(
                            ToastKind::Error,
                            "categories are only ours to add in a projected namespace",
                        )
                        .await;
                    self.flows.remove(view_id);
                    return;
                }

                let current = self
                    .store
                    .state
                    .projections
                    .get(ctx_ref)
                    .map(|p| p.declared_categories.clone())
                    .unwrap_or_default();

                ctx.view(&crate::actions::create_subspace_view(&current))
                    .await
            }
            "invite" => {
                self.open_for_channel(&ctx, ctx_ref, crate::actions::invite_view)
                    .await
            }
            "moderate" => {
                let channels = self.bridged_channels();
                ctx.view(&crate::actions::moderate_view(ctx_ref, &channels))
                    .await
            }
            "room-settings" => self.open_room_settings(&ctx, ctx_ref).await,
            "bans" => {
                let banned: Vec<String> =
                    self.store.state.bans.iter().map(str::to_string).collect();
                ctx.view(&crate::actions::bans_view(&banned)).await
            }
            _ => {
                let _ = ctx.toast(ToastKind::Error, "unknown action").await;
                self.flows.remove(view_id);
                return;
            }
        };

        if let Err(e) = opened {
            warn!(action, "opening the flow failed: {e:#}");
            let _ = ctx.toast(ToastKind::Error, &format!("{e}")).await;
            self.flows.remove(view_id);
        }
    }

    /// The Power Levels surface — the stand-in `authority=levels` promises.
    async fn open_power_levels(
        &mut self,
        ctx: &weft_appservice::Ctx,
        ns_id: &str,
    ) -> anyhow::Result<()> {
        let Some(space_room) = self.space_room_of_ns(ns_id) else {
            anyhow::bail!("this namespace is not bridged");
        };
        // Read the live map rather than our diff baseline: the view is for a
        // human, so it should show what Matrix actually says.
        let users = self
            .hs
            .get_state(&space_room, "m.room.power_levels", "")
            .await?
            .and_then(|c| c["users"].as_object().cloned())
            .map(|m| {
                m.iter()
                    .filter_map(|(u, l)| l.as_i64().map(|l| (u.clone(), l)))
                    .collect()
            })
            .unwrap_or_default();

        ctx.view(&crate::actions::power_levels_view(&space_room, &users))
            .await
    }

    async fn open_for_channel(
        &mut self,
        ctx: &weft_appservice::Ctx,
        channel: &str,
        view: fn(&str) -> weft_proto::View,
    ) -> anyhow::Result<()> {
        let Some(room) = self.room_of_channel(channel) else {
            anyhow::bail!("this channel is not bridged");
        };

        ctx.view(&view(&room)).await
    }

    async fn open_room_settings(
        &mut self,
        ctx: &weft_appservice::Ctx,
        channel: &str,
    ) -> anyhow::Result<()> {
        let Some(room) = self.room_of_channel(channel) else {
            anyhow::bail!("this channel is not bridged");
        };

        let name = self
            .hs
            .get_state(&room, "m.room.name", "")
            .await?
            .and_then(|c| c["name"].as_str().map(String::from))
            .unwrap_or_default();
        let topic = self
            .hs
            .get_state(&room, "m.room.topic", "")
            .await?
            .and_then(|c| c["topic"].as_str().map(String::from))
            .unwrap_or_default();

        ctx.view(&crate::actions::room_settings_view(&room, &name, &topic))
            .await
    }

    /// A submit or control click on an open flow.
    pub async fn on_step(
        &mut self,
        view_id: &str,
        button: Option<&str>,
        values: &std::collections::BTreeMap<String, serde_json::Value>,
        closed: bool,
    ) {
        // Dismissed: terminal, nothing to answer.
        if closed {
            self.flows.remove(view_id);
            return;
        }
        let Some(flow) = self.flows.get(view_id).cloned() else {
            return; // not ours (or already finished)
        };
        let ctx = self.step_ctx(view_id);

        let outcome = match flow.action.as_str() {
            "power-levels" => self.step_power_levels(&flow, values).await,
            "create-room" => self.step_create_room(&flow, values).await,
            "create-subspace" => self.step_create_subspace(&flow, values).await,
            "invite" => self.step_invite(&flow, values).await,
            "moderate" => self.step_moderate(&flow, button, values).await,
            "room-settings" => self.step_room_settings(&flow, values).await,
            _ => Err(anyhow::anyhow!("this flow has no steps")),
        };

        self.flows.remove(view_id);
        let answer = match outcome {
            Ok(text) => ctx.toast(ToastKind::Ok, &text).await,
            Err(e) => {
                warn!(action = %flow.action, "flow step failed: {e:#}");
                ctx.toast(ToastKind::Error, &format!("{e}")).await
            }
        };
        if let Err(e) = answer {
            warn!("answering the flow failed: {e:#}");
        }
    }

    /// Set one user's level: write it on Matrix, then mirror the mapped
    /// capabilities as the **invoker's** attributed GRANT — so weftd checks
    /// their authority, and refuses (with a revert) if they lack it.
    async fn step_power_levels(
        &mut self,
        flow: &Flow,
        values: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> anyhow::Result<String> {
        let mxid = crate::actions::value(values, "mxid").to_string();
        let level: i64 = crate::actions::value(values, "level").parse().unwrap_or(0);
        if mxid.is_empty() {
            anyhow::bail!("name a Matrix user");
        }
        let ns_id = flow.ctx_ref.clone();
        let Some(space_room) = self.space_room_of_ns(&ns_id) else {
            anyhow::bail!("this namespace is not bridged");
        };

        // The Matrix write goes first and is the thing a refusal reverts —
        // `on_power_levels_event` will not double-apply it, since our own
        // write moves the baseline with it.
        self.set_room_level(&space_room, &mxid, level).await?;

        let Some(subject) = self.weft_subject_of_mxid(&mxid) else {
            anyhow::bail!("that Matrix user maps to no WEFT identity");
        };
        let scope = format!("ns:{ns_id}");
        let label = self.park_act(PendingAct::Level {
            room: space_room,
            mxid: mxid.clone(),
            previous: 0,
            actor: flow.invoker.clone(),
        });

        let caps = crate::levels::caps_for_level(level);
        self.realm
            .revoke_as(
                &flow.invoker,
                &subject,
                &scope,
                None,
                caps.is_none().then_some(&*label),
            )
            .await?;
        if let Some(caps) = caps {
            self.realm
                .grant_as(&flow.invoker, &subject, &scope, caps, Some(&label))
                .await?;
        }

        Ok(format!("{mxid} set to {level}"))
    }

    /// Create a room, on whichever side owns it.
    ///
    /// **Projected namespace:** the WEFT channel is the real object, so this is
    /// an attributed `CHANNEL CREATE` — the invoker's `chan-create` is what
    /// weftd checks — with `permanent` retention, since nothing else projects
    /// (§3). weftd then pushes the new channel's structure to us and we mirror
    /// it through the ordinary path; no room is created here.
    ///
    /// **Consumed space:** weftd refuses local creates in a replica (its
    /// channels are whatever the realm asserts), so the room is created on
    /// Matrix and *asserted* back — the same direction as provisioning.
    async fn step_create_room(
        &mut self,
        flow: &Flow,
        values: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> anyhow::Result<String> {
        let name = crate::actions::value(values, "name").trim().to_string();
        anyhow::ensure!(!name.is_empty(), "name the room");
        let ns_id = flow.ctx_ref.clone();

        if self.store.state.projections.contains_key(&ns_id) {
            let vanity = vanity_of(&name);
            anyhow::ensure!(!vanity.is_empty(), "that name has no usable characters");

            self.realm
                .create_channel_as(
                    &flow.invoker,
                    &ns_id,
                    &vanity,
                    weft_proto::RetentionPolicy::Permanent,
                    None,
                )
                .await?;

            return Ok(format!("creating #{vanity} — it will mirror as a room"));
        }

        // A consumed space: create foreign-side, then assert it.
        let Some(space) = self.store.state.space_of_ns(&ns_id).cloned() else {
            anyhow::bail!("this namespace is not bridged");
        };
        anyhow::ensure!(
            !self.store.state.bans.is_banned(&ns_id),
            "this space is banned from bridging"
        );
        let Some(space_ref) = ident::SpaceRef::parse(&space.uri) else {
            anyhow::bail!("the stored space URI is unusable");
        };

        let room_id = self
            .hs
            .create_room(json!({
                "name": name,
                "preset": "public_chat",
                "power_level_content_override": { "users_default": 0, "state_default": 100 },
            }))
            .await?;
        self.hs
            .put_state(
                &space.room_id,
                "m.space.child",
                &room_id,
                json!({ "via": [self.identity.domain()], "order": format!("{:010}", space.rooms.len()) }),
            )
            .await?;

        // Assert it into WEFT: our ids, weftd pins them (§4).
        let chan_id = ident::stable_ulid(&room_id);
        let uri = space_ref.room_uri(&chan_id);
        let vanity = vanity_of(&name);
        let channel = self
            .realm
            .assert_channel(&weft_appservice::ChannelAssertion {
                uri: &uri,
                id: &chan_id,
                namespace_id: &ns_id,
                vanity: &vanity,
                position: space.rooms.len() as i64,
                kind: weft_proto::ChannelKind::Text,
                category: None,
            })
            .await?;

        // The **consumed** map, not the projection one: this room's events are
        // realm-minted (ordinary replica ingestion), and filing it under
        // projections would route them down the injection path instead, where
        // the home mints — two ids for every message.
        let mut space = space;
        space.rooms.insert(
            room_id,
            crate::store::Room {
                chan_id,
                channel,
                uri,
            },
        );
        self.store.save_space(space).await;

        Ok(format!("created {name}"))
    }

    /// Add a category to a projected namespace.
    ///
    /// The category list is namespace metadata, so this is an attributed
    /// `NS META … categories` — the invoker's ns-admin is what weftd checks.
    /// The sub-space is **not** created here: weftd applies the change and
    /// pushes the resulting `NS-META` back, and that push is what builds it.
    /// Creating it locally first would leave an orphan sub-space behind
    /// whenever weftd refused.
    async fn step_create_subspace(
        &mut self,
        flow: &Flow,
        values: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> anyhow::Result<String> {
        let name = crate::actions::value(values, "name").trim().to_string();
        anyhow::ensure!(!name.is_empty(), "name the category");
        anyhow::ensure!(
            !name.contains(','),
            "a category name cannot contain a comma (the list separator)"
        );

        let ns_id = flow.ctx_ref.clone();
        let Some(projection) = self.store.state.projections.get(&ns_id) else {
            anyhow::bail!("this namespace is not projected");
        };
        anyhow::ensure!(
            !projection.declared_categories.contains(&name),
            "that category already exists"
        );

        // Append to **weftd's** list, not to the sub-spaces we happen to have
        // built: the meta key is a full replace, so appending to a partial view
        // would delete every category we had not projected yet.
        let mut categories = projection.declared_categories.clone();
        categories.push(name.clone());

        self.realm
            .set_ns_meta_as(
                &flow.invoker,
                &ns_id,
                "categories",
                &categories.join(","),
                None,
            )
            .await?;

        Ok(format!("creating the {name} category"))
    }

    async fn step_invite(
        &mut self,
        flow: &Flow,
        values: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> anyhow::Result<String> {
        let mxid = crate::actions::value(values, "mxid");
        if mxid.is_empty() {
            anyhow::bail!("name a Matrix user");
        }
        let Some(room) = self.room_of_channel(&flow.ctx_ref) else {
            anyhow::bail!("this channel is not bridged");
        };

        // As the invoker's puppet when we have one: an invite is a social act,
        // and it should read as coming from the person who made it.
        let puppet = self.puppet_of_account(&account_of(&flow.invoker));
        self.hs.invite(&room, mxid, puppet.as_deref()).await?;

        Ok(format!("invited {mxid}"))
    }

    /// Kick or ban, at the scope the view collected: a kick names the chosen
    /// channel, a ban covers that channel's namespace. Both are attributed and
    /// labeled — weftd checks the invoker's caps, and a refusal reverts.
    async fn step_moderate(
        &mut self,
        flow: &Flow,
        button: Option<&str>,
        values: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> anyhow::Result<String> {
        let reason = crate::actions::value(values, "reason");
        let reason = (!reason.is_empty()).then_some(reason);
        // §13.2: the ctx-ref is `user@net`; weftd's moderation verbs name the
        // bare account (a foreign member keeps their handle).
        let target = flow.ctx_ref.clone();
        let channel = crate::actions::value(values, "channel").to_string();
        anyhow::ensure!(!channel.is_empty(), "pick a channel");

        let Some(room) = self.room_of_channel(&channel) else {
            anyhow::bail!("that channel is not bridged");
        };
        let mxid = self.matrix_id_of(&target);

        // A **foreign** member cannot be named by weftd's moderation verbs at
        // all — they take a bare `Account`, and a foreign handle is a
        // `user@realm`. That is not an oversight to route around: a foreign
        // member's membership is the realm's to state (§6), so removing them
        // is a foreign-side act, and the realm's `NS-MEMBER part` follows.
        if target.contains('@') {
            let Some(mxid) = mxid else {
                anyhow::bail!("that member has no Matrix identity to remove");
            };
            let ban = button == Some("ban");
            anyhow::ensure!(ban || button == Some("kick"), "pick an action");

            self.hs.remove_member(&room, &mxid, reason, ban).await?;

            return Ok(if ban {
                format!("banned {target} from the room")
            } else {
                format!("kicked {target} from the room")
            });
        }

        match button {
            Some("kick") => {
                // Foreign-side first, so a WEFT refusal has something to
                // revert; a kick cannot be undone, so the notice is the remedy
                // (see `revert_act`).
                let label = self.park_act(PendingAct::Membership {
                    room: room.clone(),
                    mxid,
                    was_banned: false,
                    actor: flow.invoker.clone(),
                });

                self.realm
                    .kick_as(&flow.invoker, &channel, &target, reason, Some(&label))
                    .await?;

                Ok(format!("kicked {target} from the channel"))
            }
            Some("ban") => {
                let Some(ns_id) = channel
                    .strip_prefix('#')
                    .and_then(|c| c.split_once('/'))
                    .map(|(ns, _)| ns.to_string())
                else {
                    anyhow::bail!("that channel has no namespace to ban in");
                };
                let label = self.park_act(PendingAct::Membership {
                    room: room.clone(),
                    mxid,
                    was_banned: true,
                    actor: flow.invoker.clone(),
                });

                self.realm
                    .ban_as(
                        &flow.invoker,
                        &format!("ns:{ns_id}"),
                        &target,
                        reason,
                        true,
                        Some(&label),
                    )
                    .await?;

                Ok(format!("banned {target} from the namespace"))
            }
            _ => anyhow::bail!("pick an action"),
        }
    }

    /// Every channel this bridge mirrors, as `(wire name, display label)` —
    /// what the moderate view's scope picker offers.
    fn bridged_channels(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();

        for space in self.store.state.spaces.values() {
            for room in space.rooms.values() {
                out.push((room.channel.clone(), room.uri.clone()));
            }
        }
        for projection in self.store.state.projections.values() {
            for (channel, room) in &projection.rooms {
                out.push((channel.clone(), room.clone()));
            }
        }
        out.sort();

        out
    }

    /// The Matrix identity a WEFT member maps to: a foreign handle addresses
    /// its own MXID, one of ours their puppet. `None` ⇒ nothing to revert
    /// foreign-side (the WEFT act still stands or falls on its own).
    fn matrix_id_of(&self, member: &str) -> Option<String> {
        if member.contains('@') {
            return ident::mxid_of_weft_user(member);
        }

        self.puppet_of_account(member)
    }

    async fn step_room_settings(
        &mut self,
        flow: &Flow,
        values: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> anyhow::Result<String> {
        let Some(room) = self.room_of_channel(&flow.ctx_ref) else {
            anyhow::bail!("this channel is not bridged");
        };
        let name = crate::actions::value(values, "name");
        let topic = crate::actions::value(values, "topic");

        // Room state is the **bot's** to write (§9: bridge-created rooms are
        // bridge-controlled), so this is not an attributed act — the gate is
        // the client's own: the action is only offered where it is allowed.
        if !name.is_empty() {
            self.hs
                .put_state(&room, "m.room.name", "", json!({ "name": name }))
                .await?;
        }
        self.hs
            .put_state(&room, "m.room.topic", "", json!({ "topic": topic }))
            .await?;

        Ok("room settings saved".into())
    }

    /// A `Ctx` for answering a step of an already-open flow.
    fn step_ctx(&self, view_id: &str) -> weft_appservice::Ctx {
        self.realm.ctx_for(view_id)
    }

    /// The Space room of a bridged namespace (consumed or projected).
    fn space_room_of_ns(&self, ns_id: &str) -> Option<String> {
        if let Some(space) = self.store.state.space_of_ns(ns_id) {
            return Some(space.room_id.clone());
        }

        self.store
            .state
            .projections
            .get(ns_id)
            .map(|p| p.space_room.clone())
    }

    /// The Matrix room of a bridged channel (consumed or projected).
    fn room_of_channel(&self, channel: &str) -> Option<String> {
        if let Some((room, _)) = self.store.state.room_of_channel(channel) {
            return Some(room.to_string());
        }

        self.store
            .state
            .projected_room_of_channel(channel)
            .map(|(_, room)| room.to_string())
    }

    /// Park an undo and return its correlation label.
    fn park_act(&mut self, act: PendingAct) -> String {
        self.pending_acts.park(act)
    }

    /// §10's *revert + notice*: WEFT refused an attributed act, so undo the
    /// foreign-side change that got ahead of it and tell the actor why.
    /// Without this, the two sides disagree permanently — Matrix would show a
    /// moderator power that WEFT never granted.
    async fn revert_act(&mut self, label: &str, why: &str) {
        let Some(act) = self.pending_acts.take(label) else {
            return; // not ours, or already resolved
        };

        let (room, actor, note) = match act {
            PendingAct::Level {
                room,
                mxid,
                previous,
                actor,
            } => {
                if let Err(e) = self.set_room_level(&room, &mxid, previous).await {
                    warn!(room, mxid, "revert of the power level failed: {e:#}");
                }
                (
                    room,
                    actor,
                    format!("{mxid}'s power level was reverted: {why}"),
                )
            }
            PendingAct::Membership {
                room,
                mxid,
                was_banned,
                actor,
            } => {
                // Unban restores the ability to rejoin; a kick cannot be
                // undone (only they can rejoin), so the notice is the remedy —
                // and it is the *whole* remedy when there was no Matrix-side
                // change to reverse.
                if was_banned {
                    if let Some(mxid) = &mxid {
                        if let Err(e) = self.hs.unban(&room, mxid).await {
                            warn!(room, %mxid, "revert of the ban failed: {e:#}");
                        }
                    }
                }
                let who = mxid.as_deref().unwrap_or("that member");
                (room, actor, format!("moderating {who} was refused: {why}"))
            }
        };

        // The notice goes to the room as the bot — the actor is a Matrix user
        // with no WEFT session to answer on.
        if let Err(e) = self
            .hs
            .send(
                &room,
                "m.room.message",
                json!({ "msgtype": "m.notice", "body": format!("{actor}: {note}") }),
                &format!("revert-{label}"),
                None,
            )
            .await
        {
            warn!(room, "revert notice failed: {e:#}");
        }
    }

    /// A puppet MXID → the bare local account it stands for.
    fn puppet_account_of(&self, mxid: &str) -> Option<String> {
        let parsed: &ruma::UserId = mxid.try_into().ok()?;
        let ulid = self.identity.puppet_ulid(parsed)?;

        self.store
            .state
            .users
            .by_ulid(ulid)
            .map(|u| u.account.clone())
    }

    // ---- outbound projection (matrix.md §3–§9, the daemon half) ------------

    /// Mirror a projected WEFT namespace as a Matrix Space. Idempotent: an
    /// existing projection only refreshes the display name (vanity is mutable).
    /// The alias is ULID-keyed (`#weft_<ns-id>`) for the same reason puppets
    /// are — renames must not orphan it.
    async fn ensure_projection(
        &mut self,
        ns_id: &str,
        vanity: &str,
        title: Option<&str>,
    ) -> anyhow::Result<()> {
        let name = title.unwrap_or(vanity);

        if let Some(p) = self.store.state.projections.get(ns_id) {
            let room = p.space_room.clone();
            self.hs
                .put_state(&room, "m.room.name", "", json!({ "name": name }))
                .await?;
            return Ok(());
        }

        let space_room = self
            .hs
            .create_room(json!({
                "creation_content": { "type": "m.space" },
                "name": name,
                "room_alias_name": format!("weft_{ns_id}"),
                "preset": "public_chat",
                // §9: the bot rules the room; nobody else touches state.
                "power_level_content_override": { "users_default": 0, "state_default": 100 },
            }))
            .await?;

        // The marker is what recovery reads back: which WEFT namespace this
        // Space is, and which side owns it.
        self.hs
            .put_state(
                &space_room,
                crate::recover::SPACE_MARKER,
                "",
                json!({ "kind": "projected", "ns": ns_id }),
            )
            .await?;

        self.store.save_projection(ns_id, &space_room).await;
        info!(ns_id, space_room, "projected namespace as a Space");
        Ok(())
    }

    /// Mirror a projected namespace's categories as **sub-spaces** under its
    /// top Space (matrix.md §6, locked decision 4), ordered by their position
    /// in `cats=` — the same order clients render.
    ///
    /// Additive: a category dropped from the list keeps its sub-space (with
    /// whatever rooms are in it) rather than being tombstoned, because a
    /// tombstone is unrecoverable and a rename arrives as a drop plus an add.
    async fn ensure_categories(
        &mut self,
        ns_id: &str,
        categories: &[String],
    ) -> anyhow::Result<()> {
        let Some(projection) = self.store.state.projections.get_mut(ns_id) else {
            return Ok(());
        };
        // weftd's list is the authority for later edits (the meta key is a full
        // replace), so record it before acting on it.
        projection.declared_categories = categories.to_vec();
        let space_room = projection.space_room.clone();
        let known = projection.categories.clone();

        for (index, category) in categories.iter().enumerate() {
            if known.contains_key(category) {
                continue;
            }

            let room = self
                .hs
                .create_room(json!({
                    "creation_content": { "type": "m.space" },
                    "name": category,
                    "preset": "public_chat",
                    "power_level_content_override": { "users_default": 0, "state_default": 100 },
                }))
                .await?;

            self.hs
                .put_state(
                    &space_room,
                    "m.space.child",
                    &room,
                    json!({ "via": [self.identity.domain()], "order": format!("{index:010}") }),
                )
                .await?;
            self.hs
                .put_state(
                    &room,
                    "m.space.parent",
                    &space_room,
                    json!({ "via": [self.identity.domain()], "canonical": true }),
                )
                .await?;

            self.store
                .save_projected_category(ns_id, category, &room)
                .await;
            info!(ns_id, category, room, "projected category as a sub-space");
        }

        Ok(())
    }

    /// Mirror one projected channel as a room under its Space — iff the §3
    /// rules hold: `permanent` retention only, never e2ee, never voice. A
    /// channel failing them is simply absent, not an error.
    async fn ensure_projected_room(
        &mut self,
        channel: &str,
        layout: PendingLayout,
        policy: weft_proto::RetentionPolicy,
    ) -> anyhow::Result<()> {
        let Some(ns_id) = channel
            .strip_prefix('#')
            .and_then(|c| c.split_once('/'))
            .map(|(ns, _)| ns.to_string())
        else {
            return Ok(()); // top-level channels are not projectable
        };
        let Some(projection) = self.store.state.projections.get(&ns_id) else {
            return Ok(()); // POLICY for a namespace we don't project
        };

        // §3 projection rules (locked decisions 2 + 7 + voice-out-of-v1).
        if policy != weft_proto::RetentionPolicy::Permanent
            || layout.kind == weft_proto::ChannelKind::Voice
        {
            debug!(channel, ?policy, "not projectable — absent by rule");
            return Ok(());
        }

        if projection.rooms.contains_key(channel) {
            return Ok(()); // already projected; renames ride m.room.name later
        }
        // A categorized channel hangs under its category's sub-space; an
        // uncategorized one directly under the top Space (§6).
        let parent = layout
            .category
            .as_ref()
            .and_then(|c| projection.categories.get(c))
            .cloned()
            .unwrap_or_else(|| projection.space_room.clone());

        let chan_id = channel.rsplit('/').next().unwrap_or_default().to_string();
        let room_id = self
            .hs
            .create_room(json!({
                "name": layout.vanity,
                "room_alias_name": format!("weft_{chan_id}"),
                "preset": "public_chat",
                "power_level_content_override": { "users_default": 0, "state_default": 100 },
            }))
            .await?;

        // Space ↔ room links, ordered by the WEFT position (§6).
        self.hs
            .put_state(
                &parent,
                "m.space.child",
                &room_id,
                json!({ "via": [self.identity.domain()], "order": format!("{:010}", layout.position) }),
            )
            .await?;
        self.hs
            .put_state(
                &room_id,
                "m.space.parent",
                &parent,
                json!({ "via": [self.identity.domain()], "canonical": true }),
            )
            .await?;

        self.store
            .save_projected_room(&ns_id, channel, &room_id)
            .await;
        info!(channel, room_id, "projected channel as a room");
        Ok(())
    }

    /// A labeled event on our session that answers a pending injection: link
    /// the home-minted id to the Matrix event it came from. Returns whether
    /// the label was ours — if so the event is the ack, never relay fodder.
    async fn link_injection_echo(&mut self, label: &str, msgid: &str) -> bool {
        let Some(at) = self.pending_injections.take(label) else {
            return false;
        };

        self.store.link(&at.event, msgid, &at.room).await;
        true
    }

    /// The puppet MXID for one of our users, **keyed by account ULID** (owner
    /// directive 2026-08-06): the localpart derives from the ULID, so a rename
    /// changes nothing on the Matrix side; the name rides along for display.
    /// Registered on first sight.
    async fn ensure_puppet(&mut self, ulid: &str, account: &str) -> anyhow::Result<String> {
        if let Some(user) = self.store.state.users.by_ulid(ulid) {
            let localpart = user.localpart.clone();
            let mxid = self.identity.mxid(&localpart);

            // A rename: same identity, re-point the name index — and carry the
            // new label to Matrix, which is where recovery reads names back
            // from (and what Matrix users actually see).
            if user.account != account {
                self.store.note_user(ulid, account, &localpart).await;
                if let Err(e) = self.hs.set_display_name(&mxid, account).await {
                    warn!(account, "could not update the puppet's display name: {e:#}");
                }
            }

            return Ok(mxid);
        }

        let localpart = self.identity.puppet_localpart(ulid);
        let mxid = self.identity.mxid(&localpart);
        self.hs.ensure_registered(&localpart).await?;
        // Best-effort: a nameless puppet still bridges, it just renders as its
        // ULID and recovers without its label.
        if let Err(e) = self.hs.set_display_name(&mxid, account).await {
            warn!(account, "could not set the puppet's display name: {e:#}");
        }
        self.store.note_user(ulid, account, &localpart).await;

        Ok(mxid)
    }

    /// Resolve a puppet by wire **name** — for the fan-out events, which carry
    /// no ULID. A miss means this user never reached the bridge through a
    /// membership relay (the only door in), so there is nothing to speak as.
    fn puppet_of_account(&self, account: &str) -> Option<String> {
        self.store
            .state
            .users
            .by_account(account)
            .map(|(_, user)| self.identity.mxid(&user.localpart))
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

/// The mime of a blob weftd handed us. weftd's fetch reports none, and a chat's
/// blobs are overwhelmingly images — so sniff the common magic numbers and fall
/// back to a download rather than mislabelling everything as an image.
fn sniff_mime(bytes: &[u8]) -> &'static str {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => "image/png",
        [0xFF, 0xD8, 0xFF, ..] => "image/jpeg",
        [b'G', b'I', b'F', b'8', ..] => "image/gif",
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => "image/webp",
        [0x1A, 0x45, 0xDF, 0xA3, ..] => "video/webm",
        [b'%', b'P', b'D', b'F', ..] => "application/pdf",
        _ => "application/octet-stream",
    }
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
