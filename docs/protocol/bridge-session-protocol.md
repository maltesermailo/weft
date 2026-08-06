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
ingests them exactly as it ingests a peer network's. So a replica channel is **multi-origin** — our
members' events carry our origin, the realm's carry theirs — and that is what makes the ordinary
federation machinery apply unchanged.

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
| →   | `@reg=<b64-CBOR Registration> PLUGIN-REGISTER` | Actions, hooks, and the `schemes` this provider serves.                                                                                                   |
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

| Dir | Line                                                                 |
|-----|----------------------------------------------------------------------|
| →   | `@as=<user@realm>;msgid=<realm>/<ULID> MSG @<local-account> :<body>` |

Stored in the ordinary DM scope keyed by member keys, preserving the realm's msgid. A bridged
conversation is a first-class DM, not a second table.

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

Note: a provider attaches projected-channel forwarders (and receives the structure push) at
registration/`REALM ASSERT` time — a flag flipped mid-session is picked up on reconnect (§10's
recovery story).

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
mutation verbs, on a root the realm itself minted. Authoring as a local user (`MSG`, or touching a
local-origin root) remains a refused forgery. Practically: the adapter sees its own puppet's echo,
maps the puppet back to the local user, and re-ingests under that name.

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

## 10. Failure modes worth designing for

| Situation                                    | What weftd does                                                         | What the adapter should do                                           |
|----------------------------------------------|-------------------------------------------------------------------------|----------------------------------------------------------------------|
| Provider disconnects                         | Namespaces go offline; parked provisions fail; outstanding invokes fail | Reconnect and re-assert; re-state membership with `SYNC START`/`END` |
| Provider dies mid-provision                  | The waiting client gets `NO-SUCH-TARGET`                                | Nothing — the client retries                                         |
| Realm netblocked mid-session                 | Ingestion stops at once; a fresh `REALM ASSERT` is refused              | Stop; the block is deliberate                                        |
| Provider queue full                          | The line is dropped with a warning                                      | Keep the session drained; weftd does not block on you                |
| Membership statement lost (provider offline) | Nothing is queued                                                       | Re-state on reconnect — that is what the full-replace window is for  |

---

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

## 12. Not yet built

- **DM mutations** on a bridged conversation apply locally only; the relay hook exists (`MessageRoute::Dm`
  carries a `UserRef`) but is not wired.
- **Per-device attestations** on bridged events — trust is network-level: the provider proved control
  of its key on the session, so `att=` tags are not carried per event.
- **Typing, presence, media mirroring** across the bridge.
