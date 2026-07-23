//! §16 the weftd side of the LiveKit voice backend — the `LiveKitAdmin` port.
//!
//! Two halves, both using LiveKit's own `livekit-api` crate:
//! - **M-lk-0 token minting** — `access_token` builds an HS256 access JWT
//!   (`AccessToken` + `VideoGrants`); pure, no I/O, so it's a sync fn.
//! - **M-lk-2 moderation** — `set_participant_muted` / `remove_participant` call
//!   LiveKit's **Room server API** (`RoomClient`, HTTP over `reqwest` with the
//!   ring rustls provider). A live mute revokes the participant's `can_publish`
//!   via `update_participant` (server-enforced, matching the token grant model);
//!   a ban/kick/leave removes them. Best effort — a transport failure is logged,
//!   not surfaced: the deny-list stays authoritative and is re-applied on the
//!   participant's next join/token refresh.

use std::time::Duration;

use livekit_api::access_token::{AccessToken, VideoGrants};
use livekit_api::services::room::{RoomClient, UpdateParticipantOptions};
use livekit_protocol::ParticipantPermission;
use tracing::{info, warn};
use weft_proto::NetworkName;

use weft_core::{LiveKitAdmin, LiveKitTokenReq, RelaySpec, VoiceRelay};

/// Signs LiveKit access tokens and drives the Room server API for one
/// deployment (all share the API key/secret the operator gives their LiveKit).
pub struct LiveKitSigner {
    api_key: String,
    api_secret: String,
    /// Room server API client (built once; holds a reqwest client).
    room: RoomClient,
}

impl LiveKitSigner {
    pub fn new(api_key: String, api_secret: String, url: &str) -> Self {
        // The Room API is HTTP; the client-facing `url` is a WebSocket URL, so
        // swap the scheme (LiveKit serves both on the same host).
        let host = http_host(url);
        let room = RoomClient::with_api_key(&host, &api_key, &api_secret);
        Self {
            api_key,
            api_secret,
            room,
        }
    }
}

/// `wss://…` → `https://…`, `ws://…` → `http://…`, anything else unchanged.
fn http_host(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = url.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        url.to_string()
    }
}

#[async_trait::async_trait]
impl LiveKitAdmin for LiveKitSigner {
    fn access_token(&self, req: &LiveKitTokenReq) -> String {
        // The WEFT gate already decided publish/subscribe; map them straight onto
        // LiveKit's `VideoGrants`. `room_join` + a set `room` scope the token to
        // exactly this channel's room.
        let grants = VideoGrants {
            room_join: true,
            room: req.room.clone(),
            can_publish: req.can_publish,
            can_subscribe: req.can_subscribe,
            // Data-channel publish (client-side signaling) tracks the same right.
            can_publish_data: req.can_publish,
            ..Default::default()
        };

        // Encoding a valid claim set does not fail in practice (identity is set,
        // as `room_join` requires); degrade to an empty, unusable token rather
        // than panicking the session.
        AccessToken::with_api_key(&self.api_key, &self.api_secret)
            .with_identity(&req.identity)
            .with_name(&req.identity)
            .with_ttl(Duration::from_secs(req.ttl_secs))
            .with_grants(grants)
            .to_jwt()
            .unwrap_or_default()
    }

    async fn set_participant_muted(&self, room: &str, identity: &str, muted: bool) {
        // Revoke (or restore) publish rights — LiveKit unpublishes their tracks
        // server-side, so muting can't be bypassed client-side. Subscribe stays
        // on so a muted participant still hears the room.
        let options = UpdateParticipantOptions {
            permission: Some(ParticipantPermission {
                can_subscribe: true,
                can_publish: !muted,
                can_publish_data: !muted,
                ..Default::default()
            }),
            ..Default::default()
        };

        if let Err(e) = self.room.update_participant(room, identity, options).await {
            warn!(%room, %identity, muted, "livekit mute (update_participant) failed: {e}");
        }
    }

    async fn remove_participant(&self, room: &str, identity: &str) {
        if let Err(e) = self.room.remove_participant(room, identity).await {
            warn!(%room, %identity, "livekit remove_participant failed: {e}");
        }
    }
}

/// §16 M-lk-3b: the default federated-voice relay driver — a **no-op** that logs
/// what it would bridge. It keeps the server complete + honest (the WEFT-side
/// relay lifecycle — refcount, `SEVER`/`NETBLOCK` teardown — all runs) without
/// pulling libwebrtc. The real media relay is a `livekit`-client-SDK (libwebrtc)
/// driver that connects both LiveKit rooms of the [`RelaySpec`] and forwards
/// audio each way — a heavy, platform-specific, deployment-verified dependency,
/// gated separately rather than compiled in by default.
pub struct LogRelay;

#[async_trait::async_trait]
impl VoiceRelay for LogRelay {
    async fn start(&self, spec: RelaySpec) {
        info!(
            peer = %spec.peer,
            key = %spec.key,
            remote_room = %spec.remote_room,
            local_room = %spec.local_room,
            "federated voice relay requested (no media driver installed — see M-lk-3b)"
        );
    }

