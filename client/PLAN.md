# WEFT client — implementation plan

Sequenced execution plan for the Discord-oriented client. The full backlog
lives in [`ROADMAP.md`](./ROADMAP.md); this file is the *order of work* and the
concrete steps per phase. Each phase ships independently.

Legend per item: **verbs** (WEFT wire) · **backend** (Tauri command / event
emit) · **frontend** (Svelte) · **⚠ server** (needs weft-core/proto work first).

---

## Phase 0 — plumbing the client already needs ✅

Small fixes that unblock everything else.

- [x] Handle the **`deleted`** event (was ignored → lingering messages): filters
  the message out of the channel by `msgid` in `handle()`.
- [x] Each message carries a stable **`msgid`** in the frontend model (edit /
  delete / react / reply can now target it); live `message` dedupes by msgid so
  Phase 1 backfill won't double up.
- [x] Message **time from the ULID** (`msgTime()` decodes the Crockford-base32
  timestamp) — correct for backfilled history, falls back to arrival time.
- [x] Backend emits `reaction` / `reactions` events (parsed but unmodelled
  before); frontend `WeftEvent` union updated so Phase 3 has data.

**Acceptance met:** deleting a message removes it live; messages carry ids;
client type-checks + builds.

## Phase 1 — message history / scrollback (biggest gap) ✅

Turns a live-only stream into a real chat log.

- [x] **backend** `history(target, before?)` command → `HISTORY <target>
  [before=] limit=50`. `on_line` tracks an `in_batch` flag across `BATCH
  START/END`, tags each `message` with `history: bool`, and emits `batch-start`
  / `batch-end { truncated }`.
- [x] **frontend** loads the first page on channel open (an `$effect` on
  `active`); infinite scroll-up fires `before=<oldest msgid>` near the top;
  batch messages buffer until `batch-end`, then prepend (deduped by msgid).
- [x] **scroll handling** — pinned-to-bottom only when the reader is at the
  bottom (a prepend no longer yanks them down); scroll position preserved across
  an older-page prepend; msgid-stable `{#each}` keys.
- [x] **indicators** — "loading history…", "older messages have expired"
  (`truncated`), "beginning of #channel" (no more upstream).

**Acceptance met:** default `#general` is `retained:90d`, so opening it backfills
prior traffic and scrolling up pages older; client type-checks, builds, clippy-clean.

## Phase 2 — message actions (edit / delete)

- **verbs** `EDIT <msgid> :<new>`, `DELETE <msgid>`.
- **backend** `edit(msgid, body)`, `delete(msgid)` commands.
- **frontend** hover toolbar on own messages (edit, delete, more); up-arrow in
  the composer edits the last own message; render `edited ×N`; delete →
  tombstone via the Phase 0 `deleted` handler.

**Acceptance:** edit and delete your own messages; both reflect live.

## Phase 3 — reactions

- **verbs** `REACT <msgid> <emoji>`, `UNREACT <msgid> <emoji>` → `REACTION op=` (live), `REACTIONS` (history summary).
- **backend** `react(msgid, emoji, add)` command; the `reaction`/`reactions`
  events from Phase 0.
- **frontend** reaction pills under messages (design `.reaction` CSS exists),
  toggle own reaction, add-reaction button + emoji picker. Aggregate counts;
  mark `mine`.

**Acceptance:** react/unreact, counts update live and survive a history reload.

## Phase 4 — replies · markdown · typing

- **Replies** — **verbs** `MSG … reply-to=<msgid>`. Reply affordance on hover;
  quoted-snippet render (design `.reply-thread`); click to jump.
- **Markdown** — render `fmt=md` (bold/italic/code/links); send `fmt=md`. Use a
  small sanitizing MD renderer (inline only to start).
- **Typing** — **verbs** `TYPING <#chan> start|stop` on composer input
  (debounced); render "X, Y are typing…" from `TYPING` events.

**Acceptance:** reply with a quote, formatted text renders, typing shows.

## Phase 5 — direct messages · presence

- **DMs** — **verbs** `MSG @user`. DM list in the left rail (the "home" button),
  DM conversations keyed by peer, open-DM from a member/profile. Route incoming
  `@user` messages to the DM view (backend already tags them).
- **Presence** — **verbs** `PRESENCE <status>` (self) + `PRESENCE` events
  (others). Status menu on the user footer; status dots on members/DMs.

**Acceptance:** DM a user both ways; set status and see others' dots.

## Phase 6 — servers, channels, membership (the "join dialog" phase)

- **Multi-server rail** — support several concurrent connections; one tile per
  network/namespace; switch active. *(Backend: connection registry keyed by id
  instead of a single `Conn`.)*
- **Join / discover dialog** — **verbs** `DISCOVER [cursor]`, `NS JOIN <name>`.
  "Add a server" modal: browse public namespaces (`NS-META` + `MORE`), join by
  name, or connect to a new network. Replaces the bare join box.
- **Category layout** — **verbs** `CHANNELS <ns>` → `CHANNEL-LAYOUT` (category +
  position). Group the sidebar by **category** (Discord), fall back to retention
  when none.
- **Unread state** — **verbs** `MARK`/`MARKED`. Bold unread channels, unread
  divider, badges.
- **Channel management** — `CHANNEL CREATE/META/POLICY/DELETE`; topic in the
  topbar; right-click menu; leave via `PART`.
- **Members** — ⚠ needs the **`MEMBERS`** verb (server work) for a full roster;
  until then keep the observed-joins list. Group by role; profile popover;
  `display=` names.

**Acceptance:** open the join dialog, discover + join a namespace, channels show
under categories, unread marks work.

## Phase 7 — roles · moderation · admin

- **Roles** — `GRANT`/`REVOKE` UI; capability badges on messages.
- **Invites** — `INVITE MINT/REVOKE/REDEEM`; invite links + redeem-on-join.
- **Reporting** — `REPORT` (message right-click); mod queue `REPORTS LIST/RESOLVE`.
- **Moderation UI** — reason dialogs, ban/mute list, restricted-posting toggle.
- **Namespace admin** — `NS META/VISIBILITY/DELEGATE/DELETE`; `NS CREATE` with
  client-side Ed25519 root keypair generation.

## Phase 8 — account & platform polish

- Logout / switch account; account settings; device keys (`AUTH KEY/ENROLL`).
- Desktop notifications (Tauri plugin); auto-reconnect banner; persistence
  (remember servers, auto-connect); quick switcher (Ctrl+K); context menus;
  toasts; theme/settings.

---

## Server-side prerequisites (schedule before the client phase that needs them)

- **`MEMBERS <#chan>` verb** (proto + core) — blocks the full member roster (Phase 6).
- **`PIN`/`UNPIN` verb** — the `pin` cap exists but no verb; blocks pinned messages.
- **Media** (`STREAM`, BLAKE3) and **voice** (WEFT-RT) — M6, block attachments / voice.
- **Search** — no verb; needs a HISTORY scan or a new command.

## Suggested first cut

Phases **0 → 1 → 2 → 3** make it a real chat client (history + edit/delete +
reactions). Then **Phase 6** (multi-server + join/discover dialog) for the
Discord server experience. Phases 4–5 and 7–8 layer on from there.
