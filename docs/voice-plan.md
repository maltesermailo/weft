# Voice — implementation plan

Status: **design, for approval** (2026-07-20). Realizes WEFT-RT, the voice/video
companion (spec §16), starting with an **audio-only** slice. Builds the second
thing WEFT has deliberately not built yet: a **real-time media path** (SFU-forwarded
Opus over WebRTC) alongside the existing control plane and the media data plane.
Parked behind media + moderation, which are shipped.

## North star (fixed by the spec — do not relitigate)

- **Signaling lives in the core control plane** (§16): `VOICE JOIN #chan` →
  SFU endpoint + a **short-lived media token** carrying `speak`/`listen` caps;
  `VOICE DESC :<sdp>` is the SDP-equivalent negotiation; discovery via
  `features=voice` (already emitted in `WELCOME`).
- **Voice channels are a distinct channel kind** (`text` | `voice`), not a voice
  room bolted onto a text channel (decided 2026-07-20). A `voice` channel is
  **voice-only**: a text `JOIN` answers `NO-SUCH-TARGET`, which keeps it
  invisible to the IRC gateway (§17) with no gateway code; clients enter it via
  `VOICE JOIN` (which subscribes for the `VOICE STATE` roster). Kind is set at
  `CHANNEL CREATE #chan voice` / `[[channels]]` config, immutable after, and
  advertised in `CHANNEL-LAYOUT` (`kind=voice`). *This superseded the M-voice-1a
  assumption that `VOICE JOIN` required a prior text JOIN of the same channel —
  implemented across proto/store/core/weftd/client, all tests green.*
- **Topology is an SFU** — selective forwarding, the Discord model. **Not mesh**
  (leaks member IPs, doesn't federate, caps at ~5), **not an MCU** (server-side
  mixing, CPU-bound). Fixed by §16.
- **Opus mandatory**; AV1/H.264 negotiable *later* — v1 is audio-only.
- **Clients only ever talk to their home network.** A federated member joins voice
  through *their own* SFU, never by dialing a foreign one — the same privacy
  invariant that makes media **mirrored** rather than hotlinked (foreign dials
  leak reader IPs + the origin↔member relationship, §13/§11.8).
- **Zero-voice servers stay conformant** (§16) — voice is entirely optional and
  feature-gated; a server that never sets `features=voice` is fully compliant.
- **Caps precede side effects** (invariant 4): no RTP is forwarded to or from a
  peer before its `speak`/`listen` authorization is verified.

## Decisions (locked 2026-07-20)

