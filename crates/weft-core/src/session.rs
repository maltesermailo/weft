//! Per-connection session: the §3.3 FSM
//! (`NEGOTIATING → UNAUTHED → READY → closed`), verb dispatch, label
//! echo-acks and `(session, label)` dedup (§3.5, §9.2).
//!
//! One task per connection. Channel events arrive through per-channel
//! forwarder tasks feeding one bounded queue; when that chain backs up the
//! broadcast receiver lags and the client gets `ERR SLOW` (§9.2) instead of
//! unbounded buffering.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, error, info, info_span, warn, Instrument};
use weft_crypto::{Capability, PublicKey, Subject, TokenScope};
use weft_proto::{
    Account, ChannelName, Command, ErrCode, ErrEvent, Event, Line, MemberAction, MsgId, MsgMeta,
    ParseError, Reply, Request, RetentionPolicy, Target, UserRef, Visibility, MAX_LABEL_BYTES,
};

use weft_store::{InviteRecord, RedeemOutcome, Scope};

use crate::channel::{ChannelEvent, ChannelHandle};
use crate::context::{ServerCtx, PROTOCOL_VERSION};
use crate::directory::DirectEvent;
use crate::stream::ControlStream;

/// Process-unique connection identifier (also the actor-side member key).
pub type SessionId = u64;

/// §3.3: idle pre-auth sessions closed after 30 s.
const PREAUTH_IDLE: Duration = Duration::from_secs(30);
/// §3.4: 10 s keepalive interval, 2 missed = dead (plus slack).
const READY_IDLE: Duration = Duration::from_secs(30);
/// §9.2: dedup MSG retries by (session, label) for 5 minutes.
const DEDUP_WINDOW: Duration = Duration::from_secs(300);
/// §8: MALFORMED — close after 5 per 60 s.
const MALFORMED_LIMIT: usize = 5;
const MALFORMED_WINDOW: Duration = Duration::from_secs(60);
/// Bound on the session's event queue; overflow propagates to broadcast
/// lag → `ERR SLOW`, never unbounded memory.
const EVENT_QUEUE: usize = 256;

/// Drive one connection to completion. This is weftd's entire per-connection
/// entry point: wrap the transport in a [`ControlStream`] and call this.
pub async fn run_session<S: ControlStream>(stream: S, ctx: Arc<ServerCtx>) {
    let id = ctx.next_session_id();
    let span = info_span!("session", id);
    async move {
        let mut session = Session::new(id, stream, ctx);
        match session.run().await {
            Ok(()) => debug!("session closed"),
            Err(e) => debug!("session ended with I/O error: {e}"),
        }
        session.cleanup().await;
        // Flush/finish so a final line (ERR UNSUPPORTED, §3.6) survives the
        // transport teardown that follows.
        let _ = session.stream.close().await;
    }
    .instrument(span)
    .await;
}

/// AUTH KEY state between CHALLENGE and PROOF (§6.1). One per session;
/// a new AUTH KEY replaces it, any PROOF consumes it.
#[derive(Debug, Clone)]
struct PendingChallenge {
    account: Account,
    device: weft_crypto::PublicKey,
    nonce: [u8; weft_crypto::CHALLENGE_NONCE_LEN],
}

#[derive(Debug, Clone)]
enum State {
    Negotiating,
    Unauthed { challenge: Option<PendingChallenge> },
    Ready { account: Account },
}

enum Flow {
    Continue,
    Close,
}

/// Events flowing from channel forwarders into the session task. Variant
/// sizes are lopsided but these only transit the bounded session queue
/// (256 × ~256 B) — boxing would add an allocation per delivered event.
#[allow(clippy::large_enum_variant)]
enum SessionEvent {
    Channel {
        channel: ChannelName,
        event: ChannelEvent,
    },
    Lagged {
        channel: ChannelName,
    },
}

/// Per-joined-channel session state.
struct Joined {
    handle: ChannelHandle,
    policy: weft_proto::RetentionPolicy,
    forwarder: JoinHandle<()>,
    /// Labels of own publishes awaiting their echo, in publish order. The
    /// actor broadcasts this session's messages in the same order they were
    /// sent (one mpsc, one actor), so a FIFO pairs each echo with its label.
    pending: VecDeque<Option<String>>,
}

struct DedupEntry {
    line: String,
    at: Instant,
}

/// Where a msgid's mutations must be sent (each scope has one writer).
enum MessageRoute {
    Channel {
        handle: ChannelHandle,
        channel: ChannelName,
        root: MsgId,
    },
    Dm {
        peer: Account,
        root: MsgId,
    },
}

struct Session<S> {
    id: SessionId,
    stream: S,
    ctx: Arc<ServerCtx>,
    state: State,
    joined: HashMap<ChannelName, Joined>,
    events_tx: mpsc::Sender<SessionEvent>,
    events_rx: mpsc::Receiver<SessionEvent>,
    /// Account-scoped events (DMs, MARK sync) from the directory.
    direct_tx: mpsc::Sender<DirectEvent>,
    direct_rx: mpsc::Receiver<DirectEvent>,
    /// Labels of own DM commands awaiting their echo — the directory
    /// counterpart of each channel's `pending` FIFO, same ordering
    /// argument (one mpsc into one actor).
    pending_direct: VecDeque<Option<String>>,
    /// Set once registered with the directory; drives deregistration.
    registered: Option<Account>,
    /// label → serialized echo, replayed verbatim on MSG retry (§9.2).
    dedup: HashMap<String, DedupEntry>,
    /// HISTORY batch id counter (per session, opaque to clients).
    batches: u64,
    malformed_strikes: Vec<Instant>,
    last_inbound: Instant,
}

#[allow(clippy::large_enum_variant)] // one per select iteration, stack-only
enum Action {
    Line(Option<String>),
    Event(SessionEvent),
    Direct(DirectEvent),
    Idle,
}

impl<S: ControlStream> Session<S> {
    fn new(id: SessionId, stream: S, ctx: Arc<ServerCtx>) -> Self {
        let (events_tx, events_rx) = mpsc::channel(EVENT_QUEUE);
        let (direct_tx, direct_rx) = mpsc::channel(EVENT_QUEUE);
        Self {
            id,
            stream,
            ctx,
            state: State::Negotiating,
            joined: HashMap::new(),
            events_tx,
            events_rx,
            direct_tx,
            direct_rx,
            pending_direct: VecDeque::new(),
            registered: None,
            dedup: HashMap::new(),
            batches: 0,
            malformed_strikes: Vec::new(),
            last_inbound: Instant::now(),
        }
    }

    async fn run(&mut self) -> io::Result<()> {
        loop {
            let limit = match self.state {
                State::Ready { .. } => READY_IDLE,
                _ => PREAUTH_IDLE,
            };
            let action = tokio::select! {
                line = self.stream.recv_line() => Action::Line(line?),
                event = self.events_rx.recv() =>
                    Action::Event(event.expect("session holds an events sender")),
                direct = self.direct_rx.recv() =>
                    Action::Direct(direct.expect("session holds a direct sender")),
                _ = tokio::time::sleep_until(self.last_inbound + limit) => Action::Idle,
            };
            match action {
                Action::Line(None) => return Ok(()), // peer closed
                Action::Line(Some(raw)) => {
                    self.last_inbound = Instant::now();
                    if let Flow::Close = self.on_line(&raw).await? {
                        return Ok(());
                    }
                }
                Action::Event(event) => self.on_event(event).await?,
                Action::Direct(direct) => self.on_direct(direct).await?,
                Action::Idle => {
                    debug!("idle timeout");
                    return Ok(());
                }
            }
        }
    }

