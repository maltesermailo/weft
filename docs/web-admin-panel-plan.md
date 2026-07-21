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


NEW PLAN

# WEFT Console — Feature Plan

Scope decisions: deep coverage of federation ops, trust & keys, moderation & reports, and IRC gateway ops. The panel ships with the server for any WEFT admin, and it gets full control including destructive deletes. Those two decisions drive most of the architecture below — a panel that ships to strangers can't assume a trusted single operator, and destructive power over E2EE rooms needs careful framing since the server never sees plaintext.

## 0. Design pack, front-end & content boundary (resolved)

### Design pack

The console's visual target is the template pack in **`design/admin/`**:
`weft.css` (the entire "dyed thread" dark design system — single source of
truth), `layout.html` (the shell: left **selvage** strip, grouped sidebar,
operator header with the woven weft line), and four content templates —
`page-search-list.html`, `page-detail.html`, `page-moderation.html`,
`page-data-table.html` — plus `components.html` (a rendered gallery) and a
`README.md` of conventions (type discipline, scarce gold accent, `knot` status
vocabulary `woven|frayed|severed|idle`, typed-name confirmation on deletes).
Build the real panel to match its class names and visual output.

### Front-end: Client SPA on the JSON API (decided)

**Resolved:** port the design pack into the **existing self-contained SPA that
fetches the JSON API** — *not* server-side templating. Keep §10's API-first
stance: `ui/index.html` grows to render the design's markup with `weft.css`
inlined/embedded, and every action calls `fetch("/admin/api/v1/…")`. The
pack's `method="post" action="{{endpoint}}"` forms become `fetch` POSTs; its
`{{…}}` / `@each` / `@slot` placeholders become client-side rendering. This
**supersedes** two suggestions written elsewhere: the design README's
"Askama/Maud/Tera server-side templating" and NEW-PLAN §1's `include_dir` +
separate `weft-admin-api` crate — we stay in the single `weft-admin` crate with
a versioned `/admin/api/v1` prefix + a typed `types` module (WC1). The crate
split stays deferred until a real third-party API client exists.

### Content boundary: readable where the server holds plaintext (decided)

The panel **can read message content wherever the server legitimately holds
plaintext** — public/unlisted channels, any non-`e2ee` channel, and **non-E2EE
DMs**. WC0 already browses channel messages and per-account authored messages;
this extends the same materialized view to non-e2ee DMs. The gate is the
channel/DM **retention policy**: an `e2ee` target shows "unavailable by policy"
and the panel holds/reconstructs **no** plaintext for it (invariant 8, spec
§14). Because E2EE (openmls) is **deferred (M6+)** in this codebase, today the
`e2ee` branch is effectively empty and essentially all content is readable — but
the check is written against the policy so it's correct the moment E2EE lands.
This **amends** the NEW PLAN's "never content" framing in §2 and §5: content
moderation is *not* limited to voluntarily-attached excerpts here — it's limited
to non-e2ee surfaces, which is a superset (and, pre-E2EE, everything).

### Sidebar groups → WC milestones

| Design nav group | Pages | WC milestone |
|---|---|---|
| Lookup | Users · Channels · Applications | WC4 (+ WC0 users, content browse) |
| Federation | Peers · Transit Queue · Remote Channels | WC5 |
| Trust & Keys | Devices · Capability Tokens · Revocations | WC6 (device/MLS parts E2EE-gated) |
| Moderation | Reports · Phrase Bans · Media Blocklist | WC7 (+ WC0 reports, media blocks) |
| Gateways | IRC Bridge | WC8 |
| Observability | QUIC Transport · Audit Log | WC9 (+ WC1 audit) |

### Naming: "Channels" (decided)

**Resolved:** UI copy uses **Channels** (and **Namespaces**) — the protocol +
store nouns — not the design pack's "Rooms". Operators reason in the terms the
wire uses. The design mocks + `page-*.html` templates still say "room" as
reference prose; the build substitutes Channels/Namespaces (nav: `Channels`,
`Remote Channels`). The `knot` weaving vocabulary (woven/frayed/severed) stays —
it's status, not a noun. The "MLS epoch" / device-group surfaces the pack mocks
show are E2EE-gated (WC6/WC7) — surface them only once openmls lands.