| # | Area | Decision |
|---|------|----------|
| 1 | Framework | **webrtc-rs/sfu** (the sans-I/O SFU crate) — sans-I/O matches weft-core's port discipline (we drive the socket + timers). Chosen over str0m as the more promising track; over external LiveKit (a second non-Rust server breaks the single-sovereign-binary ethos). |
| 2 | Media transport | **WebRTC** (SDP offer/answer, ICE, DTLS-SRTP). Serves **both** clients day one: browsers via `getUserMedia`/`RTCPeerConnection`, desktop via webrtc-rs. The §16 "QUIC datagrams" wording is demoted to a **future desktop-native optimization** — a spec amendment (below). |
| 3 | Media scope | **Audio-only (Opus).** Video/screenshare add later on the same SFU with no rework — audio is the smallest correct slice (capture → forward → playback → mute → presence). |
| 4 | Backend seam | A **`VoiceBackend` port trait in weft-core** — the pluggable-SFU seam. `WebrtcSfu` (weft-rt) is the default/reference impl; a `LiveKitBackend` is a future drop-in behind the same `VOICE` signaling, no protocol change. |
| 5 | Deployment | **Embedded in weftd behind a `voice` feature flag** (like the deferred `e2ee` flag). New crate `weft-rt` holds the SFU; one binary, shared config/caps. Not-enabled = zero-voice conformant. |
| 6 | Local authz | The offer's **ICE ufrag arrives over the already-authenticated control stream**, so the SFU maps `ufrag → account` — that is the credential. A session-scoped media token (`VOICE OFFER <token>`) binds the media session + carries `speak`/`listen`; **no separate bearer is needed locally**. |
| 7 | Moderation | **SFU-enforced**, tied to M7: a muted speaker's RTP is **dropped at the SFU** (not client-cooperative). `mute` cap governs; `MODERATED` already renders. |
| 8 | Federation | **In scope — the v1 capstone.** Cascaded SFUs: member ↔ home SFU ↔ origin SFU over the bridge, **one hop** (§11). Relay is **network-key self-authenticating** (like `MIRROR`); manifest-gated via a new `voice` mode; **NETBLOCK severs** it (invariant 7 "stop media"). |
| 9 | ICE | **Non-trickle by default** — a public-IP SFU gathers host/srflx candidates up front, so one `VOICE DESC` round-trip suffices. Trickle (`VOICE CAND`) is an optional latency optimization, wired in the codec from the start. |
| 10 | Voice presence | A **separate `VOICE-STATE`** event (roster + speaking + mute/deafen), distinct from the text `MEMBER`/presence machinery — it carries speaking/mute state text presence doesn't, and joins/leaves voice independently of channel membership. |
| 11 | NAT/TURN | v1 relies on the SFU's **public host/srflx** candidates only. UDP-blocked networks (needing TURN-over-443) are a **noted limitation**, deferred to a follow-up. |
| 12 | E2EE voice | **Deferred** (SFrame / insertable-streams over the SFU, §14 territory). v1 media is transport-encrypted (DTLS-SRTP), SFU-visible. The token/policy design must not preclude a later host-blind voice path. |

