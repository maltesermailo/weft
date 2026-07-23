# WEFT — Security & Threat Model

Adversary-oriented companion to [`security-posture.md`](./security-posture.md).
The posture document covers supply chain, CI, and tooling — *how the project
defends itself as a codebase*. This document covers *what an attacker can try
against a running deployment*, what stops them, and what does not yet.

Method: this is a review of the actual code, not a design aspiration. Every
enforcement claim carries a `file:line`; every "tested" claim names the test.
Findings were produced by four independent code audits (attack surface, the 13
security invariants, DoS/resource exhaustion, cryptography) and reconciled
against direct measurement.

Last reviewed: 2026-07-23 · workspace ~33 k LOC of server Rust (`weft-proto`
7.7k, `weft-core` 11.9k, `weft-store` 7.1k, `weft-crypto` 2.5k, `weftd` 3.8k,
`weft-admin` 3.4k) · 430 tests.

---

## 1. System model & trust boundaries

A **network** is one sovereign `weftd` deployment. It owns accounts, hosts
channels and namespaces, holds the network Ed25519 signing key, and is the
abuse-accountable party. Nothing leaves a network except across an explicitly
agreed, signed **bridge manifest** — federation is **non-transitive** and every
event is **at most one hop** from origin.

Four trust boundaries, each with a different crossing credential:

| Boundary | Crossing credential | Trusted afterward as |
|---|---|---|
| **Client ↔ server** | `AUTH PASSWORD` or `AUTH KEY`+`AUTH PROOF` (device-key challenge/response) | `Actor::Local(account)`; every privileged verb still re-checks caps per-scope |
| **Server ↔ peer server** (bridge) | `AUTH BRIDGE` — peer proves control of its **network signing key** | `State::Bridge{peer,key}`; may inject events for acked-manifest channels, tunnel user sessions, pull backfill |
| **Operator ↔ admin panel** | HTTP password → HMAC session cookie | `AdminScopes`; **bypasses the wire protocol**, reads/writes the store and drives channel actors directly |
| **IRC client ↔ gateway** | Same WEFT `AUTH` verbs, synthesized from IRC registration | Ordinary `Actor::Local` — no privilege it couldn't get natively |

All four transports converge on one per-connection entry point,
`weft_core::run_session` (`crates/weft-core/src/session.rs`). TLS uses
`with_no_client_auth` (`weft-transport/src/quic.rs:106`) — the server never
authenticates clients at the TLS layer; ALPN `weft/1` is the only handshake
filter. Authentication is entirely a WEFT-layer concern.

### Assets, in priority order

1. **The network signing key.** Signs device attestations, capability tokens,
   manifests, profiles, and rung-3 namespace rotations — *and* (see §6, F-1) the
   admin cookie MAC. Its compromise is total.
2. **Account credentials & sessions.** Password hashes, device keys, live
   sessions.
3. **Message confidentiality** where the model promises it: private/view-gated
   channels (anti-enumeration), report/reporter identity, `e2ee` content.
4. **Namespace ownership** (the community-governance root key).
5. **Availability** of the server.

---

## 2. Adversary model

| Actor | Capabilities assumed | Primary goals |
|---|---|---|
| **Anonymous remote** | Reach a listener; send arbitrary bytes; open many connections | Crash/exhaust the server; enumerate private resources; brute credentials |
| **Authenticated member** | A valid account | Escalate caps; read channels/DMs they aren't in; amplify load (search, history); evade moderation |
| **Malicious / compromised peer network** | A bridge with a proven network key | Inject events for channels it shouldn't; act as arbitrary `account@peer`; pull scrollback beyond its manifest |
| **Delegated admin** | An `admin.*` capability grant (not operator) | Self-promote to operator; act beyond granted scope; read content without the scope for it |
| **Malicious operator** | Hosts the data; holds the network key | *Out of scope for prevention* — an operator can freeze, delete, read any non-`e2ee` channel, and seize any namespace. The design's honest limit is **accountability** (audit log, permanent operator-initiated marks), and **`e2ee` as the one boundary an operator cannot cross** (invariant 8). |

---

## 3. Attack surface & pre-auth exposure

