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
| Permissions | Scoped capability tokens (signed CBOR, delegable, short-lived) — no role tables. Roles (§6.5.1) are named, colored *bundles* of these tokens: a display layer, never a separate enforcement path |
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
| **NS recovery** | **Three-rung ladder: root transfer → social quorum (7 d delay) → operator last resort (30 d delay); all delayed rungs announced and cancellable** |
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

Failure mode addressed: the namespace root key is lost (device loss, owner death, departure) and the community would otherwise be permanently ownerless. Recovery is a **ladder** — each rung slower, louder, and more auditable than the last. All delayed rungs share three properties: a **mandatory public delay**, a **mandatory announcement** (`NS-META` event with `recovery=` fields to all members), and **cancellability by the current root** during the window (a live root can always veto — this defeats coerced or hostile recovery).

**Rung 1 — Transfer (no delay).** The root signs `NS TRANSFER`. Normal succession; nothing new.

**Rung 2 — Social recovery (7-day delay, RECOMMENDED default).**
- The owner MAY designate a recovery set at any time: `NS RECOVERY SET <name> <m> <key1,key2,...>` — an M-of-N quorum of keys (typically trusted co-admins). Stored in signed ns metadata; members can see that a recovery set exists (not necessarily who).
- Recovery: quorum members co-sign a **rotation record** naming the new root key; any of them submits `NS RECOVER <name> <b64-rotation-record>`. The server verifies M valid signatures from the set, then starts the delay window.
- During the window: `NS-META` announces `recovery=pending;recovery-eta=<ts>;recovery-rung=2` to all members. The current root may cancel with `NS RECOVERY CANCEL <name>` (one signature beats the quorum — the point is that a *live* owner always wins).
- At expiry the rotation applies: new root key takes over; all tokens chained to the old root expire naturally (short-lived anyway); the rotation is permanently recorded in ns metadata (`root-history`).

**Rung 3 — Operator last resort (30-day delay).**
- Available only when no recovery set is configured or the quorum itself is unreachable. The operator (network signing key) initiates `NS RECOVER` with an operator-signed rotation record.
- Same announcement/cancellation mechanics, longer window. The resulting rotation is **permanently marked operator-initiated** in `root-history` — auditable by every member and by bridge peers forever. An operator who abuses this pays in visible reputation, which is the honest limit of what protocol can enforce against the party hosting the data.

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
| `NS META` | `NS META <name> <title\|description\|icon> :<value>` | `ns-admin` | → `NS-META`. |
| `NS VISIBILITY` | `NS VISIBILITY <name> <tier>` | `ns-admin` | → `private` applies anti-enumeration immediately. → `NS-META`. |
| `NS DELEGATE` | `NS DELEGATE <name> <account\|pubkey> <cap>[,…]` | grant chain | Sugar for `GRANT` at `ns:` scope. → `TOKEN`. |
| `NS TRANSFER` | `NS TRANSFER <name> <account>` | root key | Rung-1 succession, root-signed. → `NS-META` (new owner). |
| `NS RECOVERY SET` | `NS RECOVERY SET <name> <m> <key1,key2,…>` | root | Designate the M-of-N quorum (§2.4). → `NS-META` (`recovery-set=yes`). |
| `NS RECOVER` | `NS RECOVER <name> <b64-rotation-record>` | quorum / operator sig | Rung 2 (quorum) or rung 3 (operator); starts the delay window. → `NS-META` \| `FORBIDDEN` (bad sig) \| `CONFLICT` (pending). |
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
| `JOIN` | `JOIN <#chan> [invite-ref]` | membership / invite | → `MEMBER` + `POLICY` + `count=` \| `NO-SUCH-TARGET` \| `BANNED`. |
| `PART` | `PART <#chan> [:reason]` | — | → `MEMBER … part`. |
| `MEMBERS` | `MEMBERS <#chan> [cursor]` | membership | Paginated; bridge peers see remote members only as they've appeared. |
| `TYPING` | `TYPING <#chan> <start\|stop>` | `send` | Never stored; rate-limited (1/3 s RECOMMENDED); bridged only under manifest `typing: yes`. |
| `MARK` | `MARK <#chan> <msgid>` | membership | Account-scoped read marker, synced via `MARKED`; survives `ephemeral`. |

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
| `STREAM` | `STREAM OFFER <media\|backfill> <mime> <bytes>` | — | → `STREAM ACCEPT <token>` → data-plane transfer. HISTORY switches to STREAM above ~200 events (RECOMMENDED). |

