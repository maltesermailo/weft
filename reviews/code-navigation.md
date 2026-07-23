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
| A verb the *client* must send | the full chain, in order: `weft-proto` (command + round-trip test) → `weft-store` trait + `memory.rs`/`postgres.rs` + a case in the shared `tests/backends.rs` contract → `weft-core/src/session/<area>.rs` handler + `session.rs` dispatch → `weft-client-core/src/lib.rs` `build_*` → **both** frontends (`weft-client-wasm/src/lib.rs` dispatch arm *and* a `#[tauri::command]` in `client/src-tauri/src/lib.rs` + its `generate_handler!` entry) → `client/src/lib/weft.ts` wrapper → `+page.svelte` action + `AppCtx` in `context.ts` → the component. Missing either frontend leaves web or desktop silently broken |
| §6.5 roles (define / order / rename / assign) | `weft-core/src/session/roles.rs` (all handlers); store in `RoleStore` (`traits.rs` + both impls; `rename_role` migrates definition **and** membership together); client UI is `client/src/lib/components/modals/RolesTab.svelte` (drag-to-reorder + inline editor), backed by `saveRole`/`reorderRoles`/`moveRole` in `+page.svelte`. Rename is a store migration, never delete+create — the latter drops every assignment |
| Social layer: group DMs | `GroupId` = `&<ulid>` target sigil (`Target::Group`) + `Scope::Group` (store key `&<ulid>`). Store: `GroupStore` (mem+PG, migration 0033: `weft_groups`+`weft_group_members`). Messaging rides the **directory** (`Cmd::GroupMsg`/`group_msg`/`deliver_many`) — single-writer ULID mint like DMs, NOT the channel actor. Membership handlers: `weft-core/src/session/groups.rs` (`on_group_create` mints `GroupId(Ulid::new())`, add/remove/leave/name broadcast via `directory.notify`). `on_msg`/`on_history` `Target::Group` = membership-gated, local-member fan-out (cross-network deferred). Client: `groups` state + `createGroup/openGroup/leaveGroup/groupLabel` in `+page.svelte`, groups in `dmList` (`DmList.svelte`), `FriendsView` create input, `ChatTopbar` group branch. |
| Social layer: friends (federation-able) | `FRIEND ADD/ACCEPT/REMOVE` + `FRIENDS` handlers in `weft-core/src/session/friends.rs` (`on_friend_*`); everything keys on `UserRef` (`account@network`) so local + cross-network share one path. Store: `FriendStore` (`traits.rs` + both backends, migration `0032`, symmetric one-row-per-pair with `requested_by`); `ctx.friends`. Same-network delivery = `directory.notify`; **cross-network delivery is deferred** (needs the bridge user-event transport — the §18 cross-network-DM primitive). Proto: `FriendState` enum + `Command::Friend*` + `Event::Friend`/`FriendRemoved`. No existence check (anti-enumeration). **Cross-network**: reuses the §11.10 FSession tunnel. Receive = `on_federated` runs friend cmds as the foreign caller (handlers take a `UserRef` caller, local or federated). Send = `deliver_if_remote` (friends.rs) emits `FriendDeliver` via `ctx.request_friend_deliver` (port like `mirror_tx`); weftd `dialer::spawn_friend_deliver_consumer`/`deliver_friend` dials a fresh authed bridge + tunnels `FSESSION OPEN/CMD` (SSRF-guarded, `auto_bridge=open`). Fire-and-forget; each network keeps its own edge copy. **Client**: `FriendsView.svelte` (home main pane when `homeView && !activeChannel`) + Friends button in `sidebar/DmList.svelte`; `friends` state + `addFriend/acceptFriend/removeFriend/messageFriend/openFriends` in `+page.svelte`; `weft.ts friendAdd/Accept/Remove/listFriends`; client-core `build_friend_*` + `ClientEvent::Friend`. |
| §9.4 threads (name / list) | `THREAD NAME`/`THREADS` handlers in `weft-core/src/session/relay.rs` (`on_thread_name` gates via `can_post` + `find_root`; `on_threads` = `BATCH` of `THREAD`). Store: `EventStore::channel_threads` (aggregates the existing `thread` column) + `set_thread_name` over `weft_thread_names` (migration `0031`). A thread name is metadata keyed by the root msgid — **no** new identity; threads stay "views, not channels". Client: `openThreads`/`renameThread`/`threadNames` in `+page.svelte`, `ThreadsModal.svelte` (list) + `ThreadPanel.svelte` (inline-editable title) |
| Link-preview / unfurl proxy | `weftd/src/unfurl.rs` — `GET /unfurl` (meta JSON) + `GET /unfurl/image` (image bytes), mounted in `lib.rs` (gated on `[unfurl] enabled`). **All fetches are SSRF-guarded** via `dialer::is_dialable` in `resolve_and_guard` (every resolved IP, every redirect hop) — the invariant-13 template is `dialer::fetch_signing_key`. Auth = the `/media` session bearer (`ctx.media_bearer_account`). Meta extraction (`parse_meta`) is a pure, panic-free, tested parser — no HTML-parser dep. Client: `weft.ts unfurl()`/`unfurlImageUrl()` + `LinkPreview.svelte` (rendered per-message in `MessageItem.svelte` for the first http(s) link) |
| CORS on the HTTP data plane | `weftd/src/cors.rs` — one permissive `from_fn` layer (`ACAO: *`, answers `OPTIONS` preflight) on the `/media` and `/unfurl` routers. Safe because those endpoints auth by query-string bearer, not cookies. **Without it, cross-origin uploads (custom `Content-Type`) fail preflight → the client sees `TypeError: Load failed`** (the avatar-upload bug) |
| New ERR code semantics | `weft-proto/src/errcode.rs`, then the `send_err` call sites in session.rs |
| Wire grammar/limits | `weft-proto/src/line.rs` (consts at the top) |
| Session states / idle limits | session.rs consts + `State` enum at the top |
| What gets stored / compaction semantics | `weft-store/src/materialize.rs` (never per-backend!) |
| Storage backend | implement the two traits in `weft-store/src/traits.rs`; `memory.rs` is the reference semantics |
| Channel behavior (ordering, fan-out) | `weft-core/src/channel.rs` |
| Config options | `weftd/src/config.rs` (serde) + `lib.rs :: start` wiring |
| Timeouts/keepalive | transport idle: `weft-transport/src/quic.rs :: transport_config`; app liveness: session.rs consts; client PING: `weft-tui/src/net.rs` |
| Load / throughput testing | `weftd/src/bin/loadtest.rs` — an in-process generator that drives the real session→actor→store→broadcast pipeline (no QUIC) via in-memory `ControlStream`s. `cargo run --release -p weftd --bin loadtest -- --channels 16 --senders-per-channel 1 --messages 20000`. Reports ingest events/s, fan-out deliveries/s, ack-latency percentiles. Per-channel ceiling ≈ single-writer actor rate; aggregate scales with channel count. Use 1 sender/channel for a clean ingest number (multi-sender/channel is a fan-out-contention stress test where broadcast lag drops copies — realistic §9.2 backpressure) |

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

