//! §6.3 / §9 relay handlers: JOIN/PART/MSG/EDIT/DELETE/REACT/HISTORY/
//! MEMBERS/PIN/PINS/MARK/TYPING.

use super::*;
use crate::directory::GroupMutKind;

impl<S: ControlStream> Session<S> {
    pub(super) async fn on_join(
        &mut self,
        label: Option<String>,
        channel: ChannelName,
        invite: Option<String>,
        account: Account,
    ) -> io::Result<Flow> {
        if invite.is_some() {
            // JOIN with an invite-ref is INVITE REDEEM territory; redeem
            // directly (§6.5) rather than here.
            return self
                .unsupported(label, "use INVITE REDEEM to redeem an invite")
                .await;
        }
        match self.join_one(&channel, &account, label.clone()).await? {
            JoinResult::Joined => Ok(Flow::Continue),
            JoinResult::Banned => {
                self.send_err(label, ErrCode::Banned, None, "you are banned")
                    .await?;
                Ok(Flow::Continue)
            }
            // §2.2 anti-enumeration: unknown and hidden collapse to one code.
            JoinResult::Missing | JoinResult::Hidden => self.no_such_target(label).await,
            JoinResult::Unavailable => {
                self.send_err(label, ErrCode::Internal, None, "channel unavailable")
                    .await?;
                Ok(Flow::Continue)
            }
        }
    }

    /// Join one channel: registry lookup, view-gate + ban checks, subscribe,
    /// and emit the §6.3 `MEMBER` + `POLICY` response (with `label`). Shared by
    /// `JOIN` and `NS JOIN`; the caller maps the non-`Joined` results to errors.
    pub(super) async fn join_one(
        &mut self,
        channel: &ChannelName,
        account: &Account,
        label: Option<String>,
    ) -> io::Result<JoinResult> {
        let Some(handle) = self.ctx.registry.get(channel) else {
            return Ok(JoinResult::Missing);
        };
        // §16 a voice channel is not text-joinable: a text JOIN answers
        // NO-SUCH-TARGET, which also keeps voice channels invisible to the IRC
        // gateway (§17). Voice is entered via VOICE JOIN.
        if self.channel_kind(channel).await == ChannelKind::Voice {
            return Ok(JoinResult::Missing);
        }
        if self.view_gated_denied(channel, account).await {
            return Ok(JoinResult::Hidden);
        }
        if self
            .ctx
            .moderation
            .is_moderated(account, &covering_scopes(channel), ModKind::Ban)
            .await
            .unwrap_or(false)
        {
            return Ok(JoinResult::Banned);
        }
        // A persistent "joined" system line is posted only on a genuine *first*
        // join (no membership recorded yet) — auto-rejoin on reconnect must not
        // repost it, or reloading would spam the channel.
        let first_join = !self
            .ctx
            .memberships
            .memberships(account)
            .await
            .map(|chans| chans.contains(channel))
            .unwrap_or(false);
        let Some(ack) = handle.join(self.id, account.clone(), first_join).await else {
            return Ok(JoinResult::Unavailable);
        };
        // Re-JOIN replaces the subscription; pending echo labels die with the
        // old receiver (their broadcasts went there), so drop them too.
        if let Some(old) = self.joined.remove(channel) {
            old.forwarder.abort();
        }
        let forwarder = spawn_forwarder(channel.clone(), ack.events, self.events_tx.clone());
        self.joined.insert(
            channel.clone(),
            Joined {
                handle,
                policy: ack.policy,
                forwarder,
                pending: VecDeque::new(),
            },
        );
        debug!(%channel, members = ack.count, "joined");
        let me = UserRef::new(account.clone(), self.ctx.info.network.clone());
        self.send_event(
            label.clone(),
            Event::Member {
                channel: channel.clone(),
                user: me,
                action: MemberAction::Join,
                display: None,
                count: Some(ack.count),
            },
        )
        .await?;
        self.send_event(
            label,
            Event::Policy {
                channel: channel.clone(),
                policy: ack.policy,
            },
        )
        .await?;
        // §6.3 persist membership for auto-rejoin on the next auth.
        if let Err(e) = self.ctx.memberships.set_membership(account, channel).await {
            error!("persist membership failed: {e}");
        }
        Ok(JoinResult::Joined)
    }

    pub(super) async fn on_part(
        &mut self,
        label: Option<String>,
        channel: ChannelName,
    ) -> io::Result<Flow> {
        let State::Ready { account } = self.state.clone() else {
            unreachable!("on_part only dispatched in READY");
        };
        match self.joined.remove(&channel) {
            None => self.no_such_target(label).await,
            Some(joined) => {
                joined.forwarder.abort();
                // Explicit PART = a genuine leave (clears membership) → post the
                // persistent "left" system line.
                joined.handle.part(self.id, true).await;
                // §6.3 drop the persistent membership — no auto-rejoin.
                if let Err(e) = self
                    .ctx
                    .memberships
                    .clear_membership(&account, &channel)
                    .await
                {
                    error!("clear membership failed: {e}");
                }
                // Direct ack mirrors the JOIN response shape; the broadcast
                // copy goes to remaining members only.
                let me = UserRef::new(account, self.ctx.info.network.clone());
                self.send_event(
                    label,
                    Event::Member {
                        channel,
                        user: me,
                        action: MemberAction::Part,
                        display: None,
                        count: None,
                    },
                )
                .await?;
                Ok(Flow::Continue)
            }
        }
    }

