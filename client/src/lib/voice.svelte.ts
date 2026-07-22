// §16 voice — the browser side. Signaling always rides the WEFT control stream
// (`voice*` commands + `voice-*` events); the media plane depends on the server's
// backend, carried by the `voice-offer`'s `mode`:
//
//   • "webrtc"  — the embedded WEFT-RT SFU. We getUserMedia + build a non-trickle
//     offer → `voiceDesc` → the SFU answers with `voice-desc` → Opus both ways.
//     (Non-trickle: candidates ride the SDP, so we gather fully before sending.)
//   • "livekit" — an external LiveKit server. The token is a LiveKit access JWT
//     and the endpoint is the LiveKit URL; we connect the LiveKit SDK's `Room`,
//     which handles publish/subscribe, renegotiation, active-speaker, and
//     quality. The SDK is dynamically imported so it loads only on this path.
//
// One voice room at a time.

import { voiceJoin, voiceLeave, voiceDesc, voiceCand, onWeft, type WeftEvent } from "./weft";
import { invoke as tauriInvoke, Channel } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { Room, Participant, Track, AudioCaptureOptions } from "livekit-client";

export type VoiceParticipant = {
  user: string;
  speaking: boolean;
  muted: boolean;
  deaf: boolean;
  self: boolean;
  /** Publishing a camera track (LiveKit path). */
  cameraOn?: boolean;
  /** Publishing a screenshare track (LiveKit path). */
  sharingScreen?: boolean;
};

type VoiceModel = {
  /** The joined voice channel, or null when not in a voice room. */
  channel: string | null;
  /** True from `joinVoice` until the peer connection is negotiated. */
  connecting: boolean;
  /** Local microphone mute. */
  muted: boolean;
  /** Deafen: all incoming audio silenced (implies muted while active). */
  deafened: boolean;
  /** Local camera published (LiveKit path). */
  cameraOn: boolean;
  /** Local screen share published (LiveKit path). */
  sharingScreen: boolean;
  /** Room roster keyed by account. */
  participants: Record<string, VoiceParticipant>;
  /** Bumped on every video-track change so the stage re-attaches its <video>s.
   *  (The actual LiveKit tracks live outside `$state` — see `videoTracks`.) */
  mediaTick: number;
  /** Last error (mic denied, a rejected join, …); shown then cleared by the UI. */
  error: string | null;
};

export const voice = $state<VoiceModel>({
  channel: null,
  connecting: false,
  muted: false,
  deafened: false,
  cameraOn: false,
  sharingScreen: false,
  participants: {},
  mediaTick: 0,
  error: null,
});

/** Live presence for *every* voice channel we can see (Discord-style), keyed by
 *  channel then account. Populated from server `voice-state` pushes — the roster
 *  shows under a voice channel even when we haven't joined. The channel we're in
 *  is also mirrored into `voice.participants` (which the LiveKit path drives). */
export const voiceRosters = $state<Record<string, Record<string, VoiceParticipant>>>({});

const RTC_CONFIG: RTCConfiguration = {
  iceServers: [{ urls: "stun:stun.l.google.com:19302" }],
};

let account = "";
let subscribed = false;

// Mic state captured when deafening, so un-deafen can restore it (deafen mutes).
let micBeforeDeafen = false;

// webrtc-path media state.
let pc: RTCPeerConnection | null = null;
let localStream: MediaStream | null = null;
let audioEl: HTMLAudioElement | null = null;

// livekit-path media state. `room` non-null ⇒ we're on the LiveKit path. Self is
// identified by `participant.isLocal`; remote identities are the `user@network`
// the token set, so the roster matches the WebRTC path's federated keys.
let room: Room | null = null;
const attached = new Set<HTMLMediaElement>();

// LiveKit video tracks (camera + screenshare), kept OUT of `$state` so Svelte's
// reactive proxy never wraps a LiveKit Track (which would break its methods and
// identity). The UI observes presence via the roster flags + `voice.mediaTick`
// and attaches the raw track to a <video> via `attachVideo`.
const videoTracks = new Map<string, { camera?: Track; screen?: Track }>();

