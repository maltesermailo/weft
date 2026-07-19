# Code Navigation Guide — weftd

*How to find your way around, written after M3a (134 tests). File sizes and
function names are accurate as of this milestone; line numbers will drift,
so pointers are `file :: function` — grep for the function name.*

## The 30-second map

Requests flow **down** through the layers; events flow **back up**. Every
crate boundary is also a testing boundary.

```
weftd        glue: config, key files, TLS, accept loops     (~450 lines total)
  │
weft-transport   bytes → lines (QUIC framing, WS frames)    never parses verbs
  │
weft-core        the actual server: sessions, actors        never touches sockets
  │         ├── weft-crypto   keys, attestations, argon2    pure, no I/O
  │         └── weft-store    EventStore/AccountStore,      pure logic + memory impl
  │                           §12.1 materialization
weft-proto       the wire: Line grammar, Command, Event     pure, fuzzable
```

Biggest files, where you'll spend most time:
`weft-core/src/session.rs` (~1150 — the protocol brain),
`weft-proto/src/event.rs` (~920) and `command.rs` (~700 — mechanical
parse/serialize arms), `weft-core/src/channel.rs` (~410 — the actor).

## Chain 1: boot — `main` to a listening server

1. `weftd/src/main.rs :: main` — parses argv, loads TOML, calls `start`.
2. `weftd/src/lib.rs :: start` — the whole boot recipe in one function,
   top to bottom: validate network/channel names → load-or-generate the
   signing key (`load_or_generate_key`) → build `ServerCtx` → TLS
   (`load_tls` PEM or `self_signed` rcgen) → QUIC endpoint → spawn accept
   loops (+ optional WS, + optional well-known HTTP).
3. `weft-core/src/context.rs :: ServerCtx::new` — wires the store into
   `Accounts` and hands it to `Registry::spawn`.
4. `weft-core/src/registry.rs :: Registry::spawn` → one
   `channel::spawn` per configured channel — **channel actors already
   exist before the first connection arrives**.

## Chain 2: a connection — accept to session loop

1. `weftd/src/acceptor.rs :: accept_quic` — one spawned task per
   connection; QUIC handshake, then `QuicControlStream::accept` waits for
   the client to open the control stream.
2. Same file, `QuicLines` / `WsLines` — the ~10-line adapters that turn a
   transport stream into weft-core's `ControlStream` trait
   (`weft-core/src/stream.rs`). This is the only place transport and core
   meet.
3. `weft-core/src/session.rs :: run_session` — entry point; makes a
   `Session`, runs it, cleans up (parts channels, flushes the stream).
4. `Session::run` — **the select loop**. Three wake sources, one each:
   inbound line, queued channel event, idle deadline. Everything the
   session ever does starts here.

## Chain 3: inbound — a line becomes an action

Follow a `MSG #general :hi` from socket to actor:

1. `Session::run` → `on_line` — two-stage parse:
   `Line::parse` (grammar, `weft-proto/src/line.rs`) then
   `Request::from_line` (typed verb, `weft-proto/src/command.rs`).
   Parse failures → `on_malformed` (5 strikes/60 s closes).
2. `on_request` — the FSM gate: dispatches on `self.state`
   (`Negotiating | Unauthed | Ready`). Unknown verbs are dropped here,
   before any state logic (§4).
3. `on_ready` — the verb → handler match. Every READY verb has an
   `on_<verb>` method below it in the same file.
4. `on_msg` — session-side checks in order: target kind → attachments →
   empty body → membership → **label dedup** (§9.2, the `dedup` map) →
   `push pending label` → `ChannelHandle::publish`.
5. `weft-core/src/channel.rs :: Actor::handle(Cmd::Publish)` — the single
   writer: `mint()` assigns the msgid (the ONLY place msgids are born),
   `persist()` appends to the store (skipped for ephemeral), `broadcast()`
   fans out.

EDIT/DELETE/REACT take the same shape with one extra hop:
`on_edit`/`on_delete`/`on_react` → `resolve_message` (the shared
origin/existence/tombstone/membership/authorship checks) → actor.

## Chain 4: outbound — an event becomes bytes (the "main to event" chain)

This is the fan-out path; read it once and the concurrency model is clear:

1. `Actor::broadcast` sends `ChannelEvent { origin, event }` into the
   channel's `tokio::broadcast` ring (512 slots).
2. Each member session has a **forwarder task** pumping that ring into the
   session's own bounded queue —
   `weft-core/src/session.rs :: spawn_forwarder` (bottom of the file).
   Lag here becomes `SessionEvent::Lagged` → `ERR SLOW` (§9.2). Forwarders
   are created in `on_join`, aborted in `on_part`/`cleanup`.