    pub(super) async fn on_typing(
        &mut self,
        label: Option<String>,
        channel: ChannelName,
        state: weft_proto::TypingState,
    ) -> io::Result<Flow> {
        match self.joined.get(&channel).map(|j| j.handle.clone()) {
            None => self.not_member(label, &channel).await,
            Some(handle) => {
                // Relay only — never stored, no direct response (§6.3).
                handle.typing(self.id, state).await;
                Ok(Flow::Continue)
            }
        }
    }

    pub(super) async fn on_msg(
        &mut self,
        label: Option<String>,
        target: Target,
        body: Option<String>,
        mut meta: MsgMeta,
    ) -> io::Result<Flow> {
        // `system=` is server-only — a client can't forge a membership/system
        // line, so strip any inbound value before the message is relayed.
        meta.system = None;
        // §13 validate attachments: well-formed, SAME-NETWORK `weft-media://`
        // refs only (foreign media = M-media-3 mirroring). The `attach` cap gate
        // is a follow-up. The channel actor records the refs as it mints the msgid.
        if !meta.attachments.is_empty() {
            let net = self.ctx.info.network.to_string();
            let ok = meta.attachments.iter().all(|uri| {
                matches!(crate::media::parse_media_uri(uri), Some((origin, _)) if origin == net)
            });
            if !ok {
                self.send_err(
                    label,
                    ErrCode::Policy,
                    None,
                    "invalid or foreign media reference",
                )
                .await?;
                return Ok(Flow::Continue);
            }
        }
        // §6.4: empty body legal iff attachments.
        let body = body.unwrap_or_default();
        if body.is_empty() && meta.attachments.is_empty() {
            self.send_err(
                label,
                ErrCode::Policy,
                None,
                "empty body requires attachments",
            )
            .await?;
            return Ok(Flow::Continue);
        }
        // §9.2 dedup: a retried label replays the stored echo (the ack),
        // and a label still awaiting its echo is dropped — never republished.
        if let Some(l) = &label {
            let now = Instant::now();
            self.dedup
                .retain(|_, entry| now.duration_since(entry.at) < DEDUP_WINDOW);
            if let Some(hit) = self.dedup.get(l) {
                let line = hit.line.clone();
                self.stream.send_line(&line).await?;
                return Ok(Flow::Continue);
            }
            let in_flight = self
                .joined
                .values()
                .any(|j| j.pending.iter().any(|p| p.as_deref() == Some(l)))
                || self
                    .pending_direct
                    .iter()
                    .any(|p| p.as_deref() == Some(l.as_str()));
            if in_flight {
                return Ok(Flow::Continue);
            }
        }
        match target {
            Target::Channel(channel) => {
                if !self.joined.contains_key(&channel) {
                    return self.not_member(label, &channel).await;
                }
                // §6.7 posting gate: not banned, not muted, and (open channel
                // or holds `send`).
                let State::Ready { account } = self.state.clone() else {
                    unreachable!("on_msg only dispatched in READY");
                };
                match self.can_post(&channel, &account).await {
                    Ok(None) => {}
                    Ok(Some((code, context))) => {
                        self.send_err(label, code, Some(context), "cannot post to this channel")
                            .await?;
                        return Ok(Flow::Continue);
                    }
                    Err(e) => return self.internal(label, &e).await,
                }
                // §13 attachments to a restricted channel require `attach`.
                if !meta.attachments.is_empty() {
                    match self.can_attach(&channel, &account).await {
                        Ok(true) => {}
                        Ok(false) => return self.cap_required(label, "attach").await,
                        Err(e) => return self.internal(label, &e).await,
                    }
                }
                // §11.13 home-authoritative: if this network owns the channel's
                // namespace we mint locally (the common case, unchanged). Otherwise
                // we're a spoke — relay the post to the home to be minted into the
                // one total order; the minted copy returns over the event mirror and
                // the client reconciles it by `meta.nonce`.
                if self.ctx.registry.is_home(&channel) {
                    let joined = self
                        .joined
                        .get_mut(&channel)
                        .expect("membership checked above");
                    joined.pending.push_back(label);
                    joined.handle.publish(self.id, body, meta).await;
                } else {
                    let home = self.ctx.registry.home(&channel);
                    let me = UserRef::new(account.clone(), self.ctx.info.network.clone());
                    let cmd = Command::ChannelRelay {
                        channel,
                        sender: me,
                        msgid: None,
                        body,
                        meta,
                        echo: None,
                    };
                    if let Ok(line) = Request::new(cmd).serialize() {
                        self.ctx.request_friend_deliver(crate::FriendDeliver {
                            peer: home,
                            from: account,
                            line,
                        });
                    }
                }
            }
            // §9.5 same-network DM, routed through the account directory.
            Target::User(to) => {
                let State::Ready { account } = self.state.clone() else {
                    unreachable!("on_msg only dispatched in READY");
                };
                if !self
                    .ctx
                    .directory
                    .dm(self.id, account, to, body, meta)
                    .await
                {
                    // Unknown account — one code for everything hidden (§2.2).
                    return self.no_such_target(label).await;
                }
                self.pending_direct.push_back(label);
            }
            // Group DM: membership-gated; the directory mints the ULID (single
            // writer → group total order) and fans out to local members.
            Target::Group(group) => {
                let State::Ready { account } = self.state.clone() else {
                    unreachable!("on_msg only dispatched in READY");
                };
                let me = UserRef::new(account.clone(), self.ctx.info.network.clone());
                match self.ctx.groups.is_group_member(group, &me).await {
                    Ok(true) => {}
                    // Not a member = indistinguishable from nonexistent (§2.2).
                    Ok(false) => return self.no_such_target(label).await,
                    Err(e) => return self.internal(label, &e).await,
                }
                let members = match self.ctx.groups.group_members(group).await {
                    Ok(m) => m,
                    Err(e) => return self.internal(label, &e).await,
                };
                let local_net = &self.ctx.info.network;
                let local: Vec<Account> = members
                    .iter()
                    .filter(|u| &u.network == local_net)
                    .map(|u| u.account.clone())
                    .collect();
                let has_remote = members.iter().any(|u| &u.network != local_net);

                if !has_remote {
                    // Same-network group: the local directory is the single writer.
                    self.ctx
                        .directory
                        .group_msg(self.id, account, group, local, body, meta)
                        .await;
                    self.pending_direct.push_back(label);
                } else {
                    // Cross-network: the group's **home** (creator's network) is the
                    // single ULID writer (§9.1). If we're home, mint + fan out; else
                    // relay the post there (the poster's copy arrives via ingest).
                    let home = self.group_home(group).await;
                    if home == *local_net {
                        if let Some(msg) = self
                            .ctx
                            .directory
                            .group_mint(self.id, me.clone(), group, local, body, meta)
                            .await
                        {
                            self.fanout_group_message(group, &members, &me, &msg, None)
                                .await;
                        }
                        self.pending_direct.push_back(label);
                    } else {
                        // Spoke: relay to the home, carrying an echo token so we can
                        // deliver the home's minted copy back as the poster's own
                        // (labelled) message.
                        let token = weft_proto::Ulid::new().to_string();
                        self.ctx
                            .register_group_echo(token.clone(), self.id, unix_now_ms());
                        self.pending_direct.push_back(label);
                        let cmd = Command::GroupRelay {
                            group,
                            sender: me.clone(),
                            msgid: None,
                            body,
                            meta,
                            echo: Some(token),
                        };
                        if let Ok(line) = Request::new(cmd).serialize() {
                            self.ctx.request_friend_deliver(crate::FriendDeliver {
                                peer: home,
                                from: account,
                                line,
                            });
                        }
                    }
                }
            }
        }
        // The ack is the echoed MESSAGE, sent when the broadcast returns.
        Ok(Flow::Continue)
    }

