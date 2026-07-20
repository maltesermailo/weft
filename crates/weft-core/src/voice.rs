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

use async_trait::async_trait;

use weft_proto::{Account, ChannelName};

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

/// What the backend returns for a `VOICE OFFER`: a short-lived media token
/// (bearing the granted `speak`/`listen` scope) and an optional SFU endpoint
/// hint. For an embedded SFU reachable at the session host the hint is `None`
/// and the client negotiates against the same address via ICE.
#[derive(Debug, Clone)]
pub struct VoiceGrant {
    pub token: String,
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
