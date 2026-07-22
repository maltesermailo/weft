//! Native voice (desktop only) — the Tauri binary is the sole LiveKit
//! participant, using the `livekit` Rust SDK + its libwebrtc audio device.
//!
//! Why: on macOS a WKWebView capturing the mic for a call routes through the
//! system *voice-processing* audio unit, which ducks every other app's audio,
//! and there's no web API to opt out. libwebrtc's own audio device module
//! captures via the plain HAL (no ducking) and does noise-suppression / echo-
//! cancellation in software — so moving the mic + playback out of the webview
//! and into this process fixes the ducking while keeping NS/AEC.
//!
//! Phase 1 = audio only (mic publish + auto-played remote audio + roster). The
//! webview drives this via commands and renders the roster from `voice-native-*`
//! events. (Camera/screenshare move here in a later phase.)

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use futures_util::StreamExt;
use livekit::options::TrackPublishOptions;
use livekit::prelude::*;
use livekit::webrtc::native::yuv_helper;
use livekit::webrtc::prelude::RtcVideoTrack;
use livekit::webrtc::video_frame::{I420Buffer, VideoBuffer, VideoFrame, VideoRotation};
use livekit::webrtc::video_source::{native::NativeVideoSource, RtcVideoSource, VideoResolution};
use livekit::webrtc::video_stream::native::NativeVideoStream;
use nokhwa::pixel_format::RgbAFormat;
use nokhwa::utils::{ApiBackend, CameraIndex, RequestedFormat, RequestedFormatType};
use nokhwa::{query, Camera};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use xcap::image::{
    codecs::jpeg::JpegEncoder, imageops, DynamicImage, ExtendedColorType, RgbaImage,
};

/// A running native video publication (screen share or camera).
struct VideoPub {
    sid: TrackSid,
    flag: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

/// One live native voice session (at most one at a time).
struct Session {
    room: Arc<Room>,
    // The ADM handle must outlive the room — dropping it stops capture/playout.
    _audio: PlatformAudio,
    mic: LocalAudioTrack,
    muted: Arc<AtomicBool>,
    task: tauri::async_runtime::JoinHandle<()>,
    screen: Option<VideoPub>,
    camera: Option<VideoPub>,
}

#[derive(Default)]
pub struct NativeVoice(tokio::sync::Mutex<Option<Session>>);

#[derive(Serialize, Clone)]
struct RosterEntry {
    user: String,
    speaking: bool,
    muted: bool,
    #[serde(rename = "cameraOn")]
    camera_on: bool,
    #[serde(rename = "sharingScreen")]
    sharing_screen: bool,
    #[serde(rename = "self")]
    self_: bool,
}

/// Snapshot the room roster for the webview. `local_muted` is authoritative for
/// our own row (the publication's muted flag can lag a local mute). A remote is
/// "muted" if it has no live, unmuted microphone publication.
fn roster(room: &Room, local_muted: bool) -> Vec<RosterEntry> {
    let lp = room.local_participant();
    let lpubs = lp.track_publications();
    let mut out = vec![RosterEntry {
        user: lp.identity().as_str().to_string(),
        speaking: lp.is_speaking(),
        muted: local_muted,
        camera_on: lpubs
            .values()
            .any(|t| t.source() == TrackSource::Camera && !t.is_muted()),
        sharing_screen: lpubs.values().any(|t| t.source() == TrackSource::Screenshare),
        self_: true,
    }];
    for (_id, p) in room.remote_participants() {
        let pubs = p.track_publications();
        let muted = pubs
            .values()
            .find(|t| t.source() == TrackSource::Microphone)
            .map_or(true, |t| t.is_muted());
        out.push(RosterEntry {
            user: p.identity().as_str().to_string(),
            speaking: p.is_speaking(),
            muted,
            camera_on: pubs
                .values()
                .any(|t| t.source() == TrackSource::Camera && !t.is_muted()),
            sharing_screen: pubs.values().any(|t| t.source() == TrackSource::Screenshare),
            self_: false,
        });
    }
    out
}

#[derive(Serialize, Clone)]
struct VideoFrameMsg {
    user: String,
    source: String, // "camera" | "screen"
    data: String,   // data:image/jpeg;base64,…
}

#[derive(Serialize, Clone)]
struct VideoEndMsg {
    user: String,
    source: String,
}

fn source_kind(s: TrackSource) -> &'static str {
    if s == TrackSource::Screenshare {
        "screen"
    } else {
        "camera"
    }
}

