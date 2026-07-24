# Federation as a command/event stream — a layering that collapses the tunnels

**Status:** design (proposed; not yet implemented).
**Supersedes (if adopted):** the `FSESSION` sub-session frame, the `CHANNEL RELAY` /
`GROUP RELAY` / `CHANNEL MUT` / `GROUP MUT` / `CHANNEL BACKFILL` / `GROUP BACKFILL`
verb families, the per-message `echo` token, and the "friend-delivery conduit vs event
mirror" split.
**Companion:** `docs/architecture/home-authoritative-channels.md` (the ordering model this
keeps), `docs/protocol/weft-spec-v0.11.adoc` §11 (which this rewrites).

---

## 1. The problem

Federation today wraps a single idea in three layers:

1. an **`FSESSION OPEN/CMD` frame** carrying an inner line (a connection inside the
   connection);
2. a dedicated **relay verb** (`CHANNEL RELAY`, `GROUP RELAY`, …) instead of the actual
   command the user ran;
3. a per-message **`echo` token** instead of the ordinary `label`.

Each was added on top of the last as home-authority landed, and the result is two parallel
verb families (channel / group), two delivery mechanisms ("conduit" vs "event mirror"), and
a bespoke reconcile token — a lot for a second implementer to build and for the spec to
carry. None of it is load-bearing: underneath, it is just one server sending commands to
another and getting events back.

## 2. The model in one paragraph

A bridge is **one network-authenticated QUIC session** between two servers (`AUTH BRIDGE`,
network key — unchanged). Over it the two servers exchange exactly the two message kinds the
client protocol already has: **commands** (requests, one direction) and **events**
(everything that comes back, the async return). A server treats the bridge much like a
client connection, with two differences: it is authenticated at the *network* level, and the
peer may act on behalf of any of *its own* users by tagging a command `@as=<account>`. That
one tag replaces `FSESSION`; the ordinary `label` replaces `echo`; ordinary `MSG` / `EDIT` /
`HISTORY` replace the relay verbs; and an event's *audience* (who it fans out to) is the only
thing that still differs between a channel and a group.

## 3. The two layers

### 3.1 Commands go up

Two kinds cross the bridge, both plain §4 lines — no wrapper:

- **Network commands** — the network itself acts, no actor tag. The manifest lifecycle
  (`BRIDGE PROPOSE`/`ACCEPT`/`ADD`/`REMOVE`/`SEVER`/`REQUEST`), `REPORT-FORWARD`, and the
  data-plane `MIRROR`. Authorized by the network key / the §11.3 authority ladder.
- **User commands** — `@as=<account> <ordinary §6 command>`. The peer asserts its local user
  `account` is acting; the home reconstructs `account@<peer-network>` and enforces against
  **its own** grant store (homeserver authority, §11.11 — unchanged). This one form carries
  *everything* a federated user does:
  - content: `MSG` / `EDIT` / `DELETE` / `REACT`,
  - caps & roles: `GRANT` / `REVOKE` / `ROLE …` / `CAPS`,
  - moderation: `MUTE` / `BAN` / `KICK` / `REPORT` / `REPORTS …`,
  - channel & namespace admin: `CHANNEL …` / `NS …` / `INVITE …`,
  - social: `FRIEND …` / `CALL …` / `GROUP …`,
  - history: `HISTORY`.

`@as=` is trustworthy because the bridge proved the peer's network key: `F` vouches for
`alice@F`, exactly as `FSESSION` did. The backstop for a lying `F` is still `NETBLOCK`.

### 3.2 Events come back

- **Events are the only return type.** No command has a synchronous reply frame; every
  effect is an event. (This is the rule you asked for — events are how a server answers when
  no direct reply is needed.)
- **A labelled command is acked by the event carrying the same `label` back** — the §3.5 rule,
  extended to the bridge. `@label=L GRANT …` → `@label=L TOKEN …`; `@label=L MSG …` →
  `@label=L MESSAGE …`.
