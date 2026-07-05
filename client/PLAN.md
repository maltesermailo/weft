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

## Phase 2 — message actions (edit / delete) ✅

- [x] **backend** `edit(msgid, body)` → `EDIT <msgid> :<body>`, `delete(msgid)`
  → `DELETE <msgid>`. `Message` now forwards an `edited` flag (batch form);
  `Edited` forwards `edit_of` so the client can update in place.
- [x] **frontend** hover toolbar on own messages (edit ✎ / delete 🗑); up-arrow
  in an empty composer edits the last own message; inline editor (Enter save,
  Esc cancel, caret-at-end autofocus); optimistic update + `EDITED` echo
  updates the original in place (no more duplicate line); delete → the Phase 0
  `deleted` handler drops it.
- [x] **`(edited)` marker** rendered from live `EDITED` and from history
  (`MessageEvent.edited`).

**Acceptance met:** edit and delete your own messages live; client type-checks,
builds, clippy-clean.

## Phase 3 — reactions ✅

- [x] **backend** `react(msgid, emoji, add)` → `REACT`/`UNREACT <msgid> <emoji>`.
  The `reaction`/`reactions` events were plumbed in Phase 0.
- [x] **frontend** reaction pills under messages (aggregate `count`, `.mine`
  highlight, click to toggle); a react button in the hover toolbar opens a
  quick-emoji picker popover. `findMsg` also searches the batch buffer so
  history `REACTIONS` summaries attach to still-buffered messages.
- [x] **non-optimistic + correct** — toggling sends `REACT`/`UNREACT` and the
  server echoes our own `REACTION` back (like a MSG ack), so counts can't
  double; other users' reactions and history summaries fold into the same
  aggregate.

**Acceptance met:** react/unreact toggles live and survives a history reload;
client type-checks, builds, clippy-clean.

## Phase 4 — replies · markdown · typing ✅

- [x] **Replies** — `send_message` carries `reply-to=<msgid>`; hover reply
  button sets a compose banner; each message renders a clickable quote snippet
  (author + preview) that scrolls to the original (`id="msg-<key>"` + `jumpTo`).
- [x] **Markdown** — client sends `fmt=md`; bodies render via an inline,
  escape-first `renderMd` (bold/italic/code/strikethrough/links, safe for
  `{@html}`). Non-`md` messages stay plain (Svelte auto-escape).
- [x] **Typing** — `typing(channel, active)` → `TYPING start|stop`, sent
  debounced on composer input (4 s idle stop, stop on send/channel-switch);
  incoming `TYPING` shows "X is typing…" / "X and Y…" / "several people…" with a
  6 s fallback expiry. Server broadcasts + `meta` preservation verified.

**Acceptance met:** reply with a quote, markdown renders, typing shows; client
type-checks, builds, clippy-clean.

## Phase 5 — direct messages · presence ✅

- [x] **DMs** — a DM `MESSAGE` carries `target=@recipient` + `sender`, so the
  conversation is keyed by the *peer* (`own ? target : sender`), landing both
  sides in one thread. The rail "home" button flips the sidebar to a DM list;
  click a member (name or ✉ button) to open/create a DM; `@user` input starts
  one. DMs reuse the whole message pipeline (history, edit/delete, react,
  reply); DM scrollback works (`HISTORY` supports `Target::User`).
- [x] **Presence** — `presence(status)` → `PRESENCE`; a status menu on the user
  footer (online/away/dnd/invisible) with a corner dot; incoming `PRESENCE`
  drives status dots on members, the DM list, and DM headers. We re-announce
  our status on each channel join (the server only broadcasts presence to
  shared channels).

**Limitation:** presence is broadcast-only (no server store), so a member who
was already in a channel before you joined shows a default dot until they next
change status — a proper roster+presence store is Phase 6 / server work.

**Acceptance met:** DM a user both ways; set status and see dots; type-checks,
builds, clippy-clean.

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