## M-prof addendum — §10.3 display profiles (nick + avatar)

A profile = display name + avatar (the avatar's BLAKE3 hash → a `weft-media://`
blob). New load-bearing pieces:
- `weft-crypto/src/profile.rs` — `SignedProfile` (home-network-key-signed CBOR,
  avatar-hash-bound; models `manifest.rs`). Used at federation (M-prof-5).
- `weft-store` — `ProfileStore` + `ProfileRecord` (`kind`-less per-account row),
  migration 0022; `avatar_exists` powers the fetch gate + GC skip.
- `weft-core/src/session/profile.rs` — `on_profile_set` (partial update →
  `ctx.profiles.set_profile` → labeled ack + `announce_as` to co-members) and
  `on_profiles_query`. `ctx.profiles` is the port; `ServerCtx::may_fetch` lets any
  authed session fetch an avatar blob (§10.3 semi-public); `maintenance ::
  gc_orphan_blobs` skips avatar hashes so avatars aren't GC'd.

| Change | Touch |
|---|---|
| Profile wire form | `weft-proto` command.rs (`PROFILE SET`/`PROFILES`) + event.rs (`PROFILE`) **+ round-trip test first** |
| Profile storage | `weft-store` `ProfileStore`/`ProfileRecord` (mem + PG + migration + contract) |
| Profile authz/broadcast | `weft-core/src/session/profile.rs`; avatar fetch gate in `context.rs :: may_fetch`; GC skip in `maintenance.rs` |
| Profile federation | send: `session/federation.rs :: on_bridge_event` (signs + forwards `PROFILE sig=…`); receive: `on_bridge_line` routes `PROFILE`→`ingest_bridged`→`ingest_profile` (verify vs peer key + mirror avatar). `SignedProfile` in weft-crypto; `Event::Profile` carries a `UserRef` |
| Avatar rendering (client) | `Avatar.svelte` (image-or-initials, uses `app.avatarUrl`); `+page.svelte` `profiles` store + `avatarUrl`/`displayName`/`queryProfile`; edit in `UserSettingsModal` (`weft.profileSet` + `upload()`); wrappers in `weft.ts` (`profileSet`/`profilesQuery`/`avatarUrl`) |

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

