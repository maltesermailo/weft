//! Group DMs (social layer): `GROUP CREATE/ADD/REMOVE/LEAVE/NAME` + `GROUPS`.
//! Membership lives in the `GroupStore`; messages ride the directory
//! (`Scope::Group`), minted single-writer like DMs. Members are `UserRef`s
//! (federation-able) — but message + membership *delivery* to remote members
//! rides the bridge (deferred; same-network fan-out works now).

use super::*;
use crate::voice::RelaySpec;
use weft_proto::{
    CallMediaGrant, CallState, GroupId, MemberAction, MessageEvent, MsgId, Ulid, UserRef,
};

impl<S: ControlStream> Session<S> {
    /// Push an event to every *local* member's live sessions, optionally
    /// skipping one account (the actor, who got the direct reply).
    async fn notify_group(&self, members: &[UserRef], event: Event, except: Option<&Account>) {
        for m in members {
            if m.network == self.ctx.info.network && except != Some(&m.account) {
                self.ctx
                    .directory
                    .notify(m.account.clone(), event.clone())
                    .await;
            }
        }
    }

    /// `GROUP CREATE <user@net>…` — open a group DM. The server mints the id,
    /// records the group (creator always a member), and pushes a `GROUP` event
    /// to every member; the creator's copy is the labelled reply.
    pub(super) async fn on_group_create(
        &mut self,
        label: Option<String>,
        members: Vec<UserRef>,
        account: Account,
    ) -> io::Result<Flow> {
        let me = UserRef::new(account.clone(), self.ctx.info.network.clone());
        let mut all = members;
        all.push(me);
        all.sort();
        all.dedup();

        let id = GroupId::new(Ulid::new());
        if let Err(e) = self
            .ctx
            .groups
            .create_group(
                id,
                &UserRef::new(account.clone(), self.ctx.info.network.clone()),
                &all,
                unix_now_ms(),
            )
            .await
        {
            return self.internal(label, &e).await;
        }
        let event = Event::Group {
            id,
            name: None,
            members: all.clone(),
        };
        self.send_event(label, event.clone()).await?;
        self.notify_group(&all, event, Some(&account)).await;
        // Cross-network: sync the group to remote members' networks so it exists
        // consistently everywhere (the enabler for cross-network messaging).
        let creator = UserRef::new(account, self.ctx.info.network.clone());
        self.sync_group_to_remotes(id, &creator, None, &all);
        Ok(Flow::Continue)
    }

    /// `GROUP ADD <&id> <user@net>` — add a member (any member may add). Pushes
    /// a `GROUP-MEMBER … join` to the group and a full `GROUP` to the newcomer.
    pub(super) async fn on_group_add(
        &mut self,
        label: Option<String>,
        group: GroupId,
        user: UserRef,
        account: Account,
    ) -> io::Result<Flow> {
        let me = UserRef::new(account, self.ctx.info.network.clone());
        if !self.member_or_deny(group, &me, &label).await? {
            return Ok(Flow::Continue);
        }
        match self.ctx.groups.add_group_member(group, &user).await {
            Ok(true) => {}
            Ok(false) => return self.no_such_target(label).await, // unknown group
            Err(e) => return self.internal(label, &e).await,
        }
        let members = self.group_members(group).await?;
        // Ack the actor.
        self.send_event(
            label,
            Event::GroupMember {
                group,
                user: user.clone(),
                action: MemberAction::Join,
            },
        )
        .await?;
        // Tell existing members someone joined, and the newcomer the whole group.
        self.notify_group(
            &members,
            Event::GroupMember {
                group,
                user: user.clone(),
                action: MemberAction::Join,
            },
            Some(&user.account).filter(|_| user.network == self.ctx.info.network),
        )
        .await;
        if user.network == self.ctx.info.network {
            let name = self.group_name(group).await;
            self.ctx
                .directory
                .notify(
                    user.account.clone(),
                    Event::Group {
                        id: group,
                        name,
                        members,
                    },
                )
                .await;
        }
        Ok(Flow::Continue)
    }