### 6.5 Capabilities & invites (§10.4)

| Command | Syntax | Cap | → Result / notes |
|---|---|---|---|
| `GRANT` | `GRANT <account\|pubkey> <scope> <cap>[,…] [expiry=<s>]` | `grant:<cap>` at ≥ scope | Scope `<#chan>` \| `ns:<name>` \| `*`; the chain rule is cryptographic. → `TOKEN`. |
| `REVOKE` | `REVOKE <account\|pubkey> <scope> [caps=<list>] [epoch]` | grant chain | Stops refresh; a bare `epoch` number bumps the scope revocation epoch. → `TOKEN` (remaining caps). |
| `INVITE MINT` | `INVITE MINT <scope> [max-uses=] [expiry=]` | `invite` | → `INVITED` (`@token=`, `weft://<net>/i/<b64>` link). |
| `INVITE REVOKE` | `INVITE REVOKE <invite-id>` | `invite` | Closes the counter; already-redeemed members unaffected. |
| `INVITE REDEEM` | `INVITE REDEEM <b64>` | — | Verifies chain + counter, mints a member token **bound to the redeemer's key**, auto-joins the default channel. Dead invites → `NO-SUCH-TARGET` (indistinct). |

Invite tokens are capability tokens with an **unbound subject**: one object serves single-use / expiring / vanity links — offline-verifiable authorization, never itself a membership credential.

#### 6.5.1 Roles — named capability-token bundles

A **role** is a named, colored bundle of capability tokens at a scope: `(scope, name, color, caps)`. Roles are a *presentation and convenience* layer over §10.4 capabilities — **there is still no role table in the enforcement path.** A role *is* its capabilities: assigning a role grants exactly its `caps` as ordinary tokens, and every permission check remains a pure capability-token check. "Bob is a Moderator" is derived, not stored per-member: Bob displays a role iff his effective caps at the scope are a superset of that role's `caps`. This keeps the invariant "permissions = scoped capability tokens, no role tables" intact while giving clients human-readable, colored labels.

