# WEFT Plugin System — Specification

**Status:** normative specification (pre-code), 2026-08-03. Companion to the design/rationale doc
`plugin-system.md` (which records *why* each decision was made). This doc is the *what* — the
complete, implementable reference: manifest schema, lifecycle, runtimes, host-API surface, hook
catalog, SDUI component catalog, wire grammar, limits, errors, security invariants, and the build
plan. Decisions locked in `plugin-system.md` §1 are assumed here; genuinely open forks are marked
**DECISION** and collected in §19.

Conventions: wire grammar follows the WEFT control plane (`weft-spec-v0.13.adoc` §4) —
`@tags VERB params :trailing`, lenient-in/strict-out, `label` echo on direct responses (§3.5).
Structured payloads ride as **base64-CBOR in a tag** (`@key=<b64>`), exactly as signed manifests and
capability tokens already do (`ciborium` + `weft_crypto::b64`). "MUST/SHOULD/MAY" are normative.

---

## Table of contents

1. Terminology
2. Architecture & layering
3. Plugin package & manifest (`plugin.toml`)
4. Lifecycle & state machine
5. Runtimes — Rhai & WASM binding
6. Host-API reference
7. Event-hook catalog & semantics
8. Identity & `act_as`
9. SDUI — component catalog
10. SDUI — views, flows, panels, patches
11. Wire protocol — verbs, events, grammar
12. Action declaration — surfaces, contexts, visibility
13. Configuration & secrets
14. Isolation & resource limits
15. Error taxonomy
16. Security invariants (implement as tests)
17. Foreign-bridge integration (action-provider unification)
18. Build milestones
19. Open decisions
20. Worked examples

---

## 1. Terminology

- **Plugin** — a trusted, operator-installed extension: a manifest + a Rhai script or a WASM module.
- **Host** — the weftd-side runtime that loads plugins, owns the Rhai/WASM engines, exposes the
  host API, routes wire verbs, and drives SDUI. Lives in a new crate `weft-plugin` (L3) used by weftd.
- **Action provider** — anything that declares client actions + handles their invocations over the
  `PLUGIN*` protocol. Two kinds: **plugins** (in-process) and **pinned external daemons**
  (`State::ForeignBridge`, §17). "Provider" = either, when the distinction doesn't matter.
- **Action** — a provider-declared client-invocable operation with a surface, context, and input schema.
- **View** — an SDUI screen (a tree of components) rendered in a **modal** or **panel** container.
- **Flow** — a multi-step interaction: a sequence of views owned by the provider, correlated by a **view-id**.
- **Hook** — a plugin subscription to a server event, declared **observe** or **veto**.
- **Actor** — the identity a server-side action runs as: **bot**, **system**, or **user** (§8).

## 2. Architecture & layering

```
weft-proto   (L0)  PLUGIN* verbs + PLUGIN-* events + the SDUI codec (component/view/patch/result
                   types) + the manifest-catalog wire type. Pure, round-trip-tested. No I/O.
weft-core    (L2)  the HookPort: a trait weftd implements so core can surface hookable events +
                   consult veto hooks at the pre-commit point, without core knowing about plugins.
weft-plugin  (L3)  NEW crate. The host: Rhai + wasmtime engines, the host-API implementation,
                   plugin loading/lifecycle, the SDUI router (view-id + panel-key registries),
                   the action registry, timer scheduler. Depends on proto, core, store.
weftd        (L3)  owns a weft-plugin Host, wires PLUGIN verbs from sessions into it, installs the
                   HookPort, provisions bot accounts, injects config/secrets, exposes [plugins] config.
client       (—)   SDUI renderer: renders PLUGIN-VIEW/PATCH, surfaces declared actions in the four
                   surfaces, round-trips INVOKE/SUBMIT/ACTION/SUBSCRIBE/CLOSE. No plugin code runs client-side.
```

**Layering rule (STRICT, per CLAUDE.md):** the SDUI/verb *types* are L0 (fuzzable, no tokio). The
*engines* (Rhai, wasmtime) live only in `weft-plugin` (L3) — never in proto/core. `weft-core` learns
only an abstract `HookPort`; it never links a script engine. New deps (`rhai`, `wasmtime`) are L3-only
and gate on `cargo deny` (§18, M-plug-0).

**Data flow — a user-invoked action (happy path):**
```
client                weftd session         weft-plugin Host        plugin (rhai/wasm)
  │  PLUGIN INVOKE  ───────►│                       │                      │
  │                         │  route(invoke) ──────►│                      │
  │                         │                       │  on_invoke(ctx,p) ──►│
  │                         │                       │◄──── View | Result ──│
  │                         │◄── PLUGIN-VIEW/RESULT ─│                      │
  │◄── PLUGIN-VIEW ─────────│                       │                      │
  │  (render modal/panel)   │                       │                      │
  │  PLUGIN SUBMIT ────────►│ ─── route(submit) ───►│  on_submit ─────────►│
  │                         │                       │◄──── View | Result ──│
  │◄── PLUGIN-VIEW|RESULT ──│                       │                      │
```

**Data flow — a veto hook (pre-commit):**
```
session about to commit a MSG  ─► HookPort.veto("message.posted", payload, deadline)
   Host runs each veto hook (bounded) ─► allow | deny(reason)
   deny  ─► session answers the poster ERR (the effect never commits)
   allow ─► commit proceeds; observe hooks fire post-commit, async
```