fn jpeg_data_url_rgba(rgba: Vec<u8>, w: u32, h: u32, quality: u8) -> Option<String> {
    let img = RgbaImage::from_raw(w, h, rgba)?;
    let rgb = DynamicImage::ImageRgba8(img).to_rgb8();
    let mut buf = Vec::new();
    JpegEncoder::new_with_quality(&mut buf, quality)
        .encode(rgb.as_raw(), w, h, ExtendedColorType::Rgb8)
        .ok()?;
    Some(format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&buf)
    ))
}

/// Read a remote video track, throttle, convert I420→RGBA→JPEG, and push each
/// frame to the webview to render on the participant's tile. (The webview can't
/// render the SDK's decoded frames directly, so we stream them as images.)
async fn remote_video_task(rtc: RtcVideoTrack, user: String, source: String, app: AppHandle) {
    let mut stream = NativeVideoStream::new(rtc);
    let period = Duration::from_millis(60); // ~16 fps to the webview
    let mut last = Instant::now() - period;
    while let Some(frame) = stream.next().await {
        if last.elapsed() < period {
            continue; // drop to throttle
        }
        last = Instant::now();

        let i420 = frame.buffer.to_i420();
        let (w, h) = (i420.width(), i420.height());
        let (y, u, v) = i420.data();
        let (sy, su, sv) = i420.strides();
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        yuv_helper::i420_to_abgr(y, sy, u, su, v, sv, &mut rgba, w * 4, w as i32, h as i32);

        if let Some(data) = jpeg_data_url_rgba(rgba, w, h, 60) {
            let _ = app.emit(
                "voice-native-frame",
                VideoFrameMsg {
                    user: user.clone(),
                    source: source.clone(),
                    data,
                },
            );
        }
    }
}

/// Join a LiveKit room natively and publish the mic. `url`/`token` come from the
/// server's `voice-offer` (same as the web path uses for the JS SDK).
#[tauri::command]
pub async fn voice_native_connect(
    app: AppHandle,
    state: State<'_, NativeVoice>,
    url: String,
    token: String,
) -> Result<(), String> {
    voice_native_disconnect(app.clone(), state.clone()).await.ok();

    let audio = PlatformAudio::new().map_err(|e| format!("audio device: {e}"))?;
    let (room, mut events) = Room::connect(&url, &token, RoomOptions::default())
        .await
        .map_err(|e| format!("connect: {e}"))?;
    // Room isn't Clone; share it with the event task via Arc.
    let room = Arc::new(room);

    // A `Device`-sourced track = capture from libwebrtc's audio device module
    // (HAL, no ducking) with its software NS/AEC. Recording auto-starts on publish.
    let mic = LocalAudioTrack::create_audio_track("microphone", audio.rtc_source());
    room.local_participant()
        .publish_track(
            LocalTrack::Audio(mic.clone()),
            TrackPublishOptions {
                source: TrackSource::Microphone,
                ..Default::default()
            },
        )
        .await
        .map_err(|e| format!("publish mic: {e}"))?;

    let muted = Arc::new(AtomicBool::new(false));

    let task = {
        let room = room.clone();
        let app = app.clone();
        let muted = muted.clone();
        tauri::async_runtime::spawn(async move {
            // Remote video tasks, keyed by publication sid (aborted on unsubscribe).
            let mut video_tasks: HashMap<TrackSid, tauri::async_runtime::JoinHandle<()>> =
                HashMap::new();
            let emit_roster =
                |app: &AppHandle, room: &Room| {
                    let _ = app.emit("voice-native-roster", roster(room, muted.load(Ordering::SeqCst)));
                };

            let _ = app.emit("voice-native-state", "connected");
            emit_roster(&app, &room);

            while let Some(ev) = events.recv().await {
                match ev {
                    RoomEvent::Disconnected { .. } => {
                        let _ = app.emit("voice-native-state", "disconnected");
                        break;
                    }
                    // A remote video track (camera/screen) → stream its frames to
                    // the webview to render.
                    RoomEvent::TrackSubscribed {
                        track: RemoteTrack::Video(vtrack),
                        publication,
                        participant,
                    } => {
                        let user = participant.identity().as_str().to_string();
                        let source = source_kind(publication.source()).to_string();
                        let handle = tauri::async_runtime::spawn(remote_video_task(
                            vtrack.rtc_track(),
                            user,
                            source,
                            app.clone(),
                        ));
                        video_tasks.insert(publication.sid(), handle);
                        emit_roster(&app, &room);
                    }
                    RoomEvent::TrackUnsubscribed {
                        track: RemoteTrack::Video(_),
                        publication,
                        participant,
                    } => {
                        if let Some(h) = video_tasks.remove(&publication.sid()) {
                            h.abort();
                        }
                        let _ = app.emit(
                            "voice-native-frame-end",
                            VideoEndMsg {
                                user: participant.identity().as_str().to_string(),
                                source: source_kind(publication.source()).to_string(),
                            },
                        );
                        emit_roster(&app, &room);
                    }
                    // Other roster-affecting events → re-emit the snapshot.
                    RoomEvent::ParticipantConnected(_)
                    | RoomEvent::ParticipantDisconnected(_)
                    | RoomEvent::ActiveSpeakersChanged { .. }
                    | RoomEvent::TrackMuted { .. }
                    | RoomEvent::TrackUnmuted { .. }
                    | RoomEvent::TrackPublished { .. }
                    | RoomEvent::TrackUnpublished { .. }
                    | RoomEvent::ParticipantsUpdated { .. } => {
                        emit_roster(&app, &room);
                    }
                    _ => {}
                }
            }
            for (_sid, h) in video_tasks {
                h.abort();
            }
        })
    };

    *state.0.lock().await = Some(Session {
        room,
        _audio: audio,
        mic,
        muted,
        task,
        screen: None,
        camera: None,
    });
    Ok(())
}