function setVideoTrack(user: string, source: "camera" | "screen", track: Track): void {
  const slot = videoTracks.get(user) ?? {};
  slot[source] = track;
  videoTracks.set(user, slot);
}
function clearVideoTrack(user: string, source: "camera" | "screen"): void {
  const slot = videoTracks.get(user);
  if (!slot) return;
  delete slot[source];
  if (!slot.camera && !slot.screen) videoTracks.delete(user);
}

// On desktop, the LiveKit connection lives in the Tauri binary (the sole
// participant — fixes macOS mic ducking). `nativeActive` marks that path so
// mute/leave route to the native commands instead of the webview SDK.
let nativeActive = false;

// Desktop remote video: the Rust side streams each remote camera/screen track as
// JPEG frames (the webview can't render the SDK's decoded frames). Keyed by
// `${user}|${source}` → data URL, consumed by VoiceStage tiles.
const nativeVideo = $state<Record<string, string>>({});
export function nativeVideoUrl(user: string, source: "camera" | "screen"): string | undefined {
  return nativeVideo[`${user}|${source}`];
}

/** Wire the voice event handler once, and record who "we" are (for the roster).
 *  Call on connect. */
export function initVoice(myAccount: string): void {
  account = myAccount;
  if (!subscribed) {
    subscribed = true;
    void onWeft(onVoiceEvent);

    // Desktop: the native voice session pushes roster + connection state + remote
    // video frames here.
    if (IS_DESKTOP) {
      type RosterEntry = {
        user: string;
        speaking: boolean;
        muted: boolean;
        cameraOn: boolean;
        sharingScreen: boolean;
        self: boolean;
      };
      void listen<RosterEntry[]>("voice-native-roster", (e) => {
        const parts: Record<string, VoiceParticipant> = {};
        for (const r of e.payload) {
          parts[r.user] = {
            user: r.user,
            speaking: r.speaking,
            muted: r.muted,
            deaf: false,
            self: r.self,
            cameraOn: r.cameraOn,
            sharingScreen: r.sharingScreen,
          };
        }
        voice.participants = parts;
        voice.connecting = false;
      });
      void listen<string>("voice-native-state", (e) => {
        if (e.payload === "connected") voice.connecting = false;
        else if (e.payload === "disconnected" && nativeActive) leaveVoice();
      });
      void listen<{ user: string; source: string; data: string }>("voice-native-frame", (e) => {
        nativeVideo[`${e.payload.user}|${e.payload.source}`] = e.payload.data;
      });
      void listen<{ user: string; source: string }>("voice-native-frame-end", (e) => {
        delete nativeVideo[`${e.payload.user}|${e.payload.source}`];
      });
    }
  }
}

/** Join a channel's voice room. Mic permission is requested only once the
 *  server has authorized the join (on the `voice-offer`). */
export async function joinVoice(channel: string): Promise<void> {
  if (voice.channel) await leaveVoice();
  voice.error = null;
  voice.connecting = true;
  voice.channel = channel;
  try {
    await voiceJoin(channel);
  } catch (err) {
    voice.error = String(err);
    teardown();
  }
}

/** Leave the current voice room and tear the peer connection down. */
export async function leaveVoice(): Promise<void> {
  const chan = voice.channel;
  teardown();
  if (chan) {
    try {
      await voiceLeave(chan);
    } catch {
      /* already gone */
    }
  }
}

/** Toggle the local microphone. On LiveKit this (un)publishes the mic track and
 *  the roster updates via the resulting `TrackMuted`/`TrackUnmuted`; on WebRTC
 *  it disables the local track (server-enforced mute is M-voice-4 / M-lk-2). */
