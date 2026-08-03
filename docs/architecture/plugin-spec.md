# WEFT Plugin System — Specification

**Status:** normative specification (pre-code), 2026-08-03 (consolidated through design round 7).
Companion to the design/rationale doc `plugin-system.md` (which records *why* each decision was made).
This doc is the *what* — the complete, implementable reference: hosting models, manifest/registration,
lifecycle, runtimes, the host-API surface, the hook catalog, the SDUI component catalog, the widget +
client-controller surfaces, the wire grammar, limits, errors, security invariants, and the build plan.
Genuinely open forks are marked **DECISION** and collected in §19.

Conventions: wire grammar follows the WEFT control plane (`weft-spec-v0.13.adoc` §4) —
`@tags VERB params :trailing`, lenient-in/strict-out, `label` echo on direct responses (§3.5).
Structured payloads ride as **base64-CBOR in a tag** (`@key=<b64>`), exactly as signed manifests and
capability tokens already do (`ciborium` + `weft_crypto::b64`). "MUST/SHOULD/MAY" are normative.

---

## Table of contents

1. Terminology
2. Architecture & layering
3. Hosting models & render surfaces
4. Plugin identity & registration (in-process package vs remote self-description)
5. Lifecycle & state
6. Runtimes & the handler contract
7. Host-API reference
8. Event-hook catalog & semantics
9. Identity & `act_as`
10. SDUI — component catalog
11. SDUI — views, flows, panels, patches, widgets
12. Wire protocol — verbs, events, grammar
13. Action declaration — surfaces, contexts, visibility
14. Configuration & secrets
15. Isolation & resource limits
16. Error taxonomy
17. Security invariants (implement as tests)
18. Foreign-bridge integration
19. Build milestones
20. Open decisions
21. Worked examples

---

## 1. Terminology

- **Plugin** — a trusted, operator-authorized extension. It is **hosted** one of three ways (§3.1):
  `remote` (an external process), `rhai` or `wasm` (in-process). It may contribute a **server side**
  (hooks, act-as-service, actions), a **client controller** (§3.4), and/or **custom views** (§3.3).
- **App Service** — the `remote` hosting mode: an external process on a pinned-key session (the Matrix
  Application-Service model). Built on the `weft-appservice` SDK (§3.5). A bridge is an App Service.
- **Provider** — anything that declares client actions + handles their invocations over the `PLUGIN*`
  protocol: a plugin (any hosting mode) or the foreign-bridge adapter (§18). Used when the distinction
  doesn't matter.
- **Host** — the weftd-side machinery (crate `weft-plugin`) that authenticates remote plugins, loads
  in-process ones, routes `PLUGIN` verbs, pushes events, and maintains the action catalog + SDUI router.
- **Action** — a provider-declared, client-invocable operation with a surface, context, and input schema.
- **View** — an SDUI screen (a tree of typed components) in a **modal** or **panel** container, **or** a
  **widget** (a custom web UI in a sandboxed iframe, `container = custom`).
- **Flow** — a multi-step interaction: a sequence of views owned by the provider, correlated by a **view-id**.
- **Hook** — a plugin subscription to a server event, declared **observe** or **veto**.
- **Actor** — the identity a server-side action runs as: **bot**, **system**, or **user** (§9).
- **Widget** — a plugin's own web UI rendered in a `sandbox="allow-scripts"` null-origin iframe (§3.3).
- **Client controller** — an optional per-plugin **client-side Rhai** script that drives widgets + client
  UX from inside `weft-client-core` (§3.4).

## 2. Architecture & layering

```
weft-proto     (L0)  PLUGIN* verbs + PLUGIN-* events + the SDUI codec (component/view/patch/result/widget
                     types) + the action-catalog wire type. Pure, round-trip-tested. No I/O, no engines.
weft-core      (L2)  the HookPort: a trait weftd implements so core can surface hookable events + consult
                     veto hooks at the pre-commit point, without core knowing about plugins or engines.
weft-plugin    (L3)  NEW crate. The weftd-side host: the remote-plugin session router + action catalog +
                     SDUI router (view-id / panel-key registries) + event-push fan-out; later, the
                     in-process Rhai/wasmtime engines + host-API impl. Depends on proto, core, store.
weft-appservice(—)   NEW client SDK (sibling to weft-tui) for building a `remote` plugin / App Service:
                     connection + AUTH ADAPTER handshake + registration + dispatch loop + Ctx. Deps
                     proto, transport, crypto, tokio. NO core/weftd/store dep. `bridge` feature = §18.
weftd          (L3)  owns a weft-plugin Host, generalizes State::ForeignBridge → State::PluginService,
                     wires PLUGIN verbs, installs the HookPort, provisions bot accounts, injects config.
weft-client-core(—)  hosts the client-side Rhai controller runtime (§3.4) + the SDUI/widget model twins;
                     compiled to WASM-for-web + native-for-desktop (one runtime, both targets).
client (Tauri/Svelte) SDUI renderer + widget host (sandboxed iframe + postMessage broker) + the CSP/
                     command-allowlist. Renders PLUGIN-VIEW/PATCH across the six surfaces (§13.1).
```

**Layering rule (STRICT, per CLAUDE.md):** the SDUI/verb/widget *types* are L0 (fuzzable, no tokio). No
script **engine** ever lives in proto/core — the in-process Rhai/wasmtime engines are confined to
`weft-plugin` (L3); the client-side Rhai engine is confined to `weft-client-core`. `weft-core` learns only
an abstract `HookPort`. New deps (`rhai`, `wasmtime`) are introduced only in their milestones and gate on
`cargo deny`.

**Data flow — a user-invoked action against a `remote` plugin (the primary path):**
```
client            weftd session          weft-plugin Host      remote plugin (App Service)
  │ PLUGIN INVOKE ────►│                        │                        │
  │                    │ route(invoke) ────────►│  push over session ───►│  on_invoke(ctx,p)
  │                    │                        │◄── PLUGIN-VIEW/RESULT ──│
  │◄── PLUGIN-VIEW ────│◄───────────────────────│                        │
  │ (render modal)     │                        │                        │
  │ PLUGIN SUBMIT ────►│ route(submit) ────────►│  push ────────────────►│  on_submit → View|Result
  │◄── PLUGIN-VIEW|RES ─│◄───────────────────────│                        │
```

**Data flow — a veto hook (pre-commit):**
```
session about to commit a MSG  ─► HookPort.veto("message.posted", payload, deadline)
   Host runs each veto hook (in-process only — §8.3; remote plugins are observe-only) ─► allow | deny
   deny  ─► session answers the poster ERR (the effect never commits)
   allow ─► commit proceeds; observe hooks fire post-commit, async
```

## 3. Hosting models & render surfaces

A plugin is composed of up to three parts, each optional: a **server side** (§3.1), a **client
controller** (§3.4), and one or more **custom views / widgets** (§3.3). A pure server bot ships only §3.1;
a bridge ships §3.1 + realm verbs; a rich admin tool may ship all three.

### 3.1 Server hosting — three modes, one API (`remote` built first)

