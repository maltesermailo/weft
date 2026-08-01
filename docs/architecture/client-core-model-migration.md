# Client-core model migration — plan

Move the client's **management/business logic** (the reducer + model mutation) out of TypeScript
and into `weft-client-core`, so it is written once in Rust and runs on **both** targets — native
(desktop, via `src-tauri`) and WASM (web, via `weft-client-wasm`). TS becomes a thin **reactive
mirror** that renders. This is not a fork: the web build already *is* the WASM build of that crate,
so one core serves both. A separate TS backend is explicitly rejected (it would duplicate the model
in two languages).

## The seam we ride on (verified)

- `weft-client-core` is **pure** (deps: `weft-proto`, `weft-crypto`, `serde` — no I/O, WASM-ready).
  Today it is a stateless codec: `on_line<E: EventSink>(…)` parses a wire line and pushes
  `ClientEvent`s to a sink; `build_*` builds outbound lines.
- The host boundary is one trait:
  ```rust
  pub trait EventSink { fn emit(&self, event: ClientEvent); }
  ```
  `ClientEvent` is `#[serde(tag = "kind", rename_all = "kebab-case")]`; TS discriminates on `.kind`.
  Desktop `TauriSink::emit` → `app.emit("weft", event)`; web `JsSink::emit` → the JS callback passed
  to `new WeftClient(cb)`. **Both already carry arbitrary tagged payloads.**
- TS `weft.ts` picks the backend at runtime (`IS_TAURI ? tauriInvoke : wasm.invoke`); inbound events
  land in `reducer.svelte.ts` `handle(e)`, which dispatches on `e.kind` to per-domain handler maps
  (`channelHandlers`, `sessionHandlers`, …).
- Web WASM is loaded by `ensureWasm()` as a runtime `import("/wasm/weft_client_wasm.js")`
  (`@vite-ignore`), built by `npm run wasm` (`wasm-pack … --target web --out-dir static/wasm`,
  gitignored). **Adding to the core needs no new web wiring** — the next `wasm` rebuild picks it up.

**Consequence:** a state diff is just a new `ClientEvent` `kind` emitted through the existing sink.
TS routes it to a new **mirror** handler map — the same dispatch mechanism the reducer already uses.
No wrapper change, no transport change, no new IPC channel for the slice.

## New pieces in `weft-client-core` — clear codec/model separation, per-domain handlers

Three layers, kept strictly separate (mirrors the Phase-4 TS discipline):

1. **Codec** (crate root, unchanged): `ClientEvent` = the **wire** vocabulary; `build_*` = outbound.
   Diffs are NOT folded in here — the codec stays about the wire.
2. **Model** (`src/model/mod.rs`):
   - **`StateDiff`** — the model's *own* output vocabulary, distinct from `ClientEvent`. An aggregate
     over per-domain diff enums (`#[serde(untagged)]` → the inner domain diff's `kind` reaches TS, so
     the TS mirror routes on `kind` exactly as for wire events).
   - **`AppState`** — one sub-state field per migrated domain (`channels`, later `namespaces`, …).
   - **`reduce(&mut self, &ClientEvent) -> Vec<StateDiff>`** — the **dispatcher/registry**: offers the
     event to every domain handler and collects diffs. The Rust mirror of the TS reducer's
     `domainHandlers` spread. Un-migrated events yield no diffs (forwarded raw by the sink glue).
3. **Per-domain handlers** (`src/model/<domain>.rs`, e.g. `channels.rs`): each domain **fully owns**
   its `struct <Domain>State`, its diff enum (`ChanDiff`), and its handler
   `fn handle(&mut self, &ClientEvent) -> Vec<ChanDiff>` — the Rust mirror of one TS `*Handlers` map
   (`sync/channel-handlers.ts`). Adding a domain = a new submodule + one `.extend(…)` line in `reduce`.

Everything is pure (no I/O, WASM-safe) and **unit-tested per domain** — a concrete win over the TS
reducer, and reusable by `weft-tui`.