export function toggleMute(): void {
  voice.muted = !voice.muted;
  if (nativeActive) {
    void tauriInvoke("voice_native_set_muted", { muted: voice.muted });
    return;
  }
  if (room) {
    void room.localParticipant.setMicrophoneEnabled(!voice.muted);
    return;
  }
  if (localStream) {
    for (const t of localStream.getAudioTracks()) t.enabled = !voice.muted;
  }
  const me = voice.participants[account];
  if (me) me.muted = voice.muted;
}

/** Toggle deafen: silence every incoming stream so you hear nothing. Deafen also
 *  mutes the mic (Discord-style); un-deafening restores the mic to its pre-deafen
 *  state. Muting is local playback — the server and peers keep sending, we just
 *  don't play it. */
export function toggleDeafen(): void {
  voice.deafened = !voice.deafened;
  applyDeafen();

  if (voice.deafened) {
    micBeforeDeafen = voice.muted;
    if (!voice.muted) toggleMute();
  } else if (!micBeforeDeafen && voice.muted) {
    toggleMute();
  }

  const me = voice.participants[account];
  if (me) me.deaf = voice.deafened;
}

/** Start the local camera on a chosen device (or the default). Video rides the
 *  LiveKit SFU only — on the WebRTC path this hints and no-ops. The `cameraOn`
 *  flag is confirmed by the resulting `LocalTrackPublished` event. */
export async function startCamera(deviceId?: string): Promise<void> {
  if (!room) {
    voice.error = "camera needs the LiveKit voice backend";
    return;
  }
  try {
    await room.localParticipant.setCameraEnabled(true, deviceId ? { deviceId } : undefined);
  } catch (err) {
    voice.error =
      err instanceof Error && err.name === "NotAllowedError"
        ? "camera permission denied"
        : "couldn't start the camera";
  }
}
export async function stopCamera(): Promise<void> {
  if (!room) return;
  try {
    await room.localParticipant.setCameraEnabled(false);
  } catch {
    /* already off */
  }
}

// Tauri v2 injects `__TAURI_INTERNALS__`; its absence ⇒ a plain browser. The
// desktop app uses the custom native screen picker; the web build uses the OS
// getDisplayMedia picker.
export const IS_DESKTOP = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

/** Start screen sharing (LiveKit path only). Uses `getDisplayMedia`, which brings
 *  up the OS-native picker (screens · windows · tabs, with thumbnails) and manages
 *  the publish + the "Stop sharing" lifecycle. Screen audio is included when the
 *  chosen source offers it.
 *
 *  On the desktop app the native picker is provided by the OS/WebKit — there is
 *  no app-code hook to summon it (macOS exposes no display-capture permission,
 *  only camera/mic) — and it's gated behind the OS "Screen Recording" permission.
 *  A denial there surfaces as the same `NotAllowedError` as a user cancel, so on
 *  desktop we surface an actionable hint instead of silently doing nothing. */
export async function startScreenShare(): Promise<void> {
  if (!room) {
    voice.error = "screen sharing needs the LiveKit voice backend";
    return;
  }
  try {
    await room.localParticipant.setScreenShareEnabled(true, { audio: true });
  } catch (err) {
    const notAllowed = err instanceof Error && err.name === "NotAllowedError";
    // In the browser a NotAllowedError is almost always a deliberate cancel — stay
    // quiet. On the desktop app it usually means the OS Screen-Recording grant is
    // missing (no picker ever appears), so point the user at it.
    if (IS_DESKTOP) {
      voice.error =
        "Screen sharing needs the Screen Recording permission — enable Weft in " +
        "System Settings → Privacy & Security → Screen Recording, then reopen the app.";
    } else if (!notAllowed) {
      voice.error = "couldn't start screen sharing";
    }
  }
}
export async function stopScreenShare(): Promise<void> {
  // A native (custom-picker) capture takes priority over the getDisplayMedia one.
  if (nativeScreenTrack) {
    await stopNativeScreenShare();
    return;
  }
  if (!room) return;
  try {
    await room.localParticipant.setScreenShareEnabled(false);
  } catch {
    /* already off */
  }
}

