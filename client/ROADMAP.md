# WEFT client — feature roadmap (Discord-oriented)

Gap analysis of the Tauri/SvelteKit client against a Discord-class experience,
mapped to WEFT protocol verbs. `[ ]` = not started · `[~]` = partial.

## Where it is today (implemented)

- **Connect**: login / register (host, account, password), keepalive, auth-error surfacing.
- **One network** in the community rail (single connection).
- **Channels**: live list grouped by *retention policy*; join a `#channel` or a namespace (`NS JOIN`); manual join box.
- **Messages**: live `MESSAGE` render, own-echo, day separator, composer (Enter/Shift+Enter), `EDITED` shown as a new line, `MODERATED` as a system line.
- **Members**: flat list from observed joins; local/federated origin dot; mute/ban buttons.
- **Slash commands**: `/ban /kick /mute /unmute /join /part /help`.
- **Backend commands**: `connect, join, ns_join, send_message, send_raw`.
- **Events wired**: connected, auth-failed, closed, policy, member, message, edited, moderated, error. (`deleted`, `reaction`/`reactions`, `pong`→raw are **not** handled.)

---

## Tier 1 — core chat parity (makes it a usable Discord-like client)

- [ ] **Message history / scrollback** — `HISTORY` + `BATCH`. Load on channel open; infinite-scroll older. *Biggest gap: today only live messages exist.* Use ULID-derived timestamps (arrival time is wrong for history).
- [ ] **Delete** — `DELETE` verb + **handle the `deleted` event** (currently ignored, so deleted messages linger). Hover action + tombstone render.
- [ ] **Edit own message** — `EDIT`; up-arrow / hover pencil; render `edited ×N` indicator (proto already carries it).
- [ ] **Reactions** — `REACT`/`UNREACT` + render `REACTION`/`REACTIONS` (currently fall into `raw`). Emoji picker; reaction pills (design CSS exists).
- [ ] **Replies** — `reply-to=` tag; quoted-snippet render (design `.reply-thread` markup exists).
- [ ] **Typing indicators** — `TYPING start/stop` on input; render "X is typing…" from `TYPING` events.
- [ ] **Markdown rendering** — `fmt=md`; bold/italic/code/links. Discord-style.
- [ ] **Message grouping** — collapse consecutive messages from one author; hover toolbar.
- [ ] **Direct messages** — `MSG @user`; DM list, DM conversations, open-DM from a member; the "home" rail button is currently inert.
- [ ] **Presence** — set own status (`PRESENCE online/away/dnd/invisible`) + render others' status dots from `PRESENCE` events.

## Tier 2 — servers, channels, membership

- [ ] **Multiple namespaces/networks** in the community rail — connect to several, switch between them (currently one tile only). Discord's server list.
- [ ] **Join dialog** — a proper "Add a server" modal: `DISCOVER` public namespaces, join by name, create. (Replaces the bare join box.)
- [ ] **Namespace channel layout** — `CHANNELS <ns>` → `CHANNEL-LAYOUT` (categories + position). Group channels by **category** (Discord), not retention.
- [ ] **Unread / read state** — `MARK`/`MARKED`; bold unread channels, unread divider, mention badges. (Design has static unread pills.)
- [ ] **Channel management** — `CHANNEL CREATE / META (topic|view-gated|posting|category|position) / POLICY / DELETE`; right-click + settings. Show channel **topic** in the topbar (currently empty).
- [ ] **Leave channel** — `PART` button/right-click (only `/part` today).
- [ ] **Member roster** — full list (needs `MEMBERS` verb, see Protocol gaps); currently only observed joins. Group by role/capability (design: Root / Delegated / Members).
- [ ] **User profile popover** — click a member → key, roles, DM button.
- [ ] **Display names / avatars** — `display=` on `MEMBER`; render display identity, not just account.

## Tier 3 — roles, moderation, admin

- [ ] **Roles & permissions** — `GRANT`/`REVOKE` UI (Discord roles). Capability badges on messages (owner/mod/bridged — design exists, needs cap data).
- [ ] **Invites** — `INVITE MINT/REVOKE/REDEEM`; invite links, redeem-on-join.
- [ ] **Reporting** — `REPORT` (right-click a message); mod queue `REPORTS LIST/RESOLVE`.
- [ ] **Moderation polish** — reason dialog, ban/mute list view, unban/unmute UI (beyond slash), restricted-posting toggle (`CHANNEL META posting`).
- [ ] **Namespace settings** — `NS META/VISIBILITY/DELEGATE/DELETE`; server-settings panel.
- [ ] **Create namespace** — `NS CREATE` with **client-side Ed25519 root keypair** generation (backend crypto) + submit pubkey.

## Tier 4 — identity & account

- [ ] **Logout / disconnect / switch account** — the gear icon is inert.
- [ ] **Account settings** — password, **device keys** (`AUTH KEY/PROOF/ENROLL`), show attestation/key (design shows an `ed25519:…` line).
- [ ] **Multiple accounts** across networks.

## Tier 5 — platform & polish (Tauri)

- [ ] **Desktop notifications** — mentions/DMs (Tauri notification plugin).
- [ ] **Reconnect** — auto-reconnect with a status banner (today it drops to the connect screen).
- [ ] **Persistence** — remember servers/accounts, auto-connect on launch (Tauri store).
- [ ] **Search** — message search (needs a server verb or HISTORY scan).
- [ ] **Quick switcher** (Ctrl+K), keyboard shortcuts.
- [ ] **Context menus** — message / channel / member / server right-click.
- [ ] **Toasts** — proper error surfacing (errors currently land as system lines in the chat pane).
- [ ] **Settings panel**, theme, window/tray polish, link previews, loading/empty states.

## Federation surfacing (WEFT-specific, no Discord analog)

- [ ] Trust marks (signed / bridged) on the community rail from real manifest state (design has them).
- [ ] Bridged-message badges; bridged-member origin.
- [ ] Operator tools: `BRIDGE PROPOSE/ACCEPT/…`, `NETBLOCK` admin panel.

---

## Protocol gaps (need server/proto work *before* the client can use them)

- [ ] **`MEMBERS`** full-roster query — mentioned in spec §6.3 but **not a wire command** yet. Needed for a real member list.
- [ ] **`PIN`** — the `pin` capability exists but there is **no `PIN` verb**. Needed for pinned messages.
- [ ] **Media / attachments** — M6 (BLAKE3, `STREAM`), deferred. Blocks image/file messages.
- [ ] **Voice channels** — M6 (WEFT-RT), deferred.
- [ ] **Message search backend** — no verb; would need HISTORY scan or a new search command.
