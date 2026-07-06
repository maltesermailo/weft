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
use weft_crypto::{Capability, PublicKey, SignedManifest, Subject, TokenScope};
use weft_proto::{
    Account, BridgeState, ChannelName, Command, ContentState, ErrCode, ErrEvent, Event,
    HistoryMode, Line, MediaMode, MemberAction, ModAction, MsgId, MsgMeta, NamespaceName,
    NetworkName, ParseError, Reply, ReportScope, ReportStatus, Request, ResolveAction,
    RetentionPolicy, Target, Ulid, UserRef, Visibility, MAX_LABEL_BYTES,
};

use weft_store::{
    EventKind, EventRecord, InviteRecord, ModKind, ModRecord, NetblockRecord, PeerRecord,
    RedeemOutcome, ReportRecord, ReportResolution, Scope,
};

use crate::bridge;
use crate::channel::{ChannelEvent, ChannelHandle};
use crate::context::{channel_namespace, covering_scopes, ServerCtx, PROTOCOL_VERSION};
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
/// §2.4 recovery delay windows: rung 2 (social quorum) 7 days, rung 3
/// (operator last resort) 30 days.
const RECOVERY_DELAY_RUNG2_SECS: u64 = 7 * 24 * 3600;
const RECOVERY_DELAY_RUNG3_SECS: u64 = 30 * 24 * 3600;
/// §6.7 report rate limit: RECOMMENDED 10 per rolling hour, per account.
const REPORT_RATE_LIMIT: u64 = 10;
const REPORT_RATE_WINDOW_MS: u64 = 3600 * 1000;
/// §12.1 grace: retention holds survive a resolution by this window.
const REPORT_HOLD_GRACE_MS: u64 = 7 * 24 * 3600 * 1000;
/// Bound on the session's event queue; overflow propagates to broadcast
/// lag → `ERR SLOW`, never unbounded memory.
const EVENT_QUEUE: usize = 256;

/// Drive one connection to completion. This is weftd's entire per-connection
/// entry point: wrap the transport in a [`ControlStream`] and call this.
/// Increments `ctx.connections` for the lifetime of a session; decrements on
/// drop (every exit path). Powers the admin panel's live-connection count.
struct ConnectionGuard(Arc<std::sync::atomic::AtomicUsize>);