// ── Native voice screen share (desktop, Rust SDK) ──────────────────────────
// The Tauri binary captures the chosen source (xcap) and publishes it to the
// room via the native LiveKit SDK — no webview, no canvas. `voice.sharingScreen`
// flips via the roster once the track is published.
export async function startNativeVoiceScreenshare(
  sourceId: string,
  opts?: { fps?: number; maxWidth?: number },
): Promise<void> {
  try {
    await tauriInvoke("voice_native_start_screenshare", {
      id: sourceId,
      fps: opts?.fps ?? 15,
      maxWidth: opts?.maxWidth ?? 1280,
    });
  } catch {
    voice.error = "couldn't start screen share";
  }
}
export async function stopNativeVoiceScreenshare(): Promise<void> {
  await tauriInvoke("voice_native_stop_screenshare").catch(() => {});
}

// ── Native voice camera (desktop, Rust SDK via nokhwa) ─────────────────────
export async function listNativeCameras(): Promise<{ id: string; name: string }[]> {
  try {
    return await tauriInvoke("voice_native_list_cameras");
  } catch {
    return [];
  }
}
export async function startNativeVoiceCamera(deviceId?: string): Promise<void> {
  try {
    await tauriInvoke("voice_native_start_camera", { deviceId: deviceId ?? null });
  } catch (e) {
    voice.error = String(e).toLowerCase().includes("permission")
      ? "camera permission denied"
      : "couldn't start the camera";
  }
}
export async function stopNativeVoiceCamera(): Promise<void> {
  await tauriInvoke("voice_native_stop_camera").catch(() => {});
}

// ── Native screen capture (desktop custom picker) ──────────────────────────
// Frames arrive from Rust as base64 JPEG data URLs; we paint them to a canvas
// and publish its captureStream to LiveKit. This is the path behind the
// Discord-style in-app picker, where the user chose a specific screen/window.
let nativeScreenTrack: MediaStreamTrack | null = null;

/** Begin sharing a natively-captured source (`screen:<id>` / `window:<id>`) at
 *  the chosen frame rate + max resolution. */
export async function startNativeScreenShare(
  sourceId: string,
  opts?: { fps?: number; maxWidth?: number },
): Promise<void> {
  if (!room) {
    voice.error = "screen sharing needs the LiveKit voice backend";
    return;
  }
  const fps = opts?.fps ?? 15;
  const maxWidth = opts?.maxWidth ?? 1280;
  try {
    const canvas = document.createElement("canvas");
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    const img = new Image();
    let latest: string | null = null; // latest-wins: drop stale frames if behind
    let busy = false;
    let published = false;

    const paint = () => {
      if (busy || latest === null) return;
      busy = true;
      const url = latest;
      latest = null;
      img.onload = () => {
        if (canvas.width !== img.naturalWidth || canvas.height !== img.naturalHeight) {
          canvas.width = img.naturalWidth;
          canvas.height = img.naturalHeight;
        }
        ctx.drawImage(img, 0, 0);
        busy = false;
        if (!published) {
          published = true;
          void publishCanvasStream(canvas, fps);
        }
        paint();
      };
      img.onerror = () => {
        busy = false;
        paint();
      };
      img.src = url;
    };

    const chan = new Channel<string>();
    chan.onmessage = (dataUrl) => {
      latest = dataUrl;
      paint();
    };

    await tauriInvoke("start_capture", { id: sourceId, fps, maxWidth, onFrame: chan });
  } catch {
    voice.error = "couldn't start screen sharing";
  }
}

async function publishCanvasStream(canvas: HTMLCanvasElement, fps: number): Promise<void> {
  if (!room) return;
  const stream = canvas.captureStream(fps);
  const track = stream.getVideoTracks()[0];
  if (!track) return;
  const lk = await import("livekit-client");
  await room.localParticipant.publishTrack(track, {
    source: lk.Track.Source.ScreenShare,
    name: "screen",
  });
  nativeScreenTrack = track;
}

