# M4a Review — capability foundation: tokens, GRANT/CHANNEL/INVITE, view gating

*Self-review of M4a (subtasks M4-1..M4-4 of M4). Status at time of review:
184 tests green workspace-wide, clippy `-D warnings` clean, the store
contract green against live PostgreSQL, and the full flow verified end-to-
end against the real binary over WebSocket.*

## Scope reviewed

| Subtask | Contents |
|---|---|
| M4-1 `weft-crypto` | `Capability` enum (18 standard caps + nesting `grant:<cap>`), signed deterministic-CBOR tokens, `verify_chain` (root→leaf: signature, scope narrowing, `grant:X`-to-delegate, expiry, per-scope epoch), 29 tests |
| M4-2 `weft-proto` | GRANT/REVOKE, CHANNEL CREATE/POLICY/META/DELETE, INVITE MINT/REVOKE/REDEEM; TOKEN/INVITED/CHANMETA events; round-trip tested |
| M4-3 `weft-store` | `ChannelStore` (meta+delete), `CapabilityStore` (grants + epochs), `InviteStore` (counters, atomic redeem); memory + Postgres, one shared contract suite |
| M4-4 `weft-core` | Enforcement (`account_has_cap`/`account_can_grant`), GRANT/REVOKE, CHANNEL verbs (registry now mutable — lazy actor spawn/park), INVITE lifecycle, view gating |

## Load-bearing design decisions

1. **Invariant 4 has one choke point.** Every mutating capability verb
   calls `account_has_cap`/`account_can_grant` *before* touching state, and
   the check reads grants that (a) cover the object scope, (b) are
   unexpired, (c) sit at or above the scope's current revocation epoch. A
   `CAP-REQUIRED <cap>` (or `grant:<cap>`) is the uniform refusal.
2. **The crypto is pure and separately tested.** `verify_chain` (M4-1) has
   no clock and no store — `now` and the epoch lookup are parameters — so
   delegation/forgery/expiry/revocation are proven in 15 unit tests without
   any server. The enforcement layer uses a server-side grant *table* as
   the same-network fast path; the signed token it returns is for
   delegation and (later) federation, where `verify_chain` is the checker.
3. **Operators bootstrap the chain.** weftd config `operators = [...]` names
   accounts that hold the network key's authority (every cap at `*`, §11.3).
   Without this there is no first admin — no one could ever be granted
   anything. Everyone else's caps chain from a GRANT.
4. **View gating reuses the anti-enumeration code (invariant 1).** A
   view-gated channel with no `view` cap answers `NO-SUCH-TARGET` on JOIN —
   byte-identical to a channel that does not exist — and the cap check
   *fails closed* on a store error so a hiccup never leaks existence.
5. **The registry became mutable without locks on the hot path.** An
   `RwLock<HashMap>` gives CHANNEL CREATE/DELETE their mutation; handles are
   cloned out under a brief read lock never held across `.await`, so channel
   actors keep running lock-free.

## Response-event decisions (spec was loose; pinned in Appendix A)

The spec sketched these verbs without response payloads. Rather than invent
a generic ACK, each verb replies with the event that best represents its
result: GRANT/REVOKE → `TOKEN` (REVOKE re-mints reflecting remaining caps,
empty if none); CHANNEL CREATE/POLICY → `POLICY`; META → `CHANMETA`; DELETE
→ `CHANMETA … deleted`; INVITE MINT → `INVITED`; INVITE REVOKE → `INVITED …
max-uses=0`; INVITE REDEEM → the JOIN response (auto-join). All labeled, so
the ack philosophy (§3.5) holds.

## Deliberate M4a boundaries (each with its landing spot)

- **`ns:` scopes and namespaced channels defer to M4b** — grants/invites at
  `ns:` need the namespace root key, which arrives with namespaces. GRANT
  `ns:...`, INVITE `ns:...` redeem, and `CHANNEL CREATE #ns/chan` answer
  UNSUPPORTED for now.
- **Invites are server-side id+counter records.** The offline-verifiable
  *unbound capability token* form (§6.5) is a federation concern; a same-
  network redeem only needs the server to hold the counter and mint the
  member grant. Noted in the amendment.
- **CHANNEL META topic/view-gated broadcast to members is not live yet** —
  the change persists and gates correctly, but only the acting session gets
  the labeled CHANMETA; members see settings on next join. A follow-up adds
  an actor broadcast path (like POLICY already has).
- **CHANNEL POLICY `purge` drops everything currently stored** (simplest
  correct tightening); a policy-aware selective purge can refine it.
- **Token delegation-by-presentation** (a client GRANTing via its own held
  token rather than an operator/table grant) isn't wired into enforcement —
  the table is the fast path. `verify_chain` is ready for it when federation
  needs it.

## Verification

- 40+ new tests: crypto chain/forgery/epoch (15), proto round-trips (7),
  store contract additions across both backends (channels-meta, grants,
  epochs, invites), core behavior (grant→use→revoke, operator bootstrap,
  CAP-REQUIRED naming, channel policy/meta/delete, view-gate hiding, invite
  mint/redeem/exhaust/revoke).
- Live (real binary, WS): `CAP-REQUIRED chan-create` → operator `GRANT` (a
  real 400-byte signed `TOKEN ada *`) → `CHANNEL CREATE` succeeds; a
  view-gated `#club` is `NO-SUCH-TARGET` to a non-member; `INVITE REDEEM`
  auto-joins it; a second redeem is exhausted → `NO-SUCH-TARGET`.

## Next step (M4b)

Namespaces (NS CREATE with client-generated root key, META/VISIBILITY/
DELEGATE/TRANSFER/DELETE, DISCOVER) unlock `ns:`-scope grants/invites and
namespaced channels, then the recovery ladder (invariant 9) — the subtlest
code in the protocol, deserving its own fresh session.
