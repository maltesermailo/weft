# WEFT Foreign-Realm Bridging Framework

**Status:** design concept (owner directive 2026-07-27). **UNIFIED with the plugin system
(owner directive 2026-08-03, `plugin-spec.md` §18):** this framework's capabilities — virtual
namespaces with an `origin` marker, foreign attribution (`@as`), scheme registration +
provisioning — are now **general provider capabilities** any plugin can use (an Instagram bridge is
just a plugin); only the federation-grade extras (`REALM ASSERT` per-realm connections, realm-keyed
NETBLOCK, federated backfill) stay bridge-specific behind the SDK's `bridge` feature. This doc remains
the deep spec for those, and for the realm/provisioning machinery the unified model absorbs.
The generalization of the Matrix bridge concept (`matrix.md`) into a **pluggable framework** for
bridging external chat systems — Matrix first, Discord and others later. Protocol logic lives in per-app **adapter
daemons**; weftd core learns only the generic concept of a *foreign realm*, never any single
protocol. This doc is the framework; each adapter (`matrix.md`, future `discord.md`) is a
*binding* that fills the framework's slots.

**Depends on:** the ns-membership + SYNC redesign (`namespace-membership-sync-v0.12.md`,
shipped) — hide overrides drive membership mapping; the home-authoritative replica model
(`home-authoritative-channels.md`) is reused for the foreign replica.

## 0. Why a framework (not per-app bridges)

The Matrix concept doc (`matrix.md` §5) originally laundered a remote homeserver into a
*virtual WEFT network* (`matrix.org` → `matrix-org.mx.test.example`) so weftd's existing
network/account/attestation machinery would work untouched. The owner rejected that: **foreign
things must stay foreign** — `matrix.org` is `matrix.org`, addressed by its native Matrix
coordinates, badged as external, never disguised as one of our networks. Doing that honestly
means teaching weftd a first-class *foreign-realm* dimension. Once we're paying that cost, it
must be **generic** — the same dimension serves Discord, XMPP, Slack, or anything else, so we
never re-litigate it per app. One framework, N adapters.

**Principle:** weftd core gains the *concept* of foreign realms and a trusted, pluggable bridge
contract. It contains **zero** protocol-specific code. Adding Discord later is a new adapter
daemon + an adapter-binding doc, with **no further core change**.

## 1. Locked decisions

| # | Question | Decision |
|---|---|---|
| 1 | Foreign identity | **Native, never remapped** — for **spaces/channels**: they keep their own coordinates and are addressed by a `<scheme>://<realm>/<path>` URI, never laundered into WEFT namespace grammar. Supersedes `matrix.md` §5. **AMENDED 2026-08-04 for *users*:** a replica *user* is attributed as a federated `UserRef` on the realm (`alice@matrix.org`) so replicas present as an ordinary federated network — see §7a.0. |
| 2 | Protocol logic location | **Per-app adapter daemons only.** weftd core knows the generic contract + scheme routing; it never parses a foreign protocol. Restores the `matrix.md` §17 module boundary at the framework level. |
| 3 | Trust / connection | **Two planes** (§3): a realm-agnostic **control link** per adapter (scheme registration + weftd→adapter provisioning pushes, §3.3) and **one data connection per realm** — pinned-key authed, bound to a single `(scheme, realm)` at connect via a `REALM ASSERT` handshake. **Multiple adapters may connect concurrently** (Matrix + Discord, or sharded per-realm instances). Per-realm binding of the data connection makes cross-realm spoofing structurally impossible and gives per-realm failure/NETBLOCK domains; the scope URI (`matrix://matrix.org/…`) and account (`@a:matrix.org`) are self-describing, so assertions carry no separate realm tag. |
| 4 | Home authority (inbound) | **Home-authoritative replica.** weftd mints the WEFT-side ULIDs for the foreign replica; the foreign system remains the true social home. Order-divergence is an honest limit. |
| 5 | Consent | **Per-direction, per-namespace opt-in.** Outbound (advertise a WEFT ns into a realm) requires an explicit `NS META <ns> bridge:<scheme> :open` flag. Inbound (consume a foreign space) is an explicit user `NS JOIN <uri>`. Never automatic. |
| 6 | E2EE | **Never bridged.** A foreign encrypted space is refused on join (`NO-SUCH-TARGET`) and a space that becomes encrypted after join is withdrawn + tombstoned WEFT-side. Invariant 8, per adapter. |
| 7 | NETBLOCK | **Realm-keyed.** `NETBLOCK REALM <scheme>://<realm>` severs one foreign server; §11.6's four effects map per realm. |

