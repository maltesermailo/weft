# weftd — Rust Reference Server: Core Design v0.1

## 1. Workspace Layout

Cargo workspace, five library crates + one binary. Strict layering: dependencies point downward only, no cycles.

```
weftd/
├── Cargo.toml                      # [workspace] members
├── crates/
│   ├── weft-proto/                 # LAYER 0 — wire protocol (leaf, no internal deps)
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── line.rs             # control-plane line codec: @tags VERB params :trailing
│   │       ├── verb.rs             # Verb enum (client→server) + parse/serialize
│   │       ├── event.rs            # Event enum (server→client) + serialize
│   │       ├── tags.rs             # tag map, escaping rules
│   │       ├── msgid.rs            # OriginScopedUlid: <network-id>/<ulid>
│   │       ├── policy.rs           # RetentionPolicy enum (Ephemeral/Retained/Permanent/E2ee)
│   │       └── limits.rs           # 8 KiB line cap, param counts, validation
│   │
│   ├── weft-crypto/                # LAYER 0 — identity & capability tokens (leaf)
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── keys.rs             # Ed25519 wrappers (device keys, network signing key)
│   │       ├── attestation.rs      # signed {pubkey, account, network, expiry}
│   │       ├── captoken.rs         # CBOR token: subject/channel/caps/expiry/chain
│   │       ├── caps.rs             # Capability enum incl. Grant(Box<Capability>)
│   │       ├── chain.rs            # delegation-chain verification up to root key
│   │       └── epoch.rs            # revocation epochs per channel
│   │
│   ├── weft-store/                 # LAYER 1 — persistence (deps: proto)
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── traits.rs           # EventStore, AccountStore, TokenIndex, MediaStore
│   │       ├── postgres/
│   │       │   ├── mod.rs          # sqlx PostgreSQL impl of all traits
│   │       │   ├── schema.rs       # migrations (events, tombstones, accounts, tokens)
│   │       │   └── retention.rs    # purge task: honors per-channel policy
│   │       └── memory.rs           # in-mem impl for tests + `ephemeral`-only deployments
│   │
│   ├── weft-core/                  # LAYER 2 — domain logic (deps: proto, crypto, store)
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── context.rs          # ServerCtx: shared state + ports; Actor + cap enforcement
│   │       ├── session.rs          # session engine: FSM (State), run loop, verb dispatch
│   │       │                       #   (on_ready/on_negotiating/…), response helpers, runners
│   │       ├── session/            # handler groups (one `impl Session` per protocol surface;
│   │       │   │                   #   methods `pub(super)`, split out of session.rs)
│   │       │   ├── auth.rs         #   REGISTER + authed WELCOME (§6.1)
│   │       │   ├── relay.rs        #   JOIN/PART/MSG/EDIT/DELETE/REACT/HISTORY/PIN/MARK (§9)
│   │       │   ├── caps.rs         #   CAPS/GRANT/REVOKE — token mint + grant store (§10.4)
│   │       │   ├── channels.rs     #   CHANNEL CREATE/POLICY/META/DELETE/RENAME (§6.3)
│   │       │   ├── namespaces.rs   #   NS CREATE/META/VISIBILITY/DELETE/RECOVERY/JOIN (§6.2/§2.4)
│   │       │   ├── invites.rs      #   INVITE MINT/REVOKE/REDEEM (§6.5)
│   │       │   ├── roles.rs        #   ROLE CREATE/DELETE/ASSIGN/UNASSIGN (§6.5)
│   │       │   ├── moderation.rs   #   MUTE/BAN/KICK + REPORT/REPORTS (§6.7)
│   │       │   └── federation.rs   #   bridge auth/sessions, ingest/forward, NETBLOCK,
│   │       │                       #     FEDERATE, FSESSION tunnel + federated dispatch (§11)
│   │       ├── channel.rs          # channel actor: members, policy, ULID order, event fan-out
│   │       ├── registry.rs         # ChannelName → actor handle map (lazy spawn)
│   │       ├── directory.rs        # account directory actor: DMs, presence, MARK sync
│   │       ├── accounts.rs         # account registry (password hashing, ULID, device enroll)
│   │       ├── bridge.rs           # §11 manifest build/verify + one-hop forwarding helpers
│   │       ├── maintenance.rs      # retention purge + §12.1 compaction scheduler
│   │       └── stream.rs           # ControlStream port (transport-facing; weftd/IRC adapt it)
│   │
│   ├── weft-transport/             # LAYER 2 — transports (deps: proto)
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── traits.rs           # ControlStream + DataStream abstractions
│   │       ├── quic.rs             # quinn: stream 0 control, uni streams data
│   │       ├── ws.rs               # WebSocket fallback: text/binary frame mux
│   │       └── framing.rs          # 4-byte virtual stream ID for WS binary frames
│   │
│   └── weftd/                      # LAYER 3 — the binary (deps: everything)
│       └── src/
│           ├── main.rs             # tokio runtime, listener spawn, shutdown
│           ├── config.rs           # TOML: network id, keys, transports, storage
│           ├── acceptor.rs         # per-connection task spawn, transport negotiation
│           ├── wellknown.rs        # axum: /.well-known/weft (signing key, endpoints)
│           ├── media.rs            # data-plane upload → content-hash store
│           └── telemetry.rs        # tracing subscriber, qlog hook for QUIC debug
│
└── tests/
    ├── conformance/                # black-box protocol tests over real QUIC
    └── fixtures/                   # golden files for line codec round-trips
```