| Mode | What | I/O | Package? | Status |
|---|---|---|---|---|
| **`remote`** | External process (Matrix **App Service** model) on a **pinned-key session** (`State::PluginService`, generalizing `State::ForeignBridge`). weftd **pushes** events/invocations to it; it **calls back** to act as its users. | Brings its **own** HTTP/timers/DB. | **No package** — self-describes on connect (§4), configured as a pinned key (§14). | **Built first.** |
| `rhai` | In-process Rhai script (hot-reloadable). | Host-provided `http`/`timers`/`kv` (§7). | A package in `[plugins] dir` (§4.1). | Deferred. |
| `wasm` | In-process wasmtime module. | Host-provided I/O. | A package. | Deferred. |

All three speak the **same logical API** — register actions/hooks/timers (§6.4), receive event pushes,
act-as-service (§7.3–7.5), drive SDUI (§7.6). They differ only in **transport** and **who provides I/O**.
A `remote` plugin owns its runtime, so it does its own HTTP/timers/DB and the host `http`/`timers`/`kv`
(§7.7–7.9) are conveniences only the in-process tiers need. The **remote transport**: the process
authenticates like a foreign-bridge adapter (`AUTH ADAPTER`, pinned key), entering `State::PluginService`;
it then sends its registrations (§4.2), receives `PLUGIN`-routed invocations + event pushes, and emits
act-as-service commands (the `@as` tunnelled path, `weft-spec §11.14`). **The foreign-bridge adapter is
the first `remote` plugin** — a remote plugin *plus* the realm/provisioning verbs (§18).

### 3.2 Client rendering — two surfaces

1. **Declarative SDUI** (§10–§11) — the typed catalog, native-themed, rendered by the stock client. For
   lightweight surfaces: action inputs, slash args, settings toggles, simple/live panels, toasts.
2. **Custom-view widgets** (§3.3) — a plugin's own web UI for bespoke, role-editor-class views.

The right tool per job: cheap native declarative forms for the 80%, full-custom widgets for the rest.

### 3.3 Custom-view widgets

