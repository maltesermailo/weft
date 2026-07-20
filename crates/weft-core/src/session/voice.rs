//! §16 WEFT-RT voice signaling: `VOICE JOIN/LEAVE/DESC/CAND`.
//!
//! Authority lives here, not in the SFU (invariant 4): a join is gated on
//! channel membership + the M7 mute/ban deny-list + `listen`/`speak` caps
//! *before* the [`VoiceBackend`](crate::voice::VoiceBackend) is ever asked to
//! allocate a peer. The backend (weft-rt's `WebrtcSfu`, or a future adapter)
//! owns the WebRTC negotiation; core only relays SDP/ICE to it and fans
//! `VOICE STATE` out to the room. A server with no backend answers
//! `UNSUPPORTED` (it also advertises no `features=voice`).

use super::*;

use weft_proto::VoiceAction;

use crate::voice::{VoiceError, VoiceJoinReq, VoiceMember};

impl<S: ControlStream> Session<S> {
    /// `VOICE JOIN <#chan>` — authorize, reserve an SFU slot, answer
    /// `VOICE OFFER` (the labeled ack), and tell co-members via `VOICE STATE`.
    pub(super) async fn on_voice_join(
        &mut self,
        label: Option<String>,
        channel: ChannelName,
        account: Account,
    ) -> io::Result<Flow> {
        if self.ctx.voice_backend().is_none() {
            return self
                .unsupported(label, "voice is not enabled on this server")
                .await;
        }
        // §16 the target must exist AND be a *voice* channel; missing / text /
        // private all collapse to NO-SUCH-TARGET (invariant 1). Voice channels
        // are entered here, never via a text JOIN.
        let Some(handle) = self.ctx.registry.get(&channel) else {
            return self.no_such_target(label).await;
        };
        if self.channel_kind(&channel).await != ChannelKind::Voice {
            return self.no_such_target(label).await;
        }
        // Re-join replaces any existing peer/subscription for this room.
        if let Some(room) = self.voice.remove(&channel) {
            room.forwarder.abort();
            if let Some(backend) = self.ctx.voice_backend() {
                backend.leave(self.id, &channel).await;
            }
        }

        // §6.7 moderation: a ban denies voice outright; a mute removes `speak`.
        let scopes = covering_scopes(&channel);
        match self
            .ctx
            .moderation
            .is_moderated(&account, &scopes, ModKind::Ban)
            .await
        {
            Ok(true) => {
                self.send_err(label, ErrCode::Forbidden, Some("banned"), "you are banned")
                    .await?;
                return Ok(Flow::Continue);
            }
            Ok(false) => {}
            Err(e) => return self.internal(label, &e).await,
        }
        let muted = match self
            .ctx
            .moderation
            .is_moderated(&account, &scopes, ModKind::Mute)
            .await
        {
            Ok(muted) => muted,
            Err(e) => return self.internal(label, &e).await,
        };

        let (can_listen, can_speak) = match self.voice_caps(&channel, &account, muted).await {
            Ok(pair) => pair,
            Err(e) => return self.internal(label, &e).await,
        };
        if !can_listen {
            return self.cap_required(label, "listen").await;
        }

        let backend = self.ctx.voice_backend().expect("checked above").clone();
        let grant = match backend
            .join(VoiceJoinReq {
                channel: channel.clone(),
                account: account.clone(),
                session: self.id,
                can_speak,
            })
            .await
        {
            Ok(grant) => grant,
            Err(VoiceError::Unavailable) => {
                self.send_err(label, ErrCode::Internal, None, "voice unavailable")
                    .await?;
                return Ok(Flow::Continue);
            }
            Err(VoiceError::BadDescription) => {
                self.send_err(label, ErrCode::Malformed, None, "voice rejected")
                    .await?;
                return Ok(Flow::Continue);
            }
        };

        // Subscribe to the channel's broadcast so co-members' VOICE STATE reaches
        // us — without becoming a text member (voice-only, §16).
        let Some(events) = handle.subscribe().await else {
            backend.leave(self.id, &channel).await;
            self.send_err(label, ErrCode::Internal, None, "voice unavailable")
                .await?;
            return Ok(Flow::Continue);
        };
        let forwarder = spawn_forwarder(channel.clone(), events, self.events_tx.clone());
        self.voice.insert(
            channel.clone(),
            VoiceRoom {
                handle: handle.clone(),
                forwarder,
            },
        );

        self.send_event(
            label,
            Event::VoiceOffer {
                channel: channel.clone(),
                mode: grant.mode,
                token: grant.token,
                room: grant.room,
                endpoint: grant.endpoint,
            },
        )
        .await?;
        // §16 snapshot: tell the joiner who's already in the room (before adding
        // self), so a client that joins a populated room sees a full roster.
        for member in self.ctx.voice_roster(&channel) {
            let user = UserRef::new(member.account, self.ctx.info.network.clone());
            self.send_event(
                None,
                Event::VoiceState {
                    channel: channel.clone(),
                    user,
                    action: VoiceAction::Join,
                    muted: member.muted,
                    deaf: false,
                    speaking: false,
                },
            )
            .await?;
        }
        // Register self in the roster, then announce to the room. A listen-only
        // participant renders muted.
        self.ctx.voice_room_join(
            &channel,
            self.id,
            VoiceMember {
                account: account.clone(),
                muted: !can_speak,
            },
        );
        self.announce_voice_state(&handle, &channel, &account, VoiceAction::Join, !can_speak)
            .await;
        Ok(Flow::Continue)
    }

