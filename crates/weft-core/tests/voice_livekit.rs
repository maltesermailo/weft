//! §16 M-lk-2: the LiveKit voice backend maps `VoiceBackend` moderation onto the
//! LiveKit Room server API. Driven by a `MockLiveKitAdmin` that records calls, so
//! the cap→Room-API mapping is verified with no HTTP / real LiveKit.

use std::sync::Mutex;

use weft_core::{LiveKitAdmin, LiveKitBackend, LiveKitTokenReq, VoiceBackend, VoiceJoinReq};
use weft_proto::VoiceTransport;

/// Records every Room-API call the backend makes.
#[derive(Default)]
struct MockLiveKitAdmin {
    muted: Mutex<Vec<(String, String, bool)>>, // (room, identity, muted)
    removed: Mutex<Vec<(String, String)>>,     // (room, identity)
}

#[async_trait::async_trait]
impl LiveKitAdmin for MockLiveKitAdmin {
    fn access_token(&self, req: &LiveKitTokenReq) -> String {
        format!("tok:{}:{}:{}", req.room, req.identity, req.can_publish)
    }
    async fn set_participant_muted(&self, room: &str, identity: &str, muted: bool) {
        self.muted
            .lock()
            .unwrap()
            .push((room.to_string(), identity.to_string(), muted));
    }
    async fn remove_participant(&self, room: &str, identity: &str) {
        self.removed
            .lock()
            .unwrap()
            .push((room.to_string(), identity.to_string()));
    }
}

#[tokio::test]
async fn livekit_backend_routes_moderation_to_the_room_api() {
    let admin = std::sync::Arc::new(MockLiveKitAdmin::default());
    let backend = LiveKitBackend::new(
        admin.clone(),
        "wss://lk.example".to_string(),
        "hda.example".parse().unwrap(),
        600,
    );

    let chan: weft_proto::ChannelName = "#lounge".parse().unwrap();
    let grant = backend
        .join(VoiceJoinReq {
            channel: chan.clone(),
            account: "ada".parse().unwrap(),
            session: 7,
            can_speak: true,
        })
        .await
        .expect("join");

    // The offer points at LiveKit with the room id + server URL.
    assert_eq!(grant.mode, VoiceTransport::Livekit);
    assert_eq!(grant.room.as_deref(), Some("wv:hda.example:#lounge"));
    assert_eq!(grant.endpoint.as_deref(), Some("wss://lk.example"));

    // A moderator MUTE / UNMUTE → mute_published_track on this participant's
    // room + identity, in order.
    backend.set_muted(7, &chan, true).await;
    backend.set_muted(7, &chan, false).await;
    assert_eq!(
        *admin.muted.lock().unwrap(),
        vec![
            (
                "wv:hda.example:#lounge".into(),
                "ada@hda.example".into(),
                true
            ),
            (
                "wv:hda.example:#lounge".into(),
                "ada@hda.example".into(),
                false
            ),
        ]
    );

    // set_muted for an unknown session is a no-op (no Room-API call).
    backend.set_muted(99, &chan, true).await;
    assert_eq!(admin.muted.lock().unwrap().len(), 2);

    // Ban / kick / disconnect → remove_participant, and the peer is forgotten.
    backend.leave(7, &chan).await;
    assert_eq!(
        *admin.removed.lock().unwrap(),
        vec![("wv:hda.example:#lounge".into(), "ada@hda.example".into())]
    );

    // After leaving, a second leave and a stray mute are both no-ops (the
    // session is no longer a known LiveKit peer).
    backend.leave(7, &chan).await;
    backend.set_muted(7, &chan, true).await;
    assert_eq!(admin.removed.lock().unwrap().len(), 1);
    assert_eq!(admin.muted.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn room_grant_mints_a_credential_for_an_ad_hoc_call_room() {
    let admin = std::sync::Arc::new(MockLiveKitAdmin::default());
    let backend = LiveKitBackend::new(
        admin.clone(),
        "wss://lk.example".to_string(),
        "hda.example".parse().unwrap(),
        600,
    );

    // A friend-call room id is opaque and used verbatim as the LiveKit room —
    // not run through `livekit_room` (which prefixes `wv:<net>:`).
    let grant = backend
        .room_grant("call:01ARZ", &"ada".parse().unwrap(), true)
        .await
        .expect("livekit backend serves room grants");

    assert_eq!(grant.mode, VoiceTransport::Livekit);
    assert_eq!(grant.room.as_deref(), Some("call:01ARZ"));
    assert_eq!(grant.endpoint.as_deref(), Some("wss://lk.example"));
    // Identity is the canonical user@network; publish follows can_speak.
    assert_eq!(grant.token, "tok:call:01ARZ:ada@hda.example:true");

    // A listen-only grant maps to canPublish=false.
    let muted = backend
        .room_grant("call:01ARZ", &"bob".parse().unwrap(), false)
        .await
        .unwrap();
    assert_eq!(muted.token, "tok:call:01ARZ:bob@hda.example:false");

    // room_grant never touches the per-session peer map (no moderation calls).
    assert!(admin.muted.lock().unwrap().is_empty());
    assert!(admin.removed.lock().unwrap().is_empty());
}