3. Back in the select loop, `Session::on_event`:
   - `origin != me` → serialize with **no label** (broadcast copy, §3.5);
   - `origin == me` and it's MESSAGE/EDITED/DELETED/REACTION → pop the
     per-channel `pending` label FIFO → this copy **is the ack** (§9.2),
     and labeled MSG echoes are cached in `dedup` for retry replay.
4. `Reply::serialize` (`weft-proto/src/event.rs`) → `stream.send_line` →
   transport framing (`weft-transport/src/quic.rs` LinesCodec / `ws.rs`
   text frame) → wire.

Why the label FIFO is safe: one mpsc into one actor preserves a session's
own command order across all four event types, so echoes come back in send
order. That argument is written down at `struct Joined` in session.rs.

## Chain 5: HISTORY — the read path (bypasses the actor)

`on_history` (session.rs) → membership + policy checks →
`ctx.events.roots/children` (trait: `weft-store/src/traits.rs`, impl:
`memory.rs`) → **`weft-store/src/materialize.rs :: materialize`** — the
§12.1 pure function, the most invariant-dense code in the repo — → batch
events, every line labeled. Reads never touch the channel actor; only
writes need its ordering.

## Chain 6: auth — UNAUTHED to READY

`session.rs :: on_unauthed` is the seam. REGISTER/AUTH PASSWORD →
`weft-core/src/accounts.rs` (uniformity semantics: dummy-hash for unknown
accounts) → `AccountStore` + `weft-crypto/src/password.rs` (argon2).
AUTH KEY/PROOF → `weft-crypto/src/challenge.rs` (nonce‖network) →
`ctx.mint_attestation` (`context.rs`) → `weft-crypto/src/attestation.rs`.
The public half of the signing key is served by
`weftd/src/wellknown.rs`.

## "I want to change X — where do I go?"

| Change | Touch (in order) |
|---|---|
| New verb/event | `weft-proto` command.rs/event.rs **+ round-trip test first** (CLAUDE.md rule), then session.rs handler |
| New ERR code semantics | `weft-proto/src/errcode.rs`, then the `send_err` call sites in session.rs |
| Wire grammar/limits | `weft-proto/src/line.rs` (consts at the top) |
| Session states / idle limits | session.rs consts + `State` enum at the top |
| What gets stored / compaction semantics | `weft-store/src/materialize.rs` (never per-backend!) |
| Storage backend | implement the two traits in `weft-store/src/traits.rs`; `memory.rs` is the reference semantics |
| Channel behavior (ordering, fan-out) | `weft-core/src/channel.rs` |
| Config options | `weftd/src/config.rs` (serde) + `lib.rs :: start` wiring |
| Timeouts/keepalive | transport idle: `weft-transport/src/quic.rs :: transport_config`; app liveness: session.rs consts; client PING: `weft-tui/src/net.rs` |

## Test map — which suite proves what

| Suite | Command | Proves |
|---|---|---|
| Proto round-trips | `cargo test -p weft-proto` | every wire form parse↔serialize |
| Crypto | `cargo test -p weft-crypto` | sign/verify, replay rejection, expiry |
| Store + materialization | `cargo test -p weft-store` | §12.1 invariants, paging, purge watermark |
| Core (networkless) | `cargo test -p weft-core` | the whole domain over a mock `ControlStream` — FSM, auth, relay, mutations, HISTORY |
| Conformance (black-box) | `cargo test -p weftd` | real QUIC + WS against an in-process server |
| Slow idle regression | `cargo test -p weftd --test conformance -- --ignored` | keepalive survives long quiet gaps |

The layering is the debugging strategy: a failing conformance test with
green core tests means transport/glue; failing core with green proto means
session/actor logic; and so on down.

## Reading order for a newcomer

1. `docs/weft-protocol-spec.md` §3–§9 (client-side sections) — 20 minutes.
2. `weft-proto/src/lib.rs` doc comment, then skim `line.rs`.
3. `weft-core/src/session.rs` top-of-file comment + `Session::run` +
   `on_request` — the FSM shape.
4. `weft-core/src/channel.rs` — the actor; now you know the write path.
5. `weft-store/src/materialize.rs` — read the tests before the code.
6. Everything else on demand via the chains above.

## M3b addendum — new files, new chains

New load-bearing files:
- `weft-core/src/directory.rs` — the account→sessions actor: DM delivery
  and MARK sync. Sessions register in `welcome_authed`, deregister in
  `cleanup`; events arrive via the session's 4th select arm (`on_direct`).
- `weft-core/src/maintenance.rs` — the purge/compaction loop weftd spawns.
- `weft-store/src/compact.rs` — `compaction_plan`, the §12.1 audit-window
  pure function (read its tests first, like materialize).
