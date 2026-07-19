# Web admin panel (operator) — implementation plan

Status: **API + SPA shipped, embedded** (2026-07-06). Decision: **embedded-only**
(weftd mounts it) — the standalone binary was removed. Sharding is planned as a
future path (§3a). Supersedes the "chat client in a browser" idea for the *admin*
use case — see note at the bottom.

## Shipped (embedded)

The `weft-admin` crate is a **library** weftd mounts (`[admin] enabled = true`):

- **Crate** `crates/weft-admin`: `AdminState::from_store` (fans one backend into
  the store roles), `router()` (serves the SPA at `/admin` + JSON at
  `/admin/api/*`), operator **auth** (HMAC session cookie + login +
  `require_operator` middleware).
- **Embedded**: weftd mounts the router on the HTTP listener (signs cookies with
  the server-only network seed; operators = `[operators]`). weftd holds the
  live channel registry, so kick/eject work.
- **SPA** (`ui/index.html`): a self-contained, no-build-step single page
  (`include_str!`-embedded) — login, dashboard, reports (queue → detail with
  materialized context + resolve), users, message browse, moderation
  (mute/ban/kick + active list). Plain `fetch("/admin/api/…")`, cookie auth.
- **Store gap closed**: `AccountStore::list_accounts` (mem + PG + contract test).
- **Endpoints wired**: `login`/`logout`/`me`, `stats`, `reports` — including
  `GET /reports/:id` with the **reported message + materialized context** (the
  retention-held roots, invariant 11), `/:id/resolve`, `accounts`, `channels`,
  `namespaces`, `grants`, `moderation` (GET + POST mute/ban/unmute/unban), and
  `channels/:name/messages` **fully materialized** (bodies, `edited`, reaction
  summaries, tombstones — the same view HISTORY serves).
- **Live actions (embedded)**: a `Live` **port** (dependency-inverted — weftd
  implements it over the channel registry). Wired: **kick** + **channel-scope
  ban force-part** (actor `eject`), and **`DELETE /messages`** delete-any via a
  new channel-actor `Cmd::SystemDelete` (mints the tombstone attributed to the
  moderator, `SENTINEL_ORIGIN` broadcast — no session needed). The SPA has a
  delete button per message (browse + report context) with a contextual refresh.
- **Live-connection count**: an `AtomicUsize` in `ServerCtx`, inc/dec per session
  via an RAII guard in `run_session`; surfaced on `/stats`.
- **End-to-end tests**: `weft-admin/tests/api.rs` (serves SPA; auth gate + login
  + tamper; kick + delete 501-without-live / 204-with-live); `weft-core`
  `admin_delete_tombstones_without_membership` (actor mints + broadcasts the
  tombstone for a non-member moderator; the message becomes `NoSuchTarget`).
- **Density + comprehensiveness pass (2026-07-11)**: the SPA was rebuilt denser
  (tighter tables, badges, 10-item nav) and comprehensive. New screens:
  **Channels**, **Namespaces**, **Grants** (scope-filtered), **Federation**
  (bridge peers + netblock list, add/remove netblock), **Media blocks** (§13
  blocklist, block/unblock). The dashboard gained cards (namespaces, open
  reports, peers, netblocks, blocked media). **Users** is now an enriched table
  (ULID, operator badge, caps@*, muted/banned) with client-side search and a
  per-account **detail view**: ULID, **email + verified claims** (the claim
  `subject` values), memberships, all grants, mod state, and **every message the
  user authored** across all channels/DMs (newest-first, each deletable) via
  `EventStore::messages_by_sender` (mem + PG + contract) behind
  `GET /accounts/:name/messages`. **Account delete** shipped: `AccountStore::delete_account` (mem + PG +
  contract) hard-deletes the account **and** its per-account data (memberships,
  ULID-keyed grants, moderation records, role assignments) while keeping posted
  messages; API `DELETE /accounts/:name` (refuses self-delete) + a delete button
  in the list and detail. New read/action endpoints: `GET /accounts/:name`,
  `GET/POST/DELETE /netblocks`, `GET /peers`, `GET/POST/DELETE /media-blocks`,
  enriched `GET /accounts` + `/stats`. `AdminState` gained the Membership /
  Netblock / Peer / MediaBlocklist store roles. Tests:
  `operator_deletes_a_user_but_not_themselves`,
  `netblock_and_media_block_endpoints`.
- **TODO (next phases)**: reporter anonymization per §6.7; SPA pagination for
  large lists; message-content purge on account delete (opt-in); durations +
  audit log for moderation; media-block byte deletion from the panel (the store
  role can't reach weftd's blob store — the wire `MEDIA BLOCK` verb does the
  deletion, GC + fetch gate cover panel-added hashes). Sharding routing (§3a)
  when needed.

Remaining plan below.

## 1. Goal