/// Emit a local self-preview frame (the webview can't see our own published
/// video otherwise). Best-effort + throttled by the caller.
fn emit_self_frame(app: &AppHandle, user: &str, source: &str, img: &RgbaImage) {
    if let Some(data) = jpeg_data_url_rgba(img.as_raw().to_vec(), img.width(), img.height(), 55) {
        let _ = app.emit(
            "voice-native-frame",
            VideoFrameMsg {
                user: user.to_string(),
                source: source.to_string(),
                data,
            },
        );
    }
}

/// Even-dimension down-scale to at most `max_w` wide (I420 chroma wants even).
fn scale_capture(img: RgbaImage, max_w: u32) -> RgbaImage {
    let (w, h) = (img.width(), img.height());
    let tw = (max_w.min(w).max(2)) & !1;
    let th = (((tw as u64 * h as u64) / w.max(1) as u64) as u32).max(2) & !1;
    if tw == w && th == h {
        return img;
    }
    imageops::resize(&img, tw, th, imageops::FilterType::Triangle)
}

/// RGBA (xcap byte order) → I420 for libwebrtc.
fn rgba_to_i420(img: &RgbaImage) -> I420Buffer {
    let (w, h) = (img.width(), img.height());
    let mut buf = I420Buffer::new(w, h);
    let (sy, su, sv) = buf.strides();
    let (dy, du, dv) = buf.data_mut();
    // xcap RGBA (R,G,B,A) == libyuv "ABGR".
    yuv_helper::abgr_to_i420(img.as_raw(), w * 4, dy, sy, du, su, dv, sv, w as i32, h as i32);
    buf
}

