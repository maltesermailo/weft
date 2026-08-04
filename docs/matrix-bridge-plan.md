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

- [ ] **0. Materialization decisions** — ratify: owner = reserved suspended **sentinel account** ·
      `root_key = ""` · the single **owner-shortcut gate** on `origin.is_some()` (context.rs cap
      check). Everything in Phase 1 assumes these.

## Phase 1 — server content path (weftd/core; mock-provider-testable, no daemon needed)

- [ ] **1. Foreign-display proto fields** (S) — `foreign=` tag on `MESSAGE`/`MEMBER`/`REACTION`/
      `EDITED`; `origin=` on `NS-META`/`CHANNEL-LAYOUT`/`DISCOVER`. Round-trip tests FIRST.
      Do before ingestion so ingested events are legible from day one. *(framework §7a.1–7a.2)*
- [ ] **2. Provider-session unification** (M) — merge `foreign_control` + `plugin_registry` into one
      **provider registry**; fold `State::ForeignBridge` → `State::PluginService` (one session speaks
      bridge verbs AND plugin protocol); `schemes` field in `Registration` → `PROVISION` routing.
      (The former M-plug-11 fold, pulled forward so later slices land once.) *(plugin-spec §18)*
- [ ] **3. Materialization success path** (M–L) — sentinel account provisioning; `PROVISION-OK` +
      provider `NS-META`/`CHANNEL-LAYOUT` assertions → create origin-marked ns + channels; the
      owner-authority gate; join the requester + roster reply. **`NS JOIN matrix://…` succeeds.**
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