    async fn stop(&self, peer: &NetworkName, key: &str) {
        info!(%peer, %key, "federated voice relay stopped");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// §16 M-lk-3b: the REAL media relay (feature `voice-relay`, libwebrtc).
//
// A relay is a headless LiveKit participant in BOTH rooms of a `RelaySpec`; it
// subscribes to the audio published in each room and republishes it into the
// other, so the two 1:1-call participants hear each other while each stays
// connected only to ITS OWN network's LiveKit — no client ever touches the peer
// network's LiveKit, protecting client IP addresses. One relay task per
// `(peer, key)`; `stop` aborts it, which disconnects both rooms.
//
// NOTE: this path requires the `livekit` client SDK (libwebrtc) — heavy and
// platform-specific — so it is compiled only under `--features voice-relay` and
// is verified against a real LiveKit deployment, not the default CI build.
#[cfg(feature = "voice-relay")]
mod relay {
    use super::*;
    use livekit::options::TrackPublishOptions;
    use livekit::prelude::*;
    use livekit::track::{LocalAudioTrack, LocalTrack, RemoteTrack, TrackSource};
    use livekit::webrtc::audio_source::native::NativeAudioSource;
    use livekit::webrtc::audio_source::AudioSourceOptions;
    use livekit::webrtc::audio_stream::native::NativeAudioStream;
    use futures_util::StreamExt;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    // Opus is 48 kHz; a call is mono. One relay leg republishes at these.
    const SAMPLE_RATE: u32 = 48_000;
    const CHANNELS: u32 = 1;

    /// The real libwebrtc relay: one bridging task per `(peer, key)`.
    #[derive(Default)]
    pub struct LivekitRelay {
        tasks: Mutex<HashMap<(String, String), tokio::task::JoinHandle<()>>>,
    }

    impl LivekitRelay {
        pub fn new() -> Self {
            Self::default()
        }
    }

    #[async_trait::async_trait]
    impl VoiceRelay for LivekitRelay {
        async fn start(&self, spec: RelaySpec) {
            let key = (spec.peer.to_string(), spec.key.clone());
            let handle = tokio::spawn(async move {
                if let Err(e) = run_relay(spec).await {
                    warn!("voice relay failed: {e}");
                }
            });
            if let Some(old) = self.tasks.lock().await.insert(key, handle) {
                old.abort(); // replace any stale task for the same conversation
            }
        }

        async fn stop(&self, peer: &NetworkName, key: &str) {
            if let Some(handle) = self
                .tasks
                .lock()
                .await
                .remove(&(peer.to_string(), key.to_string()))
            {
                handle.abort(); // dropping the Rooms disconnects both legs
            }
        }
    }

    /// Connect both rooms and forward audio each way until aborted.
    async fn run_relay(spec: RelaySpec) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (remote_room, remote_events) =
            Room::connect(&spec.remote_url, &spec.remote_token, RoomOptions::default()).await?;
        let (local_room, local_events) =
            Room::connect(&spec.local_url, &spec.local_token, RoomOptions::default()).await?;
        let remote_room = Arc::new(remote_room);
        let local_room = Arc::new(local_room);

        // Two independent legs: remote→local and local→remote. Each subscribes in
        // one room and publishes into the other.
        let a = tokio::spawn(forward(remote_events, Arc::clone(&local_room), "r→l"));
        let b = tokio::spawn(forward(local_events, Arc::clone(&remote_room), "l→r"));
        let _ = tokio::join!(a, b);
        Ok(())
    }

    /// For each remote audio track that appears in `events`' room, pump its frames
    /// into a fresh track published in `dest`.
    async fn forward(
        mut events: tokio::sync::mpsc::UnboundedReceiver<RoomEvent>,
        dest: Arc<Room>,
        label: &'static str,
    ) {
        let mut pumps: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        while let Some(event) = events.recv().await {
            if let RoomEvent::TrackSubscribed {
                track: RemoteTrack::Audio(track),
                ..
            } = event
            {
                let dest = Arc::clone(&dest);
                pumps.push(tokio::spawn(async move {
                    if let Err(e) = pump_track(track, dest).await {
                        warn!("relay {label} track ended: {e}");
                    }
                }));
            }
        }
        for p in pumps {
            p.abort();
        }
    }

    /// Publish a new audio track in `dest` and copy every frame of `src` into it.
    async fn pump_track(
        src: livekit::track::RemoteAudioTrack,
        dest: Arc<Room>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let source = NativeAudioSource::new(
            AudioSourceOptions::default(),
            SAMPLE_RATE,
            CHANNELS,
            // buffer_size_ms: a small jitter buffer for the passthrough.
            1000,
        );
        let track = LocalAudioTrack::create_audio_track(
            "relay",
            livekit::webrtc::prelude::RtcAudioSource::Native(source.clone()),
        );
        dest.local_participant()
            .publish_track(
                LocalTrack::Audio(track),
                TrackPublishOptions {
                    source: TrackSource::Microphone,
                    ..Default::default()
                },
            )
            .await?;

        let mut stream =
            NativeAudioStream::new(src.rtc_track(), SAMPLE_RATE as i32, CHANNELS as i32);
        while let Some(frame) = stream.next().await {
            // Republish the decoded frame verbatim (Opus is re-encoded by the SDK).
            source.capture_frame(&frame).await?;
        }
        Ok(())
    }
}

#[cfg(feature = "voice-relay")]
pub use relay::LivekitRelay;
