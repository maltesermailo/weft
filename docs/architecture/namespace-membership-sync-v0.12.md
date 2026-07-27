# Namespace membership + SYNC (v0.11 → v0.12) — change plan

> **Status:** COMPLETE. The four open questions are resolved (see Decision log) and
> Tasks 14–21 have shipped — proto + store + membership + SYNC + BATCH cleanup + client
> + spec, all tested and PG-16-validated; the §-edits are folded into
> `docs/protocol/weft-spec-v0.11.adoc`. Source: owner change spec, 2026-07-25. Only
> milestone-scale follow-ups remain (body stream + previews, full `ModSeq` in-flight
> wiring, session auto-subscribe for zero-join posting, full client cache model) — each
> is functional-gap-free today and tracked in the Progress log below.

## Decision log

- ✅ **Roster query stays per-channel `MEMBERS <#ns/chan>`, server-derived.**
  The server returns the already-derived set `{ m : member(m, ns) ∧ ¬view_gated_denied(chan, m) ∧ ¬hidden(m, chan) }`.
  The client renders the response as-is — **no client-side filtering** (it must
  never be told who lacks `view` on a gated channel; that is itself the invariant-1
  leak). `NS MEMBERS <ns>` is an optional, cacheable **non-gated fast-path**, deferred
  (gated channels always fall back to per-channel `MEMBERS`). The member-list panel
  re-requests on channel switch, as it already does. Folds into §6.2 / Part 1.3.
- ✅ **Q1 → new `NS-MEMBER` event.** On ns join, emit one `NS-MEMBER <ns> <account> join`
  (add to §7.4) that clients expand into the derived roster — O(1) per join, not O(visible
  channels). Per-channel `MEMBER` is kept for **top-level** channels. `part` side too.
- ✅ **Q2 → `NS LEAVE <ns>` verb + `PART ns:<name>` alias.** Canonical verb mirrors `NS JOIN`;
  the alias routes for the IRC gateway. Both drop the membership row + hide overrides +
  ns-scoped role assignments and fan out `NS-MEMBER … part`.
- ✅ **Q3 → 30-day delta horizon** (config knob; effective = `max(retention window, 30 d)`).
- ✅ **Q4 → `preview=0` pushes the non-preview per-account rows unchanged** (MARKED /
  UNREAD-COUNTS / FRIEND / GROUP / media tokens). `preview=0` suppresses only message previews
  + the CHANSYNC body stream; the skeleton stays complete.
- ✅ **Seq stamping → `BEFORE INSERT/UPDATE` triggers** (DB-assigned from a global sequence;
  centralized, hard to forget on new write paths).
- ✅ **Cursor LWM → build the full min-in-flight tracker now** (not deferred to task 17).
- ✅ **Seq-tracker mechanism → batched app-reserved seqs + in-flight tracker, trigger as
  `COALESCE` fallback.** Resolves the trigger-vs-tracker tension for **high throughput**: you
  can't compute min-in-flight from committed DB state (MVCC hides in-flight rows), so the pure
  DB-snapshot approach isn't robust; serializing commit order caps throughput. Instead the app
  reserves seqs from `nextval` in **batches** (amortized), knows each seq *before* the write,
  registers it in an in-memory in-flight set, does a plain **autocommit** write passing the seq
  (no per-write explicit txn, no write serialization), and deregisters on completion.
  `cursor = min(in-flight) − 1`, else `max(completed)`. A `BEFORE INSERT/UPDATE` trigger stamps
  `COALESCE(NEW.seq, nextval)` as a safety net so no write path can go unstamped. Reserved-but-
  unused seqs on crash are harmless gaps (bias-stale + epoch cover them). Memory backend: same
  interface, trivially correct (writes complete synchronously under one lock — no in-flight
  window). **Stamping is incremental**: event log in task 15, metadata tables in task 17 as SYNC
  consumes them.

## Progress

- ✅ **Task 14 (proto)** — SYNC verb, NS LEAVE + `PART ns:` alias, NS-MEMBER / CHANSYNC /
  SYNC START|BODY|END events. Round-trip tested; workspace green; clippy+fmt clean.
