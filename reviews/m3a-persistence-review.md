# M3a Review — persistence (memory path): weft-store, mutations, HISTORY

*Self-review of the first half of M3 (memory path; PostgreSQL/purge/
compaction/MARK/DMs = M3b). Status at time of review: 134 tests green
workspace-wide (57 proto, 14 crypto, 13 store, 32 core, 11 conformance,
7 tui), clippy `-D warnings` clean, live-verified against the real binary
over a raw WebSocket.*

## Scope reviewed

| Piece | Contents |
|---|---|
| `weft-proto` | EDIT / DELETE / REACT / UNREACT / HISTORY commands; EDITED / DELETED / REACTION / REACTIONS / BATCH START·END events; MESSAGE batch tags (`edited=`, `edited-at=`); emoji ≤32 B validation; HISTORY `key=value` params |
| `weft-crypto` | argon2id PHC-string password hashes (replaces SHA-256 before anything persists — the M2 review's precondition) |
| `weft-store` (new, L1) | `EventStore`/`AccountStore` traits (async-trait, dyn-usable), `Scope`/`EventRecord`/`EventKind`, MemoryStore with purge watermark, **`materialize()`** — the §12.1 pure function |
| `weft-core` | Store-backed `Accounts`; per-channel retention from config; actor persists events (ephemeral skips) and mints an own msgid per edit/delete/react (§9.3); session-side validation for mutations; HISTORY → materialized BATCH |
| `weftd` | `channels = ["#x", { name, policy }]` config (default `retained:90d`, §6.3); memory store wiring; e2ee policies rejected until M6 |

## The load-bearing design decisions

1. **Materialization is one shared pure function** (`weft-store::materialize`),
   not per-backend SQL. Both backends fetch raw rows; the §12.1 semantics —
   edit collapse, reaction cancellation, tombstone-wins — live in one place
   with 8 dedicated unit tests. The Postgres backend (M3b) cannot get
   invariant 10 wrong by construction, because it never implements it.
2. **Mutation validation is session-side, minting is actor-side.** The session
   does `find_root` → origin check (§11.4 FORBIDDEN off-origin) → tombstone
   check (NO-SUCH-TARGET, §2.2) → membership → authorship, then fires a
   pre-validated command; the actor only mints the event msgid, appends, and
   broadcasts. Sound because one mpsc into one actor preserves a session's
   own command order (a session cannot outrun its own PART). Cross-session
   races (concurrent DELETE vs EDIT) are tolerated by materialization —
   Delete wins regardless of arrival order.
3. **One pending-label FIFO covers all four echo types** (MESSAGE, EDITED,
   DELETED, REACTION). Same ordering argument; a rejected command never
   pushes a label, so the FIFO can't desync on errors.
4. **Honest `truncated` needs a purge watermark**, not guesswork: the store
   records the cutoff of the last purge; HISTORY sets `truncated` when the
   page ran dry while the window's older edge reaches into the purged region.
   Ephemeral channels serve an empty batch with `truncated` — §6.4's
   "silence about gaps is forbidden" made mechanical.
5. **Messages expire as a unit**: purge drops root + children (tombstones
   included) by the *root's* age — "tombstones persist in retained/
   permanent" read as: tombstones live exactly as long as the message would
   have, no shorter (deletion is not early purge), no longer.

## What the round-trip discipline caught

`:shortcode:` emoji (§6.4) **cannot travel as a middle param** — a leading
`:` is the §4 trailing marker, so `REACT <msgid> :ferris:` mangles on the
wire. Custom emoji is already spec open question #8, whose decisions belong
to the spec owner; the codec now sends shortcodes bare, rejects
leading-colon emoji, and the conflict is recorded in §18 #8 rather than
solved unilaterally.

## Spec amendments (Appendix A)

HISTORY `key=value` param syntax pinned; shortcode/grammar conflict noted;
EDITED/DELETED/REACTION/REACTIONS targets widened to `<#chan|@user>` ahead
of DMs (M3b); "every batch line echoes the label" documented (data-page
reading of §3.5).

## Accepted limitations (M3b or noted)

- **Persistence failure degrades to relay-only**: if `append` errors, the
  actor logs and still broadcasts — live members see the message, HISTORY
  won't. The alternative (drop + no echo → client retries into the same
  failure) starves delivery to punish storage. Revisit when Postgres brings
  real failure modes worth surfacing.
- **`limit` is clamped, not errored** (spec says `≤500`); default page 100.
- **No purge/compaction tasks yet** — the store-side purge (+watermark) is
  implemented and tested, but nothing schedules it until M3b. `retained:`
  policies currently behave like `permanent` between boots (memory store
  forgets on restart anyway).
- **Thread filter** parses (`thread=`) but answers UNSUPPORTED (M6 per
  milestone list).
- **argon2 work in the session task**: ~10–50 ms per hash inline on a tokio
  worker. Fine at dev scale; `spawn_blocking` is the fix if auth throughput
  ever matters.
- Batch items are sent inline by the session (no interleaving is possible —
  single task), so no per-item batch tag is needed; revisit if batches ever
  stream asynchronously.

## Verification

- 33 new tests across the stack: proto round-trips for every new wire form
  (including flag-tag `BATCH END` canonical form), materialization suite
  (edit collapse, cancellation, idempotent re-adds, actor cap, tombstone
  supremacy, structural-meta survival), store paging/purge/watermark, core
  end-to-end flows (edit echo + broadcast, author-only enforcement, foreign
  origin FORBIDDEN, tombstone NO-SUCH-TARGET, live reactions, compacted
  HISTORY batches, cursor paging, ephemeral truncation, membership gating),
  black-box QUIC lifecycle test with per-channel policies from config.
- Live smoke on the real binary (raw WS): POLICY announces `retained:90d`,
  EDITED carries `edit-of=`, REACTION `op=add`, and HISTORY returns
  `@edited=1;edited-at=…` MESSAGE + REACTIONS summary + `@compacted BATCH
  END`, every line labeled `h1`.

## Next step (M3b)

PostgreSQL backend matching MemoryStore semantics exactly (it is the
reference implementation), migrations, purge + compaction tasks, MARK sync,
DMs via an account router. Docker is up and a disposable Postgres container
is the test target; integration tests gate on `WEFT_TEST_DATABASE_URL`.