    /// Leave all channels so members see MEMBER part even on abrupt drops,
    /// and drop out of the account directory.
    async fn cleanup(&mut self) {
        for (_, joined) in self.joined.drain() {
            joined.forwarder.abort();
            joined.handle.part(self.id).await;
        }
        if let Some(account) = self.registered.take() {
            self.ctx.directory.deregister(account, self.id).await;
        }
    }

    // ---- inbound lines ----

    async fn on_line(&mut self, raw: &str) -> io::Result<Flow> {
        let line = match Line::parse(raw) {
            Ok(line) => line,
            Err(e) => return self.on_malformed(None, &e).await,
        };
        let request = match Request::from_line(&line) {
            Ok(request) => request,
            Err(e) => {
                // Echo the label if one is salvageable — MALFORMED is a
                // direct response too (§3.5).
                let label = line
                    .tags
                    .get("label")
                    .filter(|v| !v.is_empty() && v.len() <= MAX_LABEL_BYTES)
                    .cloned();
                return self.on_malformed(label, &e).await;
            }
        };
        let span = info_span!("verb", verb = %line.verb);
        self.on_request(request.label, request.command)
            .instrument(span)
            .await
    }

    async fn on_malformed(&mut self, label: Option<String>, err: &ParseError) -> io::Result<Flow> {
        self.send_err(label, ErrCode::Malformed, None, &err.to_string())
            .await?;
        let now = Instant::now();
        self.malformed_strikes
            .retain(|t| now.duration_since(*t) < MALFORMED_WINDOW);
        self.malformed_strikes.push(now);
        Ok(if self.malformed_strikes.len() >= MALFORMED_LIMIT {
            warn!("closing: {MALFORMED_LIMIT} malformed lines inside {MALFORMED_WINDOW:?} (§8)");
            Flow::Close
        } else {
            Flow::Continue
        })
    }

    async fn on_request(&mut self, label: Option<String>, cmd: Command) -> io::Result<Flow> {
        // §4: unknown verbs are ignored in every state — labels make the
        // silence detectable client-side.
        if let Command::Unknown { verb } = &cmd {
            debug!(%verb, "ignoring unknown verb");
            return Ok(Flow::Continue);
        }
        match self.state.clone() {
            State::Negotiating => self.on_negotiating(label, cmd).await,
            State::Unauthed { challenge } => self.on_unauthed(label, cmd, challenge).await,
            State::Ready { account } => self.on_ready(label, cmd, account).await,
        }
    }

    /// §3.3 NEGOTIATING: only HELLO.
    async fn on_negotiating(&mut self, label: Option<String>, cmd: Command) -> io::Result<Flow> {
        match cmd {
            Command::Hello { version } if version == PROTOCOL_VERSION => {
                let info = &self.ctx.info;
                let welcome = Event::Welcome {
                    network: info.network.clone(),
                    features: info.features.clone(),
                    attestation: None,
                    motd: info.motd.clone(),
                };
                self.send_event(label, welcome).await?;
                debug!("negotiated weft/1 → UNAUTHED");
                self.state = State::Unauthed { challenge: None };
                Ok(Flow::Continue)
            }
            Command::Hello { .. } => {
                // §3.6: version mismatch → ERR UNSUPPORTED, close.
                self.send_err(
                    label,
                    ErrCode::Unsupported,
                    None,
                    "only weft/1 is spoken here",
                )
                .await?;
                Ok(Flow::Close)
            }
            _ => self.not_authed(label, "say HELLO first").await,
        }
    }

    /// §3.3 UNAUTHED: only AUTH, REGISTER, PING, QUIT.
    async fn on_unauthed(
        &mut self,
        label: Option<String>,
        cmd: Command,
        challenge: Option<PendingChallenge>,
    ) -> io::Result<Flow> {
        match cmd {
            Command::Register { account, password } => {
                self.on_register(label, account, &password).await
            }
            Command::AuthPassword { account, password } => {
                // Constant-time verify, dummy-hash for unknown accounts —
                // one code, one text, one timing envelope (invariant 5).
                match self.ctx.accounts.verify_password(&account, &password).await {
                    Ok(true) => {
                        info!(%account, "authenticated (password)");
                        self.welcome_authed(label, account, None).await
                    }
                    Ok(false) => {
                        // Server-side only — the wire never distinguishes.
                        debug!(%account, "password verification failed (unknown account or wrong password)");
                        self.auth_failed(label).await
                    }
                    Err(e) => self.internal(label, &e).await,
                }
            }
            // §6.1 step 1: issue a 32-byte nonce. Issued regardless of
            // whether the account or key is known — existence must not be
            // observable before PROOF (anti-enumeration discipline).
            Command::AuthKey { account, pubkey } => {
                let Ok(device) = weft_crypto::PublicKey::from_b64(&pubkey) else {
                    // Undecodable key material is a shape error, independent
                    // of any account state — MALFORMED leaks nothing.
                    return self
                        .on_malformed(
                            label,
                            &ParseError::Invalid {
                                what: "device key",
                                value: pubkey,
                            },
                        )
                        .await;
                };
                let nonce: [u8; weft_crypto::CHALLENGE_NONCE_LEN] = rand::random();
                self.send_event(
                    label,
                    Event::Challenge {
                        nonce: weft_crypto::b64::encode(nonce),
                    },
                )
                .await?;
                // A second AUTH KEY replaces any pending challenge.
                self.state = State::Unauthed {
                    challenge: Some(PendingChallenge {
                        account,
                        device,
                        nonce,
                    }),
                };
                Ok(Flow::Continue)
            }
            // §6.1 step 2: verify sig(nonce ‖ network-name) and enrollment.
            Command::AuthProof { signature } => {
                // The challenge is single-use: consumed on any PROOF.
                self.state = State::Unauthed { challenge: None };
                let Some(pending) = challenge else {
                    return self.auth_failed(label).await;
                };
                let Ok(signature) = weft_crypto::signature_from_b64(&signature) else {
                    return self.auth_failed(label).await;
                };
                // Evaluate both conditions unconditionally — no early exit
                // that would let timing separate "bad proof" from
                // "unknown device" (invariant 5).
                let proof_ok = weft_crypto::verify_challenge(
                    &pending.device,
                    &pending.nonce,
                    self.ctx.network_name(),
                    &signature,
                );
                let enrolled = match self
                    .ctx
                    .accounts
                    .device_enrolled(&pending.account, &pending.device)
                    .await
                {
                    Ok(enrolled) => enrolled,
                    Err(e) => return self.internal(label, &e).await,
                };
                if proof_ok && enrolled {
                    info!(account = %pending.account, "authenticated (device key)");
                    let attestation =
                        self.ctx
                            .mint_attestation(&pending.account, pending.device, unix_now());
                    self.welcome_authed(label, pending.account, Some(attestation.to_b64()))
                        .await
                } else {
                    debug!(account = %pending.account, proof_ok, enrolled, "key auth rejected");
                    self.auth_failed(label).await
                }
            }
            Command::Ping { token } => {
                self.send_event(label, Event::Pong { token }).await?;
                Ok(Flow::Continue)
            }
            Command::Quit { .. } => Ok(Flow::Close),
            _ => self.not_authed(label, "authenticate first").await,
        }
    }