export async function stopNativeScreenShare(): Promise<void> {
  await tauriInvoke("stop_capture").catch(() => {});
  if (nativeScreenTrack && room) {
    try {
      await room.localParticipant.unpublishTrack(nativeScreenTrack, true);
    } catch {
      /* already gone */
    }
  }
  nativeScreenTrack = null;
}

/** Attach a participant's video track (camera or screenshare) to a <video>
 *  element owned by the stage; a no-op if that track isn't present. */
export function attachVideo(el: HTMLVideoElement, user: string, source: "camera" | "screen"): void {
  const t = videoTracks.get(user)?.[source];
  if (t) t.attach(el);
}
export function detachVideo(el: HTMLVideoElement, user: string, source: "camera" | "screen"): void {
  const t = videoTracks.get(user)?.[source];
  if (t) t.detach(el);
  el.srcObject = null;
}

/** Push the current deafen state onto every remote audio element. Both media
 *  planes play remote audio through elements we own (`attached` for LiveKit,
 *  `audioEl` for WebRTC), so muting those is the whole of "hear nothing." */
function applyDeafen(): void {
  if (audioEl) audioEl.muted = voice.deafened;
  for (const el of attached) el.muted = voice.deafened;
}

function teardown(): void {
  // Native (desktop) path — the LiveKit connection lives in the Tauri binary.
  if (nativeActive) {
    void tauriInvoke("voice_native_disconnect");
    nativeActive = false;
  }
  // LiveKit path (webview / web).
  if (room) {
    void room.disconnect();
    room = null;
  }
  for (const el of attached) {
    el.srcObject = null;
    el.remove();
  }
  attached.clear();

  // WebRTC path.
  if (pc) {
    try {
      pc.close();
    } catch {
      /* ignore */
    }
    pc = null;
  }
  if (localStream) {
    for (const t of localStream.getTracks()) t.stop();
    localStream = null;
  }
  if (audioEl) {
    audioEl.srcObject = null;
    audioEl.remove();
    audioEl = null;
  }

  videoTracks.clear();

  voice.channel = null;
  voice.connecting = false;
  voice.participants = {};
  voice.deafened = false;
  voice.cameraOn = false;
  voice.sharingScreen = false;
  voice.mediaTick++;
  micBeforeDeafen = false;
}

async function onVoiceEvent(e: WeftEvent): Promise<void> {
  switch (e.kind) {
    case "voice-offer":
      await onOffer(e);
      break;
    case "voice-desc":
      await onAnswer(e.channel, e.sdp);
      break;
    case "voice-cand":
      await onCandidate(e.channel, e.candidate);
      break;
    case "voice-state":
      onState(e);
      break;
    case "closed":
      teardown();
      break;
    case "error":
      // Only a *pre-media* error means the VOICE JOIN itself was rejected
      // (UNSUPPORTED / NO-SUCH-TARGET / FORBIDDEN). Once the media connection is
      // being established (a room/peer exists), the media path owns its own
      // errors — an unrelated server error must not kick us out of voice.
      if (voice.connecting && voice.channel && !room && !pc) {
        voice.error = e.text;
        teardown();
      }
      break;
  }
}

/** The server authorized the join. Branch on the media plane it chose. */
async function onOffer(e: Extract<WeftEvent, { kind: "voice-offer" }>): Promise<void> {
  if (e.channel !== voice.channel) return;
  if (e.mode === "livekit") {
    // Desktop joins the room natively (Rust SDK) so the mic is captured outside
    // the webview — the only way to stop macOS ducking other apps. Web keeps the
    // in-page JS SDK.
    if (IS_DESKTOP) await onNativeLiveKit(e.channel, e.endpoint, e.token);
    else await onLiveKitOffer(e.channel, e.endpoint, e.token);
  } else {
    await onWebrtcOffer(e.channel, e.endpoint);
  }
}

/** Desktop LiveKit path: hand the connect off to the Tauri binary, which becomes
 *  the sole participant. Roster + state arrive via the `voice-native-*` events
 *  wired in initVoice. */