A purpose-built **operator** surface: assess the reports queue (with the
reported message + surrounding context), browse channel/DM history, list every
account on the network, see live moderation state + grants + stats, and take
moderation actions. Reachable in a browser, operator-only.

## 2. Why not "the chat client in a browser"

The WEFT wire protocol is deliberately **channel-scoped and user-facing**: it
has no *list-every-account*, no *browse arbitrary history*, no *report + full
context* surface. Those are operator queries. But the **store already holds the
data** — `ReportStore`, `EventStore` (history + `find_root` + materialize;
retention holds keep reported context, invariant 11), `ChannelStore`,
`NamespaceStore`, `ModerationStore`, `CapabilityStore`. So the right shape is a
thin **operator-authed HTTP/JSON API on weftd that reads the store directly**,
plus a small SPA — not a second protocol client.

## 3. Architecture — a `weft-admin` crate, embedded in weftd

The admin router + handlers + auth + SPA live in the crate `weft-admin` (L3),
which takes the `weft-store` trait objects it needs. weftd mounts it:

```
 weftd axum listener
   ├── /.well-known/weft
   └── /admin            (SPA, include_str!)
       /admin/api/*      (operator-authed JSON)
              │ shares the in-process
              ▼ Store + channel Registry
```

- weftd builds `AdminState::from_store(store, auth, network)` (fanning its one
  backend into the store roles, like `ServerCtx`), attaches a `Live` adapter
  over the channel registry, and `merge`s `weft_admin::router(state)` into the
  HTTP router. Config: `[admin] enabled = true`. Front with Caddy for HTTPS.
- **No new datastore** — it reads the existing stores. Cookies are signed with
  the server-only network seed (no new secret).
- Being in-process gives it the two things a separate process couldn't have:
  the live-connection count and live actions (kick/eject, and — once the
  `SystemDelete` actor command lands — message deletion) that reach connected
  sessions immediately.

### 3a. Sharding — the future scale path (planned, not built)

Today weftd is a single process owning one store + all channel actors. If a
network outgrows that, the likely shape is **horizontal sharding**: several weftd
instances over a **shared Postgres**, each *owning* a subset of channels (the
actor for a channel — the single ULID writer — must live on exactly one node).
The admin panel must not assume single-process, so we keep these seams now:

1. **Reads already shard-safe.** Every read handler goes through the store
   traits against Postgres — the shared source of truth. An embedded admin on
   *any* node can serve the full reports/accounts/messages/moderation views for
   the whole network with zero change. (The in-memory backend can't be shared,
   so sharded deployments are Postgres-only — already our engine of record.)
2. **Live actions must route to the owning node.** `eject`/`SystemDelete` touch a
   channel's actor, which lives on one shard. The `Live` **port** is the seam:
   its embedded adapter would grow from "get the local handle" to "if I own this
   channel, act locally; else forward to the owner" (a small node-to-node admin
   RPC, or reuse the federation/bridge session plumbing). Because callers only
   see the trait, no handler changes.
3. **`/stats` live-connections becomes a sum.** The per-process `AtomicUsize`
   (still to be added) turns into "my count" + a fan-out/aggregate across nodes
   (or a shared counter). Store-derived stats (accounts/channels) are already
   global via Postgres.
4. **Sessions/cookies are already stateless.** Auth is an HMAC over
   `account|exp` with the network key — identical on every node, so a cookie
   minted on one shard validates on all. No sticky sessions, no shared session
   store.
5. **Ownership lookup.** Routing (2) needs "which node owns channel X" — a
   `channel → node` map (a Postgres table or a small coordinator), consulted by
   the `Live` adapter. This is the one genuinely new surface sharding introduces;
   everything else is already seam-compatible.

Net: the store-trait layer + the `Live` port mean sharding is an operational and
routing change, **not** an admin-panel rewrite. We build none of it now (YAGNI),
but we don't wall ourselves off from it either.

## 4. Auth (operator sessions)

HTTP needs its own login (WEFT auth is over QUIC/WS):

- `POST /admin/api/login {account, password}` → verify against `AccountStore`
  (argon2, constant-time, uniform failure) **and** require operator (holds a
  cap at `*`) → set a signed, http-only, short-expiry session cookie
  (HMAC over `account|exp` with the network signing key; no new secret).
- Middleware gates every `/admin/api/*`: valid cookie + still-operator.
- Rate-limit login; `AUTH-FAILED`-style uniform error.

## 5. Endpoints

### Read (the "assess / list" core)