## 1. Architecture foundation

**Admin API as a first-class surface.** The panel should be a pure client of a versioned admin API exposed by the server (e.g. `/_weft/admin/v1/…` over the same QUIC/HTTP stack, or a dedicated mTLS listener). Nothing in the panel talks to the database directly. This keeps the panel replaceable, makes every admin action scriptable, and means third parties can build their own tooling against the same API. Consider a seventh workspace crate (`weft-admin-api`) holding the route handlers and typed request/response structs, with the panel's static assets embedded via `include_dir!` so a single binary serves everything.

**Panel authentication and RBAC.** Since any WEFT admin will run this, operator identity can't be hardcoded. Reuse WEFT's own primitives: an operator is a WEFT user whose device holds an *operator capability token* with scoped grants. Suggested roles as capability scopes rather than an enum: `admin.read` (observability), `admin.moderate` (structural actions), `admin.destroy` (deletes), `admin.federation`, `admin.keys`. This dogfoods the capability system and gives you fine-grained delegation for free — an admin can issue a read-only token to a junior moderator the same way users delegate room capabilities.

**Admin audit trail (non-optional).** Every admin API call gets an append-only, hash-chained audit record: who, what, target, timestamp, request payload digest, previous-record hash. Because the panel ships to others, tamper-evidence matters more than it would for a personal tool. Surface this as the Audit Log view, filterable by operator, action type, and target.

**Confirmation model for destructive actions.** Typed-name confirmation (retype the room name / handle) for deletes, plus a configurable soft-delete grace window (default 7 days) during which the object is tombstoned but recoverable. Optionally a two-operator rule for `admin.destroy` actions, off by default, for larger deployments.

## 2. Lookup

**Users.** Search by handle, user ID, device fingerprint, email, or IP. Detail page as mocked: account info, device list, capability tokens, flags, room memberships, report history, and DM/room metadata — plus **content** for non-e2ee channels and non-E2EE DMs (readable per the §0 content boundary; `e2ee` targets show metadata only). Add a "find related" pivot on IP and email domain like Fluxer has; it's genuinely useful for spam waves.

**Rooms.** Search local and known-federated rooms. Detail: state chain head, MLS epoch, member list with per-member join path (direct, invite, gateway), media storage footprint, federation replication status per peer.

**Applications/bots.** Registered bot accounts and their token scopes, with rate-limit class assignment.

## 3. Federation ops

**Peers view** (as mocked): every known peer with state (*woven / frayed / severed*), RTT, last handshake, protocol version, pinned server key fingerprint, and shared-room count. Actions: sever (block), re-weave (unblock), force re-handshake, and a peer detail page showing per-room replication lag.

