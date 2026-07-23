//! Social layer: 1:1 friend calls — `CALL` / `CALL ACCEPT/DECLINE/END`. The
//! server rings the callee (`CALL-RING`), tracks the call in `ServerCtx`, and
//! pushes `CALL-STATE` lifecycle updates. On accept each party receives a
//! `CALL-MEDIA` credential for a LiveKit room and joins it; with no LiveKit
//! backend the call stays signaling-only.
//!
//! **Federation-able**: the callee may live on another network. Each handler
//! takes the caller's full `UserRef` (`me`) — for a local session that is
//! `account@thisnet`; for a **federated (tunnelled) session** it is the foreign
//! `account@peer` the peer vouched for — so one handler serves both directions.
//! A local caller targeting a remote user forwards the verb over the §11.10
//! tunnel (`deliver_if_remote`), whose peer-side session re-runs the handler and
//! rings the local callee.
//!
//! **Cross-network media is a cascade (§16 M-lk-3b), not a shared room** — so a
//! client never connects to the other network's LiveKit (protecting its IP).
//! Each network hosts its **own** LiveKit room for its local participant; the
//! caller's network mints a *relay leg* (its room + a relay token + URL) and
//! carries it on the tunnelled `CALL` (`CallMediaGrant`). The callee's network
//! stashes it (`CallInfo.relay_leg`) and, on accept, spawns a [`VoiceRelay`]
//! bridging the two rooms — the only party touching both LiveKit instances is
//! that relay (server-to-server). A same-network call is one room, minted at
//! accept. Teardown releases the relay on `END`/`DECLINE`.

use super::*;
use crate::context::{CallInfo, CallPlace};
use crate::voice::{RelaySpec, VoiceBackend, VoiceGrant};
use weft_proto::{CallMediaGrant, CallState, UserRef, VoiceTransport};

/// Build a `CALL-MEDIA` event from a minted [`VoiceGrant`] for `room`.
fn call_media_event(room: &str, grant: VoiceGrant) -> Event {
    Event::CallMedia {
        room: room.to_string(),
        mode: grant.mode,
        token: grant.token,
        endpoint: grant.endpoint,
    }
}

/// Build a `CALL-MEDIA` event from a pre-minted [`CallMediaGrant`] (cross-network:
/// always LiveKit).
fn call_media_from_grant(grant: &CallMediaGrant) -> Event {
    Event::CallMedia {
        room: grant.room.clone(),
        mode: VoiceTransport::Livekit,
        token: grant.token.clone(),
        endpoint: grant.endpoint.clone(),
    }
}

/// The relay's LiveKit identity for bridging to `peer`'s room.
fn relay_identity(peer: &NetworkName) -> String {
    format!("relay@{peer}")
}

impl<S: ControlStream> Session<S> {
    /// Deliver a call event to a peer's live sessions when the peer is local.
    async fn ring(&self, peer: &UserRef, event: Event) {
        if peer.network == self.ctx.info.network {
            self.ctx.directory.notify(peer.account.clone(), event).await;
        }
    }

    /// Mint the local `participant`'s own credential for the room **this** network
    /// hosts (`room`). `None` with no LiveKit backend.
    async fn mint_own_media(&self, room: &str, participant: &UserRef) -> Option<CallMediaGrant> {
        let backend = self.ctx.voice_backend().cloned()?;
        backend
            .room_grant(room, &participant.account, true)
            .await
            .map(|g| CallMediaGrant {
                room: g.room.unwrap_or_else(|| room.to_string()),
                token: g.token,
                endpoint: g.endpoint,
            })
    }

    /// On accept, hand each **local** participant its `CALL-MEDIA` and, on the
    /// callee's network, spawn the media relay. The two parties are the accepter
    /// (`me`) and the original `caller`; on a cross-network call exactly one is
    /// local (served from its pre-minted `local_media`), on a same-network call
    /// both are (minted here against our backend).
    async fn deliver_call_media(&mut self, me: &UserRef, info: &CallInfo) -> io::Result<()> {
        let local = self.ctx.info.network.clone();

        // Callee's network: bring the media relay up first, so audio bridges the
        // moment either client joins.
        if let Some(leg) = &info.relay_leg {
            self.spawn_call_relay(info, leg).await;
        }

        // The accepter (this session), if local.
        if me.network == local {
            if let Some(grant) = &info.local_media {
                self.send_event(None, call_media_from_grant(grant)).await?;
            } else if let Some(backend) = self.ctx.voice_backend().cloned() {
                self.send_call_media(&backend, None, &me.account, &info.room)
                    .await?;
            }
        }

        // The original caller, if local — delivered to their live sessions.
        if info.caller.network == local {
            if let Some(grant) = &info.local_media {
                self.ring(&info.caller, call_media_from_grant(grant)).await;
            } else if let Some(backend) = self.ctx.voice_backend().cloned() {
                if let Some(grant) = backend
                    .room_grant(&info.room, &info.caller.account, true)
                    .await
                {
                    self.ring(&info.caller, call_media_event(&info.room, grant))
                        .await;
                }
            }
        }
        Ok(())
    }