## 2. Addressing model

```
<scheme>://<realm>/<space>[/<channel>]

matrix://matrix.org/gaming            a Matrix Space  → foreign namespace
matrix://matrix.org/gaming/general    a room in it    → foreign channel
matrix://matrix.org/lobby             a standalone room → foreign namespace w/ one channel
discord://123456789/general           a Discord guild+channel  (adapter-defined path shape)
```

- **Scheme** = protocol (`matrix`, `discord`, …). **Realm** = the external server/instance
  (`matrix.org`; for Discord, the guild — adapter-defined). **Path** = the space/channel, in the
  adapter's own terms.
- **Foreign account** keeps its native form, scheme-tagged: `@alice:matrix.org`,
  `discord:824…snowflake`. weftd stores it opaquely with `(scheme, realm)`; only the adapter and
  the client fully interpret it. Clients render foreign accounts + namespaces **badged** with the
  realm ("Matrix · matrix.org").
- weftd stores foreign namespaces / channels / accounts as **first-class foreign objects** —
  origin `(scheme, realm)`, addressed by URI. **(Implementation, owner call 2026-08-03: these
  reuse the existing namespace/channel/membership tables with an `origin` URI marker —
  discriminated, not a parallel table set. A namespace with `origin = Some("<scheme>://<realm>/<space>")`
  is the replica; `None` is native. The marker keeps it badged + URI-addressed and gates it out of
  local social-home authority (§7), so "never disguised" holds without duplicate tables.)**

## 3. The `State::ForeignBridge` session context (new)

An adapter holds **two kinds of link** to weftd, both `State::ForeignBridge`, both pinned-key
authed (config: `[[foreign_bridge]] scheme = "…", key = "<b64>"`; **multiple entries allowed** —
several adapters, or one adapter authorized for several schemes, or sharded per-realm instances,
connect concurrently). The handshake is **`AUTH ADAPTER <pubkey>`**, reusing the §6.1
`CHALLENGE`/`AUTH PROOF` flow: the adapter proves control of a key pinned in `[[foreign_bridge]]`
(an unpinned key → uniform `AUTH-FAILED`, no adapter-existence oracle) and enters
`State::ForeignBridge`. The *scheme(s)* the key may speak for are checked later, at
`REALM REGISTER` / `REALM ASSERT` (a key pinned for `matrix` cannot assert a `discord://` realm).

- a **control link** (one per adapter, realm-agnostic) — registers the scheme(s) the adapter
  handles and carries **provisioning** requests weftd pushes for realms without a live data
  connection (§3.3). It never carries realm-bound content.
- a **data connection per realm** — **bound to a single `(scheme, realm)`** by a `REALM ASSERT`
  handshake, carrying all of that realm's traffic both ways. This is the connection meant in
  "one connection per realm."

The **data connection** is the trusted component that may:

- **assert foreign namespaces/channels/membership/events** for its bound realm into weftd (weftd
  mints the replica ULIDs — home-authoritative replica); the scope URI and `@as=<foreign-account>`
  are self-describing, so assertions need no separate realm tag;
- **receive** our users' actions on the same connection (weftd relays local joins/posts/edits/
  moderation targeting the realm's URIs back to the adapter to translate outward).