## 3. Plugin package & manifest (`plugin.toml`)

A **package** is a directory (or a `.zip` of one) containing `plugin.toml` plus the entrypoint and
assets. The manifest is the single source of truth for everything the host must know **without running
plugin code** — id, runtime, declared actions, declared hooks, declared timers, bot identity, and
requested config keys. Handlers (the *how*) live in the script/module.

### 3.1 Full schema

```toml
[plugin]
id          = "welcome-bot"     # REQUIRED. [a-z0-9-]{1,64}, unique per server. Store/route key.
name        = "Welcome Bot"     # REQUIRED. Human label.
version     = "0.1.0"           # REQUIRED. semver; shown in admin, used for reload diffing.
runtime     = "rhai"            # REQUIRED. "rhai" | "wasm".
entrypoint  = "main.rhai"       # REQUIRED. Path (relative, no ..) to the .rhai script or .wasm module.
description = "Greets new members."   # optional.
icon        = "icon.png"        # optional. Relative asset; surfaced in admin + as action fallback icon.
api         = 1                 # REQUIRED. Plugin-API version this plugin targets (§18 compat).

[plugin.bot]                    # OPTIONAL. Declares a bot identity (§8). Absent ⇒ no bot account.
account = "welcome"             # provisions `welcome@<network>` (an Account, §2.3 charset).
display = "Welcome Bot"         # profile display name.
avatar  = "avatar.png"          # optional relative asset → profile avatar.

[[actions]]                     # zero or more declared client actions (§12).
id         = "translate"        # REQUIRED. [a-z0-9-]{1,64}, unique within the plugin.
label      = "Translate"        # REQUIRED. Menu/button text.
icon       = "🌐"               # optional. Emoji or a relative asset ref.
surface    = "context-menu"     # REQUIRED. context-menu | slash | settings | global.
context    = "message"          # REQUIRED. message | channel | member | user | namespace | none.
description= "Translate to a chosen language."   # optional; slash-command help / tooltip.
visibility = "actor.is_admin"   # optional. Client-side show/hide predicate (§12.3). Absent ⇒ always shown.

  [[actions.input]]             # optional ordered input schema, collected BEFORE on_invoke (§12.4).
  type = "select"; id = "lang"; label = "Language"; required = true; options = ["en","de","fr"]

[[hooks]]                       # zero or more event-hook subscriptions (§7).
event = "message.posted"        # REQUIRED. A catalog event id (§7.1).
kind  = "observe"               # REQUIRED. observe | veto.
# fail = "open"                 # veto only. open | closed. Default open. (§7.3)

[[timers]]                      # zero or more scheduled tasks (§6.7). WASM or Rhai.
id       = "digest"             # REQUIRED. unique within plugin; names the handler.
schedule = "every 1h"           # REQUIRED. "every <dur>" | "cron <expr>". (§6.7)

[config]                        # requested config keys (§13). Operator fills in server config.
api_key      = { secret = true, required = true, description = "Translation API key" }
default_lang = { default = "en" }
```

### 3.2 Manifest validation (at load)

- `id`, `name`, `version`, `runtime`, `entrypoint`, `api` present and well-formed; `entrypoint` exists,
  no `..` traversal, extension matches `runtime`.
- Every `actions[].id`, `hooks[].event`, `timers[].id` valid; action ids unique; `event` in the catalog
  (§7.1); `surface`/`context`/`kind` in their enums.
- `api` ≤ the host's supported plugin-API version (§18) — else the plugin is quarantined with a clear
  version error (never a silent skip).
- A malformed manifest ⇒ the plugin does **not** load (state `Failed`, §4); other plugins are unaffected.

## 4. Lifecycle & state machine

```
        discover(package)
   ┌───────────────► Loaded ──enable──► Active ──(panic/hang/host-err)──► Quarantined
   │                   │                  │                                    │
   │             (bad manifest/           │  disable                           │ operator reload
   │              compile/link)           ▼                                    ▼
   └─────────────────► Failed          Disabled ◄──────────── reload / re-enable ┘
```

- **Loaded** — manifest parsed, script compiled / module linked, actions+hooks+timers **registered**
  (declared surface known to the host). Bot account provisioned if declared. Not yet receiving events.
- **Active** — receiving hooks/timers/invocations. The steady state.
- **Disabled** — operator turned it off; registrations removed; bot account left suspended (not deleted).
- **Quarantined** — a fault (panic, deadline overrun beyond policy, repeated host-API errors) tripped the
  breaker; the plugin is auto-disabled + logged. Never wedges weftd (§14). Operator must reload to recover.
- **Failed** — could not load (bad manifest / compile error / api-version too new). Surfaced in admin.

**Hot reload (Rhai):** on entrypoint file change the host recompiles into a *new* engine instance and
atomically swaps it; in-flight flows keyed to the old instance are abandoned with a `PLUGIN-RESULT close`
(reason "reloaded"). Actions/hooks re-registered from the (possibly changed) manifest. **WASM** reloads
only on operator command or restart (compiled artifact).

**Ordering:** load order is manifest-id lexicographic (deterministic hook ordering, §7.4). Enable/disable
is idempotent.

## 5. Runtimes — Rhai & WASM binding

