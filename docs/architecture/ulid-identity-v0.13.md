# ULID identity for namespaces & roles (v0.12 → v0.13) — change plan

> **Status:** design in progress. Owner directive 2026-07-27. A **wire- and
> storage-breaking** change: namespaces and roles become **ULID-identified**;
> their former names become mutable **vanity labels**. Comparable in scale to the
> v0.12 membership change — sequenced into independently reviewable phases below.
> Precise per-crate file lists are being filled in from a code-map pass.

## 0. Motivation

Today a namespace *is* its name and a role *is* its (scope, name) — so the name is
the identity. That means:

- **Renames are impossible / unsafe.** A namespace or role name is embedded in
  capability-token scopes (signed CBOR), grant records, channel names (`#ns/chan`),
  and federation addressing. Renaming would silently invalidate every token/grant
  and orphan every channel.
- **Names can't be reclaimed or moderated** independently of the entity.

Moving identity to a ULID (assigned once, immutable) and demoting the name to a
mutable vanity label fixes both: rename freely, lock/moderate vanity names, and keep
tokens/channels/federation stable across renames.

## 1. Locked decisions (owner, 2026-07-27)

| # | Question | Decision |
|---|---|---|
| 1 | Namespace identity | A **ULID** assigned at `NS CREATE`, immutable. The former name becomes a mutable **vanity name**. |
| 2 | Role identity | A **ULID** assigned at `ROLE CREATE`, immutable. Role name becomes a mutable display label. **ROLE commands take the role ULID**, not the name. |
| 3 | Channel wire identity | Channels **embed the namespace ULID**, not the vanity name: `#<ns-ulid>/<chan>` (was `#<vanity>/<chan>`). A rename of the namespace vanity never touches channel identity. |
| 4 | Token / grant scopes | Scopes embed the ULID: `ns:<ns-ulid>` and `#<ns-ulid>/<chan>`. This is what makes tokens/grants rename-safe — and forces a migration of every existing token/grant/role scope. |
| 5 | Federation | Federation **pins to the ULID**. A peer namespace is identified by `<network>/<ns-ulid>`; the vanity is display/discovery only and never the pinned identity. |
| 6 | Vanity names | Per-network unique, mutable, settable in server settings. Resolvable vanity→ULID for human-facing entry (invites, DISCOVER, `FEDERATE`). |
| 7 | Vanity **lock** | A locked vanity name **cannot be registered or changed without admin intervention**. Admins set/clear locked vanity↔namespace bindings in the **web admin panel** (store-direct). A lock reserves the name (optionally already bound to a namespace). |

## 2. The precedent: accounts already did this

Accounts were migrated name→ULID and it is the template to copy:

- `Subject::Account(Ulid)` in `weft-crypto/src/captoken.rs:44-54`; token `VERSION = 2`
  (`captoken.rs:34`) **hard-denies** old name-subject tokens.
- Migrations `weft-store/migrations/0016_account_ulid.sql`, `0017_grants_by_ulid.sql`
  re-keyed grants to the account ULID.
- Grant **enforcement already reads records keyed by the stable id**, and role
  assignments (`weft_role_assignments`) are display-only, so re-keying is a store
  migration + re-mint, not a logic rewrite.

**What that migration left open (this change closes it):** the token *scope* is still a
name string. `TokenScope::Namespace(String)` / `Channel(String)` (`captoken.rs:95-102`)
serialize `ns:<name>` / `#ns/chan` into the **signed** CBOR payload (`to_wire` 177-190,
`signing_bytes` 193-197). And `ChannelName::namespace()` (`weft-proto/src/name.rs:169`)
plus SQL `substring(m.channel from '#([^/]+)/')` (`0035_ns_membership.sql:31,45`) derive
the ns from the channel string. So the ns name is baked into signed bytes *and* channel
identity — exactly why this is wire+storage breaking.

## 3. Design notes / implications

