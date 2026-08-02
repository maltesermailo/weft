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

**Deferred:** the ns-category *list* (`moveCategory`/`setCategories`/`nsCategories`) stays TS.
`layoutCache.cats` stays live for the ns-category list; `layoutCache.chans` is now vestigial (unread).

## Rename + delete slice — DONE (model owns the channel identity lifecycle)

The model now owns `channel-renamed` + CHANNEL `deleted`, closing the rename-ghost gap (the
persisted layout re-keys with the channel instead of stranding the old name):
- **Model** (`channels.rs`): `renamed(old, new)` re-keys `ChannelState` old→new, clears the stale
  vanity, and marks the (name-keyed) layout dirty so persistence follows; `deleted(channel)` drops
  the state. Two new diffs — `ChanRenamed { old, new }` and `ChanRemoved { name }` — tell the mirror
  to re-key / drop its instance. `chanmeta` routes `deleted` here *before* the `or_default()` entry
  so a delete never resurrects a channel. Idempotent (rename arrives as broadcast + labeled copy).
- **TS flip**: `channelMirrorHandlers` gains `chan-renamed` (re-key the `Channel` instance —
  unread/mention tallies + messages ride it — and clear vanity) and `chan-removed` (drop it).
  `channel-handlers.ts` keeps only the **side-effects**: `chanmeta` deleted → leave the view;
  `channel-renamed` → nav + `ui.chanPerms` re-target + `weft.join(new)` re-subscribe + toast. The
  record re-key / removal / vanity-clear / `cacheChanLayout` are gone from TS (model-owned now).

**Deferred:** the ns-category *list* (above); `cacheChanLayout` is now unused on the rename path
(`layoutCache.chans` fully vestigial).

## Seed reconciliation slice — DONE (instant first paint, no ghosts)

The proactive layout seed is back on — the sidebar paints its last-known channel order instantly on
connect — now made ghost-safe by SYNC-end reconciliation (the seed was disabled earlier because a
stale cache stranded channels deleted/left while offline, which also wedged the history
single-flight):
- **Model** (`channels.rs`): a `provisional` set tracks cache-seeded channels. `seed()` marks each
  seeded channel provisional (and now only restores **namespaced** channels — the only ones with a
  layout *and* a confirming server event; `serialize` filters the same way). Any live event for a
  channel (`layout`/`chanmeta`/`renamed`) **confirms** it (clears provisional). On `SyncEnd` —
  which the server sends *after* a `CHANNEL-LAYOUT` per visible namespaced channel — `reconcile_seed`
  prunes every still-provisional channel (drops it + emits `ChanRemoved`), so a stale entry can never
  linger. The layout blob is marked dirty on prune so the host re-saves the cleaned cache.
- **Hosts**: both wrappers `seed_layout(blob)` again on connect (`load_layout` restored — file in
  app-data for desktop, `localStorage["weft:chan-layout"]` for web) and emit the diffs. Reconnect
  re-seeds + re-reconciles the same way (the server re-enumerates on every SYNC).
- **TS**: no changes — the seed rides the existing `chan-state` mirror handler and the prune rides
  the `chan-removed` handler (from the rename/delete slice).

**Edge:** deep-linking to a channel deleted-while-offline shows it until `SyncEnd` prunes it, then the
route degrades to EmptyHome (the record is gone) — acceptable; the ghost is transient, not permanent.

## Category-list slice — DONE (model owns categories; the TS `layoutCache` is gone)

The per-namespace category list (Discord-style headers) is now model-owned, and with it the **whole TS
`layoutCache`** (`weft:layout`) is deleted — all channel-layout persistence lives in one Rust blob:
- **Model** (`channels.rs`): a `categories: BTreeMap<ns, Vec<String>>`. `set_categories` adopts the
  server-authoritative list from `NsMeta` (a **no-op when unchanged** — NS-META fires on every ns
  update); `move_category(ns, drag, target)` ports the TS reorder and returns a `CatList` diff (instant
  UI) + the `NS META <ns> categories <list>` write. New `ChanDiff::CatList { ns, categories }`. The
  persisted blob became `LayoutBlob { channels, categories }` (both `#[serde(default)]` → older
  channel-only caches still load); `serialize`/`seed` carry categories too (seed emits a `CatList` per
  ns — categories need no provisional reconcile, since NS-META replaces the whole list).