4. **`ReducingSink`** (wrapper glue, S1) — implements the codec's `EventSink` (receives each
   `ClientEvent` from `on_line`), holds `RefCell<AppState>`, and forwards to the *host* sink: the raw
   event (so un-migrated TS handlers/side-effects still fire) **plus** each `StateDiff` from `reduce`.
   `AppState` lives beside the connection — `WeftClient` (web, already `RefCell`) and a managed
   `Mutex<AppState>` (desktop). The host boundary carries a `{ Wire(ClientEvent) | State(StateDiff) }`
   union so TS routes wire→reducer, diff→mirror.

## Migration strategy — one domain at a time, both targets green

`reduce` handles migrated kinds and passes the rest through, so the TS reducer keeps owning every
un-migrated domain unchanged. Each domain flips independently; `check` + `build` + a desktop **and**
web smoke test gate every step.

## Vertical slice: **channels** (metadata / unread / typing / layout — NOT messages)

Deliberately excludes the message list and optimistic message send (the hard optimistic-UI problem)
— those come in a later slice. Channel *management* is the cleanest first cut.

- **S0 — model + diffs + reduce in core (Rust-only, no client change).** Define `ChannelState`
  (name, vanity, category, position, unread, mention, counts, typers, voice, restricted, viewGated)
  + the `Chan*` diff variants + `reduce` for `chanmeta` / `channel-layout` / `channel-renamed` and
  the unread/typing side-effects of `message`/`typing`. Rust unit tests. `cargo test` green.
- **S1 — insert `ReducingSink` in both wrappers, parity mode.** `AppState` in `WeftClient` /
  managed state. `reduce` **passes every event through unchanged** *and additionally* emits `Chan*`
  diffs that TS **ignores** for now (a debug listener logs them). Verify the Rust diffs match what
  the TS `channelHandlers` compute. **No behavior change.** Desktop + web smoke test.
- **S2 — flip channels to the mirror.** In core, stop forwarding raw `chanmeta`/`channel-layout`/
  `channel-renamed` (consume them); keep emitting `Chan*` diffs. In TS: delete `channelHandlers`
  (the chanmeta/layout/renamed cases) + the reducer's unread bump; add `channelMirrorHandlers`
  (`chan-upsert`/`chan-remove`/`chan-unread`/`chan-typing`/`chan-renamed`) that patch `ChannelStore`.
  `Channel.messages` stays TS-owned (message slice deferred). Layout writes (`moveChannel`/
  `setCategories`) keep sending their verbs; the echoed `channel-layout` now round-trips through the
  model → diff → mirror (keep the existing thin optimistic overlay for instant drag feel).
- **S3 — verify.** Desktop **and** web: channel list renders/updates, unread + mention badges bump
  and clear, drag-reorder + categories, rename. `check` 0/0 + both builds.

**Explicitly deferred out of the slice:** the message list + optimistic send (own slice, with the
optimistic-overlay decision); layout/DM **persistence** to Rust (keep in TS localStorage for now —
moving it needs a `HostStorage` trait injected into both wrappers); nav side-effects (self-join
`goto`) stay TS, triggered off a diff.

## What stays in TS — permanently

Rendering + the reactive mirror; DOM/geometry (MessageList virtualization, popover positions);
optimistic overlays; media (`upload`/`unfurl` = `fetch`) and WebRTC/voice (LiveKit). None are model
logic; they are platform/UI and belong in the view for both targets.

## Risks & containment

- **Diff/model divergence from the TS reducer** → the S1 *parity mode* runs both and compares before
  any flip; per-domain flips keep the blast radius to one domain.
- **Optimistic UI** → excluded from this slice; layout uses the existing thin overlay + echo
  reconcile (local server latency is sub-ms). The message-send optimistic path gets its own slice.
- **Web regressions** → every step smoke-tested on the WASM build too, not just desktop.
- **WASM bundle growth** (400 KB today) → watch it; the channel model is small.
- **`--no-typescript`** → the WASM `WasmClient` interface + the new diff-event kinds are hand-typed
  in TS; keep them in sync with the Rust enums.

## Effort (rough)

Slice: S0 ~0.5–1d (model + reduce + tests), S1 ~0.5d (sink insertion + parity), S2 ~1d (TS mirror
swap), S3 ~0.5d (two-target verify). The subsequent domains (servers, roles, social, session,
membership, federation, invites) each ~0.5–1d once the pattern is proven; **messages last** (the
optimistic-UI slice, the genuinely hard one).