| Endpoint | Backed by |
|---|---|
| `GET /reports?scope=&state=` | `ReportStore::list_reports` |
| `GET /reports/:id` → report + **reported message + ±N context** | `report()` + `EventStore::find_root` + materialize; strips reporter per §6.7 / invariant 12 |
| `GET /accounts?page=&q=` → **all network users** + presence, devices, verification, `*`-roles | **NEW** `AccountStore::list_accounts` + presence/registry |
| `GET /accounts/:name` → channels, roles/grants, mod state, reports for/against | existing store reads |
| `GET /channels`, `GET /namespaces` | `list_channels`, namespace list |
| `GET /channels/:name/messages?before=&limit=` → browse history | `EventStore::roots` + materialize |
| `GET /moderation?scope=` → active mutes/bans | `ModerationStore::list_moderation` |
| `GET /grants?scope=` / `?subject=` | `CapabilityStore` |
| `GET /stats` → accounts, channels, **live connections**, storage sizes | new counters (below) + store |

### Write (admin actions — Phase 2)

`POST /reports/:id/resolve`, `POST /moderation` (mute/ban/kick), `DELETE
/messages/:msgid` (tombstone), `POST|DELETE /grants`, `POST /netblock`, channel
create/delete, etc. Each mutates the store **and broadcasts to live sessions**
via the channel actor's `announce` (so connected clients see the effect), reusing
the exact logic the protocol handlers use.

## 6. New server surfaces needed (small)

1. **`AccountStore::list_accounts(page) -> Vec<AccountSummary>`** — enumerate
   accounts (+ created-at, device count, verification). mem + PG impls + one
   contract-suite case. *(The only real store gap.)*
2. **Live-connection counter** — an `AtomicUsize` in `ServerCtx`, inc/dec per
   session (in `run_session` setup/teardown), read by `/stats`.
3. **Report context helper** — compose `find_root` + materialize ±N around the
   reported msgid (no new store method; lives in the admin module).
4. *(Later)* message search — PG `ILIKE` query behind `EventStore`, gated to PG.
5. weftd deps: `axum` (have), a cookie/HMAC helper (hand-rolled with `sha2` +
   `base64`, both already in the tree), `rust-embed` for serving the SPA.

## 7. The SPA

A small, dedicated admin SPA — **its own routes**, not the chat UI. Views:
**Reports** (queue → detail with content states + context + resolve),
**Users** (searchable list → detail), **Messages** (pick channel → browse),
**Moderation**, **Grants**, **Federation** (reuse the netblock/bridge calls),
**Stats**. It talks plain `fetch("/admin/api/…")` (no WS, no protocol codec).

Reuse the existing design system (`app.css` tokens + a few components like the
report/ns cards). **[DECIDE]** where it lives — see §10.

Served by weftd at `/admin` via `rust-embed` (single binary); Caddy fronts
HTTPS.

## 8. Confidentiality & safety (non-negotiable)

- **Reporter identity** (§6.7, invariant 12): `/reports/:id` must not leak the
  reporter to anyone who shouldn't see it; forwarded reports already strip it.
- **Content states** shown honestly (`verified` / `unverified` /
  `reporter-attested`) — never fabricate verification (invariant 11).
- **e2ee / expired content**: the panel shows "unavailable by policy" — it must
  not hold or reconstruct plaintext it isn't entitled to (invariant 8).
- **Retention holds** keep reported context queryable but stay invisible on
  normal surfaces — the admin panel is the *only* place they surface.
- Operator-only, HTTPS-only, audited: every write action logged (who/when/what).

## 9. Phases (each shippable)

| Phase | Deliverable | Value |
|---|---|---|
| **P1** | Auth + **read-only** API: reports (+context), accounts list/detail, message browse, moderation/grants view | The core "assess reports, list users, view messages" ask |
| **P2** | Write actions: resolve report, mute/ban/kick, delete message, grant/revoke (store + live broadcast) | Act on what you see |
| **P3** | `/stats` dashboard (accounts/channels/live conns/storage) + federation admin | Situational awareness |
| **P4** | Serve `/admin` embedded (`rust-embed`) + Caddy routes + operator login UI | Ship it behind HTTPS |
| **P5** *(opt)* | Message search, audit log, account suspend/delete | Depth |

`AccountStore::list_accounts` + the connection counter land in P1.

## 10. Decisions (resolved)

- **Deployment:** embedded-only (weftd mounts it). Standalone binary removed.
  Sharding is the future scale path (§3a), not a second run mode.
- **SPA:** a self-contained, no-build-step `ui/index.html` (`include_str!`),
  borrowing the client's design tokens — not a Svelte/Vite app or `rust-embed`.
  KISS; it can graduate to a build pipeline if it grows.
- **Auth secret:** the server-only network seed (no new config).
- **Shape:** read-heavy first, then the moderation actions that were safe
  without a new actor command — both shipped. `DELETE`/suspend/audit next.

---

*Note:* the earlier `web-control-panel-plan.md` (chat client over WS) still
stands for a different goal — giving operators the *desktop admin UI* in a
browser. This document is the plan for a **dedicated operator dashboard**, which
is what "assess reports / list users / browse messages" actually needs.
