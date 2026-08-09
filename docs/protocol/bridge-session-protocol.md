# The bridge (provider) session — wire reference

Everything a **provider** (an App Service: a bridge adapter, or a plugin that owns namespaces) says
to weftd and hears back. This is the contract `weft-appservice` implements and the Matrix daemon is
written against.

Unlike its neighbours `weft-protocol-flows.md` and `weft-federation-flows.md`, which are conceptual
maps, **this is a wire reference**: exact verbs, tags and failure modes.

Scope: **provider sessions only** — none of it is client↔server or peer↔peer protocol, which is why
it is a document of its own rather than part of `weft-spec-v0.13.adoc`. Design rationale for each
rule lives in [`docs/architecture/foreign-bridge-framework.md`](../architecture/foreign-bridge-framework.md);
this file is the mechanical contract.

> **Reading the tables.** `→` is provider→weftd, `←` is weftd→provider. Tags are one `@`-prefixed
> group separated by `;` — `@as=…;msgid=… MSG …`, **not** `@as=… @msgid=… MSG …` (the second `@`
> parses as the verb). `=` inside a tag *value* is fine.

---

## 1. The two governing rules

Everything below follows from two decisions, and they explain most of the asymmetries:

**A realm is a network.** A bridged realm is modeled as a WEFT network, not as a set of local puppet
accounts. Its users are `alice@matrix.org`; it *mints its own msgids* under `matrix.org`; weftd
ingests them exactly as it ingests a peer network's — which is what makes the ordinary federation
machinery apply unchanged.