- ✅ **Task 15 (store)** — DONE & PG-validated.
  - *Membership*: migration `0035` (`weft_ns_membership` + `weft_channel_hide` + backfill +
    drop namespaced per-channel rows), `MembershipStore` extended (ns membership + hide
    overrides + derived-roster helpers), mem + PG impls, dual-backend contract test. Backfill
    correctness simulated on real PG 16 (acceptance #4 ✓).
  - *Seq/modseq*: migration `0036` (global `weft_seq` + `weft_sync_epoch` + reusable
    `weft_stamp_seq` `COALESCE` trigger + seq on the event log), `ModSeq` allocator + in-flight
    tracker with unit tests incl. acceptance #8 (out-of-order commit). Trigger auto-stamp +
    app-`COALESCE` override both verified on PG 16.
  - *Deferred to task 17* (tied to consumption): wire `ModSeq` reserve/complete into write
    paths, stamp metadata tables, `WHERE seq > since` delta queries, epoch boot sanity check.
- ✅ **Task 16 (core membership)** — derivation (`is_member`/`derived_channels`/`channel_roster`),
  verb rewiring (`NS JOIN` = one ns row + subscribe visible + `NS-MEMBER`; `JOIN #ns/chan`,
  `PART #ns/chan` = hide; `NS LEAVE`; `INVITE REDEEM` ns = ns row + subscribe), derived `MEMBERS`
  roster, auto-rejoin via `derived_channels`. 171 core tests green (incl. a new hide/leave test);
  full workspace green.
  - **Design reconciliation (deviates from change-doc 1.2's literal text):** `JOIN #ns/chan` by
    a non-member does **not** error — it **auto-joins the namespace** then subscribes to that
    channel. This keeps `JOIN #ns/chan` natively valid for the IRC gateway (§17) and matches the
    existing test suite; a private/view-gated channel still errors (anti-enum intact), so a
    private namespace can't be auto-joined. *Owner: flag if you want strict-error instead.*
  - **Deferred:** (a) `NS-MEMBER` currently goes to the **acting** client as its ns-level ack;
    **other** online members still get live roster updates via the channel actors' existing
    per-channel `MEMBER` broadcasts (correct, but not the single-event fan-out Q1 envisioned —
    a true ns-wide broadcast needs an ns-level pub/sub primitive that doesn't exist yet).
    (b) Live `CHANNEL-LAYOUT`+`POLICY` push to ns members on `CHANNEL CREATE`, and "post to a
    new channel with zero joins" (acceptance #1) — both ride task 17's seq-stamped delta.
- ◑ **Task 17 (SYNC) — v1 done & PG-validated.** Store: `EventStore::sync_cursor()` (`epoch:seq`,
  epoch loaded at connect with the Part 2.4 boot sanity check) + `events_since(scopes, since)`;
  memory backend stamps a per-event seq; both backends' new methods PG-16 validated. Core:
  `session/sync.rs` — `SYNC` (no cursor) emits the inline **skeleton** (NS-META + CHANNEL-LAYOUT
  + POLICY + MARKED + UNREAD-COUNTS per visible channel, top-level channels) + `@cursor SYNC END`;
  `SYNC since=` serves the **materialized delta** of channel messages + mutations (`seq > cursor`,
  epoch-gated → stale epoch falls back to fresh). Tests: fresh-skeleton→delta catch-up, and
  **acceptance #5** (offline edit of an old message caught by the delta — the ULID-paging gap).
  173 core tests green.
  - **Deferred (task 17 follow-ups):** the data-plane **body stream** with real previews
    (currently withheld — legal; client uses HISTORY); the **`reset`** flag for newly-visible
    channels; **metadata-row deltas** (channel/ns meta, roles, friends/groups changes — needs
    seq stamping on those tables, migration 0037); **DM scopes** in the delta; and the full
    in-flight-tracker wiring (cursor is currently committed-max — correct under weftd's
    per-channel-serialized writes, `ModSeq` struct built + tested and ready to wire).
- ✅ **Task 18 (HISTORY materialized / drop `compacted`)** — removed the `compacted` flag from
  `BatchEnd` everywhere (proto + ~11 core emit sites + weft-tui + tests + conformance); a legacy
  `@compacted` tag is tolerated on input. HISTORY was already always-materialized and `EDITED` is
  already live-only, so those needed no change. BATCH framing kept for the request/response pages
  (Phase 1). Full workspace green. *Deferred (coupled to the client gap rule): making `truncated`
  federated-backfill-only.*
- ✅ **Task 19 (client SYNC adoption)** — v1. `weft-client-core`: `SyncEnd`/`ChanSync`/`NsMember`
  ClientEvents mapped + unit-tested (SYNC START/BODY are silent). Client (`weft.ts` + `+page.svelte`):
  a `sync(cursor?)` helper; on `connected` the client sends `SYNC` (fresh) or `SYNC since=<stored
  cursor>` (reconnect); `sync-end` stores the cursor per account+device in localStorage; the delta's
  materialized rows flow through the existing message/edit/reaction handlers, now with **upsert by
  msgid** (an offline edit re-delivered as a materialized MESSAGE replaces the stale copy in place,
  preserving accumulated reactions) instead of skip-on-duplicate. svelte-check clean (406 files);
  full workspace green. SYNC is **additive** — the server's auth auto-rejoin stays for TUI/IRC/legacy.
  - **Deferred (task 21):** the full client cache model (per-channel contiguity ranges + CHANSYNC
    headers + eviction) rides the body-stream work; `reset`/`chan-sync` handling is stubbed; the
    server-side "move subscription from auth into SYNC + drop the auth push" is a later clean-up.
- ✅ **Task 20 (spec sync)** — folded v0.12 into `docs/protocol/weft-spec-v0.11.adoc`: §1 table
  (Membership + Sync rows, History → materialized); §6.2 `NS JOIN`/`NS LEAVE`; §6.3 `JOIN`/`PART`/
  `MEMBERS` + the membership-durable paragraph; §6.4 HISTORY materialized; **new §6.9
  Synchronization** (SYNC verb, opaque `epoch:seq` cursor, apply rule, previews, delta horizon);
  §7.3 `NS-MEMBER`; §7.9 `CHANSYNC`/`SYNC START|BODY|END` + `compacted` flag removed; §9.0 invariant
  1 sharpened (inside-boundary) + reserved slot 10 → "sync is upsert"; §9.7 reconnect rewritten to
  the SYNC flow; §17 BATCH federated form; Appendix A v0.12 milestone entry. Renders clean
  (asciidoctor), all xrefs resolve, **speclint 267/267** wire examples round-trip through the codec.
  *Minor polish deferred (task 21): §2.1 entities prose, §7.2 REACTIONS re-description, §12.1
  wire-form paragraph tidy, §18 Phase-2 BATCH note.*
- ◑ **Task 21 (refinements) — in progress.** Done so far:
  - **DM scopes in the SYNC delta** (#4): `on_sync_delta` now scans the account's DM conversations
    (via `dm_partners`) alongside its derived channels, mapping each `Scope::dm` back to its
    `@peer` target — a reconnect catches up 1:1 messages, not just channels. Tested.
  - **Live CHANNEL CREATE push** (#6, acceptance #1 partial): creating a namespaced channel now
    pushes `CHANNEL-LAYOUT` + `POLICY` to every online ns member via `directory.notify`, so it
    appears in their sidebar with no reconnect. Tested. *Still needed for full "post with zero
    joins": the member's session must auto-subscribe to the new channel actor (session-task-local
    state — needs a small subscribe-on-notify mechanism); until then posting needs one JOIN/SYNC.*
  - **Channel-metadata delta** (#3): migration `0037` (seq + reused stamp trigger on
    `weft_channels`), memory `stamp_channel` at every mutator (contract parity with the PG trigger),
    `channels_changed_since` on both backends, `sync_cursor` = `GREATEST` over the stamped tables,
    `on_sync_delta` emits `CHANNEL-LAYOUT`+`POLICY` for changed visible channels. Tested + PG-16.
  - **Namespace-metadata delta** (#3): migration `0038` (seq + trigger on `weft_namespaces`),
    memory `stamp_namespace` at every NS-META mutator, `namespaces_changed_since` on both backends,
    `sync_cursor`/boot-check extended to namespaces, delta emits `NS-META` for changed ns's the
    account belongs to. Tested + PG-16. (Together: an offline re-category / re-policy / rename /
    server-settings change reaches the sidebar on reconnect.)
  - **Minor spec prose** (#10): §7.2 rows re-described as the materialized (SYNC/HISTORY) form;
    §18 Phase-2 BATCH-retirement note added. Renders clean, speclint 267/267.

  **Resolved as NOT needed (decisions, not deferrals):**
  - *role/friend/group/pin metadata deltas* — the client already re-fetches these on connect
    (`listFriends`/`listGroups`) or on-demand (pins/roles on channel/ns open), so a server-side
    delta would be redundant.
  - *`reset`/newly-visible-channel handling* — the retained auth auto-rejoin re-establishes the full
    derived channel set + `POLICY` on every reconnect, so no channel is missed; `reset` becomes
    relevant only once the auth push is removed (below).

  **Genuinely separate future projects (each milestone-scale, not a quick refinement):**
  - *data-plane body stream + real previews* — no functional gap today (previews are legally
    withheld; the client loads cold channels via `HISTORY`).
  - *full `ModSeq` in-flight-tracker wiring* — the committed-max cursor is correct under weftd's
    per-channel-serialized writes; the struct is built + tested, ready to wire when needed.
  - *session auto-subscribe-on-notify* (the last mile of zero-join posting) — needs the session
    event loop to act on pushed `CHANNEL-LAYOUT`; a deliberate core-loop change.
  - *full client cache model* (contiguity ranges + eviction) and *moving subscription from auth
    into SYNC + dropping the auth push* — larger client/server refactors; the current
    upsert-by-msgid + auth-push path is functional and correct.

---

## 0. Motivation

Pain points this change removes:

1. **Startup is an N+1 loop.** Fresh login = DISCOVER + CHANNELS per ns + HISTORY per
   channel + MARKED + UNREAD, each with its own BATCH envelope, all head-of-line-
   blocking live traffic on QUIC stream 0.
2. **BATCH is event-log framing used for state transfer.** §12.1 already says "live is
   event-sourced, at-rest is materialized" — but HISTORY still serves event pages with
   special-case rules (`compacted` flag, EDITED-chains-except-in-batches).
3. **`HISTORY after=<msgid>` catch-up structurally misses offline mutations.** An
   edit/reaction on a message older than the client's newest msgid is invisible to
   ULID-ordered catch-up, and compaction may have dropped the mutation events entirely.
4. **Per-channel membership can't express Discord semantics.** A channel created after
   you joined its namespace has no members. `NS JOIN` is sugar minting N `(account,
   channel)` rows; new channels require retroactive join-minting hacks.

---

## Part 1 — Namespace-level membership

### 1.1 Data model

Membership inside a namespace is keyed **`(account, namespace)`**. Channel access is
**derived**, never stored per-channel:

```
in_channel(a, #ns/chan) = member(a, ns)
                        AND can_view(a, #ns/chan)     -- existing view-gate caps, unchanged
                        AND NOT hidden(a, #ns/chan)   -- new per-channel opt-out override
```

- `hidden` is a per-account, per-channel override row (`hide_override(account, channel)`).
  It expresses "I left this one channel but stayed in the server."
- **Top-level channels (no namespace) keep the existing per-channel `(account, channel)`
  membership unchanged.** The flat-IRC deployment mode must survive untouched.
- `can_view` is the existing predicate `view_gated_denied(chan, a)` (weft-core
  `session.rs`): true when the channel is `view_gated` and the account lacks the `view`
  cap on the channel scope. Fail-closed, indistinguishable from nonexistent (invariant 1).

### 1.2 Command semantics changes (§6.2, §6.3)

| Verb | Old meaning | New meaning |
|---|---|---|
| `NS JOIN <ns>` | Sugar: mint per-channel joins for every visible channel | **The membership operation.** One `(account, ns)` row. Response unchanged in shape: `MEMBER` + `POLICY` per *visible* channel (clients still learn the channel set), but no per-channel membership rows are written. |
| `JOIN <#ns/chan>` | Become a member of the channel | **Clear the hide override** for a channel in a namespace you're already a member of. If not an ns member → `CAP-REQUIRED` / `NO-SUCH-TARGET` per visibility (invariant 1). Top-level channels: unchanged (real join). |
| `PART <#ns/chan>` | Remove channel membership | **Set the hide override.** Broadcasts `MEMBER … part` to the channel as today (the derived roster shrank). Top-level: unchanged. |
| `NS LEAVE <ns>` *(new verb — Q2)* | — | Drop the `(account, ns)` row + all hide overrides + role assignments scoped under it. Broadcasts `MEMBER … part` on every channel the account was visible in. Also accept `PART ns:<name>` as an alias if that's cheaper to route. |
| `CHANNEL CREATE <#ns/chan>` | Creates channel; nobody is in it | Creates channel; **every ns member is in it immediately by derivation.** Broadcast `CHANNEL-LAYOUT` + `POLICY` to all ns members. **No membership writes.** |
| `CHANNEL DELETE` | Deletes + clears members | Deletes; clears hide overrides for that channel; broadcast as today. No membership rows to clear. |
| `INVITE REDEEM` (ns-scoped) | Mints member token + auto-joins default channel | Mints member token + **creates the ns membership row**; default channel visible by derivation. |

### 1.3 Rosters, counts, broadcasts

- **The real roster is namespace-scoped.** Query surface stays per-channel:
  `MEMBERS <#ns/chan>` returns the *derived* set — ns members with `view`, minus hiders
  (see Decision log). `count=` tags on `MEMBER` events count that derived set.
- Join/part announcement dedup (§6.3) is unchanged in spirit but re-anchored: an
  `NS JOIN` broadcasts one `MEMBER … join` per visible channel — **or (Q1)** a single new
  `NS-MEMBER <ns> <account> join` event that clients expand (if added: §7.4, and keep
  per-channel `MEMBER` for top-level channels).
- `NS-META` gains an optional `members=<n>` tag (distinct-account ns member count).

### 1.4 Anti-enumeration inside the membership boundary (invariant 1, sharpened)

A namespace member **without `view`** on a gated channel must observe *nothing*: no
`CHANNEL-LAYOUT` row in `CHANNELS`/SYNC output, no `POLICY`, `NO-SUCH-TARGET` on direct
probes, absent from that channel's derived roster. Invariant 1 now explicitly applies
*inside* the membership boundary, not just at it. Add this sentence to §9.0 invariant 1.

### 1.5 Federation

- Spoke relays `@as=alice NS JOIN gaming` to the home; the home writes the one membership
  row and fans out derived visibility. No change to §11.11/§11.12 mechanics.
- Auto-federation (§11.10): `FEDERATE peer.example/gaming` now results in **one** membership
  row on the home instead of a per-channel join fan-out. Simplification only.
- Manifests stay channel-listing as today (no change to §11.1) — the manifest gates
  *forwarding*, membership gates *people*. Don't conflate them in this pass.

### 1.6 Migration (weftd)

New migration (next free number after `0031`):

1. Create `ns_membership(account, namespace, joined_at)` and `channel_hide(account, channel)`.
2. Backfill: for every account with ≥1 per-channel membership row inside a namespace →
   insert one `ns_membership` row; for every channel of that ns the account was **not** in
   (and can view) → insert a `channel_hide` row. Net effect: **no one's sidebar changes on
   upgrade day.**
3. Drop the per-channel membership rows for namespaced channels only. Keep the table for
   top-level channels.
4. Auto-rejoin on auth (§6.3) reads ns membership + derivation for namespaced channels.

---

## Part 2 — Server-global modification sequence (modseq)

### 2.1 Stamping rule

Every stored row a client can receive gets a **server-global, monotonically increasing
`seq`**, stamped on every insert **and every update**:

- messages (final materialized form), tombstones, reaction summary rows
- channel meta (`CHANMETA` keys, layout, policy), namespace meta, pins, thread names
- read markers, membership rows, hide overrides, role definitions/assignments, friend/group rows

One counter per server (a Postgres `BIGSERIAL`/sequence is fine). This is IMAP
CONDSTORE/QRESYNC's MODSEQ, applied server-wide.

### 2.2 Cursor semantics — the commit-visibility caveat (Appendix B)

Seq is assigned at insert but visible at commit: a lower seq can become visible *after* a
higher one. Therefore the cursor handed to clients advances only to the **low-water mark of
committed seqs**, never `max(seq)`. Implement as: track in-flight transactions' minimum
assigned seq; `cursor = min(in-flight) - 1`, or `max(committed)` when nothing is in flight.
A slightly-stale cursor is harmless (client re-receives a few rows; upsert semantics absorb
it). A too-fresh cursor loses data. Bias stale.

### 2.3 Federation stamping

A spoke stamps its **own local seq** when it ingests home-origin events into its replica.
Clients only ever sync against their own network (§12 cornerstone), so cursors never cross
networks. No change to bridge wire format.

### 2.4 Cursor opacity + sync epoch (normative)

Wrap is a non-issue: at 10k stamped writes/sec a signed 64-bit sequence lasts ~29 million
years; use `NO CYCLE` so hypothetical exhaustion errors loudly instead of wrapping. The real
risk is the counter going **backward**: a server restored from backup re-issues seqs it
already handed out, and a client with a newer cursor gets a silently empty delta — permanent,
invisible data loss. Two rules fix both at once:

1. **The cursor is opaque on the wire.** ≤64 B token (same convention as `label` and the
   `MORE` cursor). Clients store and echo it verbatim, never parse it. Integer cursors in the
   wire examples are illustrative only. Removes integer width from the protocol entirely.
2. **The token internally encodes `epoch:seq`.** The *sync epoch* is a value persisted with
   the database (random or generational), bumped on any restore-from-backup, storage rebuild,
   or migration that could reuse seq values. A `SYNC since=` whose epoch ≠ current epoch is
   treated exactly as a cursor-less fresh login: full skeleton + resync. This is IMAP's
   UIDVALIDITY, applied server-wide.

weftd: store the epoch in a one-row table created by the seq migration; document in the ops
runbook that restore procedures MUST bump it (and have the server bump it automatically when
it detects `nextval` below the highest seq present in stamped rows at boot — a cheap startup
sanity check).

---

## Part 3 — The `SYNC` verb

### 3.1 Fresh login (no cursor)

```
C: @label=s1 SYNC preview=30
S: @label=s1;owner=ada@test.example;unread=12 NS-META gaming public
S: @label=s1;category=Text CHANNEL-LAYOUT #gaming/general 0
S: @label=s1;category=Text CHANNEL-LAYOUT #gaming/clips 1
S: @label=s1 POLICY #gaming/general retained:90d
S: @label=s1 MARKED #gaming/general test.example/01J…A
S: @label=s1 UNREAD-COUNTS #gaming/general 3 1
S: @label=s1;owner=eve@peer.example NS-META artclub unlisted
…
S: @label=s1 FRIEND bob@peer.example friends
S: @label=s1;name=Weekend\sCrew GROUP &01J…G :ada@… bob@…
S: @label=s1 SYNC BODY s_9f3c…
```

- The **skeleton** is served inline on stream 0: per namespace the account is a member of —
  `NS-META`, `CHANNEL-LAYOUT` per *visible* channel, `POLICY`, `MARKED`, `UNREAD-COUNTS`; plus
  `FRIEND`/`GROUP` rows. All existing §7 event shapes; no new row types in the skeleton.
  Channel set comes straight from ns membership + derivation (Part 1) — no DISCOVER/CHANNELS
  round trips.
- Instead of `SYNC END`, the skeleton terminates with **`SYNC BODY <stream-token>`** — a
  one-time data-plane token, same machinery as `BACKFILL` (§11.7). **Live event delivery
  begins at this moment** (see 3.5 race rule).
- Rosters are **not** in the skeleton or body. `MEMBERS` stays on-demand (Decision log).
- `preview=0` is legal: skeleton only, no `SYNC BODY` line, terminate with `@cursor=<seq>
  SYNC END` inline. Minimal-client mode. **(Q4: does preview=0 still push the non-preview
  per-account rows — MARKED/UNREAD/FRIEND/GROUP/tokens? assume yes.)**

### 3.2 The body stream (previews)

Pulled via the token over the data plane (`BACKFILL <token>` framing), newline-delimited §4
grammar lines:

```
SYNC START
@expired-before=test.example/01H… CHANSYNC #gaming/general
@msgid=test.example/01J…A;edited=1;edited-at=1721… MESSAGE #gaming/general ada@… :final body
@by=ada@…,bob@… REACTIONS #gaming/general test.example/01J…A 🎉 3
@by=mod@… DELETED #gaming/general test.example/01J…9
CHANSYNC #gaming/clips
…
@cursor=8412 SYNC END
```

- Per channel: a **`CHANSYNC <#chan>`** header (new event; §7.9) carrying
  `expired-before=<msgid>` (retention watermark), then up to `preview` newest surviving
  messages **oldest-first, materialized**: final-body `MESSAGE` with `edited=`/`edited-at=`
  tags, `REACTIONS` summaries, `DELETED` tombstones. Never `EDITED` chains, never
  `REACTION` add/remove pairs.
- **Previews are a server-side optimization the server MAY withhold per channel** (normative —
  the lazy-load baseline). Heuristic (non-normative): previews for unread channels + each
  namespace's few most-recently-active; every other channel gets a bare `CHANSYNC` header. A
  conforming client MUST handle a header-only channel via on-demand `HISTORY`.
- **Channel order inside the stream is a QoS knob**: unread first, then by last activity.
- The single global `@cursor=<opaque>` rides `SYNC END` at the tail.

### 3.3 Delta sync (reconnect)

```
C: @label=s2 SYNC since=8412 preview=30
S: @label=s2;msgid=… MESSAGE #gaming/general bob@… :hey
S: @label=s2;msgid=test.example/01J…A;edited=2;edited-at=… MESSAGE #gaming/general ada@… :re-fixed
S: @label=s2 REACTIONS #gaming/general test.example/01J…A 🎉 4
S: @label=s2 DELETED #gaming/general test.example/01J…B
S: @label=s2;reset CHANSYNC #gaming/new-chan
S: @label=s2 UNREAD-COUNTS #gaming/general 5 2
S: @label=s2;cursor=9010 SYNC END
```

- Serves every row with `seq > since` in the account's visible set, **as materialized current
  state**. Small deltas inline; server MAY upgrade to `SYNC BODY <token>` above a threshold
  (reuse `HISTORY_STREAM_THRESHOLD`, 200 lines).
- **Client apply rule (normative): a row whose key you already have replaces it.** Keys:
  message → msgid; reaction summary → (msgid, emoji); meta → (target, key); marker → channel.
  Upsert, idempotent, order-insensitive.
- **`reset` flag on `CHANSYNC`** (valueless tag): server cannot serve an honest delta for this
  channel — client MUST drop its cached rows and treat the following preview (if any) as a
  fresh head. Emitted when: the channel became visible after the cursor (ns joined, channel
  created, view granted, hide cleared), the cursor predates the delta horizon, or the channel
  was re-keyed (`CHANNEL RENAME`). **Replaces `truncated`** for sync purposes.
- **Purge is signaled by watermark, not tombstones**: purged rows produce no delta entry; the
  client evicts everything older than `expired-before`.
- **Delta horizon (Q3):** RECOMMENDED serve deltas for cursors up to `max(retention window,
  30 d)` old; older → per-channel `reset`. Config knob.

### 3.4 What SYNC replaces

Rewrite §9.7 (client reconnect) in full: reconnect with jittered backoff → `HELLO` →
`AUTH KEY` → `SYNC since=<cursor> preview=N` → resend unacked labels (§9.2 dedup makes this
safe). Fresh login is the same verb with no cursor. **One code path.** Delete the per-channel
`HISTORY after=` loop and the "render truncated as a gap" step.

### 3.5 Snapshot/live race rule (normative)

Live events start when `SYNC BODY` is handed over; a live event has `seq > cursor` but may
arrive before its channel's body section. Rule: **the client buffers live lines for a channel
until that channel's CHANSYNC section has been applied, then replays them on top** (after
which ordinary upsert applies). Per-channel buffer, bounded by body-stream latency. Server
side: never buffer — that's the §9.2 sin.

---

## Part 4 — Materialized wire form everywhere; HISTORY & BATCH cleanup

1. **`HISTORY` always serves materialized rows.** Same request shape
   (`before=`/`after=`/`limit=`/`thread=`), unchanged pagination-by-msgid. Delete the
   `compacted` flag — now unconditionally true. Delete every "in batches" special case in
   §7.2 (`EDITED` is live-only *everywhere*; `REACTIONS` is the *only* non-live reaction form).
2. **Lazy-load baseline (normative, §6.4 or new sync section):** deep scrollback and
   cold-channel first-load are the same `HISTORY before=` request; a client opening a channel
   with no cached rows fetches on demand. Prefetch triggers are client policy, non-normative.
3. **Stale-with-gap client rule (non-normative, reference-client doc):** on open with cached
   rows but no recent preview, `HISTORY after=<newest cached>` either bridges the gap
   (contiguous → merge) or returns a full page with more remaining → drop the stale island,
   adopt the newest page as head, let scrollback re-fetch the middle. Client twin of `reset`.
4. **BATCH retirement — phased, do NOT break clients in one release:**
   - **Phase 1 (this change):** `SYNC` never uses BATCH. `HISTORY`/`PINS`/`SEARCH`/`MEMBERS`/
     `THREADS`/`REPORTS LIST` keep BATCH framing but drop the `compacted` flag; `truncated`
     survives only on network-level federated backfill (§11.7).
   - **Phase 2 (flagged, later):** replace BATCH on request/response pages with label-echoed
     lines + a `DONE [more=<cursor>]` terminator; keep BATCH only for genuinely
     server-initiated bulk. Behind a `features=` flag; not this pass — note in §18.
5. **§12.1 simplification:** compaction becomes purely a storage-cost concern, never a
   wire-form concern. Audit window, retention holds, hold invisibility **unchanged**
   (invariant 11 intact). E2EE: sync rows for `e2ee` channels are opaque ciphertext blobs with
   seq stamps; the client materializes after decrypt (invariant 8 intact).

---

## Part 5 — Client cache model (reference client / docs, non-normative)

Per channel: ordered map `msgid → materialized row` + **one contiguity range** (preview seeds
the head; scrollback extends downward; deltas and live events extend upward) + the `CHANSYNC`
header (watermark, marker, badge). Global: one cursor. Eviction: LRU message rows down to
~1–2 pages for cold channels; **never evict the CHANSYNC header**. The cursor is a property of
the **cache/device**, not the account: a second device does a fresh full SYNC; the first
device's next login is a small delta.

---

## Part 6 — Spec document edits (checklist) → `weft-spec-v0.11.adoc`

- [ ] §1 table: History row → "per-channel policy, materialized sync"; add a Sync row.
- [ ] §2.1/§6.2/§6.3: membership rewrite per Part 1 (durable-membership paragraph re-anchored
      to ns rows + derivation; top-level channels exempted explicitly; **roster query stays
      per-channel `MEMBERS <#ns/chan>`, server-derived** — Decision log).
- [ ] New section (suggest §9.8 or new §6.9 "Synchronization"): SYNC verb, skeleton, body,
      delta, `reset`, race rule, apply rule, delta horizon.
- [ ] §7.9: add `CHANSYNC`, `SYNC BODY`, `SYNC START/END` shapes; note `cursor=`, `reset`,
      `expired-before=` tags.
- [ ] §7.2: delete batch-form special cases; `REACTIONS` re-described as "materialized summary
      form (sync/HISTORY)".
- [ ] §9.7: rewrite to the four-step flow (Part 3.4).
- [ ] §9.0: sharpen invariant 1 (inside-boundary sentence, 1.4); consider a reserved slot for
      "**Sync is upsert**: every non-live fetch serves materialized current state keyed for
      replacement; event chains never appear off the live path."
- [ ] Cursor opacity + sync epoch (2.4): opaque ≤64 B token, epoch mismatch = cursor-less;
      cite the `MORE`-cursor precedent.
- [ ] §12.1: wire-form paragraphs deleted; storage semantics kept; holds/e2ee paragraphs kept.
- [ ] §11.7: unchanged mechanics; add one sentence that lazily-pulled federated rows are
      seq-stamped on ingest and surface in the next client delta automatically.
- [ ] §18: add Phase-2 BATCH retirement.
- [ ] Appendix A: new milestone entry (house style).
- [ ] Appendix B: seq low-water-mark note (2.2), migration number, delta-horizon config knob,
      preview heuristic.

---

## Part 7 — weftd implementation order

1. Migration: `ns_membership` + `channel_hide` + backfill + drop (1.6). Contract tests on both
   backends.
2. Membership derivation in the access-check path; rewire `NS JOIN`/`JOIN`/`PART`/`NS LEAVE`/
   `CHANNEL CREATE`/`INVITE REDEEM`; roster derivation for `MEMBERS`/counts.
3. Seq column + stamping on every write path in 2.1; low-water-mark cursor tracker.
4. `SYNC` no-cursor path: skeleton query + body stream over the existing BACKFILL token
   machinery; preview heuristic.
5. `SYNC since=` delta query (`WHERE seq > ? AND target IN (visible set)`); `reset` emission
   cases; delta horizon config.
6. HISTORY → materialized-only; drop `compacted`; keep BATCH framing (Phase 1).
7. Reference client: cache model, race buffer, upsert apply, gap rule, prefetch.

---

## Acceptance tests (invariants are tests, per §9.0)

1. Channel created in a namespace → an existing ns member receives `CHANNEL-LAYOUT`+`POLICY`
   and can post with **zero** join commands.
2. `PART <#ns/chan>` then `SYNC` fresh → channel absent from that account's skeleton; `JOIN`
   restores it.
3. Ns member without `view` on a gated channel: no layout row, no sync section,
   `NO-SUCH-TARGET` on probe, absent from derived roster (invariant 1, inside-boundary).
4. Migration round-trip: pre-upgrade sidebar == post-upgrade sidebar for a fixture with
   partial channel membership.
5. Edit + reaction on an old message while client offline → `SYNC since=` delivers the message
   re-materialized with `edited=` bump and updated `REACTIONS`; `HISTORY after=` alone would
   have missed it (regression-pin the old bug).
6. Cursor older than delta horizon → per-channel `reset`, client cache drop, correct fresh head.
7. Live event racing the body stream: event for `#x` arrives before `#x`'s CHANSYNC section →
   applied after, final state correct (property test: any interleaving converges).
8. Low-water mark: concurrent transactions commit out of seq order → no client ever misses the
   slow transaction's row.
9. Purge: rows past retention produce no delta entries; client evicts via `expired-before`.
10. E2EE channel sync serves ciphertext blobs only; hold-flagged rows serve identically to
    unheld (no surface difference).
11. Epoch mismatch (simulated restore-from-backup): a client holding a pre-restore cursor is
    treated as cursor-less — full skeleton + resync, never a silent empty delta. Also test the
    boot-time sanity check: `nextval` below the max stamped seq → automatic epoch bump.
