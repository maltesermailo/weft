# Modular monolith & scaling plan for weftd

Status: **proposal — not started.** Decision points for Jannik are marked ⚖ throughout and
collected in §9. Nothing here changes the wire protocol; everything below is deployment
topology and internal plumbing.

## 0. Goal

Turn weftd from a single process into a **modular monolith**: one binary, composed of
modules ("roles") that can all run in one process (today's behavior, forever the default)
or be started separately and talk to each other over an internal fabric. The point is
operational: scale the part that is hot (connections, media bytes, federation dials)
without redesigning the core, and keep `weftd --config weftd.toml` working unchanged for
small deployments.

Guiding principles, in priority order:

1. **The single-process path stays first-class.** All-roles-in-one-process is the default
   and must never get slower or more complex to operate. Role separation is opt-in config.
2. **One binary.** `weftd` grows a `[node]` config section selecting roles; there is no
   second binary, no orchestration requirement, no service discovery dependency. A cluster
   is N copies of weftd with different `roles = [...]` lines.
3. **Preserve the ordering invariant.** ULIDs are minted only by the channel actor
   (spec §9.1); each channel actor exists exactly once cluster-wide. Sharding moves
   actors between nodes; it never duplicates them.
4. **Cut along seams that already exist.** `ControlStream`, the `Arc<dyn Store>` traits,
   the OnceLock sink ports (`auto_bridge_tx`/`mirror_tx`/`backfill_tx`), `VoiceBackend`,
   `Mailer`, and admin's `Live` trait were designed as ports. The plan promotes them to
   network boundaries; it does not invent new abstractions where one exists.
5. **No new heavyweight deps without a decision.** No gRPC/tonic, no NATS/Redis/Kafka, no
   Kubernetes operator. The internal fabric reuses quinn + rustls + ciborium + ed25519,
   all already in the workspace. (⚖ §9.1 if we ever want an off-the-shelf bus.)

## 1. Where the monolith is coupled today (survey)

Everything boots from `weftd::start` (`crates/weftd/src/lib.rs`): QUIC acceptor, WS
acceptor, HTTP/HTTPS (well-known + admin + SPA + media), IRC gateway, outbound bridge
dialers, mirror/backfill/auto-bridge consumers, maintenance task, TLS/ACME — all sharing
one `Arc<ServerCtx>` (`crates/weft-core/src/context.rs`).

Coupling inventory, easiest to hardest to sever:

| Coupling | Where | Severability |
|---|---|---|
| Store access | ~18 `Arc<dyn …Store>` clones of one backend | **Already severed** — any role with a `DATABASE_URL` gets the same view through the traits. Postgres is the shared substrate. |
| Core → dialer sinks | `auto_bridge_tx`, `mirror_tx`, `backfill_tx` OnceLock mpsc ports | Easy — already fire-and-forget message passing; becomes a cluster call. |
| Voice | `VoiceBackend`/`VoiceRelay` traits; LiveKit is already an external process | Easy — LiveKit did this split for us; only the signer + `voice_rooms` roster are in-process. |
| Admin → live actors | `Live` trait (kick/eject/delete), shared `connections` counter | Easy — trait becomes a cluster RPC client. |
| Mailer | `Mailer` trait | Trivial — stays a library; every role that needs mail links it. Not worth a process. |
| Media data plane | QUIC bidi streams + HTTP `/media`, gated by `MediaRegistry` in-memory tokens | Medium — bytes are separable, but the token registry is in-process state (fix: signed stateless tokens, §4.2). |
| Sessions ↔ channel actors | `ChannelHandle` (mpsc) + `broadcast` fan-out + per-session forwarder tasks | **Hard** — the actual monolith. This is P4. |
| Sessions ↔ directory | one global `Directory` actor (DM order, presence, notify) | Hard — same shape as channels; shard by account. |
| In-memory maps | `presence`, `voice_rooms`, `federate_cooldown`, `verify_codes`, `MediaRegistry`, dedup `(session,label)` | Mixed — dedup is per-session (stays local); the rest must follow their owning actor or become signed/stored (§6.3). |

## 2. Target decomposition: roles

One binary; `[node]` config selects roles. Omitted section ⇒ all roles ⇒ exactly today's
process.

```toml
[node]
roles = ["chat", "media", "federation", "web", "admin", "maintenance"]  # default: all
cluster_listen = "0.0.0.0:7100"          # internal fabric (only if a subset of roles)
cluster_peers  = ["10.0.0.2:7100", ...]  # static membership, P4 may add gossip later
cluster_key    = "path/to/node.key"       # per-node Ed25519, signed by the network identity key
```

| Role | Owns | Scales with |
|---|---|---|
| **chat** | Client transports (QUIC/WS/IRC), session tasks, **channel actors + directory shard for the namespaces/accounts placed on it**, store access | Connections × message rate. The horizontally-sharded tier (§5). |
| **media** | Blob store (fs/S3 later), QUIC data-plane streams, HTTP `/media`, hash blocklist check | Bandwidth + disk. Stateless once tokens are signed (§4.2); scale-out is trivial. |
| **federation** | Outbound dialers, `PeerLinks`, mirror/backfill/auto-bridge consumers, **inbound bridge listener** (advertised via `/.well-known/weft`) | Peer count. Usually 1 instance. |
| **web** | axum: well-known, SPA, HTTPS termination | CDN-able; usually rides chat or media nodes. |
| **admin** | Admin API + SPA, store-direct, `Live` via cluster RPC | 1 instance. |
| **maintenance** | Retention purge, compaction, media GC, recovery scheduler, deletion finalizer | **Singleton** — enforced with a Postgres advisory lock (`pg_advisory_lock`), no new deps. |

Postgres stays the single source of truth for all roles (its own scaling: §8.3). LiveKit
stays the voice media plane, unchanged.

**Why namespaces are the chat-shard unit** (not channels, not users): a namespace's
channels, layout, roles, moderation covering-scopes (`#chan` ⊂ `ns:` ⊂ `*`), and voice
rosters all interrelate; co-locating them keeps every hot-path check node-local and is
exactly Discord's guild-sharding shape. Non-namespaced (network-level) channels hash as
the pseudo-namespace `""`. DMs shard separately by account hash on the directory (§5.3).

## 3. The internal fabric: `weft-cluster` (new crate, L2)

Deps: proto, crypto, tokio, quinn/rustls (like weft-transport). Never interprets domain
verbs beyond envelope framing — the domain payloads are defined in weft-core.

- **Transport**: QUIC, ALPN `weft-cluster/1`, mutual auth. Handshake modeled directly on
  AUTH BRIDGE: each node holds an Ed25519 node key; a node cert (deterministic-CBOR,
  signed by the network identity key, same pattern as `SignedManifest`) authorizes it to
  join the cluster. Nonce challenge both ways; TLS via the existing rcgen/rustls stack.
- **Framing**: two stream kinds on one connection.
  - **CALL streams** (bidi, one per request): deterministic-CBOR envelope
    `{ id, method, payload }` → `{ id, result | error }`. Same correlation idea as the
    public protocol's `label`, binary because payloads are full domain records.
  - **SUB streams** (uni, long-lived): opened by the serving node after a
    `Subscribe { channel }` call; carries a CBOR stream of `ChannelEvent`s. One SUB per
    `(subscribing node, channel)` — see the fan-out multiplexer in §5.2.
- ⚖ §9.2 covers text-vs-CBOR: the netcat-debuggable ethos governs the *public* control
  plane; internally, re-encoding `EventRecord`s to text and back is pure loss. Recommend
  CBOR + a `weftd cluster-tap` debug subcommand for observability parity.
- **Testing** (project convention: wire behavior proto-first): round-trip tests for every
  envelope in `weft-cluster` before any consumer, then two-process conformance tests in
  `crates/weftd/tests/` following the existing two-live-weftd precedent.

Method surface (grows per phase, exhaustive list per phase below):

```text
P2  media.check_blocked? (or store-direct)          — media role
P3  federation.request_auto_bridge / mirror / backfill_pull   (chat → federation)
    core.ingest_bridged                              (federation → chat, routed by placement)
P4  channel.call { name, cmd }                       (any Cmd variant, oneshot → response)
    channel.subscribe { name } → SUB stream
    directory.call { account, cmd }  + directory.subscribe
    cluster.placement_epoch / node liveness pings
P5  admin.live { kick | eject | delete_message }, admin.stats
```

## 4. Phased plan

Each phase is independently shippable and leaves `cargo test --workspace` green with the
single-process default untouched.

### P0 — Role scaffolding (pure refactor, no new behavior)

1. Add `[node]` config (`crates/weftd/src/config.rs`) with `roles`, defaulting to all.
2. Decompose `weftd::start` into per-role builders:
   `build_chat(ctx, …)`, `build_media(…)`, `build_federation(…)`, `build_web(…)`,
   `build_admin(…)`, `build_maintenance(…)` — each returning its `Vec<JoinHandle>`. Today
   `start` is one 400-line function; this is mostly a mechanical regrouping of existing
   spawn sites (acceptors, dialer spawns, axum servers, maintenance).
3. Gate each builder on its role flag. All-roles ⇒ byte-identical behavior.
4. Guard cross-role assumptions with startup validation: e.g. `admin` without `chat` in
   the same process ⇒ `Live` endpoints degrade to 501 exactly as the standalone admin
   binary path already does (`weft-admin` handles `Live = None` today).

*Exit criteria*: full suite green; a config with `roles = ["chat"]` boots without media
routes, dialers, or admin; no `weft-cluster` yet.

### P1 — `weft-cluster` crate

1. Envelope types + deterministic-CBOR codec + round-trip tests (no I/O yet — the codec
   half is L0-testable).
2. Node identity: `node.key` generation (`weftd keygen-node`), node-cert signing by the
   network identity key (extend `weft-crypto`, same module family as `rotation.rs`).
3. QUIC listener/dialer with the handshake; maintained inter-node connections with
   reconnect/backoff (clone the shape of `dialer::dial_loop`).
4. `ClusterClient` / `ClusterServer` handles that later phases register methods on.

*Exit criteria*: two test processes complete the handshake, exchange a CALL round-trip
and a SUB stream; a bad node cert is rejected; codec round-trip tests green.

### P2 — Media role (first real split)

Prerequisite, valuable even without the split: **signed stateless media tokens.**
`MediaRegistry` (in-memory one-time upload grants / fetch bearers / backfill batches,
`crates/weft-core/src/media.rs`) becomes Ed25519-signed deterministic-CBOR grants
(reuse the capability-token machinery in weft-crypto): payload = op (`put`/`get`/
`backfill`), hash or upload constraints, account, expiry (short, ~60 s — replaces
one-time-ness; ⚖ §9.3 if strict one-time matters). Chat nodes mint with the network
key; media nodes verify with the public key. **This deletes shared mutable state**
instead of distributing it.

Then:

1. Move `FsBlobStore`, `media::router`, and `accept_data_plane`/`handle_data_stream`
   under the `media` role builder. Media nodes need: blob dir, store access for
   `MediaBlocklistStore` + `MediaStore` metadata (they already write refcounts? — no:
   **ref recording stays in the channel actor**, the single writer; media nodes only
   store/serve bytes and write blob metadata rows).
2. Chat nodes advertise media endpoints to clients (the data-plane address in WELCOME /
   well-known — small spec-adjacent addition, note in Appendix A).
3. `may_fetch` (membership gating) is computed at mint time on the chat node and baked
   into the token — media nodes do **zero** membership queries. Blocklist stays a
   store read at serve time (it must apply to already-minted tokens).
4. Mirror consumer (fetching foreign blobs over bridge links) moves to the federation
   role in P3; until then it stays wherever `PeerLinks` lives.

*Exit criteria*: two-process conformance test — chat node mints token, client PUTs and
GETs against a separate media process; blocklisted hash refused; expired token refused.
Single-process mode still passes the existing media suite (tokens now signed there too —
one code path).

### P3 — Federation role

1. The three OnceLock sinks become cluster calls: `ctx.request_auto_bridge / mirror /
   backfill` route to the federation node (in-process fast path when co-resident —
   keep the mpsc when the role is local; the `set_*_sink` seam already permits either).
2. `PeerLinks`, dialers, mirror/backfill/auto-bridge consumers move under the role.
3. **Inbound** bridges: federation role runs its own QUIC listener; `/.well-known/weft`
   and the manifest exchange advertise it, so peers dial the federation node directly.
   (Fallback: chat nodes accept `AUTH BRIDGE` and proxy — rejected: pointless hop.)
4. Ingestion reverses direction: federation node receives bridged events and calls
   `core.ingest_bridged` on the chat node owning the target namespace (placement fn,
   trivial while chat is 1 node). Origin-authority + manifest gating (invariants 2, 3)
   run **on the federation node** before the call — it is the component that knows the
   authenticated peer identity.
5. Report-forward outbound dial (currently deferred) naturally lands here too.

*Exit criteria*: the existing two-live-weftd conformance tests re-run with each weftd
split into chat+federation processes (4 processes total); NETBLOCK effects still hold
(the deny must apply on the federation listener).

### P4 — Chat-node sharding (the big one)

This is where "modular monolith" becomes "horizontally scalable core". Everything before
it works with exactly one chat node.

1. **Placement**: rendezvous (HRW) hashing of namespace name over the configured chat
   node set. Pure function + table in `weft-core`; static membership from `[node]`
   config first (⚖ §9.4 for dynamic membership later). Placement epoch bumps on config
   change; a mismatch error on calls triggers re-resolution.
2. **`ChannelHandle` goes remote-capable**:
   ```rust
   enum ChannelHandle { Local(mpsc::Sender<Cmd>), Remote(RemoteChannel) }
   ```
   `Cmd`'s oneshot responders map 1:1 onto CALL streams. `Registry::lookup` consults
   placement: local namespace → spawn/find actor as today; remote → stub. Sessions,
   handlers, and actors above the handle **do not change** — this is why the seam was
   worth preserving.
3. **Subscription multiplexer**: naive remote `Cmd::Subscribe` would open one SUB per
   session per remote channel. Instead each chat node runs one mux: first local
   subscriber to a remote channel opens the SUB stream and re-publishes into a local
   `broadcast::Sender`; further subscribers attach to that; last-drop closes the SUB.
   Cross-node traffic per channel is O(nodes), not O(sessions). Lag on the local
   rebroadcast reuses the existing `RecvError::Lagged` → `SLOW` → HISTORY-resync path
   (invariant 6) unchanged.
4. **Directory sharding**: same pattern keyed by account hash: `Directory` becomes
   local-or-remote; presence moves inside the directory shard that owns the account
   (deleting the separate `presence` Mutex map); `notify` and DM delivery route by
   account. DM ULID ordering: per-account-pair order is preserved because a DM is
   minted by the *recipient-owning* shard — ⚖ §9.5 (alternative: mint by lexicographic
   min of the two accounts; must pick one rule and test it).
5. **In-memory maps** follow their owner: `voice_rooms` lives with the namespace's chat
   node; `verify_codes` and `federate_cooldown` move to the directory shard of the
   account (or drop to store-backed — they're low-rate).
6. **Failure semantics, explicitly staged**:
   - *Stage 1 (ship P4 with this)*: static placement; a dead chat node makes its
     namespaces unavailable until it returns — availability equals today's single
     process, sharding buys capacity only.
   - *Stage 2*: lease-based failover — a Postgres lease table (or advisory locks);
     surviving nodes re-run placement over live nodes, actors respawn cold from the
     store (actor state is store-backed except the member map, which rebuilds as
     sessions resubscribe via the resync path). Split-brain is prevented by the lease,
     preserving the single-writer invariant.
7. Client connection routing: any client may connect to any chat node (sessions are
   node-local; channels resolve remotely). Plain L4 load balancing / DNS RR suffices —
   no sticky routing needed beyond ordinary connection affinity.

*Exit criteria*: two-chat-node conformance — user connected to node A joins, chats,
edits, reacts, and gets history in a namespace placed on node B; ordering test (two
senders on different nodes, one channel, strictly monotone ULIDs); lag→SLOW resync test
across nodes; kill-node-B test documents stage-1 unavailability semantics.

### P5 — Ops polish

- Maintenance singleton via `pg_advisory_lock` (safe to list the role on several nodes).
- Admin: `Live` over cluster CALLs fanned to all chat nodes; `/stats` aggregates
  per-node connection counts.
- `deploy/`: compose profiles for split topologies (chat×2 + media + federation +
  postgres + livekit); document in `docs/vps-testing.md`.
- `weftd cluster-tap` debug subcommand (§3) + per-link tracing spans.
- Update `reviews/code-navigation.md` (per repo convention) as each phase lands.

## 5. Invariants under the split (checklist to carry into review)

- **§9.1 total order**: exactly one actor per channel cluster-wide (placement + lease);
  ULID generator never leaves the actor. Test with concurrent cross-node senders.
- **Invariant 1 (anti-enumeration)**: placement lookups must not leak — a remote
  `NO-SUCH-TARGET` and a local one must be indistinguishable in code path and timing
  envelope; the placement fn runs regardless of existence.
- **Invariant 4 (caps precede side effects)**: capability verification stays on the
  session's node *before* any cluster call is issued.
- **Invariants 2/3 (origin authority, manifest gating)**: enforced on the federation
  node at ingest; the chat node's `ingest` method trusts only authenticated cluster
  peers — the internal fabric's mutual auth is now load-bearing for federation security.
- **Invariant 6 (backpressure)**: bounded everywhere — CALL streams are naturally
  request-scoped; SUB streams inherit lag→SLOW; the cluster link itself must never
  buffer unboundedly (bounded per-link send queues, drop-to-resync on overflow).
- **Invariant 13 (SSRF)**: `is_dialable` continues to gate only *federation* dials;
  cluster peers are explicit config and exempt (they are internal addresses on purpose).
- **§14 (e2ee)**: unchanged — no plaintext path exists to move; SUB streams carry the
  same opaque payloads the broadcast did.

## 6. What this plan deliberately does not do

- No message bus / broker dependency (NATS, Redis, Kafka) — ⚖ §9.1.
- No Kubernetes operator, no service mesh; static config membership first.
- No per-message store sharding — Postgres remains one logical database (§8.3 is the
  path when it saturates).
- No rewrite of session handlers — the entire point of cutting at `ChannelHandle` /
  `Directory` / sinks is that `session/*.rs` stays untouched.

## 7. Cost/benefit honesty

A single Rust/tokio process on one decent box will comfortably run tens of thousands of
concurrent connections and a large message rate; Postgres will saturate before the actor
layer does for most workloads. **P0–P3 are worth doing early** (they're cheap, improve
testability, and make media/federation independently restartable and placeable near
bandwidth). **P4 is a big lift** — do it when a load test says the chat tier is the
bottleneck, not before. Recommend building a load-generation harness (a `weft-tui`-derived
bot swarm) as part of P0 so every later phase has numbers.

## 8. Other ways to scale (context and complements)

1. **Vertical scaling** — the baseline. Rust + tokio + QUIC means the ceiling on one
   machine is high; measure before sharding. Cheapest ops story by far.
2. **Federation as sharding** — WEFT's superpower: the protocol is *designed* so that
   load can split across sovereign networks bridged by signed manifests. Many
   mid-sized networks federating beats one giant network, and it needs **zero new
   code**. Auto-federation (§11.10, shipped) makes this nearly transparent to users.
   For communities that outgrow a server, "spin up a second network and bridge" is a
   legitimate, already-working answer.
3. **Database scaling** (orthogonal to every phase above):
   - Connection pooling (pgbouncer) and **read replicas** — HISTORY/BATCH, search, and
     DISCOVER are read-heavy and replica-safe (bounded staleness is fine for
     scrollback; live path never reads what it just wrote thanks to actor ordering).
   - **Native partitioning** of the events table by channel hash and/or ULID time —
     also makes retention purge a partition drop instead of a delete.
   - **Citus / per-namespace database routing** behind the store traits if a single
     writer node saturates — the `Arc<dyn Store>` seam admits a router impl that picks
     a pool by namespace, aligning data placement with P4 actor placement.
4. **Sharding axis comparison** (why §2 chose namespaces):
   - *By channel*: finest grain, but splits a namespace's moderation/layout/roles
     across nodes — every covering-scope check goes remote. Rejected.
   - *By namespace* (chosen): hot-path locality, Discord-proven shape. Downside: one
     mega-namespace can't split — mitigate with per-channel sub-placement as a later
     refinement only if a real namespace outgrows a node.
   - *By user/account*: right for the directory/DM tier (and used there, P4.4), wrong
     for channels — every channel would be remote for most of its members.
   - *By network* = federation (point 2), already built.
5. **Edge/CDN for media** — media URLs are content-addressed (BLAKE3), i.e. immutable
   and perfectly cacheable; a dumb HTTP cache or CDN in front of media nodes multiplies
   read bandwidth for free. Signed short-lived GET tokens compose with this via
   signed-URL-style caching keyed on hash (cache the blob, not the authorization).
6. **Thin edge terminators** (a possible P6): tiny `edge` role that terminates
   TLS/QUIC/WS and pipes raw `ControlStream` lines to chat nodes over the fabric —
   sessions stay on chat nodes. Only worth it if TLS termination or connection count
   (not message rate) becomes the bottleneck; listed for completeness, not planned.

## 9. Decisions for Jannik (⚖)

1. **Internal fabric: build-on-quinn (recommended) vs adopt a broker/gRPC.** Build keeps
   the dependency policy and dogfoods the stack; a broker buys ready-made fan-out and
   persistence at the cost of an ops dependency the project has so far refused.
2. **Internal encoding: CBOR envelopes (recommended) vs text control-plane verbs.**
   Text would extend netcat-debuggability inward but forces lossy re-encoding of domain
   records; the `cluster-tap` subcommand is the proposed observability substitute.
3. **Media tokens: short-expiry signed (recommended, stateless) vs strict one-time.**
   One-time semantics across processes would need a shared nonce store; 60-second expiry
   plus hash-binding is almost certainly enough.
4. **Cluster membership: static config (recommended first) vs dynamic (lease/gossip).**
   Static is one less failure mode; dynamic arrives with P4 stage-2 failover anyway.
5. **DM ULID minting rule under directory sharding** (P4.4): recipient-shard vs
   min-account-shard. Pick one, spec-note it, test it.
6. **Whether P4 happens at all soon** — see §7; P0–P3 stand alone and are the
   recommended near-term slice.

## Appendix: phase → test surface summary

| Phase | New tests | Existing suite |
|---|---|---|
| P0 | role-gated boot matrix | green, byte-identical default |
| P1 | cluster codec round-trips; handshake accept/reject | untouched |
| P2 | 2-process media conformance; signed-token unit tests | media suite now exercises signed tokens |
| P3 | 4-process federation conformance; NETBLOCK on split listener | two-live-weftd re-run split |
| P4 | 2-chat-node conformance; cross-node ordering; cross-node SLOW resync; node-loss semantics | full suite on 1-node placement (degenerate case) |
| P5 | advisory-lock singleton test; admin fan-out | — |