- **Channels also get their own ULID** (owner refinement 2026-07-27). Wire/scope identity
  is `#<ns-ulid>/<chan-ulid>` — both segments ULIDs. The channel keeps a **vanity local
  name, unique within its namespace** (multiple channels with the same name are not
  allowed), so `#<vanity-ns>/<vanity-chan>` resolves unambiguously to `#<ns-ulid>/<chan-ulid>`.
  - The **IRC gateway (§17) and clients address by `#vanity/channel-name`** and resolve to
    the ULID pair at the wire boundary; the raw WEFT wire carries the ULIDs.
  - `ChannelName::namespace()` still returns the (now ULID) first segment, so crypto
    `covers` and the SQL `substring(... '#([^/]+)/')` extraction keep working unchanged.
  - Top-level (namespace-less) channels also get a ULID: wire `#<chan-ulid>`, vanity name
    unique per network, gateway addresses by `#vanity`. Uniform "every channel = ULID +
    vanity" model.
- **Token scope cutover, mirroring the account move:** bump token `VERSION → 3`;
  `TokenScope::Namespace(Ulid)` / channel scope embeds the ns ULID. Old scope tokens
  hard-denied; clients re-mint on next auth from migrated grant records (enforcement reads
  records, §2). No name equality left in `covers` — it becomes ULID equality.
- **Vanity resolution is a per-network directory** `vanity → ns-ulid` (one server = one
  network, so effectively a unique-vanity index). DISCOVER, invite links, and
  `FEDERATE <net>/<vanity>` resolve vanity→ULID at the wire boundary, then pin the ULID.
- **Federation pins the ULID:** `Federate`/`BridgeRequest` carry (or resolve to) the peer
  ns ULID; the vanity is discovery/display only. Invite links (`weft://net/<vanity>/i/id`)
  resolve vanity→ULID on redeem.
- **Locking** = a reservation row keyed by the network-scoped vanity, with an
  `admin_locked` flag + optional bound ns ULID. A vanity set/rename refuses a locked name
  unless the actor is an operator (web-admin authority). Admins set/clear locks
  store-direct in weft-admin.
- **Netcat-debuggability (spec §4)** takes a readability hit (ULIDs in channel names).
  Accept: clients always show vanity; raw wire stays valid text.

## Progress

- ◑ **Phase 1 (proto) — started.** Foundational identity types landed in
  `weft-proto/src/name.rs` + exported: `NamespaceId`, `RoleId`, `ChannelId` (ULID
  newtypes, bare uppercase Crockford, case-insensitive parse), `VanityName` (mutable
  `[a-z0-9-_]{1,64}` label), and `ChannelName::namespace_id()`/`channel_id()` accessors
  for the `#<ns-ulid>/<chan-ulid>` wire form (legacy vanity channels still parse, no id).
  Round-trip tested; 113 proto tests green, clippy clean. **Additive so far** — the
  command/event rewiring is the next increment and cascades into core (below).
  - ✅ **ROLE codec redesigned to role-ULIDs** (proto green, 110 tests, clippy+fmt clean):
    `RoleCreate` now mints the id server-side (wire unchanged); **new `RoleUpdate <scope>
    <role-id> …`** edits by id and **subsumes `ROLE RENAME`** (removed); `RoleDelete`/
    `RoleAssign`/`RoleUnassign` take the `RoleId` positionally; `RolesReorder` order is
    `Vec<RoleId>`; the `ROLE` event carries `role: RoleId`, `ROLE-MEMBER` carries
    comma-separated role ids. Round-trip tested. **Proto compiles because NS commands are
    still name-based (untouched) — that's the next increment.**
  - ✅ **NS codec redesigned** (proto green, 110 tests, clippy+fmt clean): `NsCreate` carries
    the desired **vanity** (`VanityName`), server mints the id; `NsMeta`/`NsVisibility`/
    `NsDelegate`/`NsDelete`/`NsJoin`/`NsLeave`/`NsTransfer`/`NsRecoverySet`/`NsRecover`/
    `NsRecoveryCancel`/`NsInfo`/`Channels`/`Emoji{Add,Remove,List}` reference the ns by
    `NamespaceId`; `PART ns:<id>` alias updated; the **`NS-META` event** now carries `id:
    NamespaceId` + a `vanity=` tag; `NsMember`/`NsMemberInfo`/`Emoji`/`EmojiRemoved` events
    keyed by `NamespaceId` (`NsMemberInfo.roles` = role ids); federation (`Federate`/
    `BridgeRequest`) carries the peer's `VanityName` (resolved+pinned at handshake). Scope-
    string commands (`RoleCreate`/`GrantsAt`/`BridgePropose`/`ReportsList`/…) unchanged — the
    string just carries `ns:<id>` now.

  ### ✅ Phase 1 (proto) COMPLETE — the entire wire layer is done and green.

  - ◑ **Phase 2 (store) — started, additive foundation green.** Following the account
    0016/0017 two-step precedent, doing the *additive* half first (keeps store green +
    verifiable) before the coupled re-key.
    - ✅ **2-i namespace ids + vanity lock** (store green; contract passes mem + **live PG 16**):
      migration `0045_namespace_ulid.sql` (nullable `id` + UNIQUE, `vanity_locked BOOLEAN`);
      `NamespaceStore::namespace_id` (lazy per-read ULID backfill, like `account_ulid`),
      `namespace_by_id` (reverse), `vanity_locked`/`set_vanity_locked`; mem (`ns_ids` +
      `ns_vanity_locked`, cleared on delete) + PG (race-safe `UPDATE … WHERE id IS NULL
      RETURNING`) impls; contract assertions added. `NamespaceRecord` unchanged (no
      constructor churn). clippy + fmt clean.
    - ✅ **2-ii role ids** (store green; contract passes mem + **live PG 16**): migration
      `0046_role_ulid.sql` (nullable `id` + UNIQUE on `weft_roles`); `RoleStore::role_id`
      (lazy backfill, keyed by `(scope, name)`) + `role_by_id` (reverse → `(scope, RoleDef)`);
      mem (`role_ids` map, **carried across `rename_role`**, cleared on `delete_role`) + PG
      (race-safe backfill; id lives on the row so rename/delete need no extra handling);
      contract asserts lazy/stable/reverse + **identity survives rename**. clippy + fmt clean.
    - ⏭ **2-iii the re-key** (0017-equivalent, COUPLED with core): rewrite `ns:<name>`→
      `ns:<id>` across grants/epochs/invites/roles/moderation/nicks/ns_membership; then
      **channel ids** — `#name/chan`→`#<ns-id>/<chan-id>` across weft_channels/channel_hide/
      channel-scoped grants+roles+invites+epochs/layout/pins/events.channel + a per-ns
      channel-vanity resolver. This is the bulk + risk; lands with the core cutover.