The FSM (`session.rs`) structurally prevents any privileged action from an
unauthenticated state — the `on_request` match has no arm from `Negotiating`/
`Unauthed` that reaches a cap-gated handler. What an **anonymous remote** *can*
reach:

- `NEGOTIATING`: `HELLO` only (version-checked).
- `UNAUTHED`: `REGISTER`, `AUTH PASSWORD/KEY/PROOF/BRIDGE`, `PING`, `QUIT`. Each
  runs real crypto/account-store work on attacker-controlled input pre-auth:
  password verify, pubkey base64 decode, Ed25519 signature verify, CSPRNG nonce
  issuance, bridge-key resolution. `REGISTER` writes to the account store
  pre-auth (gated only by `registration_open` and a 12-byte password floor,
  `session/auth.rs:14,24`).

The broadest untrusted surface is the line/command parser
(`weft-proto/src/line.rs:157`, `command.rs:591`) — reached by every byte before
auth. It is `#![forbid(unsafe_code)]`, fuzzed (three targets under
`weft-proto/fuzz/`), and regression-gated on stable
(`weft-proto/tests/adversarial.rs`). See §7.

**Highest-risk untrusted-input paths**, ranked by exposure × blast radius:

1. **Unauthenticated transport front doors.** `/ws` takes no auth
   (`weftd/src/web.rs:38`) and the IRC listener is plaintext
   (`weftd/src/acceptor.rs:140`) — both hand raw bytes to `run_session`. This is
   the broadest anonymous surface and the DoS surface (§5).
2. **Federation ingest under open federation** (`bridge_accept_any` +
   `bridge_auto_accept`). See §4.
3. **The `FSESSION` homeserver-authority tunnel.** See §4.
4. **The admin panel** — bypasses the wire protocol entirely (§6).

---

## 4. Federation threats

The verification stack for a peer-injected event, in order, all in
`session/federation.rs`:

1. **Netblock** (invariant 7) — `is_netblocked(peer)` checked *before* the key
   match, so key rotation cannot evade a block (`federation.rs:19-33`).
2. **Origin authority** (invariant 2) — the event's `msgid.origin()` (and, for
   messages, `sender.network`) must equal the authenticated peer
   (`ingest_record`, `federation.rs:1434`). This is **the single most
   security-critical function in federation**: it confines any peer to its own
   `channel@its-network` msgids.
3. **Manifest gating** (invariant 3) — the channel must be in *both* the acked
   and current manifest (`bridge::forwardable_channels`, `bridge.rs:53`).
4. **Manifest authenticity** — proposals verified against the session's bound
   peer key.

**Threat: event injection under open federation.** When `bridge_accept_any` is
on, an *unpinned* stranger network authenticates with a self-asserted key
(`federation.rs:29`) and, via auto-accept, populates the acked manifest that
invariant-3 gating trusts. Origin authority still confines injection to the
peer's own msgids, but the *channel set* it can push into is whatever
auto-accept admitted. **The security of open federation rests entirely on the
auto-accept policy** (`on_bridge_request_in` requires the namespace be `public`
+ `federation`-open, `federation.rs:326`) **and on origin authority as the
backstop.** Pinned federation (the default) has no such exposure. Tested:
`open_federation_still_honors_netblock`.

**Threat: `FSESSION` impersonation — the sharpest trust concentration.** One
network-key proof at `AUTH BRIDGE` lets a peer open a tunnelled session as *any*
`account@peer` (`on_fsession`, `federation.rs:186`) with **no per-user proof** —
"homeserver authority." A malicious or compromised peer can act as any of its
users against your grant store, bounded only by the caps you granted foreign
handles (`Actor::Foreign`). This is by design (you trust the peer's homeserver
to vouch for its users), but it means **a bridge is only as trustworthy as the
peer's own account security.** Mitigations available to an operator: pinned-only
federation, `NETBLOCK`, and granting foreign handles minimal caps.

**Blob mirroring is narrower than event federation** — the data-plane MIRROR
path verifies against the *static `[[peers]]` config keys*, not the accept-any
set (`weftd/src/media.rs:293`). Open federation does **not** widen blob access.

