//! §16 WEFT-RT voice — the `VoiceBackend` port (the pluggable-SFU seam).
//!
//! Signaling authority stays in weft-core (caps precede side effects,
//! invariant 4): a `VOICE JOIN` is checked against `listen`/`speak` caps +
//! channel membership + the M7 mute/ban deny-list *here*, and only then handed
//! to the backend to allocate an SFU slot. The backend — the reference
//! `WebrtcSfu` in `weft-rt`, or a future LiveKit adapter — owns the UDP socket
//! and the WebRTC negotiation; it never interprets caps. weftd installs one via
//! [`ServerCtx::set_voice_backend`](crate::ServerCtx::set_voice_backend); a
//! server with none advertises no `features=voice` and answers voice verbs with
//! `UNSUPPORTED`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use weft_proto::{Account, ChannelName, NetworkName, UserRef, VoiceTransport};

/// An authorized voice join, handed to the backend after core's cap/membership/
/// moderation checks pass. `session` is the caller's session id — the SFU keys
/// its per-peer state by it and correlates the later `describe`/`candidate`/
/// `leave` calls (and, once media flows, the SDP's ICE ufrag).
#[derive(Debug, Clone)]
pub struct VoiceJoinReq {
    pub channel: ChannelName,
    pub account: Account,
    pub session: u64,
    /// Whether the caller may *publish* audio (holds `speak` and isn't muted).
    /// A listen-only participant still gets a peer connection — the SFU just
    /// drops any inbound RTP from it (enforcement is server-side, invariant 4).
    pub can_speak: bool,
}

/// What the backend returns for a `VOICE OFFER`: the media transport, a
/// short-lived token (bearing the granted `speak`/`listen` scope), and optional
/// room + endpoint hints. For the embedded `webrtc` SFU reachable at the session
/// host, `mode = Webrtc`, `room = None`, and the client negotiates against the
/// same address via ICE. For `livekit`, `token` is the access JWT, `room` names
/// the LiveKit room, and `endpoint` is the LiveKit server URL.
#[derive(Debug, Clone)]
pub struct VoiceGrant {
    pub mode: VoiceTransport,
    pub token: String,
    pub room: Option<String>,
    pub endpoint: Option<String>,
}

/// Why a backend refused. Both collapse to a client-visible error at the
/// session layer; neither leaks whether the channel *has* voice (invariant 1 is
/// enforced before the backend is ever called).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceError {
    /// The SFU is at capacity / temporarily unable to take the peer.
    Unavailable,
    /// A malformed or unacceptable SDP / candidate.
    BadDescription,
}

/// The SFU seam. One installed backend serves every voice channel; it fans a
/// participant's Opus RTP out to the room's other subscribers. Held as
/// `Arc<dyn VoiceBackend>` so the concrete engine (webrtc-rs) lives in `weft-rt`
/// (L2, owns the socket) without weft-core depending on it.
#[async_trait]
pub trait VoiceBackend: Send + Sync {
    /// Reserve a room slot for an already-authorized joiner (see
    /// [`VoiceJoinReq`]); returns the token + endpoint for `VOICE OFFER`.
    async fn join(&self, req: VoiceJoinReq) -> Result<VoiceGrant, VoiceError>;

    /// A client's SDP offer for its peer connection; returns the SFU's answer
    /// SDP (non-trickle: candidates already gathered into it).
    async fn describe(
        &self,
        session: u64,
        channel: &ChannelName,
        sdp: String,
    ) -> Result<String, VoiceError>;

    /// A trickle-ICE candidate from the client (no-op for non-trickle peers).
    async fn candidate(
        &self,
        session: u64,
        channel: &ChannelName,
        candidate: String,
    ) -> Result<(), VoiceError>;

    /// The client left the room (explicit `VOICE LEAVE` or disconnect); tear the
    /// peer down. Idempotent — a leave for an unknown session is a no-op.
    async fn leave(&self, session: u64, channel: &ChannelName);

    /// §6.7 server-side mute: drop (or resume) the participant's inbound audio at
    /// the SFU, so a moderator's `MUTE` silences them live, not just at join.
    /// Idempotent; a no-op for an unknown session.
    async fn set_muted(&self, session: u64, channel: &ChannelName, muted: bool);
}

/// §16 a participant in a voice room, as the server tracks it for the roster
/// snapshot + moderation. Speaking is transient (client-derived); the server
/// tracks membership + mute state.
#[derive(Debug, Clone)]
pub struct VoiceMember {
    pub account: Account,
    pub muted: bool,
}

/// The inputs for one LiveKit access-token JWT (M-lk-0). Pure data: the WEFT
/// authz gate has already run, so `can_publish`/`can_subscribe` are the *result*
/// of the `speak`/`listen`/mute checks, mapped straight to LiveKit grants.
#[derive(Debug, Clone)]
pub struct LiveKitTokenReq {
    /// LiveKit room id (see [`livekit_room`]).
    pub room: String,
    /// Participant identity — the canonical `user@network`, so the client
    /// resolves the avatar/display via §10.3 profiles.
    pub identity: String,
    /// `speak` held ∧ not muted ∧ (open ∨ has cap) → LiveKit `canPublish`.
    pub can_publish: bool,
    /// `listen` held (or open) → LiveKit `canSubscribe`.
    pub can_subscribe: bool,
    /// Token lifetime in seconds; the client re-`VOICE JOIN`s to refresh, which
    /// re-runs the gate so a revoked cap / fresh mute takes effect at refresh.
    pub ttl_secs: u64,
}