**Voice channels are a distinct kind** (`ChannelKind` in weft-proto; a `kind`
column, migration 0021; `ChannelRecord.kind`). Voice channels are **voice-only**:
`relay.rs :: join_one` rejects a text JOIN to a `Voice` channel (→ NO-SUCH-TARGET,
which is also the IRC-invisibility guarantee — no weft-irc code). Kind is set at
`CHANNEL CREATE #chan voice` / `[[channels]]` config and advertised in
`CHANNEL-LAYOUT` (`kind=voice`).

Chain 9: a voice join — `session.rs` dispatch → `voice::on_voice_join`:
`registry.get` + `channel_kind == Voice` (else NO-SUCH-TARGET) → M7 `is_moderated`
ban/mute → `voice_caps` (`listen`/`speak` on a restricted channel) — **all
authority before the backend** (invariant 4) — → `ctx.voice_backend().join()` →
**`handle.subscribe()` + `spawn_forwarder`** (a voice channel isn't text-joined,
so the session *subscribes* to the broadcast for `VOICE STATE`, tracked in
`self.voice: HashMap<ChannelName, VoiceRoom>`) → `VOICE OFFER` (labeled ack) →
`announce_voice_state` → `ChannelHandle::announce_as(self.id, …)` (the actor's own
copy is skipped, the `Cmd::SetPolicy` pattern). `VOICE DESC` relays the SDP to the
backend and returns its answer. Disconnect: `cleanup` → `teardown_voice` per room
(aborts the forwarder + SFU-leaves).

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

weftd wiring (M-voice-1c): the `voice` Cargo feature gates the optional
`weft-rt` dep (default build pulls no webrtc). `weftd/src/lib.rs ::
build_voice_sfu` (two `#[cfg]` arms) constructs the SFU from `[voice]` config
(`weftd/src/config.rs :: Voice`); `start` advertises `features=voice` iff it came
up, then `ctx.set_voice_backend` installs it. Conformance:
`tests/conformance/main.rs` — `voice_disabled_by_default_is_unsupported` (always)
+ `voice_enabled_signaling_over_quic` (`#[cfg(feature = "voice")]`, run with
`--features voice`).