> **Reading weft-core.** Start in `session.rs`: `Session<S>` (fields + `State`),
> the `run` loop's `select!`, and `on_request` — the verb → handler dispatch.
> Each match arm calls a handler that lives in a `session/*.rs` module grouped by
> protocol surface (find `MUTE` in `moderation.rs`, `GRANT` in `caps.rs`, a
> bridge verb in `federation.rs`). The submodules are descendants of `session`,
> so they read `Session`'s private fields and the response helpers
> (`send_event`/`send_err`/`cap_required`) that stay in `session.rs`; their
> handlers are `pub(super)` so the dispatch can reach them. Enforcement
> (`actor_has_cap`, invariant 4) and the `Actor` type live in `context.rs`.
>
> *(The layout above is current for weft-core; other subtrees in this v0.1 sketch
> may still read aspirationally, e.g. `verb.rs` is `command.rs`.)*

## 2. Dependency Graph

```
                weftd (bin)
              /      |       \
     weft-transport  weft-core   (axum for well-known only)
              \      /    \   \
             weft-proto  weft-crypto  weft-store
                                          |
                                      weft-proto
```

Rules:
- `weft-proto` and `weft-crypto` are leaves — pure logic, no I/O, no tokio. Fully testable with plain `#[test]`, fuzzable with cargo-fuzz (the line codec and CBOR token parser are the two fuzz targets that matter).
- `weft-core` never touches sockets; it speaks through the `ControlStream` trait and `EventStore` trait. This makes the entire domain layer testable with the in-memory store and a mock stream — no network in unit tests.
- `weft-transport` never interprets verbs; it only frames bytes into lines/streams.
- Only `weftd` knows about config files, TLS certs, and process lifecycle.

## 3. Concurrency Model

**Actor-per-channel, task-per-connection**, communicating over `tokio::mpsc`.

```
Connection task (per client)
   │  parses lines → Verb (weft-proto)
   ▼
Session (weft-core)          — auth state, token cache, rate limiter
   │  routed commands
   ▼
Channel actor (weft-core)    — single task owns channel state
   │  broadcast::Sender<Event>
   ▼
All member sessions subscribe → serialize Event → write to their stream
```

- A channel actor is the sole owner of its member list and policy → no locks on the hot path, ordering is trivially consistent (the actor's inbox *is* the event order, which is what assigns ULIDs).
- `registry.rs` holds `DashMap<ChannelId, ChannelHandle>`; actors are spawned lazily on first JOIN and parked after last PART (persistent channels keep a tombstone entry).
- Backpressure: bounded mpsc inboxes; a slow client gets its broadcast receiver lagged → session detects `RecvError::Lagged` → sends `ERR SLOW` and forces a HISTORY resync rather than buffering unboundedly. This is the netsplit-analog failure mode, made explicit.
- Bridges are just sessions with a `bridge` capability token — remote networks connect through the same acceptor path, massively reducing special-casing.

## 4. Key Crate Choices

| Concern | Crate | Note |
|---|---|---|
| Runtime | `tokio` | multi-thread, `rt-multi-thread` |
| QUIC | `quinn` | rustls-based; qlog support for debugging |
| TLS | `rustls` + `rcgen` | rcgen for dev self-signed |
| WS fallback + well-known | `axum` | one HTTP surface, keeps hyper out of core |
| Serialization (tokens) | `ciborium` + `ed25519-dalek` | deterministic CBOR encode before sign |
| IDs | `ulid` | monotonic generator per channel actor |
| Storage | `sqlx` (PostgreSQL) | pooled connections; proper concurrent writers for busy networks — the memory backend keeps the zero-dependency dev/`ephemeral` story |
| Errors | `thiserror` (libs) / `anyhow` (bin) | |
| Config | `serde` + `toml` | |
| Observability | `tracing` | span per connection, per verb |

Deliberately deferred: `openmls` (E2EE, feature-flag `e2ee`), SFU/voice (separate `weft-rt` crate later), SQLite backend (trait already allows it; PostgreSQL chosen 2026-07 — reversal of the original single-file choice).

## 5. Build Order (suggested milestones)

1. **M0 — codec**: `weft-proto` complete + fuzz targets green. Round-trip golden tests.
2. **M1 — echo server**: weftd over QUIC+WS, HELLO/AUTH(anon)/JOIN/MSG relay, `ephemeral` only. *This is already a usable IRC replacement.*
3. **M2 — identity**: weft-crypto attestations, AUTH with keypair proof, well-known endpoint.
4. **M3 — persistence**: postgres store, `retained`/`permanent` policies, HISTORY, EDIT/DELETE/REACT materialization.
5. **M4 — capabilities**: token minting, GRANT/REVOKE, refresh cycle, revocation epochs.
6. **M5 — bridging**: BRIDGE handshake, remote ingestion, strictest-policy negotiation.
7. **M6+**: media data-plane, threads filter, E2EE flag, weft-rt.

Each milestone is independently shippable and testable via the conformance suite.
