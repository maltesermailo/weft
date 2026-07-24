# CHANGES — WEFT spec v0.10 → v0.11 (editorial consolidation)

**No wire-behavior change.** Every edit is documentation of already-specified (or already-shipped-and-appendix-recorded) behavior, a stale-passage reconciliation, or a grammar repair. Judgment calls that *could* touch behavior are in `DECISIONS-NEEDED-v0.11.md`, not silently resolved. All 259 concrete wire examples in the revised document were mechanically round-tripped (parse → serialize → parse) through the reference codec (`weft-proto`); all pass.

---

## Pass 1 — §11.10 Auto-federation (new section)

Reconstructed from the scattered v0.10 sources it was cited by: §6.2 `NS META federation`, §6.5 `INVITE MINT` (embedded namespace), §6.6 `FEDERATE` + `BRIDGE REQUEST`, the M4-5 `federation=yes` tag, M-media-4 ("auto-federation always offers `history=full`"), and the unfurl amendment's SSRF description ("exactly like §11.10"). Covers: reachability rule + anti-enumeration; both triggers (explicit `FEDERATE`; foreign-invite redemption); the gates (`auto_bridge` config, NETBLOCK, per-account cooldown); the end-to-end flow (well-known key fetch → `AUTH BRIDGE` → `BRIDGE REQUEST` → peer `BRIDGE PROPOSE history=full` → auto-accept → async `MANIFEST`); and the SSRF classifier as invariant 13's normative home (class list — loopback / RFC 1918 / CGNAT / link-local / ULA / metadata / v4-mapped — recovered from the repo's invariant-13 definition; the userinfo-strip / IP-pinning / redirect-re-check details come from the unfurl amendment's own text).

Three `[TODO: unspecified — confirm with owner]` markers: (1) whether foreign-invite redemption auto-routes or the client issues `FEDERATE` first; (2) the cooldown duration; (3) the amendment-draft-only proposals (sever-on-idle, auto-rejoin re-trigger, global dial caps, "e2ee never auto-bridged") — none of which v0.10 text states.

## Pass 2 — §9.0 Protocol invariants (new registry)

Defined **1, 2, 3, 4, 8, 9, 11, 12, 13** from the spec's own scattered statements (§2.2/§8/§13 for 1; §5.1/§11.4/§11.7 for 2; §11.1/M5 amendment for 3; §10.4 for 4; §5.2/§14/§2.4 for 8; §2.4 for 9; §12.1/§6.3-rename for 11; §6.7/§11.9 for 12; §11.10/unfurl for 13). **5–7 and 10 are cited nowhere in the document** (verified by grep before and after) and are marked `[reserved — recover from repo history or retire]`, not invented. Every "invariant N" citation in the document now resolves to a defined registry row (3 is cited via "invariants 2, 3" in §11.7).

## Pass 3 — Consistency repairs (originals quoted)

1. **§11.2 stale bridge-token line.** Original: *"Mutual QUIC session authenticated by a `bridge` capability token — same acceptor path as clients."* → Rewritten to the network-key challenge-response model (asserting key + `CHALLENGE`/`AUTH PROOF`, pinned vs accept-any, pin-wins, uniform `AUTH-FAILED`), matching §6.6 and the M5 amendment; the `bridge` capability is now explicitly the §11.3 *propose* authorization, not session auth.

2. **Examples violated the §4 grammar.** Original class (one of ~25): *"`MESSAGE #gaming/general ada@test.example :gg msgid=test.example/01J…A`"* — `msgid=` after the trailing marker parses as body text. Every affected example in §6 and §7 was rewritten with tags before the verb (`@label=x;msgid=… MESSAGE … :gg`) and then **machine-verified**: all 259 extracted examples round-trip through `weft-proto`. Codec-truth fixes this surfaced beyond tag placement: `BATCH START`/`END` carry `id=` as a **tag** (original syntax column read *"`BATCH START <id>`"*); `MANIFEST`'s state is `live|added|removed|severed` (an example read *"`2 accepted`"* — not a valid state); `VERIFIED`'s states are `pending|confirmed` (original event row read *"`state=verified|pending`"*, contradicting §10.5); `THREAD`'s `replies=`/`last=` are tags (original syntax read *"`THREAD <#chan> <root> replies=<n>`"*); **`PINNED`'s `by=` is a bare local account** in the codec, unlike `DELETED`/`MODERATED` whose `by=` take full subject strings (caught by the round-trip lint); the §6.6 `VOICE` example claimed *"`VOICE JOIN` → `VOICE DESC`"* — per §16's own text JOIN answers with the endpoint + token grant, i.e. `VOICE OFFER`, now documented in §7.10.

3. **Tags vs key=value middle params.** Stated once in the §7 preamble: commands carry key=value as **middle params** (shown in Syntax); events carry them as **tags** — sole exception `ROLE`'s `hoist=`/`pos=`, which echo the command form. All examples made uniform. Whether to *unify* the asymmetry is Decision #1.

4. **§9.5 vs §6.8/§11.12.** Added the honest note: a two-member cross-network group DM already carries the conversation (the group tunnel is the current cross-network path); the deferred §18 #7 item is the 1:1 DM *semantics* (default retention, symmetric `HISTORY @user`, no-threads).

5. **§6.3 `CHANNEL META` key list.** Original: *"`<topic|view-gated|category|position>`"* → adds `posting` (used by §6.7 `posting :restricted`); §7.4's `CHANMETA` key list likewise.

6. **Rung-3 remnants.** §2.4's main text already reflected the immediate-takeover amendment; the only remaining "30-day" mentions are §2.4's own self-describing history ("Earlier drafts specified…"), the clearly-historical Appendix A entries, and an unrelated `retained:30d` example. §2.4 → Appendix A cross-links are bidirectional.