### Model A cutover — IN PROGRESS (owner directive: full id rewrite, no name-resolution shortcut)

Owner directed the **full id model** (scopes `ns:<id>` / `#<ns-id>/<chan-id>` everywhere,
vanity display-only) — not the boundary resolve-to-name shortcut. Progress:

- ✅ **`NamespaceRecord.id` added + threaded through the store** (store green, contract mem +
  **live PG 16**): `id: String` on the record (empty ⇒ lazy backfill); mem uses `record.id`
  (dropped the redundant `ns_ids` map; `namespace_id` backfills via `get_mut`, `namespace_by_id`
  scans `record.id`); PG `create_namespace` binds id (empty→NULL), `namespace_from_row` reads it.
  The id ripple turned out **small — only 8 `NamespaceRecord` literals workspace-wide** (2 admin
  tests, 1 core `on_ns_create`, 3 store contract [fixed], 1 def, 1 PG mapper [fixed]).
- ✅ **session.rs NS command dispatch** converted to pass `ns: NamespaceId` / `vanity` (+ NsDelegate
  builds `ns:{ns}`).
- ✅ **`ns_meta_event` emits `id` + `vanity`** (record carries both).
- ✅ **`on_ns_create`** takes `vanity`, mints `ns_id`, seeds `@everyone` at the id-scope
  `ns:{ns_id}`, stores `record.id`.
- ✅ **Migration `0047_backfill_ns_role_ulids.sql`** — safe standalone `weft_gen_ulid()` backfill
  of NULL namespace + role ids (like the account 0017 backfill), so core always sees a valid id
  on read. Does NOT re-key scopes.
