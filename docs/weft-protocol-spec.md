# WEFT Protocol — Specification v0.10 (Consolidated Edition)

*Fully self-contained; supersedes v0.9. New in v0.10: message reporting (§6.7, §11.9, retention holds in §12.1). v0.9 added the namespace recovery ladder (§2.4) and message compaction (§12.1). A client can be written from §1–§10; a server additionally requires §11–§17.*

**WEFT** (working name): a federated chat protocol combining IRC's operational simplicity with Discord's feature semantics. Design goals: small self-host footprint, sovereign networks, explicit consent for every federation act, privacy properties enforced by construction, and a control plane debuggable with netcat.

---

## 1. Design Decisions (locked)

| Axis | Decision |
|---|---|
| Federation | Independent sovereign networks + opt-in scoped bridging (channel / namespace / network); signed manifests; **non-transitive** |
| History | Per-channel retention; peer backfill gated by manifest `history` flag; **compacted materialized form on the wire** |
| Wire format | Text control plane + binary data plane |
| Identity | Network account + portable Ed25519 keypair attestation |
| Permissions | Scoped capability tokens (signed CBOR, delegable, short-lived) — no role tables *in enforcement*. Roles (§6.5.1) are named, colored *bundles* of these tokens with explicit membership: assigning grants tokens, every check stays a pure token check |
| Voice/video calls | Companion protocol (WEFT-RT); signaling in core |
| E2EE | Per-channel opt-in, expressed as a retention mode (MLS) |
| Transport | QUIC native, WebSocket fallback |
| Message features | Edits, deletes, reactions, threads, replies — all core |
| Communities | Optional user-owned namespaces; creation per network config (open-with-quota / cap-gated) |
| Visibility | Public / Unlisted / Private; anti-enumeration normative |
| Bridge growth | Manifest snapshot; explicit signed additions |
| Defederation | Network-wide `NETBLOCK`, name-keyed, severs manifests |
| Media | Native, content-addressed (BLAKE3), data-plane; mirrored across bridges |
| Legacy access | IRC gateway extension (WEFT-IRC) |
| DMs | Same-network in v1 |
| Presence | Same-network only; never bridged |
| Acks | Labeled responses; sender echo is the ack |
| **NS recovery** | **Three-rung ladder: root transfer → social quorum (7 d delay, announced + root-cancellable) → operator takeover (immediate, announced, permanently audit-marked)** |
| **Compaction** | **Live = event-sourced; storage & HISTORY = compacted after audit window (default 24 h)** |
| **Reporting** | **REPORT to reporter's home network; ns/net routing; retention holds; honest e2ee/ephemeral limits** |

---

## 2. Model & Naming

### 2.1 Entities
- **Network**: a sovereign deployment identified by a DNS name (`hda.example`). Owns accounts, hosts namespaces and channels, publishes its signing key, is the abuse-accountable party. **No global state**: nothing leaves a network except through an explicitly agreed bridge manifest.
- **Namespace** (optional): a named channel bundle — the Discord-"server" analog — **created and owned by a user**. At `NS CREATE` a dedicated **namespace root key** is generated client-side and held by the owner; all roles, moderator tokens, channel policies, and invites chain from it. The operator hosts but does not administer; the network key outranks a namespace root **only** for abuse handling (freeze/delete) and rung 3 recovery (§2.4) — it can never silently mint membership or read `e2ee` content. A network with only flat channels never declares a namespace and is fully conformant.
- **Channel**: one home network, optionally inside one namespace. `#general` or `#gaming/general` — one level, no nesting.
- **Account**: `user@network.tld`, registered and recoverable at the home network.

### 2.2 Namespace creation & visibility
Creation per network config: `open` (any account, quota default **10**, rate-limited) or `gated` (`ns-create` cap).

| Tier | Directory | Join | Existence disclosure |
|---|---|---|---|
| `public` | Listed in `DISCOVER` | Open, or invite (ns choice) | Anyone |
| `unlisted` | Not listed | Invite required | Invite holders only |
| `private` | Not listed | Invite required | **Denied** — indistinguishable from nonexistent |

**Anti-enumeration (normative):** "private thing you're not in" MUST be indistinguishable from "does not exist" — same code (`NO-SUCH-TARGET`, §8), same timing envelope. Covers view-gated channels, expired/foreign msgids, dead invites.

### 2.3 Normalization (normative)
- Machine identifiers: **lowercase ASCII**. Accounts `[a-z0-9-_.]{1,64}`; ns/channel segments `[a-z0-9-_]+`; channels ≤200 B incl. `#` and namespace.
- Display strings: UTF-8, NFC on ingest. `\r`/`\n` forbidden **raw** in lines but representable in the **trailing** via the §4 escape table (`\r`→`\r`, `\n`→`\n`, `\\`→`\\`), so a message body may be multi-line — it is escaped on serialize and unescaped on parse, never reaching the transport as a raw break. Display names ≤128 B; topics ≤1024 B.

### 2.4 Namespace recovery (new)

Failure mode addressed: the namespace root key is lost (device loss, owner death, departure) and the community would otherwise be permanently ownerless — plus, at rung 3, the case where the owner is present but is themselves the abuse. Recovery is a **ladder** — each rung louder and more auditable than the last.

All **delayed** rungs share three properties: a **mandatory public delay**, a **mandatory announcement** (`NS-META` event with `recovery=` fields to all members), and **cancellability by the current root** during the window (a live root can always veto — this defeats coerced or hostile recovery). Rung 3 is deliberately **not** a delayed rung: it keeps the announcement, drops the delay and the veto, and compensates with a permanent audit mark (see below). The announcement is the one property every rung shares, so **no rung is ever silent**.

**Rung 1 — Transfer (no delay).** The root signs `NS TRANSFER`. Normal succession; nothing new.

**Rung 2 — Social recovery (7-day delay, RECOMMENDED default).**
- The owner MAY designate a recovery set at any time: `NS RECOVERY SET <name> <m> <key1,key2,...>` — an M-of-N quorum of keys (typically trusted co-admins). Stored in signed ns metadata; members can see that a recovery set exists (not necessarily who).
- Recovery: quorum members co-sign a **rotation record** naming the new root key; any of them submits `NS RECOVER <name> <b64-rotation-record>`. The server verifies M valid signatures from the set, then starts the delay window.
- During the window: `NS-META` announces `recovery=pending;recovery-eta=<ts>;recovery-rung=2` to all members. The current root may cancel with `NS RECOVERY CANCEL <name>` (one signature beats the quorum — the point is that a *live* owner always wins).
- At expiry the rotation applies: new root key takes over; all tokens chained to the old root expire naturally (short-lived anyway); the rotation is permanently recorded in ns metadata (`root-history`).

**Rung 3 — Operator takeover (no delay).**
- The operator (network signing key) initiates `NS RECOVER` with an operator-signed rotation record. Available whenever the operator judges it necessary — most often *moderation seizure* of a namespace whose owner is the abuse, and secondarily as the last-resort recovery path when no recovery set is configured or the quorum is unreachable.
- **It applies immediately.** Unlike rungs 1–2 this rung has **no delay window**, and therefore no pending state and nothing to cancel: the two-of-three shared properties above (delay, root-cancellability) deliberately do **not** hold here. Earlier drafts specified a 30-day window; that made the rung unusable for the job it exists to do, because a moderator cannot wait a month and because the veto the window grants would be exercised by exactly the party being removed. See Appendix A for the amendment and its reasoning.
- **The announcement and the audit mark do hold, and they carry the whole accountability weight.** The rotation is announced (`NS-META`) and is **permanently marked operator-initiated** in `root-history` — auditable by every member and by bridge peers forever. An operator who abuses this pays in visible reputation, which is the honest limit of what protocol can enforce against the party hosting the data.
- What the zero delay removes is the *window*, never the *authorization*: the rotation record MUST still verify against the network signing key. A rotation signed by anyone else is `FORBIDDEN`, as before.

**E2EE caveat (normative):** recovery restores *administration* — token minting, policy, membership. It NEVER restores `e2ee` history: MLS keys live on member devices, the server holds ciphertext, and a recovered root joins encrypted channels as a fresh MLS member with no access to prior content. Host-blind means host-blind, including from recovery.

**Bridge interaction:** a root rotation is announced to bridge peers via a manifest metadata update; peers re-validate future manifest amendments against the new root. A peer MAY be configured to auto-suspend (not sever) bridges into a namespace during a pending rung-3 recovery.

---

## 3. Transport

### 3.1 QUIC (native)
ALPN `weft/1`. **Stream 0** (bidi): control plane, UTF-8 newline-delimited lines. **Uni streams**: data plane (media, bulk sync). **Datagrams**: voice (WEFT-RT).

### 3.2 WebSocket fallback
Single WSS connection. Text frames = control lines; binary frames = data plane with a 4-byte virtual stream ID prefix. Voice over WS best-effort; prefer QUIC.

### 3.3 Session lifecycle
```
open → NEGOTIATING --HELLO/WELCOME--> UNAUTHED --AUTH ok--> READY --QUIT/error--> CLOSED
```
`NEGOTIATING`: only `HELLO`. `UNAUTHED`: only `AUTH`, `REGISTER`, `PING`, `QUIT`. Else `ERR NOT-AUTHED`. Idle pre-auth sessions closed after 30 s (RECOMMENDED).

### 3.4 Keepalive
`PING [token]` → `PONG [token]` mandatory. RECOMMENDED 10 s interval (matching contemporary chat clients), 2 missed = dead. QUIC keepalive may substitute for sending, not for answering.

### 3.5 Labeled responses (normative)
Any command MAY carry `label=<opaque ≤64 B>`. Every **direct** response — success event, data page, `ERR` — echoes it; broadcast copies never do. Libraries SHOULD label everything; this is request correlation and the ack foundation (§9.2).

### 3.6 HELLO
```
C: HELLO weft/1
S: @features=media,backfill,voice,irc-gw WELCOME hda.example :Willkommen
```
`features=`: `media`, `voice`, `e2ee`, `backfill`, `irc-gw`, `presence`. Unknown flags ignored. Version mismatch: `ERR UNSUPPORTED`, close.

---

## 4. Control-Plane Line Grammar

```
@tag1=value;tag2;tag3=value VERB param1 param2 :trailing free text
```
- Limits: line ≤ **8 KiB**; ≤15 middle params; ≤32 tags; tag key ≤64 B (`[a-z0-9./-]+`); unescaped value ≤1024 B.
- Verbs `[A-Z0-9-]+`. **Unknown verbs ignored by servers; unknown events ignored by clients.**
- Middle params: no spaces, no leading `:`. Only trailing (after ` :`) may contain spaces or be empty (empty trailing = empty body, meaningful).
- Tag escaping: `; → \:`, space `→ \s`, CR `→ \r`, LF `→ \n`, `\ → \\`; unknown escapes drop the backslash; dangling backslash is an error.
- **Lenient-in, strict-out**: serializers MUST refuse to emit anything their own parser rejects.
- Oversized payloads → data plane via `STREAM`.

---

## 5. Identifiers & Core Types

### 5.1 Message IDs
`msgid = <origin-network>/<ULID>`. Lexically sortable, timestamp-embedded. Origin-scoped: no bridge collisions; edit/delete authority verifiable. ULIDs assigned **only** by the origin channel actor; actor inbox order = channel total order.

### 5.2 Retention policies
`ephemeral | retained:<n>(d|h|s) | permanent | e2ee` (n>0).

| Mode | Behavior |
|---|---|
| `ephemeral` | relay only |
| `retained:<dur>` | stored, purged after |
| `permanent` | stored indefinitely |
| `e2ee` | ciphertext blobs only; server-readable-plaintext **unrepresentable** |

Strictest-policy negotiation (bridges): `e2ee` > `ephemeral` > `retained(shorter)` > `retained(longer)` > `permanent`. Policy visible before members speak (`POLICY` on join).

---

## 6. Command Reference

Every command accepts a `label` tag (§3.5); the direct response — including `ERR` — echoes it. Each subsection is tagged with its scope: **S**ession · **N**etwork · **NS** namespace · **C** channel · **F** federation/operator. In the tables, the **Cap** column is the required capability (§10.4) — `—` means none — and **→** lists the success event(s) and notable error codes. `\|` separates alternatives.

### 6.1 Session & identity (S/N)