    // ---- message mutations (§6.4 EDIT / DELETE / REACT) ----

    /// Locate the scope a msgid lives in and run the checks shared by
    /// EDIT/DELETE/REACT: origin authority, existence (tombstoned, foreign,
    /// or other people's DM msgids all answer NO-SUCH-TARGET, §2.2/§8),
    /// membership/participation, and — for edit/delete — authorship
    /// (`edit-own`/`delete-own`; `delete-any` arrives with capability
    /// tokens in M4).
    ///
    /// `Ok(None)` = refused, error already sent.
    pub(super) async fn resolve_message(
        &mut self,
        label: Option<String>,
        msgid: &MsgId,
        account: &Account,
        cap: &'static str,
        must_be_author: bool,
    ) -> io::Result<Option<MessageRoute>> {
        let root = match self.ctx.events.find_root(msgid.ulid()).await {
            Err(e) => {
                self.internal(label, &e).await?;
                return Ok(None);
            }
            Ok(None) => {
                // §11.4 + anti-enumeration: a **foreign-origin** msgid we don't
                // have answers the uniform FORBIDDEN `origin` — the same as a
                // foreign message we *do* have (a bridged channel), so probing
                // can't tell whether we hold it. A missing **local**-origin msgid
                // is NO-SUCH-TARGET. (A group message we participate in is never
                // missing here — it was ingested — so it reaches the scope match.)
                if msgid.origin() != &self.ctx.info.network {
                    self.send_err(
                        label,
                        ErrCode::Forbidden,
                        Some("origin"),
                        "not this message's origin",
                    )
                    .await?;
                } else {
                    self.no_such_target(label).await?;
                }
                return Ok(None);
            }
            Ok(Some(root)) => root,
        };
        match self.ctx.events.is_deleted(&root.scope, msgid.ulid()).await {
            Err(e) => {
                self.internal(label, &e).await?;
                return Ok(None);
            }
            Ok(true) => {
                // A tombstoned msgid is indistinguishable from an expired
                // one — same code (§2.2).
                self.no_such_target(label).await?;
                return Ok(None);
            }
            Ok(false) => {}
        }
        match root.scope.clone() {
            Scope::Channel(channel) => {
                let Some(joined) = self.joined.get(&channel) else {
                    self.not_member_cap(label, &channel, cap).await?;
                    return Ok(None);
                };
                if must_be_author && root.sender.account != *account {
                    self.send_err(label, ErrCode::CapRequired, Some(cap), "not your message")
                        .await?;
                    return Ok(None);
                }
                // §11.13 the channel's **home** is the sole writer. If we're the
                // home (or a non-federated channel), apply locally — the msgid is
                // ours. Otherwise relay the mutation to the home; the resulting
                // EDITED/DELETED/REACTION returns over the event mirror.
                if self.ctx.registry.is_home(&channel) {
                    // §11.4: a home channel mints every message, so a foreign-origin
                    // msgid here is a bridged message we must not author.
                    if msgid.origin() != &self.ctx.info.network {
                        self.send_err(
                            label,
                            ErrCode::Forbidden,
                            Some("origin"),
                            "not this message's origin",
                        )
                        .await?;
                        return Ok(None);
                    }
                    Ok(Some(MessageRoute::Channel {
                        handle: joined.handle.clone(),
                        channel,
                        root: root.msgid,
                    }))
                } else {
                    let home = self.ctx.registry.home(&channel);
                    Ok(Some(MessageRoute::ChannelRemote {
                        channel,
                        home,
                        root: root.msgid,
                    }))
                }
            }
            Scope::Dm(a, b) => {
                // Not your conversation → indistinguishable from
                // nonexistent (§2.2) — never CAP-REQUIRED here.
                if *account != a && *account != b {
                    self.no_such_target(label).await?;
                    return Ok(None);
                }
                if must_be_author && root.sender.account != *account {
                    self.send_err(label, ErrCode::CapRequired, Some(cap), "not your message")
                        .await?;
                    return Ok(None);
                }
                let peer = if *account == a { b } else { a };
                Ok(Some(MessageRoute::Dm {
                    peer,
                    root: root.msgid,
                }))
            }
            // Group DM mutations: membership-gated (non-member = hidden, §2.2),
            // author-gated for EDIT/DELETE. The group's **home** (creator's
            // network) is the single writer (§11.4): if we're home, mint + fan
            // out; else relay the mutation there (`GroupRemote`).
            Scope::Group(group) => {
                let me = UserRef::new(account.clone(), self.ctx.info.network.clone());
                match self.ctx.groups.is_group_member(group, &me).await {
                    Ok(true) => {}
                    Ok(false) => {
                        self.no_such_target(label).await?;
                        return Ok(None);
                    }
                    Err(e) => {
                        self.internal(label, &e).await?;
                        return Ok(None);
                    }
                }
                if must_be_author && root.sender != me {
                    self.send_err(label, ErrCode::CapRequired, Some(cap), "not your message")
                        .await?;
                    return Ok(None);
                }
                let home = self.group_home(group).await;
                if home != self.ctx.info.network {
                    Ok(Some(MessageRoute::GroupRemote {
                        group,
                        home,
                        root: root.msgid,
                    }))
                } else {
                    Ok(Some(MessageRoute::Group {
                        group,
                        root: root.msgid,
                    }))
                }
            }
        }
    }

