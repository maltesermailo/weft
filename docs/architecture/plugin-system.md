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
| 2 | Server execution | **Remote-first (round 7).** A plugin has three hosting modes over one API: **`remote`** (an external process — the Matrix App Service model — over a pinned-key session), plus in-process **`rhai`** and **`wasm`**. `remote` is **built first**; in-process is deferred. A foreign-bridge adapter becomes a `remote` plugin + realm verbs. |
| 3 | Runtime tiers | **Same host-API surface on both** (owner call, round 5 — reverses the earlier light/full split). Rhai and WASM differ only in **execution character** (Rhai = scripting + hot-reload; WASM = compiled, heavier compute, other languages), **not** in capabilities. One host API to design + secure (§4). |
| 4 | Authority scoping | **Unrestricted (trusted).** No per-plugin capability confinement — a plugin may use anything its runtime tier exposes. Blast radius = the whole server, accepted. |
| 5 | Server powers | **All of:** event hooks · act-as-service · outbound network · storage + timers (§4). Plus declaring client actions (core). |
| 6 | Client execution | **Declarative SDUI + sandboxed client-side Rhai (round 7 — supersedes "no client code").** The stock client renders SDUI from a typed catalog **and** runs an optional **client-side Rhai controller** (in `weft-client-core`) that orchestrates custom-view widgets + client UX. Still **no arbitrary JS** — Rhai is a sandbox by construction (only the bound API; never `__TAURI__`/DOM/eval). |
| 7 | Plugin identity | **The plugin decides, per call:** its own **bot account**, a **system** identity, or **on behalf of the invoking user** (§5). |
| 8 | Action result | **Interactive** — multi-step forms/wizards + rendered panels, not just fire-and-forget (§6). |
| 9 | Action surfaces | **Six:** context menus · slash commands · settings/admin panels · global (palette + side panel) · **server-menu** (namespace header dropdown) · **channel-list** (sidebar) — the last two added round 6 (`plugin-spec.md` §12.1). |
| 10 | Flow state | **Plugin-driven** — the plugin holds flow state; the client renders the current view and round-trips each step (§6). |
| 11 | Live views | **Push-updatable panels** — a plugin may push a fresh view into an open panel unsolicited (live dashboards/queues) (§6). |
| 12 | View richness | **Two render surfaces (round 7).** (a) **Declarative SDUI** — the typed catalog, native-themed, for lightweight surfaces (action inputs, settings, simple panels). (b) **Custom-view widgets** — a plugin's own web UI in a **sandboxed null-origin iframe** (Matrix-widget model) for bespoke views (role-editor-class). Custom UI is delivered by **isolation, not HTML sanitization**. |
| 13 | Registration | **Unified code registration (round 6).** Actions, hooks, and timers all register in a load-time `register()` pass, **not** the manifest — which keeps only identity/runtime/bot/config. `plugin-spec.md` §5.4. |
| 14 | Custom-view control | **Client-side Rhai (round 7).** The client controller mounts/places/destroys widgets, routes their `postMessage`, subscribes to client events, and drives SDUI locally — a safe client scripting layer (Rhai sandbox), reached through a curated client API broker (never the ~123 raw Tauri commands). |
| 15 | Webview hardening | **CSP required (round 7).** The Tauri CSP is currently `null` (no defense-in-depth); a real policy + a scoped `frame-src` for widget origins is a prerequisite for the iframe surface — a security improvement independent of plugins. |

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

## 11a. Round-6 resolutions (owner, 2026-08-03)

1. **Unified code registration.** Actions, hooks, and timers all register in a load-time `register()`
   pass; the manifest keeps only identity/runtime/bot/config (`plugin-spec.md` §5.4, §6.11). Handlers
   are inline closures (Rhai) / registration-token exports (WASM).
2. **Two new action surfaces — `server-menu` + `channel-list`** (`plugin-spec.md` §12.1), bringing the
   total to six. Both invoke with `context = namespace`.

## 11b. Round-7 resolutions (owner, 2026-08-03)

1. **Remote-first hosting (App Services).** A plugin's hosting is `remote` | `rhai` | `wasm` over one
   API; **`remote` is built first**, in-process later. `remote` = the Matrix Application Service model —
   an external process on a pinned-key session, receiving event pushes and calling back to act as its
   users. The foreign-bridge adapter becomes a `remote` plugin + realm verbs; `State::ForeignBridge`
   generalizes to the remote-plugin transport. A remote plugin brings its **own** I/O (HTTP/timers/DB),
   so the host-provided `http`/`timers`/`kv` are conveniences only the *in-process* tiers need.
2. **Two render surfaces (supersedes decision 12's "no iframes").** (a) declarative SDUI (native, light);
   (b) **custom-view widgets** — the plugin's own web UI in a `sandbox="allow-scripts"` **null-origin**
   iframe (no `allow-same-origin` ⇒ no `__TAURI__`, no command access) with a `postMessage` capability
   bridge. This is the Matrix-widget model; custom UI comes from **isolation, not sanitization**. The
   earlier "sanitize plugin HTML into the main webview" idea is **dropped** (main context = csp:null +
   ~123 commands; too dangerous).
3. **Client-side Rhai controller.** An optional per-plugin client script, run in a Rhai sandbox inside
   `weft-client-core` (one runtime, WASM-for-web + native-for-desktop), that **controls the widgets** and
   client UX: mount/place/destroy widgets, route their messages, subscribe to client events, drive SDUI
   locally. Safe by construction — Rhai reaches only a **curated client API broker**, never the raw Tauri
   commands, DOM, or `eval`. This is the safe form of "client-side scripts for GUI"; it reverses round-5's
   "no client code" but keeps "no arbitrary JS."
4. **CSP hardening is a prerequisite.** The Tauri CSP is `null` today; a real policy (+ a scoped
   `frame-src` for widget origins, + a command allowlist behind the broker) must land with the widget
   surface. A security improvement in its own right (a latent XSS→full-compromise risk today).

## 12. Remaining before build

The shape is settled. **The complete normative specification lives in `plugin-spec.md`** — now
**consolidated through round 7**: the three hosting models (remote-first), the in-process package vs
remote self-description, lifecycle (in-process states + remote connection), the shared handler
contract + all three runtime bindings, the full host-API reference (with the in-process-only
`http`/`timers`/`kv`), the hook catalog, the SDUI catalog + widget + client-controller surfaces, the
wire grammar (incl. `PLUGIN-REGISTER`/`container=custom`), limits, errors, 12 security invariants (as
tests), foreign-bridge integration, a 13-milestone 3-track build plan, and worked examples (remote-Rust
first). This doc remains the *design rationale*; `plugin-spec.md` is the *implementer's reference*.

Six decisions are flagged **open** in `plugin-spec.md` §19 (hook catalog, component catalog, the
`DENIED` code, the WASM ABI, the tier-collapse confirmation, and slash-arg mapping) — each gates a
specific milestone and none blocks starting M-plug-0/1. Per the workspace rule, the L0 proto types +
round-trip tests (M-plug-1) come first once those two catalogs are ratified.