- **Hosts**: `move_category` command — web dispatch (`build_ns_meta` send + explicit save) and a desktop
  `#[tauri::command]` (relies on the NS-META echo to save, like `move_channel`).
- **TS**: `weft.moveCategory` + a `cat-list` mirror handler (`store.server(ns).categories = …`).
  `Server.applyMeta` no longer sets `categories` (the `cat-list` diff from the same NS-META does);
  the reducer's `cacheNsCats` and the viewmodel's `layoutCache` fallback are gone; `channel.svelte.ts`
  lost `layoutCache`/`saveLayout`/`loadLayout`/`cacheNsCats`/`cacheChanLayout` + the `NsLayout` type +
  the boot `loadLayout()`. `nsCategories()`/`setCategories()` stay (category **add/remove** still send
  NS-META and update via the Rust echo path).

## Roster slice — DONE (model owns the channel member list)

The per-channel member roster is now model-owned; the cross-domain side-effects stay in TS:
- **Model** (`channels.rs`): `roster: BTreeMap<channel, Vec<RosterMember{account, network}>>` (transient —
  rebuilt from events each session). `member` handles `MEMBER join`/`part` (incl. the MEMBERS batch):
  dedup-add on join, remove on part, **no-op when unchanged** (a re-fetch's duplicate join, a part of
  someone absent). Emits the **full list** as `ChanDiff::Roster` — idempotent, so a reconnect's re-sync
  replaces cleanly (no incremental-drift/duplication). Roster follows `renamed` (re-key) and clears on
  `deleted`. `network` is carried raw — the mirror resolves local/federated origin (needs the home net).
- **TS**: a `roster` mirror handler sets `Channel.members` (resolving `origin` from `store.session.network`)
  **only on an existing instance** — so a self-part (which deletes the instance first) makes the trailing
  roster diff no-op instead of resurrecting a ghost. The `member` handler keeps its side-effects
  (`ensureCaps`/`queryProfile`/self-join nav+presence/other-online) + the self-part leave (delete + nav,
  which needs "me"); it no longer touches `Channel.members`.

## Typing slice — DONE (model owns the typing set; timer stays host-side)

- **Model** (`channels.rs`): `typers: BTreeMap<channel, Vec<String>>` (transient). `typing` handles
  `TYPING start`/`stop` — dedup-add / remove, **no-op when unchanged** — and emits the full list as
  `ChanDiff::Typers`. Follows `renamed`, clears on `deleted`. Never holds "me": the server broadcasts
  typing **excluding the origin**, so self-typing is never received (no session knowledge needed).
- **The 6s fallback-expiry timer stays host-side** (a timer isn't pure-model): on expiry the host fires
  a **local-only `typing_stop` command** (`AppState::typing_stop` → `Typers` diff, no server write) —
  web dispatch + a desktop `#[tauri::command]`, plus `weft.typingStop`.
- **TS**: a `typers` mirror handler sets `Channel.typers` (existing instance only). `Channel.setTyping`
  is now **timer-only** — it arms/clears the per-user 6s timer whose expiry calls `weft.typingStop`; the
  set itself is model-owned. The reducer's `typing` case is unchanged (still calls `setTyping`).

**Migration status:** the channels domain owns metadata + layout + rename/delete + seed-reconcile +
categories + roster + typing. Remaining channel pieces: unread/mention (needs `mentionsMe` = roles/caps
first — cross-domain), and the big **messages + history + optimistic send** slice.

## Presence slice — DONE (first non-channel domain)

The first domain module that isn't `channels`, proving the multi-domain shape (the `StateDiff` enum now
aggregates `Chan(..)` + `Presence(..)`; `AppState` gained a `presence` field; `reduce` offers each event
to both):
- **Model** (new `model/presence.rs`): `Presence { map: BTreeMap<account, status> }` (transient). The
  `Presence` wire event sets it, **no-op when unchanged** (presence is re-announced on join/reconnect),
  emitting `PresenceDiff::AcctPresence { account, status }`. The diff kind is **`acct-presence`** —
  deliberately distinct from the raw `presence` wire event so the model's diff and the wire event never
  collide in the TS handler map.
