# CLAUDE.md — weftd

Reference server for the **WEFT protocol** (working name): a federated chat protocol combining IRC's operational simplicity with Discord's feature semantics. Rust, tokio, QUIC-native.

**Normative source**: `docs/weft-protocol-spec.md` (**v0.10, consolidated — fully self-contained**). Design rationale: `docs/weftd-server-architecture.md`. When code and spec disagree, the spec wins; if the spec is wrong, amend the spec in the same PR and note it in its Appendix A decision history.

## Protocol in one paragraph

Independent sovereign networks; federation is explicit **signed manifest peering** (spec §11) — never transitive, every event at most one hop from origin. Text control plane (`@tags VERB params :trailing`, §4, netcat-debuggable) + binary data plane, over QUIC (stream 0 = control) with WS fallback (§3). Identity = `user@network` + Ed25519 device attestations (§10). Permissions = **scoped capability tokens** (§10.4: signed deterministic CBOR, delegation chains, short expiry + refresh, revocation epochs) — no role tables anywhere. Namespaces = user-owned Discord-style servers with `public|unlisted|private` visibility (§2); channels carry retention policies `ephemeral|retained:<d>|permanent|e2ee` (§5.2); bridges negotiate to strictest. Requests correlate via `label` tags; the sender's echo is the message ack (§3.5, §9.2). Full command reference: spec §6. Events: §7. Errors: §8.

## Workspace layout & layering (STRICT)

```
crates/weft-proto      L0  wire codec, verbs, events, errcode, IDs, policies — pure, no I/O, no tokio
crates/weft-crypto     L0  attestations, capability tokens                   — pure, no I/O, no tokio
crates/weft-store      L1  storage traits + postgres/memory impls            — deps: proto
crates/weft-core       L2  sessions, channel actors, router                  — deps: proto, crypto, store
crates/weft-transport  L2  quinn + WS framing                                — deps: proto ONLY
crates/weftd           L3  binary: config, acceptor, well-known, telemetry   — deps: everything
crates/weft-tui        —   dev tool: terminal test client (ratatui)          — deps: proto, transport (insecure-client feature)
crates/weft-irc        L3  WEFT-IRC gateway (§17): a ControlStream that translates IRC↔WEFT — deps: proto, core
```

Non-negotiable:
- Dependencies point downward only. `weft-transport` never interprets verbs; `weft-core` never touches sockets (traits: `ControlStream`, `EventStore`).
- **No tokio, no I/O in L0.** They are the security-critical parsers and must stay fuzzable in isolation.
- New wire behavior = code + round-trip test in `weft-proto` FIRST, then consumers.

## Commands