/// Port to the operator's LiveKit deployment. Token minting is pure HS256 crypto
/// (sync); the M-lk-2 moderation calls are async HTTP to LiveKit's Room server
/// API. Both live behind this trait so the real impl (JWT + `reqwest`) stays in
/// weftd (L3, may do I/O) while weft-core stays socket-free, and a mock drives
/// the core tests.
#[async_trait]
pub trait LiveKitAdmin: Send + Sync {
    /// Mint a signed LiveKit access-token JWT for one participant.
    fn access_token(&self, req: &LiveKitTokenReq) -> String;

    /// §6.7 mute (or unmute) all of a participant's published tracks live —
    /// the LiveKit equivalent of the SFU dropping their inbound audio. Best
    /// effort: a transport error is logged by the impl, not surfaced (the deny
    /// list remains the source of truth, re-applied on the participant's next
    /// join/token refresh).
    async fn set_participant_muted(&self, room: &str, identity: &str, muted: bool);

    /// Remove a participant from the room (ban / kick / disconnect). Best effort.
    async fn remove_participant(&self, room: &str, identity: &str);
}

/// The LiveKit room id for a channel: `wv:<network>:<channel>`. Stable across a
/// call and collision-free across namespaces (the channel name already carries
/// its `ns/`). It never exposes a title the joiner can't already see — they've
/// passed the membership gate before a token is minted.
pub fn livekit_room(network: &str, channel: &ChannelName) -> String {
    format!("wv:{network}:{channel}")
}

/// A [`VoiceBackend`] that fulfils `VOICE JOIN` with a **LiveKit** access token
/// rather than an in-server SFU negotiation (M-lk-0). Core still runs the full
/// authz gate (caps / mute / ban / voice-kind) *before* calling `join`; this
/// only mints the media credential and points the client at LiveKit. The WebRTC
/// `describe`/`candidate` handshake is unused in this mode — the client talks to
/// the LiveKit server directly with the SDK.
pub struct LiveKitBackend {
    admin: Arc<dyn LiveKitAdmin>,
    /// LiveKit server URL handed to the client (`wss://…`) as the offer trailing.
    url: String,
    /// This network's name — for the room id and the participant identity.
    network: NetworkName,
    /// Access-token lifetime (seconds).
    ttl_secs: u64,
    /// Session → (room, identity), recorded at `join`. Moderation is keyed by
    /// session at the trait boundary (like the SFU), but LiveKit's Room API is
    /// keyed by room + identity — this map bridges the two without widening the
    /// `VoiceBackend` signatures.
    peers: Mutex<HashMap<u64, LiveKitPeer>>,
}

#[derive(Clone)]
struct LiveKitPeer {
    room: String,
    identity: String,
}

impl LiveKitBackend {
    pub fn new(
        admin: Arc<dyn LiveKitAdmin>,
        url: String,
        network: NetworkName,
        ttl_secs: u64,
    ) -> Self {
        Self {
            admin,
            url,
            network,
            ttl_secs,
            peers: Mutex::new(HashMap::new()),
        }
    }

    fn peer(&self, session: u64) -> Option<LiveKitPeer> {
        self.peers
            .lock()
            .expect("livekit peers lock")
            .get(&session)
            .cloned()
    }
}

#[async_trait]
impl VoiceBackend for LiveKitBackend {
    async fn join(&self, req: VoiceJoinReq) -> Result<VoiceGrant, VoiceError> {
        let room = livekit_room(self.network.as_str(), &req.channel);
        let identity = UserRef::new(req.account, self.network.clone()).to_string();

        // Record the session's (room, identity) so a later moderation call —
        // keyed by session — can address the LiveKit Room API.
        self.peers.lock().expect("livekit peers lock").insert(
            req.session,
            LiveKitPeer {
                room: room.clone(),
                identity: identity.clone(),
            },
        );

        // The gate's decision maps one-to-one onto LiveKit grants (see the plan's
        // cap→grant table): `can_speak` → canPublish; subscribe is always granted
        // to a member that passed the `listen` gate above.
        let token = self.admin.access_token(&LiveKitTokenReq {
            room: room.clone(),
            identity,
            can_publish: req.can_speak,
            can_subscribe: true,
            ttl_secs: self.ttl_secs,
        });

        Ok(VoiceGrant {
            mode: VoiceTransport::Livekit,
            token,
            room: Some(room),
            endpoint: Some(self.url.clone()),
        })
    }

    async fn describe(
        &self,
        _session: u64,
        _channel: &ChannelName,
        _sdp: String,
    ) -> Result<String, VoiceError> {
        // A LiveKit client negotiates with the LiveKit server, never with us.
        Err(VoiceError::Unavailable)
    }

    async fn candidate(
        &self,
        _session: u64,
        _channel: &ChannelName,
        _candidate: String,
    ) -> Result<(), VoiceError> {
        Err(VoiceError::Unavailable)
    }

    async fn leave(&self, session: u64, _channel: &ChannelName) {
        // Actively remove the participant from the LiveKit room. Used both on a
        // normal `VOICE LEAVE`/disconnect and on a ban/kick eject — LiveKit also
        // reaps on WebSocket close, but a moderator-driven eject can't wait for
        // the client to hang up. Drop the lock before awaiting (guard is !Send).
        let peer = self
            .peers
            .lock()
            .expect("livekit peers lock")
            .remove(&session);
        if let Some(peer) = peer {
            self.admin
                .remove_participant(&peer.room, &peer.identity)
                .await;
        }
    }

    async fn set_muted(&self, session: u64, _channel: &ChannelName, muted: bool) {
        // §6.7 live mute via the LiveKit Room server API. Best effort: if the
        // session isn't a LiveKit peer (unknown / already gone), nothing to do.
        if let Some(peer) = self.peer(session) {
            self.admin
                .set_participant_muted(&peer.room, &peer.identity, muted)
                .await;
        }
    }
}
