//! §6.3 channel administration: CREATE / POLICY / META / DELETE / RENAME.

use super::*;

impl<S: ControlStream> Session<S> {
    pub(super) async fn on_channel_create(
        &mut self,
        label: Option<String>,
        channel: ChannelName,
        policy: Option<RetentionPolicy>,
        kind: ChannelKind,
        actor: Actor,
    ) -> io::Result<Flow> {
        // A namespaced channel (#ns/chan) needs its namespace to exist;
        // the owner (or an ns-admin/chan-create holder) may create it.
        if let Some(ns) = channel.namespace() {
            if !self.namespace_exists(ns).await {
                return self.no_such_target(label).await;
            }
        }
        let policy = policy.unwrap_or_else(|| "retained:90d".parse().expect("valid default"));
        if policy == RetentionPolicy::E2ee {
            return self.unsupported(label, "e2ee channels land in M6").await;
        }
        let scope = TokenScope::Channel(channel.to_string());
        match self
            .ctx
            .actor_has_cap(&actor, &Capability::ChanCreate, &scope, unix_now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "chan-create").await,
            Err(e) => return self.internal(label, &e).await,
        }
        match self.ctx.registry.create(channel.clone(), policy) {
            None => {
                self.send_err(label, ErrCode::Conflict, None, "channel already exists")
                    .await?;
                Ok(Flow::Continue)
            }
            Some(_) => {
                if let Err(e) = self
                    .ctx
                    .channel_store
                    .upsert_channel(&channel, policy, kind)
                    .await
                {
                    return self.internal(label, &e).await;
                }
                debug!(%channel, ?kind, "channel created");
                self.send_event(label, Event::Policy { channel, policy })
                    .await?;
                Ok(Flow::Continue)
            }
        }
    }

    pub(super) async fn on_channel_policy(
        &mut self,
        label: Option<String>,
        channel: ChannelName,
        policy: RetentionPolicy,
        purge: bool,
        actor: Actor,
    ) -> io::Result<Flow> {
        if !self.ctx.registry.exists(&channel) {
            return self.no_such_target(label).await;
        }
        if policy == RetentionPolicy::E2ee {
            return self.unsupported(label, "e2ee transitions land in M6").await;
        }
        let scope = TokenScope::Channel(channel.to_string());
        match self
            .ctx
            .actor_has_cap(&actor, &Capability::Policy, &scope, unix_now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "policy").await,
            Err(e) => return self.internal(label, &e).await,
        }
        if let Err(e) = self
            .ctx
            .channel_store
            // Kind is immutable after creation; `upsert` only updates the policy
            // for an existing channel, so the passed kind is inert here.
            .upsert_channel(&channel, policy, ChannelKind::Text)
            .await
        {
            return self.internal(label, &e).await;
        }
        if let Some(handle) = self.ctx.registry.get(&channel) {
            handle.set_policy(self.id, policy).await; // broadcasts POLICY to members
        }
        if purge {
            // Tightening purges now (§6.3): drop everything currently stored.
            if let Err(e) = self
                .ctx
                .events
                .purge_before(&Scope::Channel(channel.clone()), unix_now() * 1000)
                .await
            {
                return self.internal(label, &e).await;
            }
        }
        // Labeled ack to the actor's own session (members got the broadcast).
        self.send_event(label, Event::Policy { channel, policy })
            .await?;
        Ok(Flow::Continue)
    }

    pub(super) async fn on_channel_meta(
        &mut self,
        label: Option<String>,
        channel: ChannelName,
        key: String,
        value: String,
        actor: Actor,
    ) -> io::Result<Flow> {
        if !self.ctx.registry.exists(&channel) {
            return self.no_such_target(label).await;
        }
        let scope = TokenScope::Channel(channel.to_string());
        match self
            .ctx
            .actor_has_cap(&actor, &Capability::Pin, &scope, unix_now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "pin").await,
            Err(e) => return self.internal(label, &e).await,
        }
        let result = match key.as_str() {
            "topic" => {
                self.ctx
                    .channel_store
                    .set_channel_topic(&channel, &value)
                    .await
            }
            "view-gated" => {
                let gated = matches!(value.as_str(), "yes" | "true" | "on" | "1");
                self.ctx
                    .channel_store
                    .set_channel_view_gated(&channel, gated)
                    .await
            }
            // §6.7 posting mode: `restricted` requires the `send` cap to post.
            "posting" => {
                let restricted = matches!(value.as_str(), "restricted" | "locked");
                self.ctx
                    .channel_store
                    .set_channel_restricted(&channel, restricted)
                    .await
            }
            // Layout (spec extension): category groups channels, position
            // orders them. Both read the current record to preserve the
            // other field.
            "category" | "position" => {
                let current = match self.ctx.channel_store.channel(&channel).await {
                    Ok(Some(record)) => record,
                    Ok(None) => return self.no_such_target(label).await,
                    Err(e) => return self.internal(label, &e).await,
                };
                let (category, position) = if key == "category" {
                    let cat = (!value.is_empty()).then(|| value.clone());
                    (cat, current.position)
                } else {
                    let Ok(pos) = value.parse::<i64>() else {
                        self.send_err(label, ErrCode::Policy, None, "position must be an integer")
                            .await?;
                        return Ok(Flow::Continue);
                    };
                    (current.category, pos)
                };
                self.ctx
                    .channel_store
                    .set_channel_layout(&channel, category.as_deref(), position)
                    .await
            }
            _ => {
                self.send_err(
                    label,
                    ErrCode::Policy,
                    None,
                    "meta key must be topic|view-gated|posting|category|position",
                )
                .await?;
                return Ok(Flow::Continue);
            }
        };
        if let Err(e) = result {
            return self.internal(label, &e).await;
        }
        // Layout changes broadcast to the channel's members so every client
        // re-renders from server state (no client-only ordering).
        if key == "category" || key == "position" {
            if let (Ok(Some(rec)), Some(handle)) = (
                self.ctx.channel_store.channel(&channel).await,
                self.ctx.registry.get(&channel),
            ) {
                handle
                    .announce(Event::ChannelLayout {
                        channel: channel.clone(),
                        category: rec.category,
                        position: rec.position,
                        kind: rec.kind,
                    })
                    .await;
            }
        }
        self.send_event(
            label,
            Event::Chanmeta {
                channel,
                key,
                value,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    pub(super) async fn on_channel_delete(
        &mut self,
        label: Option<String>,
        channel: ChannelName,
        confirm: ChannelName,
        actor: Actor,
    ) -> io::Result<Flow> {
        if channel != confirm {
            self.send_err(
                label,
                ErrCode::Policy,
                None,
                "DELETE must repeat the channel name",
            )
            .await?;
            return Ok(Flow::Continue);
        }
        if !self.ctx.registry.exists(&channel) {
            return self.no_such_target(label).await;
        }
        let scope = TokenScope::Channel(channel.to_string());
        // ns-admin covers channels in a namespace; operators cover all.
        match self
            .ctx
            .actor_has_cap(&actor, &Capability::NsAdmin, &scope, unix_now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "ns-admin").await,
            Err(e) => return self.internal(label, &e).await,
        }
        self.ctx.registry.remove(&channel); // drops the actor handle
        if let Err(e) = self.ctx.channel_store.delete_channel(&channel).await {
            return self.internal(label, &e).await;
        }
        debug!(%channel, "channel deleted");
        self.send_event(
            label,
            Event::Chanmeta {
                channel,
                key: "deleted".to_string(),
                value: String::new(),
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    /// CHANNEL RENAME — change a channel's identity within its namespace (§6.3),
    /// re-keying every scoped record (invariant 4: cap first). The store move is
    /// atomic; the actor is respawned under the new name and members are told
    /// via `CHANNEL-RENAMED` so their clients re-join the new identity.
    pub(super) async fn on_channel_rename(
        &mut self,
        label: Option<String>,
        channel: ChannelName,
        new_name: ChannelName,
        account: Account,
    ) -> io::Result<Flow> {
        // A rename stays within one namespace (moving across namespaces would
        // change ownership/authority — that's not a rename).
        if channel.namespace() != new_name.namespace() {
            self.send_err(
                label,
                ErrCode::Policy,
                None,
                "rename must stay within the same namespace",
            )
            .await?;
            return Ok(Flow::Continue);
        }
        if channel == new_name {
            return self
                .send_event(
                    label,
                    Event::ChannelRenamed {
                        old: channel.clone(),
                        new: new_name,
                    },
                )
                .await
                .map(|()| Flow::Continue);
        }
        // Anti-enumeration: absent source is indistinguishable from unauthorized.
        if !self.ctx.registry.exists(&channel) {
            return self.no_such_target(label).await;
        }
        // Invariant 4: verify the cap before any mutation. ns-admin covers a
        // namespace's channels (operators cover all) — same authority as DELETE.
        let scope = TokenScope::Channel(channel.to_string());
        match self
            .ctx
            .account_has_cap(&account, &Capability::NsAdmin, &scope, unix_now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "ns-admin").await,
            Err(e) => return self.internal(label, &e).await,
        }
        if self.ctx.registry.exists(&new_name) {
            self.send_err(
                label,
                ErrCode::Conflict,
                None,
                "target channel name already exists",
            )
            .await?;
            return Ok(Flow::Continue);
        }
        // Policy needed to respawn the actor under the new name.
        let policy = match self.ctx.channel_store.channel(&channel).await {
            Ok(Some(record)) => record.policy,
            Ok(None) => return self.no_such_target(label).await,
            Err(e) => return self.internal(label, &e).await,
        };
        // Re-key the store atomically (grants, membership, roles, holds, pins,
        // history — invariants 4 & 11 preserved because the scope moves whole).
        match self
            .ctx
            .channel_store
            .rename_channel(&channel, &new_name)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                self.send_err(label, ErrCode::Conflict, None, "rename failed")
                    .await?;
                return Ok(Flow::Continue);
            }
            Err(e) => return self.internal(label, &e).await,
        }
        // Tell current members via the OLD actor's broadcast BEFORE swapping —
        // buffered broadcasts still drain to their forwarders after the drop.
        if let Some(handle) = self.ctx.registry.get(&channel) {
            handle
                .announce(Event::ChannelRenamed {
                    old: channel.clone(),
                    new: new_name.clone(),
                })
                .await;
        }
        self.ctx.registry.rename(&channel, new_name.clone(), policy);
        debug!(%channel, %new_name, "channel renamed");
        // Direct (labeled) ack to the initiator.
        self.send_event(
            label,
            Event::ChannelRenamed {
                old: channel,
                new: new_name,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    /// §11.13 a federated channel message arriving over the bridge. `msgid`
    /// **absent** = a spoke relayed a member's post to us (the home): mint it into
    /// the one total order — the ordinary event mirror then fans the home-origin
    /// message out one hop to every spoke. `msgid` **present** = a home-minted
    /// message for us (a spoke) to ingest, persisted with the origin msgid intact
    /// (invariant 2) — e.g. a `CHANNEL BACKFILL` replay.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn on_channel_relay(
        &mut self,
        _label: Option<String>,
        channel: ChannelName,
        sender: UserRef,
        msgid: Option<MsgId>,
        body: String,
        meta: MsgMeta,
        _echo: Option<String>,
    ) -> io::Result<Flow> {
        match msgid {
            None => {
                // Only the home may mint; a misrouted relay is silently dropped
                // (anti-enumeration — a non-home network reveals nothing).
                if self.ctx.registry.is_home(&channel) {
                    if let Some(handle) = self.ctx.registry.get(&channel) {
                        handle.relay_publish(sender, body, meta).await;
                    }
                }
            }
            Some(id) => {
                if let Some(handle) = self.ctx.registry.get(&channel) {
                    let event = Event::Message(Box::new(MessageEvent {
                        target: Target::Channel(channel.clone()),
                        sender: sender.clone(),
                        msgid: id.clone(),
                        body: body.clone(),
                        meta: meta.clone(),
                        edited: None,
                        edited_at: None,
                    }));
                    let record = EventRecord {
                        scope: Scope::Channel(channel),
                        msgid: id.clone(),
                        root: id,
                        sender,
                        kind: EventKind::Message { body, meta },
                    };
                    // No local session owns this — fan out to every member.
                    handle.ingest(u64::MAX, record, event).await;
                }
            }
        }
        Ok(Flow::Continue)
    }

    /// §11.13 relay a channel mutation to the channel's **home** (we're a spoke;
    /// only the home applies it). The resulting EDITED/DELETED/REACTION returns
    /// over the ordinary event mirror.
    pub(super) fn relay_channel_mut(
        &self,
        home: NetworkName,
        channel: ChannelName,
        sender: &UserRef,
        root: MsgId,
        op: &str,
        arg: String,
    ) {
        let cmd = Command::ChannelMut {
            channel,
            sender: sender.clone(),
            root,
            op: op.to_string(),
            arg,
            msgid: None,
        };
        if let Ok(line) = Request::new(cmd).serialize() {
            self.ctx.request_friend_deliver(crate::FriendDeliver {
                peer: home,
                from: sender.account.clone(),
                line,
            });
        }
    }

    /// §11.13 federation-internal `CHANNEL MUT`. `@id` **absent** = a spoke relayed
    /// a member's mutation to us (the home): verify authorship (§11.4) and apply —
    /// the event mirror fans the EDITED/DELETED/REACTION out to the spokes. `@id`
    /// **present** = a home-applied mutation for us (a spoke) to ingest — a
    /// `CHANNEL BACKFILL` replay (live home→spoke rides the mirror, so this is the
    /// recovery path only).
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn on_channel_mut(
        &mut self,
        label: Option<String>,
        channel: ChannelName,
        sender: UserRef,
        root: MsgId,
        op: String,
        arg: String,
        msgid: Option<MsgId>,
    ) -> io::Result<Flow> {
        match msgid {
            None => {
                // Only the home applies; a misrouted relay reveals nothing (§2.2).
                if !self.ctx.registry.is_home(&channel) {
                    return Ok(Flow::Continue);
                }
                // §11.4 authored-by: EDIT/DELETE require the sender to own the
                // target message (REACT is open to any member). The home is the
                // authority, so it re-checks regardless of the spoke's own gate.
                if op == "edit" || op == "delete" {
                    match self.ctx.events.find_root(root.ulid()).await {
                        Ok(Some(target)) if target.sender == sender => {}
                        // Not the author, or the message is gone → silently drop.
                        Ok(_) => return Ok(Flow::Continue),
                        Err(e) => return self.internal(label, &e).await,
                    }
                }
                if let Some(handle) = self.ctx.registry.get(&channel) {
                    handle.relay_mutate(sender, root, op, arg).await;
                }
            }
            Some(_id) => {
                // Backfill replay of a mutation (deferred; live home→spoke
                // mutations arrive over the event mirror, and CHANNEL BACKFILL
                // replays message roots only — as the group backfill does).
            }
        }
        Ok(Flow::Continue)
    }

    /// §11.13 spoke → home recovery: ask the channel's home to replay anything it
    /// minted while we were unreachable, carrying our newest local root as the
    /// cursor (`None` = replay all). No-op when we are the home.
    pub(super) async fn request_channel_home_backfill(&mut self, channel: ChannelName) {
        let home = self.ctx.registry.home(&channel);
        if home == self.ctx.info.network {
            return;
        }
        let State::Ready { account } = self.state.clone() else {
            return;
        };

        // Our newest local root is the cursor (same shape as the group backfill).
        let scope = Scope::Channel(channel.clone());
        let page = weft_store::Page {
            before: None,
            after: None,
            limit: 1,
        };
        let after = match self.ctx.events.roots(&scope, page).await {
            Ok(roots) => roots.last().map(|r| r.msgid.clone()),
            Err(_) => None,
        };

        let cmd = Command::ChannelBackfill { channel, after };
        if let Ok(line) = Request::new(cmd).serialize() {
            self.ctx.request_friend_deliver(crate::FriendDeliver {
                peer: home,
                from: account,
                line,
            });
        }
    }

    /// §11.13 home → spoke recovery: replay this channel's message roots after the
    /// caller's cursor as `CHANNEL RELAY` (`@id` present) ingests. We must be the
    /// home; the caller's network must mirror the channel in the acked manifest
    /// (else it gets nothing — anti-enumeration, §11.1). Message roots only —
    /// mutations ride the live mirror (matching the group backfill).
    pub(super) async fn on_channel_backfill(
        &mut self,
        label: Option<String>,
        caller: UserRef,
        channel: ChannelName,
        after: Option<MsgId>,
    ) -> io::Result<Flow> {
        if !self.ctx.registry.is_home(&channel) {
            return Ok(Flow::Continue);
        }
        // Gate on the acked manifest: only a peer that mirrors this channel may
        // pull it (same forwardability check as live ingestion).
        let bridged = self
            .ctx
            .peers
            .peer(&caller.network)
            .await
            .ok()
            .flatten()
            .map(|p| crate::bridge::is_forwardable(&p, channel.as_str()))
            .unwrap_or(false);
        if !bridged {
            return Ok(Flow::Continue);
        }

        let scope = Scope::Channel(channel.clone());
        let page = weft_store::Page {
            before: None,
            after: after.as_ref().map(|m| m.ulid()),
            limit: weft_proto::MAX_HISTORY_LIMIT as usize,
        };
        let roots = match self.ctx.events.roots(&scope, page).await {
            Ok(roots) => roots,
            Err(e) => return self.internal(label, &e).await,
        };

        for root in roots {
            let EventKind::Message { body, meta } = root.kind else {
                continue;
            };
            // Skip server-generated system lines (join/part) — they are re-derived
            // locally, never replayed across the bridge.
            if meta.system.is_some() {
                continue;
            }
            let cmd = Command::ChannelRelay {
                channel: channel.clone(),
                sender: root.sender,
                msgid: Some(root.msgid),
                body,
                meta,
                echo: None,
            };
            if let Ok(line) = Request::new(cmd).serialize() {
                self.ctx.request_friend_deliver(crate::FriendDeliver {
                    peer: caller.network.clone(),
                    from: caller.account.clone(),
                    line,
                });
            }
        }
        Ok(Flow::Continue)
    }
}
