// Friend-call media (LiveKit audio). Deliberately independent of the channel-voice
// model in `voice.svelte.ts`: a 1:1 friend call is its own lightweight session, and
// the CallOverlay — not the server VoiceBar — is its UI. The signaling (ring /
// accept / end) lives in `+page.svelte`; this module owns only the media plane,
// connecting when the server pushes a `CALL-MEDIA` credential and tearing down when
// the call ends.
//
// Audio-only for now — mute is supported; camera / screenshare stay on the
// channel-voice path. Media transport matches channel voice:
//   • Desktop — the native Rust LiveKit SDK (Tauri `voice_native_*`), so the mic is
//     captured outside the webview (the only way to stop macOS ducking other apps).
//     That native connection is a SINGLETON, so a call and a channel-voice session
//     are mutually exclusive; we drop any channel voice before dialing a call.
//   • Web — the in-page JS SDK's `Room`.

import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { IS_DESKTOP, leaveVoice, voice } from "./voice.svelte";
import type { Room } from "livekit-client";

export const callMedia = $state<{
  /** True from connect until the room is joined. */
  connecting: boolean;
  /** Local microphone mute. */
  muted: boolean;
  /** Last media error (mic denied, connect failed); shown then cleared by the UI. */
  error: string | null;
}>({
  connecting: false,
  muted: false,
  error: null,
});

// Web (JS SDK) path.
let room: Room | null = null;
const attached = new Set<HTMLMediaElement>();

// Desktop (native Rust SDK) path. `nativeCall` marks that the singleton native
// connection is ours (a call), so mute/leave route to the native commands and a
// native-side drop tears the call down.
let nativeCall = false;
let nativeStateUnlisten: UnlistenFn | null = null;

/** Connect the call's LiveKit room with the pushed access token, publish the mic,
 *  and play the peer's audio. Replaces any existing call connection. */
export async function connectCallMedia(endpoint: string | null, token: string): Promise<void> {
  if (!endpoint) {
    callMedia.error = "voice server URL missing";
    return;
  }
  teardown();
  callMedia.connecting = true;
  callMedia.error = null;

  if (IS_DESKTOP) {
    await connectNative(endpoint, token);
    return;
  }

  try {
    const lk = await import("livekit-client");
    const r = new lk.Room({ adaptiveStream: true, dynacast: true });
    room = r;

    r.on(lk.RoomEvent.TrackSubscribed, (track) => {
      if (track.kind === lk.Track.Kind.Audio) {
        const el = track.attach();
        el.autoplay = true;
        attached.add(el);
      }
    });
    r.on(lk.RoomEvent.TrackUnsubscribed, (track) => {
      for (const el of track.detach()) {
        attached.delete(el);
        el.remove();
      }
    });
    r.on(lk.RoomEvent.Disconnected, () => {
      if (room === r) teardown();
    });

    await r.connect(endpoint, token);
    // A hang-up may have landed while we were connecting.
    if (room !== r) {
      await r.disconnect();
      return;
    }
    await r.localParticipant.setMicrophoneEnabled(!callMedia.muted);
    callMedia.connecting = false;
  } catch (err) {
    callMedia.error =
      err instanceof Error && err.name === "NotAllowedError"
        ? "microphone permission denied"
        : "call connection failed";
    teardown();
  }
}

/** Desktop: dial the call through the native Rust LiveKit SDK. */
async function connectNative(url: string, token: string): Promise<void> {
  // The native connection is a singleton — a channel voice session would be
  // clobbered by our connect, so leave it cleanly first.
  if (voice.channel) await leaveVoice();
  nativeCall = true;

  // React to a native-side drop (server kick / network) so the call UI clears.
  // The channel-voice listener ignores this while `nativeActive` is false, so the
  // two never both act on one connection.
  if (!nativeStateUnlisten) {
    nativeStateUnlisten = await listen<string>("voice-native-state", (e) => {
      if (!nativeCall) return;
      if (e.payload === "connected") callMedia.connecting = false;
      else if (e.payload === "disconnected") teardown();
    });
  }

  try {
    await tauriInvoke("voice_native_connect", { url, token });
    callMedia.connecting = false;
  } catch (e) {
    callMedia.error = `call connection failed — ${e}`;
    teardown();
  }
}

/** Toggle the local microphone for the call. */
export function toggleCallMute(): void {
  callMedia.muted = !callMedia.muted;
  if (nativeCall) {
    void tauriInvoke("voice_native_set_muted", { muted: callMedia.muted });
    return;
  }
  if (room) void room.localParticipant.setMicrophoneEnabled(!callMedia.muted);
}

/** Tear the call's media connection down (hang-up / decline / peer ended). */
export function disconnectCallMedia(): void {
  teardown();
}

function teardown(): void {
  // Native (desktop) path.
  if (nativeCall) {
    void tauriInvoke("voice_native_disconnect");
    nativeCall = false;
  }
  // Web (JS SDK) path.
  if (room) {
    void room.disconnect();
    room = null;
  }
  for (const el of attached) {
    el.srcObject = null;
    el.remove();
  }
  attached.clear();

  callMedia.connecting = false;
  callMedia.muted = false;
}
