//! §16 SFU integration tests over real webrtc: a client `RTCPeerConnection`
//! negotiates through the `VoiceBackend` API, and audio forwards publisher →
//! subscriber. ICE uses host candidates only (empty STUN list) so the tests run
//! offline and fast on localhost loopback.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;

use weft_core::{VoiceBackend, VoiceJoinReq};
use weft_proto::ChannelName;
use weft_rt::{SfuConfig, WebrtcSfu};

use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_OPUS};
use webrtc::api::{APIBuilder, API};
use webrtc::interceptor::registry::Registry;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTPCodecType};
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;

/// A test-side WebRTC client (a browser stand-in).
fn client_api() -> API {
    let mut media = MediaEngine::default();
    media
        .register_default_codecs()
        .expect("register default codecs");
    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut media).expect("interceptors");
    APIBuilder::new()
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .build()
}

/// Host-candidates-only config over a caller-chosen UDP range — no STUN, so
/// gathering is instant + offline. Distinct ranges per test so the SFUs (which
/// run concurrently under `cargo test`) never contend for the same ports.
fn offline_config(port_min: u16, port_max: u16) -> SfuConfig {
    SfuConfig {
        ice_servers: vec![],
        udp_port_min: port_min,
        udp_port_max: port_max,
    }
}

fn client_config() -> RTCConfiguration {
    RTCConfiguration::default()
}

fn opus() -> RTCRtpCodecCapability {
    RTCRtpCodecCapability {
        mime_type: MIME_TYPE_OPUS.to_owned(),
        clock_rate: 48000,
        channels: 2,
        ..Default::default()
    }
}

/// Create the client's non-trickle offer (gather all candidates first).
async fn gathered_offer(pc: &RTCPeerConnection) -> String {
    let offer = pc.create_offer(None).await.expect("create offer");
    let mut done = pc.gathering_complete_promise().await;
    pc.set_local_description(offer).await.expect("set local");
    let _ = done.recv().await;
    pc.local_description().await.expect("local desc").sdp
}

#[tokio::test]
async fn sfu_answers_a_client_offer_with_gathered_candidates() {
    let sfu = WebrtcSfu::new(offline_config(41000, 41099)).expect("sfu");
    let channel: ChannelName = "#general".parse().unwrap();

    let api = client_api();
    let pc = api
        .new_peer_connection(client_config())
        .await
        .expect("client pc");
    // A sendrecv audio m-line so the offer carries Opus.
    let track: Arc<dyn TrackLocal + Send + Sync> = Arc::new(TrackLocalStaticSample::new(
        opus(),
        "audio".into(),
        "mic".into(),
    ));
    pc.add_track(track).await.expect("add track");

    sfu.join(VoiceJoinReq {
        channel: channel.clone(),
        account: "alice".parse().unwrap(),
        session: 1,
        can_speak: true,
    })
    .await
    .expect("join");

    let offer = gathered_offer(&pc).await;
    let answer = sfu.describe(1, &channel, offer).await.expect("describe");

    assert!(
        answer.contains("m=audio"),
        "answer has an audio m-line:\n{answer}"
    );
    assert!(
        answer.contains("a=candidate"),
        "answer carries gathered ICE candidates (non-trickle):\n{answer}"
    );
    // The client accepts the SFU's answer — proves it's a well-formed answer.
    pc.set_remote_description(RTCSessionDescription::answer(answer).expect("answer sdp"))
        .await
        .expect("client accepts answer");

    sfu.leave(1, &channel).await;
}

