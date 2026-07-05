# M4c Review — reporting + retention holds (§6.7, §12.1, invariants 11 & 12)

*Self-review of the last M4 piece: message reporting, honest content
states, retention holds, and reporter confidentiality. Status: **214 tests
green** workspace-wide, clippy `-D warnings` clean, store contract green
against live PostgreSQL, file→queue→resolve verified end-to-end over real
QUIC.*

## What shipped

- **`REPORT <msgid> <category> [scope] [:note]`** (§6.7): membership-gated
  (you can only report what you can see — invariant 1), per-account rate
  limited (RECOMMENDED 10/rolling hour → `THROTTLED` + `retry-after=`),
  routed to the reporter's home network. Category = the normative set or an
  `x-` extension. `scope` is the `ns|net` routing hint (default `ns`);
  `csam`/`illegal` ALWAYS also reach the operator, who is legally
  accountable.
- **`REPORTS LIST <scope> [status=] [cursor]`** and **`REPORTS RESOLVE
  <id> <action> [:note]`** for `reports`-cap holders — `<scope>` is the
  *concrete* cap scope (`ns:<name>` or `*`), so a handler lists exactly the
  queue their cap covers. `escalated` re-routes an ns report up to net,
  keeping it open and its holds intact.
- **Events**: `REPORTED` (labeled ack), `REPORT-FILED` (`state=`/`scope=`/
  `reporter=`), `REPORT-RESOLVED` (handler echo carries `by=`/`note=`; the
  reporter's push carries neither).
- **Retention holds** (§12.1, invariant 11): a `verified` report places
  refcounted holds on the reported root ± `HOLD_RADIUS` (=25) context roots;
  held roots are exempt from **both** purge and compaction until the report
  resolves + a 7-day grace, then released by the maintenance scheduler.

## Layering, one pass per boundary

Proto-first, as always: wire enums (`ReportScope`/`ReportStatus`/
`ResolveAction`/`ContentState`) + `report_category_ok`, three commands,
three events, all with round-trip tests (68 codec tests). Then the store
(`ReportStore` trait + `ReportRecord`/`ReportResolution`, both backends,
one shared contract). Then core (handlers + routing + cap enforcement +
confidentiality). Then weftd conformance + docs. No layer reaches past its
neighbour.

## How the two invariants hold

**Invariant 11 (holds exempt reported content from purge + compaction):**
the hold is a `(scope, root) → refcount` structure living *with* the events
(same `Inner` in memory, same DB in Postgres), so `purge_before` and
`compact_before` consult it directly — a held family is simply skipped.
Refcounting means overlapping report contexts compose and releasing one
report never unholds content another still holds. Proven in the store
contract against both backends: an 11-message channel with a held report
purges **0**, and after resolution + grace-window release, purges all 11.

**Invariant 12 (reporter confidentiality):** upheld *structurally by who
receives what* — the reported party is on none of the three delivery paths
(`REPORTED`→reporter, `REPORT-FILED`→handlers, `REPORT-RESOLVED`→handlers +
reporter). The reporter's resolution push is minted with `by: None, note:
None`, so even the reporter never learns the handler or their note. A core
test asserts the reporter's push is the minimal form while the handler's
echo is full; the conformance test re-checks it over QUIC.

**Invariant 4 (caps precede side effects):** `REPORTS RESOLVE` verifies the
resolver holds `reports` at one of the report's queue scopes *before* any
mutation; an unknown report answers `NO-SUCH-TARGET` (fetch fails before the
cap check — anti-enumeration, so a stranger can't probe report existence).

## The honest content-state decision

The same-network path only ever produces `verified`. `unverified`
(expired/ephemeral) and `reporter-attested` (e2ee) can't arise here: an
`ephemeral` channel stored nothing, so `find_root` misses and we already
answered `NO-SUCH-TARGET` (invariant 1 — anything the server can't find is
indistinguishable from nonexistent). Rather than fabricate an `unverified`
acceptance that would leak "this msgid once existed", both states are wired
fully through the codec + store and first *emitted* when the content they
describe exists: bridged replicas (M5) and e2ee (M6). Called out in the
spec amendment, not glossed.

## Honest limitations (flagged, not hidden)

- **Live push reaches default handlers only.** `REPORT-FILED` is pushed to
  the namespace owner (ns queue) or operators (net queue) via a new
  directory `notify`; delegated `reports`-cap holders fetch via `REPORTS
  LIST`. There is no reverse cap→account index for a live fan-out — the same
  pull-not-push limit as the §2.4 recovery announcement. The queue (pull) is
  the source of truth; the push is a best-effort nicety.
- **No reporter anonymization toward ns handlers yet** (§6.7 says a network
  MAY). Handlers currently always see the reporter (accountability); the
  operator-preserving/ns-anonymizing config is deferred.
- **`REPORT-FORWARD` across bridges** (§11.9) is federation — M5.

## Testing

- Codec: `report_round_trips` (default-scope minimalism, net+note, the
  ns-scope-with-note case that forces the scope onto the wire so a note
  isn't re-parsed as scope), `reports_list_resolve_round_trip`, and
  `report_events_round_trip` (mandatory `state=`/`scope=`, anonymized form).
- Store contract (×2 backends): verified filing places 11 holds; purge
  returns 0 while held; list by scope (ns-only excluded from `*` until
  escalate); unverified holds nothing; rate-limit counter; escalate;
  resolve refuses double-resolve; grace-gated release is idempotent and
  re-enables purge.
- Core: full flow ack→queue→resolve with the confidentiality split;
  unseen-msgid and made-up-msgid both `NO-SUCH-TARGET`; `reports` cap gate
  on LIST + anti-enumeration on RESOLVE.
- Conformance: file→operator-queue→resolve over real QUIC, reporter gets the
  minimal push.

## Next

**M4 is done.** M5 federation: BRIDGE manifest state machine, remote
ingestion, strictest-policy negotiation, backfill (§11.7), NETBLOCK, and
`REPORT-FORWARD` (§11.9) — which finally exercises the `unverified` content
state and cross-network report routing this milestone laid the groundwork
for.