    /// §6.1 REGISTER: gated on config, password ≥ 12 B, unique name.
    /// Success is also authentication (→ WELCOME → READY).
    async fn on_register(
        &mut self,
        label: Option<String>,
        account: Account,
        password: &str,
    ) -> io::Result<Flow> {
        if !self.ctx.registration_open {
            self.send_err(
                label,
                ErrCode::Forbidden,
                None,
                "registration is closed on this network",
            )
            .await?;
            return Ok(Flow::Continue);
        }
        if password.len() < 12 {
            self.send_err(
                label,
                ErrCode::Policy,
                None,
                "password must be at least 12 bytes",
            )
            .await?;
            return Ok(Flow::Continue);
        }
        match self.ctx.accounts.register(&account, password).await {
            Ok(crate::accounts::RegisterOutcome::Exists) => {
                self.send_err(label, ErrCode::Conflict, None, "account name is taken")
                    .await?;
                Ok(Flow::Continue)
            }
            Ok(crate::accounts::RegisterOutcome::Created) => {
                self.welcome_authed(label, account, None).await
            }
            Err(e) => self.internal(label, &e).await,
        }
    }

    /// Successful auth: WELCOME (with `attestation=` for key auth, §6.1)
    /// and the READY transition.
    async fn welcome_authed(
        &mut self,
        label: Option<String>,
        account: Account,
        attestation: Option<String>,
    ) -> io::Result<Flow> {
        let welcome = Event::Welcome {
            network: self.ctx.info.network.clone(),
            features: Vec::new(),
            attestation,
            motd: None,
        };
        self.send_event(label, welcome).await?;
        // Join the account directory (DM delivery, MARK sync)...
        self.ctx
            .directory
            .register(account.clone(), self.id, self.direct_tx.clone())
            .await;
        // ...and restore read state (§9.7: MARKED snapshot after auth).
        match self.ctx.accounts.marks(&account).await {
            Ok(marks) => {
                for (target, msgid) in marks {
                    if let Ok(channel) = target.parse::<ChannelName>() {
                        self.send_event(None, Event::Marked { channel, msgid })
                            .await?;
                    }
                }
            }
            Err(e) => error!("marks snapshot failed: {e}"),
        }
        self.registered = Some(account.clone());
        self.state = State::Ready { account };
        Ok(Flow::Continue)
    }

    /// The single failure surface for every credential problem — unknown
    /// account, wrong password, bad proof, unknown device, missing
    /// challenge. One code, one text (§8: AUTH-FAILED is uniform).
    async fn auth_failed(&mut self, label: Option<String>) -> io::Result<Flow> {
        self.send_err(label, ErrCode::AuthFailed, None, "authentication failed")
            .await?;
        Ok(Flow::Continue)
    }

    async fn on_ready(
        &mut self,
        label: Option<String>,
        cmd: Command,
        account: Account,
    ) -> io::Result<Flow> {
        match cmd {
            Command::Ping { token } => {
                self.send_event(label, Event::Pong { token }).await?;
                Ok(Flow::Continue)
            }
            // Client answering our keepalive; the idle deadline already
            // advanced when the line arrived.
            Command::Pong { .. } => Ok(Flow::Continue),
            Command::Quit { .. } => Ok(Flow::Close),
            Command::Join { channel, invite } => {
                self.on_join(label, channel, invite, account).await
            }
            Command::Part { channel, .. } => self.on_part(label, channel).await,
            Command::Typing { channel, state } => self.on_typing(label, channel, state).await,
            Command::Msg { target, body, meta } => self.on_msg(label, target, body, meta).await,
            Command::Edit { msgid, body } => self.on_edit(label, msgid, body, account).await,
            Command::Delete { msgid } => self.on_delete(label, msgid, account).await,
            Command::React { msgid, emoji } => {
                self.on_react(label, msgid, emoji, true, account).await
            }
            Command::Unreact { msgid, emoji } => {
                self.on_react(label, msgid, emoji, false, account).await
            }
            Command::History {
                target,
                before,
                after,
                limit,
                thread,
            } => {
                self.on_history(label, target, before, after, limit, thread)
                    .await
            }
            // §6.1: add a device while authed. Responds like key auth —
            // WELCOME carrying the new device's attestation.
            Command::AuthEnroll { pubkey } => {
                let Ok(device) = weft_crypto::PublicKey::from_b64(&pubkey) else {
                    return self
                        .on_malformed(
                            label,
                            &ParseError::Invalid {
                                what: "device key",
                                value: pubkey,
                            },
                        )
                        .await;
                };
                if let Err(e) = self.ctx.accounts.enroll_device(&account, device).await {
                    return self.internal(label, &e).await;
                }
                let attestation = self.ctx.mint_attestation(&account, device, unix_now());
                let welcome = Event::Welcome {
                    network: self.ctx.info.network.clone(),
                    features: Vec::new(),
                    attestation: Some(attestation.to_b64()),
                    motd: None,
                };
                self.send_event(label, welcome).await?;
                Ok(Flow::Continue)
            }
            // §6.1 PRESENCE: same-network relay to co-members of joined
            // channels. `invisible` renders offline — relaying it would
            // reveal the hiding, so it is accepted and NOT broadcast (there
            // is no "went offline" wire status; spec gap noted in review).
            Command::Presence { status } => {
                if status != weft_proto::PresenceStatus::Invisible {
                    let handles: Vec<ChannelHandle> =
                        self.joined.values().map(|j| j.handle.clone()).collect();
                    for handle in handles {
                        handle.presence(self.id, status).await;
                    }
                }
                Ok(Flow::Continue)
            }
            Command::Mark { channel, msgid } => self.on_mark(label, channel, msgid, account).await,
            // §6.5 / §6.3 capability verbs.
            Command::Grant {
                subject,
                scope,
                caps,
                expiry,
            } => {
                self.on_grant(label, subject, scope, caps, expiry, account)
                    .await
            }
            Command::Revoke {
                subject,
                scope,
                caps,
                epoch,
            } => {
                self.on_revoke(label, subject, scope, caps, epoch, account)
                    .await
            }
            Command::ChannelCreate { channel, policy } => {
                self.on_channel_create(label, channel, policy, account)
                    .await
            }
            Command::ChannelPolicy {
                channel,
                policy,
                purge,
            } => {
                self.on_channel_policy(label, channel, policy, purge, account)
                    .await
            }
            Command::ChannelMeta {
                channel,
                key,
                value,
            } => {
                self.on_channel_meta(label, channel, key, value, account)
                    .await
            }
            Command::ChannelDelete { channel, confirm } => {
                self.on_channel_delete(label, channel, confirm, account)
                    .await
            }
            Command::InviteMint {
                scope,
                max_uses,
                expiry,
            } => {
                self.on_invite_mint(label, scope, max_uses, expiry, account)
                    .await
            }
            Command::InviteRevoke { invite_id } => {
                self.on_invite_revoke(label, invite_id, account).await
            }
            Command::InviteRedeem { token } => self.on_invite_redeem(label, token, account).await,
            // §6.2 namespace verbs.
            Command::NsCreate {
                name,
                visibility,
                root_key,
            } => {
                self.on_ns_create(label, name, visibility, root_key, account)
                    .await
            }
            Command::NsMeta { name, key, value } => {
                self.on_ns_meta(label, name, key, value, account).await
            }
            Command::NsVisibility { name, visibility } => {
                self.on_ns_visibility(label, name, visibility, account)
                    .await
            }
            Command::NsDelegate {
                name,
                subject,
                caps,
            } => {
                // Sugar for GRANT at ns: scope (§6.2).
                self.on_grant(label, subject, format!("ns:{name}"), caps, None, account)
                    .await
            }
            Command::NsDelete { name, confirm } => {
                self.on_ns_delete(label, name, confirm, account).await
            }
            Command::Discover { cursor } => self.on_discover(label, cursor).await,
            Command::Channels { namespace } => self.on_channels(label, namespace).await,
            Command::Hello { .. }
            | Command::AuthPassword { .. }
            | Command::AuthKey { .. }
            | Command::AuthProof { .. }
            | Command::Register { .. } => self.not_authed(label, "already authenticated").await,
            Command::Unknown { .. } => Ok(Flow::Continue), // handled in on_request
        }
    }