- **TS**: `accountHandlers` swapped its raw `presence` handler for an `acct-presence` mirror
  (`store.accountOf(account).presence = status`). The raw `presence` event now flows to TS unhandled
  (the model owns it). No wrapper changes — the presence diff rides the sink generically.

Note the two presence writes that intentionally stay in TS: the optimistic `??= "online"` on a member
join (best-effort, the real event confirms via the model) and `store.session.myStatus` for *my own*
status (the server never echoes my presence back to me, so the model never sees it).

## Moderation slice — DONE (deny-list cache; second non-channel domain)

The §6.7 mute/ban deny-list cache is model-owned (`StateDiff` now aggregates `Chan` + `Presence` +
`Mod`):
- **Model** (new `model/moderation.rs`): `deny: BTreeMap<scope, Vec<DenyRow{account, kind, by, reason}>>`
  (transient). `MODERATED` carries both sides — `mute`/`ban` add-or-replace (re-mute updates
  `by`/`reason` in place), `unmute`/`unban` remove, `kick` transient (no entry) — each emitting the
  scope's full list as `ModDiff::Deny` (idempotent → MOD LIST re-fetch / reconnect replaces cleanly).
  A `mod_refresh` command clears a scope ahead of the MOD LIST re-fetch.
- **TS**: `moderationHandlers` swapped its raw `moderated` handler for a `deny` mirror
  (`store.deny.set(scope, rows)`); `refreshBans` now calls `weft.modRefresh(scope)` (the model clear)
  then `weft.modList`. The **gate stays TS** — `banScope()`, the covering-scope walk, and `can_post`
  are unchanged; only the cache moved. `mod_refresh` command wired in both wrappers (local-only, like
  `typing_stop`).

**Federation was deliberately skipped** for now: its `netblocks` half has optimistic clear/remove plus a
protocol quirk (the removal echo `NETBLOCKED{reason:None}` re-adds the entry via `applyNetblock`), so a
faithful migration would preserve a latent bug — better handled as its own considered change than folded
into a routine slice.

## Reports slice — DONE (report queue; third non-channel domain)

The §6.7 moderation report queue is model-owned (`StateDiff` now: `Chan` + `Presence` + `Mod` + `Report`):
- **Model** (new `model/reports.rs`): `queue: BTreeMap<report_id, ReportInfo{report_id, msgid, category,
  state, reporter}>` (transient; fetched on demand). `REPORT-FILED` adds, `REPORT-RESOLVED` removes (no-op
  if absent) — each emitting the whole queue as `ReportDiff::Reports` (idempotent; report_id/ULID order).
  A `reports_clear` command backs the modal's open-reset.