- `weft-store/src/postgres.rs` + `migrations/` — the sqlx backend. It
  contains **no semantics**: materialize/compaction_plan stay shared, and
  `tests/backends.rs` runs one contract suite against both backends.

Chain 7: a DM — `on_msg(Target::User)` → `Directory::dm` (existence check,
mint, persist, fan out to every session of both accounts) → each session's
`on_direct` (same origin/label echo rule as channels, separate
`pending_direct` FIFO).

Chain 8: boot with Postgres — `weftd::start` → backend match →
`boot()` helper: **upsert config channels → `list_channels()` → registry**
(the store, not the config, is the source of truth) → `spawn_maintenance`.

| Change | Touch |
|---|---|
| Storage schema | new file in `weft-store/migrations/` (never edit applied ones) + both backends + `tests/backends.rs` |
| Compaction semantics | `weft-store/src/compact.rs` only |
| DM behavior | `directory.rs` + session `on_direct`/`on_msg` |
| Verification kinds/flows | store substrate exists (`Verification`); wire flow = spec decision first |

## M-voice addendum — §16 WEFT-RT voice signaling (M-voice-0/1a)

Voice is a **projection over the same session/actor machinery**, not a new
server. The media plane (an SFU) is separate — see below.

New load-bearing files:
- `weft-proto/src/command.rs` + `event.rs` — the `VOICE JOIN/LEAVE/DESC/CAND`
  verbs and `VOICE OFFER`/`VOICE STATE`/`VOICE DESC`/`VOICE CAND` events. `DESC`
  is symmetric (command = client offer, event = SFU answer); raw SDP rides the
  trailing (CR/LF auto-escaped, same as a message body — no base64).
- `weft-core/src/voice.rs` — the **`VoiceBackend` port** (the pluggable-SFU
  seam): `Arc<dyn VoiceBackend>` (async-trait) with `join`/`describe`/
  `candidate`/`leave`. Held as an optional `OnceLock` on `ServerCtx`; weftd
  installs one via `set_voice_backend` (like the mirror/backfill sink ports).
  `None` = zero-voice server → voice verbs answer `UNSUPPORTED`.
- `weft-core/src/session/voice.rs` — the handlers (`Session::on_voice_*`).

Chain 9: a voice join — `session.rs` dispatch → `voice::on_voice_join`:
membership (`self.joined`) → M7 `is_moderated` ban/mute (`covering_scopes`) →
`voice_caps` (`listen`/`speak` on a restricted channel) — **all authority before
the backend** (invariant 4) — → `ctx.voice_backend().join()` → `VOICE OFFER`
(labeled ack) → `announce_voice_state` → `ChannelHandle::announce_as(self.id, …)`
(broadcasts `VOICE STATE` to co-members; the actor's own copy is skipped, the
`Cmd::SetPolicy` pattern). `VOICE DESC` relays the SDP to the backend and returns
its answer. Disconnect: `cleanup` → `teardown_voice` per room.

The **SFU media engine is not here** — `weft-core` never touches a socket. The
`WebrtcSfu` (webrtc-rs) implementing `VoiceBackend` lives in the `weft-rt` crate
(below) and owns the UDP/DTLS/ICE; `on_voice_*` only carry SDP/ICE to it.

The media plane — `weft-rt` (M-voice-1b), a **`members`-but-not-`default-members`**
crate (webrtc 0.17.1; only built with weftd's `voice` feature):
- `weft-rt/src/sfu.rs` — `WebrtcSfu` (the reference `VoiceBackend`). One shared
  `webrtc::API` (MediaEngine+Opus, pinned UDP range); a `rooms:
  Mutex<HashMap<ChannelName, Room>>`, each `Room` = per-session PeerConnections +
  per-session `TrackLocalStaticRTP` publishers. `join` sets `on_track` → mirror
  inbound Opus into a local track + pump RTP to it (webrtc rewrites SSRC/PT per
  subscriber binding = verbatim fan-out). `describe` = **`add_track` the existing
  publishers BEFORE `set_remote_description`** (the ordering that binds the
  sender — the reverse leaves it paused and forwards zero bytes) → non-trickle
  gather+answer. Tests (`weft-rt/tests/sfu.rs`) drive real webrtc client PCs over
  loopback (host ICE, no STUN); one asserts a gathered answer, one asserts Opus
  actually forwards publisher→subscriber.

| Change | Touch |
|---|---|
| Voice signaling authz | `weft-core/src/session/voice.rs` (never the SFU) |
| The SFU seam / a new backend (e.g. LiveKit) | implement `VoiceBackend` (`weft-core/src/voice.rs`); the default lives in `weft-rt` |
| Voice wire form | `weft-proto` command.rs/event.rs **+ round-trip test first** |