async function onNativeLiveKit(channel: string, url: string | null, token: string): Promise<void> {
  if (!url) {
    voice.error = "voice server URL missing";
    void leaveVoice();
    return;
  }
  if (channel !== voice.channel) return;
  nativeActive = true;
  try {
    await tauriInvoke("voice_native_connect", { url, token });
  } catch (e) {
    // Surface the real Rust error (audio device / connect / publish) rather than
    // a generic message, so failures are diagnosable.
    voice.error = `voice failed — ${e}`;
    void leaveVoice();
  }
}

/** LiveKit path: connect the SDK `Room` with the access token, publish the mic,
 *  and mirror participants/speaking/mute into the roster from Room events. */
async function onLiveKitOffer(
  channel: string,
  url: string | null,
  token: string,
): Promise<void> {
  if (!url) {
    voice.error = "voice server URL missing";
    void leaveVoice();
    return;
  }
  try {
    const lk = await import("livekit-client");
    // Any OS audio processing (echo cancel / noise suppress / auto-gain) makes
    // WebKit route the mic through macOS's *voice-processing* audio unit, which
    // ducks (lowers) every other app's audio while the mic is live. In the
    // desktop webview we capture RAW (all processing off) so other apps stay at
    // full volume — the only way to keep WebKit off that unit. Use headphones to
    // avoid speaker echo. The browser build keeps processing (no ducking there).
    const r = new lk.Room({
      adaptiveStream: true,
      dynacast: true,
      audioCaptureDefaults: {
        echoCancellation: !IS_DESKTOP,
        noiseSuppression: !IS_DESKTOP,
        autoGainControl: !IS_DESKTOP,
      } as AudioCaptureOptions,
    });
    room = r;

    r.on(lk.RoomEvent.TrackSubscribed, (track, pub, participant) => {
      if (track.kind === lk.Track.Kind.Audio) {
        const el = track.attach();
        el.autoplay = true;
        el.muted = voice.deafened;
        attached.add(el);
        return;
      }
      if (track.kind === lk.Track.Kind.Video) {
        const source = pub.source === lk.Track.Source.ScreenShare ? "screen" : "camera";
        setVideoTrack(participant.identity, source, track);
        upsertParticipant(participant);
        voice.mediaTick++;
      }
    });
    r.on(lk.RoomEvent.TrackUnsubscribed, (track, pub, participant) => {
      if (track.kind === lk.Track.Kind.Video) {
        const source = pub.source === lk.Track.Source.ScreenShare ? "screen" : "camera";
        clearVideoTrack(participant.identity, source);
        upsertParticipant(participant);
        voice.mediaTick++;
        track.detach();
        return;
      }
      for (const el of track.detach()) {
        attached.delete(el);
        el.remove();
      }
    });
    r.on(lk.RoomEvent.ParticipantConnected, (p) => upsertParticipant(p));
    r.on(lk.RoomEvent.ParticipantDisconnected, (p) => {
      delete voice.participants[p.identity];
    });
    r.on(lk.RoomEvent.ActiveSpeakersChanged, (speakers) => onSpeakers(speakers));
    r.on(lk.RoomEvent.TrackMuted, (_pub, p) => upsertParticipant(p));
    r.on(lk.RoomEvent.TrackUnmuted, (_pub, p) => upsertParticipant(p));
    r.on(lk.RoomEvent.LocalTrackPublished, (pub) => {
      if (pub.track && pub.kind === lk.Track.Kind.Video) {
        const source = pub.source === lk.Track.Source.ScreenShare ? "screen" : "camera";
        setVideoTrack(r.localParticipant.identity, source, pub.track);
        if (source === "screen") voice.sharingScreen = true;
        else voice.cameraOn = true;
        voice.mediaTick++;
      }
      upsertParticipant(r.localParticipant);
    });
    r.on(lk.RoomEvent.LocalTrackUnpublished, (pub) => {
      if (pub.kind === lk.Track.Kind.Video) {
        const source = pub.source === lk.Track.Source.ScreenShare ? "screen" : "camera";
        clearVideoTrack(r.localParticipant.identity, source);
        if (source === "screen") voice.sharingScreen = false;
        else voice.cameraOn = false;
        voice.mediaTick++;
      }
      upsertParticipant(r.localParticipant);
    });
    r.on(lk.RoomEvent.Disconnected, (reason) => {
      if (room !== r) return;
      // Tell our own leave/teardown apart from a server-side drop (rejected
      // token, removed, room gone) so the user learns *why* they dropped
      // instead of being silently kicked.
      if (reason !== undefined && reason !== lk.DisconnectReason.CLIENT_INITIATED) {
        voice.error = "voice disconnected — check the LiveKit server URL and keys";
      }
      teardown();
    });

    await r.connect(url, token);
    // A leave (or a re-join) may have landed while we were connecting.
    if (room !== r || voice.channel !== channel) {
      await r.disconnect();
      return;
    }
    await r.localParticipant.setMicrophoneEnabled(!voice.muted);

    // Seed the full roster: self plus everyone already in the room.
    upsertParticipant(r.localParticipant);
    for (const p of r.remoteParticipants.values()) upsertParticipant(p);
    voice.connecting = false;
  } catch (err) {
    voice.error =
      err instanceof Error && err.name === "NotAllowedError"
        ? "microphone permission denied"
        : "voice connection failed";
    void leaveVoice();
  }
}