| Change | Touch |
|---|---|
| Channel kind (text/voice) | `weft-proto :: ChannelKind` + `CHANNEL CREATE`/`CHANNEL-LAYOUT`; store `kind` column (new migration) + `ChannelRecord`; the `join_one` reject + `on_voice_join` gate in weft-core |
| Voice signaling authz | `weft-core/src/session/voice.rs` (never the SFU) |
| Voice roster / snapshot / live-mute | `ServerCtx.voice_rooms` + `voice_room_join`/`leave`/`voice_set_muted` (context.rs); snapshot in `on_voice_join`; `mute_in_voice` (voice.rs) called from `on_moderate`; SFU drop = `WebrtcSfu::set_muted` (per-publisher `AtomicBool`) |
| The SFU seam / a new backend | implement `VoiceBackend` (`weft-core/src/voice.rs`); native default in `weft-rt` |
| LiveKit voice backend (M-lk-0) | `weft_core::LiveKitBackend` (voice.rs) mints via the `LiveKitAdmin` port; weftd's `LiveKitSigner` (`weftd/src/livekit.rs`) uses `livekit-api`'s `AccessToken`/`VideoGrants`; selected by `[voice] backend="livekit"` in `build_voice_backend` (weftd/lib.rs); `VOICE OFFER` `mode`/`room` carry it |
| LiveKit client (M-lk-1) | `client/src/lib/voice.svelte.ts` branches on `mode`: `onLiveKitOffer` dynamically imports `livekit-client`, connects a `Room`, mirrors roster/active-speaker/mute from Room events; `onWebrtcOffer` = the old SFU path. Same `voice` `$state` + `VoiceBar.svelte` for both |
| LiveKit moderation (M-lk-2) | `LiveKitAdmin` async `set_participant_muted`/`remove_participant` (voice.rs); `LiveKitBackend` session→(room,identity) map routes `set_muted`/`leave`; ban/kick → `eject_channel_voice` (session/voice.rs) + `ctx.voice_eject_account`; weftd `LiveKitSigner` impl = `RoomClient.update_participant`/`remove_participant` |
| Federated voice foundation (M-lk-3a) | Manifest `voice`-mode = a `voice: bool` mirroring `typing` (crypto `manifest.rs`, proto `Event::Manifest`/`BRIDGE PROPOSE`, core `bridge::build_manifest`); crypto `SignedVoiceRelayGrant` (`weft-crypto/src/voice.rs`); `VOICE REQUEST`/`VOICE GRANT` verbs; gating in `on_voice_request_in` (session/federation.rs) using `bridge::is_forwardable` + manifest voice flag + `VoiceBackend::relay_grant` |
| Federated voice relay lifecycle (M-lk-3b) | `VoiceRelay` trait + `RelaySpec` (weft-core `voice.rs`); `ServerCtx.voice_relays` refcount + `relay_acquire`/`relay_release`/`relay_drop_peer` (context.rs); `SEVER`/`NETBLOCK` teardown in `on_bridge_sever_in`/`on_netblock_add`; weftd no-op `LogRelay` (`weftd/src/livekit.rs`). **Real libwebrtc media driver = deferred deployment dep** |
| Account verification (§10.5) | `VERIFY EMAIL/CONFIRM/BIRTHDAY/LIST` handlers in `weft-core/src/session/verify.rs`; `Mailer` port (`weft-core/src/mailer.rs`); code store + `verify_send_code`/`verify_check_code` in context.rs; claims via `Accounts` → `AccountStore.upsert/confirm_verification`; weftd `SmtpMailer`/`LogMailer` (`weftd/src/mailer.rs`, `lettre` + `[smtp]` config); client `verify*` in weft.ts |
| Voice wire form | `weft-proto` command.rs/event.rs **+ round-trip test first** |
| Voice config / enabling | `weftd/src/config.rs :: Voice` + `lib.rs :: build_voice_sfu`; the `voice` feature in `weftd/Cargo.toml` |
| The SFU media engine (forwarding, codecs, ICE) | `weft-rt/src/sfu.rs` — run its tests with `cargo test -p weft-rt` |
| Web voice UI / browser WebRTC | `client/src/lib/voice.svelte.ts` (the `$state` controller: getUserMedia + RTCPeerConnection + the JOIN→OFFER→DESC handshake) + `components/VoiceBar.svelte`; wired in `routes/+page.svelte` (`initVoice` on connect, `<VoiceBar>` in the members aside) |
| Web voice wire glue | `weft-client-core/src/lib.rs` (`ClientEvent::Voice*` + `build_voice_*`) + `weft-client-wasm/src/lib.rs` dispatch + `client/src/lib/weft.ts` (`WeftEvent` union + `voice*` wrappers) |
| Desktop voice (Tauri) | webview WebRTC — reuses `voice.svelte.ts`; `client/src-tauri/src/lib.rs` `voice_*` commands + `grant_media_permission` (`with_webview`, Linux WebKitGTK) + `Info.plist` mic string. Audio quality knobs (AEC/NS/AGC + Opus FEC/DTX) in `voice.svelte.ts` |

## WEFT Console addendum — `weft-admin` (the operator web panel)

Operator-only web admin (`docs/web-admin-panel-plan.md`). An axum router +
embedded SPA over the store roles — never speaks the wire protocol. weftd mounts
it on the HTTP listener (`[admin] enabled`), sharing the in-process stores +
live registry.