| Command | Syntax | Cap | → Result / notes |
|---|---|---|---|
| `ROLE CREATE` | `ROLE CREATE <scope> <color> <cap>[,…] :<name>` | `ns-admin` at scope | Define/replace a role (upsert on `(scope, name)`). `color` is a display hint (e.g. `#e8b93d`); `name` (may contain spaces) rides the trailing. → updated `ROLES` batch. |
| `ROLE DELETE` | `ROLE DELETE <scope> :<name>` | `ns-admin` at scope | Remove a definition. Already-granted tokens are unaffected (revoke separately). → updated `ROLES` batch. |
| `ROLE ASSIGN` | `ROLE ASSIGN <scope> <account> :<name>` | `grant:<cap>` for each cap in the bundle | Grants the role's tokens to the account — identical authority + `TOKEN` path as `GRANT`. At a **namespace** scope also propagates channel role-permissions (below). |
| `ROLES` | `ROLES <scope>` | — (public: roles aren't secret) | → a `BATCH` of `ROLE <scope> <color> <caps> :<name>`. |

The `ROLE` event carries a definition; clients map an account's caps back to role names+colors for display (profile cards, member lists).

**Role channel-permissions.** Because roles resolve to tokens and tokens are scoped, a namespace role and a **channel role of the same name** compose to give the Discord "role has permission X in channel Y" override — without a rules engine. A role `Speaker` defined at `ns:s` carries the namespace-wide caps; a role `Speaker` defined at `#s/stage` (same name) carries that role's caps *for that channel only*. `ROLE ASSIGN ns:s <account> :Speaker` grants the namespace bundle **and**, for every channel `#s/*` that has a same-named role, that channel role's caps at the channel — so e.g. giving `Speaker` `send` in one restricted channel follows every Speaker automatically on assignment. Editing a channel role affects future assignments; re-assigning re-applies it to an existing member. Enforcement stays purely token-based (§10.4): the namespace covers its channels, a channel covers only itself.

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
| `NETBLOCK` | `NETBLOCK ADD <network> [:reason]` / `REMOVE <network>` / `LIST` | `netblock` (`*` only) | Effects §11.6. → `NETBLOCKED`. |
| `REPORT-FORWARD` | `REPORT-FORWARD <report-id> <msgid> <category> [:note]` | bridge session | Forward a report to the origin over the bridge; reporter identity stripped (§11.9). Bridge-session-only. |
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
Backoff 1→60 s jittered → `HELLO` → `AUTH KEY` → server sends `MEMBER`/`POLICY` snapshots (membership is server-side) → per channel `HISTORY after=<last msgid>` (render `truncated` as a visible gap) → resend unacked labels → `MARKED` snapshot restores read state.

---

## 10. Identity

### 10.1 Account
`user@network.tld`; home network handles registration, recovery, moderation accountability.

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
token = sign(issuer_key, {
  subject: <pubkey|account> | UNBOUND,
  scope:   <#chan> | ns:<name> | *,
  caps:    [...],
  expiry:  <short>,
  chain:   [parent hashes]   // to the scope root
})
```
Deterministic CBOR, encode-before-sign (Biscuit = possible upgrade). Delegation via `grant:X`; chains verify to the namespace root key or network key. "Roles" = named token templates; editing re-mints on refresh. Revocation: short expiry + refresh (`TOKEN` events) + per-scope revocation epochs. Standard set: `send, edit-own, delete-own, delete-any, react, pin, invite, kick, ban, mute, policy, view, attach, chan-create, reports, bridge, ns-admin, ns-create, netblock, grant:<cap>` (`netblock`: `*` only; `reports` grantable at `ns:` and `*`; `mute`/`ban`/`kick` at `#chan`/`ns:`/`*` — the moderation tiers, §6.7). View gating gets full anti-enumeration. **Capability checks precede side effects, always.**

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
Bridge peers use ordinary `HISTORY` over the bridge session. Served iff: channel in acked manifest ∧ range within `history` flag (`from-epoch` = nothing before manifest `created`; cheap ULID compare) ∧ origin retention still holds it. Backfilled events verified like live traffic; stored under negotiated policy (**not a retention loophole**). Bulk → `STREAM`, ULID-cursor resumable, independently rate-limitable. Reconnect: `HISTORY after=<last stored>` per channel; expired ranges marked `truncated` — never silent. Serves **compacted materialized view** only (§12.1) — backfill is not an undelete oracle. Flipping `history=full` = manifest amendment → version bump → re-ack → `MANIFEST` to members (built-in notification).

### 11.8 Media across bridges
Referenced blobs **mirrored** (fetched over bridge data plane, BLAKE3-verified — substitution detectable). Rationale: clients only talk to home; hotlinking leaks reader IPs and breaks on origin outage. Bounded by manifest `media`; `none` renders unavailable-by-policy, never silent. Backfilled media rides `history`. Mirrors obey receiver retention **and receiver hash blocklist**.


### 11.9 Reports and federation

- A report always lands at the reporter's home network (§6.7). For a bridged message, the home network can act **locally** without anyone's permission: local redaction of its replica (its storage, its rules — analogous to the receiver-side hash blocklist in §11.8) and attestation-level blocking of the sender.
- The home network MAY additionally **forward** the report to the origin network over the bridge session (`REPORT-FORWARD <report-id> <msgid> <category> [:note]`, bridge-session-only verb). Forwarding strips the reporter's identity by default — the origin receives a network-attributed report ("hda.example forwarded a harassment report against your msgid X"). Origin networks treat forwarded reports as net-scope, `unverified`-at-minimum input; they are free to ignore them, and chronic ignoring is what `NETBLOCK` is for.
- Report queues, resolutions, and holds NEVER replicate across bridges; there is no federated moderation state, only forwarded signals.

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

*Amendments (M4-6 implementation — namespace recovery ladder, §2.4)*: signed NS verbs carry their signature in a `@sig=<b64>` tag. NS TRANSFER (rung 1) is verified against the namespace's stored root **key** — the one place same-network namespace authority is cryptographically enforced (not just table-based). NS RECOVER takes a base64 `SignedRotation` (a `{namespace, new-root-key, new-owner}` record + collected signatures, deterministic-CBOR, domain-separated from transfer/cancel); the server picks the rung by whose signatures verify — quorum ≥ m → rung 2 (7-day delay), else operator (network-key) signed → rung 3 (30-day delay), else FORBIDDEN. A second RECOVER while one is pending → CONFLICT. NS RECOVERY CANCEL is a root-signed veto (`weft-ns-cancel` domain). The delay window is applied by a scheduled task (alongside maintenance): at eta the root key + owner rotate and a `root-history` entry is appended (rung-3 marked operator-initiated forever). NS-META gains `recovery-set=yes` / `recovery=pending;recovery-eta=<ms>;recovery-rung=2|3`. **Same-network limitation (honest):** the recovery announcement is *reflected* on NS-META (queryable) but not yet *pushed* to all members — a push needs an ns-member broadcast (a follow-up); the invariant-9 guarantees that ARE enforced: no silent rotation path (every rotation is TRANSFER-signed or delayed+recorded), root-cancellable window, and permanent operator-initiated marking.

*Amendment (persistent membership, §6.3)*: channel membership is now **durable**, not session-scoped. `JOIN` records `(account, channel)` in a `MembershipStore` (migration 0011); `PART` and a forced part (kick/ban eject) clear it. On auth (`welcome_authed`), the server **auto-rejoins** the account to its stored channels — the client's channels and namespace tiles reappear on reconnect without any client-side re-join (the Discord model; replaces the earlier localStorage stopgap). Consequence: member join/part announcements now **dedupe by account** — a second device (or an auto-rejoin while another device is online) does not broadcast a fresh `MEMBER Join`/`Part`, and member counts are distinct-account counts. A brand-new account still lands in `#general` via the client on `REGISTER`.

*Amendment (PIN/CAPS + presence-in-MEMBERS)*: adds **`PIN`/`UNPIN <msgid>`** (cap `pin`, resolves the channel from the msgid) → `PINNED`/`UNPINNED` broadcasts, and **`PINS <#chan>`** (membership-gated) → a `BATCH` of `MESSAGE` (one per pin, oldest-first). Pins are a per-channel set in the store (migration 0010). Adds **`CAPS <account> <scope>`** → a `CAPS <account> <scope> :<comma-caps>` event listing the account's *effective* capabilities at the scope (operators/ns-owners expand to all); public — any member may query (caps aren't secret), powering client capability badges. Finally, **`MEMBERS`** now interleaves a `PRESENCE` event per member from an in-memory presence map (§6.1 stays "never stored/never bridged" — the map is live-only), so roster presence dots are correct for members who set status before the caller joined; `invisible` is removed from the map (renders offline, never revealed).

