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

## Phase 6 — servers, channels, membership (the "join dialog" phase) 🟡 mostly done

- [x] **Join / discover dialog** — `DISCOVER [cursor]` + `NS JOIN`. Rail "+"
  opens an "Discover namespaces" modal: browse public namespaces (`NS-META` +
  `MORE` paging), join by name, join a card. Joining also fetches the layout.
- [x] **Category layout** — `CHANNELS <ns>` → `CHANNEL-LAYOUT`; sidebar groups
  by **category** (position-ordered), uncategorized under "Channels", retention
  shown as a per-item dot.
- [x] **Unread state** — `MARK`/`MARKED`. A message in a non-active channel
  bolds it + shows an unread dot; viewing a channel clears it and advances the
  read marker (synced across devices via `MARKED`).
- [x] **Channel management** — topic in the topbar (`CHANMETA`); leave button +
  `/part`; `/create #chan`, `/delete`, `/topic <text>` (backend commands
  `part/channel_create/channel_delete/channel_meta`).
- [x] **Server tiles (flavor A)** — namespaces render as rail tiles alongside the
  network tile; selecting one filters the sidebar to that namespace's channels
  (grouped by category), keeps the active tile in sync with the open channel,
  and shows short channel names. *One connection* — a regrouping of existing
  data, not the heavy multi-network refactor.
- [x] **Members roster** — the **`MEMBERS` verb** (proto + core + spec amendment,
  8 new tests): a membership-gated `BATCH` of `MEMBER … join` rows. The client
  requests it once per channel on open, folding the full roster in.
- [ ] **Multiple networks (flavor B)** — *still deferred*: N concurrent QUIC
  connections needs the `Conn`→registry backend refactor + per-server state.
- [ ] **Roster polish** — role grouping, profile popover, `display=` names,
  server-side presence store (fixes the Phase 5 default-dot limitation).

**Done:** discover/join, category layout, unread, channel mgmt, **namespace
server tiles**, and a **real member roster** — all build clean. **Remaining:**
flavor-B multi-network + roster polish.

## Phase 7 — roles · moderation · admin ✅ (2 multi-party/server items deferred)

- [x] **Reporting** — a report (flag) action on others' messages opens a dialog
  (category · route ns/net · note) → `REPORT`; a topbar **Reports queue** runs
  `REPORTS LIST` and shows filed reports with a resolve dropdown (`REPORTS
  RESOLVE`), honest `state=` badges (verified/unverified/reporter-attested).
- [x] **Roles** — a member-row roles dialog grants/revokes a capability at a
  chosen scope (`#chan` · `ns:` · `*`) via `GRANT`/`REVOKE`; `TOKEN` confirms.
- [x] **Invites** — a topbar invite button mints a shareable link (`INVITE
  MINT` → `INVITED`), shown in a copy dialog; a redeem field in the discover
  modal runs `INVITE REDEEM` (accepts a full `weft://…/i/…` link or bare token).
- [x] **`NS CREATE` with on-device root key** — `weft-crypto` keypair generated
  in the Tauri backend, secret stored `0600` in the app-data dir (`keys.rs`,
  keyed by network+namespace), only the pubkey sent via `@root=`. Create row in
  the discover dialog auto-makes `#<ns>/general` + joins. `load_ns_key` is the
  hook for future TRANSFER/RECOVERY signing (weftd's recovery ladder is already
  fully implemented + tested server-side).
- [x] **Namespace admin panel** — a gear in the sidebar header (for a namespace
  you're in) opens settings: **profile** (`NS META` title/description +
  `NS VISIBILITY`), **delegate roles** (`NS DELEGATE`), **recovery quorum**
  (`NS RECOVERY SET` M-of-N), and a **danger zone** — `NS DELETE` and **root-
  signed `NS TRANSFER`** (backend loads the stored key + `weft-crypto::
  sign_transfer`). A **recovery-pending banner** (from `NS-META` recovery fields)
  offers a root-signed **`NS RECOVERY CANCEL`** veto.
- [ ] **`NS RECOVER`** (quorum rung) — *deferred*: needs the quorum members'
  keys + a co-signed `RotationRecord` (multi-party flow, not single-owner).
- [ ] **Capability badges on messages** — *deferred*: needs a caps-query verb
  (the client can't know who holds what without one).

Phase 7 is complete except the two multi-party/server-verb items above.

## Phase 8 — account & platform polish 🟡 core done

- [x] **Logout / switch account** — `disconnect` command drops the connection; a
  log-out item in the status menu resets session state → connect screen.
- [x] **Persistence** — remembers the last host + account in `localStorage`
  (never the password) and prefills the connect form on launch.
- [x] **Auto-reconnect** — an unexpected drop keeps the UI up, shows a
  "reconnecting…" banner, and retries with exponential backoff (login mode,
  in-memory creds); `AUTH-FAILED` aborts to the connect screen.
- [x] **Toasts** — errors now surface as top-right toasts (auto-dismiss) instead
  of polluting the chat pane.
- [x] **Desktop notifications** — `tauri-plugin-notification`; a DM or a
  `@mention` while the window is unfocused fires an OS notification.
- [x] **Cross-launch auto-login** — full creds saved to `localStorage`
  (password included — dev convenience, keychain is the hardening) and
  auto-connected on launch in login mode.
- [x] **Settings modal** — account identity, connection, and a log-out, opened
  from the status menu.
- [x] **Member profile card** — clicking a member (roster) or a message author
  opens a profile popout: status, id, Message / Roles / Mute / Ban / Copy ID.
- [x] **Multi-line messages** (fixed) — bodies with newlines were silently
  dropped (`BadTrailing`); the `Line` trailing now escapes `\r`/`\n`/`\\`
  symmetrically (spec §4 amendment), so multi-line sends work end-to-end for
  every client. Send failures now surface as toasts instead of eating the input.
- [ ] **Deferred** — device keys (`AUTH KEY/ENROLL`), OS-keychain credential
  storage (harden auto-login), quick switcher (Ctrl+K), right-click context
  menus, light/dark theme toggle.

---

## Server-side prerequisites (schedule before the client phase that needs them)

- ~~**`MEMBERS <#chan>` verb**~~ ✅ shipped — roster snapshot as a `MEMBER … join` batch.
- **`PIN`/`UNPIN` verb** — the `pin` cap exists but no verb; blocks pinned messages.
- **Media** (`STREAM`, BLAKE3) and **voice** (WEFT-RT) — M6, block attachments / voice.
- **Search** — no verb; needs a HISTORY scan or a new command.

## Suggested first cut

Phases **0 → 1 → 2 → 3** make it a real chat client (history + edit/delete +
reactions). Then **Phase 6** (multi-server + join/discover dialog) for the
Discord server experience. Phases 4–5 and 7–8 layer on from there.
