# Concept: weftd is supreme authority for its own users

**Status:** concept, for review. Not implemented. Owner decision required (§7a territory).

**Purpose (owner, 2026-08-07):** *a user must be able to leave a namespace while the bridge is
down.* Everything below serves that.

**What is already true.** `NS LEAVE` is deliberately **not** gated on provider liveness — only
`NS JOIN` is (`on_ns_join`, owner directive 2026-08-04, because a join must be relayed
foreign-side to mean anything). `on_ns_leave` clears the row, unsubscribes the channels, and
relays to the provider fire-and-forget. So leaving with the adapter down already works as
designed.

**What actually broke it** was not the leave path but the *row*: the reconnect mass-part deleted
local membership (fixed — `on_provider_sync` no longer prunes what a realm cannot enumerate),
while the client kept showing the namespace from its own cache. That is the state the owner hit:
the rail lists a server, the channel list renders, history will not load, and `NS LEAVE` answers
`NO-SUCH-TARGET` — because there is nothing left to leave. **A leave that cannot find its row is
indistinguishable, to the user, from a leave that is refused.** Two things follow, and they are
the substance of this concept: nothing may delete a local row except the user, and the client
must not display membership weftd disowns.

**Problem.** Today a provider statement can *create* local membership. `puppet_join_namespace`
joins the puppet foreign-side and then says `NS-MEMBER <ns> <local-user> join`, and weftd writes
the row. That makes the adapter the author of a fact about **our** user, which has two
consequences the owner named:

1. **A leave does not stick.** The user leaves; weftd drops the row; the adapter still holds the
   user in its own view and re-asserts them on its next reconnect. The membership comes back
   without the user doing anything.
2. **A swapped bridge inherits authorship.** `[[plugin.remote]]` pins a key *per scheme*, so an
   operator who replaces the Matrix adapter — new key, new database, or a different
   implementation entirely — gets a provider that speaks for every `matrix://` namespace we
   hold. Its view of who belongs is whatever its store says, and it can state that view as fact.

Neither is an attack requiring a malicious operator. A restored backup, a second deployment
pointed at the same weftd, or an adapter whose database was lost all produce it.

## The rule

> A provider statement about a **local** account is an *acknowledgement*, never a command.
> Only weftd creates local membership, and only in response to that user's own `NS JOIN`.

Concretely, three asymmetries — all of which follow from asking "who could possibly know this?":

| statement | about a foreign user | about a local user |
|---|---|---|
| `join` | authoritative — it reads its own room state | **only confirms a join we are already expecting** |
| `part` | authoritative | **always honored** (removing access never needs authority) |
| omission in a full-replace | prunes | **never prunes** (already shipped) |

`part` is deliberately not symmetric with `join`. A rogue or stale adapter that parts our users
is a denial of service, which is recoverable and visible; one that *adds* them to namespaces is a
confidentiality failure, which is not. When the two directions differ in blast radius, the cheap
direction stays open.

## Mechanism

**Pending-join ledger.** `NS JOIN <ns>` on a provider-managed namespace records
`(account, ns_id, expires_at)` before relaying the command, exactly the way `label` correlation
already works elsewhere. The provider's `NS-MEMBER … join` for a local account is written **iff**
a matching entry is live; otherwise it is dropped with a warning. Entries expire (a minute is
generous — it is one foreign round trip) so the ledger cannot become a standing permission.

This is a small change with a large effect: the adapter can still be slow, retry, or reconnect
mid-join and have it work, but it cannot originate a membership. And nothing about the reconnect
path changes — re-asserting a roster we already hold is idempotent, because the row exists.

**Realm epoch.** weftd stamps a monotonic `epoch` per realm, bumped whenever a *different* key
registers for a scheme that already had namespaces. `NamespaceRecord` carries the epoch of the
key that last asserted it. A statement from a key whose epoch is behind the record's is refused
with `ERR CONFLICT realm-epoch`. This is the NETBLOCK-style answer: not "prove you are the same
adapter", which we cannot check, but "a *changed* adapter cannot silently continue the old one's
authorship". The operator's remedy is explicit — an `adopt` step in the admin panel — so a bridge
swap becomes a decision with a log line instead of an inference.

**The client reconciles to SYNC.** The server half above stops rows being lost; this stops a lost
row being invisible. `SYNC` is already the authoritative snapshot of what the account belongs to,
so at its end the client should drop any namespace — and any namespaced channel — the snapshot did
not name, instead of keeping its cached copy. Today it merges, so a namespace weftd has disowned
survives in the rail as an un-leavable ghost whose history never loads. This is the same
full-replace reasoning as `SYNC START`/`SYNC END` in the other direction, applied where the client
is the one conforming, and it makes the limbo self-healing on the next reconnect rather than
something an owner has to clear by hand.

**What this does not try to do.** It does not make the foreign side converge. If the Matrix
homeserver refuses to remove a puppet, the puppet stays in the room; weftd cannot and should not
pretend otherwise. What the rule guarantees is narrower and more honest: **what our users see and
belong to is ours**, and the worst a broken or substituted adapter can do is fail to mirror it.

## Why not the alternatives

- *Sign statements per-user.* The adapter would need each user's key. It doesn't have one, and
  giving it one inverts the trust model.
- *Make the adapter durable so its view is trustworthy.* Pushes the guarantee into an
  implementation we do not control, and every future adapter re-earns it. The whole point of
  §7a is that adapters are replaceable.
- *Version the roster and reconcile on mismatch.* Detects divergence after it has been written.
  The pending-join ledger prevents the write.

## Enforcement points (if accepted)

- `weft-core/src/session/namespaces.rs :: on_ns_join` — write the ledger entry before relaying.
- `weft-core/src/session/plugin.rs :: on_ns_member_in` — gate the local-account `join` arm on a
  live entry; leave `part` and the foreign arm untouched.
- `weft-core/src/session/plugin.rs :: on_provider_sync` — already correct (local members are
  never pruned by omission).
- `weft-store` — the ledger (memory + PG, one shared contract test) and `NamespaceRecord.epoch`.
- `client/src/lib/sync/` — drop namespaces/channels absent from a completed `SYNC` (the client
  half; independent of the rest and shippable first, since it is what makes an existing ghost
  disappear).

Tests worth writing first, because each is a rule stated as a failure:
a provider `join` for a local account with no pending entry is dropped; the same `join` with one
is written; an expired entry does not authorize; a `part` needs no entry; a key that did not
assert the namespace is refused on epoch.