*Amendment (MEMBERS response shape, §6.3)*: `MEMBERS <#chan>` returns the roster framed as a `BATCH` — `BATCH START` (echoes the request `label`), one `MEMBER <#chan> <user@net> join` with the final `count=` per current member, then `BATCH END` — reusing the join event so clients fold each row into their roster exactly as for a live join. Membership-gated: a non-member of an existing channel gets `CAP-REQUIRED view` ("join first", same as `MARK`); a hidden/nonexistent channel stays `NO-SUCH-TARGET`. The reference server serves the whole roster in one batch and ignores the optional `cursor` (pagination is a later refinement); accounts are deduped across multiple devices/sessions.

*Amendment (namespace bulk-join, §6.2)*: `NS JOIN <name>` joins every channel in the namespace the caller may see in one round-trip — the server iterates the namespace's channels and joins each that isn't view-gated-away or ban-blocked ("not hidden by permissions"), emitting a `MEMBER` + `POLICY` per joined channel (unlabeled, a membership burst). If no channel is visible — nonexistent namespace, private, or all view-gated — it answers `NO-SUCH-TARGET` (one code, anti-enumeration). Complements the still-supported per-channel `JOIN #ns/chan`.

*Amendments (M7 implementation — moderation, §6.7/§10.4)*: adds a `mute` capability and five verbs — `MUTE`/`UNMUTE`/`BAN`/`UNBAN` `<scope> <account> [:reason]` (scope `#chan\|ns:<name>\|*`) and `KICK <#chan> <account> [:reason]` — plus a `MODERATED <scope> <account> <mute\|unmute\|ban\|unban\|kick>` event (`by=`/`reason=` tags). **Two composed surfaces:** (1) a **deny-list** — mute (deny `send`) / ban (deny join + send) records keyed by `(scope, account)`, checked against a channel's *covering scopes* (channel, its namespace, `*`), so a `*` record is a network-wide/global-moderator action and `ns:` a namespace one; cap-gated by `mute`/`ban`/`kick` at the target scope (operators/ns-owners implied). A fresh channel-scope ban force-parts the target (a `MEMBER part`, the ejected client cleans up on seeing its own part); kick is transient. (2) **`send`-cap enforcement** — `CHANNEL META <#chan> posting :restricted` makes posting require the `send` capability, so `GRANT`/`REVOKE send` (+ epoch) governs speech in that channel (e.g. announcements). Net gate: `can_post = ¬muted ∧ ¬banned ∧ (posting open ∨ holds send)`. A `restricted` boolean is added to the channel record (migration 0009), and a `weft_moderation` table holds the deny-list. **Honest limitation:** `MODERATED` is echoed to the acting moderator only (not broadcast to channel members) beyond the `MEMBER part` that kick/ban already emit; a full members-broadcast is a follow-up. Federated-user moderation (targeting `account@peer`) is deferred — targets are same-network accounts.

