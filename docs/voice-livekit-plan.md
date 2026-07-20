# Voice — LiveKit edition (pivot) — implementation plan

Status: **design, for approval** (2026-07-20). Reworks the voice media plane
(`docs/voice-plan.md`) to use **LiveKit** — an open-source, self-hostable SFU
(Apache-2.0) built on libwebrtc — instead of the in-Rust `weft-rt` SFU. Motivated
by the M-voice-4 walls: late-joiner renegotiation, active-speaker, N-way quality,
reconnection — all *shipped features* in LiveKit, all *hard + unverifiable* in a
DIY SFU. WEFT keeps ownership of **identity, authorization, policy, and
federation**; LiveKit owns only the **media**.

## North star (the decision)

- **LiveKit is the media plane; weftd is the control plane.** `VOICE JOIN` stays
  the WEFT-authoritative gate (caps + mute/ban + membership + voice-channel kind);
  it no longer answers with an SDP flow but with a **LiveKit access token** for
  the channel's room. The client connects with the LiveKit SDK — so
  renegotiation, active-speaker, congestion control, reconnection, screenshare/
  video-later come for free.
- **Media never tunnels through the WEFT control plane.** The client talks to its
  home network's LiveKit directly; federation adds one **server-to-server** relay.
- **Sovereignty is preserved at the "self-host" level**, not the single-binary
  level: each operator runs their own LiveKit beside weftd. `weft-rt` is kept
  behind a feature flag as a zero-extra-dependency fallback.
- **Federation is a weftd-orchestrated relay participant** (§11 one hop), authed
  over the WEFT bridge — NOT a LiveKit-native feature (it has none for sovereign
  operators). This reuses the M-voice-5 crypto voice token + manifest `voice`-mode
  as the *authorization* for a cross-network LiveKit token.

## Decisions (locked pending approval, 2026-07-20)

| # | Area | Decision |
|---|------|----------|
| 1 | Media plane | **LiveKit** (self-hosted). `weft-rt` retained behind a `voice-sfu-native` feature flag as a fallback; not the default. |
| 2 | Backend seam | The existing `VoiceBackend` port stays. A new `LiveKitBackend` fulfils `VOICE JOIN` by **minting a token** (no SDP). `VOICE OFFER` gains a **mode** (`webrtc`\|`livekit`) so the client branches; `weft-rt` and LiveKit coexist. |
| 3 | Token minting | weftd signs LiveKit JWTs with the `livekit-api` Rust crate; the API key/secret is `[livekit]` config shared with the local LiveKit. |
| 4 | Roster / speaking / mute state | Sourced from the **LiveKit SDK** (participant + active-speaker + track-mute events) on the client; the participant *identity* is the WEFT account (set in the token), mapped to avatar/display via profiles (§10.3). WEFT `VOICE STATE` is retired on the LiveKit path. |
| 5 | Server-side moderation | `on_moderate` (M7 MUTE/ban) → weftd calls LiveKit's **Room server API** (`mute_published_track` / `remove_participant`) instead of `VoiceBackend::set_muted`. |
| 6 | Federation | A **relay participant** (LiveKit Rust SDK) that weftd spawns, bridging `F/#chan` ↔ `H/#chan` (subscribe→republish both ways). Authed by the crypto voice token exchanged over the WEFT bridge; gated by manifest `voice`-mode; one hop; torn down on empty / `SEVER` / `NETBLOCK`. |
| 7 | Layering (STRICT) | weft-core stays I/O-free. `LiveKitBackend` in core calls a new **`LiveKitAdmin` port trait** (`mint_token`, `mute_track`, `remove_participant`) implemented in **weftd** with `livekit-api` — exactly like `ControlStream`/`EventStore`. A `MockLiveKitAdmin` makes M-lk-0/2 fully core-testable; only real media needs a deployment. |
| 8 | Backend selection | `[voice] backend = "livekit" \| "native" \| "off"`; `[livekit] url/api_key/api_secret`. weftd picks the `VoiceBackend` at boot; the `mode` in `VOICE OFFER` reflects it. Default: `livekit` if `[livekit]` is present, else `native`, else `off`. |
| 9 | Room naming | LiveKit room = the channel's **stable id** (ULID/record id), namespaced `wv:<network>:<channel-id>` — rename-safe, collision-free across namespaces, and it never leaks a human channel name to a participant who isn't already in it. |