A widget is a plugin's **own web UI** loaded in a **`sandbox="allow-scripts"` null-origin iframe** (no
`allow-same-origin` ⇒ the frame cannot reach `window.__TAURI__`, any Tauri command, or the parent DOM). It
talks to the client only via `postMessage` through a **capability broker** — a whitelisted subset of
act-as-service / query / subscribe, **never** the device-key / screencap / moderation commands. It delivers
arbitrary custom UI by **isolation, not HTML sanitization**. Theme tokens (the app's CSS variables) are
passed in so a widget *can* match the app; styling is the widget's responsibility.

**Widget content is served by the plugin's client-side component, from local assets — never a remote
origin** (§20-I). A plugin that wants a custom view ships a **client-side plugin package** (the Rhai
controller, §3.4, plus a web-asset bundle: HTML/JS/CSS). The controller mounts a widget by naming one of
its bundled assets; the client loads it into the sandboxed iframe from a **local `blob:`/asset URL**, so no
remote code enters the app and CSP stays tight (`frame-src 'self' blob:`, no arbitrary origins — §3.6). A
widget is emitted as `PLUGIN-VIEW container=custom` carrying a **widget ref** (which bundled asset) +
params (§11.6, §12.2); the client controller (§3.4) mounts + places it. (Widgets therefore **require** a
client-side plugin; a plugin with only SDUI needs none.)

The earlier "sanitize plugin HTML into the main webview" option is **rejected** — the main context is
csp:null with ~123 IPC commands, too dangerous for reflected content. Isolation replaces sanitization.

### 3.4 Client controller — sandboxed client-side Rhai

An optional per-plugin **client script**, run in a **Rhai sandbox inside `weft-client-core`** (one Rust
runtime, WASM-for-web + native-for-desktop). It is the plugin's **client controller**; it may

- **mount / place / destroy** custom-view widgets in the client's surfaces, passing them params;
- **route `postMessage`** between a widget and the client (broker the widget's requests);
- **subscribe to client events** (channel opened, view changed, selection) and react locally;
- **drive SDUI** locally (open a modal/panel without a server round-trip).

**Safe by construction:** Rhai reaches **only** a curated **client host API** (the broker); it has no
`eval`, no DOM handle, and no path to the raw Tauri commands. This is the safe form of "client-side scripts
for GUI" — no arbitrary JS. The curated client API and the widget broker share **one allowlist**.

The controller is a **client-side plugin package**: the Rhai script + a web-asset bundle for any widgets it
serves (§3.3). weft-client-core loads the package, runs the controller sandboxed, and mounts its widgets
from the bundle (local `blob:` URLs). **Distribution** of the client package (operator-installed in the
client, or pushed by a `remote` server plugin over the session on connect) is a follow-on detail — the
runtime model above is fixed; the delivery channel is not yet pinned (noted in §20-I).

### 3.5 The App-Service SDK — `weft-appservice`

Writing a `remote` plugin means speaking the pinned-key handshake + the plugin protocol + a dispatch loop.
The SDK provides that base so authors write only handlers + logic; **the Matrix bridge is built on it**.

- **Layering:** a standalone **client** library (sibling to `weft-tui`) — deps `weft-proto`,
  `weft-transport`, `weft-crypto`, `tokio`. **No** `weft-core`/`weftd`/`weft-store` dep.
- **Programming model — the same shape as an in-process plugin, over a socket, async-native:**

  ```rust
  AppService::builder(endpoint, keypair, "welcome-bot")
      .bot("welcome")                     // optional; asks weftd to provision the bot account (§14)
      .action(decl, handlers)             // sent on connect → appears in the client catalog
      .hook("member.join", Observe, on_join)
      .run().await?;                      // connect → AUTH ADAPTER handshake → send registrations
                                          //   → dispatch invokes/hook-pushes/event-pushes → reconnect
  ```
  Handlers receive an async `Ctx` exposing the act-as-service + SDUI surface (`ctx.messages.post`,
  `ctx.channels.*`, `ctx.query.*`, `ctx.ui.view`, `ctx.act_as`) — implemented as **label-correlated
  commands over the session**. The service owns its runtime, so HTTP/timers/DB are its own.
- **Bridge extension (§20-J):** the realm/provisioning helpers (`REALM ASSERT`, handle `PROVISION` →
  `PROVISION-OK/ERR`, assert `NS-META`/`CHANNEL-LAYOUT`, ingest foreign events) live behind a **`bridge`
  feature**, so non-bridge services don't carry bridge machinery.
- **Other languages:** the Rust SDK is first (and what the Matrix bridge uses). A Go/Python service needs
  its own thin SDK against the wire protocol (§12) — the protocol, not this crate, is the contract.

### 3.6 Webview hardening (prerequisite for §3.3/§3.4)

The Tauri CSP is `null` today (`tauri.conf.json`) — no defense-in-depth; any XSS reaches all ~123 commands
(device keys, screen/mic/camera, moderation). Before the widget surface ships: introduce a real **CSP**
(tighten from `null`) — and because widgets are **locally** served (§3.3), `frame-src` stays tight
(`'self' blob:`, no arbitrary remote origins) — plus a **command-allowlist** behind the broker. A security
improvement independent of plugins (a latent XSS→full-compromise risk today) and a hard prerequisite for
both the widget and client-controller surfaces.

## 4. Plugin identity & registration

Behavior (actions, hooks, timers) is always **registered in code**, never declared statically (owner call,
round 6). Only *identity* differs by hosting mode: an in-process plugin carries a package + manifest; a
remote plugin is a pinned-key config entry that self-describes on connect.

### 4.1 In-process package & manifest (`plugin.toml`) — `rhai`/`wasm` only

A **package** is a directory (or `.zip`) with `plugin.toml` + the entrypoint + assets, dropped in
`[plugins] dir` (§14). The manifest declares only static identity/runtime/bot/config:

```toml
[plugin]
id          = "welcome-bot"     # REQUIRED. [a-z0-9-]{1,64}, unique per server. Store/route key.
name        = "Welcome Bot"     # REQUIRED. Human label.
version     = "0.1.0"           # REQUIRED. semver; shown in admin, used for reload diffing.
runtime     = "rhai"            # REQUIRED. "rhai" | "wasm". (A remote plugin has no package — §4.2.)
entrypoint  = "main.rhai"       # REQUIRED. Path (relative, no ..); extension matches runtime.
description = "Greets new members."   # optional.
icon        = "icon.png"        # optional. Relative asset; admin + action fallback icon.
api         = 1                 # REQUIRED. Plugin-API version this plugin targets (§19 compat).

[plugin.bot]                    # OPTIONAL. Declares a bot identity (§9). Absent ⇒ no bot account.
account = "welcome"             # provisions `welcome@<network>`.
display = "Welcome Bot"
avatar  = "avatar.png"          # optional relative asset.

[config]                        # requested config keys (§14). Operator fills in server config.
api_key      = { secret = true, required = true, description = "Translation API key" }
default_lang = { default = "en" }
```

**Validation at load:** required fields present + well-formed; `entrypoint` exists, no `..`, extension
matches `runtime`; `api` ≤ the host's supported version (else `Failed`, §5). A malformed manifest ⇒ the
plugin does not load; others are unaffected.

### 4.2 Remote self-description — `remote` only

A remote plugin has **no server-side package**. The operator authorizes it with a **pinned-key config
entry** (§14) carrying its `id`, pinned `key`, optional `bot` account (provisioning is a server-side act
the operator must authorize), and any `config`/secrets. On connect it:

1. **authenticates** via `AUTH ADAPTER` (proves the pinned key) → `State::PluginService`;
2. **self-describes** by sending its registration: `id`, `api`, and its actions/hooks (a `PLUGIN-REGISTER`
   frame carrying the same declarations `register()` would emit — §6.4). Timers are the service's own.

weftd validates the declarations exactly as it validates a `register()` pass (unknown event / duplicate
action id / malformed decl ⇒ the connection is refused with a typed error, logged). The declared actions
enter the client catalog tagged with the plugin id. A remote plugin's `api` is checked the same way.

## 5. Lifecycle & state

### 5.1 In-process (`rhai`/`wasm`)

```
        discover(package)
   ┌───────────────► Loaded ──enable──► Active ──(panic/hang/host-err)──► Quarantined
   │                   │                  │                                    │
   │             (bad manifest/           │  disable                           │ operator reload
   │              compile/link)           ▼                                    ▼
   └─────────────────► Failed          Disabled ◄──────────── reload / re-enable ┘
```

- **Loaded** — manifest parsed, engine compiled/linked, then the host runs **`register()`** (§6.4) once to
  collect actions/hooks/timers. Bot provisioned if declared. A `register()` that throws/traps/overruns ⇒
  `Failed`. Not yet receiving events.
- **Active** — receiving hooks/timers/invocations. Steady state.
- **Disabled** — operator off; registrations removed; bot account left suspended (not deleted).
- **Quarantined** — a fault (panic, deadline overrun beyond policy, repeated host-API error) tripped the
  breaker; auto-disabled + logged. Never wedges weftd (§15). Operator reload to recover.
- **Failed** — could not load (bad manifest / compile error / api too new). Surfaced in admin.

**Hot reload (Rhai):** on entrypoint change, recompile into a new engine, re-run `register()`, atomically
swap; in-flight flows on the old engine are closed (`PLUGIN-RESULT close`, reason "reloaded"); the catalog
is rebuilt + re-pushed (`PLUGIN-MANIFEST`). **WASM** reloads only on operator command/restart.

### 5.2 Remote (`remote`)

A remote plugin's lifecycle is its **connection**: `Connecting → Authenticated → Registered → Active`, and
`Disconnected` (reconnect with backoff). weftd holds no engine — a remote **crash is just a disconnect**
(no quarantine needed; its actions leave the catalog until it reconnects). A misbehaving remote plugin
(flooding, protocol violations) is subject to the ordinary session backpressure/`SLOW`/close path plus
NETBLOCK-style operator control. Its bot account persists across reconnects (provisioned once).

**Ordering:** across all plugins, load/registration order is plugin-id lexicographic (deterministic hook
ordering, §8.4). Enable/disable is idempotent.

## 6. Runtimes & the handler contract

All hosting modes reach the **same host-API surface** (§7) and the **same handler contract** (§6.1). They
differ only in how handlers are expressed and how values cross the boundary.

### 6.1 Handler contract (logical, hosting-agnostic)

| Registered via | Handler | Arguments | Returns |
|---|---|---|---|
| `action(decl, {on_invoke,…})` | `on_invoke` | `ctx` (§7.1), `params` (input values) | `View` \| `Result` |
| " (a returned view's step) | `on_submit` | `ctx`, `view_id`, `values` | `View` \| `Result` |
| " (a button click) | `on_action` | `ctx`, `view_id`, `button_id`, `values` | `View` \| `Result` |
| " (optional, panel) | `on_subscribe` | `ctx`, `panel_key` | `View` (initial) |
| `hook(E, "observe", h)` | `h` | `event` payload (§8.2) | *(ignored)* |
| `hook(E, "veto", h)` | `h` | `event` payload | `Verdict` (`allow` \| `deny(reason)`) |
| `timer(id, sched, h)` | `h` | *(none)* | *(ignored)* |

The flow handlers are supplied in the action's handler map at registration; the host resolves them via
`view_id → action` (§11.1). An action with no `on_invoke` ⇒ load/registration failure.

### 6.2 Remote binding (`weft-appservice`, primary)

Handlers are **async Rust closures/functions** registered on the builder (§3.5). Registration is **sent
over the session** on connect (§4.2); invocations/hook-pushes arrive as frames the SDK dispatches to the
registered handler; the returned `View`/`Result`/`Verdict` is serialized back. The `Ctx` methods are
async, backed by label-correlated commands. No host-provided I/O — the service uses its own.

### 6.3 In-process bindings (`rhai`/`wasm`, deferred)

- **Rhai:** the entrypoint defines `fn register(reg) { … }`; handlers are **inline closures / `Fn`
  pointers** passed to `reg.*`. The host API is a `weft` module. **Synchronous** — "async" host calls
  (HTTP/query) block the call on a host worker under a per-call timeout (§15).
- **WASM:** the module exports `weft_register()` (which calls imported `reg_hook`/`reg_timer`/`reg_action`
  with guest-chosen dispatch **tokens**) and `weft_handle(ptr,len)->i64` (the host writes a CBOR request
  `{token,kind,ctx,args}`, reads a CBOR response `{view?|result?|verdict?|none}`; memory via
  `weft_alloc`/`weft_free`). Host functions imported under `weft`. Bounded by fuel/epoch/memory (§15). The
  exact ABI (raw vs component-model) is **DECISION §20-D**; the §6.1 contract is fixed regardless.

### 6.4 The registration set

However expressed (remote frame, Rhai `register()`, WASM `weft_register`), a plugin emits a set of:
`action(decl, handlers)` · `hook(event, kind, handler, opts?)` · `timer(id, schedule, handler)`
(in-process only). Registration is **side-effect-free**: it may read config/secrets (to register
*conditionally*) but MUST NOT act-as-service, HTTP, KV, or emit UI — a violation fails the load/connect.
`action`/`hook`/`timer` semantics:

- `decl` (an ActionDecl, §13): `{ id, label, icon?, surface, context, description?, visibility?, input:[Component] }`.
- `handlers.on_invoke` required; flow handlers optional (§11).
- `hook` opts: `{ fail: open|closed }` (veto only, §8.3).
- Duplicate action/timer id, unknown event, malformed decl ⇒ load/connect failure.

## 7. Host-API reference

Reachable from every hosting mode (async for `remote`, sync-blocking for in-process). `Result<T>` = value
or a typed `PluginError` (§16). Unless noted, valid in any handler (not in registration, §6.4).

### 7.1 `ctx` — invocation context (read-only)

```
ctx.plugin_id   : string
ctx.network     : string           # this server's network name
ctx.actor_user  : string | null    # the invoking user (invoke/submit/action only; null in hook/timer)
ctx.actor_roles : [string]         # advisory role/cap hints for the invoking user (for provider re-checks)
ctx.surface     : string | null    # context-menu | slash | settings | global | server-menu | channel-list
ctx.context_ref : ContextRef | null# the target the action was invoked on (§13.2)
ctx.session     : string           # opaque session id (per-session panel/widget routing)
ctx.view_id     : string | null    # present in on_submit/on_action
```

### 7.2 `log(level, msg)`
`level ∈ {trace,debug,info,warn,error}`. Routed to weftd tracing under a per-plugin span. Never panics.

### 7.3 `messages` — act-as-service (messaging)
```
post(target, body, opts?) -> Result<msgid>   # opts: { as, md, reply_to, thread, attachments }
edit(msgid, body)         -> Result<()>
delete(msgid)             -> Result<()>
react(msgid, emoji, on)   -> Result<()>       # on: add|remove
```
`target` = channel wire name, `@user`, or `&group`. `opts.as` selects the actor (§9); default per §9.4.
`as:user` flows through the user's caps; `as:bot`/`as:system` are server-authoritative. Effects surface to
clients as ordinary events.

### 7.4 `channels` / `moderation` — act-as-service (structure & moderation)
```
channels.create(ns_id, vanity, opts?) -> Result<channel>   # opts: { kind, category, position, policy }
channels.meta(channel, changes)       -> Result<()>        # rename/category/position/topic/posting
channels.delete(channel)              -> Result<()>
channels.policy(channel, retention)   -> Result<()>
moderation.mute/unmute(scope, account, reason?)   -> Result<()>
moderation.ban/unban(scope, account, reason?)     -> Result<()>
moderation.kick(channel, account, reason?)        -> Result<()>
```
Same actor/authority rules as §7.3.

### 7.5 `query` — read state
```
query.channel(channel) / channels(ns_id) / members(channel) / message(msgid)
     / history(channel, {before,after,limit≤MAX}) / namespace(ns_id) / account(user)
```
Read scope is the whole server (trusted). Returns plain data structs, never live handles.

### 7.6 `ui` — build SDUI responses
```
ui.view(container, blocks, opts?) -> View    # container: modal|panel|custom; opts:{title,panel_key,submit_label,widget,params}
ui.widget(ref, opts?)             -> View    # sugar for a container=custom view; ref = a client-bundle asset (§3.3, §11.6)
ui.patch(view_or_key, ops)        -> Result<()>   # push into a live panel; ops §11.4
ui.toast(kind, text) | ui.navigate(target) | ui.close() | ui.refresh(scope?)  -> Result   # terminals
```
Component builders live under `ui.*` (`ui.text`, `ui.button`, `ui.table`, …). A handler returns a `View`
(modal/panel/custom) or a terminal. `ui.patch` addresses an open **panel** by `panel_key` (§11.3); a closed
key is a no-op `Ok`. A `custom` view carries a widget **ref** (a client-bundle asset, §3.3) + params
instead of a blocks tree.

### 7.7 `timers` — scheduled work (**in-process only**)
Registered via `timer(id, schedule, handler)` (§6.4). `schedule`: `"every <dur>"` | `"cron <5-field>"`. No
invoking user, so `as:user` is invalid inside (§9). **A `remote` plugin runs its own timers** — this is not
part of its wire API.

### 7.8 `http` — outbound network (**in-process only**, SSRF-guarded)
```
http.request({method,url,headers?,body?,timeout?}) / get(url,h?) / post(url,body,h?) -> Result<Response>
```
The URL passes the **same SSRF classifier** as auto-federation (`weftd::dialer::is_dialable`, invariant 13):
loopback / RFC-1918 / CGNAT / link-local / ULA / metadata / v4-mapped-private are refused. Timeout + body
capped (§15). **A `remote` plugin does its own HTTP** (and owns its egress).

### 7.9 `kv` — durable per-plugin storage (**in-process only**)
```
kv.get(key) / set(key,bytes) / delete(key) / list(prefix?)
```
Namespaced to the plugin id (a plugin cannot read another's KV — a free structural isolation kept even
under "unrestricted"). Backed by a `PluginKvStore` (mem + PG, shared contract). **A `remote` plugin uses
its own DB.**

### 7.10 `config` / `secret`
```
config.get(key) -> string?    # non-secret value from the plugin's config
secret.get(key) -> string?    # secret; readable in-process, never logged, never in a PLUGIN-* payload
```
For a remote plugin, config/secrets are delivered once at connect (over the authenticated session) or held
by the service itself; a `secret=true` value is redacted everywhere weftd surfaces it.

## 8. Event-hook catalog & semantics

A plugin subscribes with `hook(event, kind, handler, opts?)` (§6.4). Events are **pushed** to the plugin
(over the session for remote; a direct engine call for in-process).

### 8.1 Hookable events (v1 catalog — §20-A, ratified)

| Event id | When | Veto-eligible | Payload highlights |
|---|---|---|---|
| `message.posted`   | a MSG before commit | ✔ | channel, author, body, meta |
| `message.edited`   | an EDIT before commit | ✔ | msgid, author, new body |
| `message.deleted`  | a DELETE before commit | ✔ | msgid, actor |
| `reaction.added`   | a REACT before commit | ✔ | msgid, actor, emoji |
| `member.join`      | a channel/ns join before commit | ✔ | channel/ns, user |
| `member.part`      | a part | ✖ (observe) | channel/ns, user |
| `invite.redeem`    | an INVITE REDEEM before grant | ✔ | invite id, redeemer |
| `channel.created`  | after a channel is created | ✖ | channel, creator |
| `namespace.created`| after NS CREATE | ✖ | ns id, owner |
| `report.filed`     | after a REPORT | ✖ | report id, category, scope |
| `user.registered`  | after REGISTER | ✖ | account |

Extensible (adding an event = a proto payload type + a core hook point).

### 8.2 Payloads
Each event has a typed L0 payload (round-trip tested), delivered as a CBOR object / Rhai map. Payloads
carry only already-authorized, non-secret data. **E2EE channel content is never delivered to a hook**
(§17) — an `e2ee` `message.posted` omits the body.

### 8.3 Veto semantics — **in-process only** (§20-H)

Veto is an **in-process** capability (`rhai`/`wasm`). **A `remote` plugin registers only `observe` hooks**
— a `veto` from a remote plugin is refused at registration. Remote moderation is done **observe + act**:
the plugin observes `message.posted` (post-commit) and, if it decides to block, **deletes the message as
its bot** (`messages.delete`, `as:bot`) — a brief flash-then-remove, not pre-commit suppression. (Rationale:
a pre-commit veto over a network round-trip would put a remote hop on every hot-path post; in-process veto
keeps that latency bounded and local.)

For an in-process veto hook:
- It runs **before** the effect commits, returning `allow` or `deny(reason)`. First `deny` wins; the
  session answers the actor an `ERR` **`POLICY`** (§16) with the reason; the effect never happens.
- Bounded by a **veto deadline** (§15, default 250 ms/hook). Overrun follows `fail`: `open` (default) ⇒
  allow + log; `closed` ⇒ deny. A quarantined plugin's veto hooks are removed (can't fail-closed the server).
- Runs **before** the capability side-effect commits (CLAUDE.md invariant 4 / `weft-spec` §10.4 order
  preserved).

### 8.4 Observe semantics & ordering
- Observe hooks fire **post-commit, async**; they cannot affect the action (this is exactly App-Service
  event push).
- Multiple hooks on one event: **veto** hooks run in plugin-id lexicographic order (first deny
  short-circuits); **observe** hooks dispatch concurrently (no ordering, no back-pressure on the commit).

## 9. Identity & `act_as`

### 9.1 The three actors
- **bot** — the plugin's declared bot account (`<account>@<network>`), provisioned at load/connect
  (registered + suspended-for-login like `support_account`; posts/acts + appears in rosters + has a
  profile). Autonomous work (timers, hooks, unsolicited posts) uses this.
- **system** — an identity-less server notice (like join/part system lines). No account, not DM-able.
- **user** — the invoking user of a client action; the act runs **through that user's own capabilities**;
  attribution is the user. Only valid where `ctx.actor_user` is set (invoke/submit/action).

### 9.2 Authority (the safety floor under "unrestricted/trusted")
`as:bot` and `as:system` are **server-authoritative** (bypass cap checks — the plugin is trusted).
`as:user` is **capability-checked** as that user, so a plugin can never let a user exceed their grants.

### 9.3–9.4 Selecting the actor & defaults
Act-as-service calls take an optional `as` (`"bot"|"system"|"user"`). Defaults: **invoke/submit/action** →
`as:user`; **hook/timer** → `as:bot` if a bot is declared, else `as:system`. `as:user` outside an
invoke/submit/action handler ⇒ typed error; `as:bot` with no bot declared ⇒ typed error.

## 10. SDUI — component catalog

L0 types serialized as CBOR (internally tagged `{ "type": "...", ... }`, the `StateDiff` serde pattern).
The client renders **only** known `type`s; an unknown type/patch-op is **skipped** (forward-compatible),
never executed. Ids are `[a-z0-9-_]{1,64}`, unique within a view. **v1 set — §20-B, ratified.**

### 10.1 Inputs
| type | fields |
|---|---|
| `text` | id, label, required?, default?, placeholder?, multiline?, max_len?, pattern? (safe anchored regex) |
| `number` | id, label, required?, default?, min?, max?, step? |
| `select` | id, label, required?, default?, options:[{value,label}] |
| `multiselect` | id, label, required?, default:[], options:[{value,label}], min?, max? |
| `toggle` | id, label, default? |
| `date` | id, label, required?, default?, min?, max? (ISO-8601) |

### 10.2 Display
| type | fields |
|---|---|
| `heading` | text, level? (1–3) |
| `markdown` | text (client's safe markdown; no raw HTML) |
| `divider` | — |
| `keyvalue` | rows:[{key,value}] |
| `table` | columns:[string], rows:[[cell]], dense? (cell = text in v1) |
| `image` | src, alt?, max_height? (media ref or https URL) |

### 10.3 Controls
| type | fields |
|---|---|
| `button` | id, label, style? (primary/default/danger), confirm? → `PLUGIN ACTION` |
| `action-row` | buttons:[button] |
| `submit` | label?, style? → `PLUGIN SUBMIT` with the view's inputs |

### 10.4 Container envelope
```
View = { container: "modal"|"panel"|"custom", title?, panel_key?, submit_label?,
         blocks:[Component]         # modal|panel
         widget, params?            # custom — widget = a client-bundle asset ref (§3.3), not a URL
       }
```

## 11. SDUI — views, flows, panels, patches, widgets

### 11.1 view-id
The **host** mints a `view_id` (`<plugin-id>:<session-short>:<seq>`) on emit, returned to the handler (so
an imperative `ui.view` yields it for an immediate `ui.patch`). The client echoes it on
`SUBMIT`/`ACTION`/`SUBSCRIBE`/`CLOSE`. Per-session, single-flow; the host maps `view_id → (plugin,
flow-state-key)`.

### 11.2 Modal flow
`INVOKE`→`on_invoke` returns a modal `View` (or terminal). Each `SUBMIT`/`ACTION` returns the next `View`
or a terminal `Result`. Flow state is the plugin's (its DB/KV, or a host-held ephemeral map keyed by
`view_id`); the client holds only the current view.

### 11.3 Panels & panel_key
A **panel** is persistent (side panel / settings section). Its `panel_key` (provider-chosen, stable) lets
the provider push updates unsolicited. The host maps `(plugin, session, panel_key) → view_id` while
subscribed; `ui.patch(panel_key, ops)` targets the open panel(s) for that key; a closed key is a no-op.

### 11.4 Patch ops
```
PatchOp = replace(view) | set(component_id, props) | append(container_id, blocks) | remove(component_id)
```
Ride `PLUGIN-PATCH <view-id> @patch=<b64cbor>`. Unknown ops ignored.

### 11.5 Terminal results
```
Result = toast(kind,text) | navigate(target) | close(reason?) | refresh(scope?)
```
Real side effects reach the client through the **normal event stream**, not the result.

### 11.6 Custom views (widgets)
A `container = custom` view carries a **widget ref + params** (not blocks, not a remote URL) — the ref
names an asset in the plugin's **client-side bundle** (§3.3). The client controller mounts it from a local
`blob:`/asset URL in a sandboxed null-origin iframe at the target surface (or wherever the controller
places it, §3.4). The widget communicates via `postMessage` → the capability broker; it does **not** use
`SUBMIT`/`ACTION` (those are for declarative views) — its interactivity is its own web app, brokered. A
`PLUGIN-PATCH` MAY target a widget to hand it new params (`set` on the root); the widget re-reads them.
Closing follows the same `CLOSE`/subscription rules as a panel.

## 12. Wire protocol — verbs, events, grammar

New verbs under the `PLUGIN` family (client→server) and `PLUGIN-*` events (server→client), added to
`weft-proto` round-trip-tested. Structured payloads ride as `@<key>=<b64cbor>` tags.

### 12.1 Client → server
| Grammar | Meaning |
|---|---|
| `PLUGINS` | request the action catalog. |
| `@params=<b64> PLUGIN INVOKE <plugin> <action> [<ctx-ref>]` | trigger an action. |
| `@values=<b64> PLUGIN SUBMIT <view-id>` | submit a form step. |
| `PLUGIN ACTION <view-id> <button-id>` | a control click (optional `@values=`). |
| `PLUGIN SUBSCRIBE <view-id>` / `UNSUBSCRIBE <view-id>` | panel/widget liveness. |
| `PLUGIN CLOSE <view-id>` | user dismissed a view. |

### 12.2 Server → client
| Grammar | Meaning |
|---|---|
| `@catalog=<b64> PLUGIN-MANIFEST` | the declared actions/surfaces (reply to `PLUGINS`; pushed on change). |
| `@view=<b64> PLUGIN-VIEW <view-id> <container>` | render/replace a view. `container ∈ modal\|panel\|custom`; a `custom` payload carries `{widget,params}` (a client-bundle asset ref, §11.6). |
| `@patch=<b64> PLUGIN-PATCH <view-id>` | update a live panel/widget. |
| `@result=<b64> PLUGIN-RESULT <view-id>` | terminal outcome. |

### 12.3 Provider (adapter/remote) side
A `remote` plugin / bridge on `State::PluginService` sends:
- `@catalog=<b64> PLUGIN-REGISTER` — its self-description (id, api, actions, hooks; §4.2). weftd validates
  + merges into the client catalog.
- `PLUGIN-VIEW`/`PLUGIN-PATCH`/`PLUGIN-RESULT` — in response to invocations weftd routed to it.
weftd → provider: routed `PLUGIN INVOKE/SUBMIT/ACTION/CLOSE` frames + event pushes (`PLUGIN-EVENT`, §8).

### 12.4 Correlation, acks, gating
- `label` echo (§3.5): a labelled `INVOKE`/`SUBMIT`/`ACTION` is acked by the `PLUGIN-VIEW`/`-RESULT`/`ERR`
  with the same label. Unsolicited `PLUGIN-PATCH` carries no label.
- Errors use the standard `ERR` event (§16).
- Gating: client `PLUGIN INVOKE/SUBMIT/…` verbs are valid only on an authenticated **client** session
  (`State::Ready`). The **provider** verbs (`PLUGIN-REGISTER` etc.) are valid only on `State::PluginService`.

### 12.5 Catalog wire type
`PLUGIN-MANIFEST` payload = a list of `{ plugin_id, name, icon?, actions:[ActionDecl] }`, where
`ActionDecl = { id, label, icon?, surface, context, description?, visibility?, input:[Component] }`. The
client caches + refreshes on push. **Declarations only** — never handler code.

## 13. Action declaration — surfaces, contexts, visibility

An action is registered with `action(decl, handlers)` (§6.4). `decl` becomes the client catalog; `handlers`
stays server-side.

### 13.1 Surfaces
- `context-menu` — a message/channel/member/user/namespace ⋯ menu.
- `slash` — a composer command `/<action>`; inputs map to args — **both** positional (a bare token fills
  the next unfilled input by declaration order) and `key:value` (binds by input id) are accepted (§20-F).
- `settings` — a button/section in a settings page (channel/namespace/server).
- `global` — command-palette entry and/or a side-panel launcher.
- `server-menu` — an item in the **namespace/server header dropdown**. Always-visible; context = namespace.
- `channel-list` — a button in the **channel-list sidebar**. Always-visible; context = namespace.

### 13.2 Context types & `ctx-ref`
| context | offered on | wire `ctx-ref` |
|---|---|---|
| `message` | a message | its `msgid` |
| `channel` | a channel | channel wire name |
| `member`/`user` | a roster member / profile | `user@net` |
| `namespace` | a namespace | `ns:<id>` |
| `none` | global/slash/settings, no target | *(omitted)* |

`server-menu` / `channel-list` always invoke with `context = namespace` + a `ns:<id>` ctx-ref.

### 13.3 Visibility predicate
Optional client-side show/hide over an advisory context (`actor.is_admin`, `actor.roles`, `context.*`) — a
tiny safe expression grammar, **not** code. Display-only; the provider MUST re-check authority on invoke.

### 13.4 Input schema
An ordered list of §10.1 inputs collected into a form **before** `INVOKE`; for `slash` they map to args.
Delivered as `params`. Mid-flow input uses the modal flow (§11.2); rich bespoke UI uses a widget (§11.6).

## 14. Configuration & secrets

```toml
# weftd config (not a package):
[plugins]
dir     = "/etc/weftd/plugins"          # in-process packages live here
enabled = ["welcome-bot", "automod"]    # allow-list (absent ⇒ all discovered enabled)

[plugins.welcome-bot.config]            # config for an in-process plugin
default_lang = "de"
api_key      = "env:TRANSLATE_KEY"      # a secret=true key; "env:X" or an inline value

[[plugin.remote]]                       # a remote plugin / App Service (mirrors [[foreign_bridge]])
id   = "jira-bot"
key  = "<b64 pinned pubkey>"            # proven at AUTH ADAPTER (§4.2)
bot  = "jira"                           # optional; authorizes provisioning jira@<network>
[plugin.remote.config]
api_key = "env:JIRA_TOKEN"
```
- Missing a `required` config key ⇒ the plugin is `Failed` (in-process) / refused (remote), with a clear
  message. Secrets are readable via `secret.get`, **redacted everywhere** (logs, admin, error text), and
  MUST NOT appear in any `PLUGIN-*` payload.

## 15. Isolation & resource limits

Trusted, so limits bound **bugs/runaways**, not adversaries.

| Limit | `remote` | `rhai` | `wasm` | Default |
|---|---|---|---|---|
| Per-call CPU/mem | the plugin's own process (OS-isolated) | op cap + wall timeout | fuel + epoch + mem cap | 250 ms wall (in-proc) / — (remote) |
| Veto deadline | network round-trip within deadline | " | " | 250 ms (§8.3) |
| Memory | own process | engine caps | linear-mem cap | 64 MiB (wasm) |
| HTTP / KV | the plugin's own | host-provided (10 s / 8 MiB / 1 MiB val) | " | (in-proc only) |
| Session flood | backpressure / `SLOW` / close | — | — | — |
| Timer min interval | own | 10 s | 10 s | 10 s |

- **In-process circuit breaker:** N faults (panic / deadline overrun / repeated host-API error) ⇒
  `Quarantined` (§5.1). Panics/traps are caught (never unwind into weftd); hangs are interrupted at the
  deadline. Shared-state corruption is the accepted residual risk of "unrestricted".
- **Remote** is OS-process-isolated: a crash is a disconnect (§5.2); no in-weftd blast radius beyond what
  its capabilities already allow.
- **Client controller (Rhai) + widgets:** the client-side Rhai is bounded like the server Rhai (op cap +
  timeout); a widget is OS/browser-isolated in its null-origin iframe (§3.3) and reaches only the broker.

## 16. Error taxonomy

**Plugin-side (`PluginError`):** `Denied`, `NotFound`, `BadArgument`, `Timeout`, `RateLimited`,
`Unsupported`, `Internal` — a throw (Rhai) / `Err` (Rust/WASM SDK).

**Wire (`ERR` to the client):**
No new plugin error code (§20-C: **reuse** the existing registry).

| Code | When |
|---|---|
| `NO-SUCH-TARGET` | unknown plugin / action / view-id (anti-enumeration, uniform) |
| `POLICY` | a veto hook refused (carries the veto's reason) |
| `FORBIDDEN` | a provider authority re-check refused on invoke (e.g. a foreign-member invoking an admin action) |
| `MALFORMED` | undecodable `@params`/`@values` / bad ctx-ref |
| `UNSUPPORTED` | a `PLUGIN` verb on a session/state that can't serve it |
| `INTERNAL` | plugin fault / quarantine / disconnect mid-flow (flow closed) |

## 17. Security invariants (implement AS TESTS)

1. **No arbitrary client code.** The stock client renders only declared, typed components; an unknown
   component `type`/patch-op is skipped, never evaluated. The client controller is **Rhai-sandboxed**
   (only the broker API; no `eval`/DOM/commands). A **widget** runs in a **null-origin iframe** (no
   `allow-same-origin` ⇒ no `__TAURI__`/command access), talking only through the broker. (Test all three:
   adversarial SDUI payloads; a controller script attempting an unbound call; an iframe attempting to reach
   `__TAURI__`.)
2. **as-user cannot exceed the user.** An `as:user` act runs through the invoking user's caps. (Test: a
   user without `ban` invoking `moderation.ban(as:user)` is refused.)
3. **as-user only where there is a user.** `as:user` in a hook/timer ⇒ typed error. (Test.)
4. **E2EE opacity.** No hook/query/host-API/widget-broker path yields `e2ee` plaintext; `message.*` hook
   payloads omit the body for such channels. (Test.)
5. **SSRF.** `weft.http` (in-process) refuses every non-public target via the shared classifier (invariant
   13). A widget is loaded from a local `blob:` (§3.3), not weftd's network; any network it makes is its
   own sandboxed frame's, never weftd acting on its behalf. (Test over the classifier.)
6. **Secret confidentiality.** A `secret=true` value never appears in a log line, admin response, or
   `PLUGIN-*` payload. (Test the redaction.)
7. **KV isolation.** A plugin cannot read/write another plugin's KV namespace. (Test.)
8. **Fault safety.** An in-process panic/hang is caught + quarantined; a remote crash is a clean
   disconnect; other plugins keep running; a quarantined/disconnected veto hook is removed (can't
   fail-closed the server). (Test.)
9. **Veto ordering & pre-commit.** A veto deny prevents commit; observe hooks never see a vetoed action.
   (Test.)
10. **Anti-enumeration.** Unknown plugin/action/view-id ⇒ `NO-SUCH-TARGET`, uniform (no existence oracle).
    (Test.)
11. **Command-broker allowlist.** The widget/controller broker exposes only its whitelisted subset; the raw
    ~123 Tauri commands (device keys, screencap, moderation) are unreachable from a widget or the
    controller. (Test the broker rejects a non-allowlisted command.)
12. **CSP present.** The shipped Tauri CSP is non-null and constrains `script-src`/`frame-src` (§3.6).
    (Test the built config.)

## 18. Foreign-bridge integration

The foreign-bridge adapter is **the first `remote` plugin** — an App Service (built on `weft-appservice`'s
`bridge` feature) *plus* the realm/provisioning verbs (`foreign-bridge-framework.md`). It is not a bespoke
server path: `State::ForeignBridge` generalizes to `State::PluginService`, and the bridge's client-facing
actions are ordinary plugin actions.

- **Declare actions:** the bridge `PLUGIN-REGISTER`s its actions (`Create channel`, `Create subspace`, …)
  scoped to its realm's namespaces, merged into the client catalog like any provider's.
- **Handle invocations:** a client `PLUGIN INVOKE` on a bridge action routes to the bridge session; the
  bridge relays to the foreign system (e.g. a Matrix room-create) and re-asserts resulting structure as
  `CHANNEL-LAYOUT`/`NS-META`; it returns `PLUGIN-VIEW`/`-RESULT` for the flow. One `INVOKE` → adapter →
  foreign API → re-asserted structure, reusing the entire SDUI stack. Authority = the actor's **foreign**
  role, enforced foreign-side; the visibility predicate hides actions a foreign-member shouldn't see, the
  bridge re-checks on invoke.
- **Realm/provisioning verbs** (`REALM ASSERT`, `PROVISION-OK/ERR`, foreign-event ingestion) are the
  `bridge`-feature superset over the generic App-Service API — reusing foreign-bridge slices 1–5.

## 19. Build milestones (round-7 order: remote-first, then widgets + client-Rhai, then in-process)

**Track A — remote hosting + SDUI (the App Service path, built first):**
- **M-plug-0 — foundations. ✅ (2026-08-03)** `weft-plugin` (host skeleton, `Host`) + `weft-appservice`
  (SDK skeleton, `AppService::builder`) crates in the workspace; `[[plugin.remote]]` config schema slot
  reserved + tested. No engine deps. **Deferred to M-plug-2:** the `State::ForeignBridge` →
  `State::PluginService` change — it's a *restructure* (the `realm` field is bridge-specific), not a
  rename, and doing it before the plugin-service session logic exists would churn green foreign-bridge
  code for no consumer (YAGNI). It lands with the remote transport that needs it.
- **M-plug-1 — SDUI codec (L0).** weft-proto: the component/view/patch/result/widget types, the
  `PLUGIN*`/`PLUGIN-*` verbs+events (incl. `PLUGIN-REGISTER`, `container=custom`), base64-CBOR. **Round-trip
  tests first.**
- **M-plug-2 — remote transport + SDK + a trivial action.** weftd `State::PluginService` routing **and**
  the `weft-appservice` SDK (builder, `AUTH ADAPTER`, `PLUGIN-REGISTER`, dispatch, `Ctx`), validated by a
  trivial App Service that registers a `global` action → `INVOKE` → `PLUGIN-RESULT toast`. End-to-end.
- **M-plug-3 — modal flows + full SDUI rendering.** `SUBMIT`/`ACTION`, multi-step flows, the full catalog
  in the client renderer.
- **M-plug-4 — act-as-service + identity.** The act-as callback (messages/channels/moderation/query),
  bot-account provisioning, `as_bot|system|user` with the §17 authority tests.
- **M-plug-5 — hooks.** The HookPort in weft-core, event push (§8); **observe** hooks for remote plugins
  (async), with the invariant tests. (Pre-commit **veto** is in-process only, §8.3 — it lands with the
  Rhai in-process tier, M-plug-10. Remote moderation = observe + delete-as-bot.)
- **M-plug-6 — panels + live patch + all surfaces.** `SUBSCRIBE`/`UNSUBSCRIBE`, `panel_key`, `PLUGIN-PATCH`,
  the six action surfaces incl. **server-menu**, **channel-list**.

**Track B — custom views (widgets) + client-side Rhai:**
- **M-plug-7 — CSP hardening (§3.6).** Tighten CSP from `null` + scoped `frame-src`; the command-allowlist
  broker. Ships independent of plugins.
- **M-plug-8 — widget surface.** The sandboxed null-origin iframe container, the `postMessage` capability
  bridge, theme-token injection, `PLUGIN-VIEW container=custom`.
- **M-plug-9 — client controller.** The Rhai runtime in `weft-client-core` (WASM-web + native-desktop), the
  curated client host API (mount/place/destroy widgets, route messages, subscribe to client events, drive
  SDUI), sandboxed to the broker.

**Track C — in-process Rhai + bridge + polish:**
- **M-plug-10 — Rhai (in-process).** `rhai` + host `http`/`timers`/`kv` (SSRF, PluginKvStore mem+PG,
  `timer`), hot-reload, `[plugins] dir` packages, **and the in-process pre-commit veto path** (§8.3).
- **M-plug-11 — foreign-bridge `bridge` feature (§18).** Realm/provisioning helpers in `weft-appservice`;
  the bridge's structural actions. Closes the loop with the framework + the Matrix bridge.
- **M-plug-12 — admin & lifecycle polish.** Enable/disable/reload, quarantine surfacing, per-plugin limits.

**Track D — WASM in-process (the last goal, §20-D):**
- **M-plug-13 — WASM (in-process).** wasmtime, the guest ABI (§6.3, ABI decided here), guest SDK,
  fuel/epoch/memory. Deliberately last: the Rhai in-process tier already validates the in-process model, so
  WASM is pure runtime-parity work with no new surface.

## 20. Decisions (ratified 2026-08-03 unless noted)

- **§20-A — hook catalog (§8.1). RESOLVED:** the v1 event set + veto-eligibility as specced is adopted.
- **§20-B — component catalog (§10). RESOLVED:** the v1 catalog as specced is adopted.
- **§20-C — plugin error code (§16). RESOLVED: reuse.** No new code — `POLICY` for a veto-deny (carries
  the reason), `FORBIDDEN` for a provider authority re-check.
- **§20-D — WASM ABI (§6.3). DEFERRED to M-plug-13** (WASM is the last goal); raw core-wasm vs.
  component-model decided when that milestone starts. Nothing earlier depends on it.
- **§20-E — tier line. RESOLVED (round 7):** modes differ only in transport/I/O, not surface.
- **§20-F — slash-arg mapping (§13.4). RESOLVED: both** — positional-by-declaration **and** `key:value`
  are accepted (a bare token binds to the next unfilled input by declaration order; `key:value` binds by id).
- **§20-G — client-Rhai runtime location (§3.4). RESOLVED: `weft-client-core`** (WASM-web + native-desktop,
  one portable runtime).
- **§20-H — veto over remote (§8.3). RESOLVED: remote = observe-only.** Remote plugins register only
  `observe`; pre-commit `veto` is in-process (§8.3). Remote moderation = observe + **delete-as-bot**.
- **§20-I — widget content origin (§3.3). RESOLVED: client-side plugins serve widgets** from local bundle
  assets (`blob:`/asset URLs) — never a remote origin; `frame-src 'self' blob:`. *Follow-on (not blocking):*
  how the client-side plugin **package** is distributed (operator-installed vs. server-pushed on connect).
- **§20-J — bridge helpers in the SDK (§3.5). RESOLVED: behind a `weft-appservice` `bridge` feature.**

## 21. Worked examples

### 21.1 Welcome bot as an App Service (remote, Rust — the primary path)

```rust
// weftd config: [[plugin.remote]] id="welcome-bot", key="…", bot="welcome"
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    AppService::builder(endpoint, keypair, "welcome-bot")
        .bot("welcome")
        .hook("member.join", Observe, |ctx, ev| async move {
            ctx.messages.post(&ev.channel, &format!("Welcome, @{}! 👋", ev.user),
                              PostOpts { as_: Actor::Bot, ..default() }).await?;
            Ok(())
        })
        .run().await
}
```

### 21.2 Automod (remote = observe + delete-as-bot, §8.3)

A remote plugin can't pre-commit veto (§20-H); it observes and removes offenders as its bot.

```rust
let blocklist = ctx_config("blocklist").split(',').collect::<Vec<_>>();
AppService::builder(endpoint, keypair, "automod")
    .bot("automod")
    .hook("message.posted", Observe, move |ctx, ev| {
        let hit = blocklist.iter().any(|w| ev.body.to_lowercase().contains(w));
        async move {
            if hit { ctx.messages.delete(&ev.msgid).await?; }   // delete-as-bot (default actor in a hook)
            Ok(())
        }
    })
    .run().await?;
// (For pre-commit suppression — block before anyone sees it — ship this as an in-process `rhai` veto
//  plugin instead, §8.3 / M-plug-10.)
```

### 21.3 Translate (context-menu action + modal + the service's own HTTP)

```rust
AppService::builder(endpoint, keypair, "translate")
    .action(action("translate").label("Translate").surface(ContextMenu).context(Message)
                .input(select("lang","Language",["en","de","fr"]).required()),
        Handlers::on_invoke(|ctx, p| async move {
            let msg = ctx.query.message(ctx.context_ref()).await?;
            let out = my_http_client.translate(&msg.body, p.str("lang")).await?;   // service's own HTTP
            Ok(View::modal("Translate").block(heading("Translation")).block(markdown(out)))
        }))
    .run().await?;
```

### 21.4 Role editor as a widget (custom view)

The **server side** (below) declares a settings action that opens a widget by **ref**; the widget's web UI
+ the Rhai controller that mounts it are the plugin's **client-side package** (§3.3/§3.4), served locally.

```rust
// SERVER side (remote App Service): declare the action; return a container=custom view naming a
// client-bundle asset ("role-editor"), passing the namespace as a param.
AppService::builder(endpoint, keypair, "role-editor")
    .action(action("roles").label("Role Editor").surface(Settings).context(Namespace),
        Handlers::on_invoke(|ctx, _p| async move {
            Ok(View::widget("role-editor")                       // ref into the client bundle, NOT a URL
                   .param("ns", ctx.context_ref())               // ns:<id>
                   .title("Roles"))
        }))
    .run().await?;
```
```rhai
// CLIENT side (the plugin's client-side Rhai controller, in weft-client-core): mount the bundled widget
// when the settings action fires; the iframe loads role-editor.html from the local bundle (blob URL).
fn register(client) {
    client.on_widget("role-editor", |mount, params| {
        mount.iframe("role-editor.html", params);   // sandboxed null-origin; talks back via the broker
    });
}
// The widget (role-editor.html, its own web app) does the whole custom UI and calls the broker to
// query roles + apply changes (channels.meta / act-as-service).
```

### 21.5 In-process equivalents (Rhai, deferred tier — for reference)

```rhai
// The same welcome bot as an in-process rhai plugin (Track C). plugin.toml: runtime="rhai", bot.account="welcome".
fn register(reg) {
    reg.hook("member.join", "observe", |ev| {
        weft::messages::post(ev.channel, `Welcome, @${ev.user}! 👋`, #{ as: "bot" });
    });
}
```
