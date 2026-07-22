//! Native screen/window enumeration + capture for the Discord-style screenshare
//! picker. An embedded webview can't enumerate sources or bypass the OS picker,
//! so we do it natively with `xcap`:
//!
//!   • `list_capture_sources` returns source *metadata* only — fast, so the grid
//!     appears instantly.
//!   • `capture_source_thumb` grabs one thumbnail; the client loads these lazily
//!     (a slow or permission-blocked capture then only affects its own tile,
//!     never the whole list).
//!   • `start_capture` streams JPEG frames of the chosen source to the webview
//!     over a Tauri `Channel`; the client draws them to a `<canvas>` and
//!     publishes `canvas.captureStream()` to LiveKit.
//!
//! Software capture (per-frame grab + JPEG) — modest fps, but no native LiveKit
//! SDK required. One capture at a time.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use serde::Serialize;
use tauri::ipc::Channel;
use xcap::image::{
    codecs::jpeg::JpegEncoder, imageops, DynamicImage, ExtendedColorType, RgbaImage,
};
use xcap::{Monitor, Window};

/// Shared "is a capture running" flag, managed by Tauri.
#[derive(Default)]
pub struct CaptureState {
    running: Mutex<Option<Arc<AtomicBool>>>,
}

#[derive(Serialize)]
pub struct CaptureSource {
    /// `screen:<id>` or `window:<id>`.
    id: String,
    kind: String,
    title: String,
    app: String,
}

/// JPEG-encode an RGBA image (down-scaled to `max_w`) as a base64 data URL.
fn jpeg_data_url(img: &RgbaImage, max_w: u32, quality: u8) -> Option<String> {
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return None;
    }
    let tw = max_w.min(w).max(1);
    let th = ((tw as u64 * h as u64) / w as u64).max(1) as u32;
    let small = imageops::thumbnail(img, tw, th);
    // JPEG has no alpha — drop it.
    let rgb = DynamicImage::ImageRgba8(small).to_rgb8();

    let mut buf = Vec::new();
    JpegEncoder::new_with_quality(&mut buf, quality)
        .encode(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            ExtendedColorType::Rgb8,
        )
        .ok()?;
    Some(format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&buf)
    ))
}

/// Enumerate monitors + shareable windows — metadata only (no capture), so this
/// returns immediately. Thumbnails come from `capture_source_thumb`.
#[tauri::command]
pub async fn list_capture_sources() -> Result<Vec<CaptureSource>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let mut out = Vec::new();

        if let Ok(monitors) = Monitor::all() {
            for m in monitors {
                let id = m.id().unwrap_or(0);
                out.push(CaptureSource {
                    id: format!("screen:{id}"),
                    kind: "screen".into(),
                    title: m.name().unwrap_or_else(|_| "Display".into()),
                    app: "Screen".into(),
                });
            }
        }

        if let Ok(windows) = Window::all() {
            for w in windows {
                if w.is_minimized().unwrap_or(false) {
                    continue;
                }
                let (width, height) = (w.width().unwrap_or(0), w.height().unwrap_or(0));
                if width < 64 || height < 64 {
                    continue;
                }
                let title = w.title().unwrap_or_default();
                let app = w.app_name().unwrap_or_default();
                if title.is_empty() && app.is_empty() {
                    continue;
                }
                let id = w.id().unwrap_or(0);
                out.push(CaptureSource {
                    id: format!("window:{id}"),
                    kind: "window".into(),
                    title: if title.is_empty() { app.clone() } else { title },
                    app,
                });
            }
        }

        out
    })
    .await
    .map_err(|e| e.to_string())
}

/// Capture a single thumbnail (base64 JPEG data URL) for one source, or "" if it
/// can't be captured (e.g. Screen-Recording permission not yet granted).
#[tauri::command]
pub async fn capture_source_thumb(id: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        grab(&id)
            .and_then(|img| jpeg_data_url(&img, 320, 70))
            .unwrap_or_default()
    })
    .await
    .map_err(|e| e.to_string())
}

/// Capture one frame of `screen:<id>` / `window:<id>`. Shared with the native
/// voice module, which publishes these frames to LiveKit.
pub(crate) fn grab(id: &str) -> Option<RgbaImage> {
    let (kind, raw) = id.split_once(':')?;
    let num: u32 = raw.parse().ok()?;
    match kind {
        "screen" => Monitor::all()
            .ok()?
            .into_iter()
            .find(|m| m.id().ok() == Some(num))?
            .capture_image()
            .ok(),
        "window" => Window::all()
            .ok()?
            .into_iter()
            .find(|w| w.id().ok() == Some(num))?
            .capture_image()
            .ok(),
        _ => None,
    }
}

fn stop_running(state: &CaptureState) {
    if let Some(flag) = state.running.lock().unwrap().take() {
        flag.store(false, Ordering::SeqCst);
    }
}

/// Start streaming JPEG frames of `id` to `on_frame` (base64 data URLs) until
/// `stop_capture`. `fps` is the target frame rate and `max_width` bounds the
/// encoded resolution (both best-effort — the real rate is bounded by how fast
/// the OS grabs + we encode a frame). The loop is paced against the capture time
/// so a fast source actually reaches the target rate.
#[tauri::command]
pub async fn start_capture(
    id: String,
    fps: Option<u32>,
    max_width: Option<u32>,
    on_frame: Channel<String>,
    state: tauri::State<'_, CaptureState>,
) -> Result<(), String> {
    stop_running(&state);

    let flag = Arc::new(AtomicBool::new(true));
    *state.running.lock().unwrap() = Some(flag.clone());

    let fps = fps.unwrap_or(15).clamp(1, 60);
    let period = Duration::from_micros(1_000_000 / fps as u64);
    let width = max_width.unwrap_or(1280).clamp(240, 3840);

    std::thread::spawn(move || {
        while flag.load(Ordering::SeqCst) {
            let started = Instant::now();
            if let Some(img) = grab(&id) {
                if let Some(url) = jpeg_data_url(&img, width, 55) {
                    if on_frame.send(url).is_err() {
                        break; // the webview went away
                    }
                }
            }
            // Sleep only for the remainder of the frame period, so the capture +
            // encode time counts toward it (rather than being pure overhead).
            let elapsed = started.elapsed();
            if elapsed < period {
                std::thread::sleep(period - elapsed);
            }
        }
    });

    Ok(())
}

#[tauri::command]
pub fn stop_capture(state: tauri::State<'_, CaptureState>) {
    stop_running(&state);
}
