# WEFT Plugin System

**Status:** design in progress (co-designed with the owner, 2026-08-03). A general
extension system with **server-side** and **client-side** reach. Its first proving use
case is the foreign-bridge framework's "custom client actions" (`foreign-bridge-framework.md`
§0/§10) — admins editing a bridged space from the client — but the system is generic:
bots, automod, integrations, dashboards, slash-commands.

**One-paragraph shape:** a plugin is trusted, operator-installed code that runs **in-process**
in weftd on one of two runtimes — **Rhai** (a light, hot-reloadable scripting tier) or **WASM**
(the full tier). It reacts to server events, acts through the normal server paths, reaches
outside over a host HTTP client, and persists state — and it **declares client actions** that
the stock client renders. The client itself runs **no plugin code**: it is a **server-driven-UI
(SDUI) renderer** for a bounded, typed component catalog. Plugins describe views; the client
renders them; interactions round-trip back to the plugin, which owns all flow logic.

## 1. Decisions locked (owner, design rounds 1–4, 2026-08-03)

| # | Axis | Decision |
|---|---|---|
| 1 | Audience / trust | **Operator-installed, trusted.** Isolation is for **stability + resource-bounding**, not defending against malice. No untrusted/community sandboxed tier yet (YAGNI). |
| 2 | Server execution | **In-process, two runtimes: Rhai (light) + WASM (full).** Not an external daemon. |
| 3 | Runtime tiers | **Same host-API surface on both** (owner call, round 5 — reverses the earlier light/full split). Rhai and WASM differ only in **execution character** (Rhai = scripting + hot-reload; WASM = compiled, heavier compute, other languages), **not** in capabilities. One host API to design + secure (§4). |
| 4 | Authority scoping | **Unrestricted (trusted).** No per-plugin capability confinement — a plugin may use anything its runtime tier exposes. Blast radius = the whole server, accepted. |
| 5 | Server powers | **All of:** event hooks · act-as-service · outbound network · storage + timers (§4). Plus declaring client actions (core). |
| 6 | Client execution | **Declarative SDUI.** The stock client renders plugin-declared views from a typed catalog; **no arbitrary client code**. |
| 7 | Plugin identity | **The plugin decides, per call:** its own **bot account**, a **system** identity, or **on behalf of the invoking user** (§5). |
| 8 | Action result | **Interactive** — multi-step forms/wizards + rendered panels, not just fire-and-forget (§6). |
| 9 | Action surfaces | **All:** context menus · slash commands · settings/admin panels · global (command palette + side panel) (§6). |
| 10 | Flow state | **Plugin-driven** — the plugin holds flow state; the client renders the current view and round-trips each step (§6). |
| 11 | Live views | **Push-updatable panels** — a plugin may push a fresh view into an open panel unsolicited (live dashboards/queues) (§6). |
| 12 | View richness | **Typed component catalog only** — no raw HTML / iframes. Safe, themeable, forward-compatible (§7). |

## 2. Goals / non-goals

**Goals:** first-party + operator-trusted extensibility of both server behavior and client UX,
without forking weftd or the client; a foreign-bridge-grade "custom actions" mechanism as a
special case; a safe, themeable, declarative client surface.

**Non-goals (for now):** an untrusted third-party marketplace, sandbox-escape-grade security,
client-side arbitrary code, cross-plugin isolation/scoping. Each is a deliberate deferral, not
an oversight — revisit if a community ecosystem materializes (then decision 1/4 change).

## 3. Where it lives (layering)

- **weft-proto (L0):** the `PLUGIN*` wire verbs (§8) + the **view/block codec** (the typed
  component catalog as pure, round-trip-tested types). No I/O — a plugin view is just wire data.
- **weft-core (L2):** **hook points** — the event stream a plugin host subscribes to (message
  posted, member join/part, …), exposed as a port so core stays I/O-free.
- **weftd (L3):** the **plugin host** — loads plugins, owns the Rhai/WASM runtimes, the host API
  (network, storage, timers, act-as-service), routes `PLUGIN` verbs, and drives SDUI to clients.
- **client (Tauri/Svelte):** the **SDUI renderer** — renders `PLUGIN-VIEW`/`PLUGIN-PATCH`, surfaces
  declared actions in the four surfaces, and round-trips `PLUGIN INVOKE`/`SUBMIT`/`ACTION`.

## 4. Runtimes & host API  *(owner-confirmed, round 5)*

**One host API, identical on both runtimes.** A plugin's manifest declares
`runtime = "rhai" | "wasm"` — a choice of authoring tool, not a capability tier. The full surface:

- declare + handle client actions (`on_invoke`, `on_submit`, `on_action`);
- **read/query** state (channels, members, messages within reach);
- **post messages** + act-as-service (create/modify channels, moderate) + emit views/patches;
- **event hooks** — each declares **observe** or **veto** (§6a);
- **outbound HTTP** (host-provided client — the capability most worth bounding: SSRF guard + secret injection);
- **durable scoped KV storage** (+ ephemeral per-flow state, host-held, keyed by view-id, for multi-step);
- **timers / scheduled tasks** (cron-like, independent of incoming events);
- choose identity per act (`as_bot` / `as_system` / `as_user`, §5).

**Rhai** = synchronous scripting, **hot-reloaded** on file change, best for small logic + actions.
**WASM** = compiled modules (other languages, heavier compute), its own memory, bounded by
fuel/epoch limits; reloads on operator command/restart. Both reach the same API; "unrestricted"
(decision 4) applies uniformly.

## 5. Plugin identity  *(proposed)*

A plugin **may** declare a **bot account** in its manifest (a reserved account handle + profile +
avatar), provisioned at load. The host API `act_as(...)` picks the actor **per call**:
- **bot** — autonomous work (timers, event hooks, unsolicited posts) appears as `plugin-bot@net`,
  attributable, DM-able, in rosters where the plugin operates;
- **system** — a serverside notice with no per-plugin identity (like existing join/part lines);
- **user** — a user-invoked client action runs *as the invoking user*, with that user's authority
  and attribution ("Ada created #general", not "the bot did").

Because authority is unrestricted (decision 4), acting as bot/system bypasses cap checks; acting
as-user still flows through the user's own capabilities (so a user can't exceed their rights via a
plugin).

## 6. The SDUI interaction model

**Action providers (owner call, round 5 — unify).** Declaring client actions + driving SDUI is a
**shared capability**, not plugin-only. Two kinds of provider speak the same `PLUGIN*`/`PLUGIN-*`
verbs (§8):
- **in-process plugins** (Rhai/WASM) — the common case;
- **pinned external daemons** on a `State::ForeignBridge` session — so the foreign-bridge adapter
  declares its own actions (`Create channel`, `Create subspace`, …) **directly** over its session
  and handles their invocations by relaying to the foreign system. **No companion plugin, no
  duplicated SDUI machinery** — the client + the SDUI router are provider-agnostic. (This is how the
  foreign-bridge "custom client actions" use case is served, `foreign-bridge-framework.md` §10.)

A provider **declares actions**; each action has: `id`, `label`, `icon`, **surface**
(context-menu / slash / settings / global), **context type** (message · channel · member · user ·
namespace · none), an **input schema** (typed params the client collects before invoking), and an
optional **visibility condition** (e.g. "only if the actor is a foreign-admin" — driven by advisory
role projection; the client shows/hides, the plugin re-checks).

**Containers:** a **modal** (transient action flows / wizards) and a **panel** (persistent — the
side-panel + settings surfaces; subscribable + live).

**Flow (plugin-driven, decision 10):**
```
1. user triggers an action  → client collects the input-schema form
2. client → server  PLUGIN INVOKE <plugin> <action> [context] {params}
3. host routes to the plugin's on_invoke; plugin returns EITHER
     • a VIEW (modal/panel) to render, OR
     • a terminal RESULT (toast / navigate / close / refresh)
4. user submits/clicks in the view
   client → server  PLUGIN SUBMIT <view-id> {values}   (or PLUGIN ACTION <view-id> <btn>)
5. plugin's on_submit returns the NEXT view or a terminal result   (it owns flow state)
… repeat until a terminal result closes the flow.
```

**Live panels (decision 11):** while a panel view is open the client holds a subscription; the
plugin may push `PLUGIN-PATCH <view-id> …` at any time (a dashboard tick, a queue update, a
progress bar). Closing the panel unsubscribes.

### 6a. Event hooks — observe vs. veto (owner call, round 5)

Each hook **declares its kind**, so only gatekeepers pay the hot-path cost:
- **observe** — fires **post-commit, async**. Zero latency on the watched action. For notify / log /
  delete-after / integrations. Cannot block anything.
- **veto** — fires **pre-commit**, inside a **bounded deadline**, and may **deny** the action (block a
  post, reject a join/redeem). Only veto hooks add latency, and only to the actions they subscribe.
  Real automod (reject-before-visible) without taxing every observer.

A veto that misses its deadline follows a per-hook **fail-open | fail-closed** policy (default
**fail-open** — a hung automod plugin degrades to "allow + log", never wedges posting). Veto hooks
run before the §10.4 capability side-effect commits, consistent with "capability checks precede side
effects" (invariant 4).