*Amendments (M6 implementation — WEFT-IRC gateway subset, §17)*: the gateway is a `weft_core::ControlStream` (its own crate `weft-irc`) that translates IRC↔WEFT at the line boundary — one IRC line may yield several WEFT commands (registration → `HELLO`+`AUTH`) and vice-versa, so translation is a pure state machine and the stream is just async I/O around it. **Shipped subset:** registration `NICK`/`USER`/`PASS` → `HELLO` then `AUTH PASSWORD` (auto-`REGISTER` on first `AUTH-FAILED`; `PASS`, if ≥12 B, is the WEFT password, else a gateway default — a documented no-SASL convenience); `JOIN`/`PART` incl. namespaced `#ns/chan` (the `/` is a legal IRC chanstring char, so "`JOIN #ns/chan` valid natively" needs no special-casing); `PRIVMSG`/`NOTICE`↔`MSG` (a bare-nick target → WEFT DM `@nick`; the sender's own echo is suppressed since IRC renders sent lines locally); `NAMES` (best-effort — WEFT `MEMBER` reports changes, not the pre-existing roster, so the list fills in from observed joins); `LIST`→`DISCOVER` (each public namespace a `322` entry, `MORE`→`323`); `PING`/`PONG` answered at the IRC layer; `QUIT`; `WELCOME`→`001..005`+MOTD; `MEMBER`→`JOIN`/`PART`; edits/deletes/reactions **degraded to text** (`* edited:` / `NOTICE * a message was deleted` / `* reacted`, §17); errors→closest numeric else `NOTICE`. Enabled by `[listen] irc = <addr>` (plaintext; TLS termination is the operator's). **Deferred (M6+):** SASL, IRCv3 `server-time`/`msgid` tags, `chathistory`→HISTORY/BATCH, TAGMSG reactions, MODE/TOPIC/KICK projection, 8 KiB↔512 B splitting, and the e2ee-invisible (`NO-SUCH-TARGET`) treatment.