Both runtimes reach the **same host-API surface** (§6) and the **same handler contract** — they differ
only in how handlers are named/exported and how values cross the boundary.

### 5.1 Handler contract (logical)

For each declared thing, the host calls a handler:

| Declared | Handler invoked | Arguments | Returns |
|---|---|---|---|
| action `X` | `on_invoke` for `X` | `ctx` (§6.1), `params` (input-schema values) | `View` \| `Result` |
| open view step | `on_submit` for the view's action | `ctx`, `view_id`, `values` | `View` \| `Result` |
| button click | `on_action` for the view's action | `ctx`, `view_id`, `button_id`, `values` | `View` \| `Result` |
| hook `E` (observe) | `on_hook` for `E` | `event` payload (§7.2) | *(ignored)* |
| hook `E` (veto) | `on_hook` for `E` | `event` payload | `Verdict` (`allow` \| `deny(reason)`) |
| timer `T` | `on_timer` for `T` | *(none)* | *(ignored)* |
| panel subscribe | `on_subscribe` (optional) | `ctx`, `panel_key` | `View` (initial) |

### 5.2 Rhai binding

- Handlers are **named functions** by convention: `on_invoke_<action>(ctx, params)`,
  `on_submit_<action>(ctx, view_id, values)`, `on_action_<action>(ctx, view_id, button, values)`,
  `on_hook_<event_slug>(ev)`, `on_timer_<timer>()`, `on_subscribe_<panel>(ctx, key)`. A missing handler
  for a declared thing ⇒ load-time `Failed` (declared but unimplemented is an error, not a silent no-op).
- Host API is a module bound as `weft` (e.g. `weft::messages::post(...)`). Rhai maps are used for
  `params`/`values`/`ctx`. SDUI trees are built with `weft::ui` builders (§6.6).
- **Synchronous.** "Async" host calls (HTTP, some queries) block the Rhai call on a host worker with a
  per-call timeout (§14); the script sees a normal return/throw. Trusted + bounded, so blocking is fine.

### 5.3 WASM binding

- Module targets the host ABI (initially a raw core-wasm ABI; component-model is a later option, §19-D):
  a single exported `weft_handle(ptr: i32, len: i32) -> i64` where the host writes a **CBOR request**
  `{ kind, action?, event?, timer?, view_id?, ctx, args }` into linear memory and reads a **CBOR
  response** `{ view? | result? | verdict? | none }`. Memory is exchanged via exported `weft_alloc`/`weft_free`.
- Host functions are imported under module `weft` (`weft.messages_post`, `weft.http_request`, …), each
  taking/returning CBOR byte ranges. A thin guest SDK (Rust/AssemblyScript/TinyGo) wraps this into
  ergonomic calls mirroring the Rhai surface.
- Bounded by wasmtime **fuel** + **epoch interruption** + a memory cap (§14).

*The exact WASM ABI (raw vs component-model, host-fn signatures) is **DECISION §19-D**; the logical
contract in §5.1 is fixed regardless.*

## 6. Host-API reference

All namespaces are reachable from both runtimes. Signatures are given in a language-neutral form;
`Result<T>` means "value or a typed PluginError (§15)". Unless noted, calls are valid in any handler.

### 6.1 `ctx` — invocation context (read-only, provided to handlers)

```
ctx.plugin_id     : string
ctx.network       : string            # this server's network name
ctx.actor_user    : string | null     # the invoking user (invoke/submit/action only; null in hook/timer)
ctx.actor_roles   : [string]          # advisory role/cap hints for the invoking user (for provider re-checks)
ctx.surface       : string | null     # which surface triggered (context-menu/slash/settings/global)
ctx.context_ref   : ContextRef | null # the target the action was invoked on (§12.2)
ctx.session       : string            # opaque session id (for per-session panel routing)
ctx.view_id       : string | null     # present in on_submit/on_action
```

### 6.2 `weft.log(level, msg)`

`level ∈ {trace,debug,info,warn,error}`. Routed to weftd tracing under a per-plugin span. Never panics.

### 6.3 `weft.messages` — act-as-service (messaging)

```
post(target, body, opts?)   -> Result<msgid>     # opts: { as, md, reply_to, thread, attachments }
edit(msgid, body)           -> Result<()>
delete(msgid)               -> Result<()>
react(msgid, emoji, on)     -> Result<()>        # on: add|remove
```
`target` = a channel wire name, `@user`, or `&group`. `opts.as` selects the actor (§8); default per §8.4.
All calls enforce the actor's authority: `as:user` flows through the user's caps; `as:bot`/`as:system`
are server-authoritative (decision 4). Effects surface to clients as ordinary events (§7 of the WEFT spec).

### 6.4 `weft.channels` / `weft.moderation` — act-as-service (structure & moderation)

```
channels.create(ns_id, vanity, opts?)   -> Result<channel>    # opts: { kind, category, position, policy }
channels.meta(channel, changes)         -> Result<()>         # rename/category/position/topic/posting
channels.delete(channel)                -> Result<()>
channels.policy(channel, retention)     -> Result<()>
moderation.mute(scope, account, reason?)   -> Result<()>
moderation.unmute(scope, account)          -> Result<()>
moderation.ban(scope, account, reason?)    -> Result<()>
moderation.unban(scope, account)           -> Result<()>
moderation.kick(channel, account, reason?) -> Result<()>
```
Same actor/authority rules as §6.3.