/** Reflect one LiveKit participant into the roster (keyed by `user@network`). */
function upsertParticipant(p: Participant): void {
  voice.participants[p.identity] = {
    user: p.identity,
    speaking: p.isSpeaking,
    muted: !p.isMicrophoneEnabled,
    deaf: false,
    self: p.isLocal,
    cameraOn: p.isCameraEnabled,
    sharingScreen: p.isScreenShareEnabled,
  };
}

/** LiveKit active-speaker update: light exactly the speakers in the list. */
function onSpeakers(speakers: Participant[]): void {
  const talking = new Set(speakers.map((s) => s.identity));
  for (const id of Object.keys(voice.participants)) {
    const p = voice.participants[id];
    if (p) p.speaking = talking.has(id);
  }
}

/** WebRTC path (embedded SFU): build the peer connection + non-trickle offer. */
async function onWebrtcOffer(channel: string, endpoint: string | null): Promise<void> {
  if (channel !== voice.channel) return;
  try {
    // Best-quality capture: the webview's libwebrtc does echo cancellation,
    // noise suppression, and auto gain when we ask for them (on by default in
    // browsers, explicit here so desktop webviews enable them too).
    localStream = await navigator.mediaDevices.getUserMedia({
      audio: {
        // See onLiveKitOffer: any OS audio processing routes through the voice-
        // processing unit that ducks other apps, so desktop captures raw.
        echoCancellation: !IS_DESKTOP,
        noiseSuppression: !IS_DESKTOP,
        autoGainControl: !IS_DESKTOP,
      } as MediaTrackConstraints,
      video: false,
    });
  } catch {
    voice.error = "microphone permission denied";
    void leaveVoice();
    return;
  }

  pc = new RTCPeerConnection(iceConfig(endpoint));
  for (const track of localStream.getAudioTracks()) {
    track.enabled = !voice.muted;
    pc.addTrack(track, localStream);
  }
  // Remote audio playback: attach the SFU-forwarded stream to a hidden element.
  audioEl = document.createElement("audio");
  audioEl.autoplay = true;
  audioEl.muted = voice.deafened;
  pc.ontrack = (ev) => {
    if (audioEl && ev.streams[0]) audioEl.srcObject = ev.streams[0];
  };

  const offer = await pc.createOffer();
  // Turn on Opus in-band FEC (loss resilience) + DTX (silence suppression) for
  // better quality on lossy links — libwebrtc honors these fmtp params.
  await pc.setLocalDescription({ type: "offer", sdp: withOpusQuality(offer.sdp ?? "") });
  await waitIceComplete(pc);
  await voiceDesc(channel, pc.localDescription?.sdp ?? offer.sdp ?? "");

  // The server never echoes our own VOICE STATE, so seed the roster with self.
  voice.participants[account] = {
    user: account,
    speaking: false,
    muted: voice.muted,
    deaf: false,
    self: true,
  };
  voice.connecting = false;
}

