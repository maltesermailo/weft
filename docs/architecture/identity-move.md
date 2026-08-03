# WEFT v0.14 — Account & Scope Migration ("Moving Day" edition)

**Instruction document for Claude Code.** Edit the WEFT spec AsciiDoc (`weft-protocol-spec-v0.13.adoc` or current file) to produce **v0.13 → v0.14**. This document contains the full normative design; your job is to translate it into the spec's existing idiom and integrate it section by section. Read the whole document before editing.

---

## 0. Editing conventions (follow the existing document's idiom exactly)

- Header: bump `:revnumber:` to `0.14`, `:revremark:` to `account & scope migration — move records, fences, redirects`.
- Update the preamble ("Fully self-contained; supersedes v0.13…") describing v0.14 as a **wire-extending, non-breaking** change: new verbs/events, one new error code, no change to existing wire forms.
- Command tables use the 5-column shape: `Command | Syntax | Cap | → Result / notes | Example`.
- Event tables use the 3-column shape: `Event | Payload / tags | Example`.
- Commands carry `key=value` as middle params; events carry them as `@tags` (§7 preamble rule).
- Examples use illustrative placeholder ULIDs (`01J8NSGAMING` convention, §5.3 NOTE) and show vanity in a trailing "_(shown as …)_" note where helpful.
- Every new cross-reference uses the `<<sX-Y,§X.Y>>` anchor style; add `[[anchor]]` IDs for all new sections.
- RFC 2119 keywords per §0.
- Anti-enumeration (invariant 1) must be threaded through every new surface — this spec treats it as a first-class property; do not forget it on any new query path.
- Add the decision-history entry in Appendix A and implementation notes in Appendix B (both required by the document's own convention).

---

## 1. Design summary (context for the editor — condensed rationale)

v0.14 adds **account migration** (a user moves `old@A` → `new@B` carrying friendships, namespace memberships, groups, DMs, and read state) and **scope re-homing** (groups follow their creator automatically; namespaces move manually by owner decision; DMs re-home only in corner cases). Two primitives carry everything:

1. **Move record** — a co-signed portable proof that `old@A` became `new@B`. Signed by a dedicated **migration keypair** minted at `MIGRATE PREPARE` (device keys are login-only and accounts may be password-only, so the record cannot depend on device-key continuity) **and** by A's network key. Peers verify the origin signature against A's cached well-known key (§10.2).

2. **Fence** — a signed splice point that re-homes a home-authoritative scope (§11.12): "A minted through msgid `final`; the successor home is B." Peers accept A-origin msgids up to `final` and B-origin after. This is the **only** legal origin switch (new invariant). For groups the fence is network-key-signed by the old home; for namespaces it is **namespace-root-key-signed** (owner-sovereign: A cannot block it, B cannot fabricate it).

3. **Redirect tombstones (this revision's addition)** — after a move, the old home **retains the old address and answers queries for it with a signed redirect** (`ACCOUNT-MOVED` / `NS-MOVED` / `GROUP-MOVED`) for a **configurable retention period of at least N months**, so late-arriving peers, stale invite links, and returning clients find the new home instead of a dead end.

Key consequences already settled in design discussion — encode them as written:

- **DM homing rule:** a DM is a two-member home-authoritative conversation; **home = the non-moving party's network**. When one party moves, the DM's home does not change — the mover participates via `@as` over the B↔A bridge like any spoke member. A fence applies only when the *home party* moves (successor = their new network) or for self-DMs. This resolves open question §18 #7; specify cross-network DMs as the home-authoritative two-party form.
- **Groups follow the creator:** an account move by the creator auto-triggers a fence with `successor = B`.
- **Namespaces move manually** (`NS MOVE`, root-signed). Channel/role/account ULIDs are random, not network-derived — nothing re-keys. Root-chained cap tokens stay valid; A-network-key-chained grants go inert (re-grant on B).
- **History authorship never rewrites:** pre-move msgids stay `A/<ulid>` forever (invariant 2). `ACCOUNT-MOVED` lets clients display-rewrite attribution; the wire never does.
- **E2EE never transfers:** reuse invariant 8's recovery language verbatim — `new@B` joins MLS groups as a fresh member.
- **ULIDs never reissue; vanities/handles recycle** after `redirect-retention + vanity-release-cooldown`, minting fresh ULIDs that inherit nothing (§10.1's existing no-inheritance rule does the heavy lifting).
- **Pre-move invites die at the fence** (implicit `INVITE REVOKE-ALL`); redemption gets the uniform dead-invite `NO-SUCH-TARGET`; the owner republishes B-minted links.
- **Foreign caps:** memberships auto-rebind on a verified record; **moderation caps require re-grant by default**, per-namespace config to auto-rebind.

---

## 2. New core type — §5.4 "Move records & fences" (insert after §5.3)

Add `[[s5-4]] === 5.4 Move records & fences (v0.14)`.

### 5.4.1 Move record

```
move-record = sign-both {          // deterministic CBOR, encode-before-sign (§10.4 convention)
  v:        1,
  old:      <account-ULID>,        // §10.1 ULID — never the mutable handle
  old-net:  <A>,
  new:      <account@B>,           // names the destination network explicitly
  mig-key:  <b64-ed25519-pubkey>,  // minted client-side at MIGRATE PREPARE, single-purpose
  issued:   <ms>, expiry: <ms>,    // RECOMMENDED validity 30 d
  sig-user:   sign(mig-key, body),
  sig-origin: sign(A-network-key, body)
}
```

Normative points:

- Both signatures REQUIRED. `sig-origin` carries the authority within the §11.11 trust model (A is the identity provider); `sig-user` distinguishes a user-initiated move from a unilateral relocation by A — audit value, honestly stated as not preventing a hostile A.
- The record names `new: account@B`; redemption by any third network is `FORBIDDEN` (a leaked record is inert elsewhere).
- The `mig-key` is **not** a device key: minted at `MIGRATE PREPARE`, used only to sign this record and `MIGRATE CLAIM` proofs, discarded after `COMMIT`. Works identically for password-only accounts.
- Verifiers check `sig-origin` against A's well-known key (§10.2, cached); a record from a netblocked network is refused (`BLOCKED`, §11.6 effect 3 extended: netblock also rejects move records).

### 5.4.2 Fence

```
fence = sign(authority-key, {      // deterministic CBOR
  v:         1,
  scope:     &<group-id> | dm:<ULID-pair> | ns:<ns-id>,
  final:     <last msgid minted by the old home>,
  successor: <network>,
  reason:    account-move | manual,
  record:    <b3-hash of move-record>,   // present iff reason=account-move
  issued:    <ms>, expiry: <ms>          // freeze bound — see thaw rule
})
```

- **Authority key:** the old home's network key for groups and DMs; the **namespace root key** for `ns:` scopes (strictly stronger — outranks both networks for ns administration, §2.1).
- **Splice rule (normative):** peers accept scope events with old-home-origin msgids with ULIDs ≤ `final`, and successor-origin msgids after. Any other origin remains `FORBIDDEN origin` (invariant 2 unchanged — each msgid is honored from the network that minted it; the fence changes only *who mints next*).
- **Gap detection:** the successor's first minted event on the scope carries `@after=<final>`; a peer that never saw the fence detects the unknown-origin msgid + `@after` tag and MUST fetch the fence (`FENCE? <scope>` query, below) before accepting — never silently accept a re-home.
- **Thaw rule:** a fence freezes minting at the old home from `issued`. If the successor has not confirmed takeover (first minted event or explicit ack) by `expiry`, the fence is void and the old home thaws (resumes minting). RECOMMENDED expiry: 72 h. A voided fence is announced like a fence (no silent state).
- Pre-fence history is never re-minted; dedup, `MARK`, and `reply-to` references are untouched.

---

## 3. New command family — §6.10 "Migration (MIGRATE / NS MOVE)" (insert after §6.9)

Add `[[s6-10]] === 6.10 Migration — account moves & scope re-homing (v0.14)`, scope tag *(S/N/NS/F)*.

### 6.10.1 Account migration commands (client → own server, except CLAIM/IMPORT)

| Command | Syntax | Cap | → Result / notes |
|---|---|---|---|
| `MIGRATE PREPARE` | `MIGRATE PREPARE <account@B>` | authed (on A) | Client submits the freshly-minted `mig-key` pubkey in `@migkey=<b64>`. A verifies B is not netblocked, mints + co-signs the move record, marks the account `move-pending`. → `MOVE-RECORD <b64>` \| `BLOCKED` \| `CONFLICT` (move already pending). |
| `MIGRATE EXPORT` | `MIGRATE EXPORT` | move-pending | → `STREAM ACCEPT <token>`; the **portable bundle** (deterministic CBOR, data plane): profile blob (§10.3), friends[] (`user@net` + state), ns-memberships[] (`net/ns-id` + hide-overrides + nick), groups[] (`&id` + home + roster snapshot), DM index (peer + home + last msgid), read markers, verification claims (**minus subjects** — PII stays; states re-prove on B, email re-verifies). |
| `MIGRATE IMPORT` | `MIGRATE IMPORT <b64-move-record>` | authed (on B, as `new`) | Followed by the bundle via `STREAM OFFER migration …`. B verifies both record signatures + that `new` = the authed account, then walks the bundle (rebinding phase, 6.10.2). → `MIGRATION-STATUS` per target + terminal `MIGRATION-DONE` \| `FORBIDDEN` (bad sigs / wrong destination / expired). |
| `MIGRATE COMMIT` | `MIGRATE COMMIT` | move-pending (on A) | Client confirms after B reports acceptable status. A: flips the account to `moved`, locks the handle (never reissued — §10.1 extension), fans out `ACCOUNT-MOVED` over every bridge holding state about the account, auto-fences groups the account created (`reason=account-move`), applies the DM homing rule, and starts the **redirect retention clock** (§6.10.4). → `ACCOUNT-MOVED` (own copy). |
| `MIGRATE ABORT` | `MIGRATE ABORT` | move-pending | Voids the record (A records its hash as revoked and answers `FENCE?`/verification queries accordingly), thaws any pending fences. → `MIGRATION-DONE aborted`. |
| `MIGRATE CLAIM` | `@as=new MIGRATE CLAIM <old-ULID>@A <b64-move-record>` | bridge session (B→peer) | **Destination-driven rebinding** for peers A can't reach or that missed the fan-out: the peer verifies the record (origin sig vs cached A key; `new` = the asserting `account@B`) and re-keys its edges — ns membership rows, friend edges, group roster entries, grant *records* (tokens naming `old@A` go inert and re-mint, exactly the v0.13 name→ULID precedent). Moderation-cap grants rebind only if the scope's config says so (default: re-grant required). → the rebound state events \| `FORBIDDEN` \| `NO-SUCH-TARGET` (peer holds nothing about `old@A` — indistinguishable, invariant 1). |

### 6.10.2 B's rebinding phase (normative sequence, prose after the table)

For each bundle target, B emits one `MIGRATION-STATUS` line:

1. **Foreign namespaces:** reuse auto-federation verbatim — `FEDERATE net/ns-id` (§11.10, all gates apply: `auto_bridge`, netblock, cooldown, SSRF classifier invariant 13), then `@as=new MIGRATE CLAIM` on the resulting bridge.
2. **Friendships:** `@as=new FRIEND ADD <peer>` carrying `@record=<b64>`; the peer's network verifies and re-keys the existing edge in place — **no new friend-request UX** for the peer; the peer receives an informational `FRIEND … moved-from=old@A` state push.
3. **Groups:** claim against each group's home; roster entry re-keys. Groups **created** by the mover are handled by A's auto-fence at COMMIT (successor = B); B confirms takeover by minting its first event `@after=<final>`.
4. **DMs:** per the homing rule — no re-home when the peer's network is the home (the normal case); the mover's copy backfills via `@as HISTORY`. Fence only for DMs the mover's network homed (self-DMs; both-parties-on-A conversations where the peer stays: home remains A, mover goes remote).

Status values: `rebound | queued | refused | netblocked | no-such | unreachable`. **Partial success is the normal case (normative):** unresolved targets form a retry queue on B (backoff, RECOMMENDED horizon 30 d), not a failure of the migration.

### 6.10.3 Manual namespace move

| Command | Syntax | Cap | → Result / notes |
|---|---|---|---|
| `NS MOVE` | `NS MOVE <ns-id> <network>` (`@sig=<b64>` root signature, §6.2 signed-verb convention) | **root key** | Owner-sovereign re-homing. A freezes the ns's channels (spokes' posts queue in the §11.13 bounded outbox), transfers rosters + materialized history to B (STREAM; lazy backfill acceptable — the fence, not transfer completeness, is the correctness anchor), then fans out the root-signed fence as `NS-MOVED` to all manifest-sharers + members. Peers re-pin `<B>/<ns-id>` on the **root signature alone** — A's cooperation affects transfer quality, never the re-pin. Channel/role ULIDs unchanged; root-chained tokens survive; A-key-chained grants go inert. → `NS-MOVED` \| `FORBIDDEN` (bad root sig) \| `BLOCKED` (successor netblocked *from A's side*: A MAY refuse to serve transfer but MUST still honor the fence — state this honestly). |
| `FENCE?` | `FENCE? <scope>` | membership / bridge session | Fetch the current fence (or `moved` redirect) for a scope — the gap-detection recovery path (§5.4.2). → `NS-MOVED`/`GROUP-MOVED` \| `NO-SUCH-TARGET` (no fence, or caller not entitled — invariant 1). |

### 6.10.4 Redirect tombstones & retention (**the new requirement — give this its own subsection**)

After `MIGRATE COMMIT` (accounts) or a confirmed `NS MOVE` / group fence (scopes), the old home **retains the old address as a tombstone** and serves signed redirects:

- **Config:** `redirect-retention: <dur>` — network configuration, **normative floor 1 month, RECOMMENDED default 6 months** ("at least X months, configurable"). Independently settable for accounts and namespaces if the operator wishes (Appendix B detail).
- **What redirects, and with what:**
    - `AUTH` (password or key) against a moved account → `ERR MOVED account@B` with `@record=<b64>` — the returning client verifies and re-points itself.
    - `@as=old@A` commands arriving over any bridge → `ACCOUNT-MOVED` (labeled to the caller) instead of execution.
    - Attestation/verification lookups for the old account (§10.2 well-known path) → the well-known document MAY list moved accounts is **rejected** — instead the *protocol* surface answers: any query resolving `old@A` gets `ACCOUNT-MOVED`.
    - `FEDERATE A/<vanity>` / `BRIDGE REQUEST <vanity>` for a moved namespace → `NS-MOVED <ns-id> <B>` carrying the root-signed fence; the requester re-runs the flow against B — **gated by the visibility rule below** (members and manifest peers get the redirect; strangers get `NO-SUCH-TARGET`).
    - **Pre-move invites are invalid (normative).** The fence acts as an implicit `INVITE REVOKE-ALL` for the scope: every A-minted invite dies at the fence, redemption attempts receive the standard uniform dead-invite `NO-SUCH-TARGET` (§8 — no special "moved" disclosure to invite holders, which keeps the anti-enumeration story simple: an invite that no longer validates confirms nothing). Already-redeemed members are unaffected (redemption minted membership, not the invite). New joiners need fresh B-minted invites; the owner should announce the move in-band (the `NS-MOVED` fan-out reaches all members) and republish links. Consequence: B never imports A's invite store, and the `@invite=` unlock on `FEDERATE`/`BRIDGE REQUEST` (§11.10) simply stops matching for old invites — a non-public moved namespace is reachable post-move only via B-minted invites.
    - `HISTORY`/backfill for a moved scope → served read-only from A's final replica during retention (A already keeps it, §11.13 availability rule), with the fence attached so the puller knows where live traffic went.
    - `FENCE? <scope>` → the fence, for as long as the tombstone lives.
- **Anti-enumeration (normative):** a redirect is served **only to callers entitled to see the thing pre-move** — members, manifest peers, valid-invite holders, the friend on a friend edge. Everyone else gets the uniform `NO-SUCH-TARGET`. A move MUST NOT turn a `private` namespace or an unlisted account relationship into a public disclosure of "this existed and went to B." (Thread this into invariant 1's text.)
- **After retention expires:** the old home MAY drop the tombstone; queries then get `NO-SUCH-TARGET`.
- **Vanity release (normative):** the moved **vanity label** (namespace/channel vanity, and the account **handle** — same rule, they are the same kind of thing under v0.13) becomes re-registerable after `redirect-retention` **plus** a `vanity-release-cooldown` (network config, RECOMMENDED default 3 months — so ~9 months total on defaults). The **ULIDs never reissue** — a re-registered vanity mints a *fresh* ULID and inherits nothing: no memberships, no grants, no federation pins, no history (this is precisely the v0.13 identity guarantee doing its job — every token, grant, and peer pin names the old ULID and simply doesn't match the newcomer). Operators MAY lock a released vanity instead of releasing it (§5.3 lock mechanism, unchanged). Note in the spec text that this rule also answers §18 #3 (namespace squatting cooldown after `NS DELETE`) — apply the same `vanity-release-cooldown` there for consistency.
- **New error code (§8):** `MOVED` — "target migrated; follow the redirect" — context = the new address, `@record=`/`@fence=` tag carries the proof. Note in the registry that `MOVED` is deliberately distinct from `NO-SUCH-TARGET`: it is served only inside the visibility gate above.

---

## 4. New events — §7.12 "Migration" (insert after §7.11)

| Event | Payload / tags | Example shape |
|---|---|---|
| `MOVE-RECORD <b64>` | direct response to `MIGRATE PREPARE` | — |
| `ACCOUNT-MOVED <old-ULID>@A <account@B>` | `@record=<b64>`; fan-out at COMMIT to every bridge holding state about the account; also the redirect response for account queries during retention. Clients use it to display-rewrite old attributions (wire history unchanged). | `@record=<b64> ACCOUNT-MOVED 01J8ACCADA@a.example ada@b.example` |
| `GROUP-MOVED <&id> <successor>` | `@fence=<b64>`, `@final=<msgid>`, `@reason=account-move\|manual`; audience = member networks | — |
| `NS-MOVED <ns-id> <successor>` | `@fence=<b64>`, `@final=<msgid>`, `@vanity=`; **root-signed** fence; audience = manifest-sharers + members; **also the redirect response** for `FEDERATE`/`BRIDGE REQUEST`/invites/`FENCE?` during retention | `@fence=<b64>;vanity=gaming NS-MOVED 01J8NSGAMING b.example` |
| `MIGRATION-STATUS <target> <status>` | `@detail=`; one per bundle target during IMPORT; status ∈ `rebound\|queued\|refused\|netblocked\|no-such\|unreachable` | — |
| `MIGRATION-DONE <ok\|partial\|aborted>` | `@queued=<n>` remaining retry-queue size | — |

Also add to §7.1: the `ERR MOVED` behavior on `AUTH` (cross-ref §8 + §6.10.4).

---

## 5. Ripple edits across existing sections (apply each)

- **§1 Design Decisions table:** add row `Migration | Co-signed move record + scope fences; groups follow creator, namespaces move by root signature, DMs home on the non-moving party; old home serves signed redirects for a configurable ≥1-month retention`.
- **§9.0 invariants:** add (next free numbers; 5–7 stay reserved):
    - **14 — No silent move.** Every account move and scope re-home is announced (`*-MOVED`) to its full pre-move audience; a voided fence is announced the same way.
    - **15 — Moved identity never reissues; labels recycle cold.** A moved account/ns/channel/role **ULID** is never reissued. The **vanity/handle** becomes re-registerable only after `redirect-retention + vanity-release-cooldown`, and a re-registration mints a fresh ULID that inherits nothing — no grant, token, membership, or federation pin can resolve to the newcomer.
    - **16 — The fence is the only origin switch.** A scope's minting authority changes only via a verified fence; peers MUST fetch an unknown fence (`FENCE?`) before accepting successor-origin events (`@after` gap rule).
    - **17 — Redirects respect the visibility gate.** A tombstone redirect is served only to callers entitled to the pre-move object; all others receive `NO-SUCH-TARGET` (invariant 1 extension).
- **§9.5 DMs:** replace the "Cross-network note (honest)" — cross-network DMs are now specified as two-member home-authoritative conversations, home = the non-moving/original party's network; migration interplay per §6.10.2(4). Mark §18 #7 **resolved** with a pointer.
- **§10.1:** clarify the rule per invariant 15 — the account **ULID** is never reused; the **handle** releases after `redirect-retention + vanity-release-cooldown` and a re-registered handle gets a fresh ULID with no inherited authority (the existing "never inherits stale authority" sentence already says this — extend it to name the release timing).
- **§6.5 / §11.10 invites:** state that a scope fence implies `INVITE REVOKE-ALL` (all pre-move invites for the scope die at the fence); dead-invite redemption stays the uniform `NO-SUCH-TARGET`.
- **§10.4:** note that `account@network` subjects re-key on a verified move record (grant records re-key, tokens re-mint — same mechanism as the v0.13 migration note already in that section); moderation-cap rebinding is config-gated.
- **§11.6 NETBLOCK:** add a fifth effect: move records and fences from a blocked network are refused (and a blocked successor is refused as a move destination).
- **§11.14/§11.15 federation references:** add `MIGRATE CLAIM` (S→H user command), `FENCE?` (↔), and the three `*-MOVED` events (H→audience) rows.
- **§6.9 SYNC / §7.9 CHANSYNC:** a re-homed scope MUST produce `CHANSYNC <chan> reset` for any cursor predating the fence — B's modseq shares nothing with A's. State this in §6.10.3 and cross-ref.
- **§18 Open questions:** remove/resolve #7; add: (a) dead-origin migration — a degraded rung on cached-attestation device keys is TOFU-grade and raceable by a hostile A; deferred, with AT-protocol-style recovery-key hierarchy noted as the known solution shape; (b) redirect-retention legal minimums by jurisdiction; (c) namespace-move media re-mirroring economics (blob store transfer vs lazy `MIRROR` pulls — RECOMMENDED lazy). Also mark **#3 (squatting cooldown) resolved** by the `vanity-release-cooldown` rule (invariant 15) — same cooldown applies after `NS DELETE`.
- **§13 Media note:** attachments referenced by moved-scope history keep their `weft-media://<A>/<hash>` origin; B mirrors lazily via §11.8 on demand; A's blobs live at least as long as the tombstone serves history.

---

## 6. Appendix A entry (required)

Add `=== v0.14 — account & scope migration` following the established format ("what changed, why, where it lives"): the two primitives + tombstone redirects; the mig-key decision (device keys are login-only, password accounts must migrate); the DM homing rule resolving §18 #7; groups-follow-creator; root-signed `NS MOVE` (owner-sovereign, A cannot block the re-pin); the honest limits — history authorship immutable, e2ee never transfers (invariant 8 reuse), hostile-A unpreventable within the §11.11 trust model, dead-origin deferred. Bump the version chain line at the top of Appendix A.

## 7. Appendix B notes (required)

- Config keys: `redirect-retention` (default `6mo`, floor `1mo`), `vanity-release-cooldown` (default `3mo`, applied after retention; also reused for the post-`NS DELETE` cooldown), `moderation-cap-rebind` (per-ns, default `off`), fence expiry constant (`FENCE_EXPIRY_SECS`, default 72 h), retry-queue horizon (`MIGRATION_RETRY_HORIZON`, 30 d).
- Vanity release is a lazy check at registration time (compare `moved-at + retention + cooldown` on the tombstone row), not a scheduled job; the tombstone row therefore outlives the redirect service by the cooldown.
- Storage: tombstone table (old identifier → record/fence hash + expiry), fence store, migration retry queue; note the migration that adds them.
- The mig-key is held client-side in session memory only; the server stores only the record.

## 8. Consistency checklist before finishing

- [ ] Every new command has Cap, result events, error codes, and a grammar-true example (tags before verb).
- [ ] Every new event appears in §7 (or §11.15) and in the emitting command's `→` column.
- [ ] `MOVED` added to §8 with the visibility-gate note; no other error semantics changed.
- [ ] Invariants 14–17 cited from the sections that enforce them (the doc's "cited by number" convention).
- [ ] Anti-enumeration statement present on: `FENCE?`, `MIGRATE CLAIM`, all redirect surfaces, and dead-invite redemption (uniform `NO-SUCH-TARGET`, no moved-disclosure).
- [ ] Vanity-release timing stated in §5.3, §10.1, and invariant 15 consistently (retention + cooldown; fresh ULID; lock option preserved).
- [ ] ULID/vanity discipline: all new wire forms use ULIDs; vanities only at human entry points.
- [ ] TOC/anchors valid; §18 renumbering (if any) doesn't break existing `<<>>` refs.