# E2EE via OpenMLS — implementation plan

Status: **design, for approval** (2026-07-10). Realizes the `e2ee` retention mode
(spec §5.2, §14) with **MLS (RFC 9420)** via [OpenMLS], turning the reference
server into a **blind Delivery Service** and the clients into the only holders of
plaintext.

[OpenMLS]: https://github.com/openmls/openmls

> This lifts two standing entries: CLAUDE.md's "Deliberately deferred — do not add:
> openmls" and the "M6+ … E2EE (openmls, feature `e2ee`)" line. Per the decisions
> below the engine is **always-on**, not feature-gated. Update CLAUDE.md when Phase
> E0 lands.

## North star (already fixed by the spec — do not relitigate)

- `e2ee` channel mode = **MLS group keying; server = blind DS** (§14).
- **Invariant 8 — unrepresentability:** no code path may hold plaintext for an
  `e2ee` channel. The retention enum makes "encrypted but server-readable"
  impossible to express. Policy transitions to/from `e2ee` need an **empty channel
  or explicit `purge`**.
- **Invariant 10:** the server cannot compact ciphertext — no materialization, no
  `EDITED`/reaction folding on e2ee events.
- **§2.4 recovery never restores e2ee history** — a recovered root joins as a
  fresh MLS member with no access to prior content. Host-blind includes
  host-blind-from-recovery.
- Today `RetentionPolicy::E2ee` exists in `weft-proto` but is hard-blocked at
  `CHANNEL CREATE`/`POLICY` and boot seeding ("lands in M6"). This plan *unblocks*
  it correctly.

## Decisions (locked 2026-07-10)