    pub(super) async fn on_edit(
        &mut self,
        label: Option<String>,
        msgid: MsgId,
        body: String,
        account: Account,
    ) -> io::Result<Flow> {
        if body.is_empty() {
            self.send_err(
                label,
                ErrCode::Policy,
                None,
                "edited body must not be empty",
            )
            .await?;
            return Ok(Flow::Continue);
        }
        match self
            .resolve_message(label.clone(), &msgid, &account, "edit-own", true)
            .await?
        {
            None => {}
            Some(MessageRoute::Channel {
                handle,
                channel,
                root,
            }) => {
                self.push_pending(&channel, label);
                handle.edit(self.id, root, body).await;
            }
            Some(MessageRoute::Dm { peer, root }) => {
                self.pending_direct.push_back(label);
                self.ctx
                    .directory
                    .edit(self.id, account, peer, root, body)
                    .await;
            }
            Some(MessageRoute::Group { group, root }) => {
                self.pending_direct.push_back(label);
                let me = UserRef::new(account, self.ctx.info.network.clone());
                self.apply_group_mutation(self.id, group, me, root, GroupMutKind::Edit(body))
                    .await;
            }
            Some(MessageRoute::GroupRemote { group, home, root }) => {
                // A spoke: relay to the home; the EDITED arrives via ingest.
                let me = UserRef::new(account, self.ctx.info.network.clone());
                self.relay_group_mut(home, group, &me, root, GroupMutKind::Edit(body));
            }
            Some(MessageRoute::ChannelRemote {
                channel,
                home,
                root,
            }) => {
                // §11.13 spoke: relay to the home; the EDITED returns via the mirror.
                let me = UserRef::new(account, self.ctx.info.network.clone());
                self.relay_channel_mut(home, channel, &me, root, "edit", body);
            }
        }
        Ok(Flow::Continue) // ack = the echoed EDITED broadcast
    }

    pub(super) async fn on_delete(
        &mut self,
        label: Option<String>,
        msgid: MsgId,
        account: Account,
    ) -> io::Result<Flow> {
        match self
            .resolve_message(label.clone(), &msgid, &account, "delete-own", true)
            .await?
        {
            None => {}
            Some(MessageRoute::Channel {
                handle,
                channel,
                root,
            }) => {
                self.push_pending(&channel, label);
                handle.delete(self.id, root).await;
            }
            Some(MessageRoute::Dm { peer, root }) => {
                self.pending_direct.push_back(label);
                self.ctx
                    .directory
                    .delete(self.id, account, peer, root)
                    .await;
            }
            Some(MessageRoute::Group { group, root }) => {
                self.pending_direct.push_back(label);
                let me = UserRef::new(account, self.ctx.info.network.clone());
                self.apply_group_mutation(self.id, group, me, root, GroupMutKind::Delete)
                    .await;
            }
            Some(MessageRoute::GroupRemote { group, home, root }) => {
                let me = UserRef::new(account, self.ctx.info.network.clone());
                self.relay_group_mut(home, group, &me, root, GroupMutKind::Delete);
            }
            Some(MessageRoute::ChannelRemote {
                channel,
                home,
                root,
            }) => {
                let me = UserRef::new(account, self.ctx.info.network.clone());
                self.relay_channel_mut(home, channel, &me, root, "delete", String::new());
            }
        }
        Ok(Flow::Continue)
    }

