//! The reference SFU: one [`WebrtcSfu`] serves every voice channel. Each
//! participant gets one `RTCPeerConnection`; the participant's inbound Opus
//! track is mirrored into a [`TrackLocalStaticRTP`] that every *other* peer in
//! the room subscribes to, so `write_rtp` fans one read out to all subscribers
//! (SSRC/payload-type rewritten per binding by webrtc-rs — pure forwarding).
//!
//! Signaling authority (caps, membership, mutes) is weft-core's job (invariant
//! 4); by the time a call reaches this backend the join is already authorized.
//!
//! **Scope (M-voice-1b):** a subscriber picks up every publisher that already
//! exists when it negotiates (its `describe`). Pushing a *new* publisher into
//! an already-connected peer needs SFU-initiated renegotiation (an offer the
//! server sends the client) — that path (and trickle-ICE from the server) is
//! M-voice-1c, so today a room converges cleanly when peers join in order.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use weft_core::{VoiceBackend, VoiceError, VoiceGrant, VoiceJoinReq};
use weft_proto::ChannelName;

use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_OPUS};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::{APIBuilder, API};
use webrtc::ice::udp_network::{EphemeralUDP, UDPNetwork};
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::{TrackLocal, TrackLocalWriter};

/// SFU configuration: the UDP port range it owns and the STUN servers it
/// advertises to clients (for their server-reflexive candidates).
#[derive(Debug, Clone)]
pub struct SfuConfig {
    pub udp_port_min: u16,
    pub udp_port_max: u16,
    pub ice_servers: Vec<String>,
}

impl Default for SfuConfig {
    fn default() -> Self {
        Self {
            udp_port_min: 40000,
            udp_port_max: 40100,
            ice_servers: vec!["stun:stun.l.google.com:19302".to_string()],
        }
    }
}

/// SFU construction / negotiation failures.
#[derive(Debug, thiserror::Error)]
pub enum SfuError {
    #[error("webrtc: {0}")]
    Webrtc(#[from] webrtc::Error),
    #[error("udp port range: {0}")]
    UdpRange(String),
}

/// The audio Opus capability the SFU offers (48 kHz stereo, in-band FEC).
fn opus_capability() -> RTCRtpCodecCapability {
    RTCRtpCodecCapability {
        mime_type: MIME_TYPE_OPUS.to_owned(),
        clock_rate: 48000,
        channels: 2,
        sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
        rtcp_feedback: vec![],
    }
}

/// One voice channel's live WebRTC state.
#[derive(Default)]
struct Room {
    /// session → its peer connection.
    peers: HashMap<u64, Arc<RTCPeerConnection>>,
    /// session → the local track mirroring that session's inbound audio (what
    /// the room's other peers subscribe to).
    publishers: HashMap<u64, Arc<TrackLocalStaticRTP>>,
    /// session → its mute flag (§6.7). The forward loop drops the peer's audio
    /// while set; a moderator's `MUTE` flips it live, and a listen-only join
    /// starts muted. Shared with the forward task so the flip is instant + cheap.
    muted: HashMap<u64, Arc<AtomicBool>>,
}

/// The embedded WebRTC SFU. Cheap to clone the `Arc`; one instance per server.
pub struct WebrtcSfu {
    api: API,
    ice_servers: Vec<String>,
    rooms: Arc<Mutex<HashMap<ChannelName, Room>>>,
}

impl WebrtcSfu {
    /// Build the SFU: one shared `API` (MediaEngine with Opus, default
    /// interceptors, the UDP port range pinned on the SettingEngine).
    pub fn new(config: SfuConfig) -> Result<Self, SfuError> {
        let mut media = MediaEngine::default();
        media.register_codec(
            RTCRtpCodecParameters {
                capability: opus_capability(),
                payload_type: 111,
                ..Default::default()
            },
            RTPCodecType::Audio,
        )?;

        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut media)?;

        let mut settings = SettingEngine::default();
        let mut udp = EphemeralUDP::default();
        udp.set_ports(config.udp_port_min, config.udp_port_max)
            .map_err(|e| SfuError::UdpRange(e.to_string()))?;
        settings.set_udp_network(UDPNetwork::Ephemeral(udp));

        let api = APIBuilder::new()
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .with_setting_engine(settings)
            .build();