Additional stale-reconciliation fixes in this pass: §6.8's four wrong `§11.11` citations → `§11.12` (the group tunnel); §10.4's standard cap set gained `media-block` (used by §6.6 but missing from the list); `REGISTER`'s example no longer shows an `@attestation=` (password registration returns a plain `WELCOME`).

## Pass 4 — Promotions; Appendix A shrunk; Appendix B created

**Verbs promoted from appendix-only to §6 rows** (syntax verified against the codec): `VERIFY EMAIL/CONFIRM/BIRTHDAY/LIST` (§6.1), `EMOJI ADD/REMOVE/LIST` (§6.2), `THREADS` + `THREAD NAME` (§6.4), `CAPS` (§6.5), `MODLIST` (§6.7). **Events**: `VERIFIED` moved to §7.1 with corrected states; `VOICE OFFER` added to §7.10; `PRESENCE` gained `offline`; `CHANNEL-LAYOUT` gained `kind=`.

**Behaviors promoted into home sections:** durable membership + auto-rejoin + per-account dedup, the presence-vs-membership model, and unread-push mechanics (§6.3); `MEMBERS` batch shape + presence interleave + `CAP-REQUIRED view` (§6.3 row); report routing + the push-to-default-handlers limitation (§6.7); threads naming/listing + custom-emoji rendering (§9.4); server-authoritative categories + layout broadcast (§6.2, replacing the "Appendix A layout" pointer); manifest strictest-safe defaults (§6.6 row); lazy federated backfill + (channel, before) dedup + the `history=full` rationale (§11.7); the media transfer surfaces, `MEDIA TOKEN` bearer, uniform not-found, refcount/GC, and the unfurl proxy (§13 rebuilt); `GRANT`/`REVOKE` subject syntax now shows the foreign `user@net` form §10.4 defines.

**Appendix A** rewritten to 1–3-line entries (what/why/where; the rung-3 entry keeps its trade-off candor per the preserve directive and runs slightly longer); now **7% of the document** (target ≤15%). **Appendix B (non-normative)** holds the stripped implementation identifiers: migrations, store/CLI notes, `[smtp]`/`[unfurl]` config, constants (incl. `HISTORY_STREAM_THRESHOLD`, `HOLD_RADIUS`, `RECOVERY_DELAY_RUNG3_SECS`), the IRC shipped-subset, invite id+counter status, substring-search semantics. Body text stripped of `State::Bridge`, `Actor::Foreign`, `Scope::Group`, `FriendDeliver` (→ "friend-delivery conduit"), `EventStore::…`, table names, and `.svelte` references; verified by pattern scan — zero remain outside Appendix B.

## Pass 5 — Readability

§0 added (RFC 2119/8174 declaration + seven term definitions). §11.3's *"Blast radius priced in signatures"* expanded to full sentences. §2.3's escape rendering fixed (*"`\r`→`\r`"* → "CR (0x0D) → the two-character sequence `\r`", etc.); §4 notes the IRCv3 tag-escaping lineage. Paragraph-length rationale moved out of the `ROLE RENAME`, `NS RECOVER`, and `BRIDGE REQUEST` cells into short prose below their tables (one-line cells with pointers remain).

## Pass 5b — Readability follow-up (owner feedback: "paragraphs such as these are unreadable")

Nineteen dense enumeration paragraphs restructured into tables and lists, meaning-preserving: §6.7 `REPORT` arguments (→ argument table + routing list) and `REPORTS RESOLVE` actions; §11.6 NETBLOCK (→ the four effects as a numbered list); §11.7 (→ served-iff conditions, bulk-transfer, reconnect, lazy-pull steps); §11.8 mirror pull (→ numbered flow); §9.2 (→ send/receive/backpressure bullets); §9.7 reconnect (→ numbered sequence); §10.4 token model (→ bullets; the standard cap set and scope floors separated); §17 IRC (→ IRC↔WEFT mapping table + degradation list); §13 transfer surfaces + unfurl endpoints; §6.2 layout rules; §6.3 unread counting; §6.5.1 role model + rename rationale; §9.4 threads; §11.10 reachability; §12.1 retention holds. Machine-checked afterwards: zero >550-char prose lines remain; all 259 examples still round-trip; all cross-references still resolve.

## Pass 6 — Self-containment

All §-cross-references machine-audited: **every reference resolves to a real heading** (§11.10 and §9.0 included). The two flow documents are marked **non-normative** at their §6.8 and §11.12 mentions. The §6.2 `categories` row now points at the in-section layout prose instead of "Appendix A layout". Federation-internal group verbs (`GROUP SYNC/RELAY/MUT/BACKFILL/ROSTER`) keep their syntax table in §11.12 by design — they are bridge-only and deliberately absent from the client-facing §6.

## Verification (all checklist items)

- §11.10 exists; all references land; only owner-flagged `[TODO]`s inside — **pass**
- Every invariant citation resolves; 5–7/10 reserved, not invented — **pass**
- Every command verb in any example has a §6 row (§11.12 verbs live in §11.12's own table, by design) — **pass**
- All 259 wire examples round-trip through `weft-proto` (parse → serialize → parse, structural equality) — **pass**
- No migration numbers / Rust / Svelte / DB identifiers outside Appendix B (pattern scan) — **pass**
- Appendix A ≈ 7% of bytes; entries ≤3 lines (rung-3 entry slightly longer, deliberately) — **pass**
- All cross-references resolve; external files marked non-normative — **pass**
- Zero semantic changes outside stale-passage reconciliation; judgment calls in the Decisions list — **pass**