- **Receiver routing.** When a server sends `@as=alice @label=L <cmd>`, it remembers
  `L → alice's session`. A returning `@label=L` event is delivered to that session and the
  entry is cleared. This one small table replaces both the `fsid` bookkeeping *and* the echo
  map.
- **Fan-out.** An event may reach several peer networks at once (its *audience*, §4). The home
  emits it once; the bridge forwarder delivers a copy to each network in the audience.

## 4. The audience — the only channel-vs-group difference

An event's **audience** is the set of networks that should receive it:

- **Channel event** → the channel's manifest-sharers (peers with it in the acked manifest).
  This is exactly today's event mirror.
- **Group event** → the group's member networks (from the group roster).

Same forwarder, one different gate. And in both cases the `@label` (the ack) is put **only on
the copy to the origin network** — the one whose command produced the event; every other
network in the audience gets the event unlabelled. That is the whole reconcile mechanism: no
`echo` token, just "the label on the origin-network copy." A member (or another device) that
did not send the command has no matching label and simply displays the event.

## 5. Worked flows

### 5.1 A spoke member posts to a channel homed elsewhere

```
alice ─▶ S (her server)   @label=L MSG #ns/chan :hi           (client command)
S     ─▶ H (the home)      @as=alice @label=B MSG #ns/chan :hi (bridge command; S maps B→alice)
H                          mint msgid=H/<ULID>; store; audience = manifest-sharers
H     ─▶ every spoke       @msgid=H/<ULID> MESSAGE #ns/chan alice@S :hi   (event, home-origin)
H     ─▶ S (origin only)   …the same MESSAGE, but @label=B
S     ─▶ alice             @label=L MESSAGE …   (S saw B, delivers it as alice's labelled echo)
```

`alice`'s client reconciles her optimistic send by `label` — identical to a local post. There
is no `CHANNEL RELAY` and no `echo`: the up-leg is `MSG`, the down-leg is `MESSAGE`, and `B` is
an ordinary label `S` chose. A home-network member skips the bridge entirely.

### 5.2 Everything else is the same shape

- **Edit / delete / react:** `@as=alice EDIT <msgid> :fixed` → `EDITED` event to the audience.
  (No `CHANNEL MUT`.)
- **Admin:** `@as=alice GRANT bob ns:gaming send` → `@label=B TOKEN …` back to `S`. (No
  `FSESSION`.)
- **History / recovery:** `@as=alice HISTORY #ns/chan after=X` → `BATCH` events. (No
  `CHANNEL BACKFILL`.)
- **Groups:** `@as=alice MSG &<group> :hi` → the home mints and fans the `MESSAGE` out to the
  **member networks** instead of manifest-sharers. Membership itself is an event the home
  emits to members (a `GROUP` / `GROUP-MEMBER` roster event — the down-leg of what was
  `GROUP SYNC`).

## 6. Home-authoritative minting — kept, re-triggered

The ordering model (`docs/architecture/home-authoritative-channels.md`) is unchanged: the
channel's home is the sole ULID writer and the sole origin. The *only* change is what
triggers a home mint for a foreign member — an `@as=alice MSG` bridge command instead of a
`CHANNEL RELAY`. The home mints for `alice@F` after a cap check (`send`/`view`), with no
requirement that `alice` hold a local membership — same as `relay_publish` does now.

## 7. What the federation reference becomes

The two-reference structure we just built collapses into something an implementer can hold in
their head:

- **Client↔server** = §6 commands + §7 events (unchanged).
- **Home↔spoke** = *the same §6 commands and §7 events*, plus:
  - the `@as=<account>` attribution tag on a user command,
  - a small set of **network commands** (`BRIDGE …`, `REPORT-FORWARD`, `MIRROR`) and
    **handshake/state events** (`CHALLENGE`, `WELCOME`, `MANIFEST`, `NETBLOCKED`),
  - the rule that an event fans out to an *audience* (manifest-sharers or group members),
    labelled only on the origin-network copy.