## Order after the slice

channels (this slice) → namespaces/servers → membership → roles → session → social → federation →
invites → **messages + history + optimistic send** (last). Persistence + the `HostStorage` trait can
land whenever after S2 (independent).
```

## Extensibility / future plugin seam (decided: migration now, plugins later)

Plugins are a stated goal — three hosts: **UI** (JS view slots), **client-core** (WASM +
capability API), **server/weftd** (WASM + capability API). The two Rust hosts share the WASM +
capability pattern, authorized via WEFT's existing **§10.4 scoped capability tokens** (a plugin is a
principal holding a bounded cap). This is a separate, later, platform-scale effort (server plugins
are a largely independent weftd track). It does NOT block the migration.

The one constraint it puts on this migration: keep the `ClientEvent` / `StateDiff` / command
boundary **clean, well-named, and documented** as the future plugin seam — but do **not** freeze it
as a public API yet (no plugin authors, no versioned contract → premature). Hardening it into a
public, capability-scoped plugin API is a deliberate later layer once it has proven itself internally.

## S0 slice scope (minimal, dependency-free first cut)

To prove the whole pattern (model → diff → reduce → parity → mirror) with near-zero risk, S0 owns
ONLY the channel **scalar metadata/layout** set by `chanmeta` + `channel-layout`:
`category, position, voice, vanity, topic, restricted, view_gated`, plus create-on-first-mention.

Deliberately excluded from S0 (stay in TS, added in later channel slices): unread/mention (couples to
session+roles via `mentionsMe`), typing (needs a timer — platform, not pure-model), the roster
(`members`, couples to caps/profile/nav), `channel-renamed` + `deleted` (TS instance re-key/removal +
nav side-effects), messages/history, persistence. This keeps S0 free of cross-domain deps, timers,
and instance-management — just pure metadata reduction.

## S0 boundary refinement (learned from S1 parity)

S1 parity surfaced that **`category` + `position` are "layout", inseparable from two
TS-only layers** the model doesn't have: localStorage `layoutCache` seeding
(`ensureChannel`) + the optimistic `moveChannel` renumber. So they were **removed from the
S0 model** entirely. S0 now owns only the fields the model can be authoritative for from the
event stream alone: `topic`, `restricted` (posting), `view_gated`, `voice`, `vanity`.
`category`/`position` migrate later in a **"layout + persistence" slice** (with the
`HostStorage` trait + routing `moveChannel` through the model), which makes the model their
authority and closes their parity. The S1 parity comparator checks only the pure fields.

## Layout + persistence slice — DONE (model owns category/position)

The model is now authoritative for channel layout, with model-side drag optimism:
- **Model** (`channels.rs`): owns `category`/`position`; `serialize`/`seed`/`take_dirty` (JSON layout
  cache); `move_channel(ns, drag, target, anchor, after)` = the renumber (ported from TS), returning
  the state diffs + the `CHANNEL META` writes.
- **Persistence**: both wrappers `seed_layout` on connect (mirror paints the cached order instantly)
  and save the layout when it changes. Storage = `localStorage["weft:chan-layout"]` (web) / a file in
  app-data (desktop).
- **`move_channel` command**: web → a `dispatch` case; desktop → a `#[tauri::command]`, which required
  moving `AppState` into **Tauri managed state** (`weft::Model = Arc<Mutex<AppState>>`), shared by the
  connection task + the command. Emits the diffs (instant UI) + sends the writes.
- **TS flip**: `channelMirrorHandlers` now applies `category`/`position` too; `chanmeta` handles only
  `deleted`; `channel-layout` only `reconcileCreate`; `ensureChannel` no longer cache-seeds;
  `channelStore.moveChannel` is a one-line `invoke("move_channel", …)` (its renumber logic deleted).

**Deferred:** the ns-category *list* (`moveCategory`/`setCategories`/`nsCategories`) stays TS;
`channel-renamed` + `deleted` still TS (the model doesn't re-key on rename yet, so a renamed channel's
persisted layout can briefly ghost until reconciled — a rename-slice concern). `layoutCache.cats` stays
live for the ns-category list; `layoutCache.chans` is now vestigial (unread).
