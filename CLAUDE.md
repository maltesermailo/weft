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

Toolchain: MSRV 1.75, no nightly. Deps so far (workspace-pinned): thiserror, ulid, tokio, tokio-util, futures-util, quinn, rustls (**ring provider only** — a second provider makes the process default ambiguous), rustls-pki-types, rcgen (ring), tokio-tungstenite, tracing(+subscriber), serde, toml, anyhow, ed25519-dalek, ciborium, base64, rand, sha2, subtle, axum, rustls-pemfile.

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

## Concurrency model (from the architecture doc)

Actor-per-channel, task-per-connection, `tokio::mpsc` inboxes + `broadcast` fan-out. The channel actor's inbox order IS the ULID order. Bridges are ordinary sessions holding a `bridge` capability token — same acceptor path as clients. Slow consumers detected via `RecvError::Lagged` → `SLOW` path.

## Milestones (each independently shippable)

- **M0 ✅** codec: weft-proto for session+relay verbs (HELLO/REGISTER/AUTH×4/QUIT/PING/PONG/PRESENCE/JOIN/PART/TYPING/MARK/MSG incl. `@user` DM targets), events, error registry. 49 tests green.
- **M1 ✅** echo server: `ControlStream` trait (defined in weft-core — its port; weftd adapts the transport types), quinn acceptor (ALPN `weft/1`), WS fallback (tokio-tungstenite; axum arrives with well-known in M2), session FSM `NEGOTIATING→UNAUTHED→READY` (§3.3), static config channel registry + actors, MSG relay with label echo-ack + `(session,label)` dedup, `ephemeral` only, anonymous AUTH (real auth = M2). Conformance: black-box QUIC+WS tests in `crates/weftd/tests/conformance/`. 73 tests green workspace-wide.
- **M2 ✅** identity: weft-crypto (Ed25519 keys, deterministic-CBOR attestations, challenge proofs, constant-time password hashes), REGISTER/AUTH PASSWORD/AUTH KEY/AUTH PROOF/AUTH ENROLL with uniform AUTH-FAILED, in-memory account registry (traits + persistence = M3), `/.well-known/weft` (axum), operator PEM certs + persisted signing key. 101 tests green workspace-wide.
- **M3** persistence: sqlx **PostgreSQL** store (connection URL in config; memory impl remains the test/dev backend, so `weftd` still runs DB-less), retention policies + purge task, HISTORY/BATCH, EDIT/DELETE/REACT materialization, **compaction task (§12.1: audit window, edit collapse, REACTIONS summaries)**, MARK sync, DMs. Postgres integration tests gate on `WEFT_TEST_DATABASE_URL` (skipped when absent).
- **M4** capabilities: token mint/verify chains, GRANT/REVOKE, revocation epochs, NS verbs **incl. recovery ladder (NS RECOVERY SET / RECOVER / RECOVERY CANCEL, delay windows, root-history)**, CHANNEL verbs, INVITE lifecycle, view gating, DISCOVER, REPORT/REPORTS verbs + retention holds (§6.7, §12.1).
- **M5** federation: BRIDGE manifest state machine, remote ingestion, strictest-policy negotiation, backfill (§11.7), NETBLOCK, REPORT-FORWARD (§11.9).
- **M6+** media (BLAKE3 content addressing, STREAM, mirroring §11.8), threads filter, E2EE (openmls, feature `e2ee`), WEFT-RT voice, WEFT-IRC gateway (a third `ControlStream` impl in its own crate).

Current focus: **M3** (persistence): weft-store traits (`EventStore`, `AccountStore` — the in-memory `Accounts` in weft-core becomes the memory impl and password hashing upgrades to a real KDF before hashes touch disk), sqlx PostgreSQL, retention + purge, HISTORY/BATCH, EDIT/DELETE/REACT materialization, compaction (§12.1), MARK sync, DMs.

## Deliberately deferred — do not add

openmls, SFU/voice, SQLite backend (the traits allow it; Postgres is the chosen engine — decision reversed 2026-07), Biscuit tokens, SRV discovery, cross-network DMs, per-message rate-limiter beyond THROTTLED plumbing, shared blocklists. If a task appears to need one, flag it instead of adding the dependency. Open questions live in spec §18 — decisions there belong to Jannik, not to a coding session.