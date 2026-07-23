# Identity, caps, and federated roles — design plan

Status: **design, for approval** (2026-07-08). Two intertwined asks that both
reshape the capability **subject** model, so one plan:

1. **Caps keyed by a stable per-account ULID**, not the mutable handle — and the
   ULID lives *in the signed token* (the "full" option chosen).
2. **Federation users can hold roles** on a network they're not a member of —
   with **real, enforced authority**, delivered by a **federation session**
   (§7): a bridge-tunneled session under **homeserver authority** (trust F, like
   IRC).

Both change the same thing — *what a capability is granted to* — so they're
designed together. This is the model invariant 4 rests on, so it's plan-first.

Settled decisions (this thread): full authority scope · command-over-bridge ·
persistent full session · **homeserver authority (trust F), no device-signing** ·
deny v1 tokens · drop foreign grants on NETBLOCK · events via the mirror ·
**NETBLOCK is the sole network backstop (no per-scope ACL)**.

## 1. The problem with today's model

- Caps/grants key by the **account name** (a `String`). `GrantRecord.subject`
  and the token `Subject::Account(name)` both hold the handle.
- Names are currently permanent (no `DELETE ACCOUNT`), so name-reuse inheritance
  is **latent, not live** — but the shape is wrong: the moment deletion/rename
  lands, a re-registered handle inherits stale grants.
- Foreign users have **no local presence at all** — they can't be granted
  anything, so a partner network's moderator shows up as a plain bridged sender.

## 2. The unified subject model

A capability (grant record + signed token) is issued to exactly one of:

| Subject | Identifier | Used for |
|---|---|---|
| **Device key** | Ed25519 pubkey | `Subject::Key` — device-bound tokens (unchanged) |
| **Local account** | its **ULID** (immutable, minted at register) | the stable key for a local user's caps/roles |
| **Foreign user** | **`account@network`** | a federated user's caps/roles (F owns her ULID; H can't key on it) |
| **Unbound** | — | offline/invite tokens (unchanged) |