**Per-device attestation gap (invariant 2, partial).** Trust is network-key
level: `msgid.origin == authenticated peer`. Per-event `att=` attestations for
backfilled events (§11.7) are **not implemented** — a documented M5b refinement.
The invariant's "verify attestations for backfill exactly as for live" is met
only at network granularity, not per device.

---

## 5. Denial of service & resource exhaustion

### Well-bounded (defense present)

| Vector | Bound | Where |
|---|---|---|
| Line flood (no newline) | 8 KiB cap at the **codec**, before parsing | `quic.rs:179`, `ws.rs:28` |
| Line/tag/param/attachment grammar | 8192 B / 32 tags / 15 params / 10 attachments | `weft-proto/src/line.rs:16-28` |
| Slow consumer | Bounded broadcast (512) + session queues (256); `Lagged → ERR SLOW` | `channel.rs:26`, `session.rs:1719` |
| Idle connections | 30 s pre-auth / 120 s ready / 30 s voice, + quinn idle | `session.rs:59-74` |
| HISTORY / SEARCH size | clamp to 500 / hard 50; large pages stream out-of-band | `relay.rs:537,820` |
| Malformed flood | disconnect after 5 in 60 s | `session.rs:645` |
| Namespaces per account | quota (default 10) | `session/namespaces.rs:93` |
| REPORT / FEDERATE | 10/h · per-account cooldown | `session/moderation.rs:19`, `context.rs:642` |
| Delegation-chain verify | **iterative, not recursive** — no stack blow-up | `captoken.rs:303` |

### Genuine DoS risks (defense absent or partial)

- **D-1 · Unauthenticated Argon2id on the async runtime.** Every `AUTH` runs
  Argon2id (~19 MiB, t=2) **inline on a tokio worker thread** — no
  `spawn_blocking` (`weft-crypto/src/password.rs:23`, called from
  `accounts.rs:99`). By anti-enumeration design, *every* attempt — even for a
  nonexistent account — pays the full cost via the dummy-hash path. With **no
  auth-attempt throttle** and **no connection cap** (D-2), a modest number of
  connections spamming `AUTH` can saturate CPU and exhaust worker threads. **This
  is the most material unauthenticated resource-exhaustion vector.** Fix:
  `spawn_blocking` for Argon2 + a per-IP auth throttle.
- **D-2 · No global concurrent-connection cap.** The accept loops
  (`acceptor.rs:88,140,165`) spawn every connection into a `JoinSet` with no
  `Semaphore`. Per-connection memory is bounded (several 256-deep queues) but the
  *count* is not — memory and thread pressure scale linearly with connections.
- **D-3 · No general per-message rate limiter.** Well-formed command floods
  (`MSG`, `HISTORY`, `JOIN` churn) are unthrottled; only malformed/REPORT/FEDERATE
  are limited. The `THROTTLED` error exists but is wired only to those two verbs.
- **D-4 · `SEARCH` is an unindexed substring scan.** Postgres `body ILIKE
  '%q%'` (`postgres.rs:315`) and memory `contains` (`memory.rs:235`) both
  full-scan a channel's messages. `SEARCH_LIMIT` caps *rows returned*, not *rows
  scanned*. Any member can drive repeated full scans (no rate limit — D-3).
- **D-5 · No cap on accounts or channels created.**
- **D-6 · IRC gateway reader is uncapped.** `read_line` into a `String`
  (`weft-irc/src/lib.rs:115`) has no per-line length limit, unlike the 8 KiB-capped
  native transports — an IRC peer can buffer an unbounded single line.

### Stubbed

- **The `ERR SLOW` forced-HISTORY-resync is not implemented** — the code sends
  the error but comments "completes this once M3 exists" (`session.rs:1413`).
  Memory safety holds (entries are dropped, not buffered); the resync half does
  not exist and has **no test** — the one clear violation of the
  invariants-as-tests rule (invariant 6).

---

## 6. Cryptography & authorization

