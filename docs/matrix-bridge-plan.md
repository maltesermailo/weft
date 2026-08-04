# Matrix Bridge — Build Plan & State Tracker

**Goal:** the Matrix bridge fully running at the **Element-parity bar** (owner, 2026-08-03): a user
opens a Matrix space in the WEFT client, chats both ways, and *manages* it — invite/kick/ban, room
settings, space structure, **power levels instead of roles** — via bridge-supplied plugin UI.

**Model:** the unified provider model (`plugin-spec.md` §18) — the bridge is a `remote` plugin using
all six provider capabilities + the `bridge` feature. Content rides the foreign-bridge machinery
(`foreign-bridge-framework.md`); management rides the plugin SDUI stack (`plugin-spec.md`).

**Legend:** `[x]` done · `[~]` in progress · `[ ]` open. Sizes S/M/L. Keep this file current as
slices land.

---

## Already landed (pre-plan)

- [x] Foreign-bridge slice 1 — `Scheme`/`ForeignUri` addressing types (proto)
- [x] Foreign-bridge slice 2 — bridge-context verbs (`REALM REGISTER/ASSERT/WITHDRAW`,
      `PROVISION`/`PROVISION-OK|ERR`)
- [x] Foreign-bridge slice 3 — `AUTH ADAPTER` + `State::ForeignBridge` + `[[foreign_bridge]]` config
- [x] Foreign-bridge slice 4 — provisioning plumbing + failure path (`NS JOIN <uri>` → park →
      `PROVISION` → `PROVISION-ERR` → `NO-SUCH-TARGET`)
- [x] Foreign-bridge slice 5 — store foundation: `NamespaceRecord.origin` marker +
      `namespace_by_origin` (mem+PG, migration 0052) + known-local join branch
- [x] M-plug-0 — `weft-plugin` + `weft-appservice` crate skeletons, `[[plugin.remote]]` config slot
- [x] M-plug-1 — L0 SDUI codec (components/View/PatchOp/ViewResult/ActionDecl/Catalog/Registration)
      + `PLUGIN*` commands + `PLUGIN-*` events, round-trip tested
- [x] M-plug-2a — weftd remote transport: `State::PluginService`, `AUTH ADAPTER` → plugin session,
      `PLUGIN-REGISTER` → catalog, `PLUGINS` → manifest, `PLUGIN INVOKE` → route → relay
- [x] Design: foreign display + capability profile (`foreign-bridge-framework.md` §7a); provider
      unification (`plugin-spec.md` §18)

---

## Phase 0 — sign-off (no code)

- [x] **0. Materialization decisions — SIGNED OFF (owner, 2026-08-04):** owner = reserved suspended
      **sentinel account** · `root_key = ""` · the single **owner-shortcut gate** on
      `origin.is_some()` (context.rs cap check).

## Phase 1 — server content path (weftd/core; mock-provider-testable, no daemon needed)

- [x] **1. Foreign-display proto fields** (S) — DONE 2026-08-04. `foreign: Option<String>` on
      `MESSAGE`/`MEMBER`/`REACTION`/`EDITED`; `origin: Option<ForeignUri>` on `NS-META`/
      `CHANNEL-LAYOUT` (DISCOVER lists via NS-META → covered). Round-trip + `foreign=`-tag tests;
      `ns_meta_event` already wires `record.origin` → the badge flows the moment a replica exists.
      Native emit sites pass `None`. Workspace green (proto 134, core 204, conformance 39), clippy
      clean. *Deferred into slices 3/4: client-core `ClientEvent` pass-through + client badge UI
      (lands when real values exist); channel-level `origin` on layout rows (materialization).*
