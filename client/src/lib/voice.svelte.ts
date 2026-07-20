// §16 WEFT-RT voice — the browser side. Signaling rides the WEFT control
// stream (`voice*` commands + `voice-*` events); the media is an ordinary
// browser WebRTC connection to the server's SFU. One voice room at a time.
//
// Flow: `joinVoice` → server authorizes → `voice-offer` (media token) → we
// getUserMedia + build a non-trickle offer → `voiceDesc` → the SFU answers with
// a `voice-desc` → we set it as the remote description → Opus flows both ways.
// The server is non-trickle (candidates ride the SDP), so we gather fully
// before sending the offer and need no separate ICE exchange.

import { voiceJoin, voiceLeave, voiceDesc, voiceCand, onWeft, type WeftEvent } from "./weft";

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

const RTC_CONFIG: RTCConfiguration = {
  iceServers: [{ urls: "stun:stun.l.google.com:19302" }],
};

let account = "";
let pc: RTCPeerConnection | null = null;
let localStream: MediaStream | null = null;
let audioEl: HTMLAudioElement | null = null;
let subscribed = false;

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

/** Toggle the local microphone (a client-side track disable; server-enforced
 *  mute is M-voice-4). */
export function toggleMute(): void {
  voice.muted = !voice.muted;
  if (localStream) {
    for (const t of localStream.getAudioTracks()) t.enabled = !voice.muted;
  }
  const me = voice.participants[account];
  if (me) me.muted = voice.muted;
}

function teardown(): void {
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
      await onOffer(e.channel, e.endpoint);
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
      // A voice verb was rejected (UNSUPPORTED / NO-SUCH-TARGET / FORBIDDEN)
      // while we were mid-join — surface it and reset.
      if (voice.connecting && voice.channel) {
        voice.error = e.text;
        teardown();
      }
      break;
  }
}

/** The server authorized the join: build the peer connection + offer. */
async function onOffer(channel: string, endpoint: string | null): Promise<void> {
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
  if (e.channel !== voice.channel) return;
  if (e.action === "leave") {
    delete voice.participants[e.user];
    return;
  }
  voice.participants[e.user] = {
    user: e.user,
    speaking: e.speaking,
    muted: e.muted,
    deaf: e.deaf,
    self: e.user === account,
  };
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
