# M4-5 + layout Review — user-owned namespaces & channel categories/order

*Self-review of namespace creation (M4-5) plus the Discord-style channel-
layout extension. Status: 196 tests green workspace-wide, clippy
`-D warnings` clean, store contract green against live PostgreSQL, and the
full flow verified end-to-end against the real binary over WebSocket.*

## What shipped

**Namespaces (§2.1, §2.2)** — users create and own them:
- `NS CREATE <name> [tier]` with the client-generated root pubkey in a
  `@root=` tag; `NS META`, `NS VISIBILITY`, `NS DELEGATE` (sugar for GRANT
  at `ns:`), `NS DELETE`; `DISCOVER` with `MORE` pagination.
- Visibility tiers: `public` (in DISCOVER), `unlisted` (hidden, invite-
  only), `private` (existence denied — anti-enumeration, invariant 1).
- Creation policy: `open` (per-account quota, default 10) or `gated`
  (`ns-create` cap). `NamespaceStore` in memory + Postgres, shared contract.

**Channel layout extension** (Discord categories + order — not in base
spec, amended in Appendix A):
- Channels gain `category` (free label) + `position` (int).
- `CHANNEL META <#ns/chan> category|position :<value>` sets them.
- `CHANNELS <ns>` → ordered `CHANNEL-LAYOUT` events (sorted category,
  position, name); private-namespace layouts are view-gated.

## The load-bearing decision: namespace-owner authority

The namespace **owner account** holds every capability within
`ns:<name>` — the ns-scoped analog of an operator at `*`. One branch in
`account_has_cap` (`scope_namespace(scope)` → look up owner) unlocks
*everything* I deferred in M4a: `ns:`-scope GRANT, namespaced
`CHANNEL CREATE #ns/chan`, ns invites, and the layout verbs — all just work
for the owner, and are `CAP-REQUIRED`/`NO-SUCH-TARGET` for everyone else.
This mirrors the operator model exactly, so the enforcement code stayed a
single uniform path.

## Honest limitation (flagged, with its landing spot)

Same-network namespace authority is **account/table-based**, like operator
authority — the client-held root **key** is recorded but not yet the
cryptographic gate. This means the "operator can never silently mint
namespace membership" property (§2.1) is **not yet cryptographically
enforced** in this implementation: the grant table is the fast path, and an
operator who controls the server could fabricate table entries. The root
key is stored precisely so that NS TRANSFER (rung-1, signature-verified)
and the recovery ladder and M5 federation can enforce it cryptographically
— that hardening is deferred with those features. This is the same
table-vs-signed-token tradeoff the M4a review named, now extended to
namespaces, and it's called out in the spec amendment rather than papered
over.

## Deliberate boundaries

- **NS TRANSFER + the recovery ladder (§2.4, invariant 9) are M4c** —
  succession and recovery are the subtlest code in the protocol (a
  scheduler, coercion-resistance, root-history) and deserve a fresh
  session, exactly as flagged when M4 was phased.
- **Namespace-scope invite redeem** grants membership and returns the
  namespace's NS-META; the redeemer discovers channels via `CHANNELS`/JOIN
  (there's no single "default channel" concept yet).
- **Layout changes aren't broadcast** to members live (like CHANNEL META
  topic) — persisted and read on demand via `CHANNELS`; a broadcast path
  is a follow-up.

## Verification

- New tests: proto round-trips (NS verbs, DISCOVER, CHANNELS,
  CHANNEL-LAYOUT), store contract (namespace CRUD, quota count, public
  listing + cursor, channel layout ordering — ×2 backends), core behavior
  (any-user create + ownership unlocks, name conflict, owner-only
  meta/visibility, DISCOVER public-only, quota, bad root key, full
  category/reorder/read layout, non-owner refusal).
- Live (real binary, WS): ada creates `gaming` (public), creates three
  channels in it, categorizes + orders them, `CHANNELS gaming` returns
  `voice`(uncat) then `general`(text,0) then `random`(text,1); bob
  DISCOVERs the public namespace.

## Next

M4c: NS TRANSFER (signature-verified succession) → the recovery ladder
(the invariant-9 centerpiece) → REPORT/REPORTS + retention holds
(invariants 11, 12).