    /// `GROUP REMOVE <&id> <user@net>` — remove a member. Anyone may remove
    /// themselves; only the creator may remove someone else.
    pub(super) async fn on_group_remove(
        &mut self,
        label: Option<String>,
        group: GroupId,
        user: UserRef,
        account: Account,
    ) -> io::Result<Flow> {
        let me = UserRef::new(account, self.ctx.info.network.clone());
        if !self.member_or_deny(group, &me, &label).await? {
            return Ok(Flow::Continue);
        }
        if user != me {
            let is_creator = matches!(
                self.ctx.groups.group(group).await,
                Ok(Some(rec)) if rec.creator == me
            );
            if !is_creator {
                return self.no_such_target(label).await; // not allowed = hidden
            }
        }
        self.leave_group(label, group, user, me).await
    }

    /// `GROUP LEAVE <&id>` — leave a group (remove yourself).
    pub(super) async fn on_group_leave(
        &mut self,
        label: Option<String>,
        group: GroupId,
        account: Account,
    ) -> io::Result<Flow> {
        let me = UserRef::new(account, self.ctx.info.network.clone());
        self.leave_group(label, group, me.clone(), me).await
    }

    /// Shared remove-a-member path: `who` leaves `group` (initiated by `actor`).
    async fn leave_group(
        &mut self,
        label: Option<String>,
        group: GroupId,
        who: UserRef,
        _actor: UserRef,
    ) -> io::Result<Flow> {
        // Capture members *before* the removal so the leaver is told too.
        let members = self.group_members(group).await?;
        match self.ctx.groups.remove_group_member(group, &who).await {
            Ok(true) => {}
            Ok(false) => return self.no_such_target(label).await,
            Err(e) => return self.internal(label, &e).await,
        }
        let event = Event::GroupMember {
            group,
            user: who.clone(),
            action: MemberAction::Part,
        };
        self.send_event(label, event.clone()).await?;
        self.notify_group(&members, event, Some(&who.account)).await;
        Ok(Flow::Continue)
    }

    /// `GROUP NAME <&id> [:name]` — set/clear the group's name (any member).
    pub(super) async fn on_group_name(
        &mut self,
        label: Option<String>,
        group: GroupId,
        name: Option<String>,
        account: Account,
    ) -> io::Result<Flow> {
        let me = UserRef::new(account, self.ctx.info.network.clone());
        if !self.member_or_deny(group, &me, &label).await? {
            return Ok(Flow::Continue);
        }
        match self.ctx.groups.set_group_name(group, name.as_deref()).await {
            Ok(true) => {}
            Ok(false) => return self.no_such_target(label).await,
            Err(e) => return self.internal(label, &e).await,
        }
        let members = self.group_members(group).await?;
        let event = Event::Group {
            id: group,
            name,
            members: members.clone(),
        };
        self.send_event(label, event.clone()).await?;
        self.notify_group(&members, event, Some(&me.account)).await;
        Ok(Flow::Continue)
    }