    pub(super) async fn on_react(
        &mut self,
        label: Option<String>,
        msgid: MsgId,
        emoji: String,
        add: bool,
        account: Account,
    ) -> io::Result<Flow> {
        match self
            .resolve_message(label.clone(), &msgid, &account, "react", false)
            .await?
        {
            None => {}
            Some(MessageRoute::Channel {
                handle,
                channel,
                root,
            }) => {
                self.push_pending(&channel, label);
                handle.react(self.id, root, emoji, add).await;
            }
            Some(MessageRoute::Dm { peer, root }) => {
                self.pending_direct.push_back(label);
                self.ctx
                    .directory
                    .react(self.id, account, peer, root, emoji, add)
                    .await;
            }
            Some(MessageRoute::Group { group, root }) => {
                self.pending_direct.push_back(label);
                let me = UserRef::new(account, self.ctx.info.network.clone());
                self.apply_group_mutation(
                    self.id,
                    group,
                    me,
                    root,
                    GroupMutKind::React { emoji, add },
                )
                .await;
            }
            Some(MessageRoute::GroupRemote { group, home, root }) => {
                let me = UserRef::new(account, self.ctx.info.network.clone());
                self.relay_group_mut(home, group, &me, root, GroupMutKind::React { emoji, add });
            }
            Some(MessageRoute::ChannelRemote {
                channel,
                home,
                root,
            }) => {
                let me = UserRef::new(account, self.ctx.info.network.clone());
                let op = if add { "react-add" } else { "react-remove" };
                self.relay_channel_mut(home, channel, &me, root, op, emoji);
            }
        }
        Ok(Flow::Continue)
    }

    fn push_pending(&mut self, channel: &ChannelName, label: Option<String>) {
        if let Some(joined) = self.joined.get_mut(channel) {
            joined.pending.push_back(label);
        }
    }

    // ---- HISTORY (§6.4, §12.1) ----

    pub(super) async fn on_history(
        &mut self,
        label: Option<String>,
        target: Target,
        before: Option<MsgId>,
        after: Option<MsgId>,
        limit: Option<u32>,
        thread: Option<MsgId>,
    ) -> io::Result<Flow> {
        // §6.4: channel history needs membership; DM history is
        // participant-by-construction (the scope key contains the caller).
        let (scope, policy, target) = match target {
            Target::Channel(channel) => {
                let Some(joined) = self.joined.get(&channel) else {
                    return self.not_member_cap(label, &channel, "view").await;
                };
                (
                    Scope::Channel(channel.clone()),
                    joined.policy,
                    Target::Channel(channel),
                )
            }
            Target::User(peer) => {
                let State::Ready { account } = self.state.clone() else {
                    unreachable!("on_history only dispatched in READY");
                };
                (
                    Scope::dm(account, peer.clone()),
                    self.ctx.dm_policy,
                    Target::User(peer),
                )
            }
            // Group DM history: membership-gated, served from `Scope::Group`.
            Target::Group(group) => {
                let State::Ready { account } = self.state.clone() else {
                    unreachable!("on_history only dispatched in READY");
                };
                let me = UserRef::new(account, self.ctx.info.network.clone());
                match self.ctx.groups.is_group_member(group, &me).await {
                    Ok(true) => {}
                    Ok(false) => return self.no_such_target(label).await,
                    Err(e) => return self.internal(label, &e).await,
                }
                (
                    Scope::Group(group),
                    self.ctx.dm_policy,
                    Target::Group(group),
                )
            }
        };
        let limit = limit.unwrap_or(100).clamp(1, weft_proto::MAX_HISTORY_LIMIT) as usize;

        let (items, truncated) = if policy == weft_proto::RetentionPolicy::Ephemeral {
            // §5.2 relay-only: nothing stored, and saying so is mandatory.
            (Vec::new(), true)
        } else {
            // §9.4 a `thread=<root>` filter returns just that thread (the root +
            // its replies, oldest-first); otherwise the normal paged window.
            let roots = match &thread {
                Some(root) => match self.ctx.events.thread_roots(&scope, root, limit).await {
                    Ok(roots) => roots,
                    Err(e) => return self.internal(label, &e).await,
                },
                None => {
                    let page = weft_store::Page {
                        before: before.as_ref().map(|m| m.ulid()),
                        after: after.as_ref().map(|m| m.ulid()),
                        limit,
                    };
                    match self.ctx.events.roots(&scope, page).await {
                        Ok(roots) => roots,
                        Err(e) => return self.internal(label, &e).await,
                    }
                }
            };
            let root_ulids: Vec<_> = roots.iter().map(|r| r.msgid.ulid()).collect();
            let children = match self.ctx.events.children(&scope, &root_ulids).await {
                Ok(children) => children,
                Err(e) => return self.internal(label, &e).await,
            };
            let items = weft_store::materialize(roots, children);
            let truncated = if thread.is_some() {
                // A full thread page may have more replies upstream.
                items.len() >= limit
            } else {
                // §6.4: `truncated` marks retention gaps — set when this page ran
                // out of data (not merely full) while the window's older edge
                // reaches into the purged region.
                let watermark = match self.ctx.events.purged_before(&scope).await {
                    Ok(watermark) => watermark,
                    Err(e) => return self.internal(label, &e).await,
                };
                let window_floor_ms = after.as_ref().map(|m| m.timestamp_ms()).unwrap_or(0);
                items.len() < limit && watermark.is_some_and(|w| window_floor_ms < w)
            };
            (items, truncated)
        };

        // §11.7 lazy federated backfill: a full page (`items.len() == limit`)
        // means local scrollback still has more, so wait. A short page means we
        // ran out locally — if this channel is federated, ask the bridge to fetch
        // the same window from its peer (no-op when unfederated; the bridge gates
        // on forwardability + dedups per window). Fire-and-forget: pulled events
        // broadcast to members + persist, so the next page serves them locally.
        // We only ever fetch what a client asked to see, never a whole scrollback.
        let ran_out = items.len() < limit;
        self.emit_batch(label, &target, items, truncated).await?;
        match target {
            Target::Channel(channel) => {
                // §11.13 spoke: ask the home to replay recent messages we may have
                // missed while unreachable (regardless of `ran_out` — the missed
                // messages are the newest; no-op when we are the home).
                if !self.ctx.registry.is_home(&channel) {
                    self.request_channel_home_backfill(channel.clone()).await;
                }
                // §11.7 M5c lazy federated backfill for deeper scrollback when the
                // local page ran out (fire-and-forget; gated + deduped downstream).
                if ran_out {
                    self.ctx
                        .request_channel_backfill(crate::BackfillReq { channel, before });
                }
            }
            // A cross-network group: catch up on anything the home minted while we
            // were unreachable. Fired regardless of `ran_out` — the messages we
            // missed are the newest ones, so even a full local page can be stale.
            Target::Group(group) => {
                self.request_group_backfill(group).await;
            }
            _ => {}
        }
        Ok(Flow::Continue)
    }