- ⏭ **Remaining core (the bulk, red until done):** 11 more NS handlers + `ns_admin_gate` →
  id-scopes + `namespace_by_id`; role/federation handlers → id.
  - **The deeper dependency surfaced:** the full-id model reaches **every ns-keyed store**.
    - ✅ **`EmojiStore` re-keyed to ns-id** (store green; contract mem + **live PG 16**; clippy+fmt
      clean): trait methods `set_emoji`/`remove_emoji`/`list_emoji` take `namespace: &str` (id);
      mem `emoji` map keyed by `String` (id); PG binds the id; migration `0048_emoji_by_ns_id.sql`
      re-keys existing rows (`e.namespace = n.name` → `n.id`, after 0047 backfills ids); contract
      keys emoji by an id string.
    - ⚠️ **`MembershipStore` (`ns_membership`) is COUPLED to channels, not separable.**
      `clear_ns_membership` clears hide overrides via `substring(channel from '#([^/]+)/') =
      namespace` — it matches the **channel name's ns-segment** against the ns key. That derivation
      only lines up once *channels* embed the ns-id (`#<ns-id>/…`). Re-keying `ns_membership` to
      ns-id **before** channels would break hide-clearing (id vs name mismatch). So membership
      re-keys **with the channel increment**, where the channel ns-segment becomes the id. (The
      backfill in `0035` uses the same `substring` derivation — same coupling.)
### The channel increment — execution-ready plan (the coupled body; do as a focused effort)

Not additively separable — the channel *name is the identity*, its format changes everywhere,
and it couples with core name-production (unverifiable until core lands). Facts gathered:
`ChannelRecord` has **no `name` field** (name is the key) and only **3 literals** (def + mem +
PG). `weft_channels` is name-PK. Channel-name/scope strings live in ~15 tables (list above).

**1. Store additions:**
- ✅ **channel-id foundation done + green** (contract mem + **live PG 16**, clippy+fmt clean):
  migration `0049_channel_ulid.sql` (nullable `chan_id` + UNIQUE + `weft_gen_ulid()` backfill);
  `ChannelStore::channel_id(name)` (lazy, race-safe like ns/role); mem `chan_ids` map (cleared on
  `delete_channel`) + PG backfill; contract asserts lazy/stable/unknown. **No `ChannelRecord`
  change** (id via method). Vanity deferred to the flip (below), where it's actually needed.
- ⏭ **vanity** (deferred to the flip): `weft_channels` + `vanity TEXT` (backfill = local segment),
  `channel_by_vanity(ns_id, local)→ChannelName` (post-flip: `WHERE substring(name from '#([^/]+)/')
  = ns_id AND vanity = local`).

**2. The name-flip re-key migration (0050, intricate — the risk):** temp map
`chan_map(old_name, new_name = '#'||n.id||'/'||weft_gen_ulid(), vanity)` joining `weft_channels`
to `weft_namespaces` on `substring(name from '#([^/]+)/') = n.name` (top-level `#chan` → `#'||chan_id`).
Then `UPDATE … SET <col> = m.new_name FROM chan_map m WHERE <col> = m.old_name` for **every**
channel-name/scope column in the ~15 tables. Same migration re-keys `ns_membership.namespace`
name→id and rewrites `ns:<name>`→`ns:<id>` in grants/epochs/invites/roles/moderation/nicks.

**3. `ns_membership` re-key (rides here):** its methods take ns-id; `clear_ns_membership`'s hide
match (`substring(channel from '#([^/]+)/') = ns_id`) now lines up because channel names embed
the id. Update `0035`'s derivation expectations accordingly in the impls.

**4. Core (the bulk):** produce `#<ns-id>/<chan-id>` everywhere (channel create mints chan-id +
resolves ns-id; the registry keys by id-name); resolve `#nsvanity/chanvanity` → id-name at the
wire boundary (IRC gateway + `JOIN`); all `ns:{name}` scope builders → `ns:{id}`; the 11 NS
handlers + role/federation handlers; core tests + 2 admin `NamespaceRecord` literals.

Everything is teed up: all store id foundations are green + PG-validated, the `weft_gen_ulid()` +
temp-map migration pattern is proven (0017/0047/0048), and the membership coupling is understood.

### Core cutover — breakage assessment (key finding: channels are SEPARABLE)

`cargo build -p weft-core` = **63 errors, ALL namespace + role** field/id resolution (NsCreate
`name`→`vanity`; `NsMeta`/`NsVisibility`/`NsDelete`/`NsJoin`/`NsLeave`/`NsTransfer`/`NsRecover*`/
`NsInfo`/`NsDelegate` `name`→`ns: NamespaceId`; `RoleDelete`/`Assign`/`Unassign` `name`→`role:
RoleId`; NS-META event `id`+`vanity`; NsMember/NsMemberInfo `ns`). **Zero channel errors** —
`ChannelName` stayed a `#seg/seg` string in proto (accessors added, type unchanged), so core's
channel handling still compiles.

