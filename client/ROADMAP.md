# WEFT client — feature roadmap (Discord-oriented)

Gap analysis of the Tauri/SvelteKit client against a Discord-class experience,
mapped to WEFT protocol verbs. `[ ]` = not started · `[~]` = partial.
Refreshed 2026-07-21 after phases 0–8 of [`PLAN.md`](./PLAN.md) shipped.

## Where it is today (implemented)

- **Messaging**: history/scrollback (HISTORY/BATCH, infinite scroll), edit/delete,
  reactions (pills + picker), replies with jump-to-quote, typing indicators,
  pins (+ pins panel), inline markdown (bold/italic/code/strike/links),
  @mention pills + autocomplete + highlight, file attachments via picker
  (image/video/file render), desktop notifications (DMs + mentions).
- **Navigation**: community rail (network + namespace tiles, unread/mention
  rollups), category-grouped channel list with drag-drop reordering, member
  roster (MEMBERS verb, presence dots, owner/mod/bridged badges), DM list,
  Ctrl+K quick switcher, right-click context menus, unread bolding + `@`
  badges synced via MARK/MARKED.
- **Voice**: join/leave voice channels, mute, speaking indicators, inline
  rosters; embedded SFU + LiveKit backends.
- **Identity & admin**: login/register + device-key auth, presence menu,
  profile cards, display name + avatar upload, email/birthday verification,
  namespace create (on-device root key) / settings / roles / delegation /
  recovery quorum / transfer / delete, channel settings (topic, retention,
  restricted posting, per-role caps), invites (mint/revoke/redeem),
  moderation (mute/ban/kick UI + slash commands), report flow + mod queue,
  federation panels (bridges, netblocks, auto-federation toggle).
- **Platform**: auto-reconnect with banner, session persistence + auto-login,
  toasts, dark/light theme, server-side admin panel (weft-admin).

---

## Tier 1 — first-hour gaps (users hit these immediately)

> **Sprints A + B shipped ✅ (2026-07-21)** — fenced code blocks, spoilers,
> paste/drag-drop upload, NEW/day dividers, audio player + image lightbox,
> unread counts, and per-channel/server notification prefs all landed
> (client-only, build + typecheck clean). **Only link previews/embeds remain
> in Tier 1** — and that one needs a server-side unfurl proxy (below).

### Messaging rendering
- [x] **Fenced code blocks** — ` ``` `/`~~~` blocks with a language label,
  content rendered verbatim (escape-first, XSS-safe), inner markdown left
  untouched. **Syntax highlighting** still pending (needs a highlighter dep;
  deferred). Headings/lists/blockquotes still not rendered.
- [ ] **Link previews / embeds** — URLs render as bare links; no
  OpenGraph unfurl, no inline embed of pasted image/video URLs. **The only
  Tier 1 item left**, and it needs a **server-side unfurl proxy** — a new
  weftd axum endpoint (`GET /unfurl?url=`) reusing `dialer::is_dialable`
  (invariant 13, SSRF), fetching OpenGraph/Twitter-card tags + image
  dimensions, with a cache. Client-side fetch is a non-starter (leaks the
  user's IP to arbitrary hosts, hits CORS). Needs Jannik's sign-off on the
  new HTTP surface before building.
- [x] **Paste & drag-drop upload** — Ctrl+V clipboard files and drop-onto-
  composer both feed the shared upload path (`addFiles`), with a "Drop files
  to attach" overlay; still capped at 10/message.
- [x] **Spoilers** — `||text||` renders hidden, click/Enter to reveal
  (delegated `spoilerReveal` action). Spoiler-tagged *attachments* not yet.
- [x] **Audio player + image lightbox** — `audio/*` attachments render an
  inline `<audio controls>`; clicking an image opens a fullscreen lightbox
  (Esc / backdrop to close, "open original" link).

### Timeline orientation
- [x] **"NEW messages" divider + day-date separators** — per-day date
  dividers (Today/Yesterday/full date) from the ULID timestamp, plus a red
  "New messages" line anchored to the read marker as of channel-open
  (frozen while reading, re-anchors on switch).
- [x] **Unread counts** — channels show a numeric `@`-mention count, the
  server rail rolls those up into a numeric badge, and DMs show an unread
  message count. (Non-mention channel unread stays a bold/pill, Discord-style.)
  Now backed by **server-authoritative counts** (see Tier 2, shipped) — the
  client seeds from `UNREAD-COUNTS` and keeps a live tally between pushes.

### Notifications
- [x] **Per-namespace notification prefs** — a **Notification Settings modal**
  (sidebar-header menu → per namespace) with All messages / Only @mentions /
  Nothing. Persisted per-user in localStorage, respected by both desktop
  notifications and the unread indicators (muted namespaces dim + stop
  badging). Set per namespace (not per channel). Cross-device sync of prefs
  still wants a small server store later; per-`@everyone` suppression not yet.

## Tier 2 — the features people name when comparing to Discord

- [x] **Server-controlled unread counts** — shipped (2026-07-21). New
  `UNREAD [<#chan>]` verb → `UNREAD-COUNTS <#chan> <unread> <mentions>`
  event; `EventStore::unread_counts(scope, account, since)` counts non-own,
  non-system roots after the `MARK` marker (mem + PG, shared contract).
  Pushed unsolicited: a per-channel snapshot after each `MARKED` on login,
  and a fresh count on the cross-device `MARK` sync. Client treats it as
  authoritative (survives reload/reconnect, syncs across devices), keeping a
  live +1 tally between pushes; mute stays a client-only preference. Proto
  round-trips + store contract + core black-box tests all green; spec §6.3
  amended. *Deferred:* structured mentions (the count uses a body-text
  heuristic) and a per-message live push (client increments locally instead).
- [x] **Message search** — shipped (2026-07-22). `SEARCH <#chan> :<query>`
  verb → a `BATCH` of matching messages (newest-first, ≤50), membership-gated;
  `EventStore::search` (case-insensitive substring, mem + PG shared contract,
  tombstones + system rows excluded); client search modal (topbar 🔍) with a
  results list + jump-to. Proto round-trip + store contract + core black-box
  tests green; spec §6.4 amended. *Deferred:* Postgres `tsvector` ranking
  (substring for now), namespace-wide search, and loading context around a
  jumped-to result that isn't in the timeline yet.
- [x] **Threads** — shipped (2026-07-22). Built on the existing `thread=`
  message tag: the server's `HISTORY thread=<root>` filter (was stubbed)
  now returns a thread via `EventStore::thread_roots` (mem + PG shared
  contract); client-core carries `thread` on messages + send + history;
  the client shows a **thread side panel** (root + replies + composer),
  a "N replies" indicator on roots, a "reply in thread" action, and hides
  thread replies from the main timeline (Discord-style). Store contract +
  core black-box tests green; spec §9.4 amended. *Deferred:* server-side
  reply counts (client tallies from loaded messages), thread mute/notif,
  and a dedicated thread list.
- [ ] **Custom / per-namespace emoji** — picker is a hardcoded unicode set;
  no upload, `:name:` autocomplete, skin tones, or recently-used. Needs a
  server emoji registry (media store can host the images) + client work.
- [~] **Voice depth** — today: join/leave/mute/**deafen**/speaking rings.
  Missing: **screen share, video, per-user volume, device selection,
  push-to-talk**. LiveKit (M-lk-1/2/3) gives screenshare/video nearly free —
  cheapest "wow" gap to close.
