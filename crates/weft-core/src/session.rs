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
use weft_crypto::{Capability, PublicKey, SignedManifest, TokenScope};
use weft_proto::{
    Account, BridgeState, ChannelKind, ChannelName, Command, ContentState, ErrCode, ErrEvent,
    Event, FSessionOp, HistoryMode, Line, MediaMode, MemberAction, MessageEvent, ModAction, MsgId,
    MsgMeta, NamespaceName, NetworkName, ParseError, Reply, ReportScope, ReportStatus, Request,
    ResolveAction, RetentionPolicy, StreamMode, Target, Ulid, UserRef, VerifyState, Visibility,
    MAX_LABEL_BYTES,
};

use weft_store::{
    EventKind, EventRecord, InviteRecord, ModKind, ModRecord, NetblockRecord, PeerRecord,
    RedeemOutcome, ReportRecord, ReportResolution, Scope,
};

use crate::bridge;
use crate::channel::{ChannelEvent, ChannelHandle};
use crate::context::{channel_namespace, covering_scopes, Actor, ServerCtx, PROTOCOL_VERSION};
use crate::directory::DirectEvent;
use crate::stream::ControlStream;

// Handler groups split out of the session engine (methods are `pub(super)` so
// the dispatch here can route to them; they see this module's private fields
// and helpers as descendants).
mod auth;
mod caps;
mod channels;
mod federation;
mod invites;
mod moderation;
mod namespaces;
mod profile;
mod relay;
mod roles;
mod verify;
mod voice;

/// Process-unique connection identifier (also the actor-side member key).
pub type SessionId = u64;

/// §3.3: idle pre-auth sessions closed after 30 s.
const PREAUTH_IDLE: Duration = Duration::from_secs(30);
/// §3.4 keepalive: authed clients PING every ~10 s, so a healthy session is
/// never quiet this long. The generous ceiling tolerates browser background-tab
/// timer throttling (which can stretch the client's interval past a minute) and
/// brief network stalls without a spurious reconnect; a genuinely dead socket is
/// still reaped by the transport's own idle timeout.
const READY_IDLE: Duration = Duration::from_secs(120);
/// §16 idle ceiling for a session that is **in a voice room**. A crashed client
/// is invisible to the transport — a dead QUIC peer sends no FIN (it's UDP), so
/// the connection is only reaped at the idle timeout, and until then the caller
/// lingers in every co-member's voice roster. A session in a call is by
/// definition an active client PINGing every ~10 s (§3.4), so three missed
/// keepalives is a confident "gone" and bounds the ghost to ~30 s. Tabs playing
/// audio are exempt from browser timer throttling, so the throttling headroom
/// that `READY_IDLE` allows for isn't needed here.
const VOICE_IDLE: Duration = Duration::from_secs(30);
/// §9.2: dedup MSG retries by (session, label) for 5 minutes.
const DEDUP_WINDOW: Duration = Duration::from_secs(300);
/// §8: MALFORMED — close after 5 per 60 s.
const MALFORMED_LIMIT: usize = 5;
const MALFORMED_WINDOW: Duration = Duration::from_secs(60);
/// §2.4 recovery delay windows: rung 2 (social quorum) 7 days.
const RECOVERY_DELAY_RUNG2_SECS: u64 = 7 * 24 * 3600;
/// §2.4 rung 3 (operator takeover) — **zero delay, by deployment decision**
/// (spec Appendix A). The spec's original 30-day window made the rung useless
/// for its actual job: seizing a namespace whose owner is *actively* abusing it.
/// A moderator cannot wait a month, and the delay's purpose — letting a live
/// root veto a hostile recovery — is exactly what must NOT happen when the root
/// is the problem.
///
/// The two accountability properties that don't depend on the delay are kept,
/// and they are what make this honest: the takeover is **announced** to members
/// and is **permanently marked operator-initiated in `root-history`**, visible
/// to every member and bridge peer forever. What is knowingly given up is the
/// root's veto window (invariant 9's "delay + root-cancellable"). See §2.4.
const RECOVERY_DELAY_RUNG3_SECS: u64 = 0;
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

/// How an outbound bridge session opens (§11 M5d / §11.10).
enum OutboundStart {
    /// Transmit our operator's stored proposal for the peer (P1).
    Propose,
    /// Ask the peer to offer a manifest for its namespace `ns` and auto-accept
    /// the offer — the §11.10 requester side.
    Request(NamespaceName),
}