"In scope from the start" (#8) means **designed-in from the first line and a
committed milestone** — not enabled before single-network voice works. Sequencing
is in the milestones below.

## Target architecture

```
weft-proto  (L0)   VOICE JOIN|LEAVE|DESC|CAND verbs; VOICE-OFFER / VOICE-STATE
                   events; MediaToken + IceCandidate types. Round-trip tests
                   FIRST. NO transport, NO webrtc — pure codec.

weft-crypto (L0)   speak/listen cap scopes (channel · ns: · * covering, like
                   send/view); a signed cross-network **voice token** (deterministic
                   CBOR, scope-authority-signed, short expiry) — modeled on the
                   manifest / mirror signing (rotation.rs, mirror.rs).

weft-core   (L2)   VoiceBackend PORT trait (the pluggable seam) + signaling authz:
                   VOICE JOIN checks speak/listen caps, channel membership, the M7
                   mute/ban deny-list (invariants 1 + 4), THEN calls the backend to
                   allocate a slot and returns VOICE-OFFER. Voice-room roster +
                   VOICE-STATE broadcast. Manifest voice-mode gating for federated
                   joins (invariant 3). NO sockets, as always.

weft-rt     (L2)   NEW. The webrtc-rs/sfu engine: a per-channel voice-room actor,
                   one PeerConnection per client, Opus RTP forwarding to N
                   subscribers, active-speaker detection, mute-drop, the cascaded
                   relay to a peer SFU. Implements VoiceBackend. UDP I/O lives HERE.

weftd       (L3)   [voice] config (bind addr, public IP / advertised candidates,
                   UDP port range, feature flag); spawns the SFU; wires the
                   WebrtcSfu backend into weft-core via the port. Signaling still
                   rides the existing control stream (QUIC stream 0 / WS text).

clients            Tauri: webrtc-rs client PeerConnection + cpal capture/playback +
                   an opus binding; voice UI (join/leave, member list, speaking
                   ring, local mute/deafen). Web: getUserMedia → RTCPeerConnection,
                   SDP/ICE lines over the existing WebSocket; same UI.
```

**Why the WebSocket web client can still do voice:** browser WebRTC opens its own
UDP/ICE media path independent of how control lines are carried. The WebSocket just
ferries the `VOICE DESC`/`VOICE CAND` signaling; media never touches it. So the
WS-only web client is, if anything, the *easier* voice client (the browser owns
capture, Opus, echo cancellation, jitter buffer, and congestion control).

## Signaling flow (concrete)

```
C: @label=v1 VOICE JOIN #gaming/lounge      → core: speak/listen caps? member? not muted?
S: @label=v1 VOICE OFFER <token> :<endpoint>  → authorized; SFU slot reserved, token binds it
C: VOICE DESC :<offer-sdp>                    → client's RTCPeerConnection offer (ICE ufrag = credential)
S: VOICE DESC :<answer-sdp>                   → SFU answers as the single peer for this client
   [ VOICE CAND :<cand> both ways if trickle ] → optional; non-trickle folds candidates into the SDP
   … DTLS-SRTP handshake, Opus RTP flows client ↔ SFU …
S(broadcast): VOICE-STATE #gaming/lounge …    → roster + who is speaking / muted (renders the voice list)
C: VOICE LEAVE #gaming/lounge                 → SFU tears the peer down; VOICE-STATE update
```

## Wire additions (proto-first — codec + round-trip tests before consumers)

- `VOICE JOIN <#chan>` / `VOICE LEAVE <#chan>` — join/leave the voice room.
- `VOICE DESC [label] :<sdp>` — SDP offer/answer, both directions.
- `VOICE CAND [label] :<ice-candidate>` — trickle ICE (optional; non-trickle
  clients simply never send it).
- Event `VOICE-OFFER <token> :<endpoint>` — the `JOIN` response (spec §16's
  `VOICE OFFER tok-9 :ready`): the media token + SFU endpoint/params.
- Event `VOICE-STATE <#chan> …` — voice roster + per-participant speaking / mute /
  deafen flags; a full snapshot on (re)join, deltas thereafter.
- `MediaToken` type + `speak`/`listen` cap scopes; the cross-network voice token
  in weft-crypto.
- `NO-SUCH-TARGET` for a `VOICE JOIN` to a nonexistent / private-unmember /
  view-gated / non-voice channel — uniform code + timing (invariant 1). No
  new "no voice here" error.

## Security invariants to add AS TESTS

- **Caps precede media (invariant 4):** no RTP is forwarded to/from a peer until
  its `speak`/`listen` grant is verified; a `VOICE JOIN` without `listen` never
  reaches the backend.
- **Anti-enumeration (invariant 1):** `VOICE JOIN` to a nonexistent, private-
  unmember, or non-voice channel returns `NO-SUCH-TARGET` — same code + timing
  envelope as any other gated/absent target.
- **Mute is server-side:** a muted account's inbound RTP is dropped at the SFU,
  not by asking its client to stop; a channel/ns/`*` mute all cover per the M7
  covering-scope rule.
- **Media-token scope:** a voice token is bound to `(account, channel, caps,
  expiry)` and cannot be replayed on another channel, by another account, or after
  expiry.
- **Federated relay auth (invariant 2):** the cascade's origin SFU serves a relay
  only to a `[[peers]]`-known network proving its signing key (self-authenticating,
  like `MIRROR`); no origin↔member correlation.
- **Manifest gating (invariant 3):** a federated `VOICE JOIN` is honored only if
  the channel is in the last mutually-acked manifest with a voice-permitting
  `voice` mode; absent = protocol violation, not soft failure.
- **One hop (§11):** relayed media is never re-relayed to a third network; only
  home-origin voice crosses a given bridge.
- **NETBLOCK severs voice (invariant 7):** a name-keyed block stops the media
  relay alongside proposals/manifests/attestations/mirroring — all effects, key
  rotation can't evade.

## Milestones (each independently shippable)

- **M-voice-0 ✅ (2026-07-20) — codec (weft-proto + weft-crypto).** Commands
  `VOICE JOIN|LEAVE <#chan>`, `VOICE DESC <#chan> :<sdp>`, `VOICE CAND <#chan>
  :<candidate>` (raw SDP rides the trailing — CR/LF survive as `\r`/`\n` like any
  message body, so no base64). Events `VOICE OFFER <#chan> <token> [:endpoint]`
  and `VOICE STATE <#chan> <user@net> <join|leave|update>` with presence-style
  `mute=`/`deaf=`/`speaking=` flag tags (emitted only when set) + a `VoiceAction`
  `wire_enum!`. weft-crypto gains the `speak`/`listen` caps (auto-covered by
  `every_standard_cap_round_trips`). Round-trip tests for every form incl. the
  multi-line-SDP case; the old "VOICE is unknown" event test retargeted to a
  genuine unknown verb. weft-core routes the four verbs to the existing
  `unsupported` helper as a placeholder (the SFU is M-voice-1) so the workspace
  stays green. *Green:* `weft-proto` 86 + 4, `weft-crypto` 43, `weft-core` 105;
  clippy `-D warnings` clean. *Ships nothing user-facing; unblocks everyone.*
- **M-voice-1 — SFU skeleton + authz.** Delivered in sub-steps (as media was):
  - **M-voice-1a ✅ (2026-07-20) — core signaling authz + port (weft-core).** The
    `VoiceBackend` port (`Arc<dyn>`, async-trait) held as an optional `OnceLock`
    on `ServerCtx` + `set_voice_backend` (installed by weftd, like the sink
    ports); `on_voice_join/leave/desc/cand` in `session/voice.rs` replacing the
    M-voice-0 stub. **Authz (invariant 4, all before the backend is touched):**
    membership (unjoined → `NO-SUCH-TARGET`, invariant 1), the M7 ban (→
    `FORBIDDEN`) / mute (→ removes `speak`) deny-list via `is_moderated`, and
    `listen`/`speak` caps on a *restricted* channel (mirrors the posting gate).
    `VOICE JOIN` → `VOICE OFFER <token>` (labeled ack) + a `VOICE STATE join`
    fanned to co-members via a new `ChannelHandle::announce_as` (origin=self, so
    the actor's own copy is skipped — the `SetPolicy` pattern); `VOICE DESC` →
    the SFU answer as a `VOICE DESC` event (codec gained `Event::VoiceDesc` +
    `VoiceCand` for symmetry); disconnect tears down every voice room. A
    `MockVoice` backend drives 5 networkless tests (no-backend→UNSUPPORTED,
    unjoined→NO-SUCH-TARGET, offer+token+co-member state+DESC-relay+leave,
    muted→renders muted, `*`-banned→FORBIDDEN). *Green:* weft-core 110, weft-proto
    86, clippy `-D warnings` clean.
  - **M-voice-1b ✅ (2026-07-20) — the WebRTC SFU (weft-rt).** New crate on
    **`webrtc` 0.17.1** (the `RTCPeerConnection` API — the sans-IO `sfu` crate is
    0.0.x/immature; ring provider, no aws-lc). `WebrtcSfu` implements
    `VoiceBackend`: one shared `API` (MediaEngine+Opus, default interceptors,
    pinned UDP port range), a per-channel `Room`, one PeerConnection per session.
    `join` wires `on_track` → mirror the peer's inbound Opus into a
    `TrackLocalStaticRTP` + pump its RTP (SSRC/PT rewritten per binding —
    verbatim forward); `describe` subscribes the peer to every existing publisher
    (**`add_track` before `set_remote_description`** — the ordering that binds the
    sender; the reverse leaves it paused, the bug that cost the media path) then
    non-trickle gather+answer; `leave` closes + prunes. *Green:* two real-webrtc
    integration tests (offline host-ICE, distinct UDP ranges) — the SFU answers a
    client offer with gathered candidates, and **Opus forwards publisher →
    subscriber end-to-end over DTLS-SRTP** (~3s). clippy `-D warnings` clean;
    workspace builds. weft-rt is a `members` (not `default-members`) crate — the
    default build stays lean; weftd pulls it behind the `voice` feature in 1c.
    *Deferred to 1c:* SFU-initiated renegotiation (an offer the server pushes) so
    a *new* publisher reaches already-connected peers — today a room converges
    when peers join in order; a late publisher needs the subscriber to re-`describe`.
  - **M-voice-1c ✅ (2026-07-20) — weftd wiring + conformance.** A `voice` Cargo
    feature (`dep:weft-rt`, off by default — the lean build pulls neither webrtc
    nor its tree); a `[voice]` config block (`enabled`, UDP port range, STUN);
    `build_voice_sfu` constructs the SFU up front so WELCOME advertises
    `features=voice` only when it came up, then `ctx.set_voice_backend` installs
    it once boot returns `ctx`. A failed SFU build degrades to no-voice (logged),
    never aborts boot; `enabled` without the feature just warns. *Green:* two
    QUIC conformance tests — the default server advertises no voice + answers
    `VOICE JOIN` with `UNSUPPORTED`; the `voice`-feature server advertises voice,
    `VOICE JOIN` → a real `VOICE OFFER` (the SFU allocated a peer), and a bad
    `VOICE DESC` is rejected `MALFORMED` — the whole control→SFU path over real
    QUIC. clippy `-D warnings` clean on **both** build configs; `weftd.example.toml`
    documents `[voice]`. *Deferred to a follow-up (was noted in 1b):* SFU-initiated
    renegotiation for late joiners; a two-live-weftd media-forwarding conformance
    (media forwarding itself is already proven by the weft-rt integration test).

**M-voice-1 is complete** (1a+1b+1c): authorized voice signaling over QUIC drives
a real webrtc SFU that forwards Opus between participants, behind a feature flag.
- **M-voice-2 ✅ (2026-07-20, code) — web client.** weft-client-core: `VoiceOffer`/
  `VoiceState`/`VoiceDesc`/`VoiceCand` `ClientEvent`s + `build_voice_join|leave|
  desc|cand` (native + wasm); weft-client-wasm dispatch arms. `client/src/lib/
  voice.svelte.ts` — a Svelte-5-`$state` WebRTC controller: `joinVoice` →
  server-authorized `voice-offer` → `getUserMedia` + a **non-trickle** offer
  (gather-then-send, matching the SFU) → `voiceDesc` → set the `voice-desc`
  answer → Opus both ways; a hidden `<audio>` plays the forwarded stream;
  `voice-state` drives the roster; `toggleMute` disables the local track.
  `VoiceBar.svelte` (join/leave, mute, speaking-ring roster) rendered atop the
  member column; `initVoice` wired on connect. *Green:* `svelte-check` 0/0, the
  wasm build, `vite build`, and the full workspace all compile clean; clippy
  clean. **Not runtime-verified here** (getUserMedia + two-browser audio needs a
  real browser against a `--features voice` weftd) — but the SFU media path (1b)
  and the control path over the wire (1c) are already proven.
- **M-voice-3 ✅ (2026-07-20) — desktop client (Tauri), via webview WebRTC.**
  *Decision: use the system webview's WebRTC (libwebrtc — the same engine as
  LiveKit, with full AEC/NS/AGC/jitter/congestion) rather than a native
  webrtc-rs+cpal+Opus stack. Native would be **more** work for **less** quality
  (webrtc-rs has no echo cancellation); the webview path is fastest *and* highest
  quality and reuses M-voice-2's `voice.svelte.ts` unchanged.* Added the
  `voice_join|leave|desc|cand` **Tauri commands** (so `invoke` routes on desktop),
  the macOS mic-usage `Info.plist` (`NSMicrophoneUsageDescription`), and a
  `with_webview` **permission handler** granting the WebKitGTK (Linux) webview's
  `getUserMedia`. Quality polish in `voice.svelte.ts` (helps web too): explicit
  `echoCancellation`/`noiseSuppression`/`autoGainControl` + Opus **in-band FEC +
  DTX** SDP munging. *Green here:* client (macOS) builds, svelte-check 0/0, web
  build, clippy clean. **Unverified (need real hardware):** the Linux/Windows
  webview permission arms (cfg-gated, not compiled on the macOS dev box — the
  WebKitGTK crate version must track wry's) and actual two-desktop audio. LiveKit
  stays a future *server*-side option behind the `VoiceBackend` seam if scale
  demands it, never a client rewrite.
- **M-voice-4 — moderation + presence polish.** SFU-enforced mute/deafen tied to
  M7 (`MUTE` drops RTP live); server-side **active-speaker detection** feeding
  `VOICE-STATE`; a full `VOICE-STATE` **snapshot on (re)join** (mirrors the
  `MARKED` reconnect-snapshot pattern); a `SLOW`-equivalent teardown for a stalled
  peer (invariant 6 — never buffer unboundedly).
- **M-voice-5 — federated voice (cascaded SFU).** The crypto voice token; the
  manifest **`voice`-mode** negotiation (alongside `media`); the home ↔ origin
  **relay over the bridge** (home SFU subscribes on behalf of local members and
  forwards their audio upstream, one hop); **NETBLOCK sever**; per-hop moderation
  (origin mute at origin SFU + receiver-local mute). *Green:* two-live-weftd
  conformance (the M5d two-server pattern) — a member on H joins a voice channel
  homed on F, hears F's speakers and is heard, entirely through H's SFU; a
  `NETBLOCK` on F cuts the relay; a manifest without `voice` refuses the join.

## Hard parts / risks (call out early)

- **webrtc-rs/sfu maturity under load.** The SFU crate is young; production-grade
  load handling (remote-wallclock estimation, pacing) is not a given. **Mitigation:
  the `VoiceBackend` seam** — if it doesn't hold up, a LiveKit backend drops in
  behind the same signaling without a protocol change.
- **Desktop audio (M-voice-3) is the heaviest client lift.** `cpal` gives raw
  capture/playback but **no acoustic echo cancellation / noise suppression** (the
  browser gives these free via `getUserMedia`). Native voice likely needs a
  `webrtc-audio-processing` (APM) binding, plus device-enumeration/hotplug and
  sample-rate-conversion glue. Budget for it.
- **Cascaded SFU (M-voice-5) is the hardest server work.** Relay authentication,
  strict one-hop (no relay-of-relay loops), and cross-SFU jitter/clock handling.
  It's correctly sequenced last — a cascade needs a working SFU first.
- **NAT/TURN.** A public-IP SFU needs only host/srflx candidates for most clients,
  but UDP-blocked networks (corporate, some mobile) need TURN-over-443. **Deferred
  with a noted limitation**, not built in v1.
- **Congestion / bandwidth.** Per-peer, WebRTC handles it; the SFU must not blindly
  forward. Audio-only keeps this bounded (no simulcast/SVC), which is a reason
  audio is v1 — video reopens this in earnest.

## Code navigation (update per milestone)

Each voice milestone that lands code updates `reviews/code-navigation.md` in the
same PR — the new `weft-rt` crate in the 30-second map, the new signaling chain
(`VOICE JOIN` → `on_voice_join` → `VoiceBackend` → SFU room actor), and the
"I want to change X" / test-map tables. Follow its `file :: function` convention
(no line numbers — they drift). A voice addendum section, in the shape of the
existing "M3b addendum", is the natural home.

## Spec amendments (same-PR, per CLAUDE.md)

§16 grows from one paragraph into the concrete companion: **WebRTC (SDP/ICE/
DTLS-SRTP) as the normative media transport** (with QUIC-datagram media noted as a
future desktop-native optimization), the `VOICE JOIN|LEAVE|DESC|CAND` verb +
`VOICE-OFFER`/`VOICE-STATE` event grammar, the `speak`/`listen` caps, the
short-lived media token, the **manifest `voice` mode**, and **cascaded-SFU
federation** (relay auth = network key, one hop, NETBLOCK sever). §3.1's "Datagrams:
voice" line is softened accordingly. Appendix A gets a decision-history entry. Spec
wins over this plan — so these land as amendments in the implementing PR.