**Primitives are sound.** Ed25519 via `ed25519-dalek` with curve-point
validation on key construction (`keys.rs:18`); randomness is `OsRng`/`ThreadRng`
(CSPRNG) throughout; `#![forbid(unsafe_code)]`. Every signed body is a
**positional CBOR tuple with a domain-separation tag prefix** — deterministic
without relying on canonical-map ordering, and a signature for one statement
type cannot be replayed as another (`manifest.rs`, `rotation.rs`, `profile.rs`,
`voice.rs`, `attestation.rs`). Capability tokens are `VERSION=2` (v1 name-subject
tokens rejected at parse), and **only a `Subject::Key` can sign a child** — the
chain rules resist forgery, scope-widening, and cap-escalation, all unit-tested
(`captoken.rs:463-619`). Passwords: Argon2id, per-hash `OsRng` salt, verify by
full recompute (fails closed on malformed PHC). Admin cookie MAC verified with
`hmac`'s constant-time `verify_slice`. **No non-constant-time secret comparison
was found.**

**Flags, weakest first:**

- **F-1 · Cookie MAC secret is the raw network signing-key seed.** `weftd`
  passes `identity.seed_b64()` as the admin cookie HMAC secret
  (`weftd/src/lib.rs:698`). This is cross-primitive key reuse: the same 32 bytes
  are the Ed25519 private key (attestations, tokens, manifests, rotations) *and*
  the HMAC-SHA256 cookie key. No known interaction makes this directly
  exploitable, but the cookie secret now carries the blast radius of the network
  identity. **Fix: derive the cookie key via a KDF with a distinct label.**
- **F-2 · The admin cookie is an un-revocable stateless bearer for 12 h.** The
  token is `account|exp|HMAC` with no server-side session id
  (`weft-admin/src/auth.rs:304`); logout only clears the client cookie. A leaked
  cookie is valid until `exp` regardless of logout, password change, or grant
  revocation. (Scope *changes* do take effect immediately — scopes are recomputed
  per request.) Fix: a server-side session table or a revocation epoch.
- **F-3 · `verify_chain` has no runtime callers.** Same-network authorization
  runs off **DB grant rows** (`context.rs:734`, `actor_has_cap`), not token
  crypto — `mint_token` produces a client artifact nothing re-verifies. The
  elaborate chain rules are a complete, tested primitive **built for federation
  (M5) and currently dormant.** Consequence for this model: on the same network,
  token forgery is irrelevant (the DB is authoritative), but the token system is
  not yet defending live traffic — do not assume it gates anything today.
- **F-4 · Unbounded delegation-chain depth.** `verify_chain` iterates a
  caller-supplied slice with no length cap (`captoken.rs:303`) — one Ed25519
  verify per link. Dormant today (F-3), but a linear-CPU DoS once federation
  wires it in. Fix: a depth cap before verification.
- **F-5 · Attestations have no per-credential revocation and no nonce**
  (`attestation.rs:7`) — a leaked attestation is bearer-replayable until its
  issuer-chosen `expires_at`, with no lower/upper bound enforced. Revocation is
  key-level only, at `.well-known/weft`.
- **F-6 · Signed-blob `verify()` proves the signer, not the authority.**
  `SignedManifest`/`SignedProfile`/`SignedVoiceRelayGrant::verify` authenticate
  the embedded signer; whether that key is *entitled* to the scope is the
  caller's job (documented, `manifest.rs:109`). Safe only if callers always pair
  `verify()` with `signed_by(expected_key)`.
- **F-7 · Minor.** The challenge message is `nonce‖network` with no length
  delimiter (`challenge.rs:11`) — safe only because the nonce is fixed 32 B. The
  email/SMS verify code uses `rand % 1_000_000` (modulo bias, negligible for a
  6-digit human code) (`session/verify.rs:41`).

**Namespace takeover (zero-delay rung 3) is correctly gated.** Both the wire
path (`namespaces.rs:637`, requires a network-key signature) and the admin path
(`weft-admin` `takeover_namespace`, `require_operator` — operator-only, *not*
`admin.destroy`, so a delegated all-scope admin cannot seize communities) still
require network-key-level authority. The zero-delay change removed the *waiting
window*, not the signature requirement; it remains announced and permanently
audit-marked (invariant 9, tested).