/// Publish a native screen capture of `id` (`screen:<id>`/`window:<id>`) to the
/// room — captured + encoded natively (no webview, no canvas hack).
#[tauri::command]
pub async fn voice_native_start_screenshare(
    app: AppHandle,
    state: State<'_, NativeVoice>,
    id: String,
    fps: Option<u32>,
    max_width: Option<u32>,
) -> Result<(), String> {
    let mut guard = state.0.lock().await;
    let session = guard.as_mut().ok_or("not in a voice channel")?;

    if let Some(prev) = session.screen.take() {
        prev.flag.store(false, Ordering::SeqCst);
        let _ = session.room.local_participant().unpublish_track(&prev.sid).await;
        let _ = prev.thread;
    }

    let fps = fps.unwrap_or(15).clamp(1, 60);
    let width = max_width.unwrap_or(1280).clamp(240, 3840);
    let local_id = session.room.local_participant().identity().as_str().to_string();

    let source = NativeVideoSource::new(
        VideoResolution {
            width,
            height: width * 9 / 16,
        },
        true,
    );
    let track = LocalVideoTrack::create_video_track("screen", RtcVideoSource::Native(source.clone()));
    session
        .room
        .local_participant()
        .publish_track(
            LocalTrack::Video(track.clone()),
            TrackPublishOptions {
                source: TrackSource::Screenshare,
                ..Default::default()
            },
        )
        .await
        .map_err(|e| format!("publish screen: {e}"))?;
    let sid = track.sid();

    let flag = Arc::new(AtomicBool::new(true));
    let thread = {
        let flag = flag.clone();
        std::thread::spawn(move || {
            let period = Duration::from_micros(1_000_000 / fps as u64);
            let mut last_preview = Instant::now() - Duration::from_secs(1);
            while flag.load(Ordering::SeqCst) {
                let started = Instant::now();
                if let Some(img) = crate::screencap::grab(&id) {
                    let img = scale_capture(img, width);
                    let frame = VideoFrame::new(VideoRotation::VideoRotation0, rgba_to_i420(&img));
                    source.capture_frame(&frame);
                    // Self-preview (throttled ~8fps), so our own tile shows it.
                    if last_preview.elapsed() >= Duration::from_millis(120) {
                        last_preview = Instant::now();
                        emit_self_frame(&app, &local_id, "screen", &img);
                    }
                }
                let elapsed = started.elapsed();
                if elapsed < period {
                    std::thread::sleep(period - elapsed);
                }
            }
        })
    };

    session.screen = Some(VideoPub { sid, flag, thread });
    Ok(())
}

#[tauri::command]
pub async fn voice_native_stop_screenshare(
    app: AppHandle,
    state: State<'_, NativeVoice>,
) -> Result<(), String> {
    let mut guard = state.0.lock().await;
    if let Some(session) = guard.as_mut() {
        let local_id = session.room.local_participant().identity().as_str().to_string();
        if let Some(prev) = session.screen.take() {
            prev.flag.store(false, Ordering::SeqCst);
            let _ = session.room.local_participant().unpublish_track(&prev.sid).await;
            let _ = prev.thread;
        }
        let _ = app.emit(
            "voice-native-frame-end",
            VideoEndMsg {
                user: local_id,
                source: "screen".into(),
            },
        );
    }
    Ok(())
}

#[derive(Serialize)]
pub struct CameraDevice {
    id: String,
    name: String,
}

