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
      - [x] (a) ~~operator escape hatch~~ **REVERSED 2026-08-04 (owner directive).** Operator/admin
            power lives in a **separate permission table** and acts **only through the web admin
            panel**; `*` confers nothing inside a namespace, only `ns-admin` does, replica included.
            The hatch is removed — it was redundant as well as wrong, since the panel already deletes
            namespaces store-direct (`DELETE /api/v1/namespaces/:name`, `AdminScope::Destroy`).
            Authority in a replica is whatever **the realm grants**. The `delete_namespace_cascade` +
            `deletion_tombstone` helpers stay.
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
- [x] **4b. Realm-as-network guards** (S, from the §7a.0 amendment) — **DONE 2026-08-04.** A replica
      user *looks* federated but has no peer bridge, so the paths that assume otherwise are guarded.
      Scoping this turned up a **sharper issue than the one the slice named**:
      * **Identity-space collision (the real find — not namespaces, which are ULID-identified and
        network-pinned).** "A realm is a network" puts realm names in the same **`user@network`**
        space. A realm `hda.example` mints `alice@hda.example` — the same grant subject, member key
        and DM scope as that network's own user — and since 4d's DM routing checks
        `provider_for_realm` **before** the peer bridge, it would receive their mail. Our own name is
        worse: `member_key` collapses a local user to their bare account, so a realm `test.example`
        would let a provider act as the local account `ada`.
      * **The domain owner arbitrates** (owner directive 2026-08-04: "network should be domain
        validated … they can either have a matrix server or a WEFT server"). `REALM ASSERT` consults
        the domain via a new `NetworkProbe` port — weftd implements it on the same TLS-verified,
        SSRF-guarded `/.well-known/weft` fetch auto-federation uses, cached per process — and refuses
        a realm whose domain runs WEFT. **Only a positive answer refuses:** no well-known, NXDOMAIN,
        unreachable, or a realm that is no domain at all (a Discord guild id) must all still bind, or
        a DNS blip would lock out every legitimate bridge. Local fast-path checks stay for what the
        probe can't see: our own name, an existing **peer record**, a **netblocked** name, and a realm
        that isn't a valid `NetworkName`. Tested (incl. Discord-style + non-WEFT domains binding).
      * **NETBLOCK is name-keyed, so it bites realms (invariant 7).** Blocking a network now stops a
        bound provider's ingestion **mid-session** (effect 3), refuses a fresh `REALM ASSERT` for it,
        and refuses DM routing to it — a network an operator shut out cannot re-enter as a bridge, or
        keep talking on an already-bound session. Tested end-to-end.
      * **`FEDERATE` toward a bridged realm** answers "that network is bridged, not federated" instead
        of spending a well-known fetch + connect attempt to discover there is no WEFT server there.
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
      * **Role assignment relays** — a WEFT role is a labelled bundle that *materializes into grants*
        (`ROLE ASSIGN` → `on_grant`), so promoting someone in a replica ns raises their foreign level.
        Only **`@everyone`** does not: it is resolved live at check time and never becomes a grant
        (relaying a baseline every member holds would mean "give everyone level 50"). Inbound grants
        carry **no role record** — a Matrix ns is `authority=levels` (§7a.3), so the client renders a
        number, and a synthetic WEFT role named for a power level would model a concept Matrix lacks.
        *(Corrected 2026-08-04: an earlier note here claimed role-derived authority was not relayed —
        wrong, since `ROLE ASSIGN` goes through `on_grant`.)*
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

*The provider wire contract slices 1–5 built is written up as
[`bridge-session-protocol.md`](protocol/bridge-session-protocol.md) — the reference the SDK below
implements and the daemon is written against.*

- [x] **6. M-plug-2b — `weft-appservice` dispatch loop** (M) — **DONE 2026-08-04. The daemon can now
      be written.** Connect → `AUTH ADAPTER` handshake → `PLUGIN-REGISTER` → dispatch loop, with
      `AppService::builder(...).name/.bot/.scheme/.action/.on_action`. `connect()` returns a
      `Connected { session, realm, events }`: the loop to drive, a [`Realm`] to speak as, and the
      stream of everything weftd says (mapping acks, `PROVISION`, `NS JOIN` requests, relayed events,
      backfill). `run()` is the no-bridge shortcut.
      **`Realm` is where the SDK earns its keep** — it owns the things every adapter would otherwise
      reimplement and get subtly wrong: minting (`Realm::mint`, and `assert_channel` returns the
      canonical `#<ns-id>/<chan-id>` *without a round-trip*), attribution (`@as` always, `@msgid`
      where required — the API won't let you omit it), the full-replace `begin_sync`/`end_sync`
      window, grants, and `PROVISION` answers. Nothing hand-builds a line.
      **Two-live-endpoint conformance test** (`weftd/tests/conformance`): the real SDK against a real
      weftd over real QUIC — authenticate, register, bind a realm, assert a space + room with
      self-minted ids, replay a message, and a local member receives it with the realm's msgid
      intact. **It immediately caught a deadlock**: the first design shared the stream between a
      reader task and a writer task behind a mutex, and the reader holds the lock across
      `recv_line()` — which is always — so nothing was ever sent. Now one task owns the stream and
      selects over read/write (`recv_line` is cancel-safe, so this loses nothing).
      *Not yet:* multi-step flows (`SUBMIT`/`ACTION`), hooks, per-space ban lists (slice 10b).
- [~] **7. M-plug-3 — flows + client SDUI renderer** (L, mostly client) — **server half DONE
      2026-08-04.** `PLUGIN SUBMIT` / `ACTION` / `SUBSCRIBE` / `UNSUBSCRIBE` / `CLOSE` all route
      through one `on_plugin_step`: they are the same routing problem (find the flow by view-id,
      check it is the caller's, hand the step to the plugin that owns it). The view-id already
      carries the plugin (`<plugin>:<seq>`), so no extra bookkeeping was needed to know where a step
      goes. Each step **re-points the parked echo label** (`relabel_invoke`), so a step acks *itself*
      rather than the invoke that opened the flow.
      **Ownership check (the security-relevant part):** a view-id is a plugin name and a counter, so
      it is guessable — without a check any session could drive, read or dismiss another user's
      dialog. The parked writer *is* the requester's, so `Sender::same_channel` answers "is this
      yours" with no new state. A view that is not yours is refused exactly as one that does not
      exist (invariant 1). Tested for all five verbs.
      **Terminal steps free the parking:** `PLUGIN-RESULT` (already) and `CLOSE` (new) — otherwise a
      dismissed view would pin the requester's writer for the life of the session.
      **Client plumbing DONE 2026-08-05** (owner call: do the client before the daemon — nothing in
      slices 7/9 had a consumer, and the renderer is what validates the component catalog, patch ops
      and container kinds; a mistake there is a *proto* change that would invalidate the SDK and any
      daemon built on it).
      Three layers, bottom-up:
      * **`weft-client-core`** — builders for all seven client verbs (`SUBSCRIBE`/`UNSUBSCRIBE` are
        one call with a flag), four `ClientEvent` variants (`PluginManifest`/`View`/`Patch`/`Result`)
        carrying the label so a view can be matched to the step that asked for it, and
        `plugin_values` bridging the UI's JSON to the wire's CBOR. **Payloads are decoded to JSON at
        this boundary**, so the frontend needs no CBOR decoder *and* an undecodable payload is
        dropped where the types are rather than reaching a renderer that must guess. Tested both
        ways, including the drop.
      * **Tauri** — six commands wrapping them.
      * **TS transport** — `plugins`/`pluginInvoke`/`pluginSubmit`/`pluginAction`/`pluginSubscribe`/
        `pluginClose` + the four event variants on `WeftEvent`. `svelte-check`: 0 errors/0 warnings.
      **Renderer DONE 2026-08-05** — 13 component types, modal container, and five of the seven
      launch surfaces: `context-menu` (message), `settings` (Server Settings pages), `server-menu`,
      `channel-list`, `global` (the Cmd+K palette, with a Commands section). The catalog survived
      contact: every type rendered with **no proto change** needed, which was the point of doing the
      client before the daemon.
      **`slash` DONE 2026-08-05** — `/<action-id>` in the composer, checked **after** the built-ins
      so a plugin cannot shadow `/ban`. Arguments map per §13.4 / decision §20-F: **both**
      `key:value` (binds by input id) and positional (a bare token fills the next unbound input, by
      declaration order). Quoted runs are one token, so a free-text input can hold a phrase — without
      that no positional input could ever contain a space. A `key:value` whose key is not a declared
      input is treated as text rather than dropped (a URL or a time is likelier than a typo'd field).
      `/help` lists plugin commands too — one you can run but cannot discover may as well not exist.
      *Fixed here:* `plugin_invoke` passed `params` through **raw** while `SUBMIT`/`ACTION` encoded
      it, so a frontend (which has no CBOR) could never send readable params. Slash was the first
      caller to pass any.
      **`admin` DONE 2026-08-05 — all seven surfaces wired.** The panel is HTTP request/response and
      holds no session, so unlike every other surface it cannot be *pushed* a view.
      `ctx.admin_plugin_invoke` bridges the shapes: park a private channel, send the invoke, await
      the first line, **10 s timeout** so an HTTP request never hangs on a plugin that went quiet.
      It returns `(view_id, payload)` so a page can drive later steps of the same flow rather than
      restarting each request. `Live::plugin_catalog` + `Live::plugin_invoke` are the seam;
      `GET /api/v1/plugins` (Read) and `POST /api/v1/plugins/:plugin/:action` (Moderate) the routes.
      A plugin that does not answer is `502` with a reason, and a standalone panel `501` — neither
      pretends there are no plugins. Params travel as JSON and weftd encodes them, so the panel never
      touches the wire format either.
      **Panel UI DONE 2026-08-05.** Flow steps got their own routes —
      `POST /api/v1/plugin-views/:view_id` `{button?, values?}` (ACTION with a button, SUBMIT
      without) and `DELETE` (CLOSE, terminal) — backed by `ctx.admin_plugin_step`/`_close`.
      Panel-owned views are tracked (`admin_views`), so the panel cannot re-park (= hijack) a
      session-owned flow and a session cannot step a panel's (reply-channel identity). Answers are
      decoded to JSON at the handler (`decode_plugin_answer`), mirroring `encode_plugin_params`
      inbound — the vanilla-JS SPA never sees CBOR. The SPA renders a dynamic **Plugins** nav group
      from the catalog's `surface=admin` actions and draws the §10 component set (minus `custom`
      widgets; `markdown` is escaped verbatim — no renderer in the panel, untrusted text). A step's
      terminal result re-runs the action so the page shows post-action state; an *invoke's* terminal
      result renders as the page (re-running would loop). Leaving a page CLOSEs its flow.
      *Remaining for the plugin surface overall:* `Container::Custom` widgets (Track B).
- [~] **8. Capability-profile slice** (S–M) — **server half DONE 2026-08-04.**
      `authority=roles|levels|none` + `settings=<comma-list>` ride `NS-META` both ways: a provider
      declares them on its foreign assertion, they persist on the namespace record (migration 0054,
      mem+PG), and `ns_meta_event` carries them everywhere a namespace is described (SYNC, DISCOVER,
      join acks, provider-state pushes). Absent ⇒ the native default (roles authority, every surface
      enabled), so nothing changes for ordinary servers. New `Authority` wire enum.
      **Display gating only** — a *hint*, not a mirror: the server refuses no verb by `origin` except
      `NS RECOVERY SET`; it withholds **authority** (the owner shortcut is gated on the ns being
      native, so `ns:`-scoped verbs fail `CAP-REQUIRED`). Operators and explicit-grant holders still
      succeed, and the profile hides those surfaces from them too — safe direction, but not a mirror.
      Tested: a realm declares `authority=levels` + `settings=roles,permissions`, and a member who
      joins later sees it via DISCOVER (so it is stored, not just echoed).
      **Remaining:** (a) `level=<n>` on `MEMBER`/roster — needs per-(ns, member) storage, so a
      membership-row migration; (b) the client half — hide the native roles editor, render levels.
      The *editing* surface is the plugin's own Power Levels action (slice 7's SDUI renderer): the
      client sends numbers as `PLUGIN INVOKE` params, never as wire verbs, and the adapter translates
      — caps→levels is lossy and the translation must sit where the pinned key is, not in a client.
- [~] **9. M-plug-6 subset — settings surfaces + live panels** (M) — **server half DONE 2026-08-04.**
      §11.3 implemented as specced: weftd maps `view-id → panel_key` (noted as a `PLUGIN-VIEW` passes
      through) and tracks which views a client currently has open (`SUBSCRIBE`/`UNSUBSCRIBE`, cleared
      by `CLOSE`/terminal result).
      **A patch is addressed by view-id *or* panel key.** A plugin cannot know each open copy's
      view-id, so it patches by the key it chose and weftd fans out to every subscribed copy —
      **"a closed key is a no-op"** falls out of the subscription set, so a client that closed the
      panel is not sent updates for it. Pushes are relayed **unlabelled** (§12.4: unsolicited).
      *Design note:* the key/subscription state lives in `ServerCtx`, not on a session — the view is
      sent on the **plugin's** session and subscribed on the **client's**, so a per-session map is
      silently always empty (which is exactly how the first cut failed).
      **Client DONE 2026-08-05:** `settings` pages render in Server Settings (opening one closes the
      previous panel, so a plugin is not left pushing into a screen nobody is watching);
      `server-menu` and `channel-list` entries append below the built-ins rather than displacing
      them.

## Phase 3 — the bridge itself

- [~] **10. Matrix daemon MVP** — **inbound-consume half DONE 2026-08-05** (owner decisions: crate
      in-workspace `crates/weft-matrix` with its own `rust-version = 1.85` — excluded from the MSRV
      CI job like `voice` — using **reqwest + ruma types**; matrix-sdk rejected: client-framework
      mismatch, e2ee dead weight, MSRV).
      **Shipped:** `weft-matrix` daemon (lib + bin): `ident` (injective `=xx` MXID escaping,
      deterministic structure ULIDs from sha256(room_id), msgids timestamped from
      `origin_server_ts` so replica ordering holds), `hs` (thin CS-API client as appservice:
      resolve/join/state/send/redact/leave/register with `?user_id=` puppeting), `asapi` (AS
      transaction endpoint: hs_token auth, txn dedup, block-don't-drop), `store` (atomic JSON state:
      structure maps, event↔msgid, reactions both directions, puppets, BanList), `bridge` (the
      single-tasked core: PROVISION → resolve+join+enumerate space children → assert ns
      (`authority=levels`, roles hidden) + channels (encrypted rooms excluded, invariant 8) +
      member statements; Matrix→WEFT MSG/EDIT(m.replace)/redaction→DELETE/reaction±; WEFT→Matrix
      puppet relay with register-on-first-use; §8 member_rooms mapping; §8 return path for local
      mutations; bans enforced at assert/provision/ingest/relay), `main` (reconnect loop with
      re-assert + SYNC full-replace resync, `generate-registration`).
      **Protocol amendments shipped with it (owner-approved):** §5 ingestion sender widened to
      *any foreign* user (cross-realm rooms; local + peer identities still refused; netblock bites
      sender's network too) and §8's return path made real (local `@as` accepted for mutation verbs
      on realm-origin roots only — without it the flip side could never close). SDK grew
      `Incoming::{Event,Command}` (relayed commands were silently dropped before), `BanList`, and
      `Realm::capture` (adapter testing seam).
      **Tests:** 10 unit + 3 mock-HS integration tests (provision/exclude-e2ee, both traffic
      directions incl. puppet-echo suppression, ban both directions + re-provision refusal);
      weft-core `a_cross_realm_sender_ingests_but_local_and_peer_users_are_refused` pins the
      amendments.
      **Storage pivot (owner directive 2026-08-06):** the JSON state file is gone — the daemon's
      store is **Postgres** (`matrix_`-prefixed tables; its own database or weftd's — idempotent
      DDL instead of `sqlx::migrate!`, whose `_sqlx_migrations` table weftd already owns). Shape:
      write-through cache — memory is the read path (single-tasked, one writer), every mutation
      goes through a `Store` method that also writes its row; a failed write warns rather than
      killing the bridge. Contract test `tests/pg.rs`, gated on `WEFT_TEST_DATABASE_URL` (CI runs
      it; not validated on a live PG locally — Docker was down).
      **Identity pivot (owner directive 2026-08-06):** puppets are keyed by **account ULID**
      (`weft_<ulid>`), never by name — names are mutable vanity labels, and a name-keyed puppet is
      orphaned by a rename. Wire addition: weftd stamps `ulid=` alongside `@as` on every provider-
      bound relay (membership + mutations); the SDK surfaces it (`Incoming::Command.as_ulid`); the
      daemon's ULID↔name table (`matrix_users`) is populated at the NS JOIN relay — the only door a
      local user enters through — and the name-only fan-out events resolve against it.
      Also: OO pass on the store (structured `Reaction` key fixed a real collision bug — `|` is
      legal in a Matrix annotation key; `Links` encapsulates the two-map invariant; §8 membership
      transitions live on `Space` with unit tests), and empty Spaces provision as empty namespaces
      (join confirms immediately — nothing foreign-side to fail).
      **Outbound projection, weftd half DONE 2026-08-06** (owner decision: flag-keyed provider
      ingest — the plan's old "pure daemon work" note did not survive the framework, since provider
      ingestion hard-refused native channels). Shipped: **O1** the §17.1 `bridge:<scheme>` opt-in
      (`NS META <ns> bridge:matrix :open` — ns-admin, requires `public`, `bridges=` on NS-META,
      migration 0055 + mem/PG contract, cleared when visibility leaves public); **O2** the return
      path (`on_projected_ingest`): the flag authorizes the scheme's provider to inject foreign
      users into the namespace's channels — **the home mints** (`@msgid` refused), the injection's
      labeled echo is the §3.5 ack (`RelayPublish` echo → `on_provider_event`), EDIT/DELETE
      authorship-checked, local `@as` always refused; provider forwarders now also cover projected
      namespaces (attach at register/ASSERT — a mid-session flag flip attaches on reconnect).
      Tests: `ns_meta_bridge_flag_requires_public_and_closes_with_visibility`,
      `a_projected_namespace_bridges_both_directions_and_the_home_mints`.
      **O3–O7 daemon half DONE 2026-08-06.** weftd enablers: structure push at register/ASSERT
      (NS-META + CHANNEL-LAYOUT + POLICY per projected ns — the adapter needs the policy for the §3
      rules), `ulid=` stamped on relayed event copies with a local actor (session-memoized lookup;
      SDK `Incoming::Event { event, label, actor_ulid }`), and NS-MEMBER accepted through the flag
      door for **foreign** members only (§8 run in the outbound sense; locals refused — they join
      natively). Daemon: `Projection` state + `matrix_projections`/`matrix_projected_rooms` tables,
      `ensure_projection`/`ensure_projected_room` (Space + rooms, ULID-keyed aliases `#weft_<id>`,
      §3 rules enforced — a `retained` channel is absent by rule), unified relay routing (consumed
      replica OR projection; foreign-sender events never relay back — they originated on Matrix),
      the injection door (`Realm::inject_message`/`inject_edit` — no msgid, labeled echo links the
      home-minted id via `pending_injections`), and §8 membership statements from projected-room
      joins. Tests: the weftd conformance-style core test (structure push, ulid stamp, member door
      both ways) + the daemon mock-HS end-to-end (Space+room creation, §3 exclusion, both traffic
      directions, echo-ack linking, membership join/part).
      **Projection polish remaining:** category sub-spaces (locked decision 4 — flat under the top
      Space for now), live re-assert on rename/layout change (currently reconnect), room directory
      publishing, roster mode config.
      **HISTORY backfill DONE 2026-08-06.** Replica-only by construction (a projected channel's
      history is the home's own — it minted every id). weftd's `HISTORY #chan before=… limit=…` is
      answered by **replaying the window as ordinary ingestion** (§8, no separate ingress):
      `before` → its Matrix event via the links map → `/context` for a pagination token →
      `/messages?dir=b` → the page **reversed** (Matrix pages newest-first; the replica orders by
      ULID time). Safe to repeat because `ident::msgid_for` is deterministic and already-linked
      events are skipped; our own puppets' events are skipped too (they are WEFT-origin). Page size
      is capped at 50 — Matrix paginates far below WEFT's `MAX_HISTORY_LIMIT`, and backfill is
      demand-driven. `HISTORY` is handled **before** the `@as` gate: it is a request about a
      channel, not on anyone's behalf.
      Two bugs fixed on the way, both pre-existing: (a) `ident::stable_ulid` built a ULID from a raw
      hash u128, overflowing the 48-bit timestamp field — such a value does **not** survive a parse
      round trip, so weftd stored a different id than the daemon minted and every map keyed on ours
      missed (now `from_parts`, with a round-trip test); (b) the links map was keyed on whatever
      spelling a caller held — the lowercase wire form from ingestion vs the uppercase canonical
      `MsgId::to_string()` from events — so a WEFT reaction to an *ingested* Matrix message looked up
      the canonical form, missed, and never reached Matrix (now canonicalized inside `Links`).
      **Media DONE 2026-08-06** (§12), with the two weftd surfaces it needed —
      owner decision: provider-scoped upload grant **and** fix bot accounts.
      weftd: `STREAM OFFER` routed on a provider session (authorized by the pinned key, not an
      `attach` cap — a provider has no account; size/mime still bounded, grant one-shot), and
      `Registration.bot` finally wired end to end — the SDK collected `.bot()` from day one but the
      proto had no field, so every request was silently dropped. weftd provisions it as a **native bot account**
      (owner directive 2026-08-06: the first cut reused `suspended`, which made a bot look like a
      punished user, made un-suspending it silently grant login, and left no way to actually suspend
      one — now a `bot` flag, migration 0056, refused at the single AUTH chokepoint with the uniform
      `AUTH-FAILED`, independent of suspension) and it is the one local account a provider may name
      in `@as`. The intended second door is an API token; the flag is what makes room for it.
      Discovery that shrank the work: weftd's `GET /media/<hash>` is **already** unauthenticated by
      design (media-proxy model — the hash is the capability), so WEFT→Matrix needs no credential
      and no new bearer type was necessary.
      daemon: `media.rs` (weftd's HTTP plane + msgtype/mime mapping + magic-number sniffing, since
      weftd's fetch reports no mime), `Hs::download_mxc`/`upload_media`. Inbound waits for the
      grant before sending the message — a reference to a blob weftd does not hold yet renders as a
      broken attachment. Outbound mirrors each blob as its own Matrix event (one attachment per
      event is all Matrix carries). SDK: `Realm::offer_media`, `message_with_attachments`.
      **Honest limits:** the outbound mime is sniffed rather than known (weftd's fetch carries none
      — worth a `Content-Type` on that response later); `MEDIA BLOCK`-after-the-fact does not yet
      redact the mapped Matrix events or quarantine the mxc (§12's third bullet); inbound size is
      bounded by weftd's config, not pre-checked against it.
      **Typing + DMs DONE 2026-08-06.** Typing (§15) crosses both ways: inbound needs `@as`
      (the wire's `TYPING` names no user — a client's own session identifies them), so a provider's
      is bounded like every attributed line (replica of its scheme, foreign sender) and **announced**
      rather than ingested since it is never stored; outbound rides the event's own `user` field plus
      `ulid=`, and the daemon mirrors it as the puppet's typing EDU with a 20 s TTL so a lost `stop`
      still clears. DMs: weftd's outbound relay was **already wired** (`relay_foreign_dm` →
      `provider_for_realm`) — the doc's "not wired" note was stale; the real gap was the daemon,
      which now opens a Matrix DM room **as the puppet** (`is_direct` + invite, so it belongs to the
      two of them, not the bot), remembers it (`matrix_dm_rooms`), and routes messages in it to the
      ordinary DM scope. SDK: `Realm::dm`.
      **Remaining:** DM *edits/reactions* (the message path is wired both ways; the mutation verbs
      are not), and read receipts stay unbridged by design (WEFT's `MARK` is private, Matrix
      receipts are public).
      **Not in the MVP (deliberate):**
      multi-realm (one realm per daemon for now), moderation/power-levels (slice 11), puppet
      display-name sync on rename (the identity survives; the pretty name catches up later).
- [~] **10b. Per-space bridging bans** (owner requirement 2026-08-04) — an admin page where
      **individual foreign spaces** can be banned from bridging, finer-grained than `NETBLOCK` (which
      is name-keyed and takes out a whole realm). Banning `matrix://matrix.org/#abusive-space` must
      not require severing `matrix.org`.
      **BUILT 2026-08-04 as a generic, platform-agnostic mechanism (framework §7a.0f):**
      `BRIDGING <ns-id> banned|allowed`, pushed to the governing provider when an admin bans a space
      via `POST /api/v1/namespaces/:name/bridging`. **weftd stores nothing** — no column, nothing
      re-sent on reconnect — because the bridge stores and enforces it. Generic because "stop
      bridging" is all that Matrix rooms, Discord guilds and Instagram feeds have in common. A
      disconnected provider answers 409 rather than a success that carried nowhere. `Live::set_bridging`
      is the panel→weftd seam (embedded only), tested with the whole path.
      **DECIDED (owner 2026-08-04): the plugin enforces.** It owns the page and the list, and simply
      declines to assert or provision a banned space. **No weftd state, no new server surface** — the
      "keep weftd thin" directive applied to its own case. weftd already has `NETBLOCK` for the
      blunter, name-keyed instrument when a whole realm must go.
      *Accepted trade-off:* the ban is enforced by the party that also does the bridging, so it does
      not survive a compromised adapter. That is tolerable because the adapter is pinned-key
      authenticated and is not the adversary here — the abusive **space** is — and because an operator
      who does not trust the adapter can pull its pin (which now also unlocks deleting its
      namespaces).
      *Depends on:* the SDUI settings surfaces (slices 7 + 9) for the page.
      **SDK:** the ban list and its enforcement points (refuse assert, refuse provision, refuse
      ingest for a banned space) belong in `weft-appservice`, not in each adapter — this is exactly
      the "utilities to ensure smooth operation like a federation" the SDK is for, so every adapter
      gets one consistent implementation.
- [~] **11. Moderation + power levels DONE 2026-08-06** (the authority half; SDUI management flows
      remain). §10 implemented as *attributed* authority, both directions:
      **weftd:** `@as` `GRANT`/`REVOKE` route to the ordinary handlers as `Actor::Foreign` (a
      foreign moderator wields exactly the caps WEFT granted **them**, incl. `grant:<cap>`);
      `@as DELETE` of another author now checks `delete-any` (the slice-10 TODO closed); grant
      relays cover projected namespaces too and carry `ulid=` for local **subjects**; `PLUGIN
      INVOKE` carries the invoker (`as=`/`ulid=`) so a management action knows who is asking
      (SDK `Ctx::invoker`/`invoker_ulid`).
      **daemon:** `levels.rs` owns the mapping — three tiers (admin 90 = `ns-admin`, mod 50 =
      `mute,ban,kick,delete-any`, member 0; bot 100 above all, §9) + a `diff_users` on the PL
      users map so only real changes translate. Outbound: a grant/revoke becomes a read-modify-write
      of `m.room.power_levels` in every room of the space (subject = real MXID for a foreign handle,
      ULID-keyed puppet for a local account, registered on the spot from the relay's `ulid=`).
      Inbound: a PL event diffs against the persisted baseline (`matrix_room_levels`) and emits the
      acting moderator's attributed revoke-then-grant; a Matrix ban/kick **of a puppet** becomes the
      attributed `BAN`/`KICK` (foreign targets stay Matrix-internal — roster flow handles them).
      `ident::unescape_localpart`/`mxid_of_weft_user` close the identity round trip.
      Tests: `foreign_moderators_wield_exactly_their_granted_authority` (weftd: ungranted delete
      dropped + ungranted GRANT `CAP-REQUIRED`, then both succeed after the grant, and the grant
      relays outward), `authority_translates_both_directions` (daemon: grant→PL for foreign and
      local subjects, PL→attributed grants, unchanged map = no work, puppet ban→attributed BAN),
      `levels.rs` unit tests.
      **SDUI flows + §10 revert DONE 2026-08-06.** Flows live in `weft-matrix/src/actions.rs`
      (declarations + views) with handlers in `bridge.rs`: **Power Levels** (`settings`/namespace —
      the surface `authority=levels` promises: the live map as a table + a three-tier picker, since
      the capability mapping has three tiers and a free number would imply precision it lacks),
      **Invite** (`channel-list`), **Moderate** (`context-menu`/member), **Bridged room**
      (`channel-settings` — new surface, owner directive: `channel-list` is for custom buttons,
      configuration belongs in the settings pane), **Bans** (`admin`). Every flow's wire commands
      are **attributed to the invoker**, never the service — that is what `Ctx::invoker` was for.
      **§10 revert:** attributed acts now carry a `label`, weftd echoes it on the direct response
      **including `ERR`** (`on_provider_acting` passes the request label through), and the daemon
      parks an undo per act (`PendingAct::Level`/`Membership`) — a refusal restores the previous
      power level or unbans, then posts an `m.notice` naming the reason. One revert per act (the
      label is spent). SDK additions: `Ctx::view`/`toast`/`view_id`, `Realm::ctx_for`,
      `AppServiceBuilder::declare` (declared-without-closure actions arrive as `Incoming::Invoke` —
      the old loop **dropped** them, which read as a dead button), `Incoming::Step` (typed +
      CBOR-decoded submits/clicks/closes), labeled `*_as` moderation helpers, `mute_as`.
      Client: `channel-settings` pages render in `ChannelSettings.svelte` (same pattern as the
      server-settings surface).
      **create-room DONE 2026-08-06**, both sides of the fork it turned out to be: in a
      **projected** namespace the WEFT channel is the real object, so the flow issues the invoker's
      attributed `CHANNEL CREATE` with `permanent` retention (nothing else projects, §3) and creates
      no room itself; in a **consumed** space weftd refuses local creates, so the room is created on
      Matrix, linked under the Space, and asserted back — filed in the *consumed* map, since its
      events are realm-minted (the projection map would re-mint every message).
      weftd half: a channel created after the provider's startup push now reaches it live —
      `push_new_channel_to_providers` (structure) **plus** `SessionEvent::Attach` with a bounded
      confirm (`PROVIDER_ATTACH_TIMEOUT`, 2 s) so the create acks only once the provider is
      watching; a broadcast has no replay, so acking first loses the room's first messages.
      `PluginReg` carries the provider session's event queue; `register_plugin` takes a grouped
      `ProviderRegistration`. SDK: `Realm::create_channel_as`.
      **kick/ban flow DONE 2026-08-06.** A member action's ctx-ref is `user@net` (§13.2) — no
      channel — so the moderate view **asks**: a picker over the bridge's channels, kick names the
      chosen one, ban derives `ns:<id>` from it (never a guessed `*`). Two paths, because the target
      kinds are different acts: a **local** member takes the attributed `KICK`/`BAN`; a **foreign**
      member cannot be named by those verbs at all (they take a bare `Account`; `carol@kde.org` is a
      `user@realm`), and their membership is the realm's to state (§6) — so they are removed
      *foreign-side* via `Hs::remove_member`, and the realm's `NS-MEMBER part` follows.
      `PendingAct::Membership.mxid` became optional and every flow-initiated act is now parked: with
      no puppet there is nothing to revert, but the refusal is still reported (notice-only remedy).
      **Client gap closed:** `member`/`user` context actions were never offered — `pluginItems` was
      only ever called with `["message"]`, so every member-context plugin action was invisible. Now
      wired into both `userCtx` and the Server-Settings roster (`nsMemberCtx`).
      **create-subspace DONE 2026-08-06**, together with the sub-space projection it was blocked
      on (locked decision 4): a projected namespace's categories become **child Spaces** —
      `ensure_categories` creates one per `cats=` entry, ordered by its index, and a categorized
      channel's room is parented under its category's sub-space instead of the top Space. Additive
      by design: a category dropped from the list keeps its sub-space, because a tombstone is
      unrecoverable and a rename arrives as a drop plus an add. Persisted in
      `matrix_projected_categories`.
      The flow itself sends an attributed `NS META … categories` (the invoker's ns-admin is what
      weftd checks) and creates **nothing** locally — weftd applies the change and pushes the
      resulting `NS-META` back, and that push is what builds the sub-space; creating it first would
      orphan one whenever weftd refused. It appends to **weftd's declared list**
      (`Projection.declared_categories`, refreshed by every push), never to the sub-spaces the
      daemon happens to have built — the meta key is a full replace, so appending to a partial view
      would delete every category not yet projected. Commas and duplicates are refused before the
      wire. A *consumed* space refuses the flow outright: its structure is the realm's to describe.
      weftd half: `announce_ns_meta` pushes every NS-META change to the projecting providers (a
      provider is not an ns member, so the member fan-out never reached it). SDK:
      `Realm::set_ns_meta_as`.
      **Slice 11 is complete.**
- [ ] **12. Track B — widgets + client-Rhai + CSP** (L) — the rich PL-matrix editor as a sandboxed
      widget. **Polish, not gating**: SDUI tables/forms carry the v1 levels view.

---

## Admin panel additions (weft-admin — after the phases above)

- [x] **Replica deletion is gated on the provider being disabled** (owner directive 2026-08-04) —
      `DELETE /api/v1/namespaces/:name` refuses an `origin` namespace while that provider's scheme is
      still pinned in `[[plugin.remote]]` (409). "Disabled" = the pin is gone, **not** merely
      disconnected: a bridge restart must not open a destruction window. A **standalone** panel can't
      see the config and refuses rather than guessing. `AdminState::with_configured_schemes`, wired
      from weftd. Tested all three states (pinned → 409, standalone → 409, unpinned → 204).
- [ ] **Namespace management page**: list ALL namespaces (store-direct `list_all`) with
      visibility, member count, and **origin/provider badges** (foreign URI + live/offline state);
      **DELETE any namespace** — the operator UI form of the 3b-a escape hatch, reusing the same
      cascade (incl. orphaned virtual namespaces whose provider is long gone).
- [ ] **Provider status page**: the `[[plugin.remote]]` / `[[foreign_bridge]]` pins with their
      connection state (online/offline, registered schemes, declared actions), and each provider's
      virtual namespaces.
- [~] **Plugin-supplied admin pages** (owner requirement 2026-08-04) — a plugin declares actions on
      the **`admin` surface**, rendered in the operator panel rather than the client: a bridge's own
      per-space ban list (10b), a health view, a re-sync button. Fits the permission model rather than
      bending it — operators act through the panel, so an operator-facing plugin surface belongs
      there. **Done:** `Surface::Admin` in the codec, `AppService::admin_action(...)` in the SDK.
      **Remaining:** a `Live` routing method (the panel is store-direct and speaks no wire protocol,
      so it invokes through weftd via the same embedded-only seam as kick/eject) + the panel's
      renderer, both of which ride the SDUI work (slices 7 + 9). Standalone panels show them
      unavailable, as they already do for other live-only actions. Spec: plugin-spec §22.
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