## 7. The component catalog (typed, decision 12)  *(strawman — extend deliberately)*

Pure `weft-proto` types, serialized as a structured payload (CBOR, base64 in the verb — like
signed manifests / capability tokens). The client renders **only** known components; an unknown
component type is ignored (forward-compatible), never executed.

- **Inputs:** `text` · `number` · `select` · `multiselect` · `toggle` · `date` (each: id, label, required, default, validation).
- **Display:** `heading` · `markdown` · `divider` · `keyvalue` · `table` · `image`.
- **Controls:** `button` (with a style + an action id) · `action-row`.

Adding a widget is a deliberate catalog + codec change we control — never raw HTML/iframes (the
escape hatch was explicitly rejected: it reintroduces the client-code/security/theming surface the
declarative choice removes).

## 8. Wire protocol  *(proposed — netcat-debuggable text control plane, §4-style)*

Client → server:
- `PLUGINS` — request the installed-plugin manifest (declared actions + surfaces). Also pushed on change.
- `PLUGIN INVOKE <plugin> <action> [ctx] {params}` — trigger an action.
- `PLUGIN SUBMIT <view-id> {values}` — submit a form step.
- `PLUGIN ACTION <view-id> <button-id>` — a control click.
- `PLUGIN SUBSCRIBE <view-id>` / `PLUGIN UNSUBSCRIBE <view-id>` — panel liveness.
- `PLUGIN CLOSE <view-id>` — user dismissed a view.

Server → client:
- `PLUGIN-MANIFEST {plugins,actions}` — the declared catalog (also the `PLUGINS` reply).
- `PLUGIN-VIEW <view-id> container=modal|panel {blocks}` — render / replace a view.
- `PLUGIN-PATCH <view-id> {blocks|ops}` — push an update into a live panel.
- `PLUGIN-RESULT <view-id> {toast|navigate|close|refresh}` — terminal outcome.

Structured payloads (`{…}`) ride as base64-CBOR, consistent with how weft carries manifests/tokens.
`view-id` correlates a flow; `label` echo (§3.5) acks the triggering request.

## 9. Packaging, lifecycle, config  *(proposed)*

- **Package:** a manifest (`plugin.toml`) + the Rhai script(s) **or** a `.wasm` module + assets
  (icon). Manifest declares: `id`, `name`, `runtime`, `entrypoint`, optional `bot_account`,
  the **declared actions** (§6), and **requested config keys** (incl. secrets).
- **Install:** operator drops the package in `[plugins] dir` (or lists it in config). Loaded at
  boot; **Rhai hot-reloads** on file change; WASM reloads on operator command/restart.
- **Config / secrets:** operator sets `[plugins.<id>.config]` (e.g. an API key for outbound HTTP),
  injected via a host `config`/`secret` API (WASM tier). Secrets never leave the host.
- **Enable/disable** per plugin; a failing plugin is quarantined (logged, disabled) — never wedges weftd.

## 10. Isolation for stability (not security)

Trusted, so this bounds **bugs**, not adversaries: WASM under wasmtime with **fuel/epoch timeouts**
+ memory caps; Rhai with operation limits + timeouts. A panic/hang is caught + the plugin
quarantined. In-process, so one plugin can still corrupt shared state it's allowed to touch — that's
the accepted cost of decision 4 (unrestricted, trusted).

## 11. Round-5 resolutions (owner, 2026-08-03)

1. **Client-only plugins — DEFERRED.** Every v1 plugin is server-rooted; the client is extended only
   through server-declared SDUI (which already satisfies "client-side reach"). Purely client-only
   plugins (themes, keybinds, launchers) become a separate **client-customization** feature later —
   no client-side plugin loader or built-in-intent vocabulary in v1.
2. **Runtime tiers — COLLAPSED.** Rhai and WASM share **one** host-API surface (§4); they differ only
   in execution character. (Reverses the earlier light/full split.)
3. **Foreign-bridge — UNIFIED via action providers (§6).** Both in-process plugins and pinned external
   daemons speak the same action/SDUI protocol; the bridge declares + handles its actions directly.
   No companion plugin.
4. **Distribution — operator files + config, no registry** (trusted, YAGNI).
5. **Event hooks — declared observe | veto (§6a).** Observers post-commit/async; veto pre-commit/bounded
   with fail-open default.

## 12. Remaining before build

The shape is settled. Next design step (all pre-code): pin the concrete **weft-proto** types — the
`PLUGIN*` client verbs, the `PLUGIN-*` server events, and the **component/view codec** (the typed
catalog as round-trip-tested L0 types) — then the **host-API trait** in weftd and the **hook port**
in weft-core. Per the workspace rule, the proto types + round-trip tests come first.