## Authorization → LiveKit grants (the clean part)

The WEFT gate maps one-to-one onto LiveKit token grants — the cap check *is* the
grant computation, so there's no second policy surface to keep in sync:

| WEFT state (checked in `on_voice_join`) | LiveKit token grant |
|---|---|
| member of the (voice-kind) channel | `roomJoin`, `room = wv:<net>:<chan-id>` |
| has `listen` (or channel is open) | `canSubscribe = true` |
| has `speak`/`send` **and** not muted **and** not `restricted`-without-`send` | `canPublish = true` |
| M7 **mute** applied mid-call | `mute_published_track` **now** + `canPublish=false` on the next token |
| M7 **ban** / `KICK` | `remove_participant` + no future token (rejoin fails the gate) |
| identity | `identity = <user@network>` (drives avatar/display via §10.3) |

**Token TTL + refresh:** short TTL (e.g. 10 min) with the client re-requesting via
`VOICE JOIN` before expiry (weftd re-runs the gate — so a revoked cap or a fresh
mute takes effect at refresh even absent a live `mute_published_track`).

## Reused vs. retired

**Reused (unchanged):**
- Voice channels as a distinct **kind** (voice-only, IRC-invisible) — `ChannelKind`.
- The `VOICE JOIN` authorization gate: `speak`/`listen` caps, M7 mute/ban, membership, kind — `weft-core/src/session/voice.rs`.
- The **crypto voice token** + **manifest `voice`-mode** (M-voice-5 foundation) →
  become the federation-relay authorization.
- **Profiles** (§10.3) → map a LiveKit participant identity to a WEFT avatar/name.

**Retired / superseded (on the LiveKit path):**
- `weft-rt` SFU as the default (kept as a feature-flag fallback).
- `voice.svelte.ts`'s hand-rolled `RTCPeerConnection` / `getUserMedia` / SDP-munge
  → replaced by the LiveKit `Room` SDK (net **less** client code).
- `VOICE DESC` / `VOICE CAND` signaling (LiveKit does its own client↔server signaling).
- M-voice-4's `VoiceBackend::set_muted` + WEFT `VOICE STATE` roster/snapshot
  (LiveKit SDK events + the Room API supply these).

## Target architecture

```
Same network:
  client ──VOICE JOIN #lounge──▶ weftd (caps/mute/kind gate) ──▶ mint LiveKit JWT
  client ◀──VOICE OFFER mode=livekit {url, room, token}────────┘
  client ──LiveKit SDK──▶ operator's LiveKit ◀──LiveKit SDK── other members
          (roster / active-speaker / mute all from the SDK; identity = WEFT account)
  weftd ──Room server API (mute/remove)──▶ LiveKit   (M7 moderation)

Federation (F homes #lounge; H bridges it):
  ada@H ──▶ H's LiveKit ("H/#lounge")           F's LiveKit ("F/#lounge") ◀── bob@F
                       ▲                                   ▲
                       └──── weftd@H RELAY participant ─────┘  (LiveKit Rust SDK)
                         subscribe F→republish H, subscribe H→republish F
  Auth: H ──WEFT bridge: request #lounge voice──▶ F  (manifest voice=on? not netblocked?)
        F ──WEFT bridge: LiveKit token for F/#lounge──▶ H   (crypto voice token authorizes)
  Lifecycle: weftd@H starts the relay on first local joiner; stops on empty / SEVER / NETBLOCK.
```

New crate/deps: `weft-livekit` (or in weftd) using `livekit` (Rust SDK, for the
relay participant) + `livekit-api` (JWT minting + Room server API). `[livekit]`
config: `url`, `api_key`, `api_secret`.

### Federation: two distinct tokens (do not conflate)

The crypto voice token and the LiveKit JWT are different credentials at different
layers — this is the crux of the relay design:

1. **WEFT crypto voice token** — network-key-signed CBOR (like `SignedManifest`),
   the *WEFT-level* proof that "H is authorized to relay `#lounge`". Verifiable,
   NETBLOCK/manifest-gated, carried over the bridge. It authorizes H to *ask*.
2. **F's LiveKit JWT** — the *media* credential, signed with **F's** LiveKit
   secret (only F can mint it). F issues it to H's relay **after** validating (1)
   against the acked manifest (`voice=on`) and the netblock list.

Bridge exchange (bridge-only verbs, mirroring `BRIDGE REQUEST` / manifest flow):

```
H ──VOICE REQUEST <scope> <channel> (+ crypto voice token)──▶ F
F  validates: manifest voice=on ∧ ¬netblocked ∧ token authority
F ──VOICE GRANT {livekit_url, room, token, ttl}──▶ H     (else NO-SUCH-TARGET, inv. 1 timing)
H  spawns the relay participant → connects to F's LiveKit with the JWT
```

### Relay constraints (correctness, not polish)

- **Per-participant forward, never a mix.** The relay republishes each F speaker as
  its own track (identity preserved) so H's LiveKit computes active-speaker per
  real F user and H's client maps it to the right avatar. A single mixed track
  would collapse everyone into "the relay is speaking."
- **Loop prevention.** Relay-injected tracks carry a relay identity
  (`relay@<peer>`); the relay never re-subscribes to its own injected tracks, so
  audio doesn't ping-pong F↔H. §11 one-hop already forbids a third network
  re-relaying H's copy.
- **Lifecycle.** Start on the first local joiner; **debounce** teardown on empty
  (avoid thrash on quick leave/rejoin); reconnect/backoff like `weftd::dialer`;
  refresh the LiveKit JWT before TTL for long calls; hard-stop on `SEVER`/`NETBLOCK`.

## Milestones (each independently shippable)

- **M-lk-0 ✅ — signaling + token (weftd).** `[voice] backend = "native"|"livekit"`
  + `[voice.livekit]` config; `weft_core::LiveKitBackend` implementing
  `VoiceBackend` (mints a room JWT on `join` via the `LiveKitAdmin` port);
  `VOICE OFFER` gained `mode` (`@mode=`/`@room=` tags, default `webrtc` =
  unchanged wire form) carrying `{token=JWT, room, endpoint=url}`; `VOICE
  DESC/CAND` return `Unavailable` on this backend. weftd's `LiveKitSigner`
  (`livekit.rs`) mints via LiveKit's own `livekit_api::access_token::AccessToken`
  + `VideoGrants` (feature `access-token`, no `services`/TLS pulled — ring
  provider preserved); cap→grant maps `can_speak`→`can_publish`,
  member→`can_subscribe`. *Green:* two conformance tests — `VOICE JOIN` returns a
  JWT that LiveKit's own `TokenVerifier` **validates under the shared secret**
  with the right `video` grant + `user@network` identity (and rejects a wrong
  secret), and a text channel is refused NO-SUCH-TARGET (no token).
  weft-proto 88, weft-core 116, weftd conformance green; clippy clean (native +
  livekit paths).
- **M-lk-1 ✅ — client Room swap (web + desktop).** `voice.svelte.ts` branches on
  the offer's `mode`: `webrtc` keeps the embedded-SFU path (renamed
  `onWebrtcOffer`); `livekit` runs `onLiveKitOffer` — a **dynamically imported**
  `livekit-client` `Room` (so the ~508 KB SDK is a lazy chunk, not in the main
  bundle) with `adaptiveStream`/`dynacast` + AEC/NS/AGC capture defaults. Roster,
  **active-speaker** (`ActiveSpeakersChanged`), and mute (`TrackMuted`/`Unmuted`)
  all come from Room events; identity = `user@network` (self via `isLocal`);
  `toggleMute` → `setMicrophoneEnabled`. Renegotiation + quality are the SDK's.
  Desktop reuses it via the webview (M-voice-3 mic grant). `voice-offer`
  ClientEvent + `weft.ts` type carry `mode`/`room`. *Green:* svelte-check 0/0 +
  web build; **runtime = real LiveKit + two browsers** (deferred).