    /// Mint a call-media credential for `account` and send it on **this** session.
    async fn send_call_media(
        &mut self,
        backend: &Arc<dyn VoiceBackend>,
        label: Option<String>,
        account: &Account,
        room: &str,
    ) -> io::Result<()> {
        if let Some(grant) = backend.room_grant(room, account, true).await {
            self.send_event(label, call_media_event(room, grant))
                .await?;
        }
        Ok(())
    }

    /// Spawn the media relay bridging our room (`info.room`) to the caller
    /// network's `leg` — the callee's network runs it, keyed by our room so
    /// `END` can release it. No-op with no LiveKit backend.
    async fn spawn_call_relay(&self, info: &CallInfo, leg: &CallMediaGrant) {
        let Some(backend) = self.ctx.voice_backend().cloned() else {
            return;
        };
        let identity = relay_identity(&info.caller.network);
        let Some(local_relay) = backend.room_grant_for(&info.room, &identity, true).await else {
            return;
        };
        let spec = RelaySpec {
            peer: info.caller.network.clone(),
            key: info.room.clone(),
            remote_url: leg.endpoint.clone().unwrap_or_default(),
            remote_room: leg.room.clone(),
            remote_token: leg.token.clone(),
            local_url: local_relay.endpoint.clone().unwrap_or_default(),
            local_room: info.room.clone(),
            local_token: local_relay.token,
        };
        self.ctx.relay_acquire(spec).await;
    }

    /// Placing a call to a **remote** callee: mint our network's *relay leg* for
    /// the room we host and tunnel the `CALL` carrying it, so the callee's network
    /// can bridge into our room. A local caller only (a federated action never
    /// re-forwards); no leg minted with no LiveKit backend (still rings —
    /// signaling-only).
    async fn forward_call_place(&self, me: &UserRef, callee: &UserRef, room: &str) {
        let local = &self.ctx.info.network;
        if &me.network != local || &callee.network == local {
            return; // not a local caller reaching a remote callee
        }
        // Our relay leg: a token the callee's relay uses to join OUR room.
        let leg = match self.ctx.voice_backend().cloned() {
            Some(backend) => backend
                .room_grant_for(room, &relay_identity(&callee.network), true)
                .await
                .map(|g| CallMediaGrant {
                    room: g.room.unwrap_or_else(|| room.to_string()),
                    token: g.token,
                    endpoint: g.endpoint,
                }),
            None => None,
        };
        // Serialize a full `CALL` (with the relay-leg tags) via the proto codec,
        // then hand it to the tunnel driver — the peer parses it in `on_federated`.
        let cmd = Command::Call {
            user: callee.clone(),
            media: leg,
        };
        if let Ok(line) = Request::new(cmd).serialize() {
            self.deliver_if_remote(me, callee, line);
        }
    }