- [ ] **Group DMs** — DMs are strictly 1:1. Needs protocol design (spec §18
  territory) — flag, don't build.
- [ ] **Custom status text · per-server nicknames · bios** — profile has one
  global display name + avatar + presence enum. Nickname-per-namespace and
  bio need small server/store additions.

## Tier 3 — polish & platform

- [ ] **Collapsible categories** (persisted per-user) — trivial client fix;
  drag-reorder already works.
- [ ] **Slash-command autocomplete** — a `/` popup menu; commands are the
  main mod surface but undiscoverable today. Client-only, cheap, high
  perceived quality.
- [~] **Accessibility** — modal focus-trap, reduced-motion, keyboard nav in
  emoji/member pickers (aria-labels exist, little else).
- [ ] **Settings depth** — notification prefs, keybind customization, audio
  device selection, font scaling/density, language. Appearance is a
  dark/light toggle only.
- [ ] **Mobile / responsive layout** — fixed three-column grid, no
  breakpoints; a rival needs at least a narrow-window mode.
- [ ] **Credential hardening** — password sits in localStorage (flagged
  in-code as dev-only); OS keychain before any public release.
- [ ] **Jump-to-date** — scroll-paging only.

## Federation surfacing (WEFT-specific, no Discord analog)

- [~] Trust marks (signed / bridged) on the community rail from real
  manifest state; bridged badges exist on messages/members.
- [ ] Foreign-invite auto-routing; live connecting/failed bridge state.

---

## Suggested order

1. **Client sprint A**: paste/drag-drop upload · code blocks · NEW/day
   dividers · spoilers — transforms daily feel, zero server work.
2. **Client sprint B**: notification muting · unread counts · slash
   autocomplete · collapsible categories.
3. **Search** — server verb first, then UI. The headline feature gap.
4. **LiveKit M-lk-1/2/3** — screenshare/video/deafen/devices ride in.
5. Park **threads, group DMs, custom emoji** as spec-design items (§18) —
   all three touch the wire protocol.

## Protocol gaps (need server/proto work *before* the client can use them)

- [ ] **Search verb** — no wire command; HISTORY scan or a new command.
- [ ] **Threads** — no wire model (M6+).
- [ ] **Emoji registry** — no verb for custom-emoji upload/list.
- [ ] **Group DMs** — no multi-party DM target (§18).
- [ ] **Notification-pref sync** — optional small store for cross-device
  mute state.
- [ ] **Nickname-per-namespace / bio fields** — profile store additions.