    /// `VOICE LEAVE <#chan>` — tear the peer down, ack with `VOICE STATE leave`,
    /// and tell co-members. Not-in-that-room is the uniform NO-SUCH-TARGET.
    pub(super) async fn on_voice_leave(
        &mut self,
        label: Option<String>,
        channel: ChannelName,
    ) -> io::Result<Flow> {
        let Some(room) = self.voice.remove(&channel) else {
            return self.no_such_target(label).await;
        };
        room.forwarder.abort();
        self.ctx.voice_room_leave(&channel, self.id);
        let State::Ready { account } = self.state.clone() else {
            unreachable!("voice verbs only dispatch in READY");
        };
        if let Some(backend) = self.ctx.voice_backend() {
            backend.leave(self.id, &channel).await;
        }
        // Co-members learn via an origin=self broadcast (our own copy is skipped);
        // the caller gets the labeled leave directly as its ack.
        self.announce_voice_state(&room.handle, &channel, &account, VoiceAction::Leave, false)
            .await;
        let user = UserRef::new(account, self.ctx.info.network.clone());
        self.send_event(
            label,
            Event::VoiceState {
                channel,
                user,
                action: VoiceAction::Leave,
                muted: false,
                deaf: false,
                speaking: false,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    /// `VOICE DESC <#chan> :<sdp>` — relay the client's SDP offer to the SFU and
    /// return its answer as a `VOICE DESC` event (symmetric verb, §16).
    pub(super) async fn on_voice_desc(
        &mut self,
        label: Option<String>,
        channel: ChannelName,
        sdp: String,
    ) -> io::Result<Flow> {
        let Some(backend) = self.ctx.voice_backend().cloned() else {
            return self
                .unsupported(label, "voice is not enabled on this server")
                .await;
        };
        // Must have an active voice slot in the channel (invariant 1 otherwise).
        if !self.voice.contains_key(&channel) {
            return self.no_such_target(label).await;
        }
        match backend.describe(self.id, &channel, sdp).await {
            Ok(answer) => {
                self.send_event(
                    label,
                    Event::VoiceDesc {
                        channel,
                        sdp: answer,
                    },
                )
                .await?;
                Ok(Flow::Continue)
            }
            Err(VoiceError::BadDescription) => {
                self.send_err(label, ErrCode::Malformed, None, "bad SDP")
                    .await?;
                Ok(Flow::Continue)
            }
            Err(VoiceError::Unavailable) => {
                self.send_err(label, ErrCode::Internal, None, "voice unavailable")
                    .await?;
                Ok(Flow::Continue)
            }
        }
    }

    /// `VOICE CAND <#chan> :<candidate>` — hand a trickle-ICE candidate to the
    /// SFU. No direct response beyond the label echo on error.
    pub(super) async fn on_voice_cand(
        &mut self,
        label: Option<String>,
        channel: ChannelName,
        candidate: String,
    ) -> io::Result<Flow> {
        let Some(backend) = self.ctx.voice_backend().cloned() else {
            return self
                .unsupported(label, "voice is not enabled on this server")
                .await;
        };
        if !self.voice.contains_key(&channel) {
            return self.no_such_target(label).await;
        }
        if let Err(VoiceError::BadDescription) =
            backend.candidate(self.id, &channel, candidate).await
        {
            self.send_err(label, ErrCode::Malformed, None, "bad candidate")
                .await?;
        }
        Ok(Flow::Continue)
    }

    /// §16 disconnect cleanup: abort the room's forwarder, drop this session's
    /// SFU peer, and tell the room (SENTINEL broadcast — the session is going
    /// away). Called with the drained [`VoiceRoom`] from `cleanup`.
    pub(super) async fn teardown_voice(&self, channel: &ChannelName, room: VoiceRoom) {
        room.forwarder.abort();
        self.ctx.voice_room_leave(channel, self.id);
        if let Some(backend) = self.ctx.voice_backend() {
            backend.leave(self.id, channel).await;
        }
        let Some(account) = self.registered.clone() else {
            return;
        };
        let user = UserRef::new(account, self.ctx.info.network.clone());
        room.handle
            .announce(Event::VoiceState {
                channel: channel.clone(),
                user,
                action: VoiceAction::Leave,
                muted: false,
                deaf: false,
                speaking: false,
            })
            .await;
    }

    /// §6.7 apply a moderator's `MUTE`/`UNMUTE` to `account`'s live voice: drop
    /// (or resume) their audio at the SFU in every room they're in, and broadcast
    /// a `VOICE STATE update` so the room re-renders their mute badge.
    pub(super) async fn mute_in_voice(&self, account: &Account, muted: bool) {
        let Some(backend) = self.ctx.voice_backend().cloned() else {
            return;
        };
        for (channel, session) in self.ctx.voice_set_muted(account, muted) {
            backend.set_muted(session, &channel, muted).await;
            if let Some(handle) = self.ctx.registry.get(&channel) {
                let user = UserRef::new(account.clone(), self.ctx.info.network.clone());
                handle
                    .announce(Event::VoiceState {
                        channel,
                        user,
                        action: VoiceAction::Update,
                        muted,
                        deaf: false,
                        speaking: false,
                    })
                    .await;
            }
        }
    }

    /// §6.7 remove `account` from `channel`'s voice room (a ban/kick): tear down
    /// their backend peer (SFU slot / LiveKit participant → their media stops
    /// server-side immediately) and announce their departure to the room. The
    /// ejected client's own session state is left as-is — the LiveKit disconnect
    /// / vanished SFU media is what enforces it; a `MODERATED` line already told
    /// their client they were removed. No-op if they're not in voice here.
    pub(super) async fn eject_channel_voice(&self, account: &Account, channel: &ChannelName) {
        let Some(backend) = self.ctx.voice_backend().cloned() else {
            return;
        };
        let Some(session) = self.ctx.voice_eject_account(channel, account) else {
            return;
        };

        backend.leave(session, channel).await;

        if let Some(handle) = self.ctx.registry.get(channel) {
            let user = UserRef::new(account.clone(), self.ctx.info.network.clone());
            handle
                .announce(Event::VoiceState {
                    channel: channel.clone(),
                    user,
                    action: VoiceAction::Leave,
                    muted: false,
                    deaf: false,
                    speaking: false,
                })
                .await;
        }
    }

    /// A channel's kind (§16); `Text` if unknown — fails safe so a store hiccup
    /// never turns a text channel into a voice one.
    pub(super) async fn channel_kind(&self, channel: &ChannelName) -> ChannelKind {
        self.ctx
            .channel_store
            .channel(channel)
            .await
            .ok()
            .flatten()
            .map(|c| c.kind)
            .unwrap_or(ChannelKind::Text)
    }

    /// The `(can_listen, can_speak)` pair for a join. **Open** channels let any
    /// member do both (speaking still subject to mutes). A **restricted** channel
    /// gates each on its cap (`listen` to hear, `speak` to talk) — mirroring the
    /// posting gate — with a mute always removing `speak`.
    async fn voice_caps(
        &self,
        channel: &ChannelName,
        account: &Account,
        muted: bool,
    ) -> Result<(bool, bool), weft_store::StoreError> {
        let restricted = self
            .ctx
            .channel_store
            .channel(channel)
            .await?
            .map(|c| c.restricted)
            .unwrap_or(false);
        if !restricted {
            return Ok((true, !muted));
        }
        let scope = TokenScope::Channel(channel.to_string());
        let now = unix_now();
        let can_listen = self
            .ctx
            .account_has_cap(account, &Capability::Listen, &scope, now)
            .await?;
        let can_speak = !muted
            && self
                .ctx
                .account_has_cap(account, &Capability::Speak, &scope, now)
                .await?;
        Ok((can_listen, can_speak))
    }

    /// Broadcast a `VOICE STATE` for `account` to `channel`'s voice subscribers,
    /// attributed to this session so its own forwarder skips the copy (the actor
    /// gets its ack directly).
    async fn announce_voice_state(
        &self,
        handle: &ChannelHandle,
        channel: &ChannelName,
        account: &Account,
        action: VoiceAction,
        muted: bool,
    ) {
        let user = UserRef::new(account.clone(), self.ctx.info.network.clone());
        handle
            .announce_as(
                self.id,
                Event::VoiceState {
                    channel: channel.clone(),
                    user,
                    action,
                    muted,
                    deaf: false,
                    speaking: false,
                },
            )
            .await;
    }
}