- [x] **2. Provider-session unification** (M) — DONE 2026-08-04. `State::ForeignBridge` FOLDED into
      `State::PluginService { key, plugin_id, realm }` (one session speaks bridge verbs AND plugin
      protocol; a `[[foreign_bridge]]` pin's provider id = its scheme name). One **provider registry**
      (`foreign_control` merged away; schemes live on the registry entry, union of `REALM REGISTER` +
      `Registration.schemes`). `Registration.schemes` + `[[plugin.remote]] schemes` config →
      `PROVISION` routing via `provider_for_scheme`; unauthorized scheme fails the whole registration.
      Welcome feature is now uniformly `"plugin"`. Tests: scheme-registration routes PROVISION (the
      Instagram case) + unauthorized-scheme refused. Workspace green (core 206), clippy clean.
- [x] **3. Materialization success path** (M–L) — DONE 2026-08-04. **`NS JOIN <uri>` succeeds.**
      Providers assert structure with **normal verbs on URI targets** (owner call: no bespoke
      assert verb) — `NS-META <uri> <vis>` / `CHANNEL-LAYOUT <uri> <pos>` parse to foreign-assertion
      variants (the `NS JOIN` routing precedent); weftd mints ids, owner = suspended sentinel
      (`foreign`), `root_key=""`, replies with the minted badged mapping. `PROVISION-OK <job>` stays
      bare: resolves the pending URI by origin → ns membership → parked client gets NS-META +
      CHANNEL-LAYOUT + labeled NS-MEMBER; missing assert = loud provider-bug failure. Authority:
      the owner-shortcut cap gate + NS-LEAVE + RECOVERY-SET all origin-gated. `ChannelRecord.origin`
      (migration 0053, `set_channel_origin`) flows into every layout emission (SYNC/CHANNELS/acks).
      Tests: the full vertical (join → PROVISION → assert ns+channel → OK → badged ack, known-local
      second joiner, cap-required on NS META) + store contract. Workspace green (core 208, proto
      135), clippy clean. *Deferred: `POLICY <uri>` assertion (channels default `retained:90d`);
      structural update-sync on re-assert (currently idempotent mapping re-send); DISCOVER of a
      `public` replica is inherited free via `list_public` (untested).*
- [~] **3b. Provider lifecycle & GC** (S–M, from the 2026-08-04 failure-path audit) —
      - [x] **(e) Provider liveness gating + indicator (owner directive 2026-08-04):** a virtual
            namespace is **online only while its provider is** — offline ⇒ excluded from DISCOVER
            + `NS JOIN` refused (`NO-SUCH-TARGET`, uniform); members get live `NS-META`
            `provider=online|offline` pushes on provider connect/disconnect (`push_provider_state`
            via directory notify). Wire: `NsMeta.provider_online` (`provider=` tag);
            `ctx.origin_online`/`scheme_online`; `ns_meta_event` is now a `self` method carrying
            live state everywhere (SYNC/DISCOVER/acks). `namespaces_with_origin` store method
            (mem+PG). client-core `ClientEvent::NsMeta` passes `origin` + `provider_online`
            through. Full-cycle test (online→discover/join ✓ → death→offline push + join/discover
            refused → reconnect→online push + join ✓). *Svelte badge UI = the client display slice.*
      - [x] (a) **operator escape hatch** — DONE 2026-08-04: operators hold every cap in an
            `origin` namespace (ctx cap-check branch; "operator ≠ user-server admin" doesn't apply —
            no user owns a replica). `NS DELETE` works over the wire; cascade extracted into
            `delete_namespace_cascade` + `deletion_tombstone` helpers.
      - [x] (b) **`REALM WITHDRAW` real semantics** — DONE 2026-08-04: withdraw = full deletion
            cascade of the bound realm's namespaces + tombstone pushed to every member (distinct
            from disconnect = offline). Bonus fix the test caught: **`REALM ASSERT` now registers
            the scheme** (a bound data connection is definitionally serving → liveness + PROVISION
            routing), with the same dup-guard.
      - [x] (c) **deterministic scheme claims** — DONE 2026-08-04: first registrant holds a scheme;
            a second claimant is refused loudly (`CONFLICT`; close on PLUGIN-REGISTER, err on
            REALM REGISTER/ASSERT).
      - [x] (d) **quota exclusion** — DONE 2026-08-04: `namespaces_owned` skips `origin` rows
            (mem + PG + contract).
      Tests: operator-delete (member refused / operator tombstones / gone), withdraw-tombstone,
      duplicate-claim refusal. 37 suites green, clippy clean.
      *Offline-relay queueing stays an explicit slice-5 design decision.*