    /// Emit a `BATCH START` … events … `BATCH END` page (§7, §12.1). The wire
    /// form is always the compacted materialization; every line echoes the
    /// request label (§3.5). Shared by HISTORY and federated backfill (§11.7).
    ///
    /// §6/§13: a page larger than [`weft_proto::HISTORY_STREAM_THRESHOLD`] is
    /// **not** sent inline — it is serialized once, held under a one-time
    /// backfill token, and the caller gets a `STREAM ACCEPT <token>` to pull it
    /// over the data plane (`BACKFILL <token>`). Both direct HISTORY and the
    /// bridge backfill path (which call this) upgrade identically.
    pub(super) async fn emit_batch(
        &mut self,
        label: Option<String>,
        target: &Target,
        items: Vec<weft_store::HistoryItem>,
        truncated: bool,
    ) -> io::Result<()> {
        self.batches += 1;
        let id = format!("b{}", self.batches);
        let events = batch_events(id, target, items, truncated);
        if events.len() - 2 > weft_proto::HISTORY_STREAM_THRESHOLD {
            // Large page → serialize once + hand back a stream token. Membership
            // gating already happened building `items`, so the token is the cap.
            let body = serialize_batch(label.as_deref(), &events);
            let token = self.ctx.mint_backfill_token(body);
            debug!(target = %target, count = events.len() - 2, "batch offered as stream");
            return self.send_event(label, Event::StreamAccept { token }).await;
        }
        for event in events {
            self.send_event(label.clone(), event).await?;
        }
        Ok(())
    }