Isolation: a connection may only assert its **bound** realm — it cannot speak for another realm
even under the same adapter key, so spoofing is structurally impossible. A NETBLOCKed realm's
connection is refused at the `REALM ASSERT` handshake. Stronger than a multiplexed connection, and
it yields per-realm failure domains.

### 3.1 Contract verbs — adapter → weftd (data connection)

The adapter asserts foreign structure and events through **existing** weftd surfaces; the only
bridge-specific verbs are the realm bind/teardown handshake.

- `REALM ASSERT <scheme>://<realm>` — the **connect-time binding**: declares the single realm this
  connection speaks for. weftd verifies the adapter key is trusted for the scheme and the realm is
  not NETBLOCKed, then binds the session. (In the per-realm-connection model this is the handshake,
  not optional accounting.)
- `REALM WITHDRAW` — graceful teardown of the bound realm (lost upstream connection, or the realm
  was deleted): weftd withdraws its foreign namespaces cleanly. Distinct from an operator
  `NETBLOCK REALM` — that is a *block*, this is a *disconnect*. Closing the connection is an
  implicit withdraw.
- **Namespace/channel structure reuses `NS-META` + `CHANNEL-LAYOUT`** — no bespoke tree verbs. The
  trusted connection asserts those existing events with a `<scheme>://` target in its bound realm,
  exactly as a WEFT↔WEFT bridge ingests remote structure via ordinary events. (Retires the earlier
  `FNS`/`FCHAN` idea.)
- **Events reuse the existing verbs** — `MSG`/`EDIT`/`DELETE`/`REACT`/`MEMBER`/`TYPING`, plus
  `HISTORY` for bounded backfill — with a `<scheme>://` scope and `@as=<foreign-account>`. The
  adapter never mints WEFT ULIDs; weftd does (invariant 2 — origin authority = the realm-bound
  trusted connection).

### 3.2 Contract verbs — weftd → adapter (data connection: relayed local actions)

weftd forwards a local user's action on a foreign URI to the adapter as a relay envelope
(join/part, post/edit/delete/react, moderation) over the realm's data connection. The adapter
translates it into the foreign protocol (e.g. puppet-post into a Matrix room). Modeled on the
existing bridge relay path.

### 3.3 Provisioning flow — how `NS JOIN <uri>` reaches the adapter

First contact with a space needs weftd to reach an adapter for a realm it may not yet be connected
to — that is the control link's one job. The user surface is unchanged (`NS JOIN <uri>`, §4); the
async completion reuses the same **label-correlated pending-request** machinery as auto-federation
(`FEDERATE` → `run_bridge_requester` → the user's request completes when the peer's manifest
arrives — here the control link stands in for the remote peer, and `NS-META`/`CHANNEL-LAYOUT`
assertions stand in for the manifest).

```
1. C → weftd            @label=j1 NS JOIN matrix://matrix.org/gaming
2. weftd                store lookup on the URI:
   ├─ known locally  →  ordinary join: add membership, relay the user-join to the realm data
   │                    connection (adapter puppet-joins them), echo @label=j1 (roster + POLICY).
   │                    [no provisioning — the steady-state path]
   └─ unknown + has scheme → park the NS JOIN pending, keyed by a provisioning job; go to 3.
3. weftd → adapter      (CONTROL link)  PROVISION matrix://matrix.org/gaming j1
4. adapter              resolve #gaming:matrix.org → join via companion HS (S2S) → enumerate rooms.
5a. not found / unjoinable / encrypted:
    adapter → weftd     (CONTROL)  PROVISION-ERR j1
    weftd → C           @label=j1 ERR NO-SUCH-TARGET          (uniform, invariant 1)
5b. ok — adapter opens/uses the matrix.org DATA connection:
    REALM ASSERT        matrix://matrix.org                   (binds the connection)
    NS-META             matrix://matrix.org/gaming :Gaming
    CHANNEL-LAYOUT      matrix://matrix.org/gaming/general 0
    POLICY              matrix://matrix.org/gaming/general retained:90d
    …                   (weftd mints replica ULIDs — home-authoritative)
    adapter → weftd     (CONTROL)  PROVISION-OK j1
6. weftd                namespace now exists → add the requester as a member, relay the user-join to
                        the data connection (adapter puppet-joins the remote room), complete the
                        parked request:
   weftd → C            @label=j1 …roster + POLICY…            (identical shape to a native NS JOIN)
```

