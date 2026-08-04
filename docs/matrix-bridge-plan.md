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
- [ ] **4. Ingestion** (M) — provider `@as` `MSG`/`EDIT`/`DELETE`/`REACT`/`MEMBER` → replica channel
      actors mint events carrying `foreign=`. **Matrix → WEFT messages flow.**
- [ ] **5. Outbound relay** (M) — local posts/edits/reacts/joins in origin-marked channels forward to
      the provider session (reuse the bridge-forwarder machinery). **WEFT → Matrix flows.**

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