- **TS**: `reportsHandlers` dropped its `report-filed` handler and the `queue.delete` in `report-resolved`
  — the model owns the queue via a `reports` mirror (rebuild `store.reports.queue`). The two `sys(…)`
  channel confirmations (report filed / resolved) **stay** (they're system lines, not queue state), as do
  the modal's `open`/`target` UI. `openReports` now calls `weft.reportsClear()` then `weft.reportsList`.
  `reports_clear` wired in both wrappers (local-only). (The logout-time `queue.clear()` in
  `connection.svelte` stays a direct UI reset — the model resets on the next connect anyway.)

## Emoji slice — DONE (namespace custom-emoji map; fourth non-channel domain)

The §9.4 per-namespace `:name:` → media map is model-owned (`StateDiff`: `Chan` + `Presence` + `Mod` +
`Report` + `Emoji`):
- **Model** (new `model/emoji.rs`): `map: BTreeMap<ns, BTreeMap<name, media>>` (transient). `EMOJI` sets
  (no-op when unchanged — a re-announce), `EMOJI-REMOVED` drops (no-op if absent). **Incremental** diffs
  (`EmojiSet`/`EmojiDrop`, one entry each — matching the event granularity) rather than a full-map, since
  a namespace can carry many emoji. No command (purely event-driven).
- **TS**: `serverHandlers` swapped its raw `emoji`/`emoji-removed` handlers for `emoji-set`/`emoji-drop`
  mirrors that apply onto `Server.emoji` and keep the **`clearMdCache()`** side-effect (a `:name:` render
  changed). No wrapper changes.

## Roles slice — DONE (role defs + membership; done to unblock messages' `mentionsMe`)

The §6.5 role **definitions** (per scope) + **membership** (per `account|scope`) are model-owned. This
was picked *before* messages because messages' unread/mention needs `mentionsMe`, which reads role data.
- **Model** (new `model/roles.rs`): a **batch transformer** — `ROLE` events buffer grouped by the event's
  own `scope`; the `r…`-prefixed `BATCH END` flushes each scope's buffer, sorted by `position`, as a
  `RoleDiff::RoleList { scope, roles }` that **replaces** the scope's list. `ROLE-MEMBER` → a direct
  `MemberRoles` diff. Because the `ROLE` event carries `scope`, the fragile TS `roleFetchQueue`
  scope-cursor is **gone** — the model routes by the event's scope. The role *data* lives in the mirror;
  the model owns the batch *logic* + transform. (Model-side role storage + `mentions_me` come with the
  **messages** slice, which consumes them — kept out here to avoid dead code.)
- **TS**: `rolesHandlers` swapped `role`/`role-member` for `role-list` (route ns→`Server.roles` else
  `rolesByScope`, rebuild `Role` instances, `clearMdCache`) + `member-roles` mirrors; `grant-info` stays.
  Removed: `roleBuf`, `roleFetchQueue` (+ all its `.push` in fetch/create/delete/reorder/save), and the
  reducer's `r…` role-batch flush (now a bare boundary-consume). **Untouched:** `session.caps` and every
  gate (`can`/`moderates`/`canGrant`), grants, and `mentionsMe` (reads the now-mirror-fed data). No
  wrapper changes.

**⚠ Smoke-test after this slice (security-adjacent):** the role editor (create/edit/delete/reorder), role
display (member badges, name colors, hoisting), `@role` mention pinging, and — belt-and-suspenders — the
permission gates (which use the untouched `session.caps`).

**Migration status:** the model owns **channels** + **presence** + **moderation** + **reports** + **emoji**
+ **roles** (six domains). Remaining: the **messages** capstone (below), plus **invites**, **federation**
(netblock quirk), the **ns-meta descriptor**, and **social/threads**.

## Messages capstone — the store model (design + M1 done)

Messages is the domain where the Rust model earns its keep most, but the split must be **sharper** than
for the metadata domains, because it's the one domain with an *unbounded buffer* and a *scroll-coupled
render path*. Get it wrong and you either ship megabytes over IPC or put scroll logic in Rust.

**The line:**
- **Rust owns the store (what is *true*):** the per-channel ordered buffer (id + modseq, gap/continuity —
  the SYNC reconciliation surface), the ordering-sensitive **mutation semantics** (edit, redact, react,
  **local-echo → ack reconcile** as first-class `pending`/`failed` state), unread/mention derivation
  (feeds sidebar/notifications, so it can't live in the view), and pagination cursors.
- **TS owns the render window (how it *looks*):** virtualized list state (scroll, anchor "pin to bottom
  unless scrolled up", item-height caches, sticky day dividers), display grouping/coalescing (pure
  presentation), composer/drafts/typing. The store carries **no clock** — the window derives `ts`/`time`
  from the message `id`.
- **IPC — two tiers, never stream the buffer:** a **pull** `messages_range { channel, before, limit }`
  for bodies-in-bulk (JSON first, measure; binary later if pages get heavy); **thin push** diffs
  (`MsgAppended`/`MsgUpdated`/`MsgRemoved`/`RangeInvalidated`) for the live tail, **scoped to channels
  the frontend declared open** — background channels get only the cheap `UnreadChanged { channel, count,
  mentions }`. TS holds a *dumb window cache* (materialized range + seq watermark), not a second store;
  on a gap/`RangeInvalidated` it **refetches** the window (snapshot-recovery: diffs for speed, refetch
  for truth). Local-echo: send → `send_message` → Rust inserts `pending` → `MsgAppended(pending)` →
  instant render; ack → `MsgUpdated` swaps in the server id.

**Why not keep it in TS:** the WASM host would need a duplicate splice/dedup/reconcile impl; unread
derivation feeds non-message surfaces and desyncs when computed in the view; and modseq reconciliation
*is* the protocol — two implementations disagree exactly on the sync edge case. (The one real cost —
scroll-preserving prepends become an async fetch — is solved by anchor-based virtualization; the
"network" is a sub-µs local hop.) It's the migration's **capstone** — not urgent while the TS path works,
but new features (edits/reactions/threads) get designed into the Rust model from the start so the TS
version stops growing.

**Phases:** **M1** isolated store + semantics + thin diffs + `range` reader (unit-tested, unwired) →
**M2** modseq/gap ordering + unread/mention derivation (uses migrated roles + `me`) → **M3** the two-tier
IPC (`messages_range` + `send_message` commands, open-channel subscription scoping, `UnreadChanged`) →
**M4** TS cutover (window cache + anchor virtualization + refetch-on-gap; gut the reducer's message path).

**M1 — DONE (isolated, unwired).** `model/messages.rs`: `Msg` (incl. `pending`/`failed`/`reactions`) +
per-channel `buffers` + `me`/`home` from `Connected`. Semantics: `insert_pending` (local echo),
`ingest` (reconcile-by-label / upsert-by-id keeping reactions / append), `edit`, `redact`, `react`
(ported `applyReaction`), `fail_pending`, and a `range(channel, before, limit)` reader. Thin
`MsgAppended`/`MsgUpdated`/`MsgRemoved` diffs (`MsgUpdated` targets by *current* id, so a local→server ack
is a clean update). **Not in `reduce`/`AppState`** — the app stays on the TS path until M4.

**M2 — DONE (unread/mention derivation; isolated).** Per-channel `unread: {count, mentions}` — the
model's authoritative tally, display-gating (mute/active) stays TS. `ingest` now takes a `mentioned` flag
and **bumps** on a fresh non-own append (`+count`, `+mentions` when mentioned); `MARKED` **clears** it;
`UNREAD-COUNTS` sets the **authoritative** server tally — each emitting a `UnreadChanged { channel, count,
mentions }` diff (the cheap derived push a *background* channel gets instead of the body). `ingest` moved
off `handle` (it needs the cross-domain `mentioned`, computed by `AppState` from the roles domain at
wiring); `handle` now covers `Marked`/`UnreadCounts` + the no-cross-domain mutations. 7 tests.

**Scope note:** the `mentioned` derivation itself (`mentions_me` reading role membership + pingable roles)
lands with the **wiring** (M3) — that's where `AppState` reaches across domains and where the roles model
gains the small stored copy it needs; adding it now would be unconsumed dead code. **modseq/gap ordering**
folds into **M3** (the pull/history path) — it isn't exercised until out-of-order/older messages arrive,
so implementing it now would be untested speculation.
