# Home-authoritative channels — a fast federation model

**Status:** design (spec amended; implementation pending).
**Supersedes:** the multi-origin channel model (each network minted its own users'
channel posts, one-hop mirror, `msgid.origin == poster network`).
**Companion spec text:** `docs/protocol/weft-protocol-spec.md` §9.1, §11.4, §11.11, §11.13.

---

## 1. Why we are changing this

Channels were **multi-origin**: every network that mirrored a channel ran its own
actor, and each actor minted *its own* network as the origin of *its own* users'
posts (`channel.rs:704`), forwarding one hop with the origin preserved
(`federation.rs:779`). This has two defects.

1. **It cannot deliver a channel with members on three networks.** Federated
   channels are hub-and-spoke: a namespace is owned by one network (the **home**),
   and other networks (**spokes**) peer *with the home*, not with each other. Under
   one-hop-no-transitivity (§11.4), a post from spoke `F` reaches the home `H`
   (one hop) — and stops. `H` will not re-forward an `F`-origin event to spoke `G`,
   because re-forwarding a non-local-origin event is exactly the transitivity the
   rule forbids. So `G` never sees `F`'s messages. Multi-origin silently works only
   for a two-network channel (home + one spoke).

2. **There is no cross-network total order.** Two networks minting ULIDs from
   independent clocks interleave by wall-clock only; the per-channel total order
   that the single-writer rule (§9.1) promises holds *inside* a network but not
   *across* one. Edit/delete/moderation ordering is likewise per-origin.

Group DMs already solved both by going **home-authoritative** (§11.12): the
creator's network is the sole ULID writer; spokes relay posts to it to be minted.
This document generalizes that model to channels — the channel's home is the
**namespace owner's network** — and works out how to keep it *fast* despite the
extra authority round-trip.

---

## 2. The model in one paragraph

Each channel has exactly one **home** = the network that owns its namespace. The
home runs the single authoritative actor: it is the sole ULID writer and the sole
origin of the channel's events (`msgid.origin == home`). Every other member network
runs a **replica** — a local cache of the home-minted log plus a short tail of
*pending* local posts awaiting confirmation. A member always talks to *their own*
network; the replica relays their post to the home (server-to-server, over the
existing bridge, fire-and-forget), the home mints it into the one total order and
fans it out one hop to every spoke. Reads are served from the replica with no
round-trip; a poster sees their own message instantly via an optimistic echo that
is reconciled when the authoritative copy returns. Same-network members (home ==
their network) mint locally exactly as today — zero added cost.

---

## 3. Roles and legs

```
   alice@F (member, spoke)          bob@H (member, home)          carol@G (member, spoke)
        │  MSG #ns/chan :hi              │  MSG #ns/chan :yo             │
        ▼                                ▼                              ▼
   ┌─────────┐    CHANNEL RELAY     ┌─────────┐    CHANNEL RELAY   ┌─────────┐
   │ F replica│ ──(no @id, @echo)──▶│  H home  │◀──(local, instant)│ G replica│
   │  (cache) │                     │  actor   │                   │  (cache) │
   └─────────┘                      │  MINTS   │                   └─────────┘
        ▲                           │  ULID    │                        ▲
        │      event mirror         │ (single  │      event mirror       │
        └────(H-origin, 1 hop,──────│  writer) │────H-origin, 1 hop)─────┘
             @echo back to F)       └─────────┘
```

- **Home actor** — the real channel actor on the namespace owner's network. Single
  writer, canonical log, fan-out. Home members post straight into it (no relay).
- **Replica actor** — on every spoke hosting members. Holds the mirrored canonical
  log (home-origin events) + a bounded pending tail. Serves all local reads. Never
  mints canonical ids.
- **Relay leg** (spoke → home): `CHANNEL RELAY … <sender@net> :body`, no `@id`,
  with an `@echo=<token>`. Fire-and-forget over the authenticated bridge.
- **Mirror leg** (home → all spokes): the ordinary event mirror. Because the home
  is now the origin, `H → G` is a legitimate **one hop from origin** — this is what
  fixes defect #1. The copy to the poster's own network carries `@echo` back.

The one-hop invariant is *preserved*, not weakened: the relay leg is a distinct
relay tunnel (like `GROUP RELAY`), not a mirror hop, so no event is ever two hops
from its origin.