| # | Area | Decision |
|---|------|----------|
| 1 | First milestone | **Engine spike first** — `weft-mls` crate wrapping OpenMLS, in-memory, unit-tested; no wire/server yet. |
| 2 | Layering | **Client-only `weft-mls` crate.** The server never depends on it; only opaque blob framing lives in `weft-proto`. Keeps invariant 8 *structural*. |
| 3 | Feature gating | **Always on.** OpenMLS is a hard dependency of the clients + `weft-tui`; the server's opaque relay is always compiled. No `e2ee` cargo feature. |
| 4 | Ciphersuite | **Negotiated**, baseline set = `MLS_128_DHKEMX25519_{AES128GCM,CHACHA20POLY1305}_SHA256_Ed25519`. Ed25519 signatures throughout; chosen at group creation. |
| 5 | Credential | **Fresh per-device MLS signature key** + a **new WEFT attestation** binding it to the account (MLS key ≠ auth key). |
| 6 | Key distribution | **KeyPackages on the account/device directory** — the existing directory actor holds a per-account pool. |
| 7 | Committer | **Any member commits; the channel actor orders by epoch** — first-Commit-per-epoch wins, stale-epoch → `ERR STALE`. No single point of failure; reuses WEFT's single-writer. |
| 8 | Membership truth | **WEFT membership drives the MLS roster.** Server-tracked JOIN/PART/invite/ban stays authoritative; a committer Adds/Removes to match. |
| 9 | Wire | **Control-plane verbs + base64 blobs.** `MLS COMMIT/WELCOME`, KeyPackages via the directory, application ciphertext as `MSG` with an opaque `@enc=` body. |
| 10 | Storage | **Store opaque blobs, no compaction.** Ciphertext + handshake events are ULID-ordered and replayed verbatim on `HISTORY`; never materialized. |
| 11 | Welcome delivery | **Server pickup queue** — a per-account inbox holds opaque Welcomes for offline adds. |
| 12 | Multi-device | **Full multi-device** — each device is its own leaf; new devices sync state + history (via #13). |
| 13 | Device sync | **Per-account device group** — a second MLS group of the account's own devices; an existing device hands over each channel's history keys. |
| 14 | Federation | **Pass-through, designed-in from day one** — bridges relay opaque MLS blobs one hop; cross-network KeyPackage fetch + Welcome routing. |
| 15 | Reporting | **reporter-attested plaintext** (§6.7) — reporter MAY attach the plaintext they saw (marked unverified); ciphertext blob held. |
| 16 | First client | **Tauri/web client** — the GUI is the first E2EE surface (via `weft-client-core`/`-wasm` + `weft-mls`). |
| 17 | Persistence | **Native OpenMLS `StorageProvider` per platform** — desktop keychain/encrypted file, web IndexedDB, tui file; in-memory for the spike. |
| 18 | Group bootstrap | **At `CHANNEL CREATE` (creator builds epoch 0, self-only)**; transition-with-purge → the transitioning admin bootstraps. |
| 19 | DM scope | **DMs from the start** — a DM is a 2-member MLS group; MLS wires into the DM path too. |

"From day one" (federation #14, multi-device #12, DMs #19) means **the wire, store,
and engine are designed to accommodate them from the first line** — not that they
enable before the single-network core works. Sequencing is in the milestones.

## Target architecture

```
weft-proto  (L0, server + client)   opaque MLS frames only — bytes weftd routes
  · MlsBlob newtypes: KeyPackage, Welcome, Commit(+epoch), AppCiphertext
  · new verbs/events (§ wire below) + ErrCode::Stale
  · NO openmls dependency

weft-crypto (L0, server + client)   the credential binding
  · new attestation kind: account-key signs a device's MLS signature key
  · KeyPackage validity = MLS self-check ∧ this attestation

weft-mls    (NEW, client-only)      the OpenMLS engine — server never links it
  · wraps openmls + a StorageProvider (in-memory → native per platform)
  · create_group / add / remove / commit / encrypt_app / decrypt_app
  · per-account device-group primitive (#13); ciphersuite negotiation (#4)
  · deps: openmls, weft-proto (blob types), weft-crypto (attestation)

weft-store  (L1, server)            blind persistence
  · opaque blob events (no materialize/compact); KeyPackage pool; Welcome inbox

weft-core   (L2, server)            the blind DS
  · channel actor orders Commits by epoch; relays blobs; membership→MLS deltas
  · unblocks e2ee at CREATE/POLICY (empty-or-purge); HISTORY replays blobs
  · links weft-proto + weft-store ONLY — never weft-mls

weft-client-core / -wasm (client)   + weft-mls  → Tauri/web/tui E2EE
```

The one non-negotiable: **`weftd` must never gain an `openmls` dependency.** A CI
check (or a grep test) asserts `weft-mls` is absent from the server binary's tree.

## Wire additions (proto-first — codec + round-trip tests before consumers)

- `KEYPACKAGE PUBLISH <n> :<b64…>` (→ directory pool); server pops one on Add,
  falls back to a reusable **last-resort** KP when the pool is empty.
- `MLS WELCOME @to=<account> <#chan> :<b64>` → queued to the target's inbox;
  delivered as `MLS WELCOME` on their next connect.
- `@epoch=<n> MLS COMMIT <#chan> :<b64>` → actor accepts iff `n == current`, else
  `ERR STALE` (client refetches head, re-proposes, retries with backoff).
- Application message: `@enc=1 MSG <#chan> :<b64 ciphertext>` — an ordinary MSG
  the server ULID-orders and stores but cannot read.
- `ErrCode::Stale` (new) for epoch races; `POLICY` error already covers
  transition-without-purge.
- Events echoed to members: `MLS-EVENT`/`POLICY` as needed so clients learn a
  group advanced. `POLICY` on join already announces `e2ee` before anyone speaks.

## Store additions

- **Opaque event blobs** — reuse the events table; e2ee rows carry ciphertext and
  are flagged non-materializable (skipped by `compaction_plan`, invariant 10).
  Retention holds pin the ciphertext only (invariant 11).
- **KeyPackage pool** — per-account list on the directory (consume-on-use +
  last-resort), migration N.
- **Welcome inbox** — per-account queue of opaque Welcomes, drained on connect,
  migration N+1.
- Shared mem+PG contract suite for all three (the house rule).

## Security invariants to add AS TESTS

- **No plaintext for e2ee** (inv. 8): a test asserts every e2ee store row is
  opaque and no server path decodes it; the `openmls`-absent-from-weftd tree test.
- **No compaction of e2ee** (inv. 10): `compaction_plan` leaves e2ee events
  untouched; batches never fold e2ee edits/reactions.
- **Recovery excludes e2ee** (§2.4): a recovered root joins as a fresh member;
  test that no pre-join epoch key is derivable.
- **Transition safety** (inv. 8): to/from `e2ee` requires empty-or-`purge`;
  purge wipes plaintext before the group exists.
- **Epoch ordering**: concurrent Commits at the same epoch — exactly one wins,
  the loser gets `STALE` and converges.

## Milestones (each independently shippable)

- **E0 — engine spike (`weft-mls`).** OpenMLS wrapper, in-memory provider,
  negotiated ciphersuites, per-device Ed25519 MLS key, `create/add/remove/commit/
  encrypt_app/decrypt_app`, and the per-account device-group primitive. **Unit
  tests only, no weftd.** *Green:* two in-process members exchange an encrypted
  message and a third is added via Commit+Welcome — all in a test.
- **E1 — wire codec (`weft-proto`).** The opaque blob newtypes, the verbs/events
  above, `ErrCode::Stale`. Round-trip tests. No behavior yet.
- **E2 — credential (`weft-crypto`).** The new attestation kind binding a device's
  MLS signature key to the account; KeyPackage validation. Signing/verify tests.
- **E3 — blind DS (`weft-store` + `weft-core`).** Blob storage (no compaction),
  KeyPackage pool, Welcome inbox; channel-actor epoch ordering; membership→MLS
  add/remove; unblock e2ee at CREATE/POLICY with empty-or-purge; group bootstrap
  at create; HISTORY blob replay. *Green:* a `weft-tui`-scripted (or in-crate
  conformance) two-client encrypted exchange over real weftd, server provably
  blind.
- **E4 — Tauri/web client + DMs.** Persistent `StorageProvider` (keychain /
  IndexedDB), client group lifecycle (create/join e2ee channel, publish KPs,
  process Welcome, send/receive `@enc` MSG, commit membership), e2ee **DMs** as
  2-member groups, and the UI (e2ee indicator, key state). *Green:* two browsers
  hold an encrypted channel + DM; weftd sees only ciphertext.
- **E5 — multi-device.** The per-account device group; new-device enrollment +
  history-key hand-off so a second device decrypts prior epochs. *Green:* add a
  second device to an account; it reads history it wasn't originally a leaf for.
- **E6 — federation pass-through.** Bridge relays opaque MLS blobs one hop;
  cross-network KeyPackage fetch (well-known/bridge) + Welcome routing; manifest
  interplay (`e2ee` bridges pass-through only, §11). *Green:* `ada@net1` +
  `bob@net2` share one group over a live bridge.
- **E7 — reporting.** `reporter-attested` plaintext + ciphertext holds; emit the
  already-stubbed `reporter-attested` content state (§6.7). *Green:* a report on
  an e2ee message pins ciphertext + optional reporter plaintext, marked unverified.

## Hard parts / risks (call out early)

- **Multi-device history (#13)** is the deepest: securely handing an account's own
  new device the exporter secrets for *every* group it's in, without the server
  ever seeing them. Two group types + key hand-off. Prototype in E0, land in E5.
- **Epoch races under the actor (#7).** The actor orders bytes it can't read; it
  must trust the client-declared `@epoch=` and reject stale — clients must
  converge on `STALE`. Fuzz the concurrent-commit path.
- **Federation KeyPackage/Welcome routing (#14)** across the one-hop manifest
  model — cross-network adds need the peer's KP pool and Welcome inbox reachable
  over the bridge without leaking membership beyond the group.
- **Web key storage (#17)** — IndexedDB is softer than an OS keychain; document
  the threat model honestly (same caveat as the P4 device-key note).
- **`purge` on transition** must be irreversible and complete — a stray plaintext
  row after enabling e2ee is an invariant-8 breach.

## Spec amendments (same-PR, per CLAUDE.md)

§14 gains the concrete wire (verbs, `@enc`, `@epoch`, `ErrCode::Stale`), the
credential-attestation binding, the KeyPackage/Welcome-inbox DS model, the
committer/epoch-ordering rule, and the per-account device-group for multi-device.
Appendix A gets a decision-history entry. Where the spec and this plan differ, the
spec wins — so these land as amendments, not drift.