| Command | Syntax | Cap | → Result / notes |
|---|---|---|---|
| `HELLO` | `HELLO <version>` | — | Negotiates the protocol (§3.6). |
| `REGISTER` | `REGISTER <account> :<password>` | config | Password ≥ 12 B; needs `registration: open` else `FORBIDDEN`. Registration doubles as auth. → `WELCOME` \| `CONFLICT` \| `POLICY`. |
| `AUTH PASSWORD` | `AUTH PASSWORD <account> :<password>` | — | → `WELCOME` \| `AUTH-FAILED` (constant-time, uniform). |
| `AUTH KEY` | `AUTH KEY <account> <b64-ed25519-pubkey>` | — | Begins device-key challenge-response (flow below). → `CHALLENGE`. |
| `AUTH PROOF` | `AUTH PROOF <b64-sig>` | — | Answers the challenge, signing `nonce ‖ network-name`. → `@attestation=<b64> WELCOME` \| `AUTH-FAILED`. |
| `AUTH ENROLL` | `AUTH ENROLL <b64-pubkey>` | authed | Adds a device to the current account. → `@attestation=<b64> WELCOME`. |
| `QUIT` | `QUIT [:reason]` | — | Graceful close. |
| `PING` / `PONG` | `PING\|PONG [token]` | — | §3.4 keepalive; answering is mandatory. → `PONG`. |
| `PRESENCE` | `PRESENCE <online\|away\|dnd\|invisible>` | — | Same-network visibility only; never bridged; `invisible` renders offline. |

Device-key auth is a two-step challenge-response binding a device pubkey to the account; `nonce ‖ network-name` in the signed payload prevents cross-network replay:
```
C: AUTH KEY <account> <b64-ed25519-pubkey>
S: CHALLENGE <b64-nonce-32B>
C: AUTH PROOF <b64-sig(nonce ‖ network-name)>
S: @attestation=<b64> WELCOME hda.example
```

### 6.2 Namespace commands (NS)

Signed NS verbs (`TRANSFER`, `RECOVERY CANCEL`) carry the root signature in a `@sig=<b64>` tag; `NS CREATE` carries the new root pubkey in `@root=<b64>` (§2.4, §10.4).

| Command | Syntax | Cap | → Result / notes |
|---|---|---|---|
| `NS CREATE` | `NS CREATE <name> [public\|unlisted\|private]` | none (`open`+quota) / `ns-create` (`gated`) | Default `unlisted`. Client generates the namespace **root key**, submits its pubkey (recorded as delegation root). → `NS-META` \| `QUOTA` \| `CONFLICT` \| `FORBIDDEN`. |
| `NS META` | `NS META <name> <key> :<value>` | `ns-admin` | Keys: `title` / `description` / `icon` (free text); `categories` (comma-separated list — server-authoritative channel groups, Appendix A layout); `federation` (`open`\|`closed`, §11.10 — `open` requires `public` visibility, else `FORBIDDEN`). → `NS-META`. |
| `NS VISIBILITY` | `NS VISIBILITY <name> <tier>` | `ns-admin` | → `private` applies anti-enumeration immediately. → `NS-META`. |
| `NS DELEGATE` | `NS DELEGATE <name> <account\|pubkey> <cap>[,…]` | grant chain | Sugar for `GRANT` at `ns:` scope. → `TOKEN`. |
| `NS TRANSFER` | `NS TRANSFER <name> <account>` | root key | Rung-1 succession, root-signed. → `NS-META` (new owner). |
| `NS RECOVERY SET` | `NS RECOVERY SET <name> <m> <key1,key2,…>` | root | Designate the M-of-N quorum (§2.4). → `NS-META` (`recovery-set=yes`). |
| `NS RECOVER` | `NS RECOVER <name> <b64-rotation-record>` | quorum / operator sig | Rung 2 (quorum) starts the 7-day delay window; rung 3 (network-key signed) **applies immediately** — no window, no pending state, marked operator-initiated in `root-history`. → `NS-META` \| `FORBIDDEN` (bad sig) \| `CONFLICT` (a rung-2 recovery already pending). |
| `NS RECOVERY CANCEL` | `NS RECOVERY CANCEL <name>` | root key | Current root vetoes a pending recovery. |
| `NS DELETE` | `NS DELETE <name> <name>` | `ns-admin` / operator | Confirmed by repetition. |
| `NS JOIN` | `NS JOIN <name>` | membership | Auto-join every channel in the namespace the caller can see — view-gated and banned channels are skipped. → a `MEMBER` + `POLICY` per joined channel; no visible channel → `NO-SUCH-TARGET`. |
| `DISCOVER` | `DISCOVER [cursor]` | — | Public namespace directory. → `NS-META` per ns + `MORE <cursor>`. |
| `CHANNELS` | `CHANNELS <name>` | view | Ordered channel layout of a namespace (extension). → `CHANNEL-LAYOUT` per channel. |

### 6.3 Channel commands (C)

`CHANNEL CREATE`/`DELETE` are confirmed by repeating the name. **JOIN never auto-creates.**

| Command | Syntax | Cap | → Result / notes |
|---|---|---|---|
| `CHANNEL CREATE` | `CHANNEL CREATE <#chan> [policy]` | `chan-create` (`*`) / `ns-admin`\|`chan-create` (`ns:`) | Default policy `retained:90d`. → `POLICY`. |
| `CHANNEL POLICY` | `CHANNEL POLICY <#chan> <policy> [purge]` | `policy` | Tightening purges now; loosening applies to new events only; `e2ee` needs an empty channel or `purge`. → `POLICY`. |
| `CHANNEL META` | `CHANNEL META <#chan> <topic\|view-gated\|category\|position> :<value>` | `pin` / `ns-admin` | `category`/`position` = the layout extension. → `CHANMETA`. |
| `CHANNEL DELETE` | `CHANNEL DELETE <#chan> <#chan>` | `ns-admin` / operator | → `CHANMETA … deleted`. |
| `CHANNEL RENAME` | `CHANNEL RENAME <#old> <#new>` | `ns-admin` / operator | Change a channel's identity within its namespace; server re-keys every scoped record (grants, membership, roles, holds, pins, history). → `CHANNEL-RENAMED <#old> <#new>` (broadcast to members + labeled to actor). |
| `JOIN` | `JOIN <#chan> [invite-ref]` | membership / invite | → `MEMBER` + `POLICY` + `count=` \| `NO-SUCH-TARGET` \| `BANNED`. |
| `PART` | `PART <#chan> [:reason]` | — | → `MEMBER … part`. |
| `MEMBERS` | `MEMBERS <#chan> [cursor]` | membership | Paginated; bridge peers see remote members only as they've appeared. |
| `TYPING` | `TYPING <#chan> <start\|stop>` | `send` | Never stored; rate-limited (1/3 s RECOMMENDED); bridged only under manifest `typing: yes`. |
| `MARK` | `MARK <#chan> <msgid>` | membership | Account-scoped read marker, synced via `MARKED`; survives `ephemeral`. |
| `UNREAD` | `UNREAD [<#chan>]` | membership | Request server-computed unread counts → one `UNREAD-COUNTS` per channel. No channel = every joined channel. Absent channel must be joined, else `NO-SUCH-TARGET`. |

### 6.4 Messaging (C)