⇒ **A "core-green with namespace + role ids, channels still `#vanity/chan`" milestone is
achievable first**, deferring the channel-id change (the biggest, riskiest piece) to its own
increment. Recommended next-session plan:
1. **Core resolution helpers:** at each NS handler, `namespace_by_id(ns)` → record (use
   `record.name` for internal name-scopes for now — Model B); `NsCreate` mints a namespace with
   the vanity + lazy id; NS-META emission carries `namespace_id(name)` + vanity. Role handlers:
   `role_by_id(role)` → `(scope, name)` → existing name-keyed methods. **~63 lib sites + ~186
   test updates** (tests build old command shapes) — no green checkpoint until all done.
2. Then **2-iii internal re-key** (name-scopes → id-scopes for true rename-safety) — the 0017
   migration + switching core scope production to `ns:<id>`.
3. Then **channel ids** — the `#<ns-id>/<chan-id>` change + channel-vanity resolver (largest).

**Why not crammed here:** core compilation is all-or-nothing (no partial verification); these are
security-critical enforcement paths; ~150 edits with no green checkpoint mid-way. Best as a
focused run, starting from this assessment.

### Phase 2 migration strategy (from the account name→ULID precedent, 0016/0017)

The codebase already migrated *accounts* name→ULID and that is the exact template:
- **Add a nullable `id TEXT` column** (+ UNIQUE index), NOT a new PK yet — Postgres UNIQUE
  permits many NULLs, so legacy rows coexist. New namespaces/roles set `id` at creation.
- **Mint ULIDs in SQL** for the one-shot backfill via a temp `weft_gen_ulid()` plpgsql
  function (26-char Crockford, first char 0-7); drop it after. (`0017` did this.)
- **Lazy per-read backfill** in the Rust store (like `PostgresStore::account_ulid`) for any
  row still NULL — race-safe `UPDATE … WHERE id IS NULL RETURNING`.
- **Re-key dependent scope strings in the same migration** (0017 rewrote `grants.subject`):
  `UPDATE weft_grants SET scope = 'ns:'||n.id FROM weft_namespaces n WHERE scope = 'ns:'||n.name`,
  and likewise `weft_epochs`, `weft_invites`, `weft_roles.scope`, `weft_ns_membership.namespace`.
- **Vanity + lock:** the existing `name` column *becomes* the vanity (already UNIQUE = the
  resolver); add `vanity_locked BOOLEAN DEFAULT false`. `resolve_vanity(name)` = `SELECT id
  WHERE name=$1`; `set_vanity`/`lock_vanity` update it (refused if locked, operator override).

**The hard part — channels get their own ULID too:** `#<name>/<chan>` → `#<ns-id>/<chan-id>`
means minting a `chan-id` per channel AND rewriting the channel-name string **everywhere** it
appears — `weft_channels`, `weft_channel_hide`, channel-scoped `weft_grants`/`weft_roles`/
`weft_invites`/`weft_epochs`, `weft_channel_layout`, `weft_pins`, and the `weft_events.channel`
column — plus a per-namespace channel-vanity resolver (unique local names). This is the bulk of
Phase 2's risk and line-count.

**Coupling caveat:** the re-key UPDATEs and the Rust store/core changes must land **together** —
once grants read `ns:<id>`, core must produce `ns:<id>`. So Phase 2 (store) and Phase 3 (core)
are effectively one deployable unit; the workspace stays red across both until the end.

**Store surface changes:** `NamespaceRecord`+`id: NamespaceId`, `RoleDef`+`id: RoleId`;
`NamespaceStore`/`RoleStore` lookups re-keyed to ids + `resolve_vanity`/`set_vanity`/
`lock_vanity`/`namespace_by_id`; mem mirrors (HashMap by id + vanity index + lock set); both
backends + shared contract, PG-validated. (Crypto unchanged — `TokenScope` string carries the id.)

### Crypto finding (scope-shrinking)