### 6.5 `weft.query` — read state

```
query.channel(channel)          -> Result<ChannelInfo?>
query.channels(ns_id)           -> Result<[ChannelInfo]>
query.members(channel)          -> Result<[MemberInfo]>
query.message(msgid)            -> Result<MessageInfo?>
query.history(channel, opts?)   -> Result<[MessageInfo]>       # opts: { before, after, limit≤MAX }
query.namespace(ns_id)          -> Result<NamespaceInfo?>
query.account(user)             -> Result<AccountInfo?>        # public profile only
```
Read scope is the whole server (decision 4). All returns are plain data structs (defined in §6 of the
guest SDK), never live handles.

### 6.6 `weft.ui` — build SDUI responses

```
ui.view(container, blocks, opts?)  -> View     # container: modal|panel; opts: { title, panel_key, submit_label }
ui.patch(view_id_or_key, ops)      -> Result<()>   # push into a live view (panel); ops per §10.4
ui.toast(kind, text)               -> Result     # terminal: kind ok|error|info
ui.navigate(target)                -> Result     # terminal: open a channel/view/url-intent
ui.close()                         -> Result     # terminal: dismiss the current view
ui.refresh(scope?)                 -> Result     # terminal: hint client to refetch (roster/history/…)
```
Builders for each component (§9) live under `ui.*` (e.g. `ui.text(id,label,opts)`, `ui.button(id,label,opts)`,
`ui.table(cols,rows)`). A handler returns a `View` or a terminal (`toast/navigate/close/refresh`).
`ui.patch` is the only call that targets a view the handler did **not** just create — it addresses an
open **panel** by its `panel_key` (§10.3); calling it for a closed panel is a no-op `Ok`.

### 6.7 `weft.timers` — scheduled work (declared in manifest)

Timers are declared in `[[timers]]` and dispatched to `on_timer_<id>`. There is no runtime "register
timer" call (declared-not-imperative, so the host knows the schedule without running code). `schedule`:
`"every <dur>"` (`30s`,`5m`,`1h`,`24h`) or `"cron <5-field expr>"`. Timers run with no invoking user, so
`as:user` is invalid inside them (§8).

### 6.8 `weft.http` — outbound network (SSRF-guarded)

```
http.request(req)  -> Result<Response>    # req: { method, url, headers?, body?, timeout? }
http.get(url,h?)   -> Result<Response>
http.post(url,body,h?) -> Result<Response>
```
The URL passes the **same SSRF classifier** as auto-federation (`weftd::dialer::is_dialable`, invariant
13): loopback / RFC-1918 / CGNAT / link-local / ULA / metadata / v4-mapped-private are refused. Per-call
timeout bounded by §14. Response body capped (§14). Secrets from `[plugins.<id>.config]` are injected by
the plugin reading `weft.config`/`weft.secret`; the host never auto-attaches credentials.

### 6.9 `weft.kv` — durable per-plugin storage