- **M-lk-2 ✅ — moderation bridge.** `LiveKitAdmin` gained async
  `set_participant_muted` / `remove_participant` (the trait is now `#[async_trait]`,
  token minting stays sync). `LiveKitBackend` keeps a session→(room,identity) map
  from `join`, so the SFU-shaped `VoiceBackend::set_muted(session,…)` /
  `leave(session,…)` translate to the identity-keyed Room API without widening the
  trait. MUTE→mute (via M7 `mute_in_voice`), UNMUTE→unmute, leave/disconnect→
  remove. **Ban/kick** now also eject from voice: `on_moderate` (channel-scope
  ban) + `on_kick` → `eject_channel_voice` → `ctx.voice_eject_account` + backend
  `leave` + VOICE STATE leave. weftd's `LiveKitSigner` implements the port with
  `livekit_api::services::RoomClient`: mute = `update_participant` revoking
  `can_publish` (server-enforced, matches the grant model); remove =
  `remove_participant`. `services-tokio` + `rustls-tls-webpki-roots` resolve
  reqwest onto **ring** (no aws-lc-rs — policy preserved). *Green:* a
  `MockLiveKitAdmin` unit test asserts join/mute/unmute/remove hit the Room API
  with the right room+identity (and no-op for unknown/departed sessions); a
  session test asserts a channel-scope BAN ejects the target (co-member sees the
  VOICE STATE leave). weft-core 117 + voice_livekit 1; clippy clean.
- **M-lk-3a — federation foundation (proto-first, fully verifiable).** Manifest
  `voice`-mode (proto/crypto/store/core, mirrors `typing` end-to-end); the crypto
  voice token (weft-crypto, modeled on `SignedManifest`); `VOICE REQUEST`/
  `VOICE GRANT` bridge-verb codec; core `on_voice_request_in` returning a
  `VOICE GRANT` iff `voice=on` ∧ ¬netblocked ∧ token authority (else
  `NO-SUCH-TARGET`, invariant 1 timing). *Green:* codec round-trips + core tests +
  two-live-weftd conformance that a `VOICE REQUEST` is granted iff authorized and
  refused after `NETBLOCK` — **no media involved, all green here**.
- **M-lk-3b — the relay participant (deployment-verified).** weftd's `LiveKitAdmin`
  gains relay spawn/teardown; the relay (LiveKit Rust SDK) connects to F's LiveKit
  with the granted JWT and per-participant-forwards both ways; one-hop + loop
  prevention + debounced lifecycle + `SEVER`/`NETBLOCK` hard-stop. *Green here:*
  the lifecycle/teardown logic is unit-tested against `MockLiveKitAdmin`; **actual
  cross-network audio is verified on a real two-network deployment** (the one piece
  that can't be CI-green, flagged honestly).

## Risks / open questions

- **Second server to operate.** LiveKit (Go) beside weftd — deployment + config +
  the shared API secret. Mitigation: the `weft-rt` fallback for operators who
  refuse it; ship a compose file.
- **Relay transcode.** The relay decodes+republishes → one extra encode/hop at the
  bridge. Fine for Opus voice; note the added latency. (Investigate LiveKit
  forward-without-transcode if it bites.)
- **Trust of the LiveKit instance.** The token grants room access; weftd + LiveKit
  share a secret, so the LiveKit instance is part of the network's TCB — same
  trust boundary as weftd itself (the operator runs both).
- **Identity mapping.** LiveKit participant identity must be the canonical WEFT
  `user@network` so the client resolves avatars/display; the token sets it.
- **Fallback divergence.** Keeping `weft-rt` means two client paths (`mode`
  branch) to maintain. Acceptable while LiveKit is the default; revisit if the
  fallback is unused.

## Spec amendment (same-PR, per CLAUDE.md)

§16 gains the two media-transport modes (native WebRTC/`weft-rt` **and** LiveKit),
the `VOICE OFFER` `mode` + LiveKit token payload, the manifest `voice`-mode, and
the §11 **relay-participant** federation model (one hop, voice-token-authorized,
NETBLOCK-severed). Appendix A decision-history entry records the pivot rationale.
