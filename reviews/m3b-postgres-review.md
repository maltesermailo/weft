# M3b Review — durable persistence: PostgreSQL, DMs, MARK sync, maintenance

*Self-review of the second half of M3. Status at time of review: 146 tests
green workspace-wide, clippy `-D warnings` clean, the shared backend
contract green against **live PostgreSQL** (Docker), and the restart demo
verified end-to-end: channels, accounts, channel history, and DMs all
survive a server restart with `channels = []` in config.*

## Scope reviewed

| Piece | Contents |
|---|---|
| `weft-store` | `ChannelStore` trait (upsert/list — channels live in the store), marks + verification claims on `AccountStore`, `compact_before`/`purge_dms_before`, **`compaction_plan`** (§12.1 audit-window semantics as one pure fn), `postgres.rs` behind the `postgres` feature + sqlx migrations |
| `weft-core` | **directory actor** (account→sessions: DM delivery §9.5, MARK sync §6.3), session 4th select arm + `pending_direct` FIFO, DM branches in MSG/EDIT/DELETE/REACT/HISTORY, MARK + §9.7 MARKED snapshot after auth, `spawn_maintenance` (purge per policy + compaction) |
| `weftd` | `[storage] backend = memory\|postgres` + URL + maintenance intervals, `dm_policy`, **seed-then-load boot**: config channels upserted into the store, registry built from `list_channels()` |

## The load-bearing design decisions

1. **One contract suite, two backends.** `tests/backends.rs` runs the
   identical assertions against MemoryStore (always) and PgStore (gated on
   `WEFT_TEST_DATABASE_URL`). The Postgres module contains **no semantics**
   — materialization and `compaction_plan` are the same pure functions the
   memory backend uses; sqlx only moves rows. The PG backend passed the
   contract on its first run, which is the payoff of that structure.
2. **Channels load from the store; config is seed data.** Boot upserts the
   config channels, then builds the registry from `list_channels()`. On
   Postgres, channels created by earlier boots (and, come M4, by
   `CHANNEL CREATE`) exist without config. Demonstrated live: boot 2 with
   `channels = []` loaded 4 channels from the database.
3. **Audit-window compaction is subtle and therefore pure.** A superseded
   edit is droppable only when its *successor* left the window ("what did
   it say before" stays answerable for `compact-after` after it changed);
   a deleted family collapses to its tombstone only when the *delete* aged
   out; reaction prefixes older than the cutoff net to one add or nothing.
   Six dedicated tests; the contract suite additionally proves
   post-compaction storage materializes to the same wire form.
4. **The directory actor mirrors the channel-actor discipline**: single
   writer per concern, one monotonic ULID generator (global order ⊇ the
   §9.1 per-pair requirement), same origin/label echo rule, a separate
   `pending_direct` FIFO with the same one-mpsc ordering argument.
   Recipient existence is checked inside the actor and surfaces as plain
   NO-SUCH-TARGET (§2.2).
5. **ULIDs as text in Postgres**: 26-char Crockford sorts lexicographically
   in time order, so `ulid < $cursor` in SQL *is* msgid paging — no
   custom comparators, no binary keys.

## Verification infrastructure (owner request, scoped deliberately)

`weft_store::Verification` — claims (`kind` = "email"/"age"/…, `subject`,
`verified_at: Option<u64>`) with a claim → confirm lifecycle, re-claiming
resets to pending; memory + Postgres impls + contract coverage. **No wire
surface**: how a user proves a claim (REGISTER email param? VERIFY verb?
operator panel?) changes §6.1 and is an owner spec decision (§18
territory) — flagged in CLAUDE.md's parked list, not invented here.

## Accepted limitations (documented in code)

- **Directory delivery is `try_send`**: a session drowning in direct
  events loses some (it can HISTORY-resync) rather than one slow client
  stalling every DM on the network — the directory is global, unlike
  channel actors. Logged, never silent.
- **DM MESSAGE `target` = the addressed `@user`** on every copy (echo
  semantics, §9.2); clients derive the conversation peer as
  `sender == me ? target : sender`. DM HISTORY items use `@peer` from the
  requester's perspective.
- **`compact_before` loads a scope at a time** into memory to run the
  plan. Fine at current scale; page per root family when it isn't.
- **Maintenance runs against the boot-time channel list** — a channel
  created at runtime (M4) joins the purge rotation at next restart until
  the registry becomes dynamic.
- **Offline DM recipients** get no push; the message is stored and arrives
  via `HISTORY @peer` on reconnect (§9.7 flow) — matches the spec's
  reconnect story, no offline queue invented.
- The shared test database accumulates tagged suite scopes; the contract
  uses per-run tags and floor assertions on global counters accordingly.

## Verification

- 12 new tests: compaction plan (6), backend contract suite (paging,
  purge/watermark, DM purge, compaction round-trip, accounts, marks,
  verifications, channels — ×2 backends), DM/MARK core tests (multi-device
  fan-out, mutations + outsider anti-enumeration, DM history, MARK sync +
  login snapshot), DM/MARK conformance over real QUIC.
- Live: Postgres in Docker; boot 1 seeded channels + wrote channel message
  and DM; boot 2 with **no channels in config** loaded the registry from
  the store (`channels=4 backend=Postgres`), authenticated the persisted
  account, announced `retained:7d` from the DB, and served both messages
  with their original msgids.

## Next step

M4 — capabilities. The seams are ready: `ChannelStore::upsert_channel` for
CHANNEL CREATE, the directory for invite/token delivery, verification
claims for whatever REGISTER gating the spec decides on.