---

## 4. Staying fast — the three mechanisms

Home-authority adds a round-trip **only to the finalization of a cross-network
post** — never to how fast a post *appears*, and never to reads. Three mechanisms
carry that:

### 4.1 Optimistic local echo → poster latency ≈ 0

When `alice@F` posts, the `F` replica *immediately* renders the message locally as
**provisional** (a client/replica-local entry with a provisional tail position and
no canonical msgid), and in parallel relays it to `H` with `@echo=<token>`. When
`H`'s minted event returns (matched by the token), the replica **reconciles**: it
swaps the provisional entry for the authoritative one — real msgid, canonical
position — and, if someone else's message interleaved ahead of it, the client
reorders within the small unconfirmed tail. Confirmed history never moves. The
poster never waits for the home; this is the `GROUP RELAY` echo-token pattern
(§11.12) applied to channels.

*Reorder visibility:* only unconfirmed tail entries can move, the window is bounded
by in-flight relays, and clients render pending messages distinctly (a "sending"
state) so settling into canonical position reads as expected UX, not a glitch.

### 4.2 Local-replica reads → reader latency ≈ 0

`HISTORY`, initial channel load, and scrollback are served **entirely from the
spoke's replica** — the mirrored canonical log — with no cross-network round-trip.
Only *posting* touches the home; *reading* is always local. The common case
(reading) is exactly as fast as today.

### 4.3 Pipelined, fire-and-forget relay → no head-of-line blocking

The relay leg is not an RPC. A spoke may have many posts in flight; the home mints
them in arrival order and acks each independently by echoing its token. Nothing
blocks on a prior message's confirmation, so throughput is bridge-bandwidth-bound,
not RTT-bound.

### 4.4 Home members pay nothing

When the poster's network *is* the home, there is no relay and no echo — the actor
mints locally and instantly, exactly as a non-federated channel does today. Since
channel traffic typically concentrates on the owning network, the fast path is the
overwhelming common case.

---

## 5. Availability — surviving a home outage

Home-authority makes the home a coordination point; this is the real cost, and the
design bounds it rather than denying it.

| Home state | Reads | Posts |
|---|---|---|
| **Reachable** | local replica, instant | optimistic echo, confirmed in ~1 RTT |
| **Unreachable** | **still fully available** from the replica | queued in a durable **outbox**; local optimistic echo stays visible, marked *pending*; a "home unreachable — messages will send" surface after a short grace |
| **Recovered** | — | spoke replays the outbox via `CHANNEL BACKFILL`; home mints in replay order and fans out; provisional entries reconcile |
| **Permanently gone** | replica frozen at last-mirrored state (correct — the owning network is gone) | read-only |

Key points:

- **Nothing is lost.** The outbox + `CHANNEL BACKFILL` is the `GROUP BACKFILL`
  recovery machinery (idempotent on msgid) generalized to channels.
- **Bounded buffering (invariant 6).** The pending outbox is capped; past the cap
  the spoke surfaces "degraded" rather than buffering unboundedly.
- **Reads never depend on the home.** A spoke serves its full mirrored history
  offline.
- **Order during an outage is eventually-consistent:** pending posts have no
  canonical position until the home mints them on recovery; the client shows them
  at the local tail in send order and reconciles when the home returns.

Compared with multi-origin, the trade is: multi-origin never blocks a post but also
*never establishes a shared order and cannot deliver >2-network channels*;
home-authority always establishes the order and delivers correctly, and degrades a
home outage to "posts pending, reads fine" rather than "posts lost."

---

## 6. Authority changes

- **Origin = home.** `msgid.origin` becomes the channel's home network for *every*
  message, whoever authored it. Invariant 2 restatement: a bridged channel event is
  accepted iff `msgid.origin == the channel's home` (which is the authenticated peer
  on the mirror leg). The **author** travels separately as `<sender@net>` on the
  relay leg (like `GROUP RELAY`/`GROUP MUT`).