*Amendments (M5a–c implementation — federation, §6.6/§11)*: the §11 event payloads left "as v0.8" and several under-specified verb details are pinned here.
- **`AUTH BRIDGE <peer-network> <b64-network-pubkey>`** (new AUTH sub-verb, §11.2): a peer opens a bridge session by asserting its network signing key and proving control via the §6.1 `CHALLENGE`/`AUTH PROOF` (sign `nonce‖our-network`) flow; success → a bridge session (not an account), **bound to the proven key** (manifests verify against it). Two configurable trust modes: **pinned** (default/closed) accepts only configured peers whose asserted key matches the pin; **accept-any** (`federation.accept_any = true`, open federation) accepts any non-blocked network on the key it proves control of (trust-on-first-use — nothing external confirms the key really is that network's, so `NETBLOCK` is the escape hatch). A pin always wins over accept-any. Every failure funnels to the uniform `AUTH-FAILED` (no peer-existence oracle).
- **`BRIDGE PROPOSE <scope> <peer> [history=from-epoch|full] [media=mirror|mirror-max:<B>|none] [typing=yes|no]`** carries the signed manifest in a **`@manifest=<b64>`** tag (the `weft-manifest/1` deterministic-CBOR `SignedManifest`). Tag defaults are strictest-safe: `history=from-epoch`, `media=none`, `typing=no`. **`BRIDGE REMOVE <peer> <#chan>`** takes both params (the verb was shown bare). The §11.3 authority ladder is enforced *locally* on the proposing side (the operator must hold `bridge` at the scope / be the ns owner / be an operator); the wire manifest is uniformly **network-key-signed** so the peer verifies it against the signer's well-known key — blast-radius pricing stays a local-authorization property.
- **`MANIFEST <peer> <version> <live|added|removed|severed>`** with `channels=`/`history=`/`media=`/`typing=` tags — broadcast to affected channel members on every manifest change (§6.6, mandatory). **`NETBLOCKED <network> [:reason]`** — sent on netblock-induced sever (reason per `blocklist_visibility`), and as the labeled ack to `NETBLOCK ADD/REMOVE`; `NETBLOCK LIST` returns one `NETBLOCKED` per entry.
- **Forwarding gate (invariant 3):** a channel is forwardable to a peer iff present in **both** the last mutually-acked snapshot and the current one — `BRIDGE ADD` (current-but-not-acked) is blocked until re-ack, `BRIDGE REMOVE` (acked-but-not-current) stops at once. Same gate applies to ingestion and to §11.7 backfill.
- **Trust model (reference-server decision):** bridge trust is anchored at the **network-key session level** — the peer proved control of its network signing key at `AUTH BRIDGE`, so events on the session are attributed to that network and accepted only when `msgid.origin == authenticated peer` (invariant 2). Per-**device** attestations are therefore not carried on bridged event lines in this milestone (a noted refinement); origin authority for EDIT/DELETE is still enforced (honored only at the msgid's origin, `FORBIDDEN origin` elsewhere). **`REPORT-FORWARD`** on receipt files a net-scope **`unverified`** report into the operator queue with the reporter **stripped** (`reporter: None`, invariant 12) and no hold; queues/resolutions/holds never replicate (§11.9). §11.8 media-mirroring negotiates the manifest `media` policy only — blob mirroring rides M6. **Deferred to M5d (owner-tested manually):** the verified **outbound** QUIC dialer, `[[peers]]` config + well-known key fetch, and cross-wire transmission of operator-initiated `PROPOSE`/`REPORT-FORWARD`; `BRIDGE ADD/REMOVE` answer `UNSUPPORTED` until then.

*Amendments (M4c implementation — reporting + retention holds, §6.7/§12.1)*: `REPORT <msgid> <category> [scope] [:note]` where `scope` is the `ns|net` routing hint (default `ns`); `REPORTS LIST <scope>` / `REPORTS RESOLVE <id> <action>` take the **concrete** cap scope (`ns:<name>` or `*`), not the routing hint — a handler lists exactly the queue their `reports` cap covers. Responses: `REPORTED <report-id>` (labeled ack to reporter); `REPORT-FILED <report-id> <msgid> <category>` with `state=`/`scope=`/`reporter=` tags (to handlers); `REPORT-RESOLVED <report-id> <action>` — the handler's echo carries `by=`/`note=`, the reporter's push carries neither (confidentiality, invariant 12). **Routing:** ns-scope reports on a namespaced channel reach the namespace owner; ns-scope on a top-level channel or DM, and all net-scope, reach operators; `csam`/`illegal` always ALSO reach operators. **Content-state decision:** on the same-network path only `verified` is produced — anything the server cannot find is indistinguishable from nonexistent and already answered `NO-SUCH-TARGET` (invariant 1), so `unverified` (expired/ephemeral) and `reporter-attested` (e2ee) are wired through the codec + store but first *emitted* for bridged replicas (M5) / e2ee (M6). **Retention holds:** filing a `verified` report places refcounted holds on the reported root ± `HOLD_RADIUS` (=25) context roots; held roots are exempt from purge AND compaction until the report resolves + a 7-day grace, released by the maintenance scheduler (invariant 11). **Honest limitation:** live `REPORT-FILED` push reaches a queue's *default* handlers (ns owner / operators) only — delegated `reports`-cap holders fetch via `REPORTS LIST`, as there is no reverse cap→account index for fan-out (the same pull-not-push limit as the §2.4 recovery announcement). Reporter-identity anonymization toward ns handlers (§6.7 MAY) is deferred; handlers currently always see the reporter. Bridge `REPORT-FORWARD` (§11.9) is M5.

*Amendments (M4-5 implementation — namespaces + a channel-layout extension)*: NS CREATE carries the client-generated root pubkey in a `@root=<b64>` tag (§6.2 sketched the verb without it); default tier `unlisted`. Responses: NS CREATE/META/VISIBILITY → `NS-META <ns> <visibility>` with `owner=`/`title=`/`description=`/`icon=` tags; NS DELETE → an `NS-META … description=deleted` marker; DISCOVER → one `NS-META` per public namespace + `MORE <cursor>`; NS DELEGATE is sugar for `GRANT ns:<name>`. **Enforcement model (same-network, M4-5 scope):** the namespace *owner* account holds every cap within `ns:<name>` — the ns-scoped analog of an operator at `*`; the client-held root **key** is recorded (for TRANSFER/recovery/federation, later milestones) but same-network delegation uses the grant table, so — like operator authority — it is not yet cryptographically operator-unforgeable (that hardening comes with federation, M5). NS TRANSFER + the recovery ladder (§2.4) are M4c. **New extension — channel layout (Discord-style categories + order), Appendix A:** channels gain a `category` (free label) and `position` (integer); `CHANNEL META <#ns/chan> category|position :<value>` sets them; `CHANNELS <ns>` returns the ordered layout as `CHANNEL-LAYOUT <#chan> <position>` events (with a `category=` tag), sorted (category, position, name); private-namespace layouts are view-gated (invariant 1).

*Amendments (M4a implementation — capabilities, channels, invites)*: the loose §6.5/§6.3 syntax is pinned and the previously-unspecified response events are defined. GRANT `<subject> <scope> <caps> [expiry=<secs-ttl>]` → `@token=<b64> TOKEN <subject> <scope>`. REVOKE `<subject> <scope> [caps=<list>] [epoch]` (bare number bumps the scope epoch) → `TOKEN` reflecting the remaining caps (empty token = none). CHANNEL CREATE → `POLICY` (confirms name + policy); CHANNEL POLICY → `POLICY` (broadcast to members + labeled to actor); CHANNEL META → `CHANMETA <#chan> <key> :<value>`; CHANNEL DELETE → `CHANMETA <#chan> deleted :`. INVITE MINT → `@token=<invite-id> INVITED <scope> <invite-id> :<weft://…/i/ id>`; INVITE REVOKE → `INVITED … max-uses=0` (closed); INVITE REDEEM → the §6.3 JOIN response (auto-join). **Model notes (M4a scope):** operator accounts (weftd config `operators`) hold every cap at `*` — the network-key authority that bootstraps the grant chain (§11.3); the server keeps a grant table as the same-network enforcement fast path while the signed token is for delegation/federation; `ns:` scopes (GRANT/INVITE) and namespaced channels defer to M4b; invites are server-side id+counter records (the offline-verifiable unbound-token form is a federation concern, deferred). Reaction/emoji shortcodes and the `:` grammar clash remain §18 #8.

*Amendments (M3a implementation)*: §6.4 pins HISTORY's `key=value` middle-param syntax; §6.4 REACT emoji shortcodes travel bare (leading `:` collides with the §4 trailing marker — see §18 #8); §7 widens EDITED/DELETED/REACTION/REACTIONS targets to `<#chan|@user>` ahead of DM support; §7 documents that every line of a batch echoes the request label (data-page reading of §3.5).

*Amendments (M2 implementation)*: §6.1 defines the previously unspecified AUTH ENROLL response (`@attestation=` WELCOME, mirroring AUTH KEY success); §10.2 pins the `/.well-known/weft` document format (JSON: `protocol`, `network`, `signing-key`).

*Amendment (M1 implementation)*: §3.4 keepalive interval lowered from RECOMMENDED 60 s to **10 s** to match contemporary chat clients; the "2 missed = dead" rule scales accordingly (~30 s liveness window).

*Editorial (M0 implementation)*: §7 said "as v0.8" for the `TYPING`/`MARKED`/`PRESENCE`/`POLICY` event payloads, contradicting the "fully self-contained" claim; the table now spells them out as implemented by `weft-proto` (`TYPING <#chan> <user@net> <start|stop>`, `MARKED <#chan> <msgid>`, `PRESENCE <user@net> <status>`, `POLICY <#chan> <policy>`). `CHANMETA` remains deferred (M4).