Key asymmetry: **local users key by ULID, foreign users key by
`account@network`.** H cannot use a ULID for `alice@F` — F mints and owns it, and
it never crosses the bridge (caps don't federate). `account@network` is the most
stable thing H can name her by, and it's already how bridged events identify her
(origin authority). ULIDs stay **network-local** (§4).

## 3. Account ULID

- `AccountRecord` gains an immutable **`ulid`**, minted at `register` (which now
  returns / exposes it). Names remain the login + display handle; the ULID is the
  internal cap key, never shown to users.
- **Resolution** happens at auth, not per-check: the session resolves its
  account → ULID once at login and caches it, so `account_has_cap` and the
  GRANT/REVOKE/ROLE paths key by the cached ULID with no extra lookup on the hot
  path.
- **Uniqueness is per-network**, DB-enforced (`weft_accounts.ulid UNIQUE`). 80
  bits of per-ms randomness makes collision negligible; the constraint is the
  guarantee — same belt-and-braces as the existing name uniqueness.
- **Privacy note:** a ULID embeds its creation timestamp, leaking account age to
  anyone who can read a grant record — an operator/admin surface only (grants
  aren't federated). Acceptable; stated for honesty.

## 4. Uniqueness across federation (settled)

ULIDs are **not** globally unique and must not be relied on to be. Tokens are
**per-network authority** — signed by the minting network's key, checked only
against that network's grant store — so an F-ULID and an H-ULID live in separate
authority domains and are never compared. The global identity stays
`account@network` (the DNS-unique network scopes everything, exactly as it does
for names). Foreign subjects therefore carry `account@network`, never a ULID.

## 5. Token format change (signed CBOR)

`Subject` becomes: `Key(pubkey) | Account(ulid) | Foreign(account@network) |
Unbound`. This is a change to the **deterministic-CBOR encode-before-sign**, so:

- Bump the token version (`VERSION 1 → 2`); v1 (name-subject) tokens are refused
  outright — `from_wire` already rejects `version != VERSION`.
- Subject tags: `0=Key`, `1=Account` (16 ULID bytes), `2=Unbound`, add
  **`3=Foreign`** (account@network).
- **`Account` is fully typed `Account(ulid::Ulid)`** (final, after a brief String
  detour): the `ulid` crate is pure, so it's L0-legal, and it makes it impossible
  to ever put a non-ULID in the token. Encoding is the raw 16 bytes; decode
  length-checks. Choosing this pulled the core name→ULID resolution forward (P3),
  since weft-core can no longer hand a name — the phases merged.
- Round-trip + delegation-chain tests for every subject kind, and a
  hand-crafted-v1-denied test (weft-crypto).

## 6. Roles for federation users

`ROLE ASSIGN <scope> <account@network> :<Role>` and `GRANT <account@network> …`
become valid; the store's role-membership + grants accept a foreign subject.
A role is three things; for a *foreign* subject they split:

- **Membership + display — real now.** `alice@test.example` wears the role: shown
  with its color + badge wherever her bridged messages appear on H. Recognizing a
  partner network's moderators is the immediate, safe win.
- **Enforced authority — via a federation session (§7).** Foreign users have no
  session on H, so we give them one: a **federation session** tunnels alice's
  commands over the bridge and H enforces them against its own grant store. Full
  authority — moderation, posting, channel admin, grant delegation. See §7.

Both halves ship together: recognition falls out of the same membership/grant
records the session enforces against.

## 7. Federation sessions + homeserver authority (enforcement)

**Authority model — homeserver authority, like IRC.** [DECIDED] H trusts **F**,
not alice's device. F network-authenticates the bridge (its signing key at
`AUTH BRIDGE`), asserts `alice@F`'s identity, and relays her commands; H believes
it — exactly as a linked IRC server speaks for its users, and as a Matrix
homeserver signs for `@alice:hs`. Rationale: **F is alice's identity provider** —
it can reset her password and enroll devices on her behalf — so per-device
command signing is theater against a malicious F (F can always mint a compliant
device). The real trust boundary is the network. Consequence, stated plainly: **F
can wield any authority H grants to any F-user.** That's the IRC/Matrix bargain;
it's opt-in per grant and revocable (below).

Bounded where IRC isn't: trust is **one hop** (no transitive spanning tree),
**scoped** to manifest'd namespaces, and **revocable** — `NETBLOCK` severs the
whole relationship. (A per-scope network ACL was considered and **declined**;
`NETBLOCK` is the sole network-level backstop.)

**No device-signing.** The earlier "device-signed" direction is dropped; the
per-device-attestation-bridging prerequisite it implied is **no longer needed**.

**Delivery — command-over-bridge, fully server-to-server.** [DECIDED] alice's
client only ever talks to F (client-server); F multiplexes her session as frames
over the *existing* authenticated F↔H bridge. Every cross-network byte is
server-to-server — **alice never dials H, so H never learns her IP, only F's
server address** (the Matrix property: a client touches only its own homeserver).
The single bridge per server-pair carries both the event mirror and all command
sessions; **no per-user connection to H exists.** IP non-exposure — not just
simplicity — is why this beats a direct foreign session.

**Structure — a federation session is a bridge-tunneled `ControlStream`.** The
`ControlStream` port already lets `run_session` drive the full FSM/actors/store
over any transport (QUIC, WS, IRC gateway). A federation session is the same
seam: a `ControlStream` whose bytes are framed over the bridge. So `alice@F`
becomes a first-class session on H — `NEGOTIATING → UNAUTHED → READY`, where AUTH
is simply **F's vouching** (the bridge is already network-authed as F) — and then
**every existing enforcement path applies unchanged**: `account_has_cap(alice@F,
…)` hits H's grant store on the ordinary local fast-path. No parallel authority
logic, no remote-command RPC — she just *is* a session.

**Scope — full authority.** [DECIDED] The session grants the entire local-user
surface: moderation (mute/ban/kick), posting (incl. restricted), channel
management (create/meta/delete), and re-delegating grants — whatever H's grants to
`alice@F` cover. A foreign admin ≈ a local admin.

**Lifetime — persistent full session.** [DECIDED] While alice is an active member
of the bridged namespace, F holds her session open on H: H streams her events
directly through it and her commands/responses flow live. (Cost acknowledged:
per-user session state + streams over the bridge, and event flow that overlaps
the namespace mirror — dedup/routing is a P5 design point.)

**Wire.** New bridge-multiplexing frames tag per-user sub-sessions, e.g.
`FSESSION <fsid> OPEN <account>` / `FSESSION <fsid> CMD <line>` /
`FSESSION <fsid> EVENT <line>` / `FSESSION <fsid> CLOSE`, carried inside the
authenticated bridge session. F opens/closes; H attributes every `CMD` on `fsid`
to the vouched account.

## 8. Security model

- Granting a foreign user a role/caps is an explicit **operator/ns-owner action**
  (`grant:<cap>`-gated) — opt-in trust of a partner network's user. Nothing
  auto-grants; recognition-only (no caps) is the safe default.
- **Homeserver authority**: H verifies **F** (network key on the bridge), trusts
  F's assertion of `alice@F`, and enforces against **H's own grant store**. No
  foreign network can forge a grant — grants are H-owned. F can only assert *who*
  is acting, and only for *its own* users (origin authority, invariant 2).
- **Revocation** (two rungs): revoke alice's grant (narrowest) → `NETBLOCK` F
  (severs everything **and hard-drops** every foreign grant scoped to F). No
  per-scope ACL — Matrix-style defederation without room ACLs.
- **IP non-exposure (MUST)**: all cross-network traffic — events *and* command
  sessions — is server-to-server over the one bridge. A remote user never
  connects to H; H only ever sees F's server address. No verb, session, or link
  may expose a user's IP to a foreign network.
- Unchanged invariants: `e2ee` never bridges (8); DMs never bridge (§9.5);
  capability checks precede side effects (4) — now for foreign subjects too.

## 9. Phases (each shippable, each keeps the suite green)

- **P1 ✅ (2026-07-08) — account ULID (store).** `AccountRecord.ulid`, `register`
  mints it (`weft_proto::Ulid`), `account_ulid(name)` accessor (race-safe lazy
  backfill for pre-existing PG rows), migration 0016 (nullable UNIQUE col), mem+PG
  shared contract (register→stable→unique→absent-for-unknown) — validated live on
  both backends. Names + downstream untouched.
- **P2 ✅ (2026-07-08) — token subject (crypto).** `Subject` widens with
  **`Foreign(account@network)`** (tag 3) and **`Account(ulid::Ulid)`** (fully
  typed, 16-byte encoding); **`VERSION 1 → 2`** denies every v1 token. Tests:
  ULID/foreign round-trip + verify, foreign-is-a-leaf-only, hand-crafted-v1-
  denied. 41 crypto tests.
- **P3 ✅ (2026-07-08) — grants key by ULID, end-to-end (merged P3 + the grant
  half of P4).** `ServerCtx::resolve_subject` maps a subject string → device key
  / local **ULID** / foreign `account@network`; `account_has_cap`, GRANT, REVOKE,
  ROLE UNASSIGN, and invite-redeem all key the grant store by it (never the
  handle); `Accounts::account_ulid` passthrough; `subject_from_str` removed.
  **Behavior change:** GRANT/DELEGATE to a nonexistent account → `NO-SUCH-TARGET`
  (no identity to key on). Migration 0017 (ULID-backfill + grant-subject rewrite,
  role *membership* left handle-keyed) — validated live on PG. Foreign-subject
  GRANT works (plumbing for federated roles). 95 core tests; mem+PG green.
- **P4 ✅ (2026-07-09) — foreign role membership.** Role subject widened
  `Account` → subject string across **proto** (`RoleAssign`/`RoleUnassign`/
  `RolesOf` + `RoleMember` event), **store** (`assign_role`/`unassign_role`/
  `roles_of`/`role_members`, mem+PG), and **core** (the three handlers +
  `on_role_unassign` revokes by `resolve_subject`). `ROLE ASSIGN <account@network>`
  now records membership + grants the bundle to the foreign subject; `ROLES-OF`
  reflects it (recognition). Client already assigns via a free-text subject
  (placeholder now hints `account@network`). Tests: core foreign-assign +
  ROLES-OF, store contract with a federated holder (live PG). 96 core tests.
  **Deferred to P6:** coloring/badging a *foreign message author* by their role
  in the timeline — needs a foreign-role fetch/cache, best built with the
  federation client work.
- **P5 — federation sessions (enforced authority).** Steps 1–4 ✅ (2026-07-09):
  - **Frames**: `FSESSION <fsid> OPEN|CMD|REPLY|CLOSE` — carries the request/
    response pair only (`REPLY` renamed from `EVENT` to make it honest: broadcast
    events ride the mirror, §10.3, so the session never subscribes).
  - **`Actor`** (`Local(Account) | Foreign(account@network)`) + `actor_has_cap`/
    `actor_can_grant`/`actor_store_key`; `account_*` are thin local wrappers, so
    the 41 existing handlers are untouched. Operator/owner authority is local-only.
  - **Tunnel**: `TunnelStream` (impl `ControlStream`, mpsc-backed) → `run_federated_session`
    enters `State::Federated { user }`; the bridge demuxes `FSESSION` (`OPEN`
    spawns, `CMD` feeds, `CLOSE` ends) and drains a `fed_out` queue so all socket
    writes stay serialized through the one run loop (no race, IP non-exposure —
    she never connects to H).
  - **Moderation actor-aware**: `on_moderate`/`on_kick` take `Actor`; `Event::Moderated.by`
    widened to a subject string (attributes a foreign moderator). `on_federated`
    dispatch routes mute/ban/kick/ping; the rest answer `UNSUPPORTED`.
  - **Conformance** (weft-core): a federated user granted `mute` on H wields it
    over the bridge (`FSESSION OPEN`+`CMD` → `MODERATED` reply, `by=alice@peer`);
    without the grant → `CAP-REQUIRED`.
  - **Step 5 ✅ (2026-07-09) — full authority, nothing deferred.** Design
    refinement: *content* (posting/edit/delete/react) rides the **mirror**
    (F-origin, one-hop forward — never the session, or origin authority breaks);
    the federation session carries **control/admin** actions only. Every admin
    verb now takes `Actor` and is routed in `on_federated`: moderation
    (mute/ban/kick), **GRANT/REVOKE**, **channel admin** (create/policy/meta/
    delete), **invites** (mint/revoke), **role assign/unassign**, **ns admin**
    (meta/visibility/delete/delegate — the shared `ns_admin_gate` is now
    actor-aware), and **reports** (list/resolve). Two attribution fields widened
    to a subject string so a foreign admin is named honestly: `Moderated.by`
    (P5 step 4) and **`ReportResolution.resolved_by`** (store type + PG, live-
    validated). `account_can_grant` removed (folded into `actor_can_grant`).
    Conformance (weft-core): federated **mute**, **grant-delegation**, **channel
    create**, **ns-meta edit** — each over the real bridge, enforced as
    `account@network`. **P5 done.**
- **P6 ✅ (2026-07-09) — client + spec.**
  - **Spec** (normative, `weft-protocol-spec.md`): §10.1 account ULID; §10.4
    token subject v2 (`pubkey | account-ULID | account@network | UNBOUND`, v1
    denied) + foreign role holders; **new §11.11** federation sessions &
    homeserver authority (FSESSION, content-via-mirror/control-via-session, IP
    non-exposure, no device-signing rationale); §6.6 `FSESSION` command row;
    Appendix A amendment entry.
  - **Client**: the MODERATED system line marks a federated moderator explicitly
    (`… by alice@peer.example (via federation)`) — the "acting on H via F"
    affordance; and a federated author's roles held on this network render as
    role-color chips in the timeline (lazy `ROLES-OF` fetch keyed
    `account@network`, deduped) — foreign role-holder recognition.

**Plan complete — P1–P6 all shipped.**

## 10. Decisions (resolved)

1. **v1 tokens — denied** immediately on upgrade; no grace window. Re-grant to
   reissue (grants are cheap; dual signed-token formats are a footgun).
2. **Foreign grants on NETBLOCK — hard-dropped.** Grants scoped to the blocked
   network are deleted, not left dormant.
3. **Persistent-session event flow — events ride the namespace mirror**; the
   session carries only commands + responses (incl. her own action acks). Matches
   Matrix: a remote user reads all room events from her own homeserver's replica
   (the mirror), never a direct stream; her actions are authored + federated by
   her homeserver. No duplication.
4. **Per-scope network ACL — not built.** `NETBLOCK` is the sole network-level
   backstop (Matrix-style defederation, without per-room ACLs). Revocation is two
   rungs: revoke the grant, or NETBLOCK the network.