impl ConnectionGuard {
    fn enter(ctx: &ServerCtx) -> Self {
        ctx.connections
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self(Arc::clone(&ctx.connections))
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

pub async fn run_session<S: ControlStream>(stream: S, ctx: Arc<ServerCtx>) {
    let id = ctx.next_session_id();
    let span = info_span!("session", id);
    // Count this live connection for the whole session; the guard decrements on
    // any exit path (error, close, panic-unwind).
    let _conn = ConnectionGuard::enter(&ctx);
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

/// AUTH KEY / AUTH BRIDGE state between CHALLENGE and PROOF (§6.1, §11.2).
/// One per session; a new challenge replaces it, any PROOF consumes it.
#[derive(Debug, Clone)]
struct PendingChallenge {
    /// The key being proven — a device key (account auth) or the peer's
    /// network signing key (bridge auth).
    device: weft_crypto::PublicKey,
    nonce: [u8; weft_crypto::CHALLENGE_NONCE_LEN],
    subject: ChallengeSubject,
}

/// What a successful PROOF authenticates.
#[derive(Debug, Clone)]
enum ChallengeSubject {
    /// §6.1 device-key auth for an account.
    Device { account: Account },
    /// §11.2 bridge auth: the connecting party is the peer network.
    Bridge { peer: NetworkName },
}

#[derive(Debug, Clone)]
enum State {
    Negotiating,
    Unauthed {
        challenge: Option<PendingChallenge>,
    },
    Ready {
        account: Account,
    },
    /// §11.2 an authenticated bridge session with a peer network. Carries the
    /// events of that network; forwards our local-origin events to it. `key`
    /// is the signing key the peer proved control of (pinned or accept-any);
    /// signed manifests on this session verify against it.
    Bridge {
        peer: NetworkName,
        key: PublicKey,
    },
}

enum Flow {
    Continue,
    Close,
}

/// The outcome of an attempted channel join (shared by `JOIN` / `NS JOIN`).
enum JoinResult {
    Joined,
    /// No such channel actor.
    Missing,
    /// View-gated and the caller lacks `view`.
    Hidden,
    Banned,
    /// The channel actor was unreachable (raced teardown).
    Unavailable,
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
    /// §11: channels this (bridge) session forwards local-origin events on.
    /// One broadcast forwarder per bridged channel; empty for client sessions.
    bridged: HashMap<ChannelName, JoinHandle<()>>,
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
            bridged: HashMap::new(),
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
        for (_, forwarder) in self.bridged.drain() {
            forwarder.abort();
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
        // A bridge session's inbound stream is mostly *events* (a peer's
        // MESSAGE/EDITED/… for ingestion), which are not Commands — route
        // them before the Command decode that would treat them as Unknown.
        if let State::Bridge { peer, key } = self.state.clone() {
            return self.on_bridge_line(peer, key, &line).await;
        }
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
            // Bridge lines are intercepted in `on_line` before Command decode.
            State::Bridge { .. } => Ok(Flow::Continue),
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
                        device,
                        nonce,
                        subject: ChallengeSubject::Device { account },
                    }),
                };
                Ok(Flow::Continue)
            }
            // §11.2 open a bridge session: the peer proves control of its
            // network signing key. Only *configured* peers with a matching
            // pinned key are challenged; unknown peers or key mismatches take
            // the uniform AUTH-FAILED path (no peer-existence oracle).
            Command::AuthBridge { network, token } => {
                self.on_auth_bridge(label, network, token).await
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
                match pending.subject {
                    ChallengeSubject::Device { account } => {
                        let enrolled = match self
                            .ctx
                            .accounts
                            .device_enrolled(&account, &pending.device)
                            .await
                        {
                            Ok(enrolled) => enrolled,
                            Err(e) => return self.internal(label, &e).await,
                        };
                        if proof_ok && enrolled {
                            info!(%account, "authenticated (device key)");
                            let attestation =
                                self.ctx
                                    .mint_attestation(&account, pending.device, unix_now());
                            self.welcome_authed(label, account, Some(attestation.to_b64()))
                                .await
                        } else {
                            debug!(%account, proof_ok, enrolled, "key auth rejected");
                            self.auth_failed(label).await
                        }
                    }
                    // §11.2 bridge PROOF: the key was resolved (pinned or
                    // accept-any) at AUTH BRIDGE, so a valid proof of control
                    // establishes the bridge session, bound to that key.
                    ChallengeSubject::Bridge { peer } => {
                        if proof_ok {
                            info!(%peer, "bridge session authenticated");
                            self.welcome_bridge(label, peer, pending.device).await
                        } else {
                            debug!(%peer, "bridge auth rejected");
                            self.auth_failed(label).await
                        }
                    }
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
        // §6.3 restore persistent channel memberships — the client's channels
        // (and namespace tiles) reappear without re-joining.
        match self.ctx.memberships.memberships(&account).await {
            Ok(channels) => {
                for channel in channels {
                    self.join_one(&channel, &account, None).await?;
                }
            }
            Err(e) => error!("membership restore failed: {e}"),
        }
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
                // §6.1 remember it in-memory so MEMBERS serves correct dots.
                // Invisible is *removed* (renders offline, never revealed).
                {
                    let mut map = self.ctx.presence.lock().expect("presence lock");
                    if status == weft_proto::PresenceStatus::Invisible {
                        map.remove(&account);
                    } else {
                        map.insert(account.clone(), status);
                    }
                }
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
            // Pagination (`cursor`) isn't needed at reference channel sizes —
            // the roster is served in one batch.
            Command::Members { channel, .. } => self.on_members(label, channel).await,
            Command::Pin { msgid } => self.on_pin(label, msgid, account, true).await,
            Command::Unpin { msgid } => self.on_pin(label, msgid, account, false).await,
            Command::Pins { channel } => self.on_pins(label, channel).await,
            Command::Caps {
                account: subject,
                scope,
            } => self.on_caps(label, subject, scope).await,
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
            // §6.5 named roles (capability-token bundles).
            Command::RoleCreate {
                scope,
                color,
                caps,
                name,
            } => {
                self.on_role_create(label, scope, color, caps, name, account)
                    .await
            }
            Command::RoleDelete { scope, name } => {
                self.on_role_delete(label, scope, name, account).await
            }
            Command::RoleAssign {
                scope,
                account: subject,
                name,
            } => {
                self.on_role_assign(label, scope, subject, name, account)
                    .await
            }
            Command::RoleUnassign {
                scope,
                account: subject,
                name,
            } => {
                self.on_role_unassign(label, scope, subject, name, account)
                    .await
            }
            Command::RolesList { scope } => self.on_roles_list(label, scope).await,
            Command::RolesOf {
                scope,
                account: subject,
            } => self.on_roles_of(label, scope, subject).await,
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
            Command::ChannelRename { channel, new_name } => {
                self.on_channel_rename(label, channel, new_name, account)
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
            Command::NsJoin { name } => self.on_ns_join(label, name, account).await,
            Command::Discover { cursor } => self.on_discover(label, cursor).await,
            Command::Channels { namespace } => self.on_channels(label, namespace).await,
            // §2.4 succession + recovery ladder.
            Command::NsTransfer {
                name,
                new_owner,
                signature,
            } => {
                self.on_ns_transfer(label, name, new_owner, signature, account)
                    .await
            }
            Command::NsRecoverySet { name, m, keys } => {
                self.on_ns_recovery_set(label, name, m, keys, account).await
            }
            Command::NsRecover { name, rotation } => {
                self.on_ns_recover(label, name, rotation).await
            }
            Command::NsRecoveryCancel { name, signature } => {
                self.on_ns_recovery_cancel(label, name, signature).await
            }
            // §6.7 moderation & reporting.
            Command::Report {
                msgid,
                category,
                scope,
                note,
            } => {
                self.on_report(label, msgid, category, scope, note, account)
                    .await
            }
            Command::ReportsList {
                scope,
                status,
                cursor,
            } => {
                self.on_reports_list(label, scope, status, cursor, account)
                    .await
            }
            Command::ReportsResolve {
                report_id,
                action,
                note,
            } => {
                self.on_reports_resolve(label, report_id, action, note, account)
                    .await
            }
            // §6.7 moderation. `account` here is the acting moderator.
            Command::Mute {
                scope,
                account: target,
                reason,
            } => {
                self.on_moderate(label, scope, target, ModKind::Mute, true, reason, account)
                    .await
            }
            Command::Unmute {
                scope,
                account: target,
            } => {
                self.on_moderate(label, scope, target, ModKind::Mute, false, None, account)
                    .await
            }
            Command::Ban {
                scope,
                account: target,
                reason,
            } => {
                self.on_moderate(label, scope, target, ModKind::Ban, true, reason, account)
                    .await
            }
            Command::Unban {
                scope,
                account: target,
            } => {
                self.on_moderate(label, scope, target, ModKind::Ban, false, None, account)
                    .await
            }
            Command::Kick {
                channel,
                account: target,
                reason,
            } => self.on_kick(label, channel, target, reason, account).await,
            // §11 federation — operator-facing management (§6.6).
            Command::BridgePropose {
                scope,
                peer,
                history,
                media,
                typing,
                ..
            } => {
                self.on_bridge_propose(label, scope, peer, history, media, typing, account)
                    .await
            }
            Command::BridgeAccept { peer, version } => {
                self.on_bridge_accept_op(label, peer, version, account)
                    .await
            }
            Command::BridgeSever { peer } => self.on_bridge_sever_op(label, peer, account).await,
            // BRIDGE ADD/REMOVE amend an existing manifest; the reference
            // server manages the channel set through PROPOSE + the auto-acked
            // handshake, so these acknowledge without a separate op path yet.
            Command::BridgeAdd { .. } | Command::BridgeRemove { .. } => {
                self.unsupported(label, "BRIDGE ADD/REMOVE: manage via PROPOSE (M5)")
                    .await
            }
            Command::NetblockAdd { network, reason } => {
                self.on_netblock_add(label, network, reason, account).await
            }
            Command::NetblockRemove { network } => {
                self.on_netblock_remove(label, network, account).await
            }
            Command::NetblockList => self.on_netblock_list(label, account).await,
            // `AUTH BRIDGE` belongs in UNAUTHED; `REPORT-FORWARD` is
            // bridge-session-only (§11.9) — neither is valid from a client.
            Command::AuthBridge { .. } | Command::ReportForward { .. } => {
                self.not_authed(label, "not valid on a client session")
                    .await
            }
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

    /// §6.2 `NS JOIN <name>`: join every channel in the namespace the caller
    /// can see, skipping view-gated and banned ones ("not hidden by
    /// permissions"). No visible channel — nonexistent, private, or fully
    /// gated — answers `NO-SUCH-TARGET` (one code, anti-enumeration).
    async fn on_ns_join(
        &mut self,
        label: Option<String>,
        name: NamespaceName,
        account: Account,
    ) -> io::Result<Flow> {
        let channels = match self
            .ctx
            .channel_store
            .channels_in_namespace(name.as_str())
            .await
        {
            Ok(list) => list,
            Err(e) => return self.internal(label, &e).await,
        };
        let mut joined_any = false;
        for (channel, _record) in channels {
            // Per-channel joins are unlabeled (a bulk membership burst); the
            // client processes each MEMBER/POLICY as it arrives.
            if matches!(
                self.join_one(&channel, &account, None).await?,
                JoinResult::Joined
            ) {
                joined_any = true;
            }
        }
        if !joined_any {
            return self.no_such_target(label).await;
        }
        Ok(Flow::Continue)
    }

    /// Join one channel: registry lookup, view-gate + ban checks, subscribe,
    /// and emit the §6.3 `MEMBER` + `POLICY` response (with `label`). Shared by
    /// `JOIN` and `NS JOIN`; the caller maps the non-`Joined` results to errors.
    async fn join_one(
        &mut self,
        channel: &ChannelName,
        account: &Account,
        label: Option<String>,
    ) -> io::Result<JoinResult> {
        let Some(handle) = self.ctx.registry.get(channel) else {
            return Ok(JoinResult::Missing);
        };
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
        let Some(ack) = handle.join(self.id, account.clone()).await else {
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

    async fn on_part(&mut self, label: Option<String>, channel: ChannelName) -> io::Result<Flow> {
        let State::Ready { account } = self.state.clone() else {
            unreachable!("on_part only dispatched in READY");
        };
        match self.joined.remove(&channel) {
            None => self.no_such_target(label).await,
            Some(joined) => {
                joined.forwarder.abort();
                joined.handle.part(self.id).await;
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

        self.emit_batch(label, &target, items, truncated).await?;
        Ok(Flow::Continue)
    }

    /// Emit a `BATCH START` … events … `BATCH END` page (§7, §12.1). The wire
    /// form is always the compacted materialization; every line echoes the
    /// request label (§3.5). Shared by HISTORY and federated backfill (§11.7).
    async fn emit_batch(
        &mut self,
        label: Option<String>,
        target: &Target,
        items: Vec<weft_store::HistoryItem>,
        truncated: bool,
    ) -> io::Result<()> {
        self.batches += 1;
        let id = format!("b{}", self.batches);
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
        debug!(target = %target, truncated, "batch served");
        self.send_event(
            label,
            Event::BatchEnd {
                id,
                truncated,
                compacted: true,
            },
        )
        .await
    }

    /// §6.3 MEMBERS: a roster snapshot for a member. Framed as a `BATCH` of
    /// `MEMBER … join` (reusing the join event — the client folds each into its
    /// roster). Membership-gated; a hidden channel stays `NO-SUCH-TARGET`.
    async fn on_members(
        &mut self,
        label: Option<String>,
        channel: ChannelName,
    ) -> io::Result<Flow> {
        let Some(joined) = self.joined.get(&channel) else {
            return self.not_member_cap(label, &channel, "view").await;
        };
        let roster = joined.handle.roster().await;
        let count = roster.len() as u64;
        self.batches += 1;
        let id = format!("m{}", self.batches);
        self.send_event(label.clone(), Event::BatchStart { id: id.clone() })
            .await?;
        for account in roster {
            // §6.1 known presence rides along so the client's roster dots are
            // correct (not just members who change status while we watch).
            let status = {
                let map = self.ctx.presence.lock().expect("presence lock");
                map.get(&account).copied()
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
            if let Some(status) = status {
                self.send_event(None, Event::Presence { user, status })
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

    // ---- §6.4 pins ----

    /// `PIN`/`UNPIN <msgid>`: resolve the msgid's channel, cap-check `pin`, set
    /// the pin, and broadcast `PINNED`/`UNPINNED` to the channel.
    async fn on_pin(
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
    async fn on_pins(&mut self, label: Option<String>, channel: ChannelName) -> io::Result<Flow> {
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

    /// §10.4 `CAPS <account> <scope>`: the account's effective caps at the
    /// scope (public — caps aren't secret). Powers client capability badges.
    async fn on_caps(
        &mut self,
        label: Option<String>,
        subject: Account,
        scope_str: String,
    ) -> io::Result<Flow> {
        let Some(scope) = TokenScope::parse(&scope_str) else {
            return self.no_such_target(label).await;
        };
        let now = unix_now();
        let mut held = Vec::new();
        for cap in Capability::STANDARD {
            match self.ctx.account_has_cap(&subject, &cap, &scope, now).await {
                Ok(true) => held.push(cap.to_string()),
                Ok(false) => {}
                Err(e) => return self.internal(label, &e).await,
            }
        }
        self.send_event(
            label,
            Event::Caps {
                account: subject,
                scope: scope_str,
                caps: held.join(","),
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    /// §6.5 ROLE CREATE: define/replace a named capability-token bundle at a
    /// scope (scope admin only). Responds with the updated `ROLES` batch.
    async fn on_role_create(
        &mut self,
        label: Option<String>,
        scope: String,
        color: String,
        caps: String,
        name: String,
        account: Account,
    ) -> io::Result<Flow> {
        let Some(token_scope) = TokenScope::parse(&scope) else {
            return self.bad_scope(label).await;
        };
        if let TokenScope::Namespace(ns) = &token_scope {
            if !self.namespace_exists(ns).await {
                return self.no_such_target(label).await;
            }
        }
        let now = unix_now();
        match self
            .ctx
            .account_has_cap(&account, &Capability::NsAdmin, &token_scope, now)
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "ns-admin").await,
            Err(e) => return self.internal(label, &e).await,
        }
        // The bundle must be real capabilities (strict-out).
        let Some(parsed) = parse_caps(&caps) else {
            self.send_err(label, ErrCode::Malformed, None, "unknown capability")
                .await?;
            return Ok(Flow::Continue);
        };
        let cap_strings: Vec<String> = parsed.iter().map(Capability::to_string).collect();
        if let Err(e) = self
            .ctx
            .roles
            .set_role(&scope, &name, &color, &cap_strings)
            .await
        {
            return self.internal(label, &e).await;
        }
        // §6.5 always-propagate: a *channel* role-permission is granted to
        // everyone who currently holds the same-named namespace role, so the
        // permission applies immediately — no re-assignment needed.
        if let Some((ns, _)) = scope.strip_prefix('#').and_then(|s| s.split_once('/')) {
            self.propagate_channel_role(ns, &scope, &name, &cap_strings, &account)
                .await?;
        }
        self.on_roles_list(label, scope).await
    }

    /// Grant a channel role's caps to every **explicitly assigned** holder of
    /// the same-named namespace role — so editing a channel permission reaches
    /// existing members with no re-assignment (§6.5, "always propagate").
    async fn propagate_channel_role(
        &mut self,
        ns: &str,
        channel_scope: &str,
        role_name: &str,
        caps: &[String],
        actor: &Account,
    ) -> io::Result<()> {
        let ns_scope = format!("ns:{ns}");
        let members = self
            .ctx
            .roles
            .role_members(&ns_scope, role_name)
            .await
            .unwrap_or_default();
        let caps_csv = caps.join(",");
        for member in members {
            self.on_grant(
                None,
                member.to_string(),
                channel_scope.to_string(),
                caps_csv.clone(),
                None,
                actor.clone(),
            )
            .await?;
        }
        Ok(())
    }

    /// §6.5 ROLE DELETE (scope admin only) → updated `ROLES` batch.
    async fn on_role_delete(
        &mut self,
        label: Option<String>,
        scope: String,
        name: String,
        account: Account,
    ) -> io::Result<Flow> {
        let Some(token_scope) = TokenScope::parse(&scope) else {
            return self.bad_scope(label).await;
        };
        let now = unix_now();
        match self
            .ctx
            .account_has_cap(&account, &Capability::NsAdmin, &token_scope, now)
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "ns-admin").await,
            Err(e) => return self.internal(label, &e).await,
        }
        if let Err(e) = self.ctx.roles.delete_role(&scope, &name).await {
            return self.internal(label, &e).await;
        }
        self.on_roles_list(label, scope).await
    }

    /// §6.5 ROLE ASSIGN: grant the role's token bundle to an account. Resolves
    /// the role to its caps and reuses the GRANT path — the authority check
    /// (`account_can_grant`) and token issue are identical, so enforcement
    /// stays purely token-based.
    async fn on_role_assign(
        &mut self,
        label: Option<String>,
        scope: String,
        subject: Account,
        name: String,
        account: Account,
    ) -> io::Result<Flow> {
        let roles = match self.ctx.roles.roles(&scope).await {
            Ok(roles) => roles,
            Err(e) => return self.internal(label, &e).await,
        };
        let Some(role) = roles.into_iter().find(|r| r.name == name) else {
            return self.no_such_target(label).await;
        };
        // Record explicit membership — a role is held because it was assigned,
        // never inferred from caps (§6.5).
        if let Err(e) = self.ctx.roles.assign_role(&scope, &name, &subject).await {
            return self.internal(label, &e).await;
        }
        // Grant the role's own bundle at its scope (the labeled response).
        self.on_grant(
            label,
            subject.to_string(),
            scope.clone(),
            role.caps.join(","),
            None,
            account.clone(),
        )
        .await?;
        // §6.5 role channel-permissions: assigning a *namespace* role also
        // grants any same-named channel role's caps on every channel in that
        // namespace — so "give role X send in #chan" follows the assignment.
        if let Some(ns) = scope.strip_prefix("ns:") {
            for (cscope, caps) in self.channel_role_caps(ns, &name).await {
                self.on_grant(
                    None,
                    subject.to_string(),
                    cscope,
                    caps,
                    None,
                    account.clone(),
                )
                .await?;
            }
        }
        Ok(Flow::Continue)
    }

    /// §6.5 ROLE UNASSIGN: drop explicit membership and revoke the role's caps
    /// (its bundle at the scope + any same-named channel roles' caps).
    async fn on_role_unassign(
        &mut self,
        label: Option<String>,
        scope: String,
        subject: Account,
        name: String,
        account: Account,
    ) -> io::Result<Flow> {
        let Some(token_scope) = TokenScope::parse(&scope) else {
            return self.bad_scope(label).await;
        };
        let now = unix_now();
        match self
            .ctx
            .account_has_cap(&account, &Capability::NsAdmin, &token_scope, now)
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "ns-admin").await,
            Err(e) => return self.internal(label, &e).await,
        }
        let role = self
            .ctx
            .roles
            .roles(&scope)
            .await
            .unwrap_or_default()
            .into_iter()
            .find(|r| r.name == name);
        if let Err(e) = self.ctx.roles.unassign_role(&scope, &name, &subject).await {
            return self.internal(label, &e).await;
        }
        // Revoke the role's own caps, then any channel-role caps in the ns.
        if let Some(role) = role {
            let _ = self
                .ctx
                .caps
                .revoke_grants(subject.as_str(), &scope, Some(&role.caps))
                .await;
        }
        if let Some(ns) = scope.strip_prefix("ns:") {
            for (cscope, caps) in self.channel_role_caps(ns, &name).await {
                let caps: Vec<String> = caps.split(',').map(str::to_string).collect();
                let _ = self
                    .ctx
                    .caps
                    .revoke_grants(subject.as_str(), &cscope, Some(&caps))
                    .await;
            }
        }
        self.on_roles_of(label, scope, subject).await
    }

    /// §6.5 ROLES-OF: the roles an account is explicitly assigned at a scope.
    async fn on_roles_of(
        &mut self,
        label: Option<String>,
        scope: String,
        account: Account,
    ) -> io::Result<Flow> {
        let names = self
            .ctx
            .roles
            .roles_of(&scope, &account)
            .await
            .unwrap_or_default();
        self.send_event(
            label,
            Event::RoleMember {
                scope,
                account,
                roles: names.join(","),
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    /// `(channel-scope, caps-csv)` for every channel in `ns` that defines a
    /// role named `name` — the role's per-channel permissions (§6.5).
    async fn channel_role_caps(&self, ns: &str, name: &str) -> Vec<(String, String)> {
        let prefix = format!("#{ns}/");
        let channels = self
            .ctx
            .channel_store
            .list_channels()
            .await
            .unwrap_or_default();
        let mut out = Vec::new();
        for (chan, _) in channels {
            if !chan.as_str().starts_with(&prefix) {
                continue;
            }
            let cscope = chan.to_string();
            let croles = self.ctx.roles.roles(&cscope).await.unwrap_or_default();
            if let Some(crole) = croles.into_iter().find(|r| r.name == name) {
                if !crole.caps.is_empty() {
                    out.push((cscope, crole.caps.join(",")));
                }
            }
        }
        out
    }

    /// §6.5 ROLES: the role definitions at a scope, as a `BATCH` of `ROLE`.
    async fn on_roles_list(&mut self, label: Option<String>, scope: String) -> io::Result<Flow> {
        let roles = match self.ctx.roles.roles(&scope).await {
            Ok(roles) => roles,
            Err(e) => return self.internal(label, &e).await,
        };
        self.batches += 1;
        let id = format!("r{}", self.batches);
        self.send_event(label.clone(), Event::BatchStart { id: id.clone() })
            .await?;
        for role in roles {
            self.send_event(
                None,
                Event::Role {
                    scope: scope.clone(),
                    color: role.color,
                    caps: role.caps.join(","),
                    name: role.name,
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

    /// CHANNEL RENAME — change a channel's identity within its namespace (§6.3),
    /// re-keying every scoped record (invariant 4: cap first). The store move is
    /// atomic; the actor is respawned under the new name and members are told
    /// via `CHANNEL-RENAMED` so their clients re-join the new identity.
    async fn on_channel_rename(
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
            return self.send_event(label, Event::ChannelRenamed { old: channel.clone(), new: new_name }).await.map(|()| Flow::Continue);
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
        self.ctx
            .registry
            .rename(&channel, new_name.clone(), policy);
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

    /// Build the NS-META reply for a namespace record, including the §2.4
    /// recovery announcement fields.
    fn ns_meta_event(record: &weft_store::NamespaceRecord) -> Event {
        Event::NsMeta {
            name: record.name.clone(),
            visibility: record.visibility.parse().unwrap_or(Visibility::Unlisted),
            owner: Some(record.owner.to_string()),
            title: record.title.clone(),
            description: record.description.clone(),
            icon: record.icon.clone(),
            recovery_set: record.recovery_set.is_some(),
            recovery_pending: record.pending_recovery.as_ref().map(|p| (p.eta_ms, p.rung)),
            categories: record.categories.clone(),
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
            recovery_set: None,
            pending_recovery: None,
            categories: Vec::new(),
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
        if !matches!(
            key.as_str(),
            "title" | "description" | "icon" | "categories"
        ) {
            self.send_err(
                label,
                ErrCode::Policy,
                None,
                "meta key must be title|description|icon|categories",
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
            "categories" => {
                record.categories = value
                    .split(',')
                    .filter(|c| !c.is_empty())
                    .map(str::to_string)
                    .collect()
            }
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
                recovery_set: false,
                recovery_pending: None,
                categories: Vec::new(),
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

    // ---- namespace recovery ladder (§2.4, invariant 9) ----

    /// Load a namespace or answer NO-SUCH-TARGET.
    async fn ns_or_absent(
        &mut self,
        label: Option<String>,
        name: &weft_proto::NamespaceName,
    ) -> io::Result<Option<weft_store::NamespaceRecord>> {
        match self.ctx.namespaces.namespace(name).await {
            Ok(Some(record)) => Ok(Some(record)),
            Ok(None) => {
                self.no_such_target(label).await?;
                Ok(None)
            }
            Err(e) => {
                self.internal(label, &e).await?;
                Ok(None)
            }
        }
    }

    /// NS TRANSFER (rung 1): hand ownership to `new_owner`, proven by a
    /// signature from the current root key. No delay (§2.4).
    async fn on_ns_transfer(
        &mut self,
        label: Option<String>,
        name: weft_proto::NamespaceName,
        new_owner: Account,
        signature: String,
        _account: Account,
    ) -> io::Result<Flow> {
        let Some(record) = self.ns_or_absent(label.clone(), &name).await? else {
            return Ok(Flow::Continue);
        };
        let (Ok(root_key), Ok(sig)) = (
            weft_crypto::PublicKey::from_b64(&record.root_key),
            weft_crypto::signature_from_b64(&signature),
        ) else {
            return self.forbidden_sig(label).await;
        };
        // Authority is the root *key*, not the account — this is the one
        // place same-network namespaces are cryptographically enforced.
        if !weft_crypto::verify_transfer(&root_key, name.as_str(), new_owner.as_str(), &sig) {
            return self.forbidden_sig(label).await;
        }
        // Succession keeps the root key, changes the owner.
        if let Err(e) = self
            .ctx
            .namespaces
            .rotate_root(
                &name,
                new_owner.as_str(),
                &record.root_key,
                false,
                unix_now() * 1000,
            )
            .await
        {
            return self.internal(label, &e).await;
        }
        debug!(%name, %new_owner, "namespace transferred (rung 1)");
        let updated = self.ctx.namespaces.namespace(&name).await.ok().flatten();
        let event = updated
            .as_ref()
            .map(Self::ns_meta_event)
            .unwrap_or_else(|| Self::ns_meta_event(&record));
        self.send_event(label, event).await?;
        Ok(Flow::Continue)
    }

    /// NS RECOVERY SET: designate the M-of-N quorum. Owner (root) only.
    async fn on_ns_recovery_set(
        &mut self,
        label: Option<String>,
        name: weft_proto::NamespaceName,
        m: u32,
        keys: String,
        account: Account,
    ) -> io::Result<Flow> {
        let Some(record) = self.ns_or_absent(label.clone(), &name).await? else {
            return Ok(Flow::Continue);
        };
        if record.owner != account {
            return self.cap_required(label, "ns-admin").await;
        }
        let key_list: Vec<String> = keys
            .split(',')
            .filter(|k| !k.is_empty())
            .map(str::to_string)
            .collect();
        // Every quorum key must be a real pubkey, and m sane.
        if m == 0
            || m as usize > key_list.len()
            || key_list
                .iter()
                .any(|k| weft_crypto::PublicKey::from_b64(k).is_err())
        {
            self.send_err(
                label,
                ErrCode::Malformed,
                None,
                "bad quorum: m of valid keys required",
            )
            .await?;
            return Ok(Flow::Continue);
        }
        if let Err(e) = self
            .ctx
            .namespaces
            .set_recovery_set(&name, m, &key_list)
            .await
        {
            return self.internal(label, &e).await;
        }
        let updated = self.ctx.namespaces.namespace(&name).await.ok().flatten();
        let event = updated
            .as_ref()
            .map(Self::ns_meta_event)
            .unwrap_or_else(|| {
                let mut r = record.clone();
                r.recovery_set = Some((m, key_list));
                Self::ns_meta_event(&r)
            });
        self.send_event(label, event).await?;
        Ok(Flow::Continue)
    }

    /// NS RECOVER: submit a signed rotation; start the delay window. Rung 2
    /// = quorum-signed (7 d), rung 3 = operator-signed (30 d). No silent
    /// path — a rotation is only ever pending + announced here, or applied
    /// by the scheduler, or vetoed (invariant 9).
    async fn on_ns_recover(
        &mut self,
        label: Option<String>,
        name: weft_proto::NamespaceName,
        rotation: String,
    ) -> io::Result<Flow> {
        let Some(record) = self.ns_or_absent(label.clone(), &name).await? else {
            return Ok(Flow::Continue);
        };
        if record.pending_recovery.is_some() {
            self.send_err(
                label,
                ErrCode::Conflict,
                None,
                "a recovery is already pending",
            )
            .await?;
            return Ok(Flow::Continue);
        }
        let Ok(signed) = weft_crypto::SignedRotation::from_b64(&rotation) else {
            return self.forbidden_sig(label).await;
        };
        // The record must actually be for this namespace.
        if signed.record.namespace != name.as_str() {
            return self.forbidden_sig(label).await;
        }
        // Decide the rung by whose signatures verify.
        let quorum: Vec<weft_crypto::PublicKey> = record
            .recovery_set
            .as_ref()
            .map(|(_, keys)| {
                keys.iter()
                    .filter_map(|k| weft_crypto::PublicKey::from_b64(k).ok())
                    .collect()
            })
            .unwrap_or_default();
        let m = record
            .recovery_set
            .as_ref()
            .map(|(m, _)| *m as usize)
            .unwrap_or(0);
        let rung = if m > 0 && signed.quorum_signers(&quorum) >= m {
            2u8
        } else if signed.signed_by(&self.ctx.identity_public()) {
            3u8
        } else {
            return self.forbidden_sig(label).await;
        };
        let delay_secs = if rung == 2 {
            RECOVERY_DELAY_RUNG2_SECS
        } else {
            RECOVERY_DELAY_RUNG3_SECS
        };
        let eta_ms = unix_now() * 1000 + delay_secs * 1000;
        let pending = weft_store::PendingRecovery {
            new_root_key: signed.record.new_root_key.to_b64(),
            new_owner: signed.record.new_owner.clone(),
            eta_ms,
            rung,
        };
        if let Err(e) = self
            .ctx
            .namespaces
            .set_pending_recovery(&name, pending)
            .await
        {
            return self.internal(label, &e).await;
        }
        debug!(%name, rung, "recovery pending (§2.4)");
        // §2.4 announcement: NS-META with recovery=pending. (Same-network,
        // it's reflected on any NS query; a push to all members needs an
        // ns-member broadcast, a follow-up.)
        let updated = self.ctx.namespaces.namespace(&name).await.ok().flatten();
        if let Some(record) = updated {
            self.send_event(label, Self::ns_meta_event(&record)).await?;
        }
        Ok(Flow::Continue)
    }

    /// NS RECOVERY CANCEL: the current root vetoes a pending recovery — a
    /// live root always wins (§2.4). Root signature only.
    async fn on_ns_recovery_cancel(
        &mut self,
        label: Option<String>,
        name: weft_proto::NamespaceName,
        signature: String,
    ) -> io::Result<Flow> {
        let Some(record) = self.ns_or_absent(label.clone(), &name).await? else {
            return Ok(Flow::Continue);
        };
        let (Ok(root_key), Ok(sig)) = (
            weft_crypto::PublicKey::from_b64(&record.root_key),
            weft_crypto::signature_from_b64(&signature),
        ) else {
            return self.forbidden_sig(label).await;
        };
        if !weft_crypto::verify_cancel(&root_key, name.as_str(), &sig) {
            return self.forbidden_sig(label).await;
        }
        if let Err(e) = self.ctx.namespaces.clear_pending_recovery(&name).await {
            return self.internal(label, &e).await;
        }
        debug!(%name, "recovery cancelled by root veto");
        let updated = self.ctx.namespaces.namespace(&name).await.ok().flatten();
        if let Some(record) = updated {
            self.send_event(label, Self::ns_meta_event(&record)).await?;
        }
        Ok(Flow::Continue)
    }

    /// §2.4 / §11.4: bad signatures on a recovery/transfer are FORBIDDEN.
    async fn forbidden_sig(&mut self, label: Option<String>) -> io::Result<Flow> {
        self.send_err(
            label,
            ErrCode::Forbidden,
            Some("signature"),
            "invalid signature",
        )
        .await?;
        Ok(Flow::Continue)
    }

    // ---- §6.7 moderation & reporting ----

    /// The honest content state of a reported message (§6.7). Reaching this
    /// with a stored root means the content exists: `Verified` (a hold is
    /// placed) unless the channel is `e2ee`, where the server holds only
    /// ciphertext → `reporter-attested`. `unverified` is unreachable on the
    /// same-network path — anything the server can't find is
    /// indistinguishable from nonexistent (invariant 1) and already answered
    /// NO-SUCH-TARGET; the state exists for bridged replicas (M5).
    async fn content_state(&self, scope: &Scope) -> ContentState {
        if let Scope::Channel(channel) = scope {
            if let Ok(Some(record)) = self.ctx.channel_store.channel(channel).await {
                if record.policy == RetentionPolicy::E2ee {
                    return ContentState::ReporterAttested;
                }
            }
        }
        ContentState::Verified
    }

    /// Deliver a filed/resolved report event to a queue's live default
    /// handlers: the namespace owner for `ns:<name>`, every operator for `*`
    /// (§6.7). Delegated `reports` holders fetch via REPORTS LIST — there is
    /// no reverse index from cap to account for a live fan-out (same
    /// pull-not-push limit as the §2.4 recovery announcement).
    async fn notify_queue_handlers(&self, queue: &str, event: Event) {
        if queue == "*" {
            for op in self.ctx.operator_accounts() {
                self.ctx.directory.notify(op, event.clone()).await;
            }
        } else if let Some(name) = queue.strip_prefix("ns:") {
            if let Ok(ns_name) = name.parse() {
                if let Ok(Some(ns)) = self.ctx.namespaces.namespace(&ns_name).await {
                    self.ctx.directory.notify(ns.owner, event).await;
                }
            }
        }
    }

    async fn on_report(
        &mut self,
        label: Option<String>,
        msgid: MsgId,
        category: String,
        scope: ReportScope,
        note: Option<String>,
        account: Account,
    ) -> io::Result<Flow> {
        // §6.7 rate limit — per account, rolling hour.
        let now_ms = unix_now_ms();
        match self
            .ctx
            .reports
            .reports_by_since(&account, now_ms.saturating_sub(REPORT_RATE_WINDOW_MS))
            .await
        {
            Ok(count) if count >= REPORT_RATE_LIMIT => {
                let mut err = ErrEvent::new(ErrCode::Throttled, "report rate limit");
                err.retry_after = Some(REPORT_RATE_WINDOW_MS / 1000);
                return self
                    .send_event(label, Event::Err(err))
                    .await
                    .map(|_| Flow::Continue);
            }
            Ok(_) => {}
            Err(e) => return self.internal(label, &e).await,
        }

        // Resolve the reported message. Anything not found or not visible to
        // the reporter answers NO-SUCH-TARGET (invariant 1: you can only
        // report what you can see).
        let root = match self.ctx.events.find_root(msgid.ulid()).await {
            Ok(Some(root)) => root,
            Ok(None) => return self.no_such_target(label).await,
            Err(e) => return self.internal(label, &e).await,
        };
        match &root.scope {
            Scope::Channel(channel) => {
                if !self.joined.contains_key(channel)
                    || self.view_gated_denied(channel, &account).await
                {
                    return self.no_such_target(label).await;
                }
            }
            Scope::Dm(a, b) => {
                if account != *a && account != *b {
                    return self.no_such_target(label).await;
                }
            }
        }

        // Routing (§6.7): ns → the channel's namespace owner (or the operator
        // for a top-level channel / DM); net → the operator. `csam`/`illegal`
        // always ALSO reach the operator, who is the legally accountable party.
        let ns = match &root.scope {
            Scope::Channel(c) => channel_namespace(c),
            Scope::Dm(..) => None,
        };
        let mut queue_scopes: Vec<String> = Vec::new();
        match scope {
            ReportScope::Net => queue_scopes.push("*".into()),
            ReportScope::Ns => match &ns {
                Some(name) => queue_scopes.push(format!("ns:{name}")),
                None => queue_scopes.push("*".into()),
            },
        }
        if matches!(category.as_str(), "csam" | "illegal") && !queue_scopes.iter().any(|q| q == "*")
        {
            queue_scopes.push("*".into());
        }

        let state = self.content_state(&root.scope).await;
        let report_id = Ulid::new().to_string();
        let record = ReportRecord {
            id: report_id.clone(),
            msgid: msgid.clone(),
            scope: root.scope.clone(),
            category: category.clone(),
            state,
            reporter: account.clone(),
            note,
            queue_scopes: queue_scopes.clone(),
            status: ReportStatus::Open,
            filed_at_ms: now_ms,
            held_roots: vec![],
            resolution: None,
            holds_released: false,
        };
        if let Err(e) = self.ctx.reports.file_report(record).await {
            return self.internal(label, &e).await;
        }

        // Live push to each queue's default handlers (§6.7). The reporter's
        // identity travels to handlers (accountability), never to the
        // reported party (invariant 12: they receive nothing).
        for queue in &queue_scopes {
            let filed = Event::ReportFiled {
                report_id: report_id.clone(),
                msgid: msgid.clone(),
                category: category.clone(),
                state,
                scope: if queue == "*" {
                    ReportScope::Net
                } else {
                    ReportScope::Ns
                },
                reporter: Some(account.to_string()),
            };
            self.notify_queue_handlers(queue, filed).await;
        }

        self.send_event(label, Event::Reported { report_id })
            .await?;
        Ok(Flow::Continue)
    }

    async fn on_reports_list(
        &mut self,
        label: Option<String>,
        scope: String,
        status: Option<ReportStatus>,
        cursor: Option<String>,
        account: Account,
    ) -> io::Result<Flow> {
        const PAGE: usize = 50;
        let Some(token_scope) = TokenScope::parse(&scope) else {
            return self.bad_scope(label).await;
        };
        match self
            .ctx
            .account_has_cap(&account, &Capability::Reports, &token_scope, unix_now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "reports").await,
            Err(e) => return self.internal(label, &e).await,
        }
        let page = match self
            .ctx
            .reports
            .list_reports(&scope, status, cursor.as_deref(), PAGE)
            .await
        {
            Ok(page) => page,
            Err(e) => return self.internal(label, &e).await,
        };
        let next_cursor = (page.len() == PAGE)
            .then(|| page.last().map(|r| r.id.clone()))
            .flatten();
        let is_net = scope == "*";
        for report in &page {
            self.send_event(
                label.clone(),
                Event::ReportFiled {
                    report_id: report.id.clone(),
                    msgid: report.msgid.clone(),
                    category: report.category.clone(),
                    state: report.state,
                    scope: if is_net {
                        ReportScope::Net
                    } else {
                        ReportScope::Ns
                    },
                    reporter: Some(report.reporter.to_string()),
                },
            )
            .await?;
        }
        if let Some(cursor) = next_cursor {
            self.send_event(label, Event::More { cursor }).await?;
        }
        Ok(Flow::Continue)
    }

    async fn on_reports_resolve(
        &mut self,
        label: Option<String>,
        report_id: String,
        action: ResolveAction,
        note: Option<String>,
        account: Account,
    ) -> io::Result<Flow> {
        let report = match self.ctx.reports.report(&report_id).await {
            Ok(Some(report)) => report,
            Ok(None) => return self.no_such_target(label).await,
            Err(e) => return self.internal(label, &e).await,
        };
        // Invariant 4: authority before any state change. The resolver must
        // hold `reports` at one of the report's queue scopes.
        let now = unix_now();
        let mut authorized = false;
        for queue in &report.queue_scopes {
            let Some(qscope) = TokenScope::parse(queue) else {
                continue;
            };
            match self
                .ctx
                .account_has_cap(&account, &Capability::Reports, &qscope, now)
                .await
            {
                Ok(true) => {
                    authorized = true;
                    break;
                }
                Ok(false) => {}
                Err(e) => return self.internal(label, &e).await,
            }
        }
        if !authorized {
            return self.cap_required(label, "reports").await;
        }

        // ESCALATE re-routes an ns report up to net, leaving it open and its
        // holds intact (§6.7); it is not a resolution.
        if action == ResolveAction::Escalated {
            match self.ctx.reports.escalate_report(&report_id).await {
                Ok(true) => {}
                Ok(false) => return self.no_such_target(label).await,
                Err(e) => return self.internal(label, &e).await,
            }
            self.notify_queue_handlers(
                "*",
                Event::ReportFiled {
                    report_id: report.id.clone(),
                    msgid: report.msgid.clone(),
                    category: report.category.clone(),
                    state: report.state,
                    scope: ReportScope::Net,
                    reporter: Some(report.reporter.to_string()),
                },
            )
            .await;
            return self
                .send_event(
                    label,
                    Event::ReportResolved {
                        report_id,
                        action,
                        by: Some(account.to_string()),
                        note,
                    },
                )
                .await
                .map(|_| Flow::Continue);
        }

        let now_ms = unix_now_ms();
        let resolution = ReportResolution {
            action,
            note: note.clone(),
            resolved_by: account.clone(),
            at_ms: now_ms,
            hold_release_at: now_ms + REPORT_HOLD_GRACE_MS,
        };
        match self
            .ctx
            .reports
            .resolve_report(&report_id, resolution)
            .await
        {
            Ok(true) => {}
            // Already resolved / gone — indistinct (anti-enumeration).
            Ok(false) => return self.no_such_target(label).await,
            Err(e) => return self.internal(label, &e).await,
        }

        // The reporter gets the MINIMAL form — no handler identity, no note
        // (§6.7 confidentiality; invariant 12 protects the reported party,
        // this clause protects the handler toward the reporter).
        self.ctx
            .directory
            .notify(
                report.reporter.clone(),
                Event::ReportResolved {
                    report_id: report_id.clone(),
                    action,
                    by: None,
                    note: None,
                },
            )
            .await;
        // The resolver's echo carries the full form.
        self.send_event(
            label,
            Event::ReportResolved {
                report_id,
                action,
                by: Some(account.to_string()),
                note,
            },
        )
        .await?;
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
        // The layout fetch also carries the namespace meta (categories, title,
        // …) so the client renders category groups purely from server state.
        self.send_event(label.clone(), Self::ns_meta_event(&record))
            .await?;
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

    // ---- §11 federation: bridge sessions ----

    /// §11.2 AUTH BRIDGE: resolve the key the peer must prove control of —
    /// the pinned key (which the asserted key must match), or, in accept-any
    /// mode, the asserted key itself. A blocked network, an unknown network in
    /// pinned-only mode, or a pin mismatch all funnel to the uniform
    /// AUTH-FAILED (no peer-existence oracle, invariant 1 discipline).
    async fn on_auth_bridge(
        &mut self,
        label: Option<String>,
        network: NetworkName,
        token: String,
    ) -> io::Result<Flow> {
        let asserted = PublicKey::from_b64(&token).ok();
        let blocked = self
            .ctx
            .netblocks
            .is_netblocked(&network)
            .await
            .unwrap_or(false);
        let device = if blocked {
            None
        } else if let Some(pinned) = self.ctx.peer_key(&network).copied() {
            (asserted == Some(pinned)).then_some(pinned)
        } else if self.ctx.bridge_accept_any() {
            asserted
        } else {
            None
        };
        let Some(device) = device else {
            return self.auth_failed(label).await;
        };
        let nonce: [u8; weft_crypto::CHALLENGE_NONCE_LEN] = rand::random();
        self.send_event(
            label,
            Event::Challenge {
                nonce: weft_crypto::b64::encode(nonce),
            },
        )
        .await?;
        self.state = State::Unauthed {
            challenge: Some(PendingChallenge {
                device,
                nonce,
                subject: ChallengeSubject::Bridge { peer: network },
            }),
        };
        Ok(Flow::Continue)
    }

    /// Bridge PROOF verified: enter the bridge state and resume forwarding any
    /// previously-acked channels.
    async fn welcome_bridge(
        &mut self,
        label: Option<String>,
        peer: NetworkName,
        key: PublicKey,
    ) -> io::Result<Flow> {
        self.send_event(
            label,
            Event::Welcome {
                network: self.ctx.info.network.clone(),
                features: vec!["bridge".to_string()],
                attestation: None,
                motd: None,
            },
        )
        .await?;
        self.state = State::Bridge {
            peer: peer.clone(),
            key,
        };
        if let Ok(Some(record)) = self.ctx.peers.peer(&peer).await {
            self.sync_bridge_forwarders(&record).await;
        }
        Ok(Flow::Continue)
    }

    /// Route a line arriving on a bridge session: peer *events* ingest; peer
    /// *commands* drive the manifest state machine (§11.1) + backfill (§11.7).
    async fn on_bridge_line(
        &mut self,
        peer: NetworkName,
        key: PublicKey,
        line: &Line,
    ) -> io::Result<Flow> {
        match line.verb.as_str() {
            "MESSAGE" | "EDITED" | "DELETED" | "REACTION" => self.on_ingest(&peer, line).await,
            // Remote membership / typing / presence / marks are informational;
            // not stored, not re-broadcast in M5b.
            "MEMBER" | "TYPING" | "PRESENCE" | "MARKED" | "POLICY" => Ok(Flow::Continue),
            _ => match Request::from_line(line) {
                Ok(req) => self.on_bridge_cmd(peer, key, req.label, req.command).await,
                Err(_) => Ok(Flow::Continue), // tolerate noise on a bridge
            },
        }
    }

    async fn on_bridge_cmd(
        &mut self,
        peer: NetworkName,
        key: PublicKey,
        label: Option<String>,
        cmd: Command,
    ) -> io::Result<Flow> {
        match cmd {
            Command::BridgePropose {
                scope,
                history,
                media,
                typing,
                manifest,
                ..
            } => {
                self.on_bridge_propose_in(peer, key, scope, history, media, typing, manifest)
                    .await
            }
            Command::BridgeAccept { version, .. } => self.on_bridge_accept_in(peer, version).await,
            Command::BridgeSever { .. } => self.on_bridge_sever_in(peer).await,
            // §11.7 federated backfill: the peer pulls history over the bridge.
            Command::History {
                target,
                before,
                after,
                limit,
                ..
            } => {
                self.on_bridge_backfill(peer, label, target, before, after, limit)
                    .await
            }
            // §11.9 a forwarded report from the reporter's home network.
            Command::ReportForward {
                report_id,
                msgid,
                category,
                note,
            } => {
                self.on_report_forward_in(peer, report_id, msgid, category, note)
                    .await
            }
            Command::Ping { token } => {
                self.send_event(label, Event::Pong { token }).await?;
                Ok(Flow::Continue)
            }
            Command::Quit { .. } => Ok(Flow::Close),
            _ => Ok(Flow::Continue),
        }
    }

    /// A peer sent us a signed manifest (§11.1). Verify it against the peer's
    /// pinned key, store it, and (auto-accept path) ack + start forwarding.
    #[allow(clippy::too_many_arguments)]
    async fn on_bridge_propose_in(
        &mut self,
        peer: NetworkName,
        key: PublicKey,
        scope: String,
        _history: HistoryMode,
        _media: MediaMode,
        _typing: bool,
        manifest: Option<String>,
    ) -> io::Result<Flow> {
        let Some(blob) = manifest else {
            return Ok(Flow::Continue);
        };
        let Ok(signed) = SignedManifest::from_b64(&blob) else {
            return Ok(Flow::Continue);
        };
        // Verify against the key this session authenticated with (pinned or
        // accept-any) — not a fresh config lookup, so open federation works.
        if !bridge::verify_incoming(&signed, &key, self.ctx.network()) {
            debug!(%peer, "rejected bridge proposal: bad manifest signature/peer");
            return Ok(Flow::Continue);
        }
        let now = unix_now_ms();
        let version = signed.manifest.version;
        let auto = self.ctx.bridge_auto_accept();
        let record = PeerRecord {
            peer: peer.clone(),
            scope,
            manifest: blob.clone(),
            version,
            acked_manifest: auto.then(|| blob.clone()),
            severed: false,
            created_ms: now,
            updated_ms: now,
        };
        if let Err(e) = self.ctx.peers.upsert_peer(record.clone()).await {
            return self.internal(None, &e).await;
        }
        if auto {
            let ack = Request::new(Command::BridgeAccept {
                peer: self.ctx.network().clone(),
                version,
            });
            if let Ok(line) = ack.serialize() {
                self.stream.send_line(&line).await?;
            }
            self.sync_bridge_forwarders(&record).await;
            self.announce_manifest(&record, BridgeState::Live).await;
        }
        Ok(Flow::Continue)
    }

    /// The peer acked our manifest at `version` → live. Mark it and forward.
    async fn on_bridge_accept_in(&mut self, peer: NetworkName, version: u64) -> io::Result<Flow> {
        let Ok(Some(mut record)) = self.ctx.peers.peer(&peer).await else {
            return Ok(Flow::Continue);
        };
        if record.version != version {
            debug!(%peer, record.version, version, "bridge ack version mismatch");
            return Ok(Flow::Continue);
        }
        record.acked_manifest = Some(record.manifest.clone());
        record.updated_ms = unix_now_ms();
        if let Err(e) = self.ctx.peers.upsert_peer(record.clone()).await {
            return self.internal(None, &e).await;
        }
        self.sync_bridge_forwarders(&record).await;
        self.announce_manifest(&record, BridgeState::Live).await;
        Ok(Flow::Continue)
    }

    /// The peer tore the bridge down (§11.6/§6.6). Stop forwarding.
    async fn on_bridge_sever_in(&mut self, peer: NetworkName) -> io::Result<Flow> {
        if let Ok(Some(mut record)) = self.ctx.peers.peer(&peer).await {
            record.severed = true;
            record.updated_ms = unix_now_ms();
            let _ = self.ctx.peers.upsert_peer(record.clone()).await;
            self.announce_manifest(&record, BridgeState::Severed).await;
        }
        for (_, forwarder) in self.bridged.drain() {
            forwarder.abort();
        }
        Ok(Flow::Continue)
    }

    /// §11.7 federated backfill: serve a bridged channel's history to the peer
    /// over the bridge session. Gated on the acked manifest (invariant 3) and
    /// the manifest `history` flag (`from-epoch` = nothing before the
    /// manifest's `created` ULID timestamp); origin retention is enforced by
    /// the store (purged rows never return, `truncated` is set honestly).
    async fn on_bridge_backfill(
        &mut self,
        peer: NetworkName,
        label: Option<String>,
        target: Target,
        before: Option<MsgId>,
        after: Option<MsgId>,
        limit: Option<u32>,
    ) -> io::Result<Flow> {
        let Target::Channel(channel) = target.clone() else {
            return Ok(Flow::Continue); // DMs never bridge (§9.5)
        };
        let Some(record) = self.ctx.peers.peer(&peer).await.ok().flatten() else {
            return Ok(Flow::Continue);
        };
        if !bridge::is_forwardable(&record, channel.as_str()) {
            debug!(%peer, %channel, "backfill refused: channel not in acked manifest");
            return self
                .emit_batch(label, &target, Vec::new(), false)
                .await
                .map(|_| Flow::Continue);
        }
        // `from-epoch` lower bound = the manifest's `created` timestamp.
        let (history, created) = SignedManifest::from_b64(&record.manifest)
            .map(|s| (s.manifest.history, s.manifest.created))
            .unwrap_or_default();
        let manifest_floor = if history == "full" { 0 } else { created };
        let after_floor = after.as_ref().map(|m| m.timestamp_ms()).unwrap_or(0);
        // Respect an explicit `after` exclusivity when it's already past the
        // manifest floor; otherwise clamp up to the floor.
        let after_ulid = if after_floor >= manifest_floor {
            after.as_ref().map(|m| m.ulid())
        } else if manifest_floor > 0 {
            Some(Ulid::from_parts(manifest_floor, 0))
        } else {
            None
        };
        let scope = Scope::Channel(channel);
        let policy = self
            .ctx
            .channel_store
            .channel(match &scope {
                Scope::Channel(c) => c,
                _ => unreachable!(),
            })
            .await
            .ok()
            .flatten()
            .map(|c| c.policy)
            .unwrap_or(RetentionPolicy::Permanent);
        let limit = limit.unwrap_or(100).clamp(1, weft_proto::MAX_HISTORY_LIMIT) as usize;

        let (items, truncated) = if policy == RetentionPolicy::Ephemeral {
            (Vec::new(), true)
        } else {
            let page = weft_store::Page {
                before: before.as_ref().map(|m| m.ulid()),
                after: after_ulid,
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
            let watermark = self.ctx.events.purged_before(&scope).await.ok().flatten();
            let items = weft_store::materialize(roots, children);
            let floor_ms = manifest_floor.max(after_floor);
            let truncated = items.len() < limit && watermark.is_some_and(|w| floor_ms < w);
            (items, truncated)
        };
        self.emit_batch(label, &target, items, truncated).await?;
        Ok(Flow::Continue)
    }

    /// §11.9 a forwarded report arriving over the bridge from a reporter's home
    /// network. We're the origin of the reported msgid; treat it as a
    /// net-scope, `unverified` signal with the reporter stripped, and drop it
    /// into the operator queue. Report queues/holds never replicate — a fresh
    /// local id, no hold (unverified places none).
    async fn on_report_forward_in(
        &mut self,
        _peer: NetworkName,
        _report_id: String,
        msgid: MsgId,
        category: String,
        note: Option<String>,
    ) -> io::Result<Flow> {
        if msgid.origin().as_str() != self.ctx.network_name() {
            return Ok(Flow::Continue); // not ours to act on
        }
        let Ok(Some(root)) = self.ctx.events.find_root(msgid.ulid()).await else {
            return Ok(Flow::Continue); // content gone — nothing to file against
        };
        let report_id = Ulid::new().to_string();
        let record = ReportRecord {
            id: report_id.clone(),
            msgid: msgid.clone(),
            scope: root.scope.clone(),
            category: category.clone(),
            state: ContentState::Unverified, // §11.9 unverified-at-minimum
            reporter: forwarded_reporter(),
            note,
            queue_scopes: vec!["*".to_string()], // net scope → operator
            status: ReportStatus::Open,
            filed_at_ms: unix_now_ms(),
            held_roots: vec![],
            resolution: None,
            holds_released: false,
        };
        if let Err(e) = self.ctx.reports.file_report(record).await {
            error!("forwarded report not filed: {e}");
            return Ok(Flow::Continue);
        }
        // Notify operators — reporter stripped (§11.9, invariant 12).
        self.notify_queue_handlers(
            "*",
            Event::ReportFiled {
                report_id,
                msgid,
                category,
                state: ContentState::Unverified,
                scope: ReportScope::Net,
                reporter: None,
            },
        )
        .await;
        Ok(Flow::Continue)
    }

    /// Ingest a bridged event (§11.4): the origin must be the authenticated
    /// peer (invariant 2), and the channel must be in the acked manifest
    /// (invariant 3). Persisted with its origin msgid intact.
    async fn on_ingest(&mut self, peer: &NetworkName, line: &Line) -> io::Result<Flow> {
        // §11.6 effect 3: a blocked network's events are rejected at ingestion
        // (a mid-session block takes effect at once, not just at auth).
        if self
            .ctx
            .netblocks
            .is_netblocked(peer)
            .await
            .unwrap_or(false)
        {
            return Ok(Flow::Continue);
        }
        let Ok(reply) = Reply::from_line(line) else {
            return Ok(Flow::Continue);
        };
        let Some((channel, record)) = self.ingest_record(peer, &reply.event) else {
            return Ok(Flow::Continue);
        };
        let gated = self
            .ctx
            .peers
            .peer(peer)
            .await
            .ok()
            .flatten()
            .map(|p| bridge::is_forwardable(&p, channel.as_str()))
            .unwrap_or(false);
        if !gated {
            debug!(%peer, %channel, "dropped ingest: channel not in acked manifest");
            return Ok(Flow::Continue);
        }
        if let Some(handle) = self.ctx.registry.get(&channel) {
            handle.ingest(self.id, record, reply.event).await;
        }
        Ok(Flow::Continue)
    }

    /// Map a bridged event to its storage record, enforcing origin authority
    /// (invariant 2): the event and its root must originate on `peer`.
    fn ingest_record(
        &self,
        peer: &NetworkName,
        event: &Event,
    ) -> Option<(ChannelName, EventRecord)> {
        let channel_of = |t: &Target| match t {
            Target::Channel(c) => Some(c.clone()),
            _ => None, // DMs never bridge (§9.5)
        };
        let from_peer = |id: &MsgId| id.origin().as_str() == peer.as_str();
        match event {
            Event::Message(m) => {
                let channel = channel_of(&m.target)?;
                if !from_peer(&m.msgid) || m.sender.network.as_str() != peer.as_str() {
                    return None;
                }
                let record = EventRecord {
                    scope: Scope::Channel(channel.clone()),
                    msgid: m.msgid.clone(),
                    root: m.msgid.clone(),
                    sender: m.sender.clone(),
                    kind: EventKind::Message {
                        body: m.body.clone(),
                        meta: m.meta.clone(),
                    },
                };
                Some((channel, record))
            }
            Event::Edited {
                target,
                user,
                msgid,
                edit_of,
                body,
            } => {
                let channel = channel_of(target)?;
                // The edit and the message it edits both belong to the origin.
                if !from_peer(msgid) || !from_peer(edit_of) {
                    return None;
                }
                let record = EventRecord {
                    scope: Scope::Channel(channel.clone()),
                    msgid: msgid.clone(),
                    root: edit_of.clone(),
                    sender: user.clone(),
                    kind: EventKind::Edit { body: body.clone() },
                };
                Some((channel, record))
            }
            Event::Deleted { target, msgid, by } => {
                let channel = channel_of(target)?;
                if !from_peer(msgid) {
                    return None;
                }
                let sender = by
                    .clone()
                    .unwrap_or_else(|| UserRef::new(deleted_placeholder(), peer.clone()));
                let record = EventRecord {
                    // A replica delete row needs its own id; the tombstone is
                    // keyed on the root (`msgid`), which is what materialize
                    // uses — this id is local bookkeeping only.
                    scope: Scope::Channel(channel.clone()),
                    msgid: MsgId::new(peer.clone(), Ulid::new()),
                    root: msgid.clone(),
                    sender,
                    kind: EventKind::Delete,
                };
                Some((channel, record))
            }
            Event::Reaction {
                target,
                msgid,
                emoji,
                op,
                by,
            } => {
                let channel = channel_of(target)?;
                if !from_peer(msgid) {
                    return None;
                }
                let record = EventRecord {
                    scope: Scope::Channel(channel.clone()),
                    msgid: MsgId::new(peer.clone(), Ulid::new()),
                    root: msgid.clone(),
                    sender: by.clone(),
                    kind: EventKind::React {
                        emoji: emoji.clone(),
                        add: matches!(op, weft_proto::ReactionOp::Add),
                    },
                };
                Some((channel, record))
            }
            _ => None,
        }
    }

    /// Subscribe the bridge session to exactly the forwardable channels
    /// (invariant 3); tear down forwarders for channels no longer bridged.
    async fn sync_bridge_forwarders(&mut self, record: &PeerRecord) {
        let want: Vec<ChannelName> = bridge::forwardable_channels(record)
            .iter()
            .filter_map(|c| c.parse().ok())
            .collect();
        let stale: Vec<ChannelName> = self
            .bridged
            .keys()
            .filter(|c| !want.contains(c))
            .cloned()
            .collect();
        for channel in stale {
            if let Some(forwarder) = self.bridged.remove(&channel) {
                forwarder.abort();
            }
        }
        for channel in want {
            if self.bridged.contains_key(&channel) {
                continue;
            }
            if let Some(handle) = self.ctx.registry.get(&channel) {
                if let Some(rx) = handle.subscribe().await {
                    let forwarder = spawn_forwarder(channel.clone(), rx, self.events_tx.clone());
                    self.bridged.insert(channel, forwarder);
                }
            }
        }
    }

    /// §6.6 MANIFEST-to-members: broadcast the change into each affected
    /// channel so local members learn of the audience change (mandatory).
    async fn announce_manifest(&self, record: &PeerRecord, state: BridgeState) {
        let channels = bridge::forwardable_channels(record);
        for channel in &channels {
            if let Ok(chan) = channel.parse::<ChannelName>() {
                if let Some(handle) = self.ctx.registry.get(&chan) {
                    handle
                        .announce(manifest_event(record, state, &channels))
                        .await;
                }
            }
        }
    }

    /// A bridge session's channel events: forward only *local-origin*
    /// message-plane events to the peer (one hop, §11.4 — received events are
    /// never re-forwarded because their origin != our network).
    async fn on_bridge_event(&mut self, _peer: NetworkName, event: SessionEvent) -> io::Result<()> {
        let SessionEvent::Channel { event, .. } = event else {
            return Ok(()); // Lagged: a real bridge would resync (M5c)
        };
        let ours = |id: &MsgId| id.origin().as_str() == self.ctx.network_name();
        let forward = match &event.event {
            Event::Message(m) => ours(&m.msgid),
            Event::Edited { msgid, .. } => ours(msgid),
            Event::Deleted { msgid, .. } => ours(msgid),
            Event::Reaction { msgid, .. } => ours(msgid),
            _ => false, // MEMBER/TYPING/POLICY/MANIFEST not forwarded in M5b
        };
        if forward {
            if let Ok(line) = Reply::new(event.event).serialize() {
                self.stream.send_line(&line).await?;
            }
        }
        Ok(())
    }

    // ---- §11 federation: operator-facing management (§6.6) ----

    /// §6.6/§11.3 BRIDGE PROPOSE from an operator: check the scope authority,
    /// compile + sign a v1 manifest, and store it. Transmission to the peer
    /// over the bridge session is the dialer's job (M5d).
    #[allow(clippy::too_many_arguments)]
    async fn on_bridge_propose(
        &mut self,
        label: Option<String>,
        scope: String,
        peer: NetworkName,
        history: HistoryMode,
        media: MediaMode,
        typing: bool,
        account: Account,
    ) -> io::Result<Flow> {
        if self
            .ctx
            .netblocks
            .is_netblocked(&peer)
            .await
            .unwrap_or(false)
        {
            self.send_err(label, ErrCode::Blocked, None, "peer network is blocked")
                .await?;
            return Ok(Flow::Continue);
        }
        let Some(tscope) = TokenScope::parse(&scope) else {
            return self.no_such_target(label).await;
        };
        // §11.3 ladder: `bridge` cap at the scope (operators/ns-owners implied).
        match self
            .ctx
            .account_has_cap(&account, &Capability::Bridge, &tscope, unix_now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "bridge").await,
            Err(e) => return self.internal(label, &e).await,
        }
        let channels = self.scope_channels(&tscope).await;
        let now = unix_now_ms();
        let manifest =
            bridge::build_manifest(&peer, 1, &channels, history, media, typing, now, now);
        let record = PeerRecord {
            peer: peer.clone(),
            scope,
            manifest: self.ctx.sign_manifest(&manifest),
            version: 1,
            acked_manifest: None,
            severed: false,
            created_ms: now,
            updated_ms: now,
        };
        if let Err(e) = self.ctx.peers.upsert_peer(record.clone()).await {
            return self.internal(label, &e).await;
        }
        let channel_strs = bridge::forwardable_channels(&record);
        self.send_event(
            label,
            manifest_event(&record, BridgeState::Added, &channel_strs),
        )
        .await?;
        Ok(Flow::Continue)
    }

    /// §6.6 BRIDGE ACCEPT from an operator: mark a stored proposal live.
    async fn on_bridge_accept_op(
        &mut self,
        label: Option<String>,
        peer: NetworkName,
        version: u64,
        account: Account,
    ) -> io::Result<Flow> {
        let Some(mut record) = self.ctx.peers.peer(&peer).await.ok().flatten() else {
            return self.no_such_target(label).await;
        };
        let tscope = TokenScope::parse(&record.scope).unwrap_or(TokenScope::Wildcard);
        match self
            .ctx
            .account_has_cap(&account, &Capability::Bridge, &tscope, unix_now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "bridge").await,
            Err(e) => return self.internal(label, &e).await,
        }
        if record.version != version {
            self.send_err(label, ErrCode::Conflict, None, "manifest version race")
                .await?;
            return Ok(Flow::Continue);
        }
        record.acked_manifest = Some(record.manifest.clone());
        record.updated_ms = unix_now_ms();
        if let Err(e) = self.ctx.peers.upsert_peer(record.clone()).await {
            return self.internal(label, &e).await;
        }
        let channel_strs = bridge::forwardable_channels(&record);
        self.send_event(
            label,
            manifest_event(&record, BridgeState::Live, &channel_strs),
        )
        .await?;
        Ok(Flow::Continue)
    }

    /// §6.6 BRIDGE SEVER from an operator: unilateral teardown.
    async fn on_bridge_sever_op(
        &mut self,
        label: Option<String>,
        peer: NetworkName,
        account: Account,
    ) -> io::Result<Flow> {
        let Some(mut record) = self.ctx.peers.peer(&peer).await.ok().flatten() else {
            return self.no_such_target(label).await;
        };
        let tscope = TokenScope::parse(&record.scope).unwrap_or(TokenScope::Wildcard);
        match self
            .ctx
            .account_has_cap(&account, &Capability::Bridge, &tscope, unix_now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "bridge").await,
            Err(e) => return self.internal(label, &e).await,
        }
        record.severed = true;
        record.updated_ms = unix_now_ms();
        if let Err(e) = self.ctx.peers.upsert_peer(record.clone()).await {
            return self.internal(label, &e).await;
        }
        self.send_event(label, manifest_event(&record, BridgeState::Severed, &[]))
            .await?;
        Ok(Flow::Continue)
    }

    /// Channels covered by a bridge scope, snapshotted at propose time (§11.1).
    async fn scope_channels(&self, scope: &TokenScope) -> Vec<ChannelName> {
        match scope {
            TokenScope::Channel(c) => c.parse().ok().into_iter().collect(),
            TokenScope::Namespace(n) => self
                .ctx
                .channel_store
                .channels_in_namespace(n)
                .await
                .map(|v| v.into_iter().map(|(name, _)| name).collect())
                .unwrap_or_default(),
            TokenScope::Wildcard => self
                .ctx
                .channel_store
                .list_channels()
                .await
                .map(|v| v.into_iter().map(|(name, _)| name).collect())
                .unwrap_or_default(),
        }
    }

    // ---- §11.6 NETBLOCK ----

    async fn on_netblock_add(
        &mut self,
        label: Option<String>,
        network: NetworkName,
        reason: Option<String>,
        account: Account,
    ) -> io::Result<Flow> {
        // `netblock` cap is `*`-scope only (§10.4).
        match self
            .ctx
            .account_has_cap(
                &account,
                &Capability::Netblock,
                &TokenScope::Wildcard,
                unix_now(),
            )
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "netblock").await,
            Err(e) => return self.internal(label, &e).await,
        }
        let record = NetblockRecord {
            network: network.clone(),
            reason,
            added_ms: unix_now_ms(),
            actor: account.to_string(),
        };
        if let Err(e) = self.ctx.netblocks.add_netblock(record).await {
            return self.internal(label, &e).await;
        }
        // Effect 2 (§11.6): sever any existing manifest with this network.
        if let Ok(Some(mut peer)) = self.ctx.peers.peer(&network).await {
            peer.severed = true;
            peer.updated_ms = unix_now_ms();
            let _ = self.ctx.peers.upsert_peer(peer).await;
        }
        self.send_event(
            label,
            Event::Netblocked {
                network,
                reason: None,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    async fn on_netblock_remove(
        &mut self,
        label: Option<String>,
        network: NetworkName,
        account: Account,
    ) -> io::Result<Flow> {
        match self
            .ctx
            .account_has_cap(
                &account,
                &Capability::Netblock,
                &TokenScope::Wildcard,
                unix_now(),
            )
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "netblock").await,
            Err(e) => return self.internal(label, &e).await,
        }
        match self.ctx.netblocks.remove_netblock(&network).await {
            Ok(true) => {
                self.send_event(
                    label,
                    Event::Netblocked {
                        network,
                        reason: None,
                    },
                )
                .await?
            }
            Ok(false) => return self.no_such_target(label).await,
            Err(e) => return self.internal(label, &e).await,
        }
        Ok(Flow::Continue)
    }

    async fn on_netblock_list(
        &mut self,
        label: Option<String>,
        account: Account,
    ) -> io::Result<Flow> {
        match self
            .ctx
            .account_has_cap(
                &account,
                &Capability::Netblock,
                &TokenScope::Wildcard,
                unix_now(),
            )
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "netblock").await,
            Err(e) => return self.internal(label, &e).await,
        }
        let blocks = self
            .ctx
            .netblocks
            .list_netblocks()
            .await
            .unwrap_or_default();
        for (i, block) in blocks.iter().enumerate() {
            // Echo the request label on the first line only (§3.5).
            let lbl = (i == 0).then(|| label.clone()).flatten();
            self.send_event(
                lbl,
                Event::Netblocked {
                    network: block.network.clone(),
                    reason: block.reason.clone(),
                },
            )
            .await?;
        }
        Ok(Flow::Continue)
    }

    // ---- §6.7 moderation ----

    /// The §6.7 posting gate: `None` = may post; `Some((code, context))` = the
    /// refusal. Checks ban → mute → (restricted ⇒ `send` cap), against the
    /// channel's covering scopes.
    async fn can_post(
        &self,
        channel: &ChannelName,
        account: &Account,
    ) -> Result<Option<(ErrCode, &'static str)>, weft_store::StoreError> {
        let scopes = covering_scopes(channel);
        if self
            .ctx
            .moderation
            .is_moderated(account, &scopes, ModKind::Ban)
            .await?
        {
            return Ok(Some((ErrCode::Forbidden, "banned")));
        }
        if self
            .ctx
            .moderation
            .is_moderated(account, &scopes, ModKind::Mute)
            .await?
        {
            return Ok(Some((ErrCode::Forbidden, "muted")));
        }
        let restricted = self
            .ctx
            .channel_store
            .channel(channel)
            .await?
            .map(|c| c.restricted)
            .unwrap_or(false);
        if restricted {
            let scope = TokenScope::Channel(channel.to_string());
            if !self
                .ctx
                .account_has_cap(account, &Capability::Send, &scope, unix_now())
                .await?
            {
                return Ok(Some((ErrCode::CapRequired, "send")));
            }
        }
        Ok(None)
    }

    /// `MUTE`/`UNMUTE`/`BAN`/`UNBAN` (§6.7): cap-check the moderator, record or
    /// clear the deny, eject on a fresh channel-scope ban, and echo `MODERATED`.
    #[allow(clippy::too_many_arguments)]
    async fn on_moderate(
        &mut self,
        label: Option<String>,
        scope: String,
        target: Account,
        kind: ModKind,
        add: bool,
        reason: Option<String>,
        actor: Account,
    ) -> io::Result<Flow> {
        let Some(tscope) = TokenScope::parse(&scope) else {
            return self.no_such_target(label).await;
        };
        let (cap, cap_str) = match kind {
            ModKind::Mute => (Capability::Mute, "mute"),
            ModKind::Ban => (Capability::Ban, "ban"),
        };
        match self
            .ctx
            .account_has_cap(&actor, &cap, &tscope, unix_now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, cap_str).await,
            Err(e) => return self.internal(label, &e).await,
        }
        let outcome = if add {
            self.ctx
                .moderation
                .set_moderation(ModRecord {
                    scope: scope.clone(),
                    account: target.clone(),
                    kind,
                    actor: actor.to_string(),
                    reason: reason.clone(),
                    at_ms: unix_now_ms(),
                })
                .await
        } else {
            self.ctx
                .moderation
                .clear_moderation(&scope, &target, kind)
                .await
                .map(|_| ())
        };
        if let Err(e) = outcome {
            return self.internal(label, &e).await;
        }
        // A fresh channel-scope ban force-parts the target.
        if add && kind == ModKind::Ban {
            if let Ok(channel) = scope.parse::<ChannelName>() {
                if let Some(handle) = self.ctx.registry.get(&channel) {
                    handle.eject(target.clone()).await;
                }
            }
        }
        let action = match (kind, add) {
            (ModKind::Mute, true) => ModAction::Mute,
            (ModKind::Mute, false) => ModAction::Unmute,
            (ModKind::Ban, true) => ModAction::Ban,
            (ModKind::Ban, false) => ModAction::Unban,
        };
        self.send_event(
            label,
            Event::Moderated {
                scope,
                account: target,
                action,
                by: Some(actor),
                reason,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    /// `KICK` (§6.7): cap-check `kick`, force-part the target (no persistent
    /// state — they may rejoin), echo `MODERATED`.
    async fn on_kick(
        &mut self,
        label: Option<String>,
        channel: ChannelName,
        target: Account,
        reason: Option<String>,
        actor: Account,
    ) -> io::Result<Flow> {
        let scope = TokenScope::Channel(channel.to_string());
        match self
            .ctx
            .account_has_cap(&actor, &Capability::Kick, &scope, unix_now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "kick").await,
            Err(e) => return self.internal(label, &e).await,
        }
        let Some(handle) = self.ctx.registry.get(&channel) else {
            return self.no_such_target(label).await;
        };
        handle.eject(target.clone()).await;
        self.send_event(
            label,
            Event::Moderated {
                scope: channel.to_string(),
                account: target,
                action: ModAction::Kick,
                by: Some(actor),
                reason,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    async fn on_event(&mut self, event: SessionEvent) -> io::Result<()> {
        if let State::Bridge { peer, .. } = self.state.clone() {
            return self.on_bridge_event(peer, event).await;
        }
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
                    // §6.7: a MEMBER part naming *me* on a channel I still hold
                    // is a forced removal (kick/ban) — drop the membership + its
                    // forwarder, then still deliver the event so the client sees.
                    let forced =
                        if let (State::Ready { account }, Event::Member { user, action, .. }) =
                            (&self.state, &event.event)
                        {
                            (*action == MemberAction::Part
                                && user.account == *account
                                && self.joined.contains_key(&channel))
                            .then(|| account.clone())
                        } else {
                            None
                        };
                    if let Some(account) = forced {
                        if let Some(joined) = self.joined.remove(&channel) {
                            joined.forwarder.abort();
                        }
                        // Force-part clears the persistent membership (no auto-rejoin).
                        if let Err(e) = self
                            .ctx
                            .memberships
                            .clear_membership(&account, &channel)
                            .await
                        {
                            error!("clear membership failed: {e}");
                        }
                    }
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

/// Build a `MANIFEST` event from a stored peering (§6.6). Decodes the current
/// manifest blob for the history/media/typing bounds.
fn manifest_event(record: &PeerRecord, state: BridgeState, channels: &[String]) -> Event {
    let (history, media, typing) = SignedManifest::from_b64(&record.manifest)
        .map(|s| {
            (
                s.manifest.history.parse().unwrap_or(HistoryMode::FromEpoch),
                s.manifest.media.parse().unwrap_or(MediaMode::None),
                s.manifest.typing,
            )
        })
        .unwrap_or((HistoryMode::FromEpoch, MediaMode::None, false));
    Event::Manifest {
        peer: record.peer.clone(),
        version: record.version,
        state,
        channels: channels.iter().filter_map(|c| c.parse().ok()).collect(),
        history,
        media,
        typing,
    }
}

/// Sender for a bridged `DELETED` that arrived without a `by=` — the tombstone
/// is keyed on the root, so this only fills the delete row's `sender` column.
fn deleted_placeholder() -> Account {
    "unknown".parse().expect("valid account literal")
}

/// Placeholder reporter for a §11.9 forwarded report: the reporter's identity
/// is stripped on the wire, so the local record carries a synthetic account and
/// the emitted `REPORT-FILED` sets `reporter: None`.
fn forwarded_reporter() -> Account {
    "forwarded".parse().expect("valid account literal")
}

/// Unix seconds — server-stamped time (§9.6); client clocks are untrusted.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) // pre-1970 clock: expire everything rather than panic
}

/// Unix milliseconds — matches the store's hold/filing timestamps (§12.1).
fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
