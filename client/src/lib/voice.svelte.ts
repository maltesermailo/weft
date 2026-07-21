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
import type { Room, Participant } from "livekit-client";

export type VoiceParticipant = {
  user: string;
  speaking: boolean;
  muted: boolean;
  deaf: boolean;
  self: boolean;
};

type VoiceModel = {
  /** The joined voice channel, or null when not in a voice room. */
  channel: string | null;
  /** True from `joinVoice` until the peer connection is negotiated. */
  connecting: boolean;
  /** Local microphone mute. */
  muted: boolean;
  /** Room roster keyed by account. */
  participants: Record<string, VoiceParticipant>;
  /** Last error (mic denied, a rejected join, …); shown then cleared by the UI. */
  error: string | null;
};

export const voice = $state<VoiceModel>({
  channel: null,
  connecting: false,
  muted: false,
  participants: {},
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

// webrtc-path media state.
let pc: RTCPeerConnection | null = null;
let localStream: MediaStream | null = null;
let audioEl: HTMLAudioElement | null = null;

// livekit-path media state. `room` non-null ⇒ we're on the LiveKit path. Self is
// identified by `participant.isLocal`; remote identities are the `user@network`
// the token set, so the roster matches the WebRTC path's federated keys.
let room: Room | null = null;
const attached = new Set<HTMLMediaElement>();

/** Wire the voice event handler once, and record who "we" are (for the roster).
 *  Call on connect. */
export function initVoice(myAccount: string): void {
  account = myAccount;
  if (!subscribed) {
    subscribed = true;
    void onWeft(onVoiceEvent);
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

function teardown(): void {
  // LiveKit path.
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

  voice.channel = null;
  voice.connecting = false;
  voice.participants = {};
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
    await onLiveKitOffer(e.channel, e.endpoint, e.token);
  } else {
    await onWebrtcOffer(e.channel, e.endpoint);
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
    // libwebrtc does AEC/NS/AGC; ask explicitly so desktop webviews enable them.
    const r = new lk.Room({
      adaptiveStream: true,
      dynacast: true,
      audioCaptureDefaults: {
        echoCancellation: true,
        noiseSuppression: true,
        autoGainControl: true,
      },
    });
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
    r.on(lk.RoomEvent.ParticipantConnected, (p) => upsertParticipant(p));
    r.on(lk.RoomEvent.ParticipantDisconnected, (p) => {
      delete voice.participants[p.identity];
    });
    r.on(lk.RoomEvent.ActiveSpeakersChanged, (speakers) => onSpeakers(speakers));
    r.on(lk.RoomEvent.TrackMuted, (_pub, p) => upsertParticipant(p));
    r.on(lk.RoomEvent.TrackUnmuted, (_pub, p) => upsertParticipant(p));
    r.on(lk.RoomEvent.LocalTrackPublished, () => upsertParticipant(r.localParticipant));
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
        echoCancellation: true,
        noiseSuppression: true,
        autoGainControl: true,
      },
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

  // The channel we're in also feeds voice.participants (the LiveKit path's
  // roster) so the joined VoiceBar stays in sync on the WebRTC backend too.
  if (e.channel === voice.channel) {
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