- [x] **4. Ingestion** (M) — **DONE 2026-08-04: MSG + EDIT/DELETE/REACT/UNREACT + MEMBER join/part.**
      The mutation verbs name their target by **msgid** (not channel), resolved via
      `events.find_root` → `Scope::Channel`, then applied through the §11.13 home-authoritative
      `relay_mutate_as` (`edit`/`delete`/`react-add`/`react-remove`) carrying `foreign=`. Authorship is
      the provider's to assert — it owns the room. Test covers all four + both puppet shapes.
      An `@as=<foreign-identity>` line on a provider session is ingestion (routed on the tag, before
      the bridge verbs). **Addressing:** the provider uses the **canonical channel name it learned**
      from the `CHANNEL-LAYOUT` mapping reply — so it's an ordinary `MSG`, no URI-target parsing
      (`Target` can't hold a `://` URI; discovered by the test). weftd verifies the target is an
      `origin`-marked replica whose **scheme the provider's key is pinned for** (a native channel or
      another provider's replica is refused `UNSUPPORTED`), then mints via
      `relay_publish_as` — home-authoritative (invariant 2), attributed to a **federated-looking
      puppet** `UserRef` (owner directive 2026-08-04, framework §7a.0 — amends decision 1 for users):
      `puppet_user` = identity localpart @ its own domain (`alice@matrix.org`), falling back to the
      room's realm for a bare handle (`bob@acme-corp`), with the exact native handle in `foreign=`
      (§7a.1). `Cmd::RelayPublish`/
      `RelayMutate` gained a `foreign` field (+ `relay_publish_as`/`relay_mutate_as`); the old
      no-foreign methods delegate. Unmirrored channel ⇒ silent drop (normal for a provider).
      Test: ingest → member sees `MESSAGE` with our-network msgid + `foreign=@alice:…` + puppet
      sender; unmirrored drop proven by a FIFO barrier. *Remaining in slice 4: EDIT/DELETE/REACT via
      `relay_mutate_as` (the field is already threaded), MEMBER (foreign roster), and `POLICY`.*
- [ ] **4b. Federated-looking-puppet follow-ups** (S, from the §7a.0 amendment) — a replica user now
      *looks* federated but has **no peer bridge**, so guard the paths that assume otherwise:
      `FEDERATE`/auto-bridge dialing toward a replica's "network", and name-keyed `NETBLOCK`
      semantics for a realm-as-network. (DM routing is now slice 4d, a feature not a guard.)
- [x] **4c. Foreign namespace membership** (M) — DONE 2026-08-04. **Matrix users can join WEFT
      namespaces.** Six `MembershipStore` ns-methods now take/return a **member key string** (bare =
      local `ada`, `user@network` = bridged) — **no data migration** (the column was always free
      text). `weft_store::local_member(key) -> Option<Account>` is the one convention helper; hide
      overrides deliberately stayed on `Account` (a bridged member has no client hiding tiles).
      `channel_roster` now returns `Vec<UserRef>` — local members on our network **plus bridged
      members on their own** (their provider's roster is authoritative, so local hide/view-cap
      filters don't apply); MEMBERS renders both, and a bridged member always reads offline (presence
      is same-network-only, §6.1). ~28 call sites adapted: per-session pushes/role-grants/admin-panel
      role controls filter local; counts, NS INFO MEMBERS, and the delete cascade include everyone.
      Provider `MEMBER` ingestion writes/clears those rows (completing slice 4). Store contract test
      covers the mixed local+foreign table. *(Original scoping said "migration: yes" — wrong.)*
- [x] **4d. DM a bridged (foreign) user** — **DONE 2026-08-04** (L0 half shipped earlier; the
      routing half landed now that slice 5 exists). **DECIDED (owner): true 1:1 DMs — the group
      infrastructure is NOT reused.**
      * **Store:** `Scope::Dm(Account, Account)` → `Scope::Dm(String, String)` **member keys**, the
        same 4c convention (bare = ours, `user@network` = foreign). `dm_partners` widened to
        `&str -> Vec<String>`. The `dm:<a>:<b>` storage key stays unambiguous because a member key
        never contains `:`. New `weft_store::member_key(user, home)` — the inverse of `local_member`.
      * **Directory:** the DM peer is a `UserRef` throughout (`dm`/`edit`/`delete`/`react`), with
        `local_peer` / `dm_scope` / `dm_target` / `deliver_dm` deriving the local-vs-foreign
        behaviour. Existence is checked only for a **local** recipient — a foreign handle lives on
        their own network and the far side refuses it.
      * **Outbound:** `relay_foreign_dm` routes by network — a **bridged** realm goes to its provider
        (`ctx.provider_for_realm`) as `@as=<our user> MSG @<peer>`, anything else takes the ordinary
        peer bridge (`request_friend_deliver`, the route friends/group DMs already use). Stored and
        echoed locally either way.
      * **Inbound:** `@as=<their user>;msgid=<realm>/<ulid> MSG @<our account>` → `Cmd::DmIngest` →
        stored under the same `Scope::Dm` **preserving the realm's msgid** (invariant 2) and
        delivered to the local user. A bridged conversation is a first-class DM, not a second table —
        `HISTORY @alice@acme-corp` serves both directions interleaved.
      * Test: outbound echo + provider copy, inbound delivery, and one interleaved `HISTORY`.
        **Flake caught + fixed:** the two msgids come from independent generators, so inside one
        millisecond ULID order is decided by random bits (4/10 failures). The test now stamps the
        inbound id 1 ms after the outbound one — 10/10.
      *Remaining for a later slice:* DM **mutations** (edit/delete/react) on a bridged conversation
      still apply locally only — `MessageRoute::Dm` carries the `UserRef` now, so the relay hook is
      the same shape as the channel one (§7a.0b), but it is not wired.
- [x] **4e. Multi-origin event ordering** (S, bug found 2026-08-04) — `materialize` and
      `compaction_plan` sorted a root's children by **`MsgId`**, whose `Ord` is `(origin, ulid)`.
      In a **multi-origin** scope that resolves "which edit is final" / "does this reaction end
      net-added" by *origin name*: an older `acme-corp` edit beat a newer `test.example` one on
      every read, and compaction then deleted the winner. Now ordered by ULID (`event_order`, with
      the full msgid only as a tie-break).
      *Why it was dormant:* since the home-authoritative pivot a peer-federated channel is
      **single-origin** (the home mints everything), as are groups. The only multi-origin scopes are
      the two bridge-touching ones — a **replica channel** and a **DM with a foreign peer** — so this
      went live exactly when bridges did. Not a compaction-only bug: `materialize` runs on every
      `HISTORY`, so it was wrong on reads; compaction only made it permanent.
- [x] **4f. Bridge backfill — §11.7 for realms** (owner directive 2026-08-04: "bridges should do the
      same as federation") — **DONE.** *Correcting an earlier mis-framing of mine:* federation does
      **not** fetch state on read. It ingests + **stores** live traffic, serves `HISTORY` locally, and
      pulls a deeper window from the peer **only when the local page ran out**, deduped per
      `(channel, before)`. Replicas already matched on the storing half; what they lacked was that
      second leg (`on_backfill_demand` is gated on `State::Bridge`, so it only ever talks to a peer).
      `request_provider_backfill` now asks the **realm** for the window when a client scrolls past
      what we hold. The realm answers by replaying it as ordinary `@as` ingestion — already
      origin-checked — so there is **no separate backfill ingress** and no way to smuggle events in
      under cover of an answer. Deduped via the session's existing `backfilled` set.
      Tested: short page → realm asked; replayed older message appears in the next page, correctly
      interleaved; a repeated scroll is deduped while a genuinely new window is asked for.
- [x] **5. Outbound relay** (M) — **DONE 2026-08-04. WEFT → Matrix flows.**
      `sync_provider_forwarders(schemes)` subscribes the provider session to every replica channel of
      its namespaces (reusing `spawn_forwarder`/`self.bridged`), wired at all three registration
      points (`PLUGIN-REGISTER`, `REALM REGISTER`, `REALM ASSERT`) **and** on each newly asserted
      channel. `on_provider_event` forwards local-origin message-plane events verbatim; the provider
      maps channel → foreign room via the mapping it learned at assert time.
      **Loop guard** = the peer-bridge rule, unchanged: forward iff `msgid.origin == our network`.
      That works because of the identity pivot below — a replica is *multi-origin*, so an ingested
      event carries the realm's origin and is structurally ineligible to go back.
      Test: local MSG/EDIT/REACT relay outward, then an ingested post is proven *not* to come back
      (the next line the provider reads is the local DELETE that followed it).
      **JOIN/PART relay (owner directive 2026-08-04 — REVISED TWICE the same day):** membership is
      **namespace-level; channels are not joinable**, and **a bridge behaves as a federation peer**,
      so the two directions are federation's and are not interchangeable:
      `weftd → realm  @as=<local user> NS JOIN|NS LEAVE <ns-id>` — a *request*; and
      `realm → weftd  NS-MEMBER <ns-id> <user> join|part` — the authority *stating* membership.
      weftd never asserts membership of a foreign space (my first two attempts did: one MEMBER per
      channel, then an NS-MEMBER event sent *to* the realm — both had weftd claiming authority over
      someone else's realm, and the second also inverted the event/command direction).
      The inbound `NS-MEMBER` writes the membership row (keyed `user@realm` for a foreign member, the
      bare account for a local one) and weftd tells its own members with a channel `MEMBER` — local
      delivery only. The outbound request goes down the provider's writer (the `PROVISION` route)
      because the ns subscription is deliberately silent at channel level, leaving the ordinary event
      relay nothing to carry.
- [x] **5e. Membership resync — the realm re-states** (owner decision 2026-08-04: full-replace, and
      *drop* the "let the provider read our roster and diff" idea rather than ship both) — framework
      §7a.0c. The realm re-states its membership inside the **ordinary `SYNC` snapshot framing**
      (spec §6.9, the one a client gets on login), roles swapped: `SYNC START` → `NS-MEMBER …` × N →
      `@cursor=<opaque> SYNC END`, at which point every member of that provider's namespaces it did
      not name is dropped. **Zero proto changes** — `SYNC START`/`SYNC END`/`NS-MEMBER` all existed.
      *Why full-replace:* the adapter already holds the whole set (Matrix room state), so diffing
      would make it *also* track what WEFT believes; and read-modify-write across the link has a
      stale-read race that can part a user who joined meanwhile. Full-replace is idempotent and
      self-healing. `SYNC START` is the safety: an unopened `SYNC END` names nobody, so it is ignored
      rather than obeyed (it would otherwise wipe the namespace) — tested.
- [x] **5d. Authority translation — WEFT mods ↔ Matrix power levels** (owner directive 2026-08-04:
      "it's important that WEFT users can be made mods on Matrix spaces and vice versa") — bidirectional,
      with **weftd carrying no notion of a power level**: it speaks capabilities and the adapter owns
      the mapping, exactly as it owns identity (§7a.0). Framework §7a.3b (new); §7a.4's "advisory,
      read-only" level is amended.
      * **Inbound**: the provider sends ordinary `GRANT <user@realm> ns:<id> <caps>` / `REVOKE` on its
        session. Authority = the ingestion rule: the scope must name a namespace whose scheme its key
        is pinned for; no capability chain (the provider *is* the governing authority, §7a.3).
      * **Inbound enforcement**: a foreign moderator's `MUTE`/`BAN`/`KICK` arrives as an `@as` line and
        runs through the **ordinary actor-aware handler** as `Actor::Foreign`, checked against those
        grants — so a foreign user without a grant is refused like a local one. This is what makes the
        inbound grant real rather than decorative; without it nothing a foreign user does reaches a
        WEFT authority check at all.
      * **Outbound**: a local `GRANT`/`REVOKE` at a replica's `ns:` scope relays to the provider.
      * `@everyone`/role-derived authority is **not** relayed — only explicit grants (relaying a
        baseline every member holds would mean "give everyone level 50").
- [x] **5a. A realm IS a network — the adapter mints identity + msgids** (M, owner directive
      2026-08-04) — **DONE.** Replaces the earlier "weftd mints, `foreign=` displays" model.
      * `@as=<user@realm>` now carries the finished WEFT handle; weftd validates
        `sender.network == realm` instead of deriving a puppet (`puppet_user` deleted).
      * The adapter mints msgids: `MSG`/`EDIT` carry `@msgid=<realm>/<ulid>`; `DELETE`/`REACT` get a
        local bookkeeping id, exactly as on the peer path.
      * Ingestion **converged onto the federated path** — `federation::ingest_record` + `Cmd::Ingest`,
        replacing `RelayPublish`/`RelayMutate` (whose now-redundant `_as` variants were collapsed).
      * **`foreign=` removed** from `MESSAGE`/`MEMBER`/`EDITED`/`DELETED`/`REACTION` (framework §7a.1
        struck).
      * **Account grammar widened to `[a-z0-9-_.=+]`** (spec §2.3 amended + Appendix A decision (3))
        so a Matrix localpart survives verbatim. *This was the real motivation:* the old mapping
        stripped `.`/`=`/`+`, so `@alice.smith` and `@alicesmith` collided onto one WEFT identity.
        `/` stays excluded — it is WEFT's own path separator — so adapters escape that one.
      * Authority: `@as` and `@msgid` must both name the provider's realm, so a provider cannot forge
        a local account or another realm's event (tested).
- [x] **5b. Posting while the provider is offline — REFUSE** (owner decision 2026-08-04) — a post
      into a replica channel while its provider is down answers `ERR POLICY provider-offline` rather
      than being accepted-and-dropped. Accepting it would split-brain the room: local members would
      see a message the foreign side never receives, with no route out and nothing to reconcile
      against later. Implemented in `can_post` (the §6.7 posting gate, which already loads the
      channel record — so the origin check is free), mirroring the same rule already enforced on
      `NS JOIN`. Tested in `provider_offline_gates_virtual_namespace`.
      **Extended to every write (owner directive 2026-08-04: "Matrix is authoritative for its own
      spaces"):** `EDIT`/`DELETE`/`REACT`/`UNREACT` are gated too, in `resolve_message`'s channel arm
      — the single choke point all three verbs already share. Both gates call one predicate,
      `origin_offline(origin)`. Tested per verb, asserting the `provider-offline` **context** so the
      test proves *this* rule refused and not some other `POLICY` sharing the code.
      *Note:* operator/admin delete (`SystemDelete`, the admin panel) is deliberately **not** gated —
      it is the moderation/legal-removal path and must work whether or not a bridge is up.
- [x] **5c. Mutating a BRIDGED message — relay to the provider** (owner directive 2026-08-04) —
      the flip side the offline gate exposed: a local user could not react to a Matrix-originated
      message *at all*, because `resolve_message` refuses a foreign-origin msgid with `FORBIDDEN
      origin` (invariant 2). Correct for a native channel, wrong for a replica, where multi-origin is
      the normal case. New `MessageRoute::ChannelProvider` — when the msgid's origin is the channel's
      **realm**, the mutation relays to the provider as `@as=<local user> REACT|DELETE|EDIT …`
      (framework §7a.0b); the adapter performs it foreign-side and its event returns via ingestion.
      A foreign-origin msgid that does *not* match the channel's realm still gets `FORBIDDEN origin`.
      **Also fixed here:** authorship compared only the bare account, so a local `alice` could edit
      `alice@matrix.org`'s message — now the whole `user@network` is compared. Tested: ada's REACT
      relays, her EDIT of someone else's message is still `CAP-REQUIRED`, an operator's DELETE relays.

*Parallelism: 1 ‖ 2. After Phase 1 a mock provider gives a fully chatting Matrix-shaped namespace.*

## Phase 2 — SDK + management surface

- [ ] **6. M-plug-2b — `weft-appservice` dispatch loop** (M) — connect, `AUTH ADAPTER` handshake,
      `PLUGIN-REGISTER`, routed-invoke → handler → `PLUGIN-RESULT`, async `Ctx`; the `bridge`-feature
      verb helpers (realm/assert/ingest); two-live-endpoint QUIC conformance test. **The daemon can
      now be written.**
- [ ] **7. M-plug-3 — flows + client SDUI renderer** (L, mostly client) — `SUBMIT`/`ACTION` routing
      (weftd) + the client renderer: modals, forms, full component catalog, context-menu + global
      surfaces. **Element's dialogs.**
- [ ] **8. Capability-profile slice** (S–M) — `authority=roles|levels|none` + `settings=<disabled>`
      on `NS-META`; `level=` on `MEMBER`/roster; client gating (hide native roles editor, show
      levels). *(framework §7a.3–7a.4)*
- [ ] **9. M-plug-6 subset — settings surfaces + live panels** (M) — `settings` surface actions,
      panels + `SUBSCRIBE`/`PLUGIN-PATCH`, `server-menu` + `channel-list` surfaces. **Where "Room
      Settings" / "Power Levels" live.**

## Phase 3 — the bridge itself

- [ ] **10. Matrix daemon MVP** (L, external, on the SDK) — companion HS / AS-API, provisioning
      (resolve + join + enumerate a space), bidirectional message sync, structure assertion.
      *(Can start in parallel with #7 once #6 lands.)*
      **Outbound projection (owner directive 2026-08-04):** the daemon models WEFT namespaces **as
      Matrix Spaces** on the companion homeserver (`matrix.md` §3–16 — already designed), so Matrix
      users join the Space/rooms natively and their participation arrives as slice-4c membership +
      slice-4 ingestion. Pure daemon work: no weftd change beyond 4c/4d.
- [ ] **11. Management actions in the daemon** — invite/kick/ban/create-room/create-subspace/
      room-settings/power-level actions as SDUI flows; profile supplied via `NS-META`.
- [ ] **12. Track B — widgets + client-Rhai + CSP** (L) — the rich PL-matrix editor as a sandboxed
      widget. **Polish, not gating**: SDUI tables/forms carry the v1 levels view.

---

## Admin panel additions (weft-admin — after the phases above)

- [ ] **Namespace management page**: list ALL namespaces (store-direct `list_all`) with
      visibility, member count, and **origin/provider badges** (foreign URI + live/offline state);
      **DELETE any namespace** — the operator UI form of the 3b-a escape hatch, reusing the same
      cascade (incl. orphaned virtual namespaces whose provider is long gone).
- [ ] **Provider status page**: the `[[plugin.remote]]` / `[[foreign_bridge]]` pins with their
      connection state (online/offline, registered schemes, declared actions), and each provider's
      virtual namespaces.
- [ ] Plugin lifecycle controls (enable/disable pins, view quarantine/refusal log) — overlaps
      plugin-spec M-plug-13; reconcile when that lands.

## Explicitly deferred (not on the Matrix path)

- Hooks (M-plug-4/5) — the bridge gets its data via relay, not hooks
- In-process Rhai/WASM runtimes (Tracks C/D)
- Media-mirroring polish (§11.8 beyond policy negotiation)
- Per-device attestations on bridged events
- Read receipts / Matrix presence / typing fidelity (honest limits, framework §7a.6)

## Cross-references

- `docs/architecture/plugin-spec.md` — the plugin/provider spec (§18 = unification)
- `docs/architecture/foreign-bridge-framework.md` — realm machinery + §7a display/profile
- `docs/architecture/matrix.md` — the Matrix adapter binding (to be updated against the SDK)
- `docs/architecture/plugin-system.md` — design rationale / decision history