So the federation reference is "the client surface, run by one server on behalf of its users,
plus bridge control" — not a parallel verb catalog.

## 8. Invariants — all preserved

This is a *structural* change (framing + transport), not a semantic one. Each invariant holds
by the same argument as today:

- **Homeserver authority (§11.11):** `@as=alice` is enforced against `H`'s grants for
  `alice@F`; operator/owner power stays local-only.
- **IP non-exposure (MUST):** still server-to-server; `@as` carries an account, never an
  address. A user never connects to `H`.
- **Origin authority / no transitivity (invariant 2, §11.4):** the home mints; `msgid.origin`
  = home; events fan out home-origin, one hop. A bridge command is one hop (spoke→home).
- **Network-key trust:** `F` vouches for `@as` via its authenticated key; `NETBLOCK` is the
  backstop.
- **Manifest gating (invariant 3):** a channel event's audience *is* the acked manifest set;
  forwarding outside it remains a violation.

## 9. Cost & migration

- **It is a wire change.** `FSESSION` / `CHANNEL RELAY` / `GROUP RELAY` / `CHANNEL MUT` /
  `GROUP MUT` / `CHANNEL BACKFILL` / `GROUP BACKFILL` / the `echo` tag are all removed;
  `@as=` and the audience-fan-out are added. It is **not** backward-compatible with the
  current federation wire — but federation has no external deployments yet (reference
  implementation), so a clean break is the right call.
- **Code shape.** The bridge session becomes a thin adapter that (a) reads a line, (b) if it
  carries `@as=`, runs it through the *same* command dispatch a client uses, in an
  `account@peer` actor context, and (c) forwards emitted events to the audience with the label
  on the origin copy. Most of `weft-core/src/session/federation.rs`'s bespoke handlers
  (`on_channel_relay`, `on_group_relay`, `on_*_mut`, `*_backfill`, the echo map, the
  per-peer nonce injection) delete; the channel/group content actors keep minting but are
  driven by ordinary `Publish`/`Edit`/… in the tunnelled actor context.
- **The client is unaffected** — it already speaks `MSG` + `label` and reconciles by label.

## 10. Open questions (decide before coding)

1. **Tag name** for the actor: `@as=<account>` vs `@on-behalf=<account>`. (`as` is short and
   clear.)
2. **Stateless `@as` vs a session concept.** Proposal: purely stateless — every user command
   carries `@as`; no `OPEN`/`CLOSE`, no `fsid`. Confirm nothing needs per-user bridge state.
3. **Membership propagation for groups.** Model it as a down-leg event (`GROUP` roster event
   to member networks); a channel's equivalent is `MANIFEST`. Confirm the group roster event's
   shape.
4. **Backfill.** Fold `CHANNEL`/`GROUP BACKFILL` fully into `@as HISTORY`, or keep a thin
   server-driven "replay after reconnect" that also emits ordinary events. Proposal: fold into
   `HISTORY`.
5. **Label scope.** The receiver's `label→session` table needs labels unique per bridge; the
   sending server owns that (it mints `B`). Confirm dedup window / eviction (mirror the §9.2
   `(session,label)` 5-minute rule).
6. **Voice.** `VOICE REQUEST` → keep as a network/`@as` command; the audio itself stays on the
   separate media plane (unchanged).

## 11. Why this is the right end state

The convolution came from *adding* mechanisms (a relay verb, an echo token, a conduit) onto a
bridge that could have been a command/event channel from the start. This model removes them
instead: one authenticated stream, commands up (a user's tagged with `@as`), events down (the
ack is the labelled event), and one audience gate that is the sole channel-vs-group
difference. It is smaller to implement a second server against, and §11 shrinks from a catalog
of tunnels to "the client surface, federated."