    /// §6.3 MEMBERS: a roster snapshot for a member. Framed as a `BATCH` of
    /// `MEMBER … join` (reusing the join event — the client folds each into its
    /// roster). Membership-gated; a hidden channel stays `NO-SUCH-TARGET`.
    pub(super) async fn on_members(
        &mut self,
        label: Option<String>,
        channel: ChannelName,
    ) -> io::Result<Flow> {
        let Some(joined) = self.joined.get(&channel) else {
            return self.not_member_cap(label, &channel, "view").await;
        };
        // §6.3 the roster is the *persistent* membership — offline members are
        // shown too (Discord-style). Online-ness is who currently holds a live
        // session in the channel; the presence map only refines online→away/dnd.
        let roster = match self.ctx.memberships.members(&channel).await {
            Ok(roster) => roster,
            Err(e) => return self.internal(label, &e).await,
        };
        let live: std::collections::HashSet<Account> =
            joined.handle.roster().await.into_iter().collect();
        let count = roster.len() as u64;
        self.batches += 1;
        let id = format!("m{}", self.batches);
        self.send_event(label.clone(), Event::BatchStart { id: id.clone() })
            .await?;
        for account in roster {
            // Live in this channel ⇒ online (or the away/dnd they announced;
            // invisible reads as offline). No live session ⇒ offline (a grey
            // dot, Discord-style).
            let status = if live.contains(&account) {
                let map = self.ctx.presence.lock().expect("presence lock");
                match map.get(&account).copied() {
                    None => weft_proto::PresenceStatus::Online,
                    Some(weft_proto::PresenceStatus::Invisible) => {
                        weft_proto::PresenceStatus::Offline
                    }
                    Some(other) => other,
                }
            } else {
                weft_proto::PresenceStatus::Offline
            };
            let user = UserRef::new(account, self.ctx.info.network.clone());
            self.send_event(
                None,
                Event::Member {
                    channel: channel.clone(),
                    user: user.clone(),
                    action: MemberAction::Join,
                    display: None,
                    count: Some(count),
                },
            )
            .await?;
            self.send_event(None, Event::Presence { user, status })
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

    // ---- §6.4 pins ----

    /// `PIN`/`UNPIN <msgid>`: resolve the msgid's channel, cap-check `pin`, set
    /// the pin, and broadcast `PINNED`/`UNPINNED` to the channel.
    pub(super) async fn on_pin(
        &mut self,
        label: Option<String>,
        msgid: MsgId,
        account: Account,
        pinned: bool,
    ) -> io::Result<Flow> {
        // The channel is the msgid's scope (PIN carries no channel arg).
        let channel = match self.ctx.events.find_root(msgid.ulid()).await {
            Ok(Some(record)) => match record.scope {
                Scope::Channel(channel) => channel,
                _ => return self.no_such_target(label).await, // DMs aren't pinnable
            },
            Ok(None) => return self.no_such_target(label).await,
            Err(e) => return self.internal(label, &e).await,
        };
        let scope = TokenScope::Channel(channel.to_string());
        match self
            .ctx
            .account_has_cap(&account, &Capability::Pin, &scope, unix_now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "pin").await,
            Err(e) => return self.internal(label, &e).await,
        }
        if let Err(e) = self.ctx.pins.set_pin(&channel, &msgid, pinned).await {
            return self.internal(label, &e).await;
        }
        let event = if pinned {
            Event::Pinned {
                channel: channel.clone(),
                msgid,
                by: Some(account),
            }
        } else {
            Event::Unpinned {
                channel: channel.clone(),
                msgid,
            }
        };
        // Broadcast to the channel so every member's pins view updates. The
        // acting session (if joined) receives it too — that's its confirmation.
        if let Some(handle) = self.ctx.registry.get(&channel) {
            handle.announce(event).await;
        } else {
            self.send_event(label, event).await?;
        }
        Ok(Flow::Continue)
    }

    /// `PINS <#chan>`: the pinned messages as a `BATCH` of `MESSAGE`
    /// (membership-gated, like MEMBERS). Purged pins are skipped.
    pub(super) async fn on_pins(
        &mut self,
        label: Option<String>,
        channel: ChannelName,
    ) -> io::Result<Flow> {
        if !self.joined.contains_key(&channel) {
            return self.not_member_cap(label, &channel, "view").await;
        }
        let pins = match self.ctx.pins.pins(&channel).await {
            Ok(pins) => pins,
            Err(e) => return self.internal(label, &e).await,
        };
        self.batches += 1;
        let id = format!("p{}", self.batches);
        self.send_event(label.clone(), Event::BatchStart { id: id.clone() })
            .await?;
        for msgid in pins {
            if let Ok(Some(record)) = self.ctx.events.find_root(msgid.ulid()).await {
                if let EventKind::Message { body, meta } = record.kind {
                    self.send_event(
                        None,
                        Event::Message(Box::new(weft_proto::MessageEvent {
                            target: Target::Channel(channel.clone()),
                            sender: record.sender,
                            msgid: record.msgid,
                            body,
                            meta,
                            edited: None,
                            edited_at: None,
                        })),
                    )
                    .await?;
                }
            }
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

    /// §6.4 `SEARCH <#chan> :<query>`: membership-gated message search. Returns
    /// the matching messages as a `BATCH` of `MESSAGE` (like PINS/HISTORY), so
    /// the client folds them exactly as it renders any message.
    pub(super) async fn on_search(
        &mut self,
        label: Option<String>,
        channel: ChannelName,
        query: String,
    ) -> io::Result<Flow> {
        if !self.joined.contains_key(&channel) {
            return self.not_member_cap(label, &channel, "view").await;
        }
        const SEARCH_LIMIT: usize = 50;
        let query = query.trim();
        // An empty query would substring-match everything — return no results.
        let hits = if query.is_empty() {
            Vec::new()
        } else {
            match self
                .ctx
                .events
                .search(&Scope::Channel(channel.clone()), query, SEARCH_LIMIT)
                .await
            {
                Ok(hits) => hits,
                Err(e) => return self.internal(label, &e).await,
            }
        };

        self.batches += 1;
        let id = format!("s{}", self.batches);
        self.send_event(label.clone(), Event::BatchStart { id: id.clone() })
            .await?;
        for record in hits {
            if let EventKind::Message { body, meta } = record.kind {
                self.send_event(
                    None,
                    Event::Message(Box::new(weft_proto::MessageEvent {
                        target: Target::Channel(channel.clone()),
                        sender: record.sender,
                        msgid: record.msgid,
                        body,
                        meta,
                        edited: None,
                        edited_at: None,
                    })),
                )
                .await?;
            }
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

    /// §9.4 `THREADS <#chan>`: the channel's threads as a `BATCH` of `THREAD`
    /// events (membership-gated, like PINS/SEARCH). Each carries the root, its
    /// reply count, last activity, and display name (if named).
    pub(super) async fn on_threads(
        &mut self,
        label: Option<String>,
        channel: ChannelName,
    ) -> io::Result<Flow> {
        if !self.joined.contains_key(&channel) {
            return self.not_member_cap(label, &channel, "view").await;
        }
        const THREAD_LIMIT: usize = 200;
        let threads = match self
            .ctx
            .events
            .channel_threads(&Scope::Channel(channel.clone()), THREAD_LIMIT)
            .await
        {
            Ok(threads) => threads,
            Err(e) => return self.internal(label, &e).await,
        };

        self.batches += 1;
        let id = format!("t{}", self.batches);
        self.send_event(label.clone(), Event::BatchStart { id: id.clone() })
            .await?;
        for summary in threads {
            self.send_event(
                None,
                Event::Thread {
                    channel: channel.clone(),
                    root: summary.root,
                    replies: summary.replies,
                    last: summary.last,
                    name: summary.name,
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

    /// §9.4 `THREAD NAME <#chan> <root> [:name]`: set (or clear) a thread's
    /// display name. Requires the same authority as posting; broadcasts
    /// `THREAD-NAMED` to members so every client relabels the thread live.
    pub(super) async fn on_thread_name(
        &mut self,
        label: Option<String>,
        channel: ChannelName,
        root: MsgId,
        name: Option<String>,
        account: Account,
    ) -> io::Result<Flow> {
        // The root must be a real message in this channel. An unknown or
        // foreign root is NO-SUCH-TARGET — no distinct branch (invariant 1).
        match self.ctx.events.find_root(root.ulid()).await {
            Ok(Some(record)) if record.scope == Scope::Channel(channel.clone()) => {}
            Ok(_) => return self.no_such_target(label).await,
            Err(e) => return self.internal(label, &e).await,
        }

        match self.can_post(&channel, &account).await {
            Ok(None) => {}
            Ok(Some((code, context))) => {
                self.send_err(label, code, Some(context), "cannot name threads here")
                    .await?;
                return Ok(Flow::Continue);
            }
            Err(e) => return self.internal(label, &e).await,
        }

        if let Err(e) = self
            .ctx
            .events
            .set_thread_name(
                &Scope::Channel(channel.clone()),
                &root,
                name.as_deref(),
                &account.to_string(),
                unix_now_ms(),
            )
            .await
        {
            return self.internal(label, &e).await;
        }

        // Broadcast so every member's thread list relabels; the acting session
        // (if joined) receives it too — that is its confirmation.
        let event = Event::ThreadNamed {
            channel: channel.clone(),
            root,
            name,
        };
        if let Some(handle) = self.ctx.registry.get(&channel) {
            handle.announce(event).await;
        } else {
            self.send_event(label, event).await?;
        }
        Ok(Flow::Continue)
    }

    /// §10.4 `CAPS <account> <scope>`: the account's effective caps at the
    /// scope (public — caps aren't secret). Powers client capability badges.
    /// §6.3 MARK: persist the read marker, echo MARKED (the direct
    /// response), and sync the account's other sessions via the directory.
    pub(super) async fn on_mark(
        &mut self,
        label: Option<String>,
        channel: ChannelName,
        msgid: MsgId,
        account: Account,
    ) -> io::Result<Flow> {
        if !self.joined.contains_key(&channel) {
            return self.not_member_cap(label, &channel, "view").await;
        }
        if let Err(e) = self
            .ctx
            .accounts
            .set_mark(&account, channel.as_str(), &msgid)
            .await
        {
            return self.internal(label, &e).await;
        }
        self.send_event(
            label,
            Event::Marked {
                channel: channel.clone(),
                msgid: msgid.clone(),
            },
        )
        .await?;
        // Refreshed unread counts (now read up to `msgid`) ride the marker sync
        // to the account's *other* devices, so reading on one clears the badge
        // on all. The marking device already knows it read, so it isn't pushed.
        let (unread, mentions) = self
            .ctx
            .events
            .unread_counts(&Scope::Channel(channel.clone()), &account, msgid.ulid())
            .await
            .unwrap_or((0, 0));
        self.ctx
            .directory
            .mark_sync(self.id, account, channel, msgid, unread, mentions)
            .await;
        Ok(Flow::Continue)
    }

    /// `UNREAD [<#chan>]` (§6.3) — reply with server-computed unread counts for
    /// the given channel (must be joined) or every joined channel. One
    /// `UNREAD-COUNTS` event per channel.
    pub(super) async fn on_unread(
        &mut self,
        label: Option<String>,
        channel: Option<ChannelName>,
        account: Account,
    ) -> io::Result<Flow> {
        let channels: Vec<ChannelName> = match channel {
            Some(c) => {
                if !self.joined.contains_key(&c) {
                    return self.not_member_cap(label, &c, "view").await;
                }
                vec![c]
            }
            None => self.joined.keys().cloned().collect(),
        };
        let marks = match self.ctx.accounts.marks(&account).await {
            Ok(marks) => marks,
            Err(e) => return self.internal(label, &e).await,
        };
        for chan in channels {
            let since = marks
                .iter()
                .find(|(target, _)| target == chan.as_str())
                .map(|(_, msgid)| msgid.ulid())
                .unwrap_or_else(|| Ulid::from_parts(0, 0));
            let (unread, mentions) = match self
                .ctx
                .events
                .unread_counts(&Scope::Channel(chan.clone()), &account, since)
                .await
            {
                Ok(counts) => counts,
                Err(e) => return self.internal(label, &e).await,
            };
            self.send_event(
                label.clone(),
                Event::UnreadCounts {
                    channel: chan,
                    unread,
                    mentions,
                },
            )
            .await?;
        }
        Ok(Flow::Continue)
    }
}

/// The ordered `BATCH START` … `BATCH END` event sequence for a history page
/// (§7, §12.1) — the compacted materialization, shared by the inline and the
/// streamed backfill paths so both emit byte-identical wire forms. The first
/// and last elements are always `BatchStart`/`BatchEnd`.
pub(super) fn batch_events(
    id: String,
    target: &Target,
    items: Vec<weft_store::HistoryItem>,
    truncated: bool,
) -> Vec<Event> {
    let mut events = Vec::with_capacity(items.len() + 2);
    events.push(Event::BatchStart { id: id.clone() });
    for item in items {
        match item {
            weft_store::HistoryItem::Message {
                msgid,
                sender,
                body,
                meta,
                edited,
                reactions,
            } => {
                events.push(Event::Message(Box::new(weft_proto::MessageEvent {
                    target: target.clone(),
                    sender,
                    msgid: msgid.clone(),
                    body,
                    meta,
                    edited: edited.map(|(count, _)| count),
                    edited_at: edited.map(|(_, at)| at),
                })));
                for summary in reactions {
                    events.push(Event::Reactions {
                        target: target.clone(),
                        msgid: msgid.clone(),
                        emoji: summary.emoji,
                        count: summary.count,
                        by: summary.actors,
                    });
                }
            }
            weft_store::HistoryItem::Tombstone { msgid, by } => {
                events.push(Event::Deleted {
                    target: target.clone(),
                    msgid,
                    by: Some(by),
                });
            }
        }
    }
    events.push(Event::BatchEnd {
        id,
        truncated,
        compacted: true,
    });
    events
}

/// Serialize a batch as the newline-delimited `Reply` lines the data plane
/// streams (§6/§13). Each line echoes the request label, exactly like the inline
/// form, so a client folds a pulled backfill identically to an inline `BATCH`.
/// Unserializable events (a bug — our own events always serialize) are skipped.
fn serialize_batch(label: Option<&str>, events: &[Event]) -> Vec<u8> {
    let mut body = Vec::new();
    for event in events {
        let reply = Reply {
            label: label.map(str::to_string),
            event: event.clone(),
        };
        if let Ok(line) = reply.serialize() {
            body.extend_from_slice(line.as_bytes());
            body.push(b'\n');
        }
    }
    body
}