/// Run an **outbound** bridge session over a `stream` the dialer already
/// authenticated (the §11.2 handshake driven as a client). Enters `State::Bridge`
/// directly, opens per `start`, then reuses the ordinary bridge loop — ingesting
/// the peer's events and forwarding our local-origin ones. `key` is the peer's
/// network key (pinned / well-known), used to verify the manifests it sends.
async fn run_outbound_bridge<S: ControlStream>(
    stream: S,
    ctx: Arc<ServerCtx>,
    peer: NetworkName,
    key: PublicKey,
    start: OutboundStart,
) {
    let id = ctx.next_session_id();
    let span = info_span!("bridge-out", %peer, id);
    let _conn = ConnectionGuard::enter(&ctx);
    async move {
        let mut session = Session::new(id, stream, ctx);
        session.state = State::Bridge {
            peer: peer.clone(),
            key,
        };
        // Only an **outbound** bridge (we dialed) pulls the peer's scrollback on
        // client demand (§11.7): register this session as a backfill target.
        session
            .ctx
            .register_backfill_demand(session.backfill_demand_tx.clone());
        match start {
            OutboundStart::Propose => session.begin_outbound_bridge(&peer).await,
            OutboundStart::Request(ns) => {
                // We asked for this bridge, so accept the offer regardless of the
                // inbound auto-accept config.
                session.request_accept = true;
                session.begin_outbound_request(&ns).await;
            }
        }
        match session.run().await {
            Ok(()) => debug!("outbound bridge closed"),
            Err(e) => debug!("outbound bridge ended: {e}"),
        }
        session.cleanup().await;
        let _ = session.stream.close().await;
    }
    .instrument(span)
    .await;
}

/// P1: run an outbound bridge that transmits our stored proposal.
pub async fn run_bridge_client<S: ControlStream>(
    stream: S,
    ctx: Arc<ServerCtx>,
    peer: NetworkName,
    key: PublicKey,
) {
    run_outbound_bridge(stream, ctx, peer, key, OutboundStart::Propose).await
}

/// §11.10: run an outbound bridge that *requests* the peer's namespace `ns`
/// (`BRIDGE REQUEST`) and auto-accepts the offer — the on-demand auto-federation
/// path.
pub async fn run_bridge_requester<S: ControlStream>(
    stream: S,
    ctx: Arc<ServerCtx>,
    peer: NetworkName,
    key: PublicKey,
    ns: NamespaceName,
) {
    run_outbound_bridge(stream, ctx, peer, key, OutboundStart::Request(ns)).await
}

/// §11.10 A federated user's [`ControlStream`], multiplexed over a peer's bridge
/// (homeserver authority). Inbound = her command lines (fed by the bridge demux
/// on `FSESSION CMD`); each outbound reply is wrapped as an `FSESSION <fsid>
/// REPLY` frame and handed to the bridge session's writer — whose single run
/// loop serializes every socket write, so tunnels never race the bridge.
struct TunnelStream {
    fsid: String,
    inbound: mpsc::Receiver<String>,
    outbound: mpsc::Sender<String>,
}

impl ControlStream for TunnelStream {
    async fn recv_line(&mut self) -> io::Result<Option<String>> {
        // `None` when the bridge drops the tunnel (`FSESSION CLOSE` / bridge end).
        Ok(self.inbound.recv().await)
    }

    async fn send_line(&mut self, line: &str) -> io::Result<()> {
        let framed = Request::new(Command::FSession {
            fsid: self.fsid.clone(),
            op: FSessionOp::Reply {
                line: line.to_string(),
            },
        })
        .serialize()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        self.outbound
            .send(framed)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "bridge writer gone"))
    }
}

/// §11.10 Run a federated user's session over a bridge tunnel: enter
/// `State::Federated` directly (F already vouched for `user` by proving its
/// network key at `AUTH BRIDGE`), then reuse the ordinary session loop —
/// commands enforce against H's grant store as `Actor::Foreign(user)`. Spawned
/// per `FSESSION OPEN`; ends when the tunnel closes.
/// Spawn a tunnelled federated session. A free (non-generic) function so the
/// `tokio::spawn` Send-check runs on the concrete `TunnelStream` — the same
/// footing as the acceptor spawning a concrete transport, avoiding the RPITIT
/// auto-trait ambiguity that bites when spawning from inside `impl<S> Session`.
fn spawn_federated_session(stream: TunnelStream, ctx: Arc<ServerCtx>, user: String) {
    tokio::spawn(async move {
        run_federated_session(stream, ctx, user).await;
    });
}