    // ---- READY verb handlers ----

    async fn on_join(
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
        let Some(handle) = self.ctx.registry.get(&channel) else {
            // §2.2 anti-enumeration: unknown and hidden channels share this
            // one code.
            return self.no_such_target(label).await;
        };
        // §6.3 view gating: a view-gated channel is invisible to a
        // non-member without the `view` cap — same NO-SUCH-TARGET as a
        // channel that does not exist (invariant 1).
        if self.view_gated_denied(&channel, &account).await {
            return self.no_such_target(label).await;
        }
        let Some(ack) = handle.join(self.id, account.clone()).await else {
            self.send_err(label, ErrCode::Internal, None, "channel unavailable")
                .await?;
            return Ok(Flow::Continue);
        };
        // Re-JOIN replaces the subscription. Pending echo labels die with
        // the old receiver (their broadcasts went there), so drop them too.
        if let Some(old) = self.joined.remove(&channel) {
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
        // §6.3 direct response: MEMBER (with count=) + POLICY, both labeled.
        let me = UserRef::new(account, self.ctx.info.network.clone());
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
                channel,
                policy: ack.policy,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    async fn on_part(&mut self, label: Option<String>, channel: ChannelName) -> io::Result<Flow> {
        let State::Ready { account } = self.state.clone() else {
            unreachable!("on_part only dispatched in READY");
        };
        match self.joined.remove(&channel) {
            None => self.no_such_target(label).await,
            Some(joined) => {
                joined.forwarder.abort();
                joined.handle.part(self.id).await;
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

    async fn on_typing(
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

    async fn on_msg(
        &mut self,
        label: Option<String>,
        target: Target,
        body: Option<String>,
        meta: MsgMeta,
    ) -> io::Result<Flow> {
        if !meta.attachments.is_empty() {
            return self.unsupported(label, "media lands in M6").await;
        }
        // §6.4: empty body legal iff attachments — and M3 has none.
        let body = body.unwrap_or_default();
        if body.is_empty() {
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
                let joined = self
                    .joined
                    .get_mut(&channel)
                    .expect("membership checked above");
                joined.pending.push_back(label);
                joined.handle.publish(self.id, body, meta).await;
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
    async fn resolve_message(
        &mut self,
        label: Option<String>,
        msgid: &MsgId,
        account: &Account,
        cap: &'static str,
        must_be_author: bool,
    ) -> io::Result<Option<MessageRoute>> {
        // §11.4: EDIT/DELETE are honored only at the msgid's origin.
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
        let root = match self.ctx.events.find_root(msgid.ulid()).await {
            Err(e) => {
                self.internal(label, &e).await?;
                return Ok(None);
            }
            Ok(None) => {
                self.no_such_target(label).await?;
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
                Ok(Some(MessageRoute::Channel {
                    handle: joined.handle.clone(),
                    channel,
                    root: root.msgid,
                }))
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
        }
    }

    async fn on_edit(
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
        }
        Ok(Flow::Continue) // ack = the echoed EDITED broadcast
    }

    async fn on_delete(
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
        }
        Ok(Flow::Continue)
    }

    async fn on_react(
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
        }
        Ok(Flow::Continue)
    }

    fn push_pending(&mut self, channel: &ChannelName, label: Option<String>) {
        if let Some(joined) = self.joined.get_mut(channel) {
            joined.pending.push_back(label);
        }
    }

    // ---- HISTORY (§6.4, §12.1) ----

    async fn on_history(
        &mut self,
        label: Option<String>,
        target: Target,
        before: Option<MsgId>,
        after: Option<MsgId>,
        limit: Option<u32>,
        thread: Option<MsgId>,
    ) -> io::Result<Flow> {
        if thread.is_some() {
            return self.unsupported(label, "thread filter lands in M6").await;
        }
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
        };
        let limit = limit.unwrap_or(100).clamp(1, weft_proto::MAX_HISTORY_LIMIT) as usize;

        let (items, truncated) = if policy == weft_proto::RetentionPolicy::Ephemeral {
            // §5.2 relay-only: nothing stored, and saying so is mandatory.
            (Vec::new(), true)
        } else {
            let page = weft_store::Page {
                before: before.as_ref().map(|m| m.ulid()),
                after: after.as_ref().map(|m| m.ulid()),
                limit,
            };
            let roots = match self.ctx.events.roots(&scope, page).await {
                Ok(roots) => roots,
                Err(e) => return self.internal(label, &e).await,
            };
            let root_ulids: Vec<_> = roots.iter().map(|r| r.msgid.ulid()).collect();
            let children = match self.ctx.events.children(&scope, &root_ulids).await {
                Ok(children) => children,
                Err(e) => return self.internal(label, &e).await,
            };
            let watermark = match self.ctx.events.purged_before(&scope).await {
                Ok(watermark) => watermark,
                Err(e) => return self.internal(label, &e).await,
            };
            let items = weft_store::materialize(roots, children);
            // §6.4: `truncated` marks retention gaps — set when this page
            // ran out of data (not merely full) while the window's older
            // edge reaches into the purged region.
            let window_floor_ms = after.as_ref().map(|m| m.timestamp_ms()).unwrap_or(0);
            let truncated = items.len() < limit && watermark.is_some_and(|w| window_floor_ms < w);
            (items, truncated)
        };

        self.batches += 1;
        let id = format!("b{}", self.batches);
        // §3.5: batches are data pages — every line echoes the label.
        self.send_event(label.clone(), Event::BatchStart { id: id.clone() })
            .await?;
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
                    self.send_event(
                        label.clone(),
                        Event::Message(Box::new(weft_proto::MessageEvent {
                            target: target.clone(),
                            sender,
                            msgid: msgid.clone(),
                            body,
                            meta,
                            edited: edited.map(|(count, _)| count),
                            edited_at: edited.map(|(_, at)| at),
                        })),
                    )
                    .await?;
                    for summary in reactions {
                        self.send_event(
                            label.clone(),
                            Event::Reactions {
                                target: target.clone(),
                                msgid: msgid.clone(),
                                emoji: summary.emoji,
                                count: summary.count,
                                by: summary.actors,
                            },
                        )
                        .await?;
                    }
                }
                weft_store::HistoryItem::Tombstone { msgid, by } => {
                    self.send_event(
                        label.clone(),
                        Event::Deleted {
                            target: target.clone(),
                            msgid,
                            by: Some(by),
                        },
                    )
                    .await?;
                }
            }
        }
        debug!(target = %target, truncated, "HISTORY served");
        self.send_event(
            label,
            Event::BatchEnd {
                id,
                truncated,
                // §12.1: the wire form is always the compacted
                // materialization, whatever the storage still holds.
                compacted: true,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    /// §6.3 MARK: persist the read marker, echo MARKED (the direct
    /// response), and sync the account's other sessions via the directory.
    async fn on_mark(
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
        self.ctx
            .directory
            .mark_sync(self.id, account, channel, msgid)
            .await;
        Ok(Flow::Continue)
    }

    // ---- account-scoped events (directory) ----

    /// DM/MARK events from the directory. Same echo rule as channel
    /// broadcasts: our own mutation events pop the direct-label FIFO.
    async fn on_direct(&mut self, direct: DirectEvent) -> io::Result<()> {
        if direct.origin != self.id {
            return self.send_event(None, direct.event).await;
        }
        match direct.event {
            ev @ (Event::Message(_)
            | Event::Edited { .. }
            | Event::Deleted { .. }
            | Event::Reaction { .. }) => {
                let label = self.pending_direct.pop_front().flatten();
                let reply = Reply {
                    label: label.clone(),
                    event: ev,
                };
                match reply.serialize() {
                    Ok(line) => {
                        self.stream.send_line(&line).await?;
                        if let Some(l) = label {
                            self.dedup.insert(
                                l,
                                DedupEntry {
                                    line,
                                    at: Instant::now(),
                                },
                            );
                        }
                    }
                    Err(e) => error!(?e, "unserializable direct echo (bug)"),
                }
                Ok(())
            }
            // Own MARKED sync copies don't exist (directory skips origin).
            _ => Ok(()),
        }
    }

    // ---- capability verbs (§6.5 GRANT/REVOKE/INVITE, §6.3 CHANNEL) ----

    /// True if a view-gated channel must hide from `account` (no `view`
    /// cap) — indistinguishable from nonexistent (invariant 1). Store
    /// errors fail closed (deny) so a hiccup never leaks existence.
    async fn view_gated_denied(&self, channel: &ChannelName, account: &Account) -> bool {
        match self.ctx.channel_store.channel(channel).await {
            Ok(Some(record)) if record.view_gated => {
                let scope = TokenScope::Channel(channel.to_string());
                match self
                    .ctx
                    .account_has_cap(account, &Capability::View, &scope, unix_now())
                    .await
                {
                    Ok(has) => !has,
                    Err(e) => {
                        error!("view-gate cap check failed: {e}");
                        true
                    }
                }
            }
            Ok(_) => false,
            Err(e) => {
                error!("view-gate lookup failed: {e}");
                false
            }
        }
    }

    async fn cap_required(&mut self, label: Option<String>, cap: &str) -> io::Result<Flow> {
        self.send_err(label, ErrCode::CapRequired, Some(cap), "missing capability")
            .await?;
        Ok(Flow::Continue)
    }

    async fn on_grant(
        &mut self,
        label: Option<String>,
        subject: String,
        scope: String,
        caps: String,
        expiry: Option<u64>,
        account: Account,
    ) -> io::Result<Flow> {
        let Some(token_scope) = TokenScope::parse(&scope) else {
            return self.bad_scope(label).await;
        };
        // A grant at ns: scope needs the namespace to exist; enforcement
        // works through owner/table (the network-key-signed token is a
        // same-network artifact — a root-key-signed chain arrives with
        // federation, M5).
        if let TokenScope::Namespace(ns) = &token_scope {
            if !self.namespace_exists(ns).await {
                return self.no_such_target(label).await;
            }
        }
        let parsed = match parse_caps(&caps) {
            Some(caps) => caps,
            None => {
                self.send_err(label, ErrCode::Malformed, None, "unknown capability")
                    .await?;
                return Ok(Flow::Continue);
            }
        };
        let now = unix_now();
        // Invariant 4: authority checked before any state change.
        for cap in &parsed {
            match self
                .ctx
                .account_can_grant(&account, cap, &token_scope, now)
                .await
            {
                Ok(true) => {}
                Ok(false) => return self.cap_required(label, &format!("grant:{cap}")).await,
                Err(e) => return self.internal(label, &e).await,
            }
        }
        let epoch = match self.ctx.caps.scope_epoch(&scope).await {
            Ok(epoch) => epoch,
            Err(e) => return self.internal(label, &e).await,
        };
        let absolute_expiry = expiry.map(|ttl| now + ttl);
        let cap_strings: Vec<String> = parsed.iter().map(Capability::to_string).collect();
        if let Err(e) = self
            .ctx
            .caps
            .record_grant(&subject, &scope, &cap_strings, epoch, absolute_expiry)
            .await
        {
            return self.internal(label, &e).await;
        }
        let token = self.ctx.mint_token(
            subject_from_str(&subject),
            token_scope,
            parsed,
            epoch,
            absolute_expiry.unwrap_or(u64::MAX),
        );
        self.send_event(
            label,
            Event::Token {
                subject,
                scope,
                token,
                expiry: absolute_expiry,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    async fn on_revoke(
        &mut self,
        label: Option<String>,
        subject: String,
        scope: String,
        caps: Option<String>,
        epoch: Option<u64>,
        account: Account,
    ) -> io::Result<Flow> {
        let Some(token_scope) = TokenScope::parse(&scope) else {
            return self.bad_scope(label).await;
        };
        // The caps we intend to remove (given, or all currently held).
        let grants = match self.ctx.caps.grants_for(&subject).await {
            Ok(grants) => grants,
            Err(e) => return self.internal(label, &e).await,
        };
        let cap_list: Option<Vec<String>> = caps.as_ref().map(|c| {
            c.split(',')
                .filter(|c| !c.is_empty())
                .map(str::to_string)
                .collect()
        });
        let target: Vec<Capability> = match &cap_list {
            Some(list) => list.iter().filter_map(|c| c.parse().ok()).collect(),
            None => grants
                .iter()
                .filter(|g| g.scope == scope)
                .flat_map(|g| g.caps.iter())
                .filter_map(|c| c.parse().ok())
                .collect(),
        };
        let now = unix_now();
        for cap in &target {
            match self
                .ctx
                .account_can_grant(&account, cap, &token_scope, now)
                .await
            {
                Ok(true) => {}
                Ok(false) => return self.cap_required(label, &format!("grant:{cap}")).await,
                Err(e) => return self.internal(label, &e).await,
            }
        }
        if let Err(e) = self
            .ctx
            .caps
            .revoke_grants(&subject, &scope, cap_list.as_deref())
            .await
        {
            return self.internal(label, &e).await;
        }
        // `epoch` present = bump the scope's revocation epoch, killing every
        // already-issued token there (§10.4).
        let new_epoch = if epoch.is_some() {
            self.ctx.caps.bump_epoch(&scope).await
        } else {
            self.ctx.caps.scope_epoch(&scope).await
        };
        let new_epoch = match new_epoch {
            Ok(epoch) => epoch,
            Err(e) => return self.internal(label, &e).await,
        };
        // Re-mint a token reflecting what remains (empty caps if none).
        let remaining: Vec<Capability> = match self.ctx.caps.grants_for(&subject).await {
            Ok(grants) => grants
                .into_iter()
                .filter(|g| g.scope == scope)
                .flat_map(|g| g.caps)
                .filter_map(|c| c.parse().ok())
                .collect(),
            Err(e) => return self.internal(label, &e).await,
        };
        let token = self.ctx.mint_token(
            subject_from_str(&subject),
            token_scope,
            remaining,
            new_epoch,
            u64::MAX,
        );
        self.send_event(
            label,
            Event::Token {
                subject,
                scope,
                token,
                expiry: None,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    async fn on_channel_create(
        &mut self,
        label: Option<String>,
        channel: ChannelName,
        policy: Option<RetentionPolicy>,
        account: Account,
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
            .account_has_cap(&account, &Capability::ChanCreate, &scope, unix_now())
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
                    .upsert_channel(&channel, policy)
                    .await
                {
                    return self.internal(label, &e).await;
                }
                debug!(%channel, "channel created");
                self.send_event(label, Event::Policy { channel, policy })
                    .await?;
                Ok(Flow::Continue)
            }
        }
    }

    async fn on_channel_policy(
        &mut self,
        label: Option<String>,
        channel: ChannelName,
        policy: RetentionPolicy,
        purge: bool,
        account: Account,
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
            .account_has_cap(&account, &Capability::Policy, &scope, unix_now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "policy").await,
            Err(e) => return self.internal(label, &e).await,
        }
        if let Err(e) = self
            .ctx
            .channel_store
            .upsert_channel(&channel, policy)
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

    async fn on_channel_meta(
        &mut self,
        label: Option<String>,
        channel: ChannelName,
        key: String,
        value: String,
        account: Account,
    ) -> io::Result<Flow> {
        if !self.ctx.registry.exists(&channel) {
            return self.no_such_target(label).await;
        }
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
                    "meta key must be topic|view-gated|category|position",
                )
                .await?;
                return Ok(Flow::Continue);
            }
        };
        if let Err(e) = result {
            return self.internal(label, &e).await;
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

    async fn on_channel_delete(
        &mut self,
        label: Option<String>,
        channel: ChannelName,
        confirm: ChannelName,
        account: Account,
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
            .account_has_cap(&account, &Capability::NsAdmin, &scope, unix_now())
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

    async fn on_invite_mint(
        &mut self,
        label: Option<String>,
        scope: String,
        max_uses: Option<u32>,
        expiry: Option<u64>,
        account: Account,
    ) -> io::Result<Flow> {
        let Some(token_scope) = TokenScope::parse(&scope) else {
            return self.bad_scope(label).await;
        };
        match self
            .ctx
            .account_has_cap(&account, &Capability::Invite, &token_scope, unix_now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "invite").await,
            Err(e) => return self.internal(label, &e).await,
        }
        // The invite grants membership (view+send) at the scope on redeem.
        let caps = vec!["view".to_string(), "send".to_string()];
        let invite_id = format!("i{}", weft_proto::Ulid::new());
        let absolute_expiry = expiry.map(|ttl| unix_now() + ttl);
        if let Err(e) = self
            .ctx
            .invites
            .create_invite(InviteRecord {
                id: invite_id.clone(),
                scope: scope.clone(),
                caps,
                uses_left: max_uses,
                expiry: absolute_expiry,
            })
            .await
        {
            return self.internal(label, &e).await;
        }
        let link = format!("weft://{}/i/{invite_id}", self.ctx.info.network);
        self.send_event(
            label,
            Event::Invited {
                scope,
                invite_id: invite_id.clone(),
                token: invite_id,
                link: Some(link),
                max_uses,
                expiry: absolute_expiry,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    async fn on_invite_revoke(
        &mut self,
        label: Option<String>,
        invite_id: String,
        account: Account,
    ) -> io::Result<Flow> {
        let invite = match self.ctx.invites.invite(&invite_id).await {
            Ok(Some(invite)) => invite,
            Ok(None) => return self.no_such_target(label).await,
            Err(e) => return self.internal(label, &e).await,
        };
        let Some(token_scope) = TokenScope::parse(&invite.scope) else {
            return self.no_such_target(label).await;
        };
        match self
            .ctx
            .account_has_cap(&account, &Capability::Invite, &token_scope, unix_now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "invite").await,
            Err(e) => return self.internal(label, &e).await,
        }
        if let Err(e) = self.ctx.invites.revoke_invite(&invite_id).await {
            return self.internal(label, &e).await;
        }
        // Confirmation: the invite echoed back closed (max-uses=0, no link).
        self.send_event(
            label,
            Event::Invited {
                scope: invite.scope,
                invite_id: invite_id.clone(),
                token: invite_id,
                link: None,
                max_uses: Some(0),
                expiry: None,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    async fn on_invite_redeem(
        &mut self,
        label: Option<String>,
        invite_id: String,
        account: Account,
    ) -> io::Result<Flow> {
        let outcome = match self.ctx.invites.redeem_invite(&invite_id, unix_now()).await {
            Ok(outcome) => outcome,
            Err(e) => return self.internal(label, &e).await,
        };
        // §6.5/§2.2: dead or exhausted invites are indistinct from absent.
        let invite = match outcome {
            RedeemOutcome::Redeemed(invite) => invite,
            RedeemOutcome::Exhausted | RedeemOutcome::Gone => {
                return self.no_such_target(label).await;
            }
        };
        // Bind the granted membership caps to the redeemer.
        let epoch = match self.ctx.caps.scope_epoch(&invite.scope).await {
            Ok(epoch) => epoch,
            Err(e) => return self.internal(label, &e).await,
        };
        if let Err(e) = self
            .ctx
            .caps
            .record_grant(account.as_str(), &invite.scope, &invite.caps, epoch, None)
            .await
        {
            return self.internal(label, &e).await;
        }
        debug!(%account, scope = %invite.scope, "invite redeemed");
        // Channel-scope invites auto-join (§6.5); namespace-scope invites
        // grant membership and return the namespace's NS-META so the client
        // knows what it joined (its channels come via DISCOVER/JOIN).
        match TokenScope::parse(&invite.scope) {
            Some(TokenScope::Channel(chan)) => match chan.parse::<ChannelName>() {
                Ok(channel) => self.on_join(label, channel, None, account).await,
                Err(_) => self.no_such_target(label).await,
            },
            Some(TokenScope::Namespace(ns)) => match ns.parse::<weft_proto::NamespaceName>() {
                Ok(name) => match self.ctx.namespaces.namespace(&name).await {
                    Ok(Some(record)) => {
                        self.send_event(label, Self::ns_meta_event(&record)).await?;
                        Ok(Flow::Continue)
                    }
                    Ok(None) => self.no_such_target(label).await,
                    Err(e) => self.internal(label, &e).await,
                },
                Err(_) => self.no_such_target(label).await,
            },
            _ => self.no_such_target(label).await,
        }
    }

    async fn bad_scope(&mut self, label: Option<String>) -> io::Result<Flow> {
        self.send_err(
            label,
            ErrCode::Malformed,
            None,
            "scope must be #chan, ns:<name>, or *",
        )
        .await?;
        Ok(Flow::Continue)
    }

    // ---- namespace verbs (§6.2 NS / DISCOVER) ----

    async fn namespace_exists(&self, name: &str) -> bool {
        let Ok(name) = name.parse::<weft_proto::NamespaceName>() else {
            return false;
        };
        matches!(self.ctx.namespaces.namespace(&name).await, Ok(Some(_)))
    }

    /// Build the NS-META reply for a namespace record.
    fn ns_meta_event(record: &weft_store::NamespaceRecord) -> Event {
        Event::NsMeta {
            name: record.name.clone(),
            visibility: record.visibility.parse().unwrap_or(Visibility::Unlisted),
            owner: Some(record.owner.to_string()),
            title: record.title.clone(),
            description: record.description.clone(),
            icon: record.icon.clone(),
        }
    }

    async fn on_ns_create(
        &mut self,
        label: Option<String>,
        name: weft_proto::NamespaceName,
        visibility: Visibility,
        root_key: String,
        account: Account,
    ) -> io::Result<Flow> {
        // The submitted root key must be a real Ed25519 pubkey (§2.1).
        if weft_crypto::PublicKey::from_b64(&root_key).is_err() {
            self.send_err(
                label,
                ErrCode::Malformed,
                None,
                "root must be a b64 ed25519 pubkey",
            )
            .await?;
            return Ok(Flow::Continue);
        }
        // §2.2 creation policy: gated needs `ns-create`; open enforces a
        // per-account quota.
        if self.ctx.ns_creation_open {
            let owned = match self.ctx.namespaces.namespaces_owned(account.as_str()).await {
                Ok(n) => n,
                Err(e) => return self.internal(label, &e).await,
            };
            if owned >= self.ctx.ns_quota {
                let mut err = ErrEvent::new(ErrCode::Quota, "namespace quota reached");
                err.max = Some(self.ctx.ns_quota);
                self.send_event(label, Event::Err(err)).await?;
                return Ok(Flow::Continue);
            }
        } else {
            let scope = TokenScope::Wildcard;
            match self
                .ctx
                .account_has_cap(&account, &Capability::NsCreate, &scope, unix_now())
                .await
            {
                Ok(true) => {}
                Ok(false) => return self.cap_required(label, "ns-create").await,
                Err(e) => return self.internal(label, &e).await,
            }
        }
        let record = weft_store::NamespaceRecord {
            name: name.clone(),
            owner: account.clone(),
            root_key,
            visibility: visibility.to_string(),
            title: None,
            description: None,
            icon: None,
        };
        match self.ctx.namespaces.create_namespace(record.clone()).await {
            Ok(true) => {
                debug!(%name, %account, "namespace created");
                self.send_event(label, Self::ns_meta_event(&record)).await?;
                Ok(Flow::Continue)
            }
            Ok(false) => {
                self.send_err(label, ErrCode::Conflict, None, "namespace name is taken")
                    .await?;
                Ok(Flow::Continue)
            }
            Err(e) => self.internal(label, &e).await,
        }
    }

    /// Shared owner/ns-admin gate for NS META/VISIBILITY/DELETE.
    /// `Ok(Some(record))` = authorized; `Ok(None)` = refused/answered.
    async fn ns_admin_gate(
        &mut self,
        label: Option<String>,
        name: &weft_proto::NamespaceName,
        account: &Account,
    ) -> io::Result<Option<weft_store::NamespaceRecord>> {
        let record = match self.ctx.namespaces.namespace(name).await {
            Ok(Some(record)) => record,
            Ok(None) => {
                self.no_such_target(label).await?;
                return Ok(None);
            }
            Err(e) => {
                self.internal(label, &e).await?;
                return Ok(None);
            }
        };
        let scope = TokenScope::Namespace(name.to_string());
        match self
            .ctx
            .account_has_cap(account, &Capability::NsAdmin, &scope, unix_now())
            .await
        {
            Ok(true) => Ok(Some(record)),
            Ok(false) => {
                self.cap_required(label, "ns-admin").await?;
                Ok(None)
            }
            Err(e) => {
                self.internal(label, &e).await?;
                Ok(None)
            }
        }
    }

    async fn on_ns_meta(
        &mut self,
        label: Option<String>,
        name: weft_proto::NamespaceName,
        key: String,
        value: String,
        account: Account,
    ) -> io::Result<Flow> {
        if !matches!(key.as_str(), "title" | "description" | "icon") {
            self.send_err(
                label,
                ErrCode::Policy,
                None,
                "meta key must be title|description|icon",
            )
            .await?;
            return Ok(Flow::Continue);
        }
        let Some(mut record) = self.ns_admin_gate(label.clone(), &name, &account).await? else {
            return Ok(Flow::Continue);
        };
        if let Err(e) = self
            .ctx
            .namespaces
            .set_namespace_meta(&name, &key, &value)
            .await
        {
            return self.internal(label, &e).await;
        }
        match key.as_str() {
            "title" => record.title = Some(value),
            "description" => record.description = Some(value),
            "icon" => record.icon = Some(value),
            _ => {}
        }
        self.send_event(label, Self::ns_meta_event(&record)).await?;
        Ok(Flow::Continue)
    }

    async fn on_ns_visibility(
        &mut self,
        label: Option<String>,
        name: weft_proto::NamespaceName,
        visibility: Visibility,
        account: Account,
    ) -> io::Result<Flow> {
        let Some(mut record) = self.ns_admin_gate(label.clone(), &name, &account).await? else {
            return Ok(Flow::Continue);
        };
        if let Err(e) = self
            .ctx
            .namespaces
            .set_namespace_visibility(&name, &visibility.to_string())
            .await
        {
            return self.internal(label, &e).await;
        }
        record.visibility = visibility.to_string();
        self.send_event(label, Self::ns_meta_event(&record)).await?;
        Ok(Flow::Continue)
    }

    async fn on_ns_delete(
        &mut self,
        label: Option<String>,
        name: weft_proto::NamespaceName,
        confirm: weft_proto::NamespaceName,
        account: Account,
    ) -> io::Result<Flow> {
        if name != confirm {
            self.send_err(
                label,
                ErrCode::Policy,
                None,
                "DELETE must repeat the namespace name",
            )
            .await?;
            return Ok(Flow::Continue);
        }
        // Owner or operator (§6.2). ns_admin_gate covers both (owner holds
        // ns-admin, operators hold everything).
        if self
            .ns_admin_gate(label.clone(), &name, &account)
            .await?
            .is_none()
        {
            return Ok(Flow::Continue);
        }
        if let Err(e) = self.ctx.namespaces.delete_namespace(&name).await {
            return self.internal(label, &e).await;
        }
        debug!(%name, "namespace deleted");
        // Reflect deletion as an NS-META marker (private + no owner).
        self.send_event(
            label,
            Event::NsMeta {
                name,
                visibility: Visibility::Private,
                owner: None,
                title: None,
                description: Some("deleted".to_string()),
                icon: None,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    async fn on_discover(
        &mut self,
        label: Option<String>,
        cursor: Option<String>,
    ) -> io::Result<Flow> {
        const PAGE: usize = 50;
        let public = match self
            .ctx
            .namespaces
            .list_public(cursor.as_deref(), PAGE)
            .await
        {
            Ok(public) => public,
            Err(e) => return self.internal(label, &e).await,
        };
        let next_cursor = (public.len() == PAGE)
            .then(|| public.last().map(|ns| ns.name.to_string()))
            .flatten();
        for record in &public {
            self.send_event(label.clone(), Self::ns_meta_event(record))
                .await?;
        }
        if let Some(cursor) = next_cursor {
            self.send_event(label, Event::More { cursor }).await?;
        }
        Ok(Flow::Continue)
    }

    /// The ordered channel layout of a namespace (spec extension). A
    /// non-member of a `private` namespace can't observe it (invariant 1).
    async fn on_channels(
        &mut self,
        label: Option<String>,
        namespace: weft_proto::NamespaceName,
    ) -> io::Result<Flow> {
        let record = match self.ctx.namespaces.namespace(&namespace).await {
            Ok(Some(record)) => record,
            Ok(None) => return self.no_such_target(label).await,
            Err(e) => return self.internal(label, &e).await,
        };
        // Private namespaces are invisible unless you belong (view cap).
        if record.visibility == "private" {
            let State::Ready { account } = self.state.clone() else {
                unreachable!("on_channels only dispatched in READY");
            };
            let scope = TokenScope::Namespace(namespace.to_string());
            let member = self
                .ctx
                .account_has_cap(&account, &Capability::View, &scope, unix_now())
                .await
                .unwrap_or(false);
            if !member {
                return self.no_such_target(label).await;
            }
        }
        let channels = match self
            .ctx
            .channel_store
            .channels_in_namespace(namespace.as_str())
            .await
        {
            Ok(channels) => channels,
            Err(e) => return self.internal(label, &e).await,
        };
        for (name, record) in channels {
            self.send_event(
                label.clone(),
                Event::ChannelLayout {
                    channel: name,
                    category: record.category,
                    position: record.position,
                },
            )
            .await?;
        }
        Ok(Flow::Continue)
    }

    // ---- channel events ----

    async fn on_event(&mut self, event: SessionEvent) -> io::Result<()> {
        match event {
            SessionEvent::Lagged { channel } => {
                // §9.2 backpressure: tell the client it lost events. The
                // forced HISTORY resync completes this once M3 exists.
                self.send_err(
                    None,
                    ErrCode::Slow,
                    Some(channel.as_str()),
                    "events dropped; resync required",
                )
                .await
            }
            SessionEvent::Channel { channel, event } => {
                if event.origin != self.id {
                    // Broadcast copies never carry a label (§3.5).
                    return self.send_event(None, event.event).await;
                }
                match event.event {
                    // Own MESSAGE/EDITED/DELETED/REACTION copy = the ack;
                    // attach the corresponding command's label (FIFO — the
                    // actor broadcasts this session's commands in send
                    // order, across all four types).
                    ev @ (Event::Message(_)
                    | Event::Edited { .. }
                    | Event::Deleted { .. }
                    | Event::Reaction { .. }) => {
                        let label = self
                            .joined
                            .get_mut(&channel)
                            .and_then(|j| j.pending.pop_front())
                            .flatten();
                        let reply = Reply {
                            label: label.clone(),
                            event: ev,
                        };
                        match reply.serialize() {
                            Ok(line) => {
                                self.stream.send_line(&line).await?;
                                if let Some(l) = label {
                                    self.dedup.insert(
                                        l,
                                        DedupEntry {
                                            line,
                                            at: Instant::now(),
                                        },
                                    );
                                }
                            }
                            Err(e) => error!(?e, "unserializable echo (bug)"),
                        }
                        Ok(())
                    }
                    // Own MEMBER/TYPING copies: the direct response was
                    // already sent from the command handler.
                    _ => Ok(()),
                }
            }
        }
    }

    // ---- response plumbing ----

    async fn send_event(&mut self, label: Option<String>, event: Event) -> io::Result<()> {
        let reply = Reply { label, event };
        match reply.serialize() {
            Ok(line) => self.stream.send_line(&line).await,
            // Our own events must always serialize; log instead of killing
            // the connection if a bug slips through.
            Err(e) => {
                error!(?e, "unserializable event (bug)");
                Ok(())
            }
        }
    }

    async fn send_err(
        &mut self,
        label: Option<String>,
        code: ErrCode,
        context: Option<&str>,
        text: &str,
    ) -> io::Result<()> {
        // One line per refusal: with RUST_LOG=debug the server explains
        // every ERR it ever sends (the wire stays uniform; logs may not).
        debug!(code = %code, context = ?context, label = ?label, "refused: {text}");
        let mut err = ErrEvent::new(code, text);
        err.context = context.map(str::to_string);
        self.send_event(label, Event::Err(err)).await
    }

    async fn not_authed(&mut self, label: Option<String>, text: &str) -> io::Result<Flow> {
        self.send_err(label, ErrCode::NotAuthed, None, text).await?;
        Ok(Flow::Continue)
    }

    async fn unsupported(&mut self, label: Option<String>, text: &str) -> io::Result<Flow> {
        self.send_err(label, ErrCode::Unsupported, None, text)
            .await?;
        Ok(Flow::Continue)
    }

    /// §8 INTERNAL: log the cause, tell the client nothing.
    async fn internal(
        &mut self,
        label: Option<String>,
        cause: &(impl std::fmt::Display + Sync),
    ) -> io::Result<Flow> {
        error!("storage failure: {cause}");
        self.send_err(label, ErrCode::Internal, None, "internal error")
            .await?;
        Ok(Flow::Continue)
    }

    async fn no_such_target(&mut self, label: Option<String>) -> io::Result<Flow> {
        // §8: the anti-enumeration code — one text for every flavor of "no".
        self.send_err(label, ErrCode::NoSuchTarget, None, "no such target")
            .await?;
        Ok(Flow::Continue)
    }

    /// Speaking into a channel we're not in. M3 channels are all public
    /// (config-listed), so distinguishing "join first" from "does not
    /// exist" leaks nothing; private channels (M4) must take the
    /// NO-SUCH-TARGET branch (§2.2).
    async fn not_member(
        &mut self,
        label: Option<String>,
        channel: &ChannelName,
    ) -> io::Result<Flow> {
        self.not_member_cap(label, channel, "send").await
    }

    /// Same split, naming the §10.4 capability the caller lacks (§8:
    /// CAP-REQUIRED names the cap).
    async fn not_member_cap(
        &mut self,
        label: Option<String>,
        channel: &ChannelName,
        cap: &'static str,
    ) -> io::Result<Flow> {
        if self.ctx.registry.exists(channel) {
            self.send_err(
                label,
                ErrCode::CapRequired,
                Some(cap),
                "join the channel first",
            )
            .await?;
            Ok(Flow::Continue)
        } else {
            self.no_such_target(label).await
        }
    }
}

/// Unix seconds — server-stamped time (§9.6); client clocks are untrusted.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) // pre-1970 clock: expire everything rather than panic
}

/// Parse a comma-separated cap list; `None` if any token is not a known
/// capability (§10.4).
fn parse_caps(caps: &str) -> Option<Vec<Capability>> {
    caps.split(',')
        .filter(|c| !c.is_empty())
        .map(|c| c.parse::<Capability>().ok())
        .collect()
}

/// A GRANT/REVOKE subject: a b64 device key if it parses as one, else an
/// account name (§6.5).
fn subject_from_str(s: &str) -> Subject {
    match PublicKey::from_b64(s) {
        Ok(key) => Subject::Key(key),
        Err(_) => Subject::Account(s.to_string()),
    }
}

/// Pump one channel's broadcast into the session queue, translating lag.
fn spawn_forwarder(
    channel: ChannelName,
    mut events: broadcast::Receiver<ChannelEvent>,
    queue: mpsc::Sender<SessionEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let msg = match events.recv().await {
                Ok(event) => SessionEvent::Channel {
                    channel: channel.clone(),
                    event,
                },
                Err(broadcast::error::RecvError::Lagged(_)) => SessionEvent::Lagged {
                    channel: channel.clone(),
                },
                Err(broadcast::error::RecvError::Closed) => return,
            };
            if queue.send(msg).await.is_err() {
                return; // session gone
            }
        }
    })
}
