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
use tracing::{debug, error, info_span, Instrument};
use weft_proto::{
    Account, ChannelName, Command, ErrCode, ErrEvent, Event, Line, MemberAction, MsgMeta,
    ParseError, Reply, Request, Target, UserRef, MAX_LABEL_BYTES,
};

use crate::channel::{ChannelEvent, ChannelHandle};
use crate::context::{ServerCtx, PROTOCOL_VERSION};
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

/// Events flowing from channel forwarders into the session task.
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

struct Session<S> {
    id: SessionId,
    stream: S,
    ctx: Arc<ServerCtx>,
    state: State,
    joined: HashMap<ChannelName, Joined>,
    events_tx: mpsc::Sender<SessionEvent>,
    events_rx: mpsc::Receiver<SessionEvent>,
    /// label → serialized echo, replayed verbatim on MSG retry (§9.2).
    dedup: HashMap<String, DedupEntry>,
    malformed_strikes: Vec<Instant>,
    last_inbound: Instant,
}

enum Action {
    Line(Option<String>),
    Event(SessionEvent),
    Idle,
}

impl<S: ControlStream> Session<S> {
    fn new(id: SessionId, stream: S, ctx: Arc<ServerCtx>) -> Self {
        let (events_tx, events_rx) = mpsc::channel(EVENT_QUEUE);
        Self {
            id,
            stream,
            ctx,
            state: State::Negotiating,
            joined: HashMap::new(),
            events_tx,
            events_rx,
            dedup: HashMap::new(),
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
                Action::Idle => {
                    debug!("idle timeout");
                    return Ok(());
                }
            }
        }
    }

    /// Leave all channels so members see MEMBER part even on abrupt drops.
    async fn cleanup(&mut self) {
        for (_, joined) in self.joined.drain() {
            joined.forwarder.abort();
            joined.handle.part(self.id).await;
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
            Flow::Close // §8: MALFORMED closes after 5/60 s
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
                if self.ctx.accounts.verify_password(&account, &password) {
                    self.welcome_authed(label, account, None).await
                } else {
                    self.auth_failed(label).await
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
                let enrolled = self
                    .ctx
                    .accounts
                    .device_enrolled(&pending.account, &pending.device);
                if proof_ok && enrolled {
                    let attestation =
                        self.ctx
                            .mint_attestation(&pending.account, pending.device, unix_now());
                    self.welcome_authed(label, pending.account, Some(attestation.to_b64()))
                        .await
                } else {
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
        match self.ctx.accounts.register(&account, password) {
            crate::accounts::RegisterOutcome::Exists => {
                self.send_err(label, ErrCode::Conflict, None, "account name is taken")
                    .await?;
                Ok(Flow::Continue)
            }
            crate::accounts::RegisterOutcome::Created => {
                self.welcome_authed(label, account, None).await
            }
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
                self.ctx.accounts.enroll_device(&account, device);
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
            Command::Presence { .. } => {
                self.unsupported(label, "presence is not offered yet").await
            }
            Command::Mark { .. } => self.unsupported(label, "read markers land in M3").await,
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
            return self.unsupported(label, "invites land in M4").await;
        }
        let Some(handle) = self.ctx.registry.get(&channel).cloned() else {
            // §2.2 anti-enumeration: unknown and (future) hidden channels
            // share this one code.
            return self.no_such_target(label).await;
        };
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
                forwarder,
                pending: VecDeque::new(),
            },
        );
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
        let channel = match target {
            Target::Channel(channel) => channel,
            Target::User(_) => return self.unsupported(label, "DMs land in M3").await,
        };
        if !meta.attachments.is_empty() {
            return self.unsupported(label, "media lands in M6").await;
        }
        // §6.4: empty body legal iff attachments — and M1 has none.
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
        if !self.joined.contains_key(&channel) {
            return self.not_member(label, &channel).await;
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
                .any(|j| j.pending.iter().any(|p| p.as_deref() == Some(l)));
            if in_flight {
                return Ok(Flow::Continue);
            }
        }
        let joined = self
            .joined
            .get_mut(&channel)
            .expect("membership checked above");
        joined.pending.push_back(label);
        joined.handle.publish(self.id, body, meta).await;
        // The ack is the echoed MESSAGE, sent when the broadcast returns.
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
                    // Own MESSAGE echo = the ack; attach the publish label.
                    ev @ Event::Message(_) => {
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

    async fn no_such_target(&mut self, label: Option<String>) -> io::Result<Flow> {
        // §8: the anti-enumeration code — one text for every flavor of "no".
        self.send_err(label, ErrCode::NoSuchTarget, None, "no such target")
            .await?;
        Ok(Flow::Continue)
    }

    /// Speaking into a channel we're not in. M1 channels are all public
    /// (config-listed), so distinguishing "join first" from "does not
    /// exist" leaks nothing; private channels (M4) must take the
    /// NO-SUCH-TARGET branch (§2.2).
    async fn not_member(
        &mut self,
        label: Option<String>,
        channel: &ChannelName,
    ) -> io::Result<Flow> {
        if self.ctx.registry.get(channel).is_some() {
            self.send_err(
                label,
                ErrCode::CapRequired,
                Some("send"),
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
