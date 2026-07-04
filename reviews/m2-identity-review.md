# M2 Review — identity (weft-crypto, real AUTH, well-known)

*Self-review of the M2 implementation. Status at time of review: 101 tests
green workspace-wide (12 crypto, 23 core, 10 conformance, 7 tui, 49 proto),
clippy `-D warnings` clean, `cargo fmt` applied, binary smoke-tested live
(well-known over HTTP, signing-key persistence across restarts).*

## Scope reviewed

| Piece | Contents |
|---|---|
| `weft-crypto` (new, L0) | `keys.rs` (validated Ed25519 wrappers + b64), `challenge.rs` (`nonce‖network-name` proofs, §6.1), `attestation.rs` (deterministic-CBOR encode-before-sign, §10.2), `password.rs` (constant-time SHA-256 compare) |
| `weft-core` | `accounts.rs` in-memory registry; `session.rs::on_unauthed` rewritten: REGISTER (open/closed, ≥12 B, CONFLICT), AUTH PASSWORD, AUTH KEY→CHALLENGE→PROOF, AUTH ENROLL in READY; `ServerCtx` gained the signing key + attestation minting |
| `weftd` | `/.well-known/weft` (axum, JSON `{protocol, network, signing-key}`), persisted signing key (b64 seed file, created on first boot), operator PEM certificates (`[tls]` config), `registration = "open"\|"closed"` |
| `weft-tui` | ≥12 B dev password; unknown account → auto-REGISTER once, then proceeds |
| Spec | §6.1 AUTH ENROLL response defined (`@attestation=` WELCOME); §10.2 well-known JSON pinned; both noted in Appendix A |

## Security invariant 5, implemented as tests (per CLAUDE.md)

- **Uniform AUTH-FAILED**: wrong password, unknown account, proof-without-
  challenge, unknown device, cross-network proof — one code, one text.
  `auth_failed_is_uniform_across_causes` asserts byte-equal error text
  across causes.
- **Constant-time password compare**: `subtle::ConstantTimeEq` over SHA-256
  digests; unknown accounts verify against a static dummy hash so the
  missing-account path does the same work as the wrong-password path.
- **Cross-network replay**: proofs sign `nonce‖network-name`; a proof minted
  for `evil.example` is rejected here (unit test in crypto AND black-box
  session test).
- **Challenge hygiene**: 32-byte random nonce, single-use (consumed by any
  PROOF — replay test), replaced by a subsequent AUTH KEY. AUTH KEY issues a
  CHALLENGE *regardless* of account/key existence: nothing about account
  state is observable before PROOF, and PROOF evaluates signature validity
  and device enrollment unconditionally (no early exit) before deciding.
- **Attestations**: expiry enforced, tampering with any field (account,
  network, expiry) breaks the signature, wrong issuer fails. The conformance
  test closes the whole loop the way a *remote network* would: fetch
  `/.well-known/weft` over real HTTP, parse the signing key, verify the
  attestation from a live AUTH KEY handshake against it.

## Design decisions

- **First device enrollment is explicit**: AUTH KEY succeeds only for
  already-enrolled keys; the first key gets bound via a password-authed
  session (`REGISTER`/`AUTH PASSWORD` → `AUTH ENROLL`). "AUTH KEY binds the
  device" (§6.1) is read as binding the *session* to the proven key — an
  auto-enrolling AUTH KEY would let anyone claim any account with a fresh
  key.
- **Attestation payload is a CBOR array** (positional fields), not a map —
  deterministic by construction, no key-ordering rules to enforce or fuzz.
- **weft-crypto does not depend on weft-proto** (both are leaves, per the
  architecture graph), so attestation account/network fields are plain
  strings validated by weft-core against proto's types before use.
- **Accounts live in weft-core, in memory** — M3's `AccountStore` trait will
  absorb this as the memory backend. Registration state, hashes, and device
  enrollments do not survive a restart yet; that is the M3 boundary, not an
  oversight. The *signing key* does persist (attestation continuity matters
  across restarts in a way account state doesn't for a dev server).
- **ATTESTATION_TTL = 30 days** (const in weft-core): rotation-friendly,
  refreshed on every key auth. Becomes network config when config grows.

## Accepted limitations (each with its landing spot)

- **SHA-256 is not a KDF.** Fine while hashes are memory-only; M3 must move
  to argon2 (or similar) before password hashes touch disk. Recorded in the
  password module docs and the M3 focus line in CLAUDE.md.
- **Well-known is plain HTTP** — the spec's `https://` URL is the public
  address; TLS termination is deployment-side until the HTTP surface grows
  in later milestones. The QUIC plane does use operator PEM certs now.
- **No attestation revocation list** on well-known yet (spec: "revocation
  via well-known") — the document format has room; the mechanism belongs
  with device management UX, deferred.
- **No auth attempt rate-limiting** (deferred THROTTLED bucket, CLAUDE.md
  "do not add" list).
- **TUI does not yet do key auth** — it registers/password-auths; AUTH KEY
  is covered by conformance and core tests. A `/enroll` + keypair file in
  the TUI would be a nice follow-up.
- The M1 idle-liveness observation stands: a PING-less client relying on
  QUIC keepalive still gets dropped at ~30 s (spec would allow it). Parked
  in `reviews/fix-quic-idle-disconnects.md`.

## Verification

- 28 new tests: 12 crypto units (round-trips, tamper/expiry/issuer/garbage
  rejection, replay), 8 new core auth tests (full CHALLENGE/PROOF flow with
  attestation verification, uniformity, gates), 3 new conformance tests
  (black-box key auth + well-known verification over real QUIC+HTTP, closed
  registration + wrong password, operator PEM path), TUI fallback tests.
- Live binary check: booted with a config twice — key file generated on
  first boot, `signing-key` in the well-known JSON identical across
  restarts, `{"protocol":"weft/1","network":"m2.example","signing-key":…}`
  served.

## Next step

M3 — persistence: weft-store traits + SQLite (sqlx), retention/purge,
HISTORY/BATCH with compaction (§12.1), EDIT/DELETE/REACT materialization,
MARK sync, DMs. First move: extract `AccountStore`/`EventStore` traits and
re-home the in-memory `Accounts` behind them.