        Ok(Self {
            api,
            ice_servers: config.ice_servers,
            rooms: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    fn rtc_config(&self) -> RTCConfiguration {
        RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: self.ice_servers.clone(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// The peer connection for a session in a channel, if it has joined.
    async fn peer(&self, session: u64, channel: &ChannelName) -> Option<Arc<RTCPeerConnection>> {
        let rooms = self.rooms.lock().await;
        rooms.get(channel)?.peers.get(&session).cloned()
    }
}

#[async_trait]
impl VoiceBackend for WebrtcSfu {
    async fn join(&self, req: VoiceJoinReq) -> Result<VoiceGrant, VoiceError> {
        let pc = Arc::new(
            self.api
                .new_peer_connection(self.rtc_config())
                .await
                .map_err(|e| {
                    warn!("voice: peer connection setup failed: {e}");
                    VoiceError::Unavailable
                })?,
        );

        // A listen-only peer (no `speak`) starts muted — its audio is dropped at
        // the SFU regardless of what it sends (invariant 4, server-enforced).
        let muted = Arc::new(AtomicBool::new(!req.can_speak));

        // When this peer's audio arrives, mirror it into a local track the rest
        // of the room subscribes to, and pump its RTP out (unless muted).
        let rooms = Arc::clone(&self.rooms);
        let channel = req.channel.clone();
        let session = req.session;
        let track_muted = Arc::clone(&muted);
        pc.on_track(Box::new(move |track, _receiver, _transceiver| {
            let rooms = Arc::clone(&rooms);
            let channel = channel.clone();
            let muted = Arc::clone(&track_muted);
            Box::pin(async move {
                if track.kind() != RTPCodecType::Audio {
                    return;
                }
                let local = Arc::new(TrackLocalStaticRTP::new(
                    track.codec().capability,
                    format!("audio-{session}"),
                    format!("weft-{session}"),
                ));
                {
                    let mut rooms = rooms.lock().await;
                    if let Some(room) = rooms.get_mut(&channel) {
                        room.publishers.insert(session, Arc::clone(&local));
                    }
                }
                debug!(%channel, session, "voice: publisher track live");
                // Read every packet (to advance the stream) but forward only when
                // not muted — a muted publisher is silenced at the SFU, so the
                // mute is server-enforced, not client-cooperative.
                while let Ok((packet, _)) = track.read_rtp().await {
                    if muted.load(Ordering::Relaxed) {
                        continue;
                    }
                    if local.write_rtp(&packet).await.is_err() {
                        break;
                    }
                }
                // Publisher gone: drop its mirror so no subscriber holds a dead track.
                let mut rooms = rooms.lock().await;
                if let Some(room) = rooms.get_mut(&channel) {
                    room.publishers.remove(&session);
                }
                debug!(%channel, session, "voice: publisher track ended");
            })
        }));

        {
            let mut rooms = self.rooms.lock().await;
            let room = rooms.entry(req.channel.clone()).or_default();
            room.peers.insert(req.session, pc);
            room.muted.insert(req.session, muted);
        }

        // The media token: for the embedded SFU the credential is the session's
        // authenticated control stream (the SDP's ICE ufrag correlates back), so
        // this is an opaque handle, not a bearer.
        let _ = req.account;
        Ok(VoiceGrant {
            token: format!("v{}-{}", req.session, req.channel),
            endpoint: None,
        })
    }

    async fn describe(
        &self,
        session: u64,
        channel: &ChannelName,
        sdp: String,
    ) -> Result<String, VoiceError> {
        let Some(pc) = self.peer(session, channel).await else {
            return Err(VoiceError::Unavailable);
        };

        // Subscribe this peer to every *existing* publisher in the room, adding
        // the forwarding tracks **before** set_remote_description — the sender
        // then binds to the offered m-line and the answer is sendonly. (Adding
        // after set_remote_description leaves the sender paused / unbound.)
        let others: Vec<Arc<TrackLocalStaticRTP>> = {
            let rooms = self.rooms.lock().await;
            rooms
                .get(channel)
                .map(|room| {
                    room.publishers
                        .iter()
                        .filter(|(s, _)| **s != session)
                        .map(|(_, track)| Arc::clone(track))
                        .collect()
                })
                .unwrap_or_default()
        };
        for local in others {
            let sender = pc
                .add_track(local as Arc<dyn TrackLocal + Send + Sync>)
                .await
                .map_err(|_| VoiceError::Unavailable)?;
            // RTCP from the subscriber must be drained or the pipeline stalls.
            tokio::spawn(async move {
                let mut buf = vec![0u8; 1500];
                while sender.read(&mut buf).await.is_ok() {}
            });
        }

        let offer = RTCSessionDescription::offer(sdp).map_err(|_| VoiceError::BadDescription)?;
        pc.set_remote_description(offer)
            .await
            .map_err(|_| VoiceError::BadDescription)?;

        // Non-trickle: gather every candidate into the answer before returning it.
        let answer = pc
            .create_answer(None)
            .await
            .map_err(|_| VoiceError::BadDescription)?;
        let mut gathered = pc.gathering_complete_promise().await;
        pc.set_local_description(answer)
            .await
            .map_err(|_| VoiceError::BadDescription)?;
        let _ = gathered.recv().await;

        let local = pc
            .local_description()
            .await
            .ok_or(VoiceError::BadDescription)?;
        Ok(local.sdp)
    }

    async fn candidate(
        &self,
        session: u64,
        channel: &ChannelName,
        candidate: String,
    ) -> Result<(), VoiceError> {
        let Some(pc) = self.peer(session, channel).await else {
            return Err(VoiceError::Unavailable);
        };
        pc.add_ice_candidate(RTCIceCandidateInit {
            candidate,
            ..Default::default()
        })
        .await
        .map_err(|_| VoiceError::BadDescription)
    }

    async fn leave(&self, session: u64, channel: &ChannelName) {
        let pc = {
            let mut rooms = self.rooms.lock().await;
            let Some(room) = rooms.get_mut(channel) else {
                return;
            };
            room.publishers.remove(&session);
            room.muted.remove(&session);
            let pc = room.peers.remove(&session);
            if room.peers.is_empty() {
                rooms.remove(channel);
            }
            pc
        };
        if let Some(pc) = pc {
            let _ = pc.close().await;
        }
    }

    async fn set_muted(&self, session: u64, channel: &ChannelName, muted: bool) {
        let rooms = self.rooms.lock().await;
        if let Some(flag) = rooms.get(channel).and_then(|r| r.muted.get(&session)) {
            // The forward task reads this every packet — an instant, lock-free flip.
            flag.store(muted, Ordering::Relaxed);
        }
    }
}