- **Provisioning fires once per space, ever.** After step 5b the namespace is materialized and in
  `DISCOVER`; every later joiner takes branch 2-known — a local join + a relay puppet-join, no
  control-link traffic, no remote lookup.
- **Control-link contract:** `REALM REGISTER <scheme>` (adapter startup) · `PROVISION <uri> <job>`
  (weftd→adapter) · `PROVISION-OK`/`PROVISION-ERR <job>` (adapter→weftd). Realm-agnostic; no content.
  (`REALM REGISTER`, not the design's earlier bare `REGISTER`, so the verb never collides with
  account registration; `job` is a positional correlation token, not a `:job=` tag.)
- **Failure = `NO-SUCH-TARGET`**, uniform in code + timing with a nonexistent local namespace
  (invariant 1) — a private/encrypted/absent remote space is indistinguishable from "no such thing."

## 4. User-facing verbs (generic — weftd core)

**Zero new user verbs.** The entire user surface is the existing v0.12 namespace verbs, now
accepting a `<scheme>://…` URI target. Foreignness is a property of the *target*, not the *verb*.

- **`NS JOIN <uri>`** absorbs provisioning. `NS JOIN matrix://matrix.org/gaming`: if the target is
  already known locally, it is an ordinary join; if it is **unknown and carries a scheme**, weftd
  routes to a registered adapter, which opens (or reuses) the per-realm connection, resolves + joins
  the remote space, enumerates it, and asserts it back via `NS-META`/`CHANNEL-LAYOUT` — then the
  caller becomes a member. (An unknown target with **no** scheme is still `NO-SUCH-TARGET`,
  invariant 1.) This is the only place where a join may block on an async remote round-trip and
  return network-shaped errors — a documented property of URI targets, not a new verb. **Retires
  the earlier `FOREIGN JOIN`.**
- **Leaving reuses `NS LEAVE` / `PART`** on the URI — same membership + hide-override mechanics as
  any namespace. weftd signals the adapter when the *last* local member leaves so it can drop the
  upstream join.
- **Listing reuses `SYNC`** — joined foreign namespaces are memberships, so they already appear in
  the v0.12 SYNC skeleton like native ones.
- **Discovery reuses `DISCOVER`** — a provisioned foreign namespace is listed (badged), so everyone
  after the first joiner needs nothing special (§6).

Provisioning fires **once per remote space, ever** (first contact); every join afterward is an
ordinary local `NS JOIN`. Scheme-agnostic: `matrix` is the first scheme handled; `discord` is a
config + adapter away, with no new verb.

## 5. Directionality — every adapter fills both halves

- **Inbound (consume):** foreign realm → WEFT namespace. A user `NS JOIN <uri>`s an as-yet-unknown
  space; weftd provisions via the adapter, which asserts the foreign-addressed replica. This is the
  owner's "join spaces/rooms as namespaces."
- **Outbound (advertise/project):** WEFT namespace → foreign realm. Opt-in via
  `NS META <ns> bridge:<scheme> :open`; the adapter projects the namespace into the foreign
  system (Matrix: Spaces on the companion HS; Discord: a managed bot in a guild). Publication
  policy is adapter-defined (Matrix: open publication to the room directory).

## 6. Membership, retention, visibility for foreign namespaces

