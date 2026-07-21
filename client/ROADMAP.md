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

> **Sprint A shipped ✅ (2026-07-21)** — fenced code blocks, spoilers,
> paste/drag-drop upload, and the NEW/day dividers all landed (client-only,
> build + typecheck clean). Remaining Tier 1: syntax highlighting, link
> embeds, audio player/lightbox, unread counts, notification prefs.

### Messaging rendering
- [x] **Fenced code blocks** — ` ``` `/`~~~` blocks with a language label,
  content rendered verbatim (escape-first, XSS-safe), inner markdown left
  untouched. **Syntax highlighting** still pending (needs a highlighter dep;
  deferred). Headings/lists/blockquotes still not rendered.
- [ ] **Link previews / embeds** — URLs render as bare links; no
  OpenGraph unfurl, no inline embed of pasted image/video URLs. Do the
  unfurl **server-side** (client-side fetch leaks IPs / hits CORS).
- [x] **Paste & drag-drop upload** — Ctrl+V clipboard files and drop-onto-
  composer both feed the shared upload path (`addFiles`), with a "Drop files
  to attach" overlay; still capped at 10/message.
- [x] **Spoilers** — `||text||` renders hidden, click/Enter to reveal
  (delegated `spoilerReveal` action). Spoiler-tagged *attachments* not yet.
- [~] **Audio player + image lightbox** — audio attachments fall through to
  a generic download chip; images open raw, no gallery. Client-only.

### Timeline orientation
- [x] **"NEW messages" divider + day-date separators** — per-day date
  dividers (Today/Yesterday/full date) from the ULID timestamp, plus a red
  "New messages" line anchored to the read marker as of channel-open
  (frozen while reading, re-anchors on switch).
- [~] **Unread counts** — badges are boolean dot/`@`, not numbers.

### Notifications
- [ ] **Per-channel / per-server notification prefs** — mute, all /
  mentions-only / nothing, suppress `@everyone`. Nothing exists; desktop
  notifications are hardcoded. Client-first; cross-device sync of prefs
  wants a small server store later.

## Tier 2 — the features people name when comparing to Discord

- [ ] **Message search** — nothing client-side and **no server verb** (the
  biggest server prerequisite on this list; Postgres full-text over the
  event store would do for v1).
- [ ] **Threads** — absent entirely; spec parks it at M6+. Server + client.
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