async fn run_federated_session<S: ControlStream>(stream: S, ctx: Arc<ServerCtx>, user: String) {
    let id = ctx.next_session_id();
    let span = info_span!("fed-session", %user, id);
    let _conn = ConnectionGuard::enter(&ctx);
    async move {
        let mut session = Session::new(id, stream, ctx);
        session.state = State::Federated { user };
        match session.run().await {
            Ok(()) => debug!("federated session closed"),
            Err(e) => debug!("federated session ended: {e}"),
        }
        session.cleanup().await;
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
    /// §11.10 a **federated** user's session, tunnelled over a peer's bridge
    /// (homeserver authority): the peer network `F` vouched for `user`
    /// (`account@F`) by proving its network key. Commands enforce against H's
    /// grant store as `Actor::Foreign(user)`; it is a pure command conduit and
    /// never subscribes to channels — broadcast events ride the mirror (§10.3).
    Federated {
        user: String,
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

/// §16 a voice room this session has joined. Voice channels aren't text-joined,
/// so the session *subscribes* to the channel's broadcast (for `VOICE STATE`)
/// without becoming a text member; `handle` is used to announce state changes.
struct VoiceRoom {
    handle: ChannelHandle,
    forwarder: JoinHandle<()>,
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
    /// WC7 forced logout: this session's own cancellation, handed to the account
    /// directory at register time. Cancelling it drops the session out of its
    /// loop and through the ordinary `cleanup` (so parts/voice leaves still
    /// broadcast) — distinct from `ctx.shutdown`, which stops *every* session.
    close: tokio_util::sync::CancellationToken,
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
    /// §11.10 bridge sessions only: the single outbound queue every tunnelled
    /// federated session writes its `FSESSION … REPLY` frames to — drained by
    /// this session's run loop so all socket writes stay serialized. `tunnels`
    /// routes an inbound `FSESSION CMD` to the right sub-session by `fsid`.
    fed_out_tx: mpsc::Sender<String>,
    fed_out_rx: mpsc::Receiver<String>,
    tunnels: HashMap<String, mpsc::Sender<String>>,
    /// §11.10 requester side: this outbound session asked for the peer's
    /// manifest, so it accepts the offer regardless of the auto-accept config.
    request_accept: bool,
    /// §11.7 bridge sessions: `(channel, before)` windows we've already asked the
    /// peer to backfill, so repeated client scrolls over the same window fetch
    /// the peer only once.
    backfilled: std::collections::HashSet<(ChannelName, Option<String>)>,
    /// §11.7 outbound bridge sessions: local clients' on-demand backfill needs
    /// (a HISTORY that ran out of local scrollback), drained in the run loop and
    /// turned into a HISTORY to the peer. Empty/unused on other sessions.
    backfill_demand_tx: mpsc::UnboundedSender<crate::BackfillReq>,
    backfill_demand_rx: mpsc::UnboundedReceiver<crate::BackfillReq>,
    /// HISTORY batch id counter (per session, opaque to clients).
    batches: u64,
    /// §16 voice rooms this session has joined (voice-only channels, distinct
    /// from `joined`). Drives SFU teardown + broadcast unsubscribe on `VOICE
    /// LEAVE` and on disconnect so no peer or forwarder is orphaned.
    voice: HashMap<ChannelName, VoiceRoom>,
    /// §16 voice channels this session *watches* for presence (`VOICE WATCH`) —
    /// live roster without joining the call. Each holds the broadcast forwarder;
    /// aborted on unwatch / join / disconnect.
    voice_watches: HashMap<ChannelName, JoinHandle<()>>,
    malformed_strikes: Vec<Instant>,
    last_inbound: Instant,
}

#[allow(clippy::large_enum_variant)] // one per select iteration, stack-only
enum Action {
    Line(Option<String>),
    Event(SessionEvent),
    Direct(DirectEvent),
    /// §11.10 a tunnelled federated session's outbound frame, to write out.
    FedOut(String),
    /// §11.7 a local client's on-demand backfill need (outbound bridge only).
    Backfill(crate::BackfillReq),
    Idle,
}

impl<S: ControlStream> Session<S> {
    fn new(id: SessionId, stream: S, ctx: Arc<ServerCtx>) -> Self {
        let (events_tx, events_rx) = mpsc::channel(EVENT_QUEUE);
        let (direct_tx, direct_rx) = mpsc::channel(EVENT_QUEUE);
        let (fed_out_tx, fed_out_rx) = mpsc::channel(EVENT_QUEUE);
        let (backfill_demand_tx, backfill_demand_rx) = mpsc::unbounded_channel();
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
            close: tokio_util::sync::CancellationToken::new(),
            pending_direct: VecDeque::new(),
            registered: None,
            dedup: HashMap::new(),
            bridged: HashMap::new(),
            fed_out_tx,
            fed_out_rx,
            tunnels: HashMap::new(),
            request_accept: false,
            backfilled: std::collections::HashSet::new(),
            backfill_demand_tx,
            backfill_demand_rx,
            batches: 0,
            voice: HashMap::new(),
            voice_watches: HashMap::new(),
            malformed_strikes: Vec::new(),
            last_inbound: Instant::now(),
        }
    }

    async fn run(&mut self) -> io::Result<()> {
        loop {
            let limit = match self.state {
                // §16 a session holding a voice room is reaped far sooner: a
                // crashed client leaves no ghost in the roster for two minutes.
                State::Ready { .. } if !self.voice.is_empty() => VOICE_IDLE,
                State::Ready { .. } => READY_IDLE,
                _ => PREAUTH_IDLE,
            };
            let action = tokio::select! {
                line = self.stream.recv_line() => Action::Line(line?),
                event = self.events_rx.recv() =>
                    Action::Event(event.expect("session holds an events sender")),
                direct = self.direct_rx.recv() =>
                    Action::Direct(direct.expect("session holds a direct sender")),
                framed = self.fed_out_rx.recv() =>
                    Action::FedOut(framed.expect("session holds a fed_out sender")),
                req = self.backfill_demand_rx.recv() =>
                    Action::Backfill(req.expect("session holds a backfill sender")),
                _ = tokio::time::sleep_until(self.last_inbound + limit) => Action::Idle,
                // Graceful shutdown: this branch is only reached between commands
                // (a command in `on_line` runs to completion first), so no
                // in-flight work is interrupted; `run_session` then cleans up.
                _ = self.ctx.shutdown.cancelled() => return Ok(()),
                // WC7 forced logout: an operator cut this session. Same shape as
                // shutdown — leave the loop and let `cleanup` run.
                _ = self.close.cancelled() => {
                    debug!("session closed by operator");
                    return Ok(());
                }
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
                // §11.10 a tunnelled federated session's reply — write it out on
                // this (bridge) session's single stream.
                Action::FedOut(framed) => self.stream.send_line(&framed).await?,
                // §11.7 a local client asked for history we don't hold — fetch it
                // from the peer over this bridge (outbound bridge sessions only).
                Action::Backfill(req) => self.on_backfill_demand(req).await,
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
        // §16 tear down any voice peers first (before the channel handles go),
        // so the SFU drops them and members see a `VOICE STATE leave`.
        for (channel, room) in std::mem::take(&mut self.voice) {
            self.teardown_voice(&channel, room).await;
        }
        // §16 drop any presence-watch forwarders (no roster impact — a watcher
        // isn't in the room).
        for (_, forwarder) in std::mem::take(&mut self.voice_watches) {
            forwarder.abort();
        }
        for (_, joined) in self.joined.drain() {
            joined.forwarder.abort();
            // A disconnect keeps the persistent membership (auto-rejoin later),
            // so it broadcasts a MEMBER part for the roster but posts NO "left"
            // system line — only an explicit PART does.
            joined.handle.part(self.id, false).await;
        }
        for (_, forwarder) in self.bridged.drain() {
            forwarder.abort();
        }
        if let Some(account) = self.registered.take() {
            self.ctx
                .directory
                .deregister(account.clone(), self.id)
                .await;
            // If that was the account's last session, it is now offline —
            // drop its presence so a later MEMBERS snapshot renders it offline
            // (the live grey-out already rode each channel's disconnect part).
            if !self.ctx.directory.is_online(&account).await {
                self.ctx
                    .presence
                    .lock()
                    .expect("presence lock")
                    .remove(&account);
            }
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
            State::Federated { user } => self.on_federated(label, cmd, user).await,
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
            Command::StreamOffer { mode, mime, bytes } => {
                self.on_stream_offer(label, mode, mime, bytes, account)
                    .await
            }
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
                // Invisible is stored (so a live invisible member reads offline
                // in the snapshot) but never broadcast — revealing it would
                // defeat hiding.
                {
                    let mut map = self.ctx.presence.lock().expect("presence lock");
                    map.insert(account.clone(), status);
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
            Command::Unread { channel } => self.on_unread(label, channel, account).await,
            // Pagination (`cursor`) isn't needed at reference channel sizes —
            // the roster is served in one batch.
            Command::Members { channel, .. } => self.on_members(label, channel).await,
            Command::Pin { msgid } => self.on_pin(label, msgid, account, true).await,
            Command::Unpin { msgid } => self.on_pin(label, msgid, account, false).await,
            Command::Pins { channel } => self.on_pins(label, channel).await,
            Command::Search { channel, query } => self.on_search(label, channel, query).await,
            Command::Threads { channel } => self.on_threads(label, channel).await,
            Command::ThreadName {
                channel,
                root,
                name,
            } => {
                self.on_thread_name(label, channel, root, name, account)
                    .await
            }
            Command::EmojiAdd {
                namespace,
                name,
                media,
            } => {
                self.on_emoji_add(label, namespace, name, media, Actor::Local(account))
                    .await
            }
            Command::EmojiRemove { namespace, name } => {
                self.on_emoji_remove(label, namespace, name, Actor::Local(account))
                    .await
            }
            Command::EmojiList { namespace } => self.on_emoji_list(label, namespace).await,
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
                self.on_grant(label, subject, scope, caps, expiry, Actor::Local(account))
                    .await
            }
            Command::Revoke {
                subject,
                scope,
                caps,
                epoch,
            } => {
                self.on_revoke(label, subject, scope, caps, epoch, Actor::Local(account))
                    .await
            }
            // §6.5 named roles (capability-token bundles).
            Command::RoleCreate {
                scope,
                color,
                caps,
                hoist,
                position,
                name,
            } => {
                self.on_role_create(label, scope, color, caps, hoist, position, name, account)
                    .await
            }
            Command::RolesReorder { scope, order } => {
                self.on_roles_reorder(label, scope, order, account).await
            }
            Command::RoleDelete { scope, name } => {
                self.on_role_delete(label, scope, name, account).await
            }
            Command::RoleRename { scope, old, new } => {
                self.on_role_rename(label, scope, old, new, account).await
            }
            Command::RoleAssign {
                scope,
                account: subject,
                name,
            } => {
                self.on_role_assign(label, scope, subject, name, Actor::Local(account))
                    .await
            }
            Command::RoleUnassign {
                scope,
                account: subject,
                name,
            } => {
                self.on_role_unassign(label, scope, subject, name, Actor::Local(account))
                    .await
            }
            Command::RolesList { scope } => self.on_roles_list(label, scope).await,
            Command::RolesOf {
                scope,
                account: subject,
            } => self.on_roles_of(label, scope, subject).await,
            Command::ChannelCreate {
                channel,
                policy,
                kind,
            } => {
                self.on_channel_create(label, channel, policy, kind, Actor::Local(account))
                    .await
            }
            Command::ChannelPolicy {
                channel,
                policy,
                purge,
            } => {
                self.on_channel_policy(label, channel, policy, purge, Actor::Local(account))
                    .await
            }
            Command::ChannelMeta {
                channel,
                key,
                value,
            } => {
                self.on_channel_meta(label, channel, key, value, Actor::Local(account))
                    .await
            }
            Command::ChannelDelete { channel, confirm } => {
                self.on_channel_delete(label, channel, confirm, Actor::Local(account))
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
                self.on_invite_mint(label, scope, max_uses, expiry, Actor::Local(account))
                    .await
            }
            Command::InviteRevoke { invite_id } => {
                self.on_invite_revoke(label, invite_id, Actor::Local(account))
                    .await
            }
            Command::InviteRevokeAll { scope } => {
                self.on_invite_revoke_all(label, scope, Actor::Local(account))
                    .await
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
                self.on_ns_meta(label, name, key, value, Actor::Local(account))
                    .await
            }
            Command::NsVisibility { name, visibility } => {
                self.on_ns_visibility(label, name, visibility, Actor::Local(account))
                    .await
            }
            Command::NsDelegate {
                name,
                subject,
                caps,
            } => {
                // Sugar for GRANT at ns: scope (§6.2).
                self.on_grant(
                    label,
                    subject,
                    format!("ns:{name}"),
                    caps,
                    None,
                    Actor::Local(account),
                )
                .await
            }
            Command::NsDelete { name, confirm } => {
                self.on_ns_delete(label, name, confirm, Actor::Local(account))
                    .await
            }
            Command::NsJoin { name } => self.on_ns_join(label, name, account).await,
            Command::Discover { cursor } => self.on_discover(label, cursor).await,
            Command::Channels { namespace } => self.on_channels(label, namespace).await,
            Command::Federate { network, namespace } => {
                self.on_federate(label, network, namespace, account).await
            }
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
                self.on_reports_list(label, scope, status, cursor, Actor::Local(account))
                    .await
            }
            Command::ReportsResolve {
                report_id,
                action,
                note,
            } => {
                self.on_reports_resolve(label, report_id, action, note, Actor::Local(account))
                    .await
            }
            // §6.7 moderation. `account` here is the acting moderator.
            Command::Mute {
                scope,
                account: target,
                reason,
            } => {
                self.on_moderate(
                    label,
                    scope,
                    target,
                    ModKind::Mute,
                    true,
                    reason,
                    Actor::Local(account),
                )
                .await
            }
            Command::Unmute {
                scope,
                account: target,
            } => {
                self.on_moderate(
                    label,
                    scope,
                    target,
                    ModKind::Mute,
                    false,
                    None,
                    Actor::Local(account),
                )
                .await
            }
            Command::Ban {
                scope,
                account: target,
                reason,
            } => {
                self.on_moderate(
                    label,
                    scope,
                    target,
                    ModKind::Ban,
                    true,
                    reason,
                    Actor::Local(account),
                )
                .await
            }
            Command::Unban {
                scope,
                account: target,
            } => {
                self.on_moderate(
                    label,
                    scope,
                    target,
                    ModKind::Ban,
                    false,
                    None,
                    Actor::Local(account),
                )
                .await
            }
            Command::Kick {
                channel,
                account: target,
                reason,
            } => {
                self.on_kick(label, channel, target, reason, Actor::Local(account))
                    .await
            }
            Command::ModList { scope } => {
                self.on_modlist(label, scope, Actor::Local(account)).await
            }
            // §11 federation — operator-facing management (§6.6).
            Command::BridgePropose {
                scope,
                peer,
                history,
                media,
                typing,
                voice,
                ..
            } => {
                self.on_bridge_propose(label, scope, peer, history, media, typing, voice, account)
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
            // §11.10 BRIDGE REQUEST is spoken only *over* an authenticated bridge
            // session (peer → peer), never by a local client.
            Command::BridgeRequest { .. } => {
                self.unsupported(label, "BRIDGE REQUEST is bridge-session-only")
                    .await
            }
            // §16 VOICE REQUEST asks a *peer* to relay one of its voice channels;
            // it only makes sense over an authenticated bridge session, never from
            // a local client.
            Command::VoiceRequest { .. } => {
                self.unsupported(label, "VOICE REQUEST is bridge-session-only")
                    .await
            }
            // §11.10 FSESSION frames are spoken only over an authenticated bridge
            // (F tunnels a user's session to H), never by a local client.
            Command::FSession { .. } => {
                self.unsupported(label, "FSESSION is bridge-session-only")
                    .await
            }
            Command::NetblockAdd { network, reason } => {
                self.on_netblock_add(label, network, reason, account).await
            }
            Command::NetblockRemove { network } => {
                self.on_netblock_remove(label, network, account).await
            }
            Command::NetblockList => self.on_netblock_list(label, account).await,
            Command::MediaBlock { hash, reason } => {
                self.on_media_block(label, hash, reason, account).await
            }
            Command::MediaUnblock { hash } => self.on_media_unblock(label, hash, account).await,
            Command::MediaBlocks => self.on_media_blocks(label, account).await,
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
            // §10.3 display profiles.
            Command::ProfileSet { display, avatar } => {
                self.on_profile_set(label, display, avatar, account).await
            }
            Command::ProfilesQuery { accounts } => self.on_profiles_query(label, accounts).await,
            // §10.5 account verification (email code flow + self-attested birthday).
            Command::VerifyEmail { address } => self.on_verify_email(label, address, account).await,
            Command::VerifyBirthday { date } => self.on_verify_birthday(label, date, account).await,
            Command::VerifyConfirm { kind, code } => {
                self.on_verify_confirm(label, kind, code, account).await
            }
            Command::VerifyList => self.on_verify_list(label, account).await,
            // §16 WEFT-RT voice signaling. The SFU backend is installed by weftd;
            // a zero-voice server answers `UNSUPPORTED` inside these handlers.
            Command::VoiceJoin { channel } => self.on_voice_join(label, channel, account).await,
            Command::VoiceLeave { channel } => self.on_voice_leave(label, channel).await,
            Command::VoiceDesc { channel, sdp } => self.on_voice_desc(label, channel, sdp).await,
            Command::VoiceCand { channel, candidate } => {
                self.on_voice_cand(label, channel, candidate).await
            }
            Command::Unknown { .. } => Ok(Flow::Continue), // handled in on_request
        }
    }

    // ---- READY verb handlers ----

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
                    // Own system message (our own join/part line): other members
                    // see it live, and it was stored, so it still reaches us via
                    // HISTORY on reload — no need to echo it back to ourselves.
                    Event::Message(m) if m.meta.system.is_some() => {
                        let _ = m;
                        Ok(())
                    }
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

    /// `STREAM OFFER <media|backfill> <mime> <bytes>` (§13) — check size (the
    /// `attach` cap + per-mime limits are M-media-1), mint a one-time upload
    /// grant, and reply `STREAM ACCEPT <token>`; the bytes then ride the data
    /// plane (weftd), consuming the grant.
    async fn on_stream_offer(
        &mut self,
        label: Option<String>,
        mode: StreamMode,
        mime: String,
        bytes: u64,
        account: Account,
    ) -> io::Result<Flow> {
        if mode != StreamMode::Media {
            return self
                .unsupported(label, "STREAM backfill lands in M-media-4")
                .await;
        }
        if bytes == 0 || bytes > crate::MEDIA_MAX_BYTES {
            self.send_err(label, ErrCode::TooLarge, None, "blob size out of range")
                .await?;
            return Ok(Flow::Continue);
        }
        let token = self.ctx.mint_upload_token(account, mime, bytes);
        self.send_event(label, Event::StreamAccept { token })
            .await?;
        Ok(Flow::Continue)
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
/// manifest blob for the history/media/typing/voice bounds.
fn manifest_event(record: &PeerRecord, state: BridgeState, channels: &[String]) -> Event {
    let (history, media, typing, voice) = SignedManifest::from_b64(&record.manifest)
        .map(|s| {
            (
                s.manifest.history.parse().unwrap_or(HistoryMode::FromEpoch),
                s.manifest.media.parse().unwrap_or(MediaMode::None),
                s.manifest.typing,
                s.manifest.voice,
            )
        })
        .unwrap_or((HistoryMode::FromEpoch, MediaMode::None, false, false));
    Event::Manifest {
        peer: record.peer.clone(),
        version: record.version,
        state,
        channels: channels.iter().filter_map(|c| c.parse().ok()).collect(),
        history,
        media,
        typing,
        voice,
    }
}

/// Sender for a bridged `DELETED` that arrived without a `by=` — the tombstone
/// is keyed on the root, so this only fills the delete row's `sender` column.
/// The namespace an invite scope belongs to, for a federation-ready link:
/// `ns:<name>` → `<name>`; `#<ns>/<chan>` → `<ns>`; a top-level `#<chan>` or
/// `*` → `None` (nothing to federate).
fn invite_scope_namespace(scope: &str) -> Option<&str> {
    if let Some(ns) = scope.strip_prefix("ns:") {
        Some(ns)
    } else {
        scope
            .strip_prefix('#')
            .and_then(|c| c.split_once('/'))
            .map(|(ns, _)| ns)
    }
}

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