    /// `CALL <user@net>` — place a 1:1 call. Rings the callee; the caller's
    /// labelled reply is the call's `ringing` state. A remote callee is reached
    /// over the tunnel. `media` (federated only) is the **caller network's relay
    /// leg** — present ⇒ we're the callee's network, so we stash it to bridge our
    /// room on accept. Each network mints its own room.
    pub(super) async fn on_call(
        &mut self,
        label: Option<String>,
        user: UserRef,
        me: UserRef,
        media: Option<CallMediaGrant>,
    ) -> io::Result<Flow> {
        if user == me {
            return self.no_such_target(label).await; // can't call yourself
        }
        let local = self.ctx.info.network.clone();
        let room = format!("call:{}", weft_proto::Ulid::new());

        // Who is our local participant, and are we the callee's network?
        let (local_media, relay_leg) = if me.network == local && user.network == local {
            (None, None) // same-network: mint both at accept
        } else if me.network == local {
            (self.mint_own_media(&room, &me).await, None) // caller's network
        } else {
            (self.mint_own_media(&room, &user).await, media) // callee's network
        };

        match self
            .ctx
            .call_place(&me, &user, room.clone(), local_media, relay_leg)
        {
            CallPlace::Ringing(room) => {
                // Ring the callee — local via the directory, remote via the tunnel.
                self.ring(
                    &user,
                    Event::CallRing {
                        from: me.clone(),
                        room: room.clone(),
                    },
                )
                .await;
                self.send_event(
                    label,
                    Event::CallState {
                        user: user.clone(),
                        state: CallState::Ringing,
                    },
                )
                .await?;
                // A local caller reaching a remote callee tunnels the CALL with our
                // relay leg for the room we host; else no-op.
                self.forward_call_place(&me, &user, &room).await;
            }
            CallPlace::Busy => {
                self.send_event(
                    label,
                    Event::CallState {
                        user,
                        state: CallState::Busy,
                    },
                )
                .await?;
            }
            CallPlace::Exists => {
                self.send_event(
                    label,
                    Event::CallState {
                        user,
                        state: CallState::Ringing,
                    },
                )
                .await?;
            }
        }
        Ok(Flow::Continue)
    }

    /// `CALL ACCEPT <user@net>` — accept an incoming call; both sides go active
    /// and receive their `CALL-MEDIA` (cross-network: each for its own room, with
    /// the relay bridging them).
    pub(super) async fn on_call_accept(
        &mut self,
        label: Option<String>,
        user: UserRef,
        me: UserRef,
    ) -> io::Result<Flow> {
        let Some(info) = self.ctx.call_accept(&me, &user) else {
            return self.no_such_target(label).await; // no such ringing call
        };
        tracing::debug!(room = %info.room, caller = %info.caller, "friend call accepted");

        // Both parties transition to `active` (each sees the *other* user).
        self.ring(
            &info.caller,
            Event::CallState {
                user: me.clone(),
                state: CallState::Active,
            },
        )
        .await;
        self.send_event(
            label,
            Event::CallState {
                user: user.clone(),
                state: CallState::Active,
            },
        )
        .await?;

        self.deliver_call_media(&me, &info).await?;
        self.deliver_if_remote(&me, &user, format!("CALL ACCEPT {user}"));
        Ok(Flow::Continue)
    }

    /// `CALL DECLINE <user@net>` — decline an incoming call.
    pub(super) async fn on_call_decline(
        &mut self,
        label: Option<String>,
        user: UserRef,
        me: UserRef,
    ) -> io::Result<Flow> {
        let Some(info) = self.ctx.call_end(&me, &user) else {
            return self.no_such_target(label).await;
        };
        self.release_call_relay(&info).await;
        self.ring(
            &user,
            Event::CallState {
                user: me.clone(),
                state: CallState::Declined,
            },
        )
        .await;
        self.send_event(
            label,
            Event::CallState {
                user: user.clone(),
                state: CallState::Declined,
            },
        )
        .await?;
        self.deliver_if_remote(&me, &user, format!("CALL DECLINE {user}"));
        Ok(Flow::Continue)
    }

    /// `CALL END <user@net>` — hang up / cancel a call.
    pub(super) async fn on_call_end(
        &mut self,
        label: Option<String>,
        user: UserRef,
        me: UserRef,
    ) -> io::Result<Flow> {
        let Some(info) = self.ctx.call_end(&me, &user) else {
            return self.no_such_target(label).await;
        };
        self.release_call_relay(&info).await;
        self.ring(
            &user,
            Event::CallState {
                user: me.clone(),
                state: CallState::Ended,
            },
        )
        .await;
        self.send_event(
            label,
            Event::CallState {
                user: user.clone(),
                state: CallState::Ended,
            },
        )
        .await?;
        self.deliver_if_remote(&me, &user, format!("CALL END {user}"));
        Ok(Flow::Continue)
    }

    /// Release the call's media relay when it had one (the callee's network).
    async fn release_call_relay(&self, info: &CallInfo) {
        if info.relay_leg.is_some() {
            self.ctx
                .relay_release(&info.caller.network, &info.room)
                .await;
        }
    }
}
