//! §16 M-lk-0: the weftd side of the LiveKit voice backend — the `LiveKitAdmin`
//! port implementation.
//!
//! weft-core's [`LiveKitBackend`](weft_core::LiveKitBackend) runs the WEFT authz
//! gate and then asks this signer for a media credential. Minting uses LiveKit's
//! own [`livekit_api::access_token`] builder (an HS256 JWT over the shared
//! `api_secret`, `video` grant = room + publish/subscribe), so it stays pure —
//! no I/O — which is why the port can be called synchronously from core. The
//! M-lk-2 Room server API (mute/remove) will add the async, HTTP half here via
//! `livekit_api::services`.

use std::time::Duration;

use livekit_api::access_token::{AccessToken, VideoGrants};

use weft_core::{LiveKitAdmin, LiveKitTokenReq};

/// Signs LiveKit access tokens with the deployment's shared API key/secret.
pub struct LiveKitSigner {
    api_key: String,
    api_secret: String,
}

impl LiveKitSigner {
    pub fn new(api_key: String, api_secret: String) -> Self {
        Self {
            api_key,
            api_secret,
        }
    }
}

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
}
