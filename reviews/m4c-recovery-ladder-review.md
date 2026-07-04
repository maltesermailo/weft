# M4-6 Review — the namespace recovery ladder (§2.4, invariant 9)

*Self-review of the recovery ladder — the highest-stakes code in the
protocol (invariant 9: no silent root-rotation path). Status: 206 tests
green workspace-wide, clippy `-D warnings` clean, store contract green
against live PostgreSQL, signed NS TRANSFER verified end-to-end over real
QUIC.*

## What shipped

The full three-rung ladder from §2.4:

- **Rung 1 — TRANSFER (no delay):** `NS TRANSFER <name> <account>` with a
  `@sig=` from the current root key. Verified against the namespace's
  stored root **key** — the one place same-network namespace authority is
  now *cryptographically* enforced, not table-based.
- **Rung 2 — social quorum (7 d):** `NS RECOVERY SET <name> <m> <keys>`
  designates the M-of-N quorum; `NS RECOVER` takes a base64 `SignedRotation`
  co-signed by ≥ m quorum members → a pending recovery with a 7-day window.
- **Rung 3 — operator last resort (30 d):** the same `NS RECOVER` signed by
  the network/operator key → a 30-day window, permanently marked
  operator-initiated in `root-history`.
- **CANCEL:** `NS RECOVERY CANCEL` with a root-signed veto — a live root
  always wins.
- **Application:** a scheduled task (alongside maintenance) applies pending
  recoveries at their eta: rotate root key + owner, append `root-history`.

## The crypto (weft-crypto `rotation` module)

Three domain-separated signed statements — `weft-ns-transfer/1`,
`weft-ns-rotation/1`, `weft-ns-cancel/1` — each deterministic-CBOR
encode-before-sign, so a signature for one can never be replayed as
another. `SignedRotation::quorum_signers` counts **distinct** valid quorum
members (a test proves padding with duplicates or outsiders can't
manufacture a quorum), and tampering with the record after signing
invalidates every signature. Pure, no clock — `now` and the delay live in
the server, so all of this is unit-tested without waiting real days.

## How invariant 9 is upheld — "no silent root rotation path"

Every path that changes a namespace's root is auditable and gated:
1. **TRANSFER** requires a root-key signature (forged sigs → FORBIDDEN).
2. **RECOVER** only ever *starts a delayed, announced* pending state — it
   never rotates immediately. The rung is decided by whose signatures
   verify; insufficient or wrong-namespace signatures → FORBIDDEN; a second
   while pending → CONFLICT.
3. **Application** happens only in the scheduler, only after the delay, and
   always writes `root-history` (rung-3 marked operator-initiated forever).
4. **CANCEL** lets a live root veto during the window.

There is no code path that rotates a root without either a signature
(TRANSFER) or a delay+announcement+record (RECOVER→schedule). The
application logic is a standalone `apply_due_recoveries(store, now_ms)` so
it's unit-tested directly (drive RECOVER → pending; apply at a far-future
`now` → owner+key rotated, `root-history` records the rung).

## Honest limitation (flagged in the spec amendment)

The §2.4 recovery **announcement** is *reflected* on NS-META (any NS query
shows `recovery=pending;recovery-eta=;recovery-rung=`) but not yet *pushed*
to all members — a push needs an ns-member broadcast channel, which the
current infra doesn't have (namespaces have owners + grants, not a member
roster with fan-out). This is a delivery gap, not an enforcement gap: the
invariant-9 *guarantees* (no silent path, cancellable window, permanent
operator marking) all hold; only the proactive notification is pull-not-
push for now. Called out rather than papered over.

## Deliberate boundaries

- **Bridge interaction** (§2.4: announce root rotation to bridge peers,
  auto-suspend on rung-3) is federation (M5).
- **E2EE recovery caveat** (recovery never restores e2ee history) is
  automatic — there's no e2ee history to restore until M6.
- **TRANSFER keeps the root key** (succession between trusting parties);
  the new owner rotates it via a future RECOVER/re-key if desired.

## Verification

- New tests: 3 crypto rotation tests (quorum distinctness, tamper, operator
  single-sig) + b64 round-trip; store contract recovery state (set/pending/
  due/clear/rotate/root-history — ×2 backends); 4 core behavior tests
  (signed transfer + authority handover, rung-2 quorum → pending → root
  cancel, insufficient/wrong-namespace signature rejection, scheduler
  application at expiry with root-history); 1 conformance test (signed
  TRANSFER over real QUIC, forged-sig FORBIDDEN, authority handover).

## Next

M4c reporting: REPORT / REPORTS LIST / RESOLVE, content states, retention
holds that exempt reported events from purge + compaction (invariant 11),
reporter confidentiality (invariant 12) — the last M4 piece.