**Privilege-escalation guard is load-bearing and correct.** Changing *who is an
admin* is gated on `require_operator` — true operator only, not scope-based —
precisely so a delegated `admin.*` grant (which confers every scope) cannot
self-promote (`weft-admin/src/handlers.rs:534`, tested
`a_delegated_admin_cannot_escalate_its_own_privileges`).

---

## 7. Parsing safety & fuzzing

The L0 parsers see unauthenticated remote bytes before anything else and are the
project's designated fuzz surface (CLAUDE.md: "No I/O in L0 … must stay fuzzable
in isolation"). Status:

- `#![forbid(unsafe_code)]` on `weft-proto` and `weft-crypto`.
- **Three fuzz targets** (`parse_line`, `parse_request`, `parse_reply`) under
  `crates/weft-proto/fuzz/`, run 60 s each in CI.
- **A stable-toolchain corpus test** (`weft-proto/tests/adversarial.rs`) asserts
  the same two properties CI can enforce without nightly: **no panic** (a panic
  here is a remote crash) and **strict-out** (anything emitted must re-parse
  identically). Corpus includes dangling/unknown escapes, NUL/BiDi/ZWJ unicode,
  integer fields at overflow, the 8 KiB boundary, 500-tag lines, and **every
  prefix of a valid line** (a partial socket read).
- Result: no panic or round-trip drift found.

The bounds are enforced *before* allocation, at both the transport codec and the
parser. This is the best-defended part of the system.

---

## 8. `unwrap` / panic assessment & removal plan

Measured across `crates/*/src`, excluding test modules: **212 `unwrap`/`expect`
in production code**, categorized:

| Category | Count | Panic-reachable from untrusted input? |
|---|---:|---|
| `Mutex/RwLock::lock().expect("… lock")` (std, poisoning) | 165 | No — only after another thread already panicked |
| Provably infallible (`ciborium` to `Vec`, `Hmac::new_from_slice`, argon2 default params, const `IdleTimeout`, non-grant `base_str`) | ~20 | No — encode a type-level invariant |
| Postconditions after an explicit guard (`chain.last()` after non-empty check; voice-backend/membership "checked above"; self-held mpsc senders) | ~15 | No — the guard runs first |
| Literal `parse()` of a compile-time string (`"retained:90d"`, `"unknown"@…`) | ~5 | No |
| `weft-tui` (dev CLI, not the server) + placeholder ACME cert | ~7 | N/A / dev only |

**There are zero production `unwrap`s on a fallible untrusted-input path in the
server crates.** The parser, auth, federation-ingest, and store-query paths all
return typed `Result`s. So "lots of unwraps" overstates the correctness risk —
but the lock pattern is worth removing because a panic while holding a std lock
**poisons it permanently**, turning one bug into a persistent failure of that
subsystem (the in-memory store is the main user; there are 128 there).

**Removal plan, by leverage:**

1. **Swap `std::sync::Mutex/RwLock` → `parking_lot`** in `weft-store` and
   `weft-core`. `parking_lot` locks don't poison; `.lock()` returns the guard
   directly, so all **165** `.expect("… lock")` become `.lock()` with no
   `Result`. This is a mechanical change and removes ~78% of the total in one
   dependency swap. (Not currently a dependency — confirmed.)
2. **Keep the ~40 provably-infallible `expect`s**, which document a real
   invariant, but **prevent new fallible ones** by adding
   `#![warn(clippy::unwrap_used, clippy::expect_used)]` to the L0 crates
   (`weft-proto`, `weft-crypto`), with `#[allow]` + a justification comment at
   each of the ~11 remaining sites. This makes every future `unwrap` on the
   parser/crypto path a conscious, reviewed decision.
3. **Optionally** restructure the ~15 "checked above" postconditions to return
   the value from the check (making the invariant type-level), and hoist literal
   parses into `LazyLock` consts. Low urgency.

Net effect: 212 → ~47, all of which are either documented-infallible or
dev-tooling, plus a lint that keeps the fallible count at zero on the L0 paths.

---

## 9. Security-invariant status (the 13)

The project treats invariants as tests. Enforcement and test coverage, verified:

| # | Invariant | Enforced | Tested | Gap |
|---|---|---|---|---|
| 1 | Anti-enumeration (uniform `NO-SUCH-TARGET` + timing) | ✅ code (single sink `session.rs:1587`) | ◑ code-uniform tested; **timing not** | Timing envelope is only *structural* — no delay/constant-time normalization on the NO-SUCH-TARGET branches, and no timing test. A store-hit-then-gate (private) can differ in latency from a miss (absent). Only the password path actively equalizes timing (dummy hash). |
| 2 | Origin authority for EDIT/DELETE | ✅ `relay.rs:320`, `ingest_record:1434` | ✅ `edit_authority_is_author_only`, `bridge_drops_foreign_origin_events` | Per-device attestation for backfill not implemented (network-level only, M5b). |
| 3 | Manifest gating of forwarding | ✅ `bridge.rs:53` | ✅ `bridge_gates_ingest_on_acked_manifest` | — |
| 4 | Caps precede side effects | ✅ `context.rs:784` before every write | ✅ `channel_policy_and_delete_require_caps`, `moderation_requires_the_cap`, … | — |
| 5 | Auth: sign `nonce‖network`, CT password, uniform `AUTH-FAILED` | ✅ `challenge.rs:11`, `session.rs:786` | ✅ `cross_network_replay_is_rejected`, `auth_failed_is_uniform_across_causes` | `subtle` dropped; constant-time compare is implicit in argon2 recompute, not asserted by a timing test. |
| 6 | Backpressure / `SLOW` | ◑ `ERR SLOW` sent (`session.rs:1719`) | ❌ **no test** | **Forced-HISTORY-resync is a stub** (`session.rs:1413`). The one clear invariants-as-tests violation. |
| 7 | `NETBLOCK` name-keyed | ✅ `federation.rs:19` (checked before key match) | ✅ `netblock_stops_ingestion_from_blocked_peer` | Four effects enforced via the auth boundary, not four separate checks (adequate). |
| 8 | E2EE unrepresentability | ◑ `e2ee` channel creation *rejected* (`channels.rs:22`) | ◑ `dm_thread_browse_…_gates_e2ee` | Satisfied **vacuously** — e2ee channels can't be created (M6 deferred), so the purge/empty-transition machinery and "recovery never restores e2ee" are unimplemented, not proven. |
| 9 | Recovery ladder (rung-3 zero-delay) | ✅ `namespaces.rs:637`, still network-key-gated | ✅ `operator_takeover_seizes_the_namespace_immediately`, `a_takeover_still_needs_the_network_key` | — (recently changed and correctly reconciled with spec §2.4). |
| 10 | Compaction | ✅ shared `compaction_plan` (`compact.rs:22`) | ✅ 14 unit tests + `history_serves_compacted_batches` | — |
| 11 | Retention holds | ✅ refcounted, skip purge+compaction (`memory.rs:84`) | ✅ store contract (mem+PG) + `report_flow_…` | — |
| 12 | Report confidentiality | ✅ `moderation.rs:101`, reporter stripped `federation.rs:566` | ✅ `report_flow_…confidentiality`, `forwarded_report_…stripping_reporter` | — |
| 13 | Auto-federation SSRF guard | ✅ `is_dialable` (`dialer.rs:421`) | ✅ `ssrf_guard_rejects_internal_targets` (11 hostile cases incl. v4-mapped-private) | — the strongest-tested invariant. |

**Weakest three:** #6 (SLOW resync stub + untested), #1 (timing not equalized),
#8 (satisfied only by not-yet-implementing e2ee).

---

## 10. Comparison to similar projects

Positioned against the servers WEFT's design most resembles or reacts to. The
pattern: WEFT's **architecture** is competitive-to-better; its **operational
DoS hardening** is behind mature servers, which is a maturity gap, not a design
flaw.