| Command | Syntax | Cap | → Result / notes |
|---|---|---|---|
| `MSG` | `MSG <#chan\|@user> [:body]` + tags `fmt=md` `reply-to=` `thread=` `attach.N=` (≤10) | `send` (+`attach`) | Empty body legal iff attachments. **The echoed `MESSAGE` (with `msgid` + `label`) is the ack.** → `MESSAGE`; errors `CAP-REQUIRED` `TOO-LARGE` `THROTTLED` (`retry-after=`) `NO-SUCH-TARGET`. |
| `EDIT` | `EDIT <msgid> :<new>` | `edit-own` | No `edit-any` (deliberate). Honored only at the msgid's origin network; elsewhere `FORBIDDEN origin`. → `EDITED`. |
| `DELETE` | `DELETE <msgid>` | `delete-own` \| `delete-any` | Tombstone. → `DELETED`. |
| `REACT` / `UNREACT` | `REACT <msgid> <emoji>` | `react` | Unicode emoji ≤ 32 B; shortcodes travel **bare** (leading `:` collides with the §4 trailing marker — §18 #8). Idempotent. → `REACTION op=add\|remove` (live). |
| `HISTORY` | `HISTORY <target> [before=] [after=] [limit=≤500] [thread=]` | membership / acked manifest | `key=value` middle params, any order, unknown keys ignored; target = channel or `@user`. → `BATCH START` … **compacted** events (§12.1) … `BATCH END [truncated]`. `truncated` marks gaps — silence about them is forbidden. |
| `PIN` / `UNPIN` | `PIN <msgid>` | `pin` | Pin/unpin a message in its channel (resolved from the msgid). → `PINNED <#chan> <msgid> by=` / `UNPINNED <#chan> <msgid>` broadcast to members. |
| `PINS` | `PINS <#chan>` | membership | The pinned messages. → `BATCH START` … `MESSAGE` per pin … `BATCH END`. |
| `SEARCH` | `SEARCH <#chan> :<query>` | membership | Message search in a channel. → `BATCH START` … `MESSAGE` per match (newest-first, ≤50) … `BATCH END`. |
| `STREAM` | `STREAM OFFER <media\|backfill> <mime> <bytes>` | — | → `STREAM ACCEPT <token>` → data-plane transfer. HISTORY switches to STREAM above ~200 events (RECOMMENDED). |

### 6.5 Capabilities & invites (§10.4)

| Command | Syntax | Cap | → Result / notes |
|---|---|---|---|
| `GRANT` | `GRANT <account\|pubkey> <scope> <cap>[,…] [expiry=<s>]` | `grant:<cap>` at ≥ scope | Scope `<#chan>` \| `ns:<name>` \| `*`; the chain rule is cryptographic. → `TOKEN`. |
| `REVOKE` | `REVOKE <account\|pubkey> <scope> [caps=<list>] [epoch]` | grant chain | Stops refresh; a bare `epoch` number bumps the scope revocation epoch. → `TOKEN` (remaining caps). |
| `INVITE MINT` | `INVITE MINT <scope> [max-uses=] [expiry=]` | `invite` | → `INVITED` (`@token=`, link `weft://<net>/<ns>/i/<b64>` — the namespace is embedded so a *foreign* redeemer can auto-federate to it, §11.10; top-level channels have no `<ns>` and use `weft://<net>/i/<b64>`). |
| `INVITE REVOKE` | `INVITE REVOKE <invite-id>` | `invite` | Closes the counter; already-redeemed members unaffected. |
| `INVITE REVOKE-ALL` | `INVITE REVOKE-ALL <scope>` | `invite` | Bulk-closes every invite for the scope's namespace (`ns:<name>` + its `#<ns>/<chan>` scopes) in one shot. → `INVITED … invite-id=* max-uses=0` ack. Already-redeemed members unaffected. |
| `INVITE REDEEM` | `INVITE REDEEM <b64>` | — | Verifies chain + counter, mints a member token **bound to the redeemer's key**, auto-joins the default channel. Dead invites → `NO-SUCH-TARGET` (indistinct). |

Invite tokens are capability tokens with an **unbound subject**: one object serves single-use / expiring / vanity links — offline-verifiable authorization, never itself a membership credential.

#### 6.5.1 Roles — named capability-token bundles

A **role** is a named, colored bundle of capability tokens at a scope: `(scope, name, color, caps)`. Roles give clients human-readable, colored labels over §10.4 capabilities. **Enforcement stays purely token-based** — assigning a role grants exactly its `caps` as ordinary tokens, and every permission check is a pure capability-token check ("no role tables in the *enforcement* path"). **Membership, however, is explicit, not derived:** an account wears a role because it was *assigned* (recorded server-side, `ROLE ASSIGN` / `ROLE UNASSIGN`), never because its caps happen to be a superset of the bundle. Deriving membership from caps was rejected — it wrongly marks owners/operators (who hold every cap implicitly) as wearing every role, and can't distinguish a coincidental cap match from an intended assignment. The assignment record is metadata for *display and propagation*; it is never consulted for a permission decision.

| Command | Syntax | Cap | → Result / notes |
|---|---|---|---|
| `ROLE CREATE` | `ROLE CREATE <scope> <color> <cap>[,…] [hoist=0\|1] [pos=<n>] :<name>` | `ns-admin` at scope | Define/replace a role (upsert on `(scope, name)`). `color` is a display hint (e.g. `#e8b93d`); optional `hoist=` (Discord-style "display members separately in the member list") + `pos=` (sort position, ascending) are key=value middle params defaulting to `0`; `name` (may contain spaces) rides the trailing. → updated `ROLES` batch. |
| `ROLE REORDER` | `ROLE REORDER <scope> :<name1,name2,…>` | `ns-admin` at scope | Set each named role's `pos` to its index in the list. → updated `ROLES` batch. |
| `ROLE DELETE` | `ROLE DELETE <scope> :<name>` | `ns-admin` at scope | Remove a definition **and all its assignments**. Already-granted tokens are unaffected (revoke separately). → updated `ROLES` batch. |
| `ROLE RENAME` | `ROLE RENAME <scope> :<old>,<new>` | `ns-admin` at scope | Change a role's display name **in place**, carrying its definition *and every assignment* to the new name. Roles are keyed by name, so a client-side delete+create would silently drop membership — this is one server-side migration instead. Already-granted tokens need no migration: a role's authority is its `caps`, which are unchanged. Both names ride the trailing as a comma pair (the `ROLE REORDER` convention), so a role name may contain spaces but **not** a comma. Absent `<old>` → `NO-SUCH-TARGET`; an `<new>` that already names a live role → `POLICY` (merging two bundles is not a rename). → updated `ROLES` batch. |
| `ROLE ASSIGN` | `ROLE ASSIGN <scope> <account> :<name>` | `grant:<cap>` for each cap | Record membership + grant the role's tokens (identical authority + `TOKEN` path as `GRANT`). At a **namespace** scope also propagates channel role-permissions (below). |
| `ROLE UNASSIGN` | `ROLE UNASSIGN <scope> <account> :<name>` | `ns-admin` at scope | Drop membership + revoke the role's caps (bundle + its channel-role caps). → `ROLE-MEMBER`. |
| `ROLES` | `ROLES <scope>` | — (public) | → a `BATCH` of `ROLE <scope> <color> <caps> hoist=0\|1 pos=<n> :<name>` (definitions, position-ordered). |
| `ROLES-OF` | `ROLES-OF <scope> <account>` | — (public) | The roles an account is assigned at a scope → `ROLE-MEMBER <scope> <account> :<comma-names>`. |

The `ROLE` event carries a definition; the `ROLE-MEMBER` event carries an account's explicit assignments. Clients render pills from the intersection.

**Role channel-permissions.** A namespace role and a **channel role of the same name** compose to give the Discord "role has permission X in channel Y" override — without a rules engine. A role `Speaker` at `ns:s` carries the namespace-wide caps; a role `Speaker` at `#s/stage` (same name) carries that role's caps *for that channel only*. Both directions propagate through explicit membership: `ROLE ASSIGN ns:s <account> :Speaker` grants the namespace bundle **and** every same-named channel role's caps on `#s/*`; and **editing a channel role re-grants it to every current member of the namespace role immediately** (via the membership records) — so a newly-added channel permission reaches existing holders with no re-assignment. Enforcement stays token-based (§10.4): the namespace covers its channels, a channel covers only itself.

### 6.6 Federation & operator (F)

Bridge sessions authenticate with `AUTH BRIDGE` (§11.2). Every bridge change emits `MANIFEST` to affected members — mandatory (§11.5). The proposing side carries the signed manifest in a `@manifest=<b64>` tag.

| Command | Syntax | Cap | → Result / notes |
|---|---|---|---|
| `AUTH BRIDGE` | `AUTH BRIDGE <peer-network> <b64-pubkey>` | pinned / accept-any | Opens a bridge session — challenge-response as `AUTH KEY`, verified against the peer's network key (§11.2). |
| `BRIDGE PROPOSE` | `BRIDGE PROPOSE <scope> <peer> [history=from-epoch\|full] [media=mirror\|mirror-max:<B>\|none] [typing=yes\|no]` | ladder §11.3 | Snapshot manifest v1. → `MANIFEST`; errors `BLOCKED` `CAP-REQUIRED`. |
| `BRIDGE ACCEPT` | `BRIDGE ACCEPT <peer> <version>` | ladder | Live on mutual ack. |
| `BRIDGE ADD` | `BRIDGE ADD <peer> <#chan>` | ladder | v+1, requires re-ack before forwarding. |
| `BRIDGE REMOVE` | `BRIDGE REMOVE <peer> <#chan>` | ladder | v+1, unilateral, immediate. |
| `BRIDGE SEVER` | `BRIDGE SEVER <peer>` | ladder | Unilateral teardown. |
| `BRIDGE REQUEST` | `BRIDGE REQUEST <ns>` | bridge session | §11.10 — ask the peer to offer a manifest for one of *its* namespaces. → `BRIDGE PROPOSE` (its signed manifest, **`history=full`** so the joiner receives the namespace's existing scrollback, §11.7) iff the namespace is auto-federation-reachable (`public` + `federation` open, §6.2); else `NO-SUCH-TARGET` (uniform with private/absent) / `BLOCKED`. Bridge-session-only. |
| `FEDERATE` | `FEDERATE <network>/<namespace>` | membership; `auto_bridge` open | §11.10 — a local user asks their **home** network to auto-establish an on-demand bridge to a foreign namespace. Gated on NETBLOCK + a per-account cooldown; the bridge lands asynchronously (→ `MANIFEST` on the affected channels). Errors `UNSUPPORTED` (auto-federation off / self-network) `BLOCKED` `THROTTLED`. |
| `NETBLOCK` | `NETBLOCK ADD <network> [:reason]` / `REMOVE <network>` / `LIST` | `netblock` (`*` only) | Effects §11.6. → `NETBLOCKED`. |
| `MEDIA` | `MEDIA BLOCK <hash> [:reason]` / `UNBLOCK <hash>` / `BLOCKS` | `media-block` (`*` only) | §13 hash moderation: block deletes the blob + thumbnail and rejects re-upload + mirror (content = identity). → `MEDIA-BLOCKED`. |
| `REPORT-FORWARD` | `REPORT-FORWARD <report-id> <msgid> <category> [:note]` | bridge session | Forward a report to the origin over the bridge; reporter identity stripped (§11.9). Bridge-session-only. |
| `FSESSION` | `FSESSION <fsid> OPEN <account>` / `CMD :<line>` / `REPLY :<line>` / `CLOSE` | bridge session | §11.11 — multiplex a federated user's **control** session over the bridge (homeserver authority). `F` opens/relays; `H` attributes each `CMD` to `account@F` and enforces against its own grants. Carries commands + their direct replies only (broadcast events ride the mirror); the user never connects to `H` (IP non-exposure). Bridge-session-only. |
| `VOICE` | `VOICE JOIN\|LEAVE <#chan>` / `VOICE DESC :<sdp>` | feature-gated | §16. |


### 6.7 Moderation & reporting (C/NS/N)

| Command | Syntax | Cap | → Result / notes |
|---|---|---|---|
| `REPORT` | `REPORT <msgid> <category> [scope] [:note]` | membership | Routed to the reporter's home network. → `REPORTED <report-id>`; errors `NO-SUCH-TARGET` `THROTTLED` (10/hr RECOMMENDED) `QUOTA`. |
| `REPORTS LIST` | `REPORTS LIST <scope> [status=open\|resolved] [cursor]` | `reports` at scope | The handler queue. → `REPORT-FILED` page + `MORE`. `scope` is the concrete cap scope (`ns:<name>` or `*`). |
| `REPORTS RESOLVE` | `REPORTS RESOLVE <report-id> <action> [:note]` | `reports` | Releases the retention hold after a 7-day grace (RECOMMENDED). → `REPORT-RESOLVED`. |
| `MUTE` / `UNMUTE` | `MUTE <scope> <account> [:reason]` | `mute` at scope | Deny/allow `send`. `scope` = `#chan\|ns:<name>\|*` (a `*` mute is network-wide). → `MODERATED`. |
| `BAN` / `UNBAN` | `BAN <scope> <account> [:reason]` | `ban` at scope | Deny/allow join + send; a fresh channel-scope ban force-parts the target. → `MODERATED`; blocked joins get `BANNED`. |
| `KICK` | `KICK <#chan> <account> [:reason]` | `kick` | Force-part (no persistent state — may rejoin). → `MODERATED`. |

**Two moderation surfaces, composed** (`can_post = ¬muted ∧ ¬banned ∧ (channel open ∨ holds send)`): the **deny-list** above is targeted per-account state checked against a channel's covering scopes (channel, its namespace, `*` — so `*` = global/network moderators, `ns:` = namespace moderators). Complementarily, a channel may be set **`CHANNEL META <#chan> posting :restricted`**, after which posting requires the `send` capability — so `GRANT send` / `REVOKE send` (+ epoch, §10.4) governs who may speak (e.g. an announcements channel). A mute always denies regardless of posting mode. Kick/ban emit a `MEMBER part` to the channel (the target sees the removal); `MODERATED <scope> <account> <mute\|unmute\|ban\|unban\|kick>` (`by=`/`reason=` tags) is echoed to the acting moderator.

**`REPORT` arguments.** `category` — normative set `spam \| harassment \| violence \| sexual \| csam \| illegal \| self-harm \| other` (extensible with an `x-` prefix). `scope` — `ns` (namespace moderators, default) or `net` (network operator); `csam` and `illegal` are ALWAYS *also* routed to `net`, the legally accountable party. `note` — optional free text ≤ 1024 B. Membership-gated: you can only report what you can see — an invisible/absent msgid returns `NO-SUCH-TARGET` (anti-enumeration unchanged). Handlers are holders of the `reports` cap at the relevant scope (`ns:<name>` or `*`).

**`REPORTS RESOLVE` actions.** `dismissed \| content-removed \| user-actioned \| escalated`; `escalated` re-routes an ns-scope report up to net scope (keeping it open, holds intact). Handlers get the full `REPORT-RESOLVED` (`by=` + `note=`); the reporter gets the minimal form — no handler identity, no note.

**Content states** (marked honestly on the filed report):

| State | Meaning |
|---|---|
| `verified` | The server still holds the reported event; a **retention hold** is placed (§12.1). |
| `unverified` | The msgid is expired or the channel is `ephemeral` — nothing server-side confirms the content. Accepted and flagged; handlers weigh it accordingly. |
| `reporter-attested` | `e2ee` channel: the server holds only ciphertext. The reporter MAY voluntarily attach the plaintext they saw (marked reporter-provided, not server-verified). The alternative — server-readable "reportable e2ee" — would break §14's unrepresentability guarantee and is rejected. |

**Confidentiality.** The reported party is never notified and MUST NOT learn the reporter's identity from any protocol surface. Handlers see the reporter's account (accountability against report-flooding); a network MAY anonymize the reporter toward ns-scope handlers while preserving it for the operator.

---

## 7. Events Reference

| Event | Payload | Notes |
|---|---|---|
| `WELCOME <network>` | `features=`, `attestation=` | → READY |
| `CHALLENGE <nonce>` | | keypair auth |
| `MESSAGE <#chan|@user> <user@net> :body` | `msgid=`, `reply-to=`, `thread=`, `attach.N=`, `fmt=`, `label=` (echo only); **in batches also `edited=<n>`, `edited-at=<ms>`** | echo = ack |
| `EDITED <#chan\|@user> <user@net> :new` | own `msgid=`, `edit-of=` | **live only** — compacted out of batches |
| `DELETED <#chan\|@user> <msgid>` | `by=` | tombstone; sole survivor of a deleted message in batches |
| `REACTION <#chan\|@user> <msgid> <emoji>` | `op=`, `by=` | **live only** |
| `REACTIONS <#chan\|@user> <msgid> <emoji> <count>` | `by=` (first ≤20 actors, comma-sep) | **batch summary form** (§12.1) |
| `MEMBER <#chan> <user@net> <join\|part>` | `display=`, `count=` | `count=` = member count after the change (the §6.3 JOIN response) |
| `TYPING <#chan> <user@net> <start\|stop>` | | never stored |
| `MARKED <#chan> <msgid>` | | read-marker sync to the account's own sessions |
| `UNREAD-COUNTS <#chan> <unread> <mentions>` | | server-computed unread tally since the read marker; pushed on login + on `MARK` to the account's own sessions |
| `PRESENCE <user@net> <online\|away\|dnd\|invisible>` | | never bridged |
| `POLICY <#chan> <policy>` | | sent on join and on policy change |
| `CHANMETA` | | as v0.8 |
| `NS-META <ns> ...` | incl. `recovery-set=`, `recovery=pending`, `recovery-eta=`, `recovery-rung=`, `root-history` | recovery announcements ride here |
| `TOKEN` / `INVITED` / `MANIFEST` / `NETBLOCKED` | | as v0.8 |
| `REPORTED <report-id>` | `label=` | ack to reporter |
| `REPORT-FILED <report-id> <msgid> <category>` | `state=verified\|unverified\|reporter-attested`, `reporter=` (per config), `scope=` | to `reports` cap holders |
| `REPORT-RESOLVED <report-id> <action>` | | handlers get full form; reporter gets minimal form |
| `BATCH START\|END` | `id=`, `truncated`, **`compacted`** | brackets HISTORY; **every** line of a batch (brackets and items) echoes the request label — batches are data pages (§3.5) |
| `MORE <cursor>` / `PONG` | | |
| `ERR <CODE> [ctx] :text` | `label=`, `retry-after=`, `max=` | §8 |

Unknown events MUST be ignored.

---

## 8. Error Registry (normative)

`ERR <CODE> [context] :human text` — codes stable, text not.

| Code | Meaning | Notes |
|---|---|---|
| `MALFORMED` | unparseable | close after 5/60 s |
| `UNSUPPORTED` | version/feature absent | |
| `NOT-AUTHED` | verb illegal in state | |
| `AUTH-FAILED` | bad credentials/proof | constant-time |
| `NO-SUCH-TARGET` | absent **or hidden** | **anti-enumeration code**: nonexistent, private, view-gated, expired/foreign msgid, dead invite — one code, one timing envelope |
| `CONFLICT` | name taken / version race / recovery pending | |
| `FORBIDDEN` | categorically disallowed | closed registration, EDIT off-origin, bad recovery signatures |
| `CAP-REQUIRED <cap>` | missing capability | names the cap |
| `BANNED` | explicit ban | meant to be felt |
| `BLOCKED` | netblock | |
| `QUOTA` / `TOO-LARGE` / `THROTTLED` | limits | `max=` / `retry-after=` tags |
| `POLICY` | policy violation | weak password, e2ee transition w/o purge |
| `SLOW` | client lagging | forced HISTORY resync follows |
| `INTERNAL` | server fault | leaks nothing |

No `UNKNOWN-COMMAND` — unknown verbs are ignored; labels make the silence detectable.

---

## 9. Semantics & Guarantees

### 9.1 Ordering
Per-channel **total order** = origin actor's ULID order; bridged replicas preserve it. No cross-channel guarantees. DMs: total order per (network, pair).

### 9.2 Delivery & acks
Send: `MSG`+`label` → echo `MESSAGE` (same label, assigned msgid) = ack; no echo → resend same label; servers dedup `(session,label)` for 5 min → effectively exactly-once. Receive: dedup by msgid. Backpressure: `SLOW` + forced resync; never unbounded buffering.

### 9.3 Message model (event sourcing)
Edits/deletes/reactions are new events referencing the original msgid — never in-place mutation — **on the live path**; storage and batches use the compacted materialization (§12.1). Replies: `reply-to=`. **Threads are views, not channels**: `thread=` tag, no separate membership, `HISTORY thread=` filter.

### 9.4 Rich content
UTF-8, optional `fmt=md` (CommonMark subset); oversize → `TOO-LARGE`, never truncation. Link embeds are server-generated sub-events — clients never implicitly fetch third-party URLs.

### 9.5 DMs (v1)
`MSG @user`, same network only; network-config retention (default `permanent`); both accounts, all devices; `HISTORY @user` symmetric; edits/deletes/reactions/replies yes, threads no. Cross-network DMs deferred (open question).

### 9.6 Time
Server-stamped via ULIDs; client clocks untrusted.

### 9.7 Client reconnect (RECOMMENDED)
Backoff 1→60 s jittered → `HELLO` → `AUTH KEY` → server sends `MEMBER`/`POLICY` snapshots (membership is server-side) → per channel `HISTORY after=<last msgid>` (render `truncated` as a visible gap) → resend unacked labels → `MARKED` snapshot restores read state (each marked channel is followed by an `UNREAD-COUNTS` so badges survive the reconnect).

---

## 10. Identity

### 10.1 Account
`user@network.tld`; home network handles registration, recovery, moderation accountability.

Each account also has an immutable **ULID**, minted at registration and never
reused. The handle is the login + display name; the **ULID is the stable identity
capabilities key by** (§10.4) — so grants survive a (future) rename and a
re-registered handle never inherits stale authority. The ULID is a per-network
identifier (like the handle): unique within its network, meaningful only relative
to it. It is internal — never shown; the user-facing identity stays
`user@network`.

### 10.2 Portable keypair attestation
Ed25519 device keys; home network signs `{pubkey, account, network, expiry, sig}` (deterministic CBOR encode-before-sign); verified remotely via `https://<network>/.well-known/weft` (cached). No global identity server. Rotation = superseding attestation; revocation via well-known. Key rotation never evades NETBLOCK (name-keyed).

Well-known document (JSON):
```json
{ "protocol": "weft/1", "network": "hda.example", "signing-key": "<b64-ed25519-pubkey>" }
```

### 10.3 Display identity
Signed profile blob (nick, avatar) travels with the user; remotes MAY override display, MUST show canonical `user@network`.

### 10.4 Capability tokens
```
token = sign(issuer_key, {              // deterministic-CBOR body, version 2
  subject: <pubkey | account-ULID | account@network> | UNBOUND,
  scope:   <#chan> | ns:<name> | *,
  caps:    [...],
  expiry:  <short>,
  chain:   [parent hashes]   // to the scope root
})
```
The **subject** is one of: a device **pubkey** (device-bound); a **local
account's ULID** (§10.1 — never the mutable handle); a **foreign
`account@network`** (a federated user granted authority on this network — F owns
her ULID, which this network neither knows nor keys on, so it names her by the
network-qualified handle, §11 homeserver authority); or **UNBOUND** (an invite,
bound to the redeemer's key on redemption). Only a **pubkey** subject may sign
child tokens (delegate); account/foreign/unbound subjects are leaves. The body is
**version-tagged**: v1 (name-subject) tokens are refused on sight — an upgrade
re-grants.

Deterministic CBOR, encode-before-sign (Biscuit = possible upgrade). Delegation via `grant:X`; chains verify to the namespace root key or network key. "Roles" = named token templates; editing re-mints on refresh. A role's holder may be a **foreign `account@network`** — the membership + the granted caps key by that subject, so a partner network's user can wear a role here (§6.5). Revocation: short expiry + refresh (`TOKEN` events) + per-scope revocation epochs. Standard set: `send, edit-own, delete-own, delete-any, react, pin, invite, kick, ban, mute, policy, view, attach, chan-create, reports, bridge, ns-admin, ns-create, netblock, grant:<cap>` (`netblock`: `*` only; `reports` grantable at `ns:` and `*`; `mute`/`ban`/`kick` at `#chan`/`ns:`/`*` — the moderation tiers, §6.7). View gating gets full anti-enumeration. **Capability checks precede side effects, always.**

### 10.5 Account verification (email / age)

Accounts carry **verification claims** — `(kind, subject, state)` where `kind` is an open namespace (`email`, `birthday`, …), `subject` is what's claimed (an address, a birth date), and `state` is `pending` | `confirmed`. Two proof models:

- **Server-proven (`email`):** `VERIFY EMAIL <address>` records a `pending` claim and mails a one-time code; `VERIFY CONFIRM email <code>` proves it (`confirmed`). The code is short-lived (15 min), single-use, in-memory (a restart just means re-request).
- **Self-attested (`birthday`):** `VERIFY BIRTHDAY <YYYY-MM-DD>` records + `confirms` on the spot — honestly self-declared, not server-proven (a server cannot verify age without an ID provider, §18).

`VERIFY LIST` returns the caller's own claims (one `VERIFIED <kind> <subject>` per claim, `@state=`). **Subjects are PII** (email address, birth date) → returned **only to the owner's own session**, never broadcast. This is **badge-only**: claims do not gate channel/cap access yet (an age-gate is a later policy extension). SMTP is a weftd deployment concern (`[smtp]` config); with none configured the server records claims and logs the code (dev).

---

## 11. Federation — Scoped Bridging

### 11.1 The manifest
```
manifest = sign(scope_authority_key, {
  peer, version (monotonic), channels: [...],
  history: from-epoch | full,
  media:   mirror | mirror-max:<bytes> | none,
  typing:  yes | no,
  created, updated
})
```
Both sides store manifest + peer ack; **forwarding outside the last mutually-acked version is a protocol violation**. Scope proposals compile a **snapshot**; later channels need explicit `BRIDGE ADD`. No surprise forwarding.

### 11.2 Bridge sessions
Mutual QUIC session authenticated by a `bridge` capability token — same acceptor path as clients.

### 11.3 Authorization ladder
`#channel` → `bridge` cap holder; `ns:<name>` → namespace root; `*` → network signing key. Blast radius priced in signatures.

### 11.4 Event flow
Origin msgids + attestations intact, verified against the origin's well-known key. EDIT/DELETE honored only from the msgid's origin. Retention → strictest. `e2ee` bridges only pass-through MLS. Per-user attestation blocks without touching the manifest. **No transitivity — one hop from origin, loops structurally impossible, no shared state to resolve.**

### 11.5 Visibility interaction
Private/unlisted namespaces may bridge (root-signed only); their manifests are confidential — peers MUST NOT list their channels. `MANIFEST` notification to members on any audience change.

### 11.6 NETBLOCK
Operator blocklist of remote networks; `{network, private reason, added, actor}`. Effects (normative): reject proposals both directions (`ERR BLOCKED`); sever existing manifests (members get `MANIFEST`, owners get `NETBLOCKED`); reject the network's attestations everywhere (AUTH, ingestion, invite redemption); stop fetching/mirroring its media. **Name-keyed** — rotation-proof; evasion requires a new domain. Authority: network key or `netblock` cap. Visibility: config `blocklist_visibility: operators|members|public`. NS owners can't override but may keep narrower denylists (extension). Non-transitivity ⇒ one block = total isolation, no propagation machinery.

### 11.7 Federated history backfill
Bridge peers use ordinary `HISTORY` over the bridge session. Served iff: channel in acked manifest ∧ range within `history` flag (`from-epoch` = nothing before manifest `created`; cheap ULID compare) ∧ origin retention still holds it. Backfilled events verified like live traffic; stored under negotiated policy (**not a retention loophole**). Bulk → `STREAM`, ULID-cursor resumable, independently rate-limitable: when a served page exceeds ~200 events the server answers the `HISTORY` with `STREAM ACCEPT <token>` instead of an inline `BATCH`; the requester opens a data-plane stream and sends `BACKFILL <token>` (QUIC bidi) or `GET /backfill?t=<token>` (HTTP) to pull the serialized batch (newline-delimited `Reply` lines, folded exactly like an inline batch). The token is one-time; a failed pull is retried by re-issuing the `HISTORY` (resume = new token). Reconnect: `HISTORY after=<last stored>` per channel; expired ranges marked `truncated` — never silent. Serves **compacted materialized view** only (§12.1) — backfill is not an undelete oracle. Flipping `history=full` = manifest amendment → version bump → re-ack → `MANIFEST` to members (built-in notification).

### 11.8 Media across bridges
Referenced blobs **mirrored** (fetched over bridge data plane, BLAKE3-verified — substitution detectable). Rationale: clients only talk to home; hotlinking leaks reader IPs and breaks on origin outage. Bounded by manifest `media`; `none` renders unavailable-by-policy, never silent. Backfilled media rides `history`. Mirrors obey receiver retention **and receiver hash blocklist**.

**Mirror pull (concrete).** On ingesting a bridged message whose attachment URI has a *foreign* origin, the receiver records the reference locally (its members are then gated + can fetch) and pulls the blob back over the **same authenticated bridge connection** to the origin, on a data-plane bidi stream: `MIRROR <requester-network> <b3-hash> <sig>` → `OK <mime> <len>\n<bytes…>` | `ERR nosuch`. `sig` is the **requester network's** signing key over `hash‖requester‖origin` (domain-separated), so the request is *self-authenticating* — the origin serves iff a network it already federates with (a known peer key) proves control of that key, and it need not correlate the data-plane stream with any control-plane bridge session (no origin↔member correlation). The receiver verifies the returned bytes hash to the requested `b3-hash` before storing (content addressing: the origin cannot substitute). Any failure — unknown requester, bad signature, absent blob — is the uniform `ERR nosuch` (invariant 1: presence never leaks). The pull is eager (fired on ingest); a receiver with no live connection to the origin simply records the reference and skips the fetch until one exists.


### 11.9 Reports and federation

- A report always lands at the reporter's home network (§6.7). For a bridged message, the home network can act **locally** without anyone's permission: local redaction of its replica (its storage, its rules — analogous to the receiver-side hash blocklist in §11.8) and attestation-level blocking of the sender.
- The home network MAY additionally **forward** the report to the origin network over the bridge session (`REPORT-FORWARD <report-id> <msgid> <category> [:note]`, bridge-session-only verb). Forwarding strips the reporter's identity by default — the origin receives a network-attributed report ("hda.example forwarded a harassment report against your msgid X"). Origin networks treat forwarded reports as net-scope, `unverified`-at-minimum input; they are free to ignore them, and chronic ignoring is what `NETBLOCK` is for.
- Report queues, resolutions, and holds NEVER replicate across bridges; there is no federated moderation state, only forwarded signals.

### 11.11 Federation sessions & homeserver authority

A federated user may hold **caps/roles** on a network she is not a member of
(§10.4, §6.5) and **exercise** them there, without ever connecting to it.

- **Homeserver authority (normative).** Authority is anchored at the **network**,
  not the device. Network `F` proves control of its signing key at `AUTH BRIDGE`
  (§11.2); it then **speaks for its own users** — as a linked IRC server does, and
  as a Matrix homeserver signs for its users. `H` accepts `F`'s assertion that
  `alice@F` is acting and enforces it against **`H`'s own grant store** for the
  subject `alice@F`. `F` may only assert its *own* users (`sender.network == F`,
  origin authority, §11.4); it can never speak for `H`'s users or a third
  network's. Per-device command signing is deliberately **not** required: `F` is
  `alice`'s identity provider (it can reset her password / enroll devices), so a
  device signature buys nothing against a malicious `F` — the trust boundary is
  the network. The backstop for a misbehaving `F` is `NETBLOCK` (§11.6).

- **Content rides the mirror; control rides the session.** A federated user's
  *content* — `MSG`/`EDIT`/`DELETE`/`REACT` — stays **F-origin** and forwards one
  hop (§11.4); it is **never** authored through `H`, or origin authority would
  break. Only **control/admin** actions (moderation, `GRANT`/`REVOKE`, channel
  and namespace administration, invites, role assignment, report handling) travel
  as commands over a **federation session**.

- **The session — `FSESSION` (bridge-session-only).** `F` multiplexes a user's
  command session over the *existing* authenticated `F↔H` bridge — one channel
  per server-pair, no per-user connection to `H`:
  - `FSESSION <fsid> OPEN <account>` — `F` opens a session for its local user;
    `H` forms the actor `account@F`.
  - `FSESSION <fsid> CMD :<inner control line>` — a command from that user (F→H).
  - `FSESSION <fsid> REPLY :<inner reply line>` — the command's **direct reply**
    (H→F): a labeled ack or `ERR`. Broadcast events do **not** tunnel here — they
    reach her through `F`'s mirror of the namespace, so the session carries only
    the request/response pair and never subscribes to channels.
  - `FSESSION <fsid> CLOSE` — end the sub-session.
- **IP non-exposure (MUST).** All cross-network traffic — the event mirror *and*
  the command session — is server-to-server over the one bridge. A user never
  connects to `H`; `H` only ever sees `F`'s server address. No verb, session, or
  link may reveal a user's IP to a foreign network.
- **Enforcement.** `H` verifies `F` (network key), attributes each `CMD` on an
  `fsid` to the vouched `account@network`, and checks it against `H`'s grant store
  exactly as for a local actor (capability checks precede side effects,
  §10.4). Operator/namespace-owner authority is **local-only** — never satisfied
  by a foreign actor; her power on `H` is exactly what `H` granted `account@network`.

---

## 12. History, Retention & Compaction (server duties)

- Retention enforced by the storing network; purge tasks honor policy; tombstones persist in `retained`/`permanent`.
- Clients get `HISTORY` only from their **own** network (trust cornerstone). Origin = authoritative copy; replicas bounded by negotiated policy.
- Media blobs refcounted against referencing events.

### 12.1 Message compaction (new)

Two regimes, one principle: **live is event-sourced, at-rest is materialized.**

**Live path (unchanged):** real-time subscribers receive every event as it happens — `MESSAGE`, then `EDITED` per edit, `REACTION` per add/remove, `DELETED`. Clients need the increments for UI.

**Audit window:** intermediate events (superseded edit bodies, cancelled reaction pairs) are retained verbatim for `compact-after:<dur>` (network config, default **24 h**; settable per channel by `policy` cap holders) — the moderation window in which "what did it say before the edit" is answerable.

**Compaction (after the window):** the stored log per channel is rewritten:
- An edited message → **original event + final edit only**; intermediate edit bodies dropped. The count survives.
- Reaction add/remove pairs that cancel → dropped; surviving reactions → per-emoji summary rows.
- A deleted message → **tombstone only** (the `DELETED` event); content gone per retention rules, as before.
- Replies/threads unaffected (structural tags live on surviving events).

**Wire form (batches):** `HISTORY`/backfill responses carry the compacted materialization and mark it `compacted` on `BATCH END`:
- One `MESSAGE` per surviving message with final body + `edited=<count>` + `edited-at=<ms>` tags — no `EDITED` chains in batches.
- `REACTIONS <#chan> <msgid> <emoji> <count>` summary events (`by=` lists the first ≤20 actors; the count is authoritative) — no add/remove ping-pong.
- `DELETED` tombstones as-is.

**Retention holds (reporting interplay):** filing a `verified` report places a hold on the reported event and its context (RECOMMENDED: ±25 surrounding events in the channel). Held events are exempt from **both** compaction and retention purge — including in `retained:<d>` channels and including pre-edit bodies still inside the audit window at filing time — until the report is resolved plus a 7-day grace. Holds are invisible to ordinary members (no protocol surface reveals that a message is under report). `ephemeral` channels store nothing, so nothing can be held (hence `unverified`); `e2ee` holds preserve ciphertext blobs only.

**Effects elsewhere:**
- Backfill (§11.7) automatically benefits: bridge catch-up transfers shrink by the edit/reaction churn factor, and the existing "materialized view only" rule becomes precisely specified rather than implied.
- `MARK`/read logic unaffected (markers reference surviving msgids; a marker on a compacted-away edit event resolves to its `edit-of` root).
- E2EE channels: the server cannot compact ciphertext (it can't see event relations inside); e2ee compaction is client-side during device sync — normative non-goal for servers.
- Moderation implication, stated honestly: after the audit window, pre-edit content is **gone on this network**. Networks wanting longer audit trails raise `compact-after`; the protocol default favors the "edits eventually really disappear" privacy expectation.

---

## 13. Media

Upload: `STREAM OFFER media <mime> <bytes>` (checks `attach` + size config; RECOMMENDED 25 MiB img / 500 MiB video) → `STREAM ACCEPT <token>` → uni-stream → BLAKE3 hash → `weft-media://<origin>/<b3-hash>` + `{mime, bytes, w, h, duration?}`; dedup by construction. Posting: `attach.N=` (≤10), `attach-meta=`; bare media = empty trailing + tags. Fetching: home network only, range semantics (video = ranged/segmented fetch; live A/V = WEFT-RT). Server-generated thumbnails as derived blobs. Moderation: hash-level blocking — re-uploads dead on arrival. E2EE: client encrypts pre-upload; no server thumbnails; host-blindness extends to attachments.

## 14. E2EE

Channel mode `e2ee` = **MLS (RFC 9420)** group keying; server = blind Delivery Service. Consequences (enforced + surfaced): no server search, no server embeds, no server thumbnails, no server compaction; history = client-mediated device sync. Retention enum makes "encrypted but server-readable" unrepresentable. Policy transitions to/from `e2ee` need an empty channel or explicit `purge`. Recovery (§2.4) never restores e2ee history.

## 15. Comparison

| | IRC | Discord | Matrix | WEFT |
|---|---|---|---|---|
| Self-host cost | tiny | n/a | heavy | small |
| History | none | full | fully replicated | per-channel policy, compacted |
| Federation | netsplit mesh | none | transitive, replicated | manifest peering, 1 hop |
| Defederation | k-lines | n/a | leaky ACLs | NETBLOCK, airtight |
| Identity | nick | central | homeserver | account + portable key |
| Permissions | modes | role bitmasks | power levels | scoped capability tokens |
| Communities | none | guilds | spaces | user-owned namespaces |
| Owner ≠ platform admin | n/a | no | partial | yes — incl. auditable recovery ladder |
| Private host-blind spaces | no | no | clunky | private ns + e2ee |
| Invites | no | opaque links | links | verifiable cap tokens |
| Media moderation | n/a | URL-based | per-server | hash-level |
| Netcat-debuggable | yes | no | no | control plane: yes |

## 16. WEFT-RT — Voice/Video Companion

Signaling in core: `VOICE JOIN` → SFU endpoint + short-lived media token (`speak`/`listen` caps); `VOICE DESC` SDP-equivalent; media = QUIC datagrams to SFU. Opus mandatory; AV1/H.264 negotiable. Zero-voice servers conformant; discovery via `features=`.

## 17. WEFT-IRC — Legacy IRC Compatibility (extension)

Optional server-side RFC 2812 + IRCv3 gateway (:6697 TLS); the gateway is the home network. Mappings: NICK/SASL → display/AUTH; `JOIN #ns/chan` valid natively; PRIVMSG→MSG (`+draft/reply`→`reply-to=`); TAGMSG `+draft/react`→REACT; `server-time`/`msgid`→ULIDs/origin msgids; `chathistory`/`batch`→HISTORY/BATCH; MODE = coarse read-mostly projection; KICK/TOPIC capability-checked. Degradations (normative): edits/deletes as `* edited:`/`* message deleted` fallbacks (IRC users can't edit); threads flattened `[thread 01H…]`; media as short-lived tokened HTTPS URLs; **e2ee channels invisible** (`NO-SUCH-TARGET` treatment); DISCOVER→LIST, invites via `/msg WeftServ REDEEM`; 8 KiB↔512 B line splitting. Purpose: the likely operator audience is on IRC today; day-one irssi/WeeChat usability, and the gateway is a projection, not a lossy translator.

---

## 18. Open Questions

1. Server discovery: `.well-known` only, or SRV too?
2. Rate limiting / anti-spam beyond `THROTTLED`: PoW? Attestation reputation?
3. Namespace squatting cooldown after `NS DELETE`?
4. Shared blocklists (opt-in, per-entry review) — deferred.
5. Backfill quotas for `history: full` + `media: mirror` bridges.
6. IRC-gateway media upload — implementation-defined for now.
7. Cross-network DMs: consent + routing without a channel manifest.
8. Custom emoji sets per namespace. **Note (M3):** the `:shortcode:` form cannot travel as a middle param — a leading `:` is the §4 trailing marker. Until decided, implementations send shortcodes bare and reject leading-colon emoji.
9. Recovery-set privacy: should members see *who* the quorum is, or only that one exists? (Currently: existence only.)
10. Report data retention: how long do resolved reports themselves persist (distinct from content holds)? Legal-compliance minimums vary by jurisdiction — likely network config with a floor.
11. Name. WEFT remains a placeholder.

---

## Appendix A — Decision history

v0.1 core design → v0.2 namespaces + manifest bridging → v0.3 user-owned namespaces, visibility, invites → v0.4 NETBLOCK → v0.5 backfill + `history` flag → v0.6 media, mirroring, WEFT-IRC → v0.7 implementability audit → v0.8 consolidation → v0.9 namespace recovery ladder + message compaction → **v0.10 message reporting: home-network routing, retention holds, honest e2ee/ephemeral limits, bridge forwarding (this document)**.

### Foundational milestones (M0–M3a)

*Editorial (M0 implementation)*: §7 said "as v0.8" for the `TYPING`/`MARKED`/`PRESENCE`/`POLICY` event payloads, contradicting the "fully self-contained" claim; the table now spells them out as implemented by `weft-proto` (`TYPING <#chan> <user@net> <start|stop>`, `MARKED <#chan> <msgid>`, `PRESENCE <user@net> <status>`, `POLICY <#chan> <policy>`). `CHANMETA` remains deferred (M4).

*Amendment (M1 implementation)*: §3.4 keepalive interval lowered from RECOMMENDED 60 s to **10 s** to match contemporary chat clients; the "2 missed = dead" rule scales accordingly (~30 s liveness window).

*Amendments (M2 implementation)*: §6.1 defines the previously unspecified AUTH ENROLL response (`@attestation=` WELCOME, mirroring AUTH KEY success); §10.2 pins the `/.well-known/weft` document format (JSON: `protocol`, `network`, `signing-key`).

*Amendments (M3a implementation)*: §6.4 pins HISTORY's `key=value` middle-param syntax; §6.4 REACT emoji shortcodes travel bare (leading `:` collides with the §4 trailing marker — see §18 #8); §7 widens EDITED/DELETED/REACTION/REACTIONS targets to `<#chan|@user>` ahead of DM support; §7 documents that every line of a batch echoes the request label (data-page reading of §3.5).

### M4 — capabilities, namespaces, moderation, recovery

*Amendments (M4a implementation — capabilities, channels, invites)*: the loose §6.5/§6.3 syntax is pinned and the previously-unspecified response events are defined. GRANT `<subject> <scope> <caps> [expiry=<secs-ttl>]` → `@token=<b64> TOKEN <subject> <scope>`. REVOKE `<subject> <scope> [caps=<list>] [epoch]` (bare number bumps the scope epoch) → `TOKEN` reflecting the remaining caps (empty token = none). CHANNEL CREATE → `POLICY` (confirms name + policy); CHANNEL POLICY → `POLICY` (broadcast to members + labeled to actor); CHANNEL META → `CHANMETA <#chan> <key> :<value>`; CHANNEL DELETE → `CHANMETA <#chan> deleted :`. INVITE MINT → `@token=<invite-id> INVITED <scope> <invite-id> :<weft://…/i/ id>`; INVITE REVOKE → `INVITED … max-uses=0` (closed); INVITE REDEEM → the §6.3 JOIN response (auto-join). **Model notes (M4a scope):** operator accounts (weftd config `operators`) hold every cap at `*` — the network-key authority that bootstraps the grant chain (§11.3); the server keeps a grant table as the same-network enforcement fast path while the signed token is for delegation/federation; `ns:` scopes (GRANT/INVITE) and namespaced channels defer to M4b; invites are server-side id+counter records (the offline-verifiable unbound-token form is a federation concern, deferred). Reaction/emoji shortcodes and the `:` grammar clash remain §18 #8.

*Amendments (M4-5 implementation — namespaces + a channel-layout extension)*: NS CREATE carries the client-generated root pubkey in a `@root=<b64>` tag (§6.2 sketched the verb without it); default tier `unlisted`. Responses: NS CREATE/META/VISIBILITY → `NS-META <ns> <visibility>` with `owner=`/`title=`/`description=`/`icon=`/`cats=`/`federation=yes` tags (§11.10 auto-federation reachability); NS DELETE → an `NS-META … description=deleted` marker; DISCOVER → one `NS-META` per public namespace + `MORE <cursor>`; NS DELEGATE is sugar for `GRANT ns:<name>`. **Enforcement model (same-network, M4-5 scope):** the namespace *owner* account holds every cap within `ns:<name>` — the ns-scoped analog of an operator at `*`; the client-held root **key** is recorded (for TRANSFER/recovery/federation, later milestones) but same-network delegation uses the grant table, so — like operator authority — it is not yet cryptographically operator-unforgeable (that hardening comes with federation, M5). NS TRANSFER + the recovery ladder (§2.4) are M4c. **New extension — channel layout (Discord-style categories + order), Appendix A:** channels gain a `category` (free label) and `position` (integer); `CHANNEL META <#ns/chan> category|position :<value>` sets them; `CHANNELS <ns>` returns the ordered layout as `CHANNEL-LAYOUT <#chan> <position>` events (with a `category=` tag), sorted (category, position, name); private-namespace layouts are view-gated (invariant 1). **Server-authoritative categories (no client state):** the category *list* (including empty categories) lives on the **namespace** — `NS META <ns> categories :<comma-list>` (cap: `ns-admin`), carried back in NS-META's `cats=` tag; `CHANNELS <ns>` leads its response with the namespace's `NS-META` so a client renders every group (even empty ones) purely from server state. A `category`/`position` change **broadcasts `CHANNEL-LAYOUT`** to the channel's members, so re-ordering (e.g. dragging a channel above another) reaches every client, not just the mover. The client keeps no category state of its own.

*Amendments (M4c implementation — reporting + retention holds, §6.7/§12.1)*: `REPORT <msgid> <category> [scope] [:note]` where `scope` is the `ns|net` routing hint (default `ns`); `REPORTS LIST <scope>` / `REPORTS RESOLVE <id> <action>` take the **concrete** cap scope (`ns:<name>` or `*`), not the routing hint — a handler lists exactly the queue their `reports` cap covers. Responses: `REPORTED <report-id>` (labeled ack to reporter); `REPORT-FILED <report-id> <msgid> <category>` with `state=`/`scope=`/`reporter=` tags (to handlers); `REPORT-RESOLVED <report-id> <action>` — the handler's echo carries `by=`/`note=`, the reporter's push carries neither (confidentiality, invariant 12). **Routing:** ns-scope reports on a namespaced channel reach the namespace owner; ns-scope on a top-level channel or DM, and all net-scope, reach operators; `csam`/`illegal` always ALSO reach operators. **Content-state decision:** on the same-network path only `verified` is produced — anything the server cannot find is indistinguishable from nonexistent and already answered `NO-SUCH-TARGET` (invariant 1), so `unverified` (expired/ephemeral) and `reporter-attested` (e2ee) are wired through the codec + store but first *emitted* for bridged replicas (M5) / e2ee (M6). **Retention holds:** filing a `verified` report places refcounted holds on the reported root ± `HOLD_RADIUS` (=25) context roots; held roots are exempt from purge AND compaction until the report resolves + a 7-day grace, released by the maintenance scheduler (invariant 11). **Honest limitation:** live `REPORT-FILED` push reaches a queue's *default* handlers (ns owner / operators) only — delegated `reports`-cap holders fetch via `REPORTS LIST`, as there is no reverse cap→account index for fan-out (the same pull-not-push limit as the §2.4 recovery announcement). Reporter-identity anonymization toward ns handlers (§6.7 MAY) is deferred; handlers currently always see the reporter. Bridge `REPORT-FORWARD` (§11.9) is M5.

*Amendments (M4-6 implementation — namespace recovery ladder, §2.4)*: signed NS verbs carry their signature in a `@sig=<b64>` tag. NS TRANSFER (rung 1) is verified against the namespace's stored root **key** — the one place same-network namespace authority is cryptographically enforced (not just table-based). NS RECOVER takes a base64 `SignedRotation` (a `{namespace, new-root-key, new-owner}` record + collected signatures, deterministic-CBOR, domain-separated from transfer/cancel); the server picks the rung by whose signatures verify — quorum ≥ m → rung 2 (7-day delay), else operator (network-key) signed → rung 3 (then a 30-day delay — **superseded**, see the immediate-takeover amendment below), else FORBIDDEN. A second RECOVER while one is pending → CONFLICT. NS RECOVERY CANCEL is a root-signed veto (`weft-ns-cancel` domain). The delay window is applied by a scheduled task (alongside maintenance): at eta the root key + owner rotate and a `root-history` entry is appended (rung-3 marked operator-initiated forever). NS-META gains `recovery-set=yes` / `recovery=pending;recovery-eta=<ms>;recovery-rung=2|3`. **Same-network limitation (honest):** the recovery announcement is *reflected* on NS-META (queryable) but not yet *pushed* to all members — a push needs an ns-member broadcast (a follow-up); the invariant-9 guarantees that ARE enforced: no silent rotation path (every rotation is TRANSFER-signed or delayed+recorded), root-cancellable window, and permanent operator-initiated marking.

### M5 — federation

*Amendments (M5a–c implementation — federation, §6.6/§11)*: the §11 event payloads left "as v0.8" and several under-specified verb details are pinned here.
- **`AUTH BRIDGE <peer-network> <b64-network-pubkey>`** (new AUTH sub-verb, §11.2): a peer opens a bridge session by asserting its network signing key and proving control via the §6.1 `CHALLENGE`/`AUTH PROOF` (sign `nonce‖our-network`) flow; success → a bridge session (not an account), **bound to the proven key** (manifests verify against it). Two configurable trust modes: **pinned** (default/closed) accepts only configured peers whose asserted key matches the pin; **accept-any** (`federation.accept_any = true`, open federation) accepts any non-blocked network on the key it proves control of (trust-on-first-use — nothing external confirms the key really is that network's, so `NETBLOCK` is the escape hatch). A pin always wins over accept-any. Every failure funnels to the uniform `AUTH-FAILED` (no peer-existence oracle).
- **`BRIDGE PROPOSE <scope> <peer> [history=from-epoch|full] [media=mirror|mirror-max:<B>|none] [typing=yes|no]`** carries the signed manifest in a **`@manifest=<b64>`** tag (the `weft-manifest/1` deterministic-CBOR `SignedManifest`). Tag defaults are strictest-safe: `history=from-epoch`, `media=none`, `typing=no`. **`BRIDGE REMOVE <peer> <#chan>`** takes both params (the verb was shown bare). The §11.3 authority ladder is enforced *locally* on the proposing side (the operator must hold `bridge` at the scope / be the ns owner / be an operator); the wire manifest is uniformly **network-key-signed** so the peer verifies it against the signer's well-known key — blast-radius pricing stays a local-authorization property.
- **`MANIFEST <peer> <version> <live|added|removed|severed>`** with `channels=`/`history=`/`media=`/`typing=` tags — broadcast to affected channel members on every manifest change (§6.6, mandatory). **`NETBLOCKED <network> [:reason]`** — sent on netblock-induced sever (reason per `blocklist_visibility`), and as the labeled ack to `NETBLOCK ADD/REMOVE`; `NETBLOCK LIST` returns one `NETBLOCKED` per entry.
- **Forwarding gate (invariant 3):** a channel is forwardable to a peer iff present in **both** the last mutually-acked snapshot and the current one — `BRIDGE ADD` (current-but-not-acked) is blocked until re-ack, `BRIDGE REMOVE` (acked-but-not-current) stops at once. Same gate applies to ingestion and to §11.7 backfill.
- **Trust model (reference-server decision):** bridge trust is anchored at the **network-key session level** — the peer proved control of its network signing key at `AUTH BRIDGE`, so events on the session are attributed to that network and accepted only when `msgid.origin == authenticated peer` (invariant 2). Per-**device** attestations are therefore not carried on bridged event lines in this milestone (a noted refinement); origin authority for EDIT/DELETE is still enforced (honored only at the msgid's origin, `FORBIDDEN origin` elsewhere). **`REPORT-FORWARD`** on receipt files a net-scope **`unverified`** report into the operator queue with the reporter **stripped** (`reporter: None`, invariant 12) and no hold; queues/resolutions/holds never replicate (§11.9). §11.8 media-mirroring negotiates the manifest `media` policy only — blob mirroring rides M6. **Deferred to M5d (owner-tested manually):** the verified **outbound** QUIC dialer, `[[peers]]` config + well-known key fetch, and cross-wire transmission of operator-initiated `PROPOSE`/`REPORT-FORWARD`; `BRIDGE ADD/REMOVE` answer `UNSUPPORTED` until then.

### Media (M-media)

*Amendments (M-media implementation — content-addressed data plane + federation mirroring, §13/§11.8)*: the §13 data plane is concretely three transfer surfaces sharing one BLAKE3 blob store — a QUIC bidi framing (`PUT <upload-token>` / `GET <bearer> <hash> [range]` / `MIRROR <requester-net> <hash> <sig>`), an HTTP `POST /media` (OFFER token **or** session bearer) + `GET /media/<hash>?t=<bearer>` (Range-capable), sharing the `STREAM OFFER media <mime> <bytes>` → `STREAM ACCEPT <token>` grant flow. Fetch is membership-gated by a per-session **bearer** the server pushes as a `MEDIA TOKEN` event right after auth; a bad bearer / non-member / absent blob are one uniform not-found (invariant 1). Attachments ride a `MESSAGE` `@attach.N=weft-media://<origin>/<b3-hash>` tag; the channel actor refcounts blob↔message references and a maintenance GC collects orphans after a grace. Images are probed for dimensions + a ≤256px thumbnail stored as its own auto-referenced blob. **§11.8 federation mirroring (M-media-3):** on ingesting a bridged message with a *foreign*-origin attachment, the receiver records the reference locally then pulls the blob over the live bridge connection via a **self-authenticating** `MIRROR <requester-net> <hash> <sig>` (`sig` = requester network key over `hash‖requester‖origin`); the origin serves iff the requester is a known peer proving its key — no origin↔member correlation — and the receiver BLAKE3-verifies before storing under its own retention + blocklist. Failures are the uniform `ERR nosuch`. *Deferred:* the manifest `media`-mode gate on mirroring (always-on for now) and the `mirror-max` bandwidth bound (§18 #5).

*Amendments (M-media-4 implementation — backfill over STREAM, §6/§13/§11.7)*: a served `HISTORY` page whose compacted materialization exceeds `HISTORY_STREAM_THRESHOLD` (200) is **not** sent inline — the server serializes the whole `BATCH` once, holds it under a one-time token, and answers `STREAM ACCEPT <token>`. The requester pulls it off the generic data plane: a fourth QUIC data-stream verb `BACKFILL <token>` → `OK <len>\n<body>`, or an HTTP `GET /backfill?t=<token>` → the body, where `<body>` is the newline-delimited `Reply` lines the client folds exactly like an inline batch. The token is one-time (a spent/absent token is the uniform not-found, invariant 1); resume-after-failure = re-issue the `HISTORY` (a fresh token), so no server-side cursor state is held. The same upgrade serves **§11.7 federated bulk backfill**, fetched **lazily on client demand** (never eagerly on bridge-up — a federated scrollback nobody asks to see is never pulled): when a local client's `HISTORY` for a forwardable channel runs out of local scrollback (a short page), the *outbound* (dialing) side of the bridge sends a bulk `HISTORY` to the peer for that window (deduped per `(channel, before)`); the peer streams it if large; the dialer opens a `BACKFILL` stream over the bridge connection and feeds each pulled line back through ordinary bridged ingestion (origin-authority + manifest-gated, invariants 2/3), so symmetric requests never duplicate a side's own history. The pulled events broadcast to members and persist, so the next page serves them locally. Pre-bridge scrollback needs `history=full` (from-epoch serves only post-manifest history) — so **§11.10 auto-federation always offers `history=full`**, so that a user who federates into a foreign namespace and scrolls back reaches its existing history. *Deferred:* desktop (Tauri) client backfill pull (the web client fetches `/backfill` over HTTP; desktop paging stays under the threshold); reaction/edit fidelity on ingested backfill (the compacted `REACTIONS`/`edited=` form is lossy for replicas, as for any bridged batch).

*Amendments (M-media-5 implementation — hash moderation, §13)*: hash-level blocking is now live. A new `media-block` capability (`*`-scope — content is network-global) gates `MEDIA BLOCK <hash> [:reason]` / `MEDIA UNBLOCK <hash>` / `MEDIA BLOCKS`, each answered by a `MEDIA-BLOCKED <hash> [:reason]` event (one per entry for the list). `MEDIA BLOCK` records the hash in a `MediaBlocklistStore` (mem + PG, migration 0020) **and** deletes the blob's bytes + its derived thumbnail and forgets the blob records, so the block is immediate. The `is_blocked` seam (`ServerCtx::is_blob_blocked`) is no longer a stub: it consults the blocklist and is checked on every upload (QUIC `PUT` + HTTP `POST`), fetch (QUIC `GET` + HTTP `GET`, uniform not-found — invariant 1), and mirror (`store_mirrored`) path — so a blocked hash is dead on arrival and re-uploads of the identical bytes can't evade it (content = identity). *Deferred:* a client operator UI for the verb (a `*`-scope operator action, like `NETBLOCK`); cross-network shared blocklists (§18 #4).

### Gateways & cross-network identity (M6/M7)

*Amendments (M6 implementation — WEFT-IRC gateway subset, §17)*: the gateway is a `weft_core::ControlStream` (its own crate `weft-irc`) that translates IRC↔WEFT at the line boundary — one IRC line may yield several WEFT commands (registration → `HELLO`+`AUTH`) and vice-versa, so translation is a pure state machine and the stream is just async I/O around it. **Shipped subset:** registration `NICK`/`USER`/`PASS` → `HELLO` then `AUTH PASSWORD` (auto-`REGISTER` on first `AUTH-FAILED`; `PASS`, if ≥12 B, is the WEFT password, else a gateway default — a documented no-SASL convenience); `JOIN`/`PART` incl. namespaced `#ns/chan` (the `/` is a legal IRC chanstring char, so "`JOIN #ns/chan` valid natively" needs no special-casing); `PRIVMSG`/`NOTICE`↔`MSG` (a bare-nick target → WEFT DM `@nick`; the sender's own echo is suppressed since IRC renders sent lines locally); `NAMES` (best-effort — WEFT `MEMBER` reports changes, not the pre-existing roster, so the list fills in from observed joins); `LIST`→`DISCOVER` (each public namespace a `322` entry, `MORE`→`323`); `PING`/`PONG` answered at the IRC layer; `QUIT`; `WELCOME`→`001..005`+MOTD; `MEMBER`→`JOIN`/`PART`; edits/deletes/reactions **degraded to text** (`* edited:` / `NOTICE * a message was deleted` / `* reacted`, §17); errors→closest numeric else `NOTICE`. Enabled by `[listen] irc = <addr>` (plaintext; TLS termination is the operator's). **Deferred (M6+):** SASL, IRCv3 `server-time`/`msgid` tags, `chathistory`→HISTORY/BATCH, TAGMSG reactions, MODE/TOPIC/KICK projection, 8 KiB↔512 B splitting, and the e2ee-invisible (`NO-SUCH-TARGET`) treatment.

*Amendments (M7 implementation — moderation, §6.7/§10.4)*: adds a `mute` capability and five verbs — `MUTE`/`UNMUTE`/`BAN`/`UNBAN` `<scope> <account> [:reason]` (scope `#chan\|ns:<name>\|*`) and `KICK <#chan> <account> [:reason]` — plus a `MODERATED <scope> <account> <mute\|unmute\|ban\|unban\|kick>` event (`by=`/`reason=` tags). **Two composed surfaces:** (1) a **deny-list** — mute (deny `send`) / ban (deny join + send) records keyed by `(scope, account)`, checked against a channel's *covering scopes* (channel, its namespace, `*`), so a `*` record is a network-wide/global-moderator action and `ns:` a namespace one; cap-gated by `mute`/`ban`/`kick` at the target scope (operators/ns-owners implied). A fresh channel-scope ban force-parts the target (a `MEMBER part`, the ejected client cleans up on seeing its own part); kick is transient. (2) **`send`-cap enforcement** — `CHANNEL META <#chan> posting :restricted` makes posting require the `send` capability, so `GRANT`/`REVOKE send` (+ epoch) governs speech in that channel (e.g. announcements). Net gate: `can_post = ¬muted ∧ ¬banned ∧ (posting open ∨ holds send)`. A `restricted` boolean is added to the channel record (migration 0009), and a `weft_moderation` table holds the deny-list. **Honest limitation:** `MODERATED` is echoed to the acting moderator only (not broadcast to channel members) beyond the `MEMBER part` that kick/ban already emit; a full members-broadcast is a follow-up. Federated-user moderation (targeting `account@peer`) is deferred — targets are same-network accounts.

*Amendments (identity, caps & federation sessions — §10.1/§10.4/§6.5/§11.11)*: capability authority moves from the mutable handle to a stable identity, and a federated user can hold + exercise caps on another network.
- **Account ULID (§10.1):** every account gains an immutable ULID at registration; grants + role membership key by it (or a device pubkey, or a foreign `account@network`), never the handle — so name reuse / a future rename can't inherit authority. Per-network, DB-unique, internal (never displayed).
- **Token subject v2 (§10.4):** the signed body is version-tagged; `subject` is `pubkey | account-ULID | account@network | UNBOUND`. Only a pubkey subject may sign children. v1 (name-subject) tokens are **denied on upgrade** — re-grant to reissue.
- **Foreign subjects (§6.5):** `GRANT`/`REVOKE` and `ROLE ASSIGN`/`UNASSIGN` accept a foreign `account@network` subject; the role's membership + granted caps key by it, so a partner network's user wears a role here. `Moderated.by` and the report-resolution `resolved_by` widen from an account to a subject string to attribute a foreign moderator honestly.
- **Federation sessions & homeserver authority (§11.11):** new bridge-session verb **`FSESSION <fsid> OPEN|CMD|REPLY|CLOSE`** — `F` tunnels a user's **control** session over the bridge; `H` attributes each `CMD` to `account@F` and enforces against its own grants (operator/owner authority stays local-only). *Content* rides the mirror (F-origin, one hop); only control/admin verbs tunnel. Authority is **network-level** (trust `F`, like IRC/Matrix) — no per-device command signing (`F` is the user's identity provider, so it would be theater; `NETBLOCK` is the backstop). All cross-network traffic is server-to-server: **a user never connects to `H`** (IP non-exposure, MUST). **Honest limitations:** per-device attestations are still not carried on bridged lines (network-key session trust stands in); the client's "connected" surfacing of a newly-bridged namespace awaits the mirror-materialization step.

### Verification (M-verify)

*Amendment (M-verify implementation — account verification, §10.5)*: resolves §18-territory design for the email/age verification wire flow. New verbs under a `VERIFY` family: `VERIFY EMAIL <address>` (mails a 6-digit code, records a `pending` claim), `VERIFY CONFIRM <kind> <code>` (proves it — code is 15-min, single-use, in-memory), `VERIFY BIRTHDAY <YYYY-MM-DD>` (self-attested → `confirmed` on the spot), `VERIFY LIST` (own claims). One event `VERIFIED <kind> <subject> @state=pending|confirmed`, sent **owner-only** (subjects are PII). Reuses the existing `weft_store::Verification` claim→confirm store (no schema change). Email delivery is a `Mailer` **port** in weft-core (L2 stays I/O-free); weftd's impl is `lettre` SMTP from `[smtp]` config, with a dev log-mailer fallback (records the claim, logs the code) when unconfigured — the ring rustls provider is preserved (no aws-lc-rs). **Decisions (owner):** built-in SMTP (not a SaaS relay); age = self-attested birthday (a server can't prove age); **badge-only** (claims don't gate access — an age-gate is a later policy extension). §18 gains no new open question; the "wire protocol for proving a claim" note is now resolved.

### Client-parity & operational amendments

*Amendment (persistent membership, §6.3)*: channel membership is now **durable**, not session-scoped. `JOIN` records `(account, channel)` in a `MembershipStore` (migration 0011); `PART` and a forced part (kick/ban eject) clear it. On auth (`welcome_authed`), the server **auto-rejoins** the account to its stored channels — the client's channels and namespace tiles reappear on reconnect without any client-side re-join (the Discord model; replaces the earlier localStorage stopgap). Consequence: member join/part announcements now **dedupe by account** — a second device (or an auto-rejoin while another device is online) does not broadcast a fresh `MEMBER Join`/`Part`, and member counts are distinct-account counts. A brand-new account still lands in `#general` via the client on `REGISTER`.

*Amendment (PIN/CAPS + presence-in-MEMBERS)*: adds **`PIN`/`UNPIN <msgid>`** (cap `pin`, resolves the channel from the msgid) → `PINNED`/`UNPINNED` broadcasts, and **`PINS <#chan>`** (membership-gated) → a `BATCH` of `MESSAGE` (one per pin, oldest-first). Pins are a per-channel set in the store (migration 0010). Adds **`CAPS <account> <scope>`** → a `CAPS <account> <scope> :<comma-caps>` event listing the account's *effective* capabilities at the scope (operators/ns-owners expand to all); public — any member may query (caps aren't secret), powering client capability badges. Finally, **`MEMBERS`** now interleaves a `PRESENCE` event per member from an in-memory presence map (§6.1 stays "never stored/never bridged" — the map is live-only), so roster presence dots are correct for members who set status before the caller joined; `invisible` is removed from the map (renders offline, never revealed).

*Amendment (MEMBERS response shape, §6.3)*: `MEMBERS <#chan>` returns the roster framed as a `BATCH` — `BATCH START` (echoes the request `label`), one `MEMBER <#chan> <user@net> join` with the final `count=` per current member, then `BATCH END` — reusing the join event so clients fold each row into their roster exactly as for a live join. Membership-gated: a non-member of an existing channel gets `CAP-REQUIRED view` ("join first", same as `MARK`); a hidden/nonexistent channel stays `NO-SUCH-TARGET`. The reference server serves the whole roster in one batch and ignores the optional `cursor` (pagination is a later refinement); accounts are deduped across multiple devices/sessions.

*Amendment (channel rename, §6.3)*: `CHANNEL RENAME <#old> <#new>` changes a channel's identity within its namespace (the two must share the same `#ns/` prefix — a cross-namespace move would change ownership, so it's rejected `POLICY`). Cap: `ns-admin` at the channel scope (operators implied), verified before any mutation (invariant 4). The server re-keys **everything** scoped to the channel name in one atomic store transaction — the channel record, event history, capability grants + revocation epochs, moderation deny-list, pins, memberships, roles + assignments, per-account read markers, and retention holds (invariant 11: holds move with their content) — then respawns the channel actor under the new name. Members are told via `CHANNEL-RENAMED <#old> <#new>` (broadcast to the channel + a labeled copy to the initiator); clients re-key local state and re-join `#new`. Absent source or an already-taken target → `NO-SUCH-TARGET` / `CONFLICT`. Because channel-scope capability *tokens* are signed with the old scope string, any outstanding delegated tokens at `#old` stop matching after a rename — same effect as a scope epoch bump; re-delegate at `#new` (the server-side grant table, the same-network enforcement path, is re-keyed automatically).

*Amendment (namespace bulk-join, §6.2)*: `NS JOIN <name>` joins every channel in the namespace the caller may see in one round-trip — the server iterates the namespace's channels and joins each that isn't view-gated-away or ban-blocked ("not hidden by permissions"), emitting a `MEMBER` + `POLICY` per joined channel (unlabeled, a membership burst). If no channel is visible — nonexistent namespace, private, or all view-gated — it answers `NO-SUCH-TARGET` (one code, anti-enumeration). Complements the still-supported per-channel `JOIN #ns/chan`.

*Amendments (client-parity — presence liveness, roster, deny-list listing)*: §6.1 gains a `PresenceStatus::Offline` (`offline`) so a disconnect is representable on the wire. **Roster model (§6.3, Discord-style):** `MEMBERS` returns the *persistent* membership — offline members included — each followed by a `PRESENCE` event carrying its dot; online-ness is "holds a live session in the channel" (the presence map only refines online→`away`/`dnd`; a live `invisible` member reads `offline`). A **disconnect** broadcasts `PRESENCE <user> offline` (persistent membership retained) rather than `MEMBER … part`; only an explicit `PART`/kick/ban emits `MEMBER … part` (roster removal). A **reconnect/auto-rejoin** of an existing member broadcasts `PRESENCE <user> online` (not a fresh `MEMBER … join`). The presence map is cleared when an account's last session drops, so later snapshots render it offline. **New verb `MODLIST <scope>`** (§6.7) lists the moderation deny-list (mutes + bans) at a scope — cap `mute` **or** `ban` — answered as a `BATCH` of `MODERATED` events (each a current mute/ban, `by=`/`reason` populated); a non-moderator gets `CAP-REQUIRED`.

*Amendment (server-controlled unread counts, §6.3)*: adds **`UNREAD [<#chan>]`** → one **`UNREAD-COUNTS <#chan> <unread> <mentions>`** per channel, the server-computed tally of root messages newer than the account's read marker (`MARK`). `unread` counts only real messages from *other* senders — own messages and `join`/`part` system rows are excluded; `mentions` is the subset whose body references the account (`@account`) or `@everyone`/`@here` (a body-text heuristic — there is no structured mention field). Store method `EventStore::unread_counts(scope, account, since)`; no migration (reuses the existing events + the `system` column, and the `MARK` marker on `AccountStore`). The counts are **pushed unsolicited**: a per-channel `UNREAD-COUNTS` follows each `MARKED` in the §9.7 login snapshot, and a fresh count rides the cross-device `MARK` sync to the account's *other* sessions (the marking device already knows it read). This makes the client badge authoritative — it survives reload/reconnect and stays consistent across devices; the client keeps a live +1 tally between pushes (self-healing on the next `MARK`/reconnect). Notification-mute is a **client-only** preference (localStorage) — the server counts every channel and the client suppresses muted scopes.

*Amendment (message search, §6.4)*: adds **`SEARCH <#chan> :<query>`** (membership-gated) → a `BATCH` of `MESSAGE`, newest-first, capped at 50 — reusing the PINS/HISTORY batch shape so clients fold results exactly as any message. Store method `EventStore::search(scope, query, limit)`: non-system root messages whose body contains `query` (case-insensitive substring), tombstoned roots excluded, no migration. The reference implementation is **substring** on both backends (memory `contains`, Postgres `ILIKE` with LIKE-metacharacter escaping) for identical cross-backend semantics; a Postgres `tsvector` upgrade (stemming, ranking) is a later refinement. An empty query returns nothing. Search is per-channel for now; a namespace-wide search (fan-out across the member's channels) is a follow-up.

*Amendment (threads, §9.4)*: threads are now implemented, not deferred. A thread reply is an ordinary channel `MESSAGE` carrying a `thread=<root-msgid>` tag (already in `MsgMeta`); it broadcasts to the channel like any message. **`HISTORY <#chan> thread=<root>`** (the previously-stubbed filter) returns just that thread — the root plus every `thread=<root>` reply, oldest-first — via `EventStore::thread_roots(scope, root, limit)` (no migration; Postgres already stored the `thread` column). Clients render a thread as a side panel and **hide thread replies from the main timeline** (a root shows a "N replies" indicator instead) — a client presentation choice; the wire keeps replies in the channel so every member and bridge sees them. Thread reply counts are computed client-side from loaded messages for now (a server-side count, like unread counts, is a possible refinement). Federation carries `thread=` intact (origin-msgid preserved, invariant 2).

*Amendment (thread naming + listing, §9.4)*: threads gain an optional **display name** and a channel-level **listing**, keeping the "threads are views, not channels" model (a name is metadata keyed by the root msgid, not a new identity). New verbs: **`THREAD NAME <#chan> <root> [:name]`** — set, or (with the trailing omitted/empty) clear, a thread's name; authorized by the **same rule as posting** (`can_post`: not muted/banned, and `send` if the channel is `restricted`), and the root must be a real message in the channel or the reply is `NO-SUCH-TARGET` (invariant 1). It broadcasts **`THREAD-NAMED <#chan> <root> [:name]`** to members so every client relabels live. **`THREADS <#chan>`** (membership-gated) → a `BATCH` of **`THREAD <#chan> <root> replies=<n> [last=<msgid>] [:name]`**, one per thread that has ≥1 reply, most-recently-active first (a reply-less root is not yet a thread). Store: `EventStore::channel_threads(scope, limit)` aggregates reply counts + last activity from the existing `thread` column and joins `weft_thread_names(scope, root, name, set_by, set_at)` (migration 0031); `EventStore::set_thread_name(scope, root, name, by, at_ms)` upserts/clears. Names federate no differently than any per-channel metadata (not yet mirrored over bridges — a refinement, like emoji propagation). Clients expose a **Threads** button (channel topbar) listing the channel's threads, and an inline-editable **name** on the thread side-panel; the "N replies" indicator shows the name when set.

*Amendment (custom emoji, §9.4)*: per-namespace custom emoji. Verbs **`EMOJI ADD <ns> <name> <media>`** / **`EMOJI REMOVE <ns> <name>`** (cap `ns-admin`; `name` is 1–32 of `[A-Za-z0-9_]`, `media` a `weft-media://…` reference to an uploaded image) and **`EMOJI LIST <ns>`** (any authed session — emoji aren't secret) → an **`EMOJI <ns> <name> <media>`** batch (`EMOJI-REMOVED` on remove). Store: `weft_emoji(namespace, name, media)` (migration 0027; `EmojiStore` mem+PG); the image bytes live in the blob store, this only maps names. The §13 orphan-blob GC keeps any blob a live emoji references (`EmojiStore::emoji_media`), the same way it keeps avatars — an emoji's image is referenced by `weft_emoji`, not a message media-ref. Clients fetch a namespace's emoji on select, render `:name:` as an inline image in message bodies and reactions (reaction key = the literal `:name:` string, so no reaction-store change), and expose them in the composer/reaction picker beside the unicode set; the upload UI lives in per-namespace server settings. Live propagation to other members beyond the adder is a refinement (clients refetch `EMOJI LIST` on namespace select).

*Amendment (operators in Postgres, §11.3)*: operator authority moved from the weftd config `[operators]` list to a per-account **`operator` flag** in the store (`weft_accounts.operator`, migration 0026; `AccountStore::set_operator`/`is_operator`/`list_operators`, mem+PG). The capability check (`actor_has_cap`/`actor_can_grant`) and the admin-panel auth (`admin_scopes`) now treat an account as an operator if the DB flag is set **or** it appears in the config list — so config operators still work as a backward-compat seed, but the config list is deprecated. Managed by a new **`weftd admin` CLI** (`create` = register + flag, `grant`/`revoke`, `list`) that talks to Postgres directly (no running server needed; the bootstrap admin is created this way). Because the check reads the DB live, CLI changes take effect without a restart. This is weftd operational config, not wire protocol — no verb/event change.

*Amendment (Discord-style role display + in-place rename, §6.5)*: roles gain two display properties and one new verb. **Display (`ROLE CREATE` / `ROLE` event):** `hoist=0|1` ("display this role's members separately in the member list") and `pos=<n>` (sort position, ascending) ride as optional key=value middle params on `ROLE CREATE` and as the same tokens on the `ROLE` event; `ROLES` returns definitions position-ordered, and **`ROLE REORDER <scope> :<name1,name2,…>`** sets every named role's `pos` to its index — the wire form behind drag-and-drop role ordering. Both default to `0`/`false`, so pre-amendment clients are unaffected. **Rename (`ROLE RENAME <scope> :<old>,<new>`, cap `ns-admin`):** roles are keyed by `(scope, name)`, so renaming client-side (delete + re-create) would silently drop every assignment. It is therefore one server-side migration that carries the definition *and* all membership rows together, in a single transaction on the durable backend. Issued tokens are deliberately **not** migrated and need no migration: a role's authority is its `caps`, which a rename leaves untouched — consistent with `ROLE DELETE` also leaving granted tokens alone. Both names ride the trailing as a comma pair, reusing `ROLE REORDER`'s existing delimiter convention (a role name may contain spaces but not a comma); the alternative — `old` as a middle param — was rejected because it would make spaced role names unrenameable. Errors follow the existing registry: an absent `<old>` is `NO-SUCH-TARGET` (indistinguishable from unauthorized, invariant 1) and a `<new>` that already names a live role is `POLICY`, since merging two capability bundles under one name is not a rename. The cap check precedes both existence probes (invariant 4), so neither can be used to enumerate roles.

*Amendment (operator takeover is immediate, §2.4 rung 3)*: the rung-3 delay is **0** — the seizure applies on receipt, with no pending state and nothing to cancel. The original 30-day window came from reading rung 3 purely as *recovery* (the owner is gone and unreachable, so a long, loud window costs nothing and guards against a hostile operator). But rung 3 is also the only **moderation** lever a network operator has over a namespace whose owner is present and is themselves the abuse, and for that job the window was actively harmful in two ways: a month of continued abuse is not a moderation response, and the window's headline feature — *cancellability by the current root* — hands the veto to precisely the party being removed, so a hostile owner could stall indefinitely by re-vetoing. A delay is the right instrument against a *lost* key and the wrong one against a *live* adversary. Rungs 1–2 are unchanged; only rung 3 moves.

What is given up is stated plainly rather than papered over: rung 3 no longer satisfies the "delay + root-cancellable" half of invariant 9, and a compromised network signing key can now seize a namespace instantly instead of announcing a month in advance. That is a real reduction in defence-in-depth against a malicious or breached operator, accepted on the grounds that an operator already hosts the data, already holds every capability at `*`, and can already freeze, delete, or read any non-`e2ee` channel — a 30-day namespace-takeover window was never the thing standing between that operator and the community. **E2EE remains the actual boundary and is untouched:** a seized root joins encrypted channels as a fresh MLS member with no access to prior content (invariant 8), so a takeover confers administration, never history.

What is kept is what does the accountability work without a window: the rotation MUST still verify against the **network signing key** (authorization is unchanged — a stranger's signature is `FORBIDDEN`), it is **announced** via `NS-META`, and it is **permanently marked operator-initiated in `root-history`**, visible to every member and bridge peer forever. Implementation: `RECOVERY_DELAY_RUNG3_SECS = 0`, and `on_ns_recover` applies a zero-delay rung inline via `rotate_root` rather than parking it as pending — parking it with an already-elapsed ETA would have left the namespace in the abuser's hands until the next maintenance tick, which is the opposite of the intent. Tests: `operator_takeover_seizes_the_namespace_immediately` (applies at once, no pending state, nothing left for the scheduler, `operator_initiated` recorded) and `a_takeover_still_needs_the_network_key`.