- **Membership** reuses the ns-membership + hide-override model (`matrix.md` §8), foreign-
  addressed: first space/room join → join the foreign ns + hide the rest; per-channel join clears
  a hide; leaving the last → `NS LEAVE`. Applies uniformly to remote foreign users **and** our own
  users who `NS JOIN <uri>`ed.
- **Retention:** a foreign channel is a **bounded replica** — policy `retained:<config>`; deeper
  history via on-demand adapter backfill → WEFT `HISTORY`. **Never `permanent`** (we can't promise
  a remote space's permanence) and **never `e2ee`** (decision 6).
- **Visibility:** foreign namespaces **appear in `DISCOVER`**, subject to the foreign space's own
  visibility (public → listed; private/unjoinable → absent, invariant 1), and are always
  **client-badged** with their scheme + realm so users see they are external. Also reachable by a
  direct `NS JOIN <uri>`.

## 7. Authority (honest limit, framework-level)

For a **consumed** foreign realm we are *not* the social home — the foreign system is. WEFT caps
govern only our local replica surface and our users' ability to relay outward. A WEFT operator can
`NETBLOCK REALM …` (sever everything for that realm) but cannot moderate the foreign system's own
users inside the foreign space. Foreign roles/PLs project **inward as advisory** metadata. Each
adapter documents its exact authority mapping; the framework only guarantees the honest limit and
the NETBLOCK escape hatch. (For **outbound-projected** WEFT namespaces the reverse holds — WEFT is
home and authoritative; see `matrix.md` §10.)

## 7a. Foreign display & the namespace capability profile (owner directive 2026-08-03)

The framework so far maps foreign **structure** (namespaces/channels/messages), but the wire does not
yet carry the foreign **identity + authority metadata** the client needs to render Matrix things
properly, nor a way for a bridge/plugin to tell the client which native settings apply. This section
specs those additions. All are **additive wire fields** (proto, round-trip-tested first); the plugin
system's SDUI/widgets can't help here, because the stock client renders the message stream + settings
from these events, not from plugin UI.

### 7a.0 A realm **is a network** (owner directive 2026-08-04, refined 2026-08-04)

**This amends decision 1 (§1) for *users* and for event minting.** A bridged realm is modeled as a
network, so accessing `matrix.org` makes its users *belong to* `matrix.org` and its events *originate
on* `matrix.org`. A replica is then indistinguishable from a peer-federated channel, and the whole
peer-federation machinery applies unchanged.

- **The adapter owns identity.** `@as=<user@realm>` carries the finished WEFT handle
  (`alice=bob@matrix.org`), not a native identifier for weftd to mangle. Only the adapter knows its
  realm's escaping rules, so only the adapter can keep the mapping **injective** — which it must:
  a lossy mapping merges two foreign users into one WEFT identity (their messages, roster entry, and
  mentions all collide). To make that achievable the account grammar admits `=` and `+`
  (spec §2.3, decision (3)), covering Matrix localparts except `/`, WEFT's own path separator.
- **The adapter mints msgids.** `MSG` and `EDIT` carry `@msgid=<realm>/<ulid>`; `DELETE`/`REACT`
  name only their root and get a local bookkeeping id, exactly as on the peer path. weftd never
  mints for a foreign origin (invariant 2).
- **weftd enforces one thing:** `@as` *and* `@msgid` must both name the realm whose scheme this
  provider's key is pinned for. A provider cannot forge a local account or another realm's event.
- **Ingestion is the federated path**, verbatim: `ingest_record` re-checks origin, and the outbound
  relay forwards iff `msgid.origin == our network` — so an event we ingested is never sent back to
  the provider that produced it.
- Decision 1 still holds for **namespaces/channels**: those stay `<scheme>://`-addressed and
  `origin=`-badged; nothing is laundered into WEFT *namespace* grammar.
- **Known consequence** (a replica user *looks* federated but has no peer bridge): DM routing,
  `FEDERATE`, and name-keyed `NETBLOCK` treat `matrix.org` as a network. Guarding those paths is
  tracked as slice-4 follow-up work, not silently assumed safe.

### 7a.1 ~~Foreign identity on content — `foreign=`~~ (REMOVED 2026-08-04)

A `foreign=<native-account>` tag on `MESSAGE`/`MEMBER`/`REACTION`/`EDITED` used to carry the exact
native handle for display beside a locally-minted event. It is **gone**. Under §7a.0 the sender
`user@realm` *is* the identity — the same way `ada@hda.example` is on a peer network — so the tag
duplicated it. Worse, it concealed a defect: because it made the WEFT handle merely cosmetic, that
handle was derived lossily, and two distinct foreign users could collide onto one account. Making the
identity authoritative forces the mapping to be injective. Clients badge a bridged user from the
channel/namespace `origin=` (§7a.2) plus the network suffix they already render for federated users.

### 7a.2 Origin on namespaces/channels — `origin=`

`NS-META`, `CHANNEL-LAYOUT`, and `DISCOVER` entries gain an optional **`origin=<scheme>://<realm>/<path>`**
tag (the store marker of slice 5, now surfaced on the wire). The client badges the namespace/channel
**foreign** and reads scheme+realm for the badge label. Absent ⇒ native.

### 7a.3 The namespace capability profile — power levels instead of roles, and settings gating

A namespace carries a **capability profile** the client uses to choose its **authority rendering** and
which **native settings surfaces** to show. A native WEFT namespace has the implicit default profile
(roles authority, all settings enabled). A **provider-managed** namespace (a foreign bridge, or a
plugin that owns a namespace) supplies a profile — carried as tags on `NS-META`:

- **`authority=roles|levels|none`** — how the client renders the ns's authority.
  - `roles` (default): the native WEFT roles editor + role pills.
  - **`levels`**: a numeric/threshold model — **Matrix power levels**. Members show a **level**
    (§7a.4); the client renders a *levels* view (or the provider's own, §7a.5) **instead of** the roles
    editor. This is the "for Matrix, show power levels, not roles."
  - `none`: no local authority UI.
- **`settings=<disabled-keys>`** — a set of native settings surfaces to **disable/hide** for this ns.
  Gate-able keys (fixed, extensible): `roles` · `permissions` · `channels` (create/delete) · `invites`
  · `moderation` · `ns-edit` (title/visibility/etc.) · `recovery`. For a Matrix ns the bridge disables
  `roles`/`permissions`/`ns-edit`/`recovery` (Matrix-governed; §7 honest limit) and keeps the rest
  read-only or provider-driven. **This is "plugins can disable certain server settings"** — a general
  mechanism any provider supplies, not Matrix-specific.

A gated setting the client hides is also **refused server-side** for a foreign ns (the §5-slice
authority gating already refuses NS META/DELETE/etc. on `origin=Some`); the profile makes the *client*
match, and lets a provider disable surfaces even on a plugin-managed *native* ns.

### 7a.4 Advisory member level — `level=`

For `authority=levels`, `MEMBER` (and the roster) carry an optional **`level=<n>`** (+ optional
`level-label=`) — the member's foreign power level, **advisory** (read-only, §7). The client shows it
in place of role pills. For `authority=roles` it is absent.

### 7a.5 Who supplies it, and the plugin connection

For a **foreign** namespace, the bridge asserts the profile as part of its `NS-META` structure
assertion (§3.1) — `origin` + `authority=levels` + the disabled `settings`. The **custom "power
levels" view** itself is a **provider settings-surface action/widget** (`plugin-spec.md` §13.1
`settings` surface) — so the profile *disables* the native roles editor and the provider *supplies*
the levels view. The two mechanisms compose: the capability profile is declarative gating; the SDUI/
widget is the replacement UI.

### 7a.6 Honest limits

Read receipts, Matrix presence, and typing fidelity are **not** bridged (§7, same-network-only);
`m.emote`/`m.notice` map to WEFT message forms (lossy); pills/mentions to foreign users render as
plain badged handles unless an adapter does more. Each adapter documents its exact mapping (§10).

## 8. Security invariants (framework additions — implement AS TESTS)

1. **Trusted-context authority:** only a pinned `State::ForeignBridge` connection may assert foreign
   content, and only for the single realm its connection is bound to. A connection asserting any
   other realm's scope, or any non-bridge source asserting a `<scheme>://` scope, is a protocol
   violation.
2. **Realm-keyed NETBLOCK (four effects):** blocking `<scheme>://<realm>` ⇒ reject that realm's
   assertions + withdraw its foreign namespaces + drop relay of our users into it + stop its media.
   Name-keyed; never evadable by re-registering under a new key.
3. **E2EE exclusion (decision 6):** no code path materializes plaintext for an encrypted foreign
   space; refuse on join, withdraw on transition.
4. **Anti-enumeration (invariant 1):** a private / unjoinable / encrypted foreign target →
   `NO-SUCH-TARGET`, uniform code + timing, indistinguishable from nonexistent.
5. **Media block-list** enforced on both directions of every adapter (inbound blob → hash →
   `MEDIA BLOCK` check before `STREAM`; a later block redacts the mapped foreign events best-effort).

## 9. weftd core footprint (the whole framework cost)

1. `State::ForeignBridge` session state + pinned-adapter auth (`[[foreign_bridge]]` config, multiple
   entries) with one connection per realm, bound at the `REALM ASSERT` handshake.
2. `<scheme>://` scope grammar + foreign-account identity types (opaque path/account,
   `(scheme, realm)` origin) in `weft-proto` — **round-trip tested first**. No per-assertion realm
   tag: the per-realm connection + self-describing URI/account carry it.
3. `NS JOIN` accepting a `<scheme>://` URI (the provisioning path, §3.3) + a scheme→adapter routing
   registry + label-correlated pending-request tracking (reuse the auto-federation machinery). On
   the bridge context: the control-link contract (`REALM REGISTER`/`PROVISION`/`PROVISION-OK|ERR`) and the
   data-connection `REALM ASSERT` (binding handshake) + `REALM WITHDRAW` (teardown). Leaving/listing/
   discovery reuse `NS LEAVE`/`PART`/`SYNC`/`DISCOVER` — **no new user verbs**.
4. Foreign structure via reused `NS-META`/`CHANNEL-LAYOUT` assertions; foreign-scoped event
   ingestion (reusing `Cmd::Ingest`-style paths); home-authoritative replica minting.
5. Store: foreign namespaces / channels / membership via the **existing** tables + an `origin`
   URI marker on the namespace record (mem + PG, shared contract; migration 0052) — reuse +
   discriminator, not a parallel table set (owner call 2026-08-03).
6. Realm-keyed `NETBLOCK`.
7. **Foreign display + capability profile (§7a, owner directive 2026-08-03):** additive wire fields
   (proto, round-trip-first) — `origin=` on `NS-META`/`CHANNEL-LAYOUT`/`DISCOVER` (badging, §7a.2);
   `@msgid=` on provider ingestion (the adapter mints, §7a.0); the ns capability
   profile `authority=roles|levels|none` + `settings=<disabled-keys>` on `NS-META` (§7a.3); `level=` on
   `MEMBER`/roster (advisory PL, §7a.4). weftd emits them on the replica; the client renders badges +
   the levels view + gated settings. The general **settings-gating** mechanism is shared with the
   plugin system (a plugin owning a namespace can supply the same profile).

That is the entire core change. **Adding an adapter afterwards touches none of it** — it is a new
daemon + a config stanza + an adapter-binding doc.

## 10. Adapter contract — what each adapter-binding doc MUST specify

- scheme name(s); realm identity; foreign-account form + URI path shape;
- space/channel discovery, join, enumerate, leave;
- event translation **both ways**: message / edit / delete / reaction / reply / thread / typing /
  media, with the fidelity + honest limits;
- membership mapping onto ns-membership + hide overrides;
- identity mapping (foreign account ↔ how WEFT renders it; our users ↔ how they appear in the
  foreign system);
- moderation/authority mapping (foreign roles/PLs ↔ WEFT caps; which side is authoritative);
- retention + e2ee handling; media flow + block enforcement;
- outbound projection/advertise mechanism + consent + publication policy;
- honest limits (order, roster fidelity, redaction semantics, rate limits/ToS).

## 11. Adapters

- **Matrix — `matrix.md` (adapter #1, full spec).** Outbound projection (WEFT ns → Matrix Spaces
  via a companion homeserver, §3–§16) + the inbound binding (Matrix Spaces/rooms → foreign
  namespaces). Native identity: `@alice:matrix.org`, `matrix://matrix.org/…`. Transport: the
  companion HS speaks Matrix S2S; the adapter is simultaneously its appservice and a
  `State::ForeignBridge` client to weftd. **Built on the `weft-appservice` SDK** (its `bridge` feature):
  the bridge is a WEFT **remote plugin** (App Service) + the realm/provisioning helpers, so the generic
  plugin/app-service machinery (`plugin-spec.md` §2a.5) is its base and it is not a bespoke server path.
- **Discord — planned (`discord.md`).** Key differences to sketch when we get there: no open
  S2S federation → a bot token + gateway/websocket, so **outbound** is "our managed bot posts in a
  guild" and **inbound** is "the bot relays a guild's channels" (realm = guild snowflake); snowflake
  identities; Discord roles ↔ WEFT caps; no user-visible e2ee for guild channels; **rate limits +
  Discord ToS/self-bot rules are hard honest limits** that constrain roster fidelity + puppeting
  (likely webhook/bot-attributed messages, not per-user puppets — an adapter honest limit).

## 12. Implementation order

1. **Framework core** (§9) — proto types + verbs (round-trip first), `State::ForeignBridge`, store
   tables, scheme routing, realm-keyed NETBLOCK. Adapter-agnostic; testable with a mock adapter.
2. **Matrix adapter** — reuses most of `matrix.md`'s existing plan (projection, event mapping,
   media, moderation), now expressed against the framework contract instead of virtual networks.
