# Resume — polish + deferred + server verbs batch (paused 2026-07-06)

Continuation of the "do all polish + deferred client stuff ≤ M5 + needed server
verbs" batch. See `PLAN.md` / `ROADMAP.md` for the phase context.

## ⚠️ Build state (read first)

- **weft-proto** ✅ compiles, tests green (80).
- **weft-store** ✅ compiles, contract green (mem + live PG, migration 0010).
- **weft-core** ❌ **does NOT compile right now** — mid-edit. Two reasons:
  1. New `Command::Pin/Unpin/Pins` variants aren't handled in the exhaustive
     `on_ready` dispatch match yet (no `_` arm → compile error).
  2. `ServerCtx` got a new `pins: Arc<dyn PinStore>` field (`context.rs`) that
     isn't fully wired (missing `+ PinStore` bound, the `store.clone()` line,
     the struct-literal `pins,`, and the `use weft_store::PinStore` import).
- **weftd / client** untouched for PIN — will need the `+ PinStore` boot bound
  and the client wiring once core is done.

**First thing tomorrow: finish PIN in weft-core (steps below) to get green.**

## User decisions (locked)

Round 1 — extra scope: **device-key auth (AUTH KEY/ENROLL)** + **NS RECOVER
quorum UI** (NOT multi-network flavor-B). Pins response = **full message
content**. Emoji picker = **curated ~200**. Theme = **add light theme + toggle**.

Round 2: caps visibility = **public (any member)**. Badges = **Owner, Moderator,
Bridged, + Role names** (⚠ role *names* aren't in the server data model yet —
flag/decide when building badges: derive a synthetic label from caps, or add
role-name infra server-side). Device keys = **opt-in (enroll in settings, then
offer key login)**. Recovery keys = **client generates a per-namespace recovery
key to share the pubkey**.

## Task list (TaskCreate #24–28)

- #24 Server PIN/UNPIN/PINS — **in progress** (proto ✅, store ✅, core 🚧).
- #25 Server CAPS query verb — pending.
- #26 Server presence store — pending (in-memory only; spec §6.1 "never stored"
  means don't persist — track live presence in the directory/registry and
  include it with the MEMBERS response).
- #27 Client pins panel + capability badges — pending.
- #28 Client polish: emoji picker, Ctrl+K, context menus, theme — pending.
- Plus (from round-1): **device-key auth** (client + `keys.rs`-style storage;
  server AUTH KEY/PROOF/ENROLL already exists from M2) and **NS RECOVER quorum
  UI** (client recovery-keypair gen + co-sign a `RotationRecord` via
  `weft-crypto::rotation`; `SignedRotation`/`RotationRecord` exist).

## PIN verb — what's DONE

- **proto** (`command.rs`, `event.rs`): `Command::Pin{msgid}` / `Unpin{msgid}` /
  `Pins{channel}`; `Event::Pinned{channel,msgid,by}` / `Unpinned{channel,msgid}`.
  Parse + serialize + round-trip tests. ✅
- **store**: `PinStore` trait (`traits.rs`) — `set_pin(channel,msgid,pinned)`,
  `pins(channel) -> Vec<MsgId>` (oldest-first by ULID). Exported in `lib.rs`.
  Memory impl (`memory.rs`: `pins: HashMap<ChannelName, BTreeMap<Ulid,MsgId>>`).
  PG impl (`postgres.rs`) + migration `0010_pins.sql`. Contract test in
  `backends.rs`. ✅ mem + live-PG green.

## PIN verb — REMAINING (do these to get core green)

1. **`context.rs`**: `use weft_store::PinStore;`; add `+ PinStore` to the
   `ServerCtx::new` where-bound (next to `+ ModerationStore`); add
   `let pins: Arc<dyn PinStore> = store.clone();` and `pins,` in the struct
   literal. (The `pins` field is already declared on the struct.)
2. **`session.rs`**: add handlers + dispatch arms:
   - `Command::Pin{msgid}` → `on_pin`: cap `Capability::Pin` at the channel
     scope (the msgid's channel — find via `self.joined` membership or the
     store); `ctx.pins.set_pin(chan, &msgid, true)`; broadcast `Event::Pinned`
     to the channel (use the channel actor announce, SENTINEL origin) + labeled
     ack to the actor. Verify the msgid is a message in that channel first
     (or accept leniently).
   - `Command::Unpin{msgid}` → `on_unpin`: same cap, `set_pin(false)`, broadcast
     `Event::Unpinned`.
   - `Command::Pins{channel}` → `on_pins`: membership-gated (mirror `on_members`
     — `not_member_cap "view"`); `ctx.pins.pins(&channel)`; for each msgid
     `event_store.find_root(msgid.ulid())` → build a `MESSAGE`; frame as
     `BATCH START … MESSAGE … BATCH END` (reuse the `self.batches` counter like
     `on_members`). Skip msgids that return `None` (purged).
   - Note: `on_pin`/`on_unpin` need the channel of the msgid. `PIN <msgid>` has
     no channel arg — resolve it: check each `self.joined` channel, or
     `find_root` to get the scope. Simplest: `find_root(ulid)` → `EventRecord`
     has `scope` → the channel. Cap-check on that channel.
3. **Dispatch**: add the three `Command::Pin/Unpin/Pins` arms in `on_ready`
   (near `Command::Members`).
4. **`weftd/src/lib.rs`**: add `+ weft_store::PinStore` to the `boot` where-bound.
5. **Tests** (`weft-core/tests/session.rs`): pin a message (cap-gated), PINS
   returns it as a MESSAGE batch, unpin removes it, non-member → CAP-REQUIRED.
6. **spec**: add `PIN`/`UNPIN`/`PINS` to §6.4 table + an Appendix A amendment
   (response = `BATCH` of `MESSAGE`; `PINNED`/`UNPINNED` events; cap `pin`).

## Then the rest of the batch (order)

1. Finish PIN client: `pin`/`unpin` commands + a **pins panel** (topbar 📌 →
   `PINS` → list; PINNED/UNPINNED update live; a "Pin/Unpin" msg-action).
2. **CAPS query** (#25): `Command::Caps{account, scope?}` → a `CAPS` event
   listing the account's caps; core handler uses `account_has_cap`/the grant
   store; public (any member). Client: derive **badges** (owner/mod/bridged +
   role-name decision) on messages/profile.
3. **Presence store** (#26): in-memory live presence in the directory; send
   PRESENCE for each member in the `on_members` batch so roster dots are right.
4. **Client polish** (#28): curated emoji picker (~200, categories); Ctrl+K
   quick switcher (channels + DMs); right-click context menus (message/channel/
   member); light theme + toggle (add a `:root[data-theme=light]` palette in
   `app.css`, persist choice, toggle in Settings).
5. **Device-key auth**: `keys.rs`-style device keypair + `AUTH ENROLL` from
   Settings; connect screen offers passwordless key login (AUTH KEY/PROOF).
6. **NS RECOVER quorum UI**: client recovery-keypair gen (share pubkey) +
   co-sign a `RotationRecord` and submit `NS RECOVER`.

## Verify commands

```
cargo test -p weft-proto -p weft-store            # green now
cargo test -p weft-core                            # will pass once PIN core done
WEFT_TEST_DATABASE_URL="postgres://postgres:weft@127.0.0.1:15432/postgres" \
  cargo test -p weft-store --features postgres
cd client && pnpm check && cargo build -p client
```