**Amended 2026-08-08:** in a *replica* channel the realm mints **everything**, including a local
member's own post (§8) — the foreign system is that room's source of truth, so weftd keeps no copy of
its own to diverge. Replica messages are therefore single-origin (the realm's); only a **projected**
namespace, where weftd genuinely is the home, mints under our origin.

**A bridge behaves as a federation peer.** So commands travel *to* the authority and events come
*from* it. weftd relays `NS JOIN` as a **request**; the realm answers with `NS-MEMBER` as a
**statement**. weftd never asserts membership of a foreign space, and a provider never mints an event
under our origin. The two directions are not interchangeable.

---

## 2. Handshake

| Dir | Line                                          | Notes                                                            |
|-----|-----------------------------------------------|------------------------------------------------------------------|
| →   | `HELLO weft/1`                                |                                                                  |
| ←   | `WELCOME <network>`                           |                                                                  |
| →   | `AUTH ADAPTER <pubkey-b64>`                   | The key must be pinned in weftd's `[[plugin.remote]]` config.    |
| ←   | `CHALLENGE <nonce-b64>`                       |                                                                  |
| →   | `AUTH PROOF <sig-b64>`                        | Signs `nonce ‖ network-name` (§6.1 — anti cross-network replay). |
| ←   | `WELCOME <network>` with `plugin` in features | Session is now `State::PluginService`, realm-unbound.            |

Failure is uniform `AUTH-FAILED`. After this the session is a provider session: the client verb set
does **not** apply, and only what follows is routed.

---

## 3. Registration and realm binding

| Dir | Line                                           | Notes                                                                                                                                                     |
|-----|------------------------------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------|
| →   | `PLUGIN-REGISTER :<b64-CBOR Registration>` | Actions, hooks, and the `schemes` this provider serves. In the trailing, not a tag: §4 caps a tag value at 1024 B and a catalog passes that immediately.                                                                                                   |
| →   | `REALM REGISTER <scheme>`                      | Control link: claims a scheme without binding a realm.                                                                                                    |
| →   | `REALM ASSERT <scheme>://<realm>`              | Data connection: binds this session to one realm.                                                                                                         |
| →   | `REALM WITHDRAW`                               | **Deletion**, not disconnect: cascades away the realm's namespaces and tombstones them to members. Disconnecting instead just marks the provider offline. |

`PLUGIN-REGISTER` **fails loudly** — a malformed or unauthorized registration answers a typed `ERR`
and the connection is closed, rather than a silently inert session.

A scheme is held by its first registrant until that provider disconnects; a second claimant gets
`ERR CONFLICT`.

### Which realms may be claimed

`REALM ASSERT` refuses (`ERR FORBIDDEN` with a context naming the reason):

| Context              | Meaning                                                                                                                                                         |
|----------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `own-network`        | The realm equals our own network name. Would let the provider act as **local accounts** — a user on our own network has the *bare account* as their member key. |
| `peer-network`       | We hold a peer record for that name.                                                                                                                            |
| `netblocked`         | Invariant 7 is name-keyed, so a block bites a realm exactly as it bites a peer.                                                                                 |
| `not-a-network-name` | Not a valid `NetworkName`; it could never mint valid users.                                                                                                     |
| `domain-runs-weft`   | The domain publishes `/.well-known/weft` — **its owner chose WEFT**, so no bridge may claim it.                                                                 |

Only a *positive* well-known answer refuses. An unreachable domain, NXDOMAIN, or a realm that is not
a domain at all (a Discord guild id) all still bind — otherwise a DNS blip would lock out every
legitimate bridge.

---

## 4. Asserting structure

The provider states what exists in its realm; weftd mints the WEFT-side ids and replies with the
mapping. **Keep the reply** — it is the only way to address these objects afterwards.

| Dir | Line                                                                                            |
|-----|-------------------------------------------------------------------------------------------------|
| →   | `@id=<ns-ulid>;authority=…;settings=…;title=…;… NS-META <scheme>://<realm>/<space> <visibility>` |
| ←   | `NS-META <ns-id> …` — acknowledging the id you supplied                                         |
| →   | `@id=<chan-ulid>;vanity=…;category=…;kind=… CHANNEL-LAYOUT <…>/<space>/<room> <position>`       |
| ←   | `CHANNEL-LAYOUT #<ns-id>/<chan-id> …` — the canonical name, built from your two ids             |

`<visibility>` is `public`, `unlisted` or `private`.

**Re-asserting is how you update.** weftd refuses local edits to a namespace you govern — `NS META`,
`NS VISIBILITY`, `CHANNEL CREATE`, for everyone including operators — so a re-assertion is the one
path that changes it, and absent fields **clear**: an assertion is the whole truth, not a patch.
(`NS DELETE` stays available to operators; that is the garbage-collection path for a realm that never
comes back.)

**The capability profile** (§7a.3) rides the same assertion, and says how a client should present
this namespace:

| Tag | Meaning |
|-----|---------|
| `authority=roles\|levels\|none` | How authority is *rendered*. `levels` (Matrix) hides the native roles editor; the plugin supplies its own Power Levels surface instead. Absent ⇒ `roles`. |
| `settings=<comma-list>` | Native settings surfaces to hide: `roles`, `permissions`, `channels`, `invites`, `moderation`, `ns-edit`, `recovery`. |

Both are **display gating only** — they grant nothing and enforce nothing. They persist on the
namespace, so a member who joins later sees them too.

Inside a replica, **authority is whatever you grant** (§7). A plain member holds nothing — the
sentinel owner confers none — and a network **operator** holds nothing over the wire either: operator
power lives in a separate table and acts through the web admin panel, never as wire capability inside
a namespace. So the surfaces you disable are ones only your own appointees could have reached.

Structure is the exception in the other direction: `NS META`, `NS VISIBILITY` and `CHANNEL CREATE` are
refused even for an `ns-admin` you appointed, because that is yours to describe (see above).

Setting a level is **not** a wire verb. The client sends numbers as params of the plugin's own action
(`PLUGIN INVOKE`), the adapter decides what the number means in both systems, and it mirrors the
resulting capabilities back as a `GRANT` (§7). Translation belongs where the pinned key is: a
client-side mapping would be an authority decision computed somewhere anyone can rewrite.

**The realm mints the ids; weftd pins them.** Federation never re-mints a peer's ULIDs — a signed
manifest names `<network>/<ns-id>` and `provision_replica` takes the channel name verbatim — and a
bridge is no different. Three consequences:

- The ids **survive our store**. Re-asserting after a weftd restore, or provisioning the same space on
  another server, reproduces the same namespace and channels (mint them deterministically from the
  foreign id if you want that).
- You can address a channel **without waiting for the reply**: you already know
  `#<ns-id>/<chan-id>`, so the whole startup burst pipelines.
- An id already in use answers `ERR CONFLICT` with context `id` rather than being adopted — otherwise
  a provider could assert a native namespace's ULID and take it over.

Both are the ordinary verbs with an **origin URI** as the target instead of an id; weftd routes on
the `://`. The channel's parent namespace is the URI minus its last segment, so the namespace must be
asserted first.

Everything afterwards addresses channels by the canonical `#<ns-id>/<chan-id>` — there is no
URI-target form on the traffic path.

### On-demand provisioning

| Dir | Line                                         | Notes                                               |
|-----|----------------------------------------------|-----------------------------------------------------|
| ←   | `PROVISION <scheme>://<realm>/<path> <job>`  | A user asked to join a space we don't know.         |
| →   | `PROVISION-OK <job>` / `PROVISION-ERR <job>` | Answer after asserting the structure, or to refuse. |

The waiting client is parked on `job`; a provider that dies with jobs outstanding fails them
(`NO-SUCH-TARGET`) rather than hanging the client.

---

## 5. Ingestion — replaying the realm's traffic

Every ingested line carries **`@as=<user@domain>`**: the provider acting *on behalf of* a foreign
user. The sender must be **foreign** — weftd refuses a local account and any known WEFT peer's user,
whose identities are anchored by our auth and their signing keys respectively. It need *not* live on
the bound realm itself (amended 2026-08-05): foreign systems are cross-realm — a Matrix room homed
on matrix.org has members from kde.org — and the trust root is the provider's pinned key + the
channel's scheme, not the sender's domain. Netblocks bite on **both** ends: the channel's realm and
the sender's own network, so blocking a homeserver silences its users everywhere.

**The provider mints.** `MSG` and `EDIT` — the two verbs whose stored row is keyed by its own id —
carry `@msgid=<realm>/<ULID>`; weftd never mints for a foreign origin (invariant 2). `DELETE` and
`REACT` name only the root they act on and get a local bookkeeping id, exactly as on the peer path.

| Dir | Line                                                                   |
|-----|------------------------------------------------------------------------|
| →   | `@as=<user@realm>;msgid=<realm>/<ULID> MSG #<ns-id>/<chan-id> :<body>` |
| →   | `@as=<user@realm>;msgid=<realm>/<ULID> EDIT <root-msgid> :<body>`      |
| →   | `@as=<user@realm> DELETE <root-msgid>`                                 |
| →   | `@as=<user@realm> REACT <root-msgid> <emoji>`                          |
| →   | `@as=<user@realm> UNREACT <root-msgid> <emoji>`                        |

A `@msgid` outside the realm, an unknown channel, a channel that is not an `origin`-marked replica of
a scheme this key is pinned for, or a netblocked realm all **drop the line** (or answer `ERR
UNSUPPORTED` where a caller is waiting). Nothing is minted on a bad line.

### Moderation by a foreign moderator

| Dir | Line                                                                                |
|-----|-------------------------------------------------------------------------------------|
| →   | `@as=<user@realm> MUTE <scope> <account> [:reason]` (also `UNMUTE`, `BAN`, `UNBAN`) |
| →   | `@as=<user@realm> KICK #<ns-id>/<chan-id> <account> [:reason]`                      |

These run through weftd's **ordinary** actor-aware moderation path as `Actor::Foreign`, checked
against the grants the provider itself issued (§7). A foreign user with no grant is refused exactly
like a local one — being foreign confers nothing.

### DMs

**Routed by domain, like a peer** (2026-08-09). weftd knows a bridged realm by its
name, the way it knows a federated network by its own: a DM to `@user@<realm>` goes to
the provider serving `<realm>`, resolved from the realm the provider **asserted** and —
so it survives a disconnect — from any replica namespace whose `origin` names that
realm. That second source is what makes an offline bridge distinguishable from an
unknown network, and so:

- **provider connected** → relayed as below;
- **realm known, provider gone** → `ERR POLICY` (context `provider-offline`), the same
  refusal as posting into one of that realm's channels. It is *not* stored: a DM filed
  locally with no route looks exactly like a delivered one;
- **realm unknown** → the federation path takes its turn (it may be a WEFT peer, and
  the social layer will dial one).


| Dir | Line                                                                 |
|-----|----------------------------------------------------------------------|
| →   | `@as=<user@realm>;msgid=<realm>/<ULID> MSG @<local-account> :<body>` |

Stored in the ordinary DM scope keyed by member keys, preserving the realm's msgid. A bridged
conversation is a first-class DM, not a second table.

The other direction is already wired: a local user's `MSG @<user@realm>` is stored and echoed
locally **and** relayed to the realm's provider as `@as=<local-user>;ulid=<id> MSG @<user@realm>`
(§8) — the only route that can reach them.

### Outbound projection — the return path into a native channel (2026-08-06)

A **native** namespace whose ns-admin opted in — `NS META <ns-id> bridge:<scheme> :open` (requires
`public`; echoed as `bridges=` on NS-META; every projection closes when visibility leaves `public`)
— accepts the scheme's provider on three surfaces:

| Dir | Line                                                    | Notes                                                        |
|-----|---------------------------------------------------------|--------------------------------------------------------------|
| ←   | `MESSAGE`/`EDITED`/`DELETED`/`REACTION` (local-origin)  | The provider is subscribed to the namespace's channels, exactly like a replica's — mirror them outward. |
| →   | `@as=<user@domain>;label=<l> MSG #<ns-id>/<chan-id> :…` | **No `@msgid` — the home mints** (a carried id is refused). |
| ←   | the minted `MESSAGE`, tagged `label=<l>`                | The §3.5 echo **is the ack** — how the adapter learns the minted id. |

Mutations (`EDIT`/`DELETE`/`REACT`/`UNREACT`) name the home-minted root; EDIT/DELETE require
authorship (a foreign moderator's delete rides the authority mapping, not this path). A local `@as`
is always refused here — locals act natively, so there is no relay to confirm. The flag is the whole
authorization anchor: no flag, no injection, and `NO-SUCH-TARGET`-uniform refusals reveal nothing.

At registration/`REALM ASSERT` weftd also **pushes the projected structure** — `NS-META` (with
`bridges=`), then each channel's `CHANNEL-LAYOUT` + `POLICY` — the same events the provider speaks
inbound for a replica, roles swapped; the adapter needs the policy to apply the projection rules
(`permanent`-only, no e2ee, no voice). Membership runs §8 in the outbound sense: the provider states
**foreign** members of a projected namespace (`NS-MEMBER <ns-id> <user@domain> join|part`) as its
users join the projected rooms; a *local* member statement is refused — locals join natively.

Every relayed event copy whose actor is local carries `ulid=` (as on the §6/§8 relays): key puppets
by it, never by the mutable account name.

A channel **created after** that push is handed over immediately — its `CHANNEL-LAYOUT` + `POLICY`
arrive unprompted, and weftd waits (bounded, 2 s) for the provider to start watching it before the
creator's ack. That wait is not politeness: a channel actor's broadcast has no replay, so an ack that
outran the subscription would silently drop the room's first messages.

Note: a **flag** flipped mid-session is still picked up on reconnect — the sweep runs at
registration/`REALM ASSERT` (§10's recovery story).

---

## 6. Membership — the realm is the authority

Membership is **namespace-level; channels are not joinable.** Having access to a namespace *is*
access to its channels, so putting a user into the foreign rooms is the adapter's job — weftd never
enumerates rooms for it.

| Dir | Line                                          | Meaning                                                |
|-----|-----------------------------------------------|--------------------------------------------------------|
| ←   | `@as=<local-user>;ulid=<id> NS JOIN <ns-id>`  | A **request**: one of our users asks to join.          |
| ←   | `@as=<local-user>;ulid=<id> NS LEAVE <ns-id>` | A request to leave.                                    |
| →   | `NS-MEMBER <ns-id> <user> join` (or `part`) | A **statement**: the authority saying who is a member. |

The inbound statement is what writes the membership row — for the realm's own users *and for ours*,
once the foreign side actually has them. It is keyed `user@realm` for a foreign member and the bare
account for a local one.

### Resync (full replace)

A realm corrects drift by **re-stating**, in the same snapshot framing a client gets on login (§6.9):

| Dir | Line                                                 |
|-----|------------------------------------------------------|
| →   | `SYNC START`                                         |
| →   | `NS-MEMBER <ns-id> <user> join` … (the complete set) |
| →   | `@cursor=<opaque> SYNC END`                          |

At `SYNC END`, every member of the namespaces this provider governs whom it did **not** name is
dropped. `SYNC START` is the safety: an unopened `SYNC END` names nobody, so it is ignored rather
than obeyed — otherwise it would wipe the namespace. `@cursor` is required by the codec and opaque to
weftd; put anything stable in it.

Full-replace beats diffing because the adapter already holds the whole set (a Matrix adapter reads
room state), there is no read-modify-write across the link and so no stale-read race, and replaying
it after any gap is idempotent.

---

## 7. Authority — capabilities here, power levels there

Authority crosses in **both** directions, and weftd carries no notion of a power level: it speaks
capabilities and the adapter owns the mapping (decide that 50 means `mute`+`ban`+`delete-any`; weftd
never learns the number).

| Dir | Line                                        | Meaning                                            |                                       |
|-----|---------------------------------------------|----------------------------------------------------|---------------------------------------|
| →   | `GRANT <user@realm\                         | local-account> ns:<ns-id> <caps>`                  | A foreign moderator becomes one here. |
| →   | `REVOKE <subject> ns:<ns-id> [caps=<list>]` | …and stops being one.                              |                                       |
| ←   | `GRANT <subject> ns:<ns-id> <caps>`         | A WEFT moderator was promoted — raise their level. |                                       |
| ←   | `REVOKE <subject> ns:<ns-id> [caps=<list>]` | …demoted.                                          |                                       |

Inbound authority is the ingestion rule: the scope must name a namespace whose scheme this key is
pinned for. No capability chain is consulted — for a provider-managed namespace the **provider is
the governing authority**, as an owner is for a native one.

**Attributed authority (§10, 2026-08-06).** A `GRANT`/`REVOKE` carrying `@as=<user@domain>` is a
*foreign moderator's* act, not the provider's: it runs the ordinary handler as `Actor::Foreign` and
succeeds **iff WEFT granted that user** `grant:<cap>`. Same for `@as` `BAN`/`UNBAN`/`KICK`, and for
`@as DELETE` of another author's message (needs `delete-any`). So a foreign admin holds exactly what
some WEFT grant gave their handle — the bridge translates power, it does not confer it. The two
inbound forms differ deliberately:

| Form | Authority | Use |
|---|---|---|
| bare `GRANT` (no `@as`) | the provider as governing authority of **its own replicas** | mirroring the realm's own role/level state |
| `@as=<user@domain> GRANT` | that user's WEFT grants (`Actor::Foreign`) | a foreign moderator's PL change, in a replica **or** a projected namespace |

Outbound relays carry `ulid=` when the **subject** is one of our accounts, so an adapter can address
their ULID-keyed puppet without waiting for them to post first.

**Role assignment relays too.** A WEFT role is a labelled bundle that *materializes into grants*
(`ROLE ASSIGN` records the membership, then grants the role's caps at that scope), so promoting
someone to a role in a replica namespace emits the outbound `GRANT` above and raises their foreign
level. Only **`@everyone`** does not: it is resolved live at check time and never becomes a grant, and
relaying a baseline every member holds would mean "give everyone level 50".

### Two shapes of authority, because foreign systems differ

| Foreign model | What the provider sends | What the client shows |
|---------------|-------------------------|-----------------------|
| **Levels** (Matrix) | bare `GRANT`/`REVOKE` | `authority=levels` — the native roles screen is disabled and the plugin supplies a **Power Levels** settings surface (§7a.3–§7a.4) |
| **Roles** (Discord) | the ordinary `ROLE CREATE` / `ROLE ASSIGN` / `ROLE UNASSIGN` verbs | real WEFT roles — pills, the roles editor, the lot |

A provider speaks the role verbs as **`Actor::Provider`**: the governing authority of the namespaces
it bridges, the way an owner is of a native one, and bounded by the scheme its key is pinned for — a
scope outside its own realms answers `ERR CAP-REQUIRED`. This is the bridge's analogue of
`Actor::Foreign`, so grants, roles and moderation all follow one rule instead of a bypass per verb.

Role ids follow the same minting rule as everything else, and `ROLE ASSIGN` names the **id**.

---

## 8. Outbound traffic — what weftd sends the provider

### Relayed local events

The provider is subscribed to every replica channel of its namespaces and receives the ordinary
event lines:

```
← MESSAGE #<ns-id>/<chan-id> <user@ournet> :<body>
← EDITED / DELETED / REACTION …
```

**Only events this network minted cross the link** (`msgid.origin == our network`), which is the same
one-hop rule as a peer bridge. Because a replica is multi-origin, an event we ingested carries the
realm's origin and is structurally ineligible to go back — no ping-pong.

In a **replica** channel that now leaves only the projection direction and the bookkeeping verbs:
since weftd no longer mints a local user's post there (next section), there is no `MESSAGE` of ours
to relay. A **projected** namespace (native, `bridge:<scheme> :open`) still works exactly as above —
weftd is the home there, so it mints and relays its own events.

System messages (join/part notices) are local channel noise and are not relayed. `MEMBER` is relayed
only when the user is on our network.

### Acting on behalf of a local user

A local user cannot mutate a message the *realm* minted — that would be authoring under someone
else's origin. weftd asks the provider to do it instead, and the resulting foreign event returns
through ordinary ingestion:

```
← @as=<local-user>;ulid=<id> REACT <realm-msgid> <emoji>
← @as=<local-user>;ulid=<id> DELETE <realm-msgid>
← @as=<local-user>;ulid=<id> EDIT <realm-msgid> :<body>
← @as=<local-user>;ulid=<id> MSG @<user@realm> :<body>   (a DM to one of the realm's users)
```

### Posting into a replica channel (amended 2026-08-08)

**The realm is the source of truth in its own channels**, so a local member's post is relayed the same
way — weftd mints nothing, stores nothing, and does not echo:

```
← @as=<local-user>;ulid=<id>;label=B-<scheme>-<ulid> MSG #<ns-id>/<chan-id> :<body>
```

The adapter MUST puppet it into the foreign room, mint `<realm>/<ulid>` from the resulting foreign
event id, and hand the message back **quoting that label**:

```
→ @as=<local-user>;msgid=<realm>/<ulid>;label=B-<scheme>-<ulid> MSG #<ns-id>/<chan-id> :<body>
```

The label is what makes the returning copy the poster's own: weftd routes it to the session waiting on
that label, so their client reconciles the message it sent instead of seeing a stranger's. **Drop the
label and the author sees their own message as somebody else's** — and, because a labelled `MSG` is the
only way `@as` may name a local account (below), an unlabelled one is refused outright.

An adapter that cannot deliver MUST say so — and for a relayed post it says so **on
the label**, because there is no msgid to name:

```
→ @label=B-<scheme>-<ulid> UNDELIVERED :<reason>
```

weftd answers the waiting session with `ERR POLICY` (context `not-delivered`) carrying
the *poster's own* label, so their client fails the pending message immediately with
the realm's reason, instead of shimmering until its own send deadline. The label is
also the authorization: one weftd did not issue (or that has expired) is ignored
rather than failing a message it does not own. `UNDELIVERED <msgid> :<reason>` keeps
its meaning for the **projection** path, where weftd did mint the message. While the provider is **offline** weftd refuses
the post at send time (`ERR POLICY`, context `provider-offline`) rather than queueing it — there is no
outbox for a room we do not own.

**`ulid=` is mandatory on every line an adapter must *act as* somebody for** —
including the DM relay, which omitted it until 2026-08-09 and was therefore dropped
on arrival: relayed on weftd's side, discarded on the adapter's, with a log line on
each side that individually looked fine.

**`ulid=` names the actor's stable identity** (added 2026-08-06): account names are mutable vanity
labels, so an adapter MUST key its puppets and per-user state by the ULID, never by the name — a
name-keyed puppet is orphaned by a rename. The name still rides in `@as` for attribution.

There is no local ack — the ingested event *is* the result. Ordinary authorization runs **before** the
relay: `EDIT` still requires authorship, `DELETE` authorship or `delete-any`.

So `@as` reads the same in both directions — *on behalf of* — naming a foreign user inbound and a
local user outbound.

**Closing the loop:** the confirmation comes back as ordinary ingestion (§5) **attributed to the
local user** — `@as=<local-user> REACT <realm-msgid> <emoji>` — the one shape of local `@as`
ingestion accepts (amended 2026-08-05). It is bounded to exactly the class weftd relays: the
mutation verbs, on a root the realm itself minted, plus (amended 2026-08-08) a `MSG` carrying a bridge
label weftd issued. Touching a **local-origin** root remains a refused forgery, as does an unlabelled
`MSG` attributed to one of our accounts. Practically: the adapter sees its own puppet's echo, maps the
puppet back to the local user, and re-ingests under that name.

### Replies (§9.3, added 2026-08-09)

A reply is a pointer, and each side spells it differently — so the **link table is
the translation**, in both directions:

| Dir | WEFT | Matrix |
|-----|------|--------|
| →   | `reply-to=<msgid>` on the relayed `MSG`/`MESSAGE` | `m.relates_to.m.in_reply_to.event_id` on the sent event |
| ←   | `reply-to=<msgid>` on the ingested `MSG` | the same relation, resolved through the link map |

Two rules an adapter has to get right:

- **Read the relation, not `event_id`.** An edit (`rel_type: m.replace`) also lives
  under `m.relates_to` and also carries `event_id`, so reading that field directly
  makes every edit look like a reply to the message it edits.
- **Strip the quoted fallback inbound, and never generate one outbound.** Matrix
  historically prepends a `> `-quoted copy of the original to the body; a WEFT
  client renders the root itself, so keeping it quotes every reply twice. Going the
  other way, a WEFT body is authored text — prepending a quote would put words in
  the author's message. (MSC2781 deprecated the fallback; current clients render
  the relation.)

An **unlinked** root — never bridged, or bridged before a data loss — sends as a
plain message rather than being dropped: losing the thread pointer is a smaller
wrong than losing the message.

### Backfill

```
← HISTORY #<ns-id>/<chan-id> before=<msgid> limit=<n>
```

Sent when a local client scrolls past what we hold. **Answer by replaying the window as ordinary
ingestion** (§5) — there is no separate backfill ingress. Demand-driven and deduped per
`(channel, before)`; never an eager pull of a whole foreign scrollback.

---

## 9. Liveness

A provider-managed namespace is **online only while its provider is**. On disconnect its namespaces
leave `DISCOVER`, `NS JOIN` is refused (uniform `NO-SUCH-TARGET`), and members get a live `NS-META`
with `provider=offline`; reconnecting reverses all of it.

While offline, weftd **refuses every write** into the realm's channels — posts *and*
`EDIT`/`DELETE`/`REACT` — with `ERR POLICY provider-offline`. The foreign side is authoritative for
its own rooms, so accepting a write we cannot deliver would leave local members looking at state the
realm never agreed to, with nothing to reconcile against later.

Operator/admin delete (the admin panel) is deliberately **not** gated: it is the moderation and
legal-removal path and must work with the bridge down.

---

## 9a. Liveness is probed, not inferred (2026-08-09)

A bridge is legitimately silent whenever its realm is, so silence cannot mean
failure — but weftd advertises this provider's namespaces as **online** purely
because the session exists, and an open socket with a dead or wedged adapter behind
it makes weftd claim something it cannot support.

So weftd asks. After **5 s** of quiet it sends `PING <token>`; a session that has
produced *nothing at all* by **10 s** is closed, which takes its namespaces offline
through the ordinary disconnect path (`provider=offline` in `NS-META` to members).

**Answering is mandatory** — `PONG` to any `PING`, which the SDK does for you. An
adapter that ignores it will be reaped even while connected, and rightly: from
weftd's side that is indistinguishable from an adapter that has stopped working. The
SDK's own ~10 s keepalive means the probe rarely fires at all; it exists for the case
where the adapter's own loop has stopped.

## 10. Failure modes worth designing for

| Situation                                    | What weftd does                                                         | What the adapter should do                                           |
|----------------------------------------------|-------------------------------------------------------------------------|----------------------------------------------------------------------|
| Provider disconnects                         | Namespaces go offline; parked provisions fail; outstanding invokes fail | Reconnect and re-assert; re-state membership with `SYNC START`/`END` |
| Provider dies mid-provision                  | The waiting client gets `NO-SUCH-TARGET`                                | Nothing — the client retries                                         |
| Realm netblocked mid-session                 | Ingestion stops at once; a fresh `REALM ASSERT` is refused              | Stop; the block is deliberate                                        |
| Provider queue full                          | The line is dropped with a warning                                      | Keep the session drained; weftd does not block on you                |
| Membership statement lost (provider offline) | Nothing is queued                                                       | Re-state on reconnect — that is what the full-replace window is for  |
| Local `NS LEAVE` while the provider is down  | Applied anyway; stated back on your next registration (see below)       | Reconcile: whoever is joined foreign-side and absent has left        |

### 10.0 Re-asserting a room restates it, for everyone

A re-assert is not just idempotent bookkeeping: it may carry a **new** display name,
category or position. weftd adopts the change and announces the layout to the
channel's **members**, not only back to the provider that asked — answering the
asking session alone corrected weftd's store while every connected client kept the
name it had cached (a bare id, until the user restarted their client).

### 10.1 The membership statement weftd sends *you*

The full-replace window is one direction. It cannot carry the other, because weftd applies an
`NS LEAVE` whether or not you are connected, and its pushes are live-only — so a leave during your
downtime never reaches you, and the foreign side keeps a member we no longer have. You cannot ask
either: you hold a **key, not an account**, so the cap-gated `NS INFO MEMBERS` is closed to you.

So on `REALM REGISTER` / `REALM ASSERT`, weftd states its **local** membership of every namespace your
schemes govern, framed as the same `ni…` roster BATCH the verb produces:

```
← BATCH START ni7
← NS-MEMBER-INFO <ns-id> <user@network> …      (one per local member, any governed namespace)
← BATCH END ni7
```

Three properties the framing exists for:

- **One batch spans every governed namespace**, and each row names its own namespace. Per-namespace
  batches cannot express "this namespace has no local members left" — that batch would contain
  nothing that says which namespace it was about, so the one namespace most needing reconciliation
  is the one you could not identify.
- **`BATCH END` means the statement is whole.** An absent namespace is honestly empty, not unknown.
- **Local members only.** You are authoritative for your own realm's users and already know them —
  they are what you re-state back to us.

Reconcile by difference: anyone joined foreign-side whose account is absent has left. Touch only your
own puppets — a foreign member of the space is not yours to remove, and your bot must stay to keep
reading it.

---

## 10.2 Routing a line: direction is never inferred from decode success (2026-08-09, generalized 2026-08-10)

**The rule, for every consumer of the codec — client, server, federation, adapter:**

> Decide a line's direction by **role**, then by **tag**, then by **verb**. Never by
> whether one of the decoders returned `Ok`.

There is exactly one tokenizer, `Line::parse`, and two typed decoders over its output —
`Request::from_line` → `Command` and `Reply::from_line` → `Event`. Both are **total**: an
unrecognised verb decodes to `Command::Unknown` / `Event::Unknown` rather than failing, by
design (§4 lenient-in, §7 clients ignore unknown events). So on any well-formed line *both
decoders succeed*, and `Ok` carries no information about which direction the line came from.

Where each role decides, and how:

|Role|Entry point|Direction|How it decides|
|---|---|---|---|
|client|`weft-client-core::apply_line`, `weft-tui`|events only|its role — it only ever calls `Reply::parse`|
|server, client session|`Session::on_line` → `on_request`|commands only|session **state** (`Negotiating`/`Unauthed`/`Ready`)|
|server, federation session|`on_bridge_line`|both|`@as` tag → command; else a **verb allow-list** (`MESSAGE`/`EDITED`/… → ingest, `GROUP-ROSTER`/`STREAM` → event); else command|
|server, provider session|`on_plugin_service_line`|both|`@as` tag → command; else a **known `Command` variant** (`REALM*`/`PROVISION*`/`STREAM OFFER`/…); else fall through to the event family|
|adapter (SDK)|the session loop|both|`invoke_of`; else `known_event`; else `PING`; else `step_of`; else command|
|IRC gateway|`IrcStream::recv_line` / `send_line`|both, two grammars|`irc::parse` inbound, `Reply::parse` outbound — never one decision|

Note that weftd's provider path and the SDK are **mirror images**: weftd tries `Command`
first and falls through to `Event`, the SDK tries `Event` first and falls through to
`Command`. Both are sound only because each enumerates *its own* recognised set. Written
either way as "try the other decoder and trust `Ok`", either one has the bug below.

The history: the SDK originally discriminated on "did it parse as a reply?", which made the
whole `Incoming::Command` arm dead code. A DM, an `NS JOIN`, a post into a replica, a
`GRANT`, even the liveness `PING` all arrived as an unknown event and were dropped without a
word — from the outside indistinguishable from weftd never sending. Hence `known_event`:
`Reply::from_line` minus `Event::Unknown`.

`SYNC END` is why the event reading wins when a line is genuinely both: it is a real event
*and* a lenient `Command::Sync`.

## 11. What the adapter owns, not weftd

Some things deliberately have **no protocol surface** — they are the adapter's, and the SDK supplies
the shared implementation so every adapter behaves the same:

- **Per-space bridging bans.** weftd tells you once:

  ```
  ← BRIDGING <ns-id> banned      (an operator banned this space in the admin panel)
  ← BRIDGING <ns-id> allowed     (…and lifted it)
  ```

  **Store it and enforce it yourself.** weftd keeps no record, so nothing is re-sent on reconnect —
  persist it and re-apply it, or a restart silently resumes a banned space. What "stop bridging"
  means is yours: leaving a Matrix room, ignoring a Discord guild, dropping a feed. That is why the
  instruction says only "stop". `NETBLOCK` remains the blunter, name-keyed instrument for taking out
  a whole realm.
- The foreign→WEFT identity mapping, and keeping it injective (§5).
- Deciding what a power level means in capabilities, and vice versa (§7).
- When to re-assert, and reconciling foreign state after a gap (§4, §6).

## 11a. Media (§13's data plane)

Blobs do **not** ride the control stream. Two surfaces, and they are
asymmetric:

| Dir | What | Credential |
|-----|------|------------|
| →   | `@label=<l> STREAM OFFER media <mime> <bytes>` | the session itself |
| ←   | `@label=<l> STREAM ACCEPT <token>` | — |
| →   | `POST /media?t=<token>` (HTTP), then `attach.N=weft-media://<hash>` on the `MSG` | the one-shot grant |
| ←   | `GET /media/<hash>` (HTTP) | **none** — content-addressed |

The fetch needs no credential by design (§13's media-proxy model: the 256-bit
BLAKE3 hash *is* the capability, obtainable only from a message you can already
see). The upload does, because it consumes storage — and a provider's grant is
authorized by its pinned key rather than an `attach` capability, since it has no
account. Size and mime are bounded exactly as for a client.

**Attach after upload, never before.** A `weft-media://` reference to a blob
weftd does not hold yet renders as a broken attachment, so the message waits for
the grant.

## 11b. A provider's own identity

`PLUGIN-REGISTER` may carry `bot=<account>`. weftd provisions it as a **bot** —
a native account kind, `bot` flag, migration 0056 — and it is then the one local
account this provider may name in `@as` (§5's forgery rule otherwise refuses
every local sender). That is the service speaking as itself.

A bot is a *kind*, not a punishment: it **cannot authenticate** on a client
session (uniform `AUTH-FAILED` at the single chokepoint — whether a handle is a
bot is not probeable), yet it is not suspended, so a misbehaving bot can still be
suspended and the two states stay distinguishable. It acts through its provider
today; an API-token path is the intended second door (owner directive
2026-08-06). The first cut reused `suspended` for this and was wrong: the panel
showed bots as punished users, un-suspending one would have silently granted it
login, and a real suspension was invisible.

## 11c. Typing (§15)

| Dir | Line | Notes |
|-----|------|-------|
| →   | `@as=<user@realm> TYPING #<ns-id>/<chan-id> start\|stop` | A realm's user. Needs `@as`: the wire's `TYPING` names no user, since a client's own session identifies them. |
| ←   | `TYPING #<ns-id>/<chan-id> <user@ournet> start\|stop` with `ulid=` | Ours. Attribution rides the **event**, so there is no `@as` here. |

Never stored, so it is announced rather than ingested. Same one-hop rule as
`MEMBER`, applied to the *user*: ours goes out, a bridged user's does not (that
one is the echo of an ingest). Read receipts stay unbridged — WEFT's `MARK` is
private, Matrix receipts are public.

Both room kinds count, in **both** directions: a consumed **replica** room and a
room **projected** from a native namespace. Authorizing only the first (via the
channel's `origin` scheme) made inbound typing and presence dead in every projected
room — the same blind spot the DM route had.

**Inbound is a set, not an event** (wired 2026-08-09). Foreign systems tend to
state *who is typing now* per room rather than sending a transition — Matrix's
`m.typing` EDU carries a `user_ids` array — while `TYPING` is per-user
`start`/`stop`. The adapter therefore holds the last set per room and sends the
**difference**: a user who appeared is a `start`, one who left is a `stop`. Notes
that follow from that:

- The set is memory-only and *should* be: typing is ephemeral by definition, and a
  restart that forgets is corrected by the next EDU, which restates the whole truth.
- A user who simply stopped typing arrives as a shorter list — the foreign
  server's own timeout is what empties it, so there is no timer to keep here.
- An empty set is dropped rather than stored, or the map grows one entry per room
  anyone ever typed in.
- Our own puppets are filtered out of the set: their typing is the reflection of a
  WEFT member's, already relayed outbound, and re-ingesting it would loop.

## 11d. Presence (§6.1, added 2026-08-09)

| Dir | Line | Notes |
|-----|------|-------|
| →   | `@as=<user@realm> PRESENCE online\|away\|dnd\|offline` | A realm's user. Needs `@as` for the same reason `TYPING` does. |
| ←   | `PRESENCE <user@ournet> <status>` with `ulid=` | Ours, for the adapter to set on their puppet. |

The one difference from typing, and it decides who does what: **presence names no
channel**. It is per-user and global in every system that has it, so the adapter
sends one line per status change and *weftd* fans it out — into the channels of
the namespaces that user is a member of, bounded to namespaces whose scheme the
provider's key is pinned for. An adapter that tried to fan out per room would be
guessing at rosters it does not hold.

- **`invisible` never crosses, either way.** weftd stores it without announcing
  (§6.1), so it does not reach the link at all; an adapter must not map it onto a
  foreign "offline" either, since the user would blink back into existence the
  moment they posted.
- **Rosters serve it.** A bridged member has no session here, so `MEMBERS` reports
  what the realm last said — and their entries are dropped when the realm's adapter
  disconnects, so a dead bridge reads offline rather than serving a remembered
  green dot.
- **Mapping is the adapter's.** WEFT has four states, Matrix three: `unavailable`
  is "here but not attending", so it maps to `away` inbound and receives both
  `away` and `dnd` outbound.
- Matrix specifics: inbound needs MSC2409 ephemeral pushes (`push_ephemeral: true`
  in the registration) **and** `presence: enabled: true` on the homeserver. With
  either off, the mirror is silently one-directional — WEFT → Matrix still works.

## 12. Not yet built

- **DM edits/reactions** on a bridged conversation apply locally only; the
  message path is wired (both directions), the mutation verbs are not.
- **Per-device attestations** on bridged events — trust is network-level: the provider proved control
  of its key on the session, so `att=` tags are not carried per event.
- ~~**Presence** — never bridged (core lock).~~ **Bridged both ways since
  2026-08-09 — see §11d.**