`TokenScope::Namespace(String)`/`Channel(String)` + string-segment `covers`/`channel_namespace`
mean the signed scope string just carries a ULID instead of a name — **weft-crypto needs no
structural change**, and no token VERSION bump is strictly required (old name-scope tokens
become inert since nothing is named `ns:<name>` anymore; grants re-key in the store migration
and tokens re-mint on next GRANT/auth). The cutover is proto → store → core.

## 4. Phases (each independently shippable)

1. **proto** (`weft-proto`) — a `NamespaceId`/`RoleId` ULID type; `#<ns-ulid>/<local>`
   channel parse/serialize (`name.rs`); ROLE* commands (`command.rs:226-274`) re-keyed to
   role ULID (`RoleCreate` returns an id; `RoleDelete/Assign/Unassign/Reorder` take ids;
   `RoleRename` collapses to a label-set); `NsCreate` returns a ULID; NS-META gains a
   `vanity` field; `Role`/`RoleMember`/`NsMember*` events carry ids + vanity. **Round-trip
   tests first** (workspace rule). ~all of §5's command/event list.
2. **store + migrations** (`weft-store`) — `id UUID/TEXT` PK on `weft_namespaces` +
   `weft_roles` (`0004`, `0012`), vanity column + a `weft_vanity` directory table with
   `admin_locked`; re-key `weft_role_assignments`, `weft_ns_membership`, grants + epochs,
   invites, manifests from `ns:<name>`/`#name/…` to `ns:<ulid>`/`#<ulid>/…`; backfill
   ULIDs for existing rows and rewrite existing channel names. New `NamespaceStore`
   lookups by id + `resolve_vanity`/`set_vanity`/`lock_vanity`. Mem + PG shared contract;
   PG-validated. (Mirror `0016`/`0017`.)
3. **crypto** (`weft-crypto`) — `TokenScope::Namespace(Ulid)` + ULID channel scope; token
   `VERSION → 3`; `covers` by ULID equality; `channel_namespace` returns the ULID segment.
4. **core** (`weft-core`) — resolve vanity→ULID at the wire boundary; every scope builder
   in §5 emits `ns:<ulid>`; channel registry/actors keyed by `#<ulid>/…`; DISCOVER/invite/
   federation emit vanity + pin ULID; role handlers by role id; vanity rename + lock
   enforcement (operator-gated). Audit the ~30 scope-construction sites listed in §5.
5. **client** (`weft-client-core` + Svelte) — display vanity everywhere, carry ULIDs under
   the hood; role editors keyed by role id; a **vanity-name** field in server settings;
   `network/vanity` link parse (`weft-client-core/src/lib.rs:2040`) resolves to ULID.
6. **web admin** (`weft-admin`) — vanity-lock management UI: list/set/clear locked
   vanity↔namespace bindings (store-direct), operator-gated.

## 5. Open questions (for confirmation before Phase 1)

- **Spec/version:** bump the spec to v0.13 and token `VERSION → 3`; keep ALPN `weft/1`
  (same as the account-ULID move — no ALPN bump). Confirm.
- **Vanity uniqueness:** per-network (one server = one network) unique. Confirm.
- **Migration of live tokens:** re-mint on next auth (old-scope tokens hard-denied), as
  with accounts. Confirm.
- ✅ **Channels get their own ULID** too (`#<ns-ulid>/<chan-ulid>`); channel vanity names
  are unique within a namespace; IRC gateway + clients address by `#vanity/name` and
  resolve (owner, 2026-07-27, §3).
- ✅ **Token cutover:** re-mint on next auth, hard-deny old (VERSION 2→3), like accounts.
- ✅ **Start with Phase 1 (proto).**

## 6. Acceptance tests (invariants-as-tests)

- Rename a namespace's vanity → tokens, grants, channels, roles, and a live federation
  bridge all keep working (identity is the ULID; nothing re-signs).
- A locked vanity can't be registered or changed by a non-admin; an operator can set/clear
  it in the admin panel; a locked-but-unbound vanity blocks `NS CREATE` of that name.
- Migration round-trip: a pre-upgrade fixture (namespace + channels + grants + role
  assignments + invites) resolves identically post-upgrade, addressed by the new ULIDs.
- Token `VERSION` cutover: a v2 (name-scope) token is rejected; a re-minted v3 token works.
