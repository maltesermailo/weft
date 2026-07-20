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
use weft_proto::{ChannelName, NetworkName};

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
            channel = %spec.channel,
            remote_room = %spec.remote_room,
            local_room = %spec.local_room,
            "federated voice relay requested (no media driver installed — see M-lk-3b)"
        );
    }

    async fn stop(&self, peer: &NetworkName, channel: &ChannelName) {
        info!(%peer, %channel, "federated voice relay stopped");
    }
}