3. **Discord adapter** — new daemon only; validates that the framework generalized correctly (the
   real test of §0's premise).

## 13. Owner decisions & open confirms

**Decided (2026-07-27):**
- **Connection model:** one connection **per realm** (not multiplexed), pinned-key authed +
  realm-bound at the `REALM ASSERT` handshake; **multiple adapters may connect concurrently**.
  Chosen for spoof-proof isolation + per-realm failure domains (§1 decision 3, §3). This also makes
  `REALM ASSERT` the mandatory binding handshake — resolving the earlier keep/drop question.
- **DISCOVER:** foreign namespaces **are** listed in `DISCOVER` (subject to the foreign space's own
  visibility, invariant 1) and always badged foreign (§6).
- **Core footprint:** the §9 footprint is accepted as a real (non-"near-zero") addition, justified
  by generality across future adapters.
- **User verb surface:** **zero new user verbs** — `NS JOIN <uri>` absorbs provisioning (the whole
  user surface is existing v0.12 verbs accepting `<scheme>://` URIs; foreignness is a target
  property, not a verb). Retires the earlier `FOREIGN JOIN`/`FOREIGN` family. The only new verbs are
  the adapter-side `REALM ASSERT`/`REALM WITHDRAW` handshake on the trusted bridge context (§3.1).

**Still open:** none — the design is fully specified. Next step is implementation (§12) or the
Discord adapter binding to pressure-test the abstraction.