/// List cameras for the desktop camera picker.
#[tauri::command]
pub async fn voice_native_list_cameras() -> Result<Vec<CameraDevice>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        query(ApiBackend::Auto)
            .map(|cams| {
                cams.into_iter()
                    .map(|c| CameraDevice {
                        id: match c.index() {
                            CameraIndex::Index(i) => i.to_string(),
                            CameraIndex::String(s) => s.clone(),
                        },
                        name: c.human_name(),
                    })
                    .collect()
            })
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Start the camera (chosen device or default) and publish it to the room.
#[tauri::command]
pub async fn voice_native_start_camera(
    app: AppHandle,
    state: State<'_, NativeVoice>,
    device_id: Option<String>,
) -> Result<(), String> {
    let mut guard = state.0.lock().await;
    let session = guard.as_mut().ok_or("not in a voice channel")?;

    if let Some(prev) = session.camera.take() {
        prev.flag.store(false, Ordering::SeqCst);
        let _ = session.room.local_participant().unpublish_track(&prev.sid).await;
        let _ = prev.thread;
    }

    let local_id = session.room.local_participant().identity().as_str().to_string();
    let index = match device_id.as_deref() {
        Some(s) => s
            .parse::<u32>()
            .map(CameraIndex::Index)
            .unwrap_or_else(|_| CameraIndex::String(s.to_string())),
        None => CameraIndex::Index(0),
    };

    let source = NativeVideoSource::new(VideoResolution { width: 1280, height: 720 }, false);
    let track = LocalVideoTrack::create_video_track("camera", RtcVideoSource::Native(source.clone()));
    session
        .room
        .local_participant()
        .publish_track(
            LocalTrack::Video(track.clone()),
            TrackPublishOptions {
                source: TrackSource::Camera,
                ..Default::default()
            },
        )
        .await
        .map_err(|e| format!("publish camera: {e}"))?;
    let sid = track.sid();

    let flag = Arc::new(AtomicBool::new(true));
    let flag2 = flag.clone();
    // Open the camera on the capture thread (the handle isn't Send).
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let thread = std::thread::spawn(move || {
        let mut cam = match Camera::new(
            index,
            RequestedFormat::new::<RgbAFormat>(RequestedFormatType::AbsoluteHighestResolution),
        )
        .and_then(|mut c| c.open_stream().map(|_| c))
        {
            Ok(c) => {
                let _ = ready_tx.send(Ok(()));
                c
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e.to_string()));
                return;
            }
        };
        let mut last_preview = Instant::now() - Duration::from_secs(1);
        while flag2.load(Ordering::SeqCst) {
            let Ok(buf) = cam.frame() else { continue };
            let Ok(decoded) = buf.decode_image::<RgbAFormat>() else {
                continue;
            };
            let (w, h) = (decoded.width(), decoded.height());
            let Some(raw) = RgbaImage::from_raw(w, h, decoded.into_raw()) else {
                continue;
            };
            let img = scale_capture(raw, 1280);
            let frame = VideoFrame::new(VideoRotation::VideoRotation0, rgba_to_i420(&img));
            source.capture_frame(&frame);
            if last_preview.elapsed() >= Duration::from_millis(120) {
                last_preview = Instant::now();
                emit_self_frame(&app, &local_id, "camera", &img);
            }
        }
    });

    // Surface an open failure (permission denied / no camera) to the caller.
    if let Ok(Err(e)) = ready_rx.recv() {
        flag.store(false, Ordering::SeqCst);
        let _ = session.room.local_participant().unpublish_track(&sid).await;
        return Err(e);
    }

    session.camera = Some(VideoPub { sid, flag, thread });
    Ok(())
}

#[tauri::command]
pub async fn voice_native_stop_camera(
    app: AppHandle,
    state: State<'_, NativeVoice>,
) -> Result<(), String> {
    let mut guard = state.0.lock().await;
    if let Some(session) = guard.as_mut() {
        let local_id = session.room.local_participant().identity().as_str().to_string();
        if let Some(prev) = session.camera.take() {
            prev.flag.store(false, Ordering::SeqCst);
            let _ = session.room.local_participant().unpublish_track(&prev.sid).await;
            let _ = prev.thread;
        }
        let _ = app.emit(
            "voice-native-frame-end",
            VideoEndMsg {
                user: local_id,
                source: "camera".into(),
            },
        );
    }
    Ok(())
}

#[tauri::command]
pub async fn voice_native_set_muted(
    app: AppHandle,
    state: State<'_, NativeVoice>,
    muted: bool,
) -> Result<(), String> {
    let guard = state.0.lock().await;
    if let Some(s) = guard.as_ref() {
        s.muted.store(muted, Ordering::SeqCst);
        if muted {
            s.mic.mute();
        } else {
            s.mic.unmute();
        }
        let _ = app.emit("voice-native-roster", roster(&s.room, muted));
    }
    Ok(())
}

#[tauri::command]
pub async fn voice_native_disconnect(
    _app: AppHandle,
    state: State<'_, NativeVoice>,
) -> Result<(), String> {
    if let Some(mut s) = state.0.lock().await.take() {
        if let Some(screen) = s.screen.take() {
            screen.flag.store(false, Ordering::SeqCst);
        }
        if let Some(camera) = s.camera.take() {
            camera.flag.store(false, Ordering::SeqCst);
        }
        s.task.abort();
        let _ = s.room.close().await;
    }
    Ok(())
}