| Dimension | WEFT | Matrix / Synapse | XMPP (ejabberd / Prosody) | IRCd (Solanum) | Mastodon (ActivityPub) |
|---|---|---|---|---|---|
| Memory safety of the parser | **Rust, `forbid(unsafe)`, fuzzed** | Python (safe, slow) | Erlang/Lua (safe) | **C — historically overflow-prone** | Ruby (safe) |
| Federation blast radius | **Non-transitive, one hop, signed manifest, explicit per-peer** | Transitive-ish; full state replication + state resolution (a known DoS/complexity source) | s2s dialback/SASL, non-transitive | None (or ad-hoc links) | Transitive follows; push fan-out |
| SSRF posture | **Explicit `is_dialable` classifier, tested** | Media-repo SSRF has been a recurring CVE class | N/A mostly | N/A | **Link-preview/media SSRF a known issue class** |
| Authorization model | **Capability tokens** (dormant) + DB grants; scoped admin RBAC | Bearer access tokens, power-levels | affiliations/roles | oper/modes, K-lines | role bitmask |
| Connection/rate DoS controls | **Weak — no conn cap, no general shaper, inline Argon2** (§5) | Per-endpoint limits, worker model | **Mature: c2s shapers, stanza limits, per-IP** | **Mature: throttle module, max-per-IP, K/D-lines** | Sidekiq queues, rack-attack |
| Anti-enumeration | **Normative, single `NO-SUCH-TARGET` code** (timing gap) | Varies by endpoint | Varies | Minimal | Minimal |
| E2EE | Deferred, but **unrepresentable-when-server-readable** by construction | Olm/Megolm (shipped) | OMEMO (plugin) | None | None |

**Takeaways.** WEFT is *ahead* of the mainstream on: parser memory-safety +
fuzzing, federation containment (Synapse's transitive state model is precisely
what WEFT's one-hop manifest design avoids), and SSRF (an explicit tested
classifier where Synapse and Mastodon have had repeated CVEs). It is *behind*
ejabberd and IRCd on operational DoS controls — connection caps, per-IP
throttles, and stanza/command shapers are decades-mature there and largely
absent here (§5, D-1…D-6). Closing that gap is the single most impactful
hardening work.

---

## 11. Prioritized recommendations

**P0 — unauthenticated availability & key hygiene**

- **`spawn_blocking` for Argon2 + a per-IP auth-attempt throttle** (D-1). The
  clearest unauth DoS.
- **A global concurrent-connection cap** (`Semaphore` in the accept loops) and a
  **per-IP connection cap** (D-2).
- **Derive the admin cookie key via a KDF**, not the raw network seed (F-1).

**P1 — availability & confidentiality depth**

- **A general per-connection command rate limiter** (token bucket), wiring the
  existing `THROTTLED` error to more than REPORT/FEDERATE (D-3).
- **Cap the IRC gateway line length** to match the 8 KiB native cap (D-6).
- **Server-side admin session revocation** (table or epoch), so logout /
  password change invalidate cookies (F-2).
- **Index the SEARCH path** (Postgres trigram/FTS) or gate it behind the rate
  limiter (D-4).
- **Implement or test the `ERR SLOW` forced resync** — close the invariant-6
  gap, and add the missing test either way.

**P2 — hardening & hygiene**

- **Swap to `parking_lot`** to remove 165 poisoning `expect`s (§8), and add
  `clippy::unwrap_used`/`expect_used` warnings on L0.
- **A depth cap in `verify_chain`** before federation activates it (F-4).
- **Account/channel creation caps** (D-5).
- **A timing-equalization pass** on the NO-SUCH-TARGET branches, or an explicit
  decision that structural uniformity is sufficient (invariant 1).
- **A timing test** for the uniform-`AUTH-FAILED` path (invariant 5).

**Residual by design (accepted):** a malicious operator can read non-`e2ee`
content, freeze/delete, and seize namespaces — bounded by accountability
(audit + permanent operator marks) and by `e2ee` as the one boundary they can't
cross. The `insecure-client` cert verifier is feature-gated to test tooling and
must never be enabled in production (`weft-transport/src/insecure.rs`).

---

## 12. What this model does *not* cover

- **Load/DoS testing** — this is a static review; none of the DoS vectors above
  has a load test proving the bound (or the gap) empirically.
- **The desktop client** (Tauri) and its native-capture stack — reviewed only
  for the media-upload path, not as an attack surface.
- **Deployment/operational security** — TLS termination, reverse-proxy config,
  secret storage, and OS hardening are the operator's responsibility (the IRC
  and plain-WS listeners *assume* upstream TLS).
- **A formal timing analysis** of the anti-enumeration paths.