#[tokio::test]
async fn sfu_forwards_opus_from_publisher_to_subscriber() {
    let sfu = Arc::new(WebrtcSfu::new(offline_config(41100, 41199)).expect("sfu"));
    let channel: ChannelName = "#general".parse().unwrap();

    // --- publisher: sends Opus samples into the room ---
    let pub_api = client_api();
    let pub_pc = pub_api
        .new_peer_connection(client_config())
        .await
        .expect("pub pc");
    let mic = Arc::new(TrackLocalStaticSample::new(
        opus(),
        "audio".into(),
        "mic".into(),
    ));
    let mic_dyn: Arc<dyn TrackLocal + Send + Sync> = mic.clone();
    pub_pc.add_track(mic_dyn).await.expect("add mic");

    // Signal once the publisher's connection is up (no fixed sleep to guess at).
    let (up_tx, up_rx) = oneshot::channel::<()>();
    let up_tx = Arc::new(tokio::sync::Mutex::new(Some(up_tx)));
    pub_pc.on_peer_connection_state_change(Box::new(move |state| {
        let up_tx = Arc::clone(&up_tx);
        Box::pin(async move {
            if state == RTCPeerConnectionState::Connected {
                if let Some(tx) = up_tx.lock().await.take() {
                    let _ = tx.send(());
                }
            }
        })
    }));

    sfu.join(VoiceJoinReq {
        channel: channel.clone(),
        account: "alice".parse().unwrap(),
        session: 1,
        can_speak: true,
    })
    .await
    .expect("pub join");
    let offer = gathered_offer(&pub_pc).await;
    let answer = sfu
        .describe(1, &channel, offer)
        .await
        .expect("pub describe");
    pub_pc
        .set_remote_description(RTCSessionDescription::answer(answer).unwrap())
        .await
        .expect("pub accepts answer");

    // Pump silence frames forever so the publisher's track stays live on the SFU
    // (don't stop on a pre-connection error — keep trying until the test ends).
    tokio::spawn(async move {
        loop {
            let _ = mic
                .write_sample(&Sample {
                    data: vec![0x80; 60].into(),
                    duration: Duration::from_millis(20),
                    ..Default::default()
                })
                .await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });

    // Wait for the publisher to actually connect + its track to register on the
    // SFU (a short buffer after Connected for `on_track` to fire).
    tokio::time::timeout(Duration::from_secs(10), up_rx)
        .await
        .expect("publisher never connected")
        .expect("state sender dropped");
    tokio::time::sleep(Duration::from_secs(3)).await;

    // --- subscriber: should receive the publisher's RTP via the SFU ---
    let sub_api = client_api();
    let sub_pc = sub_api
        .new_peer_connection(client_config())
        .await
        .expect("sub pc");
    sub_pc
        .add_transceiver_from_kind(
            RTPCodecType::Audio,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                send_encodings: vec![],
            }),
        )
        .await
        .expect("recvonly transceiver");

    let (tx, rx) = oneshot::channel::<()>();
    let tx = Arc::new(tokio::sync::Mutex::new(Some(tx)));
    sub_pc.on_track(Box::new(move |track, _, _| {
        let tx = Arc::clone(&tx);
        Box::pin(async move {
            // Fire once on the first forwarded RTP packet.
            if track.read_rtp().await.is_ok() {
                if let Some(tx) = tx.lock().await.take() {
                    let _ = tx.send(());
                }
            }
        })
    }));
    let (sub_up_tx, sub_up_rx) = oneshot::channel::<()>();
    let sub_up_tx = Arc::new(tokio::sync::Mutex::new(Some(sub_up_tx)));
    sub_pc.on_peer_connection_state_change(Box::new(move |state| {
        let sub_up_tx = Arc::clone(&sub_up_tx);
        Box::pin(async move {
            if state == RTCPeerConnectionState::Connected {
                if let Some(tx) = sub_up_tx.lock().await.take() {
                    let _ = tx.send(());
                }
            }
        })
    }));

    sfu.join(VoiceJoinReq {
        channel: channel.clone(),
        account: "bob".parse().unwrap(),
        session: 2,
        can_speak: false,
    })
    .await
    .expect("sub join");
    let offer = gathered_offer(&sub_pc).await;
    let answer = sfu
        .describe(2, &channel, offer)
        .await
        .expect("sub describe");
    assert!(
        answer.matches("m=audio").count() >= 1,
        "subscriber answer carries the forwarded audio:\n{answer}"
    );
    sub_pc
        .set_remote_description(RTCSessionDescription::answer(answer).unwrap())
        .await
        .expect("sub accepts answer");

    tokio::time::timeout(Duration::from_secs(10), sub_up_rx)
        .await
        .expect("subscriber never connected")
        .expect("sub state sender dropped");

    // The forwarded Opus should reach the subscriber.
    tokio::time::timeout(Duration::from_secs(10), rx)
        .await
        .expect("subscriber received no forwarded RTP within 10s")
        .expect("sender dropped");

    sfu.leave(1, &channel).await;
    sfu.leave(2, &channel).await;
}