- **Edit/delete/react authority shifts from origin to author.** Today the guard is
  `msgid.origin() == local network` (`relay.rs:439`). Under home-authority the home
  enforces **authored-by**: the relaying network vouches `sender@net`, and the home
  applies the mutation iff `sender` authored the target (or holds `delete-any` /
  moderation caps). Mutations from a spoke relay via `CHANNEL MUT` (mirrors
  `GROUP MUT`); the home applies and fans out. This also makes **moderation order
  consistent** — every edit, delete, and mod action is minted into the same total
  order by the single writer, aligning content authority with the control-plane
  authority the home already holds via `FSESSION` (§11.11).
- **Caps unchanged.** A federated author's right to post/edit is still checked
  against the home's grant store for `sender@net` (§11.11) — capability checks
  precede the mint.

---

## 7. Wire surface

**Client-facing verbs are unchanged.** A client still sends `MSG #ns/chan :body`,
`EDIT`, `DELETE`, `REACT`, `HISTORY #ns/chan`. The home-authority relay is entirely
server-side and invisible to clients — exactly as it is for group DMs.

**New federation-internal verbs** (bridge-session-only, server-emitted; a client can
never send them), mirroring the `GROUP_*` family:

| Verb | Syntax | Direction | Meaning |
|---|---|---|---|
| `CHANNEL RELAY` | `CHANNEL RELAY <#ns/chan> <sender@net> [@id=<msgid>] [@echo=<token>] [msg-meta] :body` | both | `@id` **absent** = spoke relayed a member's post to the home → home mints + fans out; `@id` **present** = home-minted message → spoke ingests + delivers locally (echo returned only to the poster's network). |
| `CHANNEL MUT` | `CHANNEL MUT <#ns/chan> <sender@net> <root-msgid> <op> [@id=<msgid>] [:arg]` | both | Mutation (`op` ∈ `edit`\|`delete`\|`react-add`\|`react-remove`). `@id` absent = spoke → home (relay to apply+mint into order); present = home-applied → spoke ingests. |
| `CHANNEL BACKFILL` | `CHANNEL BACKFILL <#ns/chan> [@after=<msgid>]` | spoke → home | Recovery pull after a home outage / reconnect: replay every channel event after the cursor (or all). Home answers with `CHANNEL RELAY` (`@id` present) ingests. Idempotent on msgid; a non-member/unmanifested network gets nothing (anti-enumeration, §11.1). |

Membership/roster for a channel is already carried by the manifest + `MEMBER`
mirror, so no `CHANNEL SYNC` analog to `GROUP SYNC` is required; the manifest is the
channel's membership authority.

---

## 8. What stays the same

- The client wire surface (`MSG`/`EDIT`/`DELETE`/`REACT`/`HISTORY`).
- **Same-network channels** (home == your network, the common case): no relay, no
  echo, instant local mint — byte-for-byte today's behavior.
- The event mirror shape, manifest gating (§11.1), retention-to-strictest (§11.4).
- The `FSESSION` control plane (§11.11) — home-authority now extends the *same*
  authority principle to content.

---

## 9. Implementation sketch (next phase — not yet built)

1. **`weft-proto`**: add `CHANNEL RELAY`/`MUT`/`BACKFILL` commands (parse + serialize
   + round-trip tests) — L0 first, per the layering rule.
2. **`weft-core` registry/actor**: give a channel a `home: NetworkName`. On a spoke,
   spawn a *replica* actor (mirror + pending tail) instead of a minting actor; on the
   home, the actor mints as today. Route `MSG` for a non-home channel through the
   relay path (mirror `relay.rs:330-369`'s group spoke→home branch).
3. **Mint + fan-out** on the home for relayed posts (mirror `groups.rs:398-450`
   `on_group_relay` + `directory.rs` `GroupIngest` no-remint).
4. **Echo tokens** for channels (reuse the `context.rs` group-echo TTL sweeper).
5. **Mutation authority**: change `relay.rs:439` from origin-check to a
   relay-to-home-with-authored-by path for non-home channels.
6. **Outbox + `CHANNEL BACKFILL`** for home-down durability (generalize
   `session/groups.rs` backfill).
7. **Tests**: three-network delivery (F, G both reach each other via H), total-order
   assertions, home-down → outbox → recovery, optimistic-echo reconciliation, and a
   two-live-weftd conformance run.

Client work is minimal: render the *pending/sending* state and reconcile on the
authoritative copy — the wire it speaks does not change.