| Change | Touch |
|---|---|
| A new admin endpoint | `weft-admin/src/handlers.rs :: routes()` (all under `/admin/api/v1/*`) + a handler fn; reads go straight to a store role on `AdminState`, live actions via the `Live` port. Responses are typed `#[derive(Serialize)]` structs in `weft-admin/src/dto.rs` (add one + a `From<StoreRecord>`), never ad-hoc `json!` |
| Admin auth / session cookie | `weft-admin/src/auth.rs` (HMAC over `account\|exp`; `require_admin` middleware authenticates + injects the acting `Account` **and** its `AdminScopes`) |
| Admin RBAC (WC2) | `auth::AdminScope` (`admin.read/moderate/destroy/federation/keys`) + `admin_scopes()` (operators→all; else `admin`-scope capability grants by ULID, `*`/`admin.*`→all). Middleware enforces the `admin.read` baseline; each write handler calls `require(&scopes, AdminScope::…)` → 403. Delegate an admin via `GRANT admin admin.moderate <account>`. `/me` returns held scopes; the SPA hides controls via `can()` |
| Account soft-delete (WC3) | `AccountStore::schedule_deletion`/`cancel_deletion`/`deletion_scheduled`/`due_deletions` (migration `0024`, `purge_at_ms`); `DELETE /accounts/:name?confirm=<name>` schedules (typed-name), `POST /accounts/:name/restore` cancels; finalized by `weft_core::maintenance::purge_due_deletions` (in the maintenance loop). Grace = `AdminState.delete_grace_ms` from `[admin] delete_grace_days`. SPA danger-zone in `openUser` |
| Lookup depth (WC4) | User detail adds devices (`AccountStore::devices` → `device_fingerprint`), a flags card, and "find related" (`AccountStore::accounts_by_email_domain` → `account_detail.related`). Channel detail = `GET /channels/:name/detail` (policy + `MembershipStore::members`), SPA `openChannel`. DM-thread browse = `GET /dms/:a/:b/messages` (`browse_dm`, `Scope::dm`); e2ee gate via `AdminState.dm_policy` → `dto::ThreadBrowse.unavailable`, SPA `browseDm`. Deferred: IP-pivot, join-path, media footprint, per-peer replication |
| Federation ops (WC5) | Peer detail = `GET /peers/:name/detail` (`peer_detail`): parses `weft_crypto::SignedManifest::from_b64` → pinned key `fingerprint_hex` + `verified`, shared channels, history/media/typing/voice; + `is_netblocked`. SPA `openPeer`. Sever/re-weave reuse the NETBLOCK endpoints (a netblock *is* the §11.6 sever). Deferred: RTT/handshake, transit queue, force-re-handshake, key-rotation TOFU review |
| Trust & keys (WC6) | Token inspector = `POST /tokens/inspect` (`inspect_tokens`): `weft_crypto::Token::from_b64` per link → issuer/subject/scope/caps/epoch/expiry + `expired`/`rooted`/`parent_linked`/`revoked` (vs `scope_epoch`); SPA `inspectTokens`. Revocations = `GET`/`POST /revocations` (`scope_epoch` + `bump_epoch`, audited); SPA `revocations` screen. Both `admin.keys`. Deferred (E2EE): device registry, MLS leaves, propagation status |
| Account suspend (WC7) | `AccountStore::set_suspended`/`is_suspended` (migration `0025`). **Enforced at `weft-core session/auth.rs :: welcome_authed`** (the single AUTH chokepoint) → uniform AUTH-FAILED; also blocks the admin panel login. Admin `POST /accounts/:name/suspend`\|`/unsuspend` (`admin.moderate`, audited, no-self-suspend); `Accounts::set_suspended`/`is_suspended` passthroughs. SPA "Account moderation" card in `openUser`. Wire test: conformance `suspended_account_cannot_authenticate`. Deferred: forced live-session logout, shadow-limit, room actions |
| A live action (kick/eject, delete-any) | the `Live` trait (`weft-admin/src/lib.rs`); weftd's adapter = `LiveRegistry` (`weftd/src/lib.rs`) over the channel registry |
| The SPA | `weft-admin/ui/index.html` (`include_str!`; single `const API = "/admin/api/v1"` fetch base). Design target: `design/admin/` (`weft.css` + templates) |
| Audit trail (WC1) | `AuditStore` (`weft-store/src/traits.rs`) + `AuditEntry`/`AuditRecord`/`audit_hash` (shared pure blake3 chain, `types.rs`), mem + PG (advisory-lock append) + migration `0023_audit`; every write handler emits via `handlers.rs :: audit()` (payload digested, never raw); `GET /admin/api/v1/audit`. Contract: `backends.rs` audit block; e2e: `weft-admin/tests/api.rs :: write_actions_land_in_the_audit_log` |
| A new store role on the panel | add the `Arc<dyn …>` field to `AdminState` + its `from_store` bound (`weft-admin/src/lib.rs`), and to weftd's generic store bound in `run`/`serve` (`weftd/src/lib.rs`) |
| Cutting a live session (forced logout) | each `Session` owns a `close: CancellationToken` registered with the account directory (`weft-core/src/directory.rs`, `SessionEntry`); `ServerCtx::disconnect_account` cancels every token for an account. Sessions exit via the normal `cleanup`, so a cut looks like an ordinary disconnect (presence offline + voice leave, membership retained) — not a `MEMBER part` |
| Who may do what in the panel (WC2) | scopes are `auth::AdminScope`; a request's set comes from `auth::admin_scopes` (operator ⇒ all, else the `admin`-scope grant keyed by account ULID). Write handlers gate with `require(&scopes, …)`. **Changing** permissions is gated by `auth::is_operator` instead — a delegated `admin.*` grant holds every scope, so a scope gate would allow self-promotion |