```bash
cargo build                    # workspace
cargo test -p weft-proto       # codec suite (fast — run constantly; currently 49 green)
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

Toolchain: MSRV 1.75, no nightly. Deps so far (workspace-pinned): thiserror, ulid, tokio, tokio-util, futures-util, quinn, rustls (**ring provider only** — a second provider makes the process default ambiguous), rustls-pki-types, rcgen (ring), tokio-tungstenite, tracing(+subscriber), serde, toml, anyhow, ed25519-dalek, ciborium, base64, rand, argon2, sha2, subtle→(dropped), async-trait, axum, rustls-pemfile, sqlx (postgres, runtime queries only).

## Conventions

- Errors: `thiserror` in libs, `anyhow` only in `weftd`. Every parse error typed and tested.
- Parsers are **lenient-in, strict-out** (spec §4): tolerate noisy-but-safe input; `serialize()` MUST refuse to emit anything our own parser rejects. Round-trip tests mandatory for every wire type.
- Unknown verbs → `Command::Unknown`, never an error; unknown events ignored client-side. There is deliberately no `UNKNOWN-COMMAND` error (spec §8).
- Deterministic serialization wherever a signature might apply: `BTreeMap` not `HashMap`; deterministic CBOR encode-before-sign in weft-crypto.
- ULIDs are assigned ONLY by the channel actor (single writer = per-channel total order, spec §9.1). Never mint a msgid elsewhere.
- `label` handling: echo the label on every DIRECT response including ERR; never on broadcast copies (spec §3.5). Dedup MSG retries by `(session, label)` in a 5-minute window (spec §9.2).
- `tracing` spans: per connection + per verb dispatch. No `println!` outside `main.rs`.

## Security invariants (implement AS TESTS, not just code)

1. **Anti-enumeration** (spec §2.2, §8): `NO-SUCH-TARGET` is the single code for nonexistent / private-unmember / view-gated / expired msgid / dead invite — identical code, identical timing envelope. Any pre-error branch on hidden-vs-absent is a bug.
2. **Origin authority** (§11.4): EDIT/DELETE honored only when authorized by the msgid's origin. Bridged events keep origin msgids + attestations intact; verify attestations for backfill exactly as for live traffic (§11.7).
3. **Manifest gating** (§11.1): forwarding a channel absent from the last mutually-acked manifest version is a protocol violation, not a soft failure. Backfill bounded by the `history` flag (ULID timestamp compare).
4. **Capability checks precede side effects** (§10.4). Verify the token chain before mutating anything.
5. **Auth**: challenge proofs sign `nonce‖network-name` (§6.1 — anti cross-network replay); password compares constant-time; `AUTH-FAILED` is uniform.
6. **Backpressure** (§9.2): slow client ⇒ `ERR SLOW` + forced HISTORY resync. Never buffer unboundedly.
7. **NETBLOCK is name-keyed** (§11.6): key rotation never evades it; effects = reject proposals + sever manifests + reject attestations + stop media, all four.
8. **E2EE unrepresentability** (§14): policy transitions to/from `e2ee` require empty channel or explicit purge; no code path may hold plaintext for an `e2ee` channel. Recovery (§2.4) never restores e2ee history.
9. **Recovery ladder** (§2.4): every delayed recovery rung = announcement + delay + root-cancellable; rung-3 rotations permanently marked operator-initiated in `root-history`. No silent root rotation path may exist.
10. **Compaction** (§12.1): live path stays event-sourced; batches serve compacted form (`edited=` tags, `REACTIONS` summaries, tombstones) after the `compact-after` audit window (default 24 h). Batches must never contain `EDITED` chains or reaction ping-pong.
11. **Retention holds** (§12.1): reported events + context are exempt from compaction AND purge until resolution + grace. Holds are invisible on every protocol surface. Report content states (`verified`/`unverified`/`reporter-attested`) are marked honestly — never fabricate verification for e2ee or expired content.
12. **Report confidentiality** (§6.7): reported party never learns reporter identity from any protocol surface; forwarded reports (§11.9) strip reporter identity by default.
13. **Auto-federation SSRF** (§11.10): the outbound auto-bridge dialer MUST refuse any non-public target (loopback / RFC-1918 / CGNAT / link-local / ULA / metadata / v4-mapped-private) — a user-supplied network name can never make the server reach internal infrastructure. Implemented as a test over the address classifier (`weftd::dialer::is_dialable`), not just the dial path.

## Concurrency model (from the architecture doc)

Actor-per-channel, task-per-connection, `tokio::mpsc` inboxes + `broadcast` fan-out. The channel actor's inbox order IS the ULID order. Bridges are ordinary sessions holding a `bridge` capability token — same acceptor path as clients. Slow consumers detected via `RecvError::Lagged` → `SLOW` path.

## Milestones (each independently shippable)

- **M0 ✅** codec: weft-proto for session+relay verbs (HELLO/REGISTER/AUTH×4/QUIT/PING/PONG/PRESENCE/JOIN/PART/TYPING/MARK/MSG incl. `@user` DM targets), events, error registry. 49 tests green.
- **M1 ✅** echo server: `ControlStream` trait (defined in weft-core — its port; weftd adapts the transport types), quinn acceptor (ALPN `weft/1`), WS fallback (tokio-tungstenite; axum arrives with well-known in M2), session FSM `NEGOTIATING→UNAUTHED→READY` (§3.3), static config channel registry + actors, MSG relay with label echo-ack + `(session,label)` dedup, `ephemeral` only, anonymous AUTH (real auth = M2). Conformance: black-box QUIC+WS tests in `crates/weftd/tests/conformance/`. 73 tests green workspace-wide.
- **M2 ✅** identity: weft-crypto (Ed25519 keys, deterministic-CBOR attestations, challenge proofs, constant-time password hashes), REGISTER/AUTH PASSWORD/AUTH KEY/AUTH PROOF/AUTH ENROLL with uniform AUTH-FAILED, in-memory account registry (traits + persistence = M3), `/.well-known/weft` (axum), operator PEM certs + persisted signing key. 101 tests green workspace-wide.
- **M3a ✅** persistence, memory path: weft-store (EventStore/AccountStore traits, memory backend, **§12.1 materialization as one shared pure fn**), per-channel retention config, EDIT/DELETE/REACT (+UNREACT) with origin/author checks, HISTORY/BATCH (compacted wire form, honest `truncated` via purge watermark), argon2 PHC password hashes. 134 tests green.
- **M3b ✅** persistence, durable path: sqlx **PostgreSQL** backend behind the weft-store traits (one shared contract suite runs against both backends; PG tests gate on `WEFT_TEST_DATABASE_URL`), **channels load from the store at boot** (config = seed data — the substrate for M4's CHANNEL CREATE), maintenance task (retention purge + §12.1 compaction via one shared pure `compaction_plan`), MARK sync + §9.7 MARKED snapshot, DMs via the account directory actor, verification-claims infrastructure (email/age/... — store level only, wire flow needs spec design). 146 tests green.
- **M4a ✅** capabilities foundation: weft-crypto capability tokens (signed CBOR, delegation-chain verify, revocation epochs — 29 tests), M4 verb/event codec, weft-store grants+epochs+invites+channel-meta (mem+PG shared contract), enforcement in weft-core (invariant 4: caps precede side effects) — GRANT/REVOKE, CHANNEL CREATE/POLICY/META/DELETE (registry now mutable, lazy actor spawn), INVITE MINT/REVOKE/REDEEM, view gating (invariant 1). Operator accounts (config `operators`) bootstrap the grant chain. 184 tests green.
- **M4b ✅ (partial — namespaces)** NS CREATE/META/VISIBILITY/DELEGATE/DELETE, visibility tiers, DISCOVER, ns-scope grants/invites, namespaced channels — all unlocked by the namespace-owner authority model (owner holds every cap in `ns:<name>`, the ns-scoped analog of operators at `*`). **Plus a channel-layout extension**: Discord-style categories + ordering (`CHANNEL META category|position`, `CHANNELS <ns>` → ordered `CHANNEL-LAYOUT`). 196 tests. **Plus the recovery ladder ✅** (invariant 9): NS TRANSFER (root-key-signed succession — cryptographically enforced), NS RECOVERY SET (M-of-N quorum), NS RECOVER (rung-2 quorum / rung-3 operator, 7d/30d delay windows), NS RECOVERY CANCEL (root veto), scheduled application + root-history (rung-3 operator-marked). weft-crypto `rotation` module (transfer/rotation/cancel signing). 206 tests.
- **M4c ✅** moderation: REPORT (ns/net routing, `csam`/`illegal` dual-route to operator), REPORTS LIST/RESOLVE (concrete cap scope), content states (`verified` on the same-network path; `unverified`/`reporter-attested` wired for M5/M6), retention holds (refcounted, reported root ± `HOLD_RADIUS`=25 context, exempt from purge+compaction until resolution+7d grace — invariant 11), reporter confidentiality (reported party receives nothing; reporter's resolution push omits handler identity + note — invariant 12). Live push to a queue's default handlers (ns owner / operators) via a directory `notify`; delegated `reports` holders pull via LIST. 214 tests.
- **M5a ✅** federation foundations (proto-first): BRIDGE PROPOSE/ACCEPT/ADD/REMOVE/SEVER, NETBLOCK ADD/REMOVE/LIST, REPORT-FORWARD, AUTH BRIDGE commands; MANIFEST + NETBLOCKED events; HistoryMode/MediaMode/BridgeState types (74 codec tests). weft-crypto `SignedManifest` (deterministic-CBOR, scope-authority-signed, modeled on `rotation.rs`). weft-store PeerStore + NetblockStore + records, mem+PG shared contract, migration 0008 (live-PG validated).
- **M5b ✅** bridge sessions + state machine + ingestion: `AUTH BRIDGE <net> <pubkey>` reuses CHALLENGE/PROOF → `State::Bridge` (bound to the proven key). Two trust modes via `[federation]` config: **pinned** (default — only configured peers, matching key) and **accept-any** (`accept_any=true`, open federation / trust-on-first-use, NETBLOCK is the escape hatch). Manifest handshake (receive signed PROPOSE → verify → store → auto-accept → ACCEPT), remote ingestion preserving origin msgids (`Cmd::Ingest`, never re-minted — invariant 2), manifest gating on ingest+forward via the acked∩current channel set (invariant 3), one-hop forwarding (only local-origin events cross), MANIFEST-to-members announce (§6.6), NETBLOCK CRUD. **Trust is network-level**: the peer proves control of its network signing key on the session, so per-event `att=` tags aren't carried — origin authority = `msgid.origin == authenticated peer` (per-device attestation is a noted refinement).
- **M5c ✅** backfill + report-forward + netblock effects: federated `HISTORY` over the bridge session bounded by the acked manifest + the `history` flag (`from-epoch` = manifest `created` ULID floor) + origin retention (§11.7, serves the compacted view); `REPORT-FORWARD` receive → net-scope **`unverified`** operator-queue report with reporter stripped (§11.9, invariant 12); NETBLOCK invariant-7 effects (reject bridge auth + proposals, sever manifests, drop ingestion from a blocked peer). Media-mirroring (§11.8) negotiates the manifest `media` policy only — actual blob mirroring rides M6 media.
- **M5d ✅** the real two-server piece: verified **outbound** QUIC client (`weftd::dialer`), the client side of the AUTH BRIDGE handshake, `[[peers]]` config + maintained dial tasks (reconnect/backoff, shutdown-aware), `weft_core::run_bridge_client` (transmit stored proposal, forward local-origin events, ingest the peer's), and **two-live-weftd conformance** (auth, pinned-reject, end-to-end message forwarding over real QUIC). `[[peers]]` keys pin both inbound auth and outbound dialing. Deferred: well-known key-fetch client (arbitrary-domain discovery); the outbound REPORT-FORWARD dial.
- **Auto-federation (§11.10) ◑** transparent, on-demand bridging — see `docs/auto-federation-plan.md` + `docs/auto-federation-spec-amendment.md`. **P1 ✅** (M5d dialer, above). **P2 ✅** foreign-side consent: `BridgeRequest` verb, per-namespace `federation` flag (`NamespaceRecord.federation`, migration 0015, `NS META <name> federation :open|closed` — ns-admin, `open` requires `public`), `on_bridge_request_in` auto-offers a signed manifest for a reachable ns else `NO-SUCH-TARGET`. **P3 ◑** home side: SSRF guard (invariant 13), `run_bridge_requester` (`BRIDGE REQUEST` + auto-accept), `dialer::auto_bridge`, `[federation] auto_bridge = off|open`. **Deferred:** well-known fetch, the client-facing trigger verb + ctx→weftd channel, rate-limit/cap; **P4** (client) in progress.
- **M7 (moderation) ✅** §6.7 mute/ban/kick + restricted posting. Proto: `mute` cap, `MUTE`/`UNMUTE`/`BAN`/`UNBAN` `<scope> <account> [:reason]` + `KICK <#chan> <account>`, `MODERATED` event. **Two composed surfaces:** (1) a **deny-list** (`ModerationStore`, migration 0009) — mute (deny `send`) / ban (deny join+send) keyed by `(scope, account)`, checked against a channel's covering scopes (channel · `ns:` · `*` → so `*` = global/network mods, `ns:` = namespace mods, `#chan` = channel mods), cap-gated by `mute`/`ban`/`kick`; a channel-scope ban force-parts (channel actor `Cmd::Eject` + session own-part cleanup). (2) **`send`-cap enforcement** — `CHANNEL META <#chan> posting :restricted` (a `restricted` channel-record flag) makes posting require `send`, so `GRANT`/`REVOKE send` governs speech. Net: `can_post = ¬muted ∧ ¬banned ∧ (open ∨ has send)`. Mem+PG contract, core tests (mute/ban/kick/cap-gate/restricted+grant path), QUIC conformance. Client (Tauri): slash commands + member buttons send the intents, `MODERATED` renders as a system line.
- **M6 (IRC) ✅ (usable subset)** WEFT-IRC gateway (§17) in its own crate `weft-irc`: an RFC 2812 front-end exposed as a `weft_core::ControlStream`, translating IRC↔WEFT *at the line boundary* so `run_session` drives the ordinary FSM/actors/store — a projection, not a parallel server. Registration (NICK/USER/PASS → HELLO+AUTH, auto-register like weft-tui), JOIN/PART (incl. namespaced `#ns/chan` — the "namespaces can be joined" path, spec §17 "`JOIN #ns/chan` valid natively"), PRIVMSG↔MSG (bare nick → DM `@nick`, own echo suppressed), NAMES/LIST(←DISCOVER)/PING/PONG/QUIT/MOTD + core numerics, edit/delete/react degraded to text (`* edited:` etc.). Cancel-safe reader task; pure unit-tested `translate` module + a real-socket conformance test (register → JOIN `#gaming/general` → chat). `[listen] irc = "…"` enables it. **Deferred (M6+):** SASL, IRCv3 tags (server-time/msgid), chathistory→HISTORY/BATCH, TAGMSG reactions, MODE/TOPIC/KICK projection, 8 KiB↔512 B line splitting, e2ee-invisible enforcement.
- **M6+** media (BLAKE3 content addressing, STREAM, mirroring §11.8), threads filter, E2EE (openmls, feature `e2ee`), WEFT-RT voice.

Current focus: **auto-federation (§11.10)** — transparent, on-demand bridging on top of M5. The outbound dialer (M5d) is built and two live `weftd`s bridge over real QUIC. P1 (dialer) + P2 (foreign-side consent) done; P3 (home-side trigger + SSRF) partial; **P4 (client UX)** in progress. Remaining across P3/P4: well-known key fetch (arbitrary-domain discovery), the client-facing trigger (verb + ctx→weftd channel + `network/namespace` addressing), rate-limiting, and the join-foreign-namespace UX.

Parked owner requests (need spec design before implementation — §18 territory): email/age verification **wire flow** (store infrastructure exists: `weft_store::Verification`, claim→confirm lifecycle); web admin panel (would ride the axum surface in weftd).

## Deliberately deferred — do not add

openmls, SFU/voice, SQLite backend (the traits allow it; Postgres is the chosen engine — decision reversed 2026-07), Biscuit tokens, SRV discovery, cross-network DMs, per-message rate-limiter beyond THROTTLED plumbing, shared blocklists. If a task appears to need one, flag it instead of adding the dependency. Open questions live in spec §18 — decisions there belong to Jannik, not to a coding session.