```
kv.get(key)            -> Result<bytes?>
kv.set(key, bytes)     -> Result<()>
kv.delete(key)         -> Result<()>
kv.list(prefix?)       -> Result<[key]>
```
Namespaced to the plugin id (a plugin cannot read another's KV — the one structural isolation we keep even
under "unrestricted", because it's free and prevents accidental key collisions, not because of distrust).
Backed by a `PluginKvStore` trait in weft-store (mem + PG, shared contract).

### 6.10 `weft.config` / `weft.secret`

```
config.get(key)  -> string?      # non-secret config value from [plugins.<id>.config]
secret.get(key)  -> string?      # secret value; readable in-process, never logged, never leaves the host
```

## 7. Event-hook catalog & semantics

### 7.1 Hookable events (v1 catalog)

Each maps to a point in weftd's existing pipeline. **Veto-eligible** events fire at a pre-commit point;
observe-only events have no meaningful pre-commit veto.

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

*The catalog is extensible (adding an event = a proto payload type + a core hook point); v1 set above is
**DECISION §19-A** to ratify.*

### 7.2 Payloads

Each event has a typed payload (an L0 struct in weft-proto, round-trip tested) delivered to the handler as
a Rhai map / CBOR object. Payloads carry only already-authorized, non-secret data. E2EE channel content is
**never** delivered to a hook (invariant, §16) — an `e2ee` channel's `message.posted` payload omits the body.

### 7.3 Veto semantics

- A veto hook runs **before** the effect commits and returns `allow` or `deny(reason)`. First `deny` wins;
  the session answers the actor an `ERR` (`CAP-REQUIRED`-style or a plugin-supplied reason via a dedicated
  `POLICY`/`DENIED` code — §15) and the effect never happens.
- Bounded by a **veto deadline** (§14, default 250 ms per hook). Overrun follows the hook's `fail` policy:
  `open` (default) ⇒ treat as allow + log; `closed` ⇒ treat as deny. A quarantined plugin's veto hooks are
  removed (so they can't fail-closed the whole server).
- Veto runs **before** the §10.4 capability side-effect (invariant 4 order preserved).

### 7.4 Observe semantics & ordering

- Observe hooks fire **post-commit, asynchronously** off the hot path; they cannot affect the action.
- When multiple plugins hook the same event: **veto** hooks run in manifest-id lexicographic order (first
  deny short-circuits); **observe** hooks are dispatched concurrently (no ordering guarantee, no back-pressure
  on the committing action).

## 8. Identity & `act_as`

### 8.1 The three actors

- **bot** — the plugin's declared `[plugin.bot]` account (`<account>@<network>`). Provisioned at load
  (registered + suspended-for-login like the existing `support_account`, so no one can *log in* as it, but
  it posts/acts + appears in rosters + has a profile). Autonomous work (timers, hooks, unsolicited posts)
  uses this.
- **system** — an identity-less server notice (like existing join/part system lines). No account, not
  DM-able. For "the server did X" messages.
- **user** — the invoking user of a client action. The act runs **through that user's own capabilities**;
  attribution is the user ("Ada created #general"). Only valid where `ctx.actor_user` is set (invoke/submit/
  action) — never in a hook or timer.

### 8.2 Authority

Under decision 4 (unrestricted/trusted): `as:bot` and `as:system` are **server-authoritative** — they
bypass capability checks (the plugin is trusted). `as:user` is **capability-checked** as that user, so a
plugin can never let a user exceed their own rights. This asymmetry is the safety floor that survives
"unrestricted": user-initiated structural/mod actions still respect the user's grants.

### 8.3 Selecting the actor

Act-as-service calls take an optional `as` in `opts` (`"bot"|"system"|"user"`). `weft.identity.default()`
reports the contextual default.

### 8.4 Defaults

- In an **invoke/submit/action** handler: default `as:user` (a client action is the user's action).
- In a **hook/timer** handler: default `as:bot` if a bot account is declared, else `as:system`.
- Requesting `as:user` outside an invoke/submit/action handler ⇒ a typed error (§15).
- Requesting `as:bot` when no `[plugin.bot]` is declared ⇒ a typed error.

## 9. SDUI — component catalog

The catalog is a set of L0 types serialized as CBOR (internally tagged: `{ "type": "...", ... }`, the
serde pattern already used for `StateDiff`). The client renders **only** known `type`s; an unknown type is
skipped (forward-compatible), never executed. All ids are `[a-z0-9-_]{1,64}`, unique within a view.

### 9.1 Inputs (collect user values)

| type | fields | notes |
|---|---|---|
| `text`        | id, label, required?, default?, placeholder?, multiline?, max_len?, pattern? | pattern = safe anchored regex subset |
| `number`      | id, label, required?, default?, min?, max?, step? | |
| `select`      | id, label, required?, default?, options:[{value,label}] | single choice |
| `multiselect` | id, label, required?, default:[], options:[{value,label}], min?, max? | |
| `toggle`      | id, label, default? | boolean |
| `date`        | id, label, required?, default?, min?, max? | ISO-8601 date |

### 9.2 Display (no value)

| type | fields | notes |
|---|---|---|
| `heading`  | text, level? (1–3) | |
| `markdown` | text | rendered with the client's existing safe markdown (no raw HTML) |
| `divider`  | — | |
| `keyvalue` | rows:[{key,value}] | definition-list layout |
| `table`    | columns:[string], rows:[[cell]], dense? | cell = text (v1); rich cells deferred |
| `image`    | src, alt?, max_height? | src = a media ref or an https URL the client may load |

### 9.3 Controls

| type | fields | notes |
|---|---|---|
| `button`     | id, label, style? (primary/default/danger), confirm? | click → `PLUGIN ACTION` |
| `action-row` | buttons:[button] | horizontal group |
| `submit`     | label?, style? | click → `PLUGIN SUBMIT` with the view's collected input values |

### 9.4 Container envelope

```
View = { container: "modal"|"panel", title?, blocks:[Component], panel_key?, submit_label? }
```
`panel_key` (panels only) is the provider's stable handle for later `ui.patch` pushes (§10.3).

## 10. SDUI — views, flows, panels, patches

### 10.1 view-id

The **host** mints a `view_id` (`<plugin-id>:<session-short>:<seq>`) when it emits a `PLUGIN-VIEW`, and
returns it to the handler (so an imperative `ui.view` call yields the id for immediate `ui.patch`). The
client echoes `view_id` on `SUBMIT`/`ACTION`/`SUBSCRIBE`/`CLOSE`. A `view_id` is per-session and
single-flow; the host maps `view_id → (plugin, flow-state-key)`.

### 10.2 Modal flow

`INVOKE`→`on_invoke` returns a modal `View` (or terminal). Each `SUBMIT`/`ACTION` returns the next `View`
or a terminal `Result` that closes the modal. Flow state is the plugin's (KV or ephemeral host-held map
keyed by `view_id`); the client holds only the current view.

### 10.3 Panels & panel_key

A **panel** is a persistent surface (side panel / settings section). Its `panel_key` (provider-chosen,
stable) lets the provider push updates without a user round-trip. The host maps `(plugin, session,
panel_key) → view_id` while the panel is subscribed. `ui.patch(panel_key, ops)` targets the open panel(s)
for that key in the relevant session(s); a closed key is a no-op.

### 10.4 Patch ops (live updates)

```
PatchOp = replace(view)              # swap the whole view tree
        | set(component_id, props)   # update one component's props (e.g. a progress value)
        | append(container_id, blocks)
        | remove(component_id)
```
Patches ride `PLUGIN-PATCH <view-id> @patch=<b64cbor>`. Unknown ops are ignored (forward-compat).

### 10.5 Terminal results

```
Result = toast(kind, text) | navigate(target) | close(reason?) | refresh(scope?)
```
Any real side effect (a posted message, a created channel) reaches the client through the **normal event
stream**, not the result — the result is only UX closure.

## 11. Wire protocol — verbs, events, grammar

All new verbs live under the `PLUGIN` family (client→server) and `PLUGIN-*` events (server→client), added
to `weft-proto` with round-trip tests first. Structured payloads ride as `@<key>=<b64cbor>` tags.

### 11.1 Client → server

| Grammar | Meaning |
|---|---|
| `PLUGINS` | request the action/manifest catalog. |
| `@params=<b64> PLUGIN INVOKE <plugin> <action> [<ctx-ref>]` | trigger an action; `ctx-ref` per context type (§12.2); `params` = input-schema values. |
| `@values=<b64> PLUGIN SUBMIT <view-id>` | submit a form step. |
| `PLUGIN ACTION <view-id> <button-id>` | a control click (optionally `@values=` for the row's inputs). |
| `PLUGIN SUBSCRIBE <view-id>` / `PLUGIN UNSUBSCRIBE <view-id>` | panel liveness. |
| `PLUGIN CLOSE <view-id>` | user dismissed a view. |

### 11.2 Server → client

| Grammar | Meaning |
|---|---|
| `@catalog=<b64> PLUGIN-MANIFEST` | the declared actions/surfaces (reply to `PLUGINS`; also pushed on change). |
| `@view=<b64> PLUGIN-VIEW <view-id> <container>` | render/replace a view. |
| `@patch=<b64> PLUGIN-PATCH <view-id>` | update a live panel. |
| `@result=<b64> PLUGIN-RESULT <view-id>` | terminal outcome. |

### 11.3 Correlation, acks, errors

- `label` echo (§3.5): a labelled `INVOKE`/`SUBMIT`/`ACTION` is acked by the `PLUGIN-VIEW`/`-RESULT`/`ERR`
  carrying the same label. Broadcast pushes (unsolicited `PLUGIN-PATCH`) carry no label.
- Errors use the standard `ERR` event (§15): unknown plugin/action/view, denied, bad params, plugin fault.
- Gating: `PLUGIN*` verbs are valid only on an authenticated client session (`State::Ready`). An adapter
  session (`State::ForeignBridge`) speaks the **provider** side (§17), not the client side.

### 11.4 Manifest-catalog wire type

`PLUGIN-MANIFEST`'s payload is a list of `{ plugin_id, name, icon?, actions:[ActionDecl] }` where
`ActionDecl = { id, label, icon?, surface, context, description?, visibility?, input:[Component] }`. The
client caches it and refreshes on push. It contains **only declarations** — never handler code.

## 12. Action declaration — surfaces, contexts, visibility

### 12.1 Surfaces

- `context-menu` — on the context object's ⋯/right-click menu (message/channel/member/user/namespace).
- `slash` — a composer command `/<action>`; the input schema maps to arguments (positional by declaration
  order, or `key:value`). Handled server-side like any invoke.
- `settings` — a button/section injected into the relevant settings page (channel/namespace/server).
- `global` — a command-palette entry and/or a dedicated side-panel launcher.

### 12.2 Context types & `ctx-ref`

| context | client offers it on | wire `ctx-ref` |
|---|---|---|
| `message`   | a message | its `msgid` |
| `channel`   | a channel | channel wire name |
| `member`    | a roster member | `user@net` |
| `user`      | any user/profile | `user@net` |
| `namespace` | a namespace | `ns:<id>` |
| `none`      | global/slash/settings with no target | *(omitted)* |

### 12.3 Visibility predicate

Optional client-side show/hide over an advisory context (`actor.is_admin`, `actor.roles`, `context.*`). A
tiny safe expression grammar (booleans, comparisons, membership) — **not** arbitrary code. It only affects
*display*; the provider MUST re-check authority on invoke (the client is untrusted for gating). Absent ⇒
always shown.

### 12.4 Input schema

An ordered list of §9.1 input components collected into a form **before** `INVOKE`. For `slash`, they map
to command args. The plugin receives them as `params`. Anything requiring more input mid-flow uses the
modal flow (§10.2) instead.

## 13. Configuration & secrets

```toml
# in weftd's config (not the package):
[plugins]
dir     = "/etc/weftd/plugins"     # where packages live
enabled = ["welcome-bot", "automod"]   # explicit allow-list (absent ⇒ all discovered enabled)

[plugins.welcome-bot.config]
default_lang = "de"
api_key      = "env:TRANSLATE_KEY"     # a `secret=true` key; "env:X" reads env, or an inline value
```
- The operator fills declared `[config]` keys (§3.1). Missing a `required` key ⇒ the plugin is `Failed`
  with a clear message.
- Secrets are readable in-process via `weft.secret.get` and are **redacted everywhere** (logs, admin API,
  error text). A secret value MUST NOT appear in a `PLUGIN-*` payload.

## 14. Isolation & resource limits (stability, not security)

Trusted, so limits bound **bugs/runaways**, not adversaries. Defaults (operator-overridable per plugin):

| Limit | Rhai | WASM | Default |
|---|---|---|---|
| Per-call CPU | operation cap + wall timeout | fuel + epoch interrupt | 250 ms wall / call |
| Veto deadline | " | " | 250 ms (§7.3) |
| Memory | engine value caps | linear-memory cap | 64 MiB (WASM) |
| HTTP timeout | — | — | 10 s / request |
| HTTP response cap | — | — | 8 MiB |
| KV value size | — | — | 1 MiB / value |
| Timer min interval | — | — | 10 s |

**Circuit breaker:** N faults (panic / deadline-overrun / repeated host-API error) within a window ⇒
**Quarantined** (§4). A panic/trap is caught (never unwinds into weftd). A hung call is interrupted at the
deadline. In-process shared-state corruption is the accepted residual risk of decision 4.

## 15. Error taxonomy

**Plugin-side (`PluginError`, returned from host-API calls):** `Denied` (authority), `NotFound`,
`BadArgument`, `Timeout`, `RateLimited`, `Unsupported`, `Internal`. Delivered to the handler as a
throw (Rhai) / `Err` (WASM SDK).

**Wire (`ERR` to the client):** reuse the WEFT registry (§8) where it fits, add plugin-specific codes:

| Code | When |
|---|---|
| `NO-SUCH-TARGET` | unknown plugin / action / view-id (anti-enumeration uniform with other targets) |
| `DENIED` | a veto hook or provider re-check refused (carries the provider-supplied reason) |
| `MALFORMED` | undecodable `@params`/`@values`/bad ctx-ref |
| `UNSUPPORTED` | a `PLUGIN` verb on a session/state that can't serve it |
| `INTERNAL` | plugin fault / quarantine mid-flow (flow closed) |

*Whether `DENIED` is a new registry code or a reuse of `POLICY`/`FORBIDDEN` is **DECISION §19-C**.*

## 16. Security invariants (implement AS TESTS)

1. **No client code path.** The client executes only declared, typed components; an unknown component
   `type` or patch op is skipped, never evaluated. (Test the renderer against unknown/adversarial payloads.)
2. **as-user cannot exceed the user.** An `as:user` act runs through the invoking user's capabilities;
   a plugin cannot elevate a user beyond their grants. (Test: a user without `ban` invoking a plugin that
   calls `moderation.ban(as:user)` is refused.)
3. **as-user only where there is a user.** `as:user` in a hook/timer ⇒ typed error. (Test.)
4. **E2EE opacity.** No hook/query/host-API path yields plaintext for an `e2ee` channel; `message.*` hook
   payloads for such channels omit the body. (Test.)
5. **SSRF.** `weft.http` refuses every non-public target via the shared classifier (invariant 13). (Test
   over the classifier, not the dial path.)
6. **Secret confidentiality.** A `secret=true` config value never appears in a log line, admin response, or
   `PLUGIN-*` payload. (Test the redaction.)
7. **KV isolation.** A plugin cannot read/write another plugin's KV namespace. (Test.)
8. **Quarantine safety.** A panicking/hanging plugin is caught + quarantined; weftd keeps serving and other
   plugins keep running; a quarantined veto hook is removed (can't fail-closed the server). (Test.)
9. **Veto ordering & pre-commit.** A veto deny prevents the effect from committing; observe hooks never see
   a vetoed action. (Test.)
10. **Anti-enumeration.** Unknown plugin/action/view-id ⇒ `NO-SUCH-TARGET`, uniform with other missing
    targets (no plugin-existence oracle). (Test.)

## 17. Foreign-bridge integration (action-provider unification)

The foreign-bridge adapter is a **provider** without being a plugin. On its `State::ForeignBridge` session
(`foreign-bridge-framework.md` §3) it MAY:
- **declare actions** via a provider-declaration verb (`REALM ACTIONS <b64catalog>` — the adapter-side
  analog of `PLUGIN-MANIFEST`, scoped to the adapter's realm/namespaces); weftd merges these into the
  catalog it serves clients, tagged with the provider.
- **handle invocations**: when a client `PLUGIN INVOKE`s an adapter-provided action, weftd routes the
  invoke to the adapter's session (not a plugin engine); the adapter returns `PLUGIN-VIEW`/`-RESULT` over
  its session, which weftd relays to the client. The adapter handles the action by relaying to the foreign
  system (e.g. a Matrix room-create) and re-asserting resulting structure as `CHANNEL-LAYOUT`/`NS-META`.

So "Create channel/subspace" on a bridged space is one `INVOKE` → adapter → foreign API → re-asserted
structure — reusing the entire SDUI stack, no companion plugin. Authority for adapter actions is the
actor's **foreign** role, enforced foreign-side (§7 of the framework doc); the visibility predicate hides
actions a foreign-member shouldn't see, the adapter re-checks on invoke.

*The exact adapter-side declaration/handling verbs are specified with the foreign-bridge structural-relay
slice; this section fixes that they reuse the `PLUGIN*` client-facing surface.*

## 18. Build milestones (each independently shippable, proto-first)

- **M-plug-0 — foundations & deps.** New crate `weft-plugin` (empty host), add `rhai` + `wasmtime` to the
  workspace, `cargo deny` pass (licenses/advisories/bans). `[plugins]` config skeleton. No behavior yet.
- **M-plug-1 — SDUI codec (L0).** weft-proto: the component catalog (§9), View/Patch/Result/Manifest types,
  the `PLUGIN*`/`PLUGIN-*` verbs+events, base64-CBOR payloads. **Round-trip tests first.** No host yet.
- **M-plug-2 — host + Rhai + a trivial action.** weft-plugin loads a Rhai plugin from `[plugins] dir`,
  registers a `none`-context `global` action, routes `INVOKE`→`on_invoke`→`PLUGIN-RESULT toast`. Client
  renders the palette entry + toast. End-to-end vertical, one runtime, one surface.
- **M-plug-3 — modal flows + full catalog rendering.** `SUBMIT`/`ACTION`, multi-step flows, the full
  component catalog in the client renderer. `weft.ui` builders.
- **M-plug-4 — act-as-service + identity.** `weft.messages`/`channels`/`moderation`/`query`, bot-account
  provisioning, `as_bot|system|user` with the §16 authority tests.
- **M-plug-5 — hooks.** The HookPort in weft-core, the catalog (§7), observe (async) + veto (pre-commit,
  bounded) with the invariant tests. Automod becomes possible.
- **M-plug-6 — panels + live patch.** `SUBSCRIBE`/`UNSUBSCRIBE`, `panel_key`, `PLUGIN-PATCH`, the settings +
  global side-panel surfaces, remaining action surfaces (context-menu/slash/settings).
- **M-plug-7 — WASM runtime.** wasmtime engine, the guest ABI (§5.3), a guest SDK, fuel/epoch/memory limits,
  parity with the Rhai host-API surface.
- **M-plug-8 — durable KV + timers + HTTP.** `weft.kv` (PluginKvStore, mem+PG), `[[timers]]` scheduler,
  SSRF-guarded `weft.http`, config/secrets injection + redaction tests.
- **M-plug-9 — foreign-bridge provider (§17).** Adapter-side action declaration + invoke routing; the
  bridge's structural "Create channel/subspace" actions. Closes the loop with the framework.
- **M-plug-10 — admin & lifecycle polish.** Enable/disable/reload in the admin panel, quarantine surfacing,
  hot-reload, per-plugin limit config.

## 19. Open decisions (to ratify before the affected milestone)

- **§19-A — hook catalog (§7.1).** Confirm the v1 event set + which are veto-eligible. *(blocks M-plug-5)*
- **§19-B — component catalog (§9).** Confirm the v1 widget set (esp. `table` richness, `image` sources).
  *(blocks M-plug-1)*
- **§19-C — `DENIED` code (§15).** New registry code vs. reuse `POLICY`/`FORBIDDEN`. *(blocks M-plug-5)*
- **§19-D — WASM ABI (§5.3).** Raw core-wasm ABI vs. wasm component-model; exact host-fn signatures.
  *(blocks M-plug-7 only — Rhai path unaffected)*
- **§19-E — tier line.** Confirm the round-5 collapse (identical surface on both runtimes) — flagged because
  it reversed an earlier answer. *(blocks nothing structurally; affects §4/§6 wording)*
- **§19-F — slash-command arg mapping (§12.4).** Positional-by-declaration vs. `key:value` vs. both.
  *(blocks M-plug-6)*

## 20. Worked examples

### 20.1 Welcome bot (Rhai, hook + as-bot post)

```toml
[plugin] id="welcome-bot" name="Welcome Bot" version="0.1.0" runtime="rhai" entrypoint="main.rhai" api=1
[plugin.bot] account="welcome" display="Welcome Bot"
[[hooks]] event="member.join" kind="observe"
```
```rhai
fn on_hook_member_join(ev) {
    let ch = ev.channel;
    weft::messages::post(ch, `Welcome, @${ev.user}! 👋`, #{ as: "bot" });
}
```

### 20.2 Automod (Rhai, veto)

```toml
[[hooks]] event="message.posted" kind="veto" fail="open"
[config] blocklist = { default = "spam,scam" }
```
```rhai
fn on_hook_message_posted(ev) {
    let bad = weft::config::get("blocklist").split(",");
    for w in bad { if ev.body.to_lower().contains(w.trim()) { return weft::deny(`blocked: ${w}`); } }
    weft::allow()
}
```

### 20.3 Translate (Rhai, context-menu action + modal + HTTP)

```toml
[[actions]] id="translate" label="Translate" surface="context-menu" context="message"
  [[actions.input]] type="select" id="lang" label="Language" options=["en","de","fr"] required=true
[config] api_key = { secret=true, required=true }
```
```rhai
fn on_invoke_translate(ctx, params) {
    let msg = weft::query::message(ctx.context_ref);       // the target msgid
    let res = weft::http::post("https://api.example/tr",
        #{ text: msg.body, to: params.lang },
        #{ "Authorization": `Bearer ${weft::secret::get("api_key")}` });
    weft::ui::view("modal", [ weft::ui::heading("Translation"), weft::ui::markdown(res.json.text) ],
        #{ title: "Translate" })
}
```

### 20.4 Moderation queue (Rhai, live panel)

```toml
[[actions]] id="modq" label="Mod Queue" surface="global" context="none"
[[hooks]]   event="report.filed" kind="observe"
```
```rhai
fn on_invoke_modq(ctx, _p) {
    weft::ui::view("panel", render_queue(), #{ title: "Mod Queue", panel_key: "modq" })
}
fn on_hook_report_filed(ev) {                 // push into any open queue panel
    weft::ui::patch("modq", [ weft::ui::patch_replace(render_queue()) ]);
}
```