**Peer trust policy.** A server-wide setting choosing open federation, allowlist, or blocklist mode, editable here. Severing should support granularity: whole peer, or per-room (already in the mockup's room actions).

**Transit queue.** The outbound/inbound replication backlog: per-peer queue depth, oldest pending event age, retry schedule. Actions: retry now, drop poisoned events (with audit record), pause a peer's queue. This is the page you'll live in when a peer is frayed.

**Peer key rotation handling.** When a peer rotates its server signing key, surface it as a pending trust decision rather than silently accepting — TOFU with operator review.

## 4. Trust & keys

**Device registry.** Global device search by fingerprint. Per-device: Ed25519 key, MLS leaf positions across rooms, first/last seen, transport history. Revoking a device must show its blast radius before confirming: which rooms will rotate epochs, which sessions die.

**Capability token inspector.** Paste or look up any token and see its parsed chain: issuer, scopes, delegation path back to the root grant, expiry, revocation status. Render the delegation graph visually for chains deeper than two hops — this becomes the debugging tool for "why can/can't user X do Y."

**Revocation list management.** View and manage the server's published revocation set (devices and tokens), with propagation status per federation peer, so you can answer "does thread.example.net know this token is dead yet?"

**Key transparency (later).** An optional Merkle log of device key changes per user, letting clients detect a malicious server swapping keys. Big feature, but it's the kind of thing that makes WEFT credible as an E2EE protocol; the admin panel would show the log head and inclusion-proof health.

## 5. Moderation & reports

**What "full control" means under E2EE.** For **`e2ee`** channels the server holds only ciphertext, state chains, and metadata — never plaintext — so destructive control there is structural: delete state and ciphertext blobs, kick members, sever replication, suspend accounts. Content-based moderation of e2ee channels works only when a reporter voluntarily attaches decrypted excerpts to a report (the Matrix model). **For every non-e2ee channel and non-E2EE DM the server holds plaintext**, so the panel reads and moderates content directly (§0 content boundary) — as do the always-plaintext surfaces: profiles, channel names/topics, invites, and the IRC gateway. Pre-E2EE (openmls deferred, M6+) that non-e2ee path is *everything*. The panel should be honest about this boundary in its UI copy — the e2ee blind spot is a selling point, not a limitation.

**Reports queue.** User-filed reports with category, reporter, target, optional attached plaintext excerpt (signed by the reporter's device so excerpts can't be forged), and resolution workflow: claim, resolve with action, dismiss. Bulk actions for spam waves.

**Account actions.** Suspend (login blocked, tokens frozen), shadow-limit (rate-limited, invisible to non-members), forced device logout, delete with grace window. Flags as in the mockup.

**Room actions** (as mocked): rename, transfer founder, force epoch rotation, freeze (read-only), sever federation, delete with tombstone. Tombstones must federate so peers stop replicating and can show "removed by origin server."

**Plaintext-surface filters.** Phrase bans and media-hash blocklists apply only where the server sees content: profiles, room metadata, unencrypted rooms if WEFT supports them, and gateway traffic. Keep them in a "Filters" section scoped explicitly to those surfaces.

## 6. IRC gateway ops

The gateway is the one place the server legitimately sees message plaintext, so it gets its own deeper toolset. Per-network config (servers, TLS, SASL), link status with reconnect/backoff state, channel↔room mappings with create/edit/unlink, puppet account overview (which WEFT users have IRC presence, nick collisions), flood/rate controls per network, and gateway-side content filters (this is where phrase bans genuinely work). A live gateway log tail with severity filtering rounds it out — netsplit debugging without SSH.

## 7. Observability

QUIC transport dashboard (connections, handshake failures, 0-RTT resumption rate, per-peer congestion stats), storage footprint by room and media type, and the admin audit log from §1. Skip full metrics dashboards — export Prometheus metrics and let admins bring Grafana; the panel only needs the views that drive decisions inside it.

## 8. Suggested phasing

**MVP (ship with first public server release):** admin API + operator capability auth, audit trail, user/room lookup and detail, account suspend/delete, room delete with tombstone, peers view with sever/re-weave, device revocation, reports queue.

**v2:** transit queue tooling, capability token inspector, revocation propagation status, IRC gateway config and mappings, plaintext-surface filters, typed-confirmation and grace-window polish.

**Later:** delegation graph visualization, key transparency log, two-operator rule, peer key rotation review flow, storage analytics.

The MVP list is deliberately the set an admin needs on day one to run a public server responsibly: see what exists, remove what's abusive, control who they federate with, and prove afterward who did what.

## 9. Milestones (each independently shippable)

The §8 buckets, sequenced into a concrete ladder against the **already-shipped
`weft-admin` crate** (the "Shipped (embedded)" section at the top of this doc is
the substrate — call it **WC0**). Each milestone below is a real diff you could
ship and stop at. Status: ✅ done · ◑ partial · ☐ not started. Where a milestone
depends on protocol machinery this repo hasn't built yet, it's flagged and
parked — we don't fake a surface over a subsystem that doesn't exist.

Naming: **WC** = WEFT Console, to avoid colliding with the protocol milestones
(M0–M7, M-lk-*, M-media-*) in `CLAUDE.md`.

### WC0 ✅ — embedded panel baseline (shipped)

The current `weft-admin`: operator HMAC-cookie auth (operator = holds a cap at
`*`), read views (stats, reports + retention-held context, accounts
list/detail/messages, channels, namespaces, grants, moderation, peers,
netblocks, media-blocks, channel-message browse) and write actions (resolve
report, mute/ban/kick, delete account, delete message, add/remove netblock,
block/unblock media). Self-contained `ui/index.html` (`include_str!`), `Live`
port for kick/eject + delete-any. This already covers roughly half of §8's
"MVP" bucket; the milestones below are what turns it into the WEFT Console.

### WC1 ✅ — API contract + audit spine

The foundation everything destructive rides on. All three pieces shipped:

- **Hash-chained audit trail ✅ (non-optional, §1).** Shipped: a new
  `AuditStore` store role (`AuditEntry`/`AuditRecord` + a shared pure
  `audit_hash` — blake3, mem + PG both compute it identically, like
  `compaction_plan`), append-only + hash-chained (`seq`/`prev_hash`/`hash`; PG
  serializes appends under a `pg_advisory_xact_lock`), migration `0023_audit`,
  and a mem+PG contract case proving chain-linkage, newest-first filtered
  listing, and tamper-detection. Every WC0 write handler (moderate/kick, resolve,
  account-delete, message-delete, netblock ±, media ±) now emits a record via an
  `audit()` helper that digests the payload (reasons/notes are **never** stored
  raw). Read-only `GET /admin/api/v1/audit?operator=&action=` + `tests/api.rs`
  `write_actions_land_in_the_audit_log`.
- **Versioned, typed API ✅.** All routes sit behind `/admin/api/v1/*` (handlers
  + login/logout + the SPA's `API` base + the conformance/e2e tests). Every
  response is now a named `#[derive(Serialize)]` struct in `weft-admin::dto`
  (`Me`, `Stats`, `AccountSummary`/`AccountDetail`, `Grant`, `Report`/
  `ReportDetail`, `Msg` (untagged message/tombstone), `Peer`, `Netblock`,
  `MediaBlock`, `Audit`, …) with `From<StoreRecord>` conversions — no ad-hoc
  `json!` responses remain (only the audit **payload digests** still use `json!`,
  which is correct). Per §0 this stays in the single `weft-admin` crate; the
  `dto` module is the seam a `weft-admin-api` split would lift out later.
- **Design-pack SPA shell ✅.** `ui/index.html` rebuilt on the `design/admin/`
  system: `weft.css` embedded, the selvage strip, the WEFT wordmark + **grouped
  sidebar** (Lookup / Federation / Trust & Keys / Moderation / Gateways /
  Observability — the design IA, with `soon`-tagged placeholders for the
  not-yet-built pages so the roadmap is visible), the operator header (woven
  weft-line + `OPERATOR` pill + op identity from `/me`), and cards (shuttle bars),
  `.kv` grids, `.result` lookup rows, `.knot` peer states (woven/frayed/severed),
  and the `.btn`/`.pill` vocabulary. All views are `fetch("/admin/api/v1/…")`-
  driven (the design's form-posts became fetch calls, per §0) — every existing
  action preserved. **New Audit Log screen** surfaces the WC1 chain. Verified by
  rendering each view in a browser against stubbed fixtures (dashboard, users,
  audit, peers, detail). Uses **Channels/Namespaces** copy, not "Rooms" (§0).

### WC2 ✅ — capability RBAC (adopted)

**Shipped:** scoped admin capability tokens replace WC0's binary "operator =
cap at `*`" (supersedes the operator-only stance in §10 below). Scopes:
`admin.read` / `admin.moderate` / `admin.destroy` / `admin.federation` /
`admin.keys` (`auth::AdminScope`). Sources: config **operators auto-hold all**
(zero-config back-compat); delegated admins hold a subset via **`admin`-scope
capability grants keyed by account ULID** — dogfooding §10.4, so
`GRANT admin admin.moderate <account>` (or `REVOKE`) manages a junior moderator
with the same machinery users delegate room caps. `admin_scopes()` computes the
live set each request (operators → all; else unexpired `admin` grants; `*`/
`admin.*` → all), so revoke/expiry take effect immediately.

Enforcement: `require_admin` middleware authenticates + requires the
**`admin.read` baseline** (uniform 401, anti-enumeration) and injects the
caller's `AdminScopes`; each **write handler** guards its scope (`require(&scopes,
AdminScope::…)` → **403** if missing) — reads (moderate) · deletes (destroy) ·
netblocks (federation) · media/report-resolve (moderate). Login now admits any
account with the read baseline, not just config operators. `GET /me` returns the
held scopes; the **SPA hides controls it can't use** (a read-only admin sees
"Read-only — you don't hold `admin.moderate`" and its held scopes as header
pills instead of the `OPERATOR` badge — the server enforces regardless).

Tests: `read_only_admin_reads_but_cannot_write` (403 on every write scope),
`moderate_admin_moderates_but_cannot_destroy`, `a_registered_non_admin_cannot_
log_in` — plus the 8 operator-path tests still green (operators auto-hold all).
Browser-verified the read-only UI. `admin.keys` is reserved (no endpoints until
WC6). **[refinement]** epoch-revocation of admin grants relies on `REVOKE`
removing the row; bulk `bump_epoch` invalidation isn't wired for the `admin`
scope yet.

### WC3 ◑ — destructive-action safety

The account hard-delete — the one truly irreversible panel action — is now
gated. **Shipped:**

- **Typed-name confirmation ✅ (server-enforced).** `DELETE /accounts/:name`
  requires `?confirm=<name>` to echo the account name (400 otherwise); the
  no-self-delete rule is checked first. The SPA renders the design's danger
  zone: a "Type <name> to confirm" input + solid-red Delete button.
- **Soft-delete grace window ✅ (configurable, default 7 d).** DELETE now
  *schedules* the hard-delete `delete_grace_ms` out (`AccountStore::
  schedule_deletion`/`cancel_deletion`/`deletion_scheduled`/`due_deletions` —
  mem + PG + contract, migration `0024`, nullable `purge_at_ms`). The account
  keeps working during the window and is **recoverable** via
  `POST /accounts/:name/restore`. The `weft-core` maintenance loop finalizes due
  accounts (`purge_due_deletions`, split-out + unit-tested like the other
  passes). `[admin] delete_grace_days` config → `AdminState::with_delete_grace_ms`.
  The account list/detail carry `deletion_scheduled`; the SPA shows a
  "pending delete" knot + a "Deletion scheduled → Restore" card. All audited
  (`account.schedule_delete` / `account.restore`).

Tests: `account_delete_is_typed_name_confirmed_and_self_delete_blocked`,
`account_delete_schedules_a_grace_window_and_restores`, the store contract
soft-delete block, and `weft-core::purge_due_deletions_finalizes_only_past_
windows`. Browser-verified both danger-zone states.

**Remaining (deferred):**
- **Two-operator rule** for `admin.destroy` (off by default) — needs a
  pending-approval flow (first approver records intent, second executes); its
  own state + endpoints.
- **Message-delete** already soft-deletes (tombstone via the channel actor) and
  is scope-gated + audited, so it doesn't need the grace machinery; a typed-name
  gate doesn't fit a ULID. **Channel/namespace delete** (WC7) will reuse the
  same typed-name `?confirm=` gate + (where a hard purge applies) the grace
  window.
- **Login-block during the grace window** is intentionally *not* here — a
  scheduled account still functions until finalize; blocking access is the WC7
  **suspend** action, not soft-delete.

### WC4 ◑ — lookup depth (users & channels)

**Shipped — user detail depth (§2 "User"):**
- **Device list ✅** — `AccountStore::devices` (mem + PG + contract); the detail
  renders each enrolled Ed25519 pubkey as a truncated grouped-hex fingerprint
  (`device_fingerprint`, display-only).
- **Flags card ✅** — account state (`OPERATOR`/`MUTED`/`BANNED`/`PENDING_DELETE`)
  as the design's `.flags` toggle grid (client-side, from existing data).
- **Capability tokens held ✅** — the grants-across-scopes card (already WC0).
- **"Find related" ✅** — `AccountStore::accounts_by_email_domain` (case-
  insensitive on the part after `@`, mem + PG + contract); `account_detail`
  returns `related` (same-domain accounts, minus self), rendered as clickable
  pivots — the spam-wave tool.

**Shipped — channel detail:** new `GET /channels/:name/detail` (policy + the
persistent member roster via `MembershipStore::members`) + an SPA channel-detail
view (clickable from the channels list; members pivot to user details; "browse
messages" link). Test: `account_detail_carries_devices_and_related_and_channel_
roster`. Browser-verified user + channel detail.

**Remaining (deferred — not yet tracked/plumbed):**
- **IP-pivot** — parked; the transport layer doesn't surface/persist client IPs
  (needs `run_session` plumbing).
- **Per-member join path** (direct / invite / gateway), **media storage
  footprint** per channel, **per-peer replication status** — none are tracked or
  aggregated today; each needs new store instrumentation. Channel detail ships
  the roster now; these are additive.
- **Content browse for non-E2EE DMs ✅** (§0) — `GET /dms/:a/:b/messages`
  materializes the thread for a participant pair (`Scope::dm` normalizes order).
  The §0 gate is real: `AdminState.dm_policy` (wired from weftd `dm_policy`) —
  when it's `e2ee` the response is `unavailable: true` with **no** messages
  materialized (invariant 8), and the SPA shows "unavailable by policy". The
  Messages screen gained a DM row (two accounts → browse). Test:
  `dm_thread_browse_reads_non_e2ee_and_gates_e2ee`; browser-verified both states.

Deferred to later WC milestones (need new instrumentation): IP-pivot, per-member
join path, media storage footprint, per-peer replication status.

### WC5 ◑ — federation ops

**Shipped — peer detail:** clicking a peer opens `GET /peers/:name/detail`,
which parses the stored **signed manifest** (`weft_crypto::SignedManifest::
from_b64`) into the operator-relevant facts: the **pinned key fingerprint**
(`fingerprint_hex` of the signer pubkey) + `verified` self-auth, the **shared
channel set** (= "shared-room count"), and the negotiated `history`/`media`/
`typing`/`voice` modes — plus the record's scope/version/acked/severed/
created/updated and whether the peer is **netblocked**. SPA peer-detail view
with the woven/frayed/severed knot vocabulary.

**Shipped — sever / re-weave:** wired to the existing **NETBLOCK** endpoints —
because at the network level a netblock *is* the §11.6 sever (reject bridge auth
+ proposals, sever manifests, drop ingestion; invariant 7). The peer detail
shows a "Sever (netblock)" / "Re-weave (unblock)" action (`admin.federation`),
reusing `POST`/`DELETE /netblocks`. Test:
`peer_detail_parses_manifest_and_shows_shared_channels`; browser-verified.

**Remaining (deferred — need instrumentation that doesn't exist yet):**
- **RTT + last-handshake** — no transport-level timing is surfaced/persisted
  (needs connection instrumentation, like WC4's IP-pivot).
- **Transit queue** (backlog depth, oldest-pending age, retry / drop-poisoned /
  pause) — forwarding is live (mpsc/broadcast), not a persisted queue; there's
  nothing to inspect until an outbound queue is materialized.
- **Force re-handshake** — a live dialer action; needs a federation analog of
  the channel `Live` port.
- **Peer key-rotation TOFU review** — no pending-review state; today the pinned
  model rejects a changed key and accept-any trusts it. A "pending trust
  decision" queue is new state + flow.

### WC6 ☐ — trust & keys (gated on E2EE/MLS)

§4: device registry (global fingerprint search, per-device keys + first/last
seen, **revoke-with-blast-radius** preview), capability-token inspector (parse
the delegation chain: issuer → scopes → path → expiry → revocation status), and
revocation-list management with per-peer propagation status. **Gate:** the
device/MLS-leaf and key-transparency parts assume openmls/E2EE, which `CLAUDE.md`
lists as **deliberately deferred (M6+)**. Ship the capability-token inspector and
the revocation-set management now (both exist today in weft-crypto/weft-store);
park the MLS-leaf/device-epoch views until E2EE lands.

### WC7 ☐ — moderation depth

§5, beyond WC0's mute/ban/kick/delete. **Account:** suspend (login blocked +
tokens frozen), shadow-limit (rate-limited, invisible to non-members), forced
device logout — each an audited store mutation + live broadcast. **Room:**
rename, transfer founder, freeze (read-only), delete-with-**federating**
tombstone (peers stop replicating, show "removed by origin"). Force epoch
rotation is **parked on E2EE/MLS** (see WC6). **Reports:** bulk actions for spam
waves; verify reporter-attached excerpt signatures (§5 — excerpts signed by the
reporter's device so they can't be forged). Reporter confidentiality (invariant
12) stays enforced throughout.

### WC8 ☐ — IRC gateway ops

§6 — where external plaintext enters the server (and the one surface that stays
plaintext even once channels go `e2ee`), so it gets its own toolset: per-network
config (servers, TLS, SASL), link status +
reconnect/backoff state, channel↔room mapping CRUD, puppet-account overview (WEFT
users with IRC presence, nick collisions), per-network flood controls, and
**gateway-side content filters** (phrase bans / media-hash blocklists — this is
where content filtering genuinely works, vs. the E2EE surfaces where it can't).
Live gateway log tail with severity filter. Builds on the shipped `weft-irc`
crate.

### WC9 ☐ — observability

§7: a QUIC transport dashboard (connections, handshake failures, 0-RTT
resumption rate, per-peer congestion), storage footprint by room and media type,
and the WC1 audit log surfaced as a first-class view. Export Prometheus metrics
and leave Grafana to admins — the panel only carries the views that drive
decisions taken *inside* it.

### Later (optional)

Delegation-graph visualization (§4, for chains deeper than two hops),
key-transparency Merkle log (§4 — big, but it's what makes WEFT credible as an
E2EE protocol), two-operator-rule polish (§1), storage analytics (§7).

### Mapping to §8

| §8 bucket | Milestones |
|---|---|
| **MVP** | WC0 ✅ (auth, lookup, reports, suspend/delete, peers, device revoke) + WC1 (audit) + WC3 (delete safety) + WC5 (peers/sever/re-weave) |
| **v2** | WC2 (RBAC) · WC4 (lookup depth) · WC5 (transit queue) · WC6 (token inspector, revocation) · WC7 (mod depth) · WC8 (IRC) |
| **Later** | WC9 (observability) + the Later block above |

### Cross-cutting notes

- **Resolved (§0):** front-end = Client SPA on the JSON API (not server-side
  templating); the design pack `design/admin/` is the visual target; the
  `weft-admin-api` crate split stays deferred (single crate + `types` module);
  UI copy uses **Channels/Namespaces**, not "Rooms".
- **Resolved:** **capability RBAC is adopted** (WC2) — the `admin.*` scopes
  replace the WC0 binary operator, `*`-operators auto-hold every scope. The scope
  *names* still get a final ratify (spec §18 territory), but the model is settled.
- **Honesty about E2EE:** WC6 and the epoch-rotation slice of WC7 assume MLS,
  which is deferred. They're written into the ladder for completeness but gated —
  don't build a device/epoch UI over a subsystem that doesn't exist yet.
- Each milestone follows the repo convention: store change = trait + mem + PG +
  one shared contract case + migration, then handler, then a `tests/api.rs`
  end-to-end case; every write action emits an WC1 audit record once WC1 lands.