    /// `GROUPS` — the caller's group DMs, one `GROUP` per group (a `BATCH`).
    pub(super) async fn on_groups(
        &mut self,
        label: Option<String>,
        account: Account,
    ) -> io::Result<Flow> {
        let me = UserRef::new(account, self.ctx.info.network.clone());
        let groups = match self.ctx.groups.groups_for(&me).await {
            Ok(g) => g,
            Err(e) => return self.internal(label, &e).await,
        };

        self.batches += 1;
        let id = format!("gr{}", self.batches);
        self.send_event(label.clone(), Event::BatchStart { id: id.clone() })
            .await?;
        for rec in groups {
            let members = self.group_members(rec.id).await?;
            self.send_event(
                None,
                Event::Group {
                    id: rec.id,
                    name: rec.name,
                    members,
                },
            )
            .await?;
        }
        self.send_event(
            label,
            Event::BatchEnd {
                id,
                truncated: false,
                compacted: false,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    // ---- helpers ----

    /// Membership gate: `true` = a member (proceed); `false` = denied and a
    /// uniform `NO-SUCH-TARGET` has been sent (never leaks membership).
    async fn member_or_deny(
        &mut self,
        group: GroupId,
        me: &UserRef,
        label: &Option<String>,
    ) -> io::Result<bool> {
        match self.ctx.groups.is_group_member(group, me).await {
            Ok(true) => Ok(true),
            Ok(false) => {
                self.no_such_target(label.clone()).await?;
                Ok(false)
            }
            Err(e) => {
                self.internal(label.clone(), &e).await?;
                Ok(false)
            }
        }
    }

    async fn group_members(&mut self, group: GroupId) -> io::Result<Vec<UserRef>> {
        self.ctx
            .groups
            .group_members(group)
            .await
            .or_else(|_| Ok(Vec::new()))
    }

    async fn group_name(&self, group: GroupId) -> Option<String> {
        self.ctx
            .groups
            .group(group)
            .await
            .ok()
            .flatten()
            .and_then(|r| r.name)
    }

    // ---- cross-network group messaging (§9.1 home-authoritative) ----

    /// The group's **home** network — its creator's network, the single ULID
    /// writer for the group's messages. Falls back to us if the record is missing.
    pub(super) async fn group_home(&self, group: GroupId) -> NetworkName {
        self.ctx
            .groups
            .group(group)
            .await
            .ok()
            .flatten()
            .map(|r| r.creator.network)
            .unwrap_or_else(|| self.ctx.info.network.clone())
    }

    /// The local (this-network) accounts among `members`.
    fn local_accounts(&self, members: &[UserRef]) -> Vec<Account> {
        let local = &self.ctx.info.network;
        members
            .iter()
            .filter(|u| &u.network == local)
            .map(|u| u.account.clone())
            .collect()
    }

    /// Fan a **home-minted** group message out to every other member network to
    /// ingest (the origin msgid is preserved — invariant 2).
    pub(super) async fn fanout_group_message(
        &self,
        group: GroupId,
        members: &[UserRef],
        sender: &UserRef,
        msg: &MessageEvent,
    ) {
        let local = self.ctx.info.network.clone();
        let mut seen: std::collections::HashSet<NetworkName> = std::collections::HashSet::new();
        for member in members {
            if member.network == local || !seen.insert(member.network.clone()) {
                continue;
            }
            let cmd = Command::GroupRelay {
                group,
                sender: sender.clone(),
                msgid: Some(msg.msgid.clone()),
                body: msg.body.clone(),
            };
            if let Ok(line) = Request::new(cmd).serialize() {
                self.ctx.request_friend_deliver(crate::FriendDeliver {
                    peer: member.network.clone(),
                    from: sender.account.clone(),
                    line,
                });
            }
        }
    }

    /// Federation-internal `GROUP RELAY`: `@id` absent = a spoke relayed a post to
    /// us (the home) → mint + fan out; `@id` present = a home-minted message →
    /// ingest + deliver to our local members. (Text only for now — meta,
    /// attachments, edits/reactions across networks are a follow-up.)
    pub(super) async fn on_group_relay(
        &mut self,
        _label: Option<String>,
        group: GroupId,
        sender: UserRef,
        msgid: Option<MsgId>,
        body: String,
    ) -> io::Result<Flow> {
        let members = self.group_members(group).await?;
        let local = self.local_accounts(&members);
        match msgid {
            None => {
                if let Some(msg) = self
                    .ctx
                    .directory
                    .group_mint(u64::MAX, sender.clone(), group, local, body)
                    .await
                {
                    self.fanout_group_message(group, &members, &sender, &msg)
                        .await;
                }
            }
            Some(id) => {
                self.ctx
                    .directory
                    .group_ingest(sender, group, id, body, local)
                    .await;
            }
        }
        Ok(Flow::Continue)
    }

    /// Tunnel a `GROUP SYNC` (membership) to each remote member network.
    fn sync_group_to_remotes(
        &self,
        group: GroupId,
        creator: &UserRef,
        name: Option<String>,
        members: &[UserRef],
    ) {
        let local = self.ctx.info.network.clone();
        let mut seen: std::collections::HashSet<NetworkName> = std::collections::HashSet::new();
        for member in members {
            if member.network == local || !seen.insert(member.network.clone()) {
                continue;
            }
            let cmd = Command::GroupSync {
                group,
                creator: creator.clone(),
                name: name.clone(),
                members: members.to_vec(),
            };
            if let Ok(line) = Request::new(cmd).serialize() {
                self.ctx.request_friend_deliver(crate::FriendDeliver {
                    peer: member.network.clone(),
                    from: creator.account.clone(),
                    line,
                });
            }
        }
    }

    /// Federation-internal `GROUP SYNC`: record/refresh the group locally so it
    /// exists on this member network, then push a `GROUP` event to local members.
    pub(super) async fn on_group_sync(
        &mut self,
        _label: Option<String>,
        group: GroupId,
        creator: UserRef,
        name: Option<String>,
        members: Vec<UserRef>,
    ) -> io::Result<Flow> {
        // Idempotent: a first sync creates it; a re-sync's create is a no-op and we
        // refresh the name.
        let _ = self
            .ctx
            .groups
            .create_group(group, &creator, &members, unix_now_ms())
            .await;
        if name.is_some() {
            let _ = self.ctx.groups.set_group_name(group, name.as_deref()).await;
        }
        let event = Event::Group {
            id: group,
            name,
            members: members.clone(),
        };
        self.notify_group(&members, event, None).await;
        Ok(Flow::Continue)
    }

    // ---- group calls (social layer) ----

    /// `GROUP CALL <&group>` — start or join the group's voice call. The caller's
    /// labelled reply is a `GROUP-CALL … active`, followed by their `CALL-MEDIA`
    /// credential for **this network's** room and a roster snapshot; the group's
    /// other local members are notified so the call surfaces and they can join.
    ///
    /// **Cross-network (§16 M-lk-3b relay star)**: the network where the call
    /// starts is the *host*. It rings each remote member's network with a
    /// tunnelled `GROUP CALL` carrying its **relay leg** (`media`). A spoke
    /// network registers that ring and, when its first local member joins, mints
    /// its own room and spawns a relay bridging it to the host's room — so no
    /// client ever connects to another network's LiveKit.
    pub(super) async fn on_group_call(
        &mut self,
        label: Option<String>,
        group: GroupId,
        me: UserRef,
        media: Option<CallMediaGrant>,
    ) -> io::Result<Flow> {
        let account = me.account.clone();

        // Federated ring from the host network: register it and ring our local
        // members. `me` is the host member (foreign); we don't join them here. If
        // we were an active host and yielded (split-brain tiebreak), bridge our
        // existing room to the new host now.
        if let Some(host_leg) = media {
            if me.network == self.ctx.info.network {
                return Ok(Flow::Continue); // a client can't inject a host leg
            }
            let leg = host_leg.clone();
            if let Some(room) = self
                .ctx
                .group_call_ring(group, me.network.clone(), host_leg)
            {
                self.spawn_group_relay(&room, &me.network, &leg).await;
            }
            let members = self.group_members(group).await?;
            self.notify_group(
                &members,
                Event::GroupCallState {
                    group,
                    user: me,
                    state: CallState::Active,
                },
                None,
            )
            .await;
            return Ok(Flow::Continue);
        }

        // A local member joining.
        if !self.member_or_deny(group, &me, &label).await? {
            return Ok(Flow::Continue);
        }
        let join = self.ctx.group_call_join(group, account.clone());

        // Labelled ack: the caller is now active in the call.
        self.send_event(
            label,
            Event::GroupCallState {
                group,
                user: me.clone(),
                state: CallState::Active,
            },
        )
        .await?;

        // First local participant: a spoke brings its bridging relay up (before the
        // client joins, so audio flows immediately); the host rings the remote
        // member networks so their members can join.
        if join.first {
            match join.spoke {
                Some((host_net, host_leg)) => {
                    self.spawn_group_relay(&join.room, &host_net, &host_leg)
                        .await
                }
                None => {
                    self.ring_remote_group_members(group, &me, &join.room)
                        .await?
                }
            }
        }

        // This network's media credential for the group room.
        if let Some(backend) = self.ctx.voice_backend().cloned() {
            if let Some(grant) = backend.room_grant(&join.room, &account, true).await {
                self.send_event(
                    None,
                    Event::CallMedia {
                        room: join.room.clone(),
                        mode: grant.mode,
                        token: grant.token,
                        endpoint: grant.endpoint,
                    },
                )
                .await?;
            }
        }

        // Announce to local members + roster snapshot to the joiner.
        if join.newly {
            let members = self.group_members(group).await?;
            self.notify_group(
                &members,
                Event::GroupCallState {
                    group,
                    user: me.clone(),
                    state: CallState::Active,
                },
                Some(&account),
            )
            .await;
            for other in self.ctx.group_call_participants(group) {
                if other == account {
                    continue;
                }
                self.send_event(
                    None,
                    Event::GroupCallState {
                        group,
                        user: UserRef::new(other, self.ctx.info.network.clone()),
                        state: CallState::Active,
                    },
                )
                .await?;
            }
            // Cross-network roster: tell every other member network we joined, and
            // ask each for its participants (the snapshot for our client).
            self.broadcast_roster(group, &me, true, true).await?;
        }
        Ok(Flow::Continue)
    }

    /// `GROUP HANGUP <&group>` — leave the group's voice call. Releases this
    /// network's relay when the last local member leaves (a spoke). Not being in
    /// the call is the uniform `NO-SUCH-TARGET`.
    pub(super) async fn on_group_call_leave(
        &mut self,
        label: Option<String>,
        group: GroupId,
        account: Account,
    ) -> io::Result<Flow> {
        let me = UserRef::new(account.clone(), self.ctx.info.network.clone());
        let Some(left) = self.ctx.group_call_leave(group, &account) else {
            return self.no_such_target(label).await;
        };
        // The last local member left a spoke → tear the bridging relay down.
        if left.empty {
            if let Some(host_net) = left.host_net {
                self.ctx.relay_release(&host_net, &left.room).await;
            }
        }
        self.send_event(
            label,
            Event::GroupCallState {
                group,
                user: me.clone(),
                state: CallState::Ended,
            },
        )
        .await?;
        let members = self.group_members(group).await?;
        self.notify_group(
            &members,
            Event::GroupCallState {
                group,
                user: me.clone(),
                state: CallState::Ended,
            },
            Some(&account),
        )
        .await;
        // Tell every other member network we left.
        self.broadcast_roster(group, &me, false, false).await?;
        Ok(Flow::Continue)
    }

    /// Federation-internal `GROUP ROSTER`: a member on another network joined /
    /// left the call — re-emit it to our local members so their roster is complete.
    /// If `reply`, send our own current participants back (the snapshot).
    pub(super) async fn on_group_call_roster(
        &mut self,
        _label: Option<String>,
        group: GroupId,
        user: UserRef,
        active: bool,
        reply: bool,
    ) -> io::Result<Flow> {
        let members = self.group_members(group).await?;
        self.notify_group(
            &members,
            Event::GroupCallState {
                group,
                user: user.clone(),
                state: if active {
                    CallState::Active
                } else {
                    CallState::Ended
                },
            },
            None,
        )
        .await;
        if reply {
            for account in self.ctx.group_call_participants(group) {
                let participant = UserRef::new(account, self.ctx.info.network.clone());
                self.deliver_roster(&user.network, group, &participant, true, false);
            }
        }
        Ok(Flow::Continue)
    }

    /// Tunnel a `GROUP ROSTER` update for `user` to every **other** member network.
    async fn broadcast_roster(
        &mut self,
        group: GroupId,
        user: &UserRef,
        active: bool,
        reply: bool,
    ) -> io::Result<()> {
        let members = self.group_members(group).await?;
        let local = self.ctx.info.network.clone();
        let mut seen: std::collections::HashSet<NetworkName> = std::collections::HashSet::new();
        for member in &members {
            if member.network == local || !seen.insert(member.network.clone()) {
                continue;
            }
            self.deliver_roster(&member.network, group, user, active, reply);
        }
        Ok(())
    }

    /// Tunnel one `GROUP ROSTER` line for `user` to `net`.
    fn deliver_roster(
        &self,
        net: &NetworkName,
        group: GroupId,
        user: &UserRef,
        active: bool,
        reply: bool,
    ) {
        let cmd = Command::GroupCallRoster {
            group,
            user: user.clone(),
            active,
            reply,
        };
        if let Ok(line) = Request::new(cmd).serialize() {
            self.ctx.request_friend_deliver(crate::FriendDeliver {
                peer: net.clone(),
                from: user.account.clone(),
                line,
            });
        }
    }

    /// Spawn the media relay bridging **our** group room to the host network's
    /// room (a spoke of the §16 M-lk-3b star), keyed by our room so `GROUP HANGUP`
    /// can release it. No-op with no LiveKit backend.
    async fn spawn_group_relay(
        &self,
        room: &str,
        host_net: &NetworkName,
        host_leg: &CallMediaGrant,
    ) {
        let Some(backend) = self.ctx.voice_backend().cloned() else {
            return;
        };
        let identity = format!("relay@{host_net}");
        let Some(local_relay) = backend.room_grant_for(room, &identity, true).await else {
            return;
        };
        let spec = RelaySpec {
            peer: host_net.clone(),
            key: room.to_string(),
            remote_url: host_leg.endpoint.clone().unwrap_or_default(),
            remote_room: host_leg.room.clone(),
            remote_token: host_leg.token.clone(),
            local_url: local_relay.endpoint.clone().unwrap_or_default(),
            local_room: room.to_string(),
            local_token: local_relay.token,
        };
        self.ctx.relay_acquire(spec).await;
    }

    /// The **host** rings each remote member's network: a tunnelled `GROUP CALL`
    /// carrying a per-network relay leg (a relay token for our host room), so that
    /// network can bridge into it. No leg minted with no LiveKit backend (the call
    /// still surfaces to local members).
    async fn ring_remote_group_members(
        &mut self,
        group: GroupId,
        me: &UserRef,
        room: &str,
    ) -> io::Result<()> {
        let members = self.group_members(group).await?;
        let local = &self.ctx.info.network;

        // One ring per distinct remote network.
        let mut seen: std::collections::HashSet<NetworkName> = std::collections::HashSet::new();
        for member in &members {
            if &member.network == local || !seen.insert(member.network.clone()) {
                continue;
            }
            let leg = match self.ctx.voice_backend().cloned() {
                Some(backend) => backend
                    .room_grant_for(room, &format!("relay@{}", member.network), true)
                    .await
                    .map(|g| CallMediaGrant {
                        room: g.room.unwrap_or_else(|| room.to_string()),
                        token: g.token,
                        endpoint: g.endpoint,
                    }),
                None => None,
            };
            let cmd = Command::GroupCall { group, media: leg };
            if let Ok(line) = Request::new(cmd).serialize() {
                self.ctx.request_friend_deliver(crate::FriendDeliver {
                    peer: member.network.clone(),
                    from: me.account.clone(),
                    line,
                });
            }
        }
        Ok(())
    }
}