async function onAnswer(channel: string, sdp: string): Promise<void> {
  if (channel !== voice.channel || !pc) return;
  try {
    await pc.setRemoteDescription({ type: "answer", sdp });
  } catch {
    voice.error = "voice negotiation failed";
    void leaveVoice();
  }
}

async function onCandidate(channel: string, candidate: string): Promise<void> {
  if (channel !== voice.channel || !pc) return;
  try {
    await pc.addIceCandidate({ candidate, sdpMLineIndex: 0 });
  } catch {
    /* non-trickle server usually sends none; ignore stragglers */
  }
}

function onState(e: Extract<WeftEvent, { kind: "voice-state" }>): void {
  // Presence for *any* voice channel we can see — drives the sidebar roster even
  // when we're not in the call.
  const roster = (voiceRosters[e.channel] ??= {});
  if (e.action === "leave") {
    delete roster[e.user];
    if (Object.keys(roster).length === 0) delete voiceRosters[e.channel];
  } else {
    roster[e.user] = {
      user: e.user,
      speaking: e.speaking,
      muted: e.muted,
      deaf: e.deaf,
      self: e.user === account,
    };
  }

  // The channel we're in also feeds voice.participants — but only on the WebRTC
  // backend. On the LiveKit path the SDK's Room events are the sole authority on
  // that roster; folding `voice-state` in as well double-lists any peer whose
  // LiveKit identity and server account key aren't byte-identical.
  if (e.channel === voice.channel && !room) {
    if (e.action === "leave") delete voice.participants[e.user];
    else voice.participants[e.user] = { ...roster[e.user] };
  }
}

/** Non-trickle: resolve once ICE gathering finishes (bounded, so a stalled
 *  gather can't hang the join). */
function waitIceComplete(conn: RTCPeerConnection): Promise<void> {
  if (conn.iceGatheringState === "complete") return Promise.resolve();
  return new Promise((resolve) => {
    const done = () => {
      if (conn.iceGatheringState === "complete") {
        conn.removeEventListener("icegatheringstatechange", done);
        clearTimeout(timer);
        resolve();
      }
    };
    const timer = setTimeout(() => {
      conn.removeEventListener("icegatheringstatechange", done);
      resolve();
    }, 3000);
    conn.addEventListener("icegatheringstatechange", done);
  });
}

/** Enable Opus in-band FEC + DTX on the offer SDP's Opus fmtp line (adding one
 *  if absent). Loss-concealment that materially improves audio on bad networks. */
function withOpusQuality(sdp: string): string {
  const pt = sdp.match(/a=rtpmap:(\d+)\s+opus\/48000/i)?.[1];
  if (!pt) return sdp;
  const want = "useinbandfec=1;usedtx=1";
  const fmtp = new RegExp(`a=fmtp:${pt} (.*)`);
  if (fmtp.test(sdp)) {
    return sdp.replace(fmtp, (_m, params) =>
      /useinbandfec/.test(params) ? `a=fmtp:${pt} ${params}` : `a=fmtp:${pt} ${params};${want}`,
    );
  }
  // No fmtp line for Opus — add one right after its rtpmap.
  return sdp.replace(
    new RegExp(`(a=rtpmap:${pt} opus/48000/2\\r?\\n)`, "i"),
    `$1a=fmtp:${pt} ${want}\r\n`,
  );
}

function iceConfig(endpoint: string | null): RTCConfiguration {
  if (endpoint && endpoint.startsWith("stun:")) {
    return { iceServers: [{ urls: endpoint }] };
  }
  return RTC_CONFIG;
}

// `voiceCand` is exported for a future trickle-ICE path; referenced here so the
// non-trickle default doesn't flag it as unused.
export const _trickleReserved = voiceCand;
