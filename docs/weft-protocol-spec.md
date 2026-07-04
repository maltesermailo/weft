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
| Permissions | Scoped capability tokens (signed CBOR, delegable, short-lived) — no role tables |
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
- Display strings: UTF-8, NFC on ingest. `\r`/`\n` forbidden in lines. Display names ≤128 B; topics ≤1024 B.

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
`PING [token]` → `PONG [token]` mandatory. RECOMMENDED 60 s interval, 2 missed = dead. QUIC keepalive may substitute for sending, not for answering.

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

Format: **Syntax · Args · Cap · Scope · Responses · Use**. Scope: **S**ession / **N**etwork / **NS** / **C**hannel / **F**ederation-operator. All commands accept `label`.

### 6.1 Session & identity (S/N)

**HELLO** `HELLO <version>` — §3.6.
**REGISTER** `REGISTER <account> :<password>` — password ≥12 B; needs config `registration: open` else `FORBIDDEN`. → `WELCOME` | `CONFLICT` | `POLICY`.
**AUTH PASSWORD** `AUTH PASSWORD <account> :<password>` — → `WELCOME` | `AUTH-FAILED` (constant-time).
**AUTH KEY** (challenge-response, binds device key):
```
C: AUTH KEY <account> <b64-ed25519-pubkey>
S: CHALLENGE <b64-nonce-32B>
C: AUTH PROOF <b64-sig(nonce ‖ network-name)>
S: @attestation=<b64> WELCOME hda.example
```
`nonce‖network-name` prevents cross-network replay. `AUTH ENROLL <b64-pubkey>` (while authed) adds a device.
**QUIT** `QUIT [:reason]`. **PING/PONG** §3.4.
**PRESENCE** `PRESENCE <online|away|dnd|invisible>` — same-network visibility only; never bridged; `invisible` renders offline.

### 6.2 Namespace commands (NS)

**NS CREATE** `NS CREATE <name> [public|unlisted|private]` — default `unlisted`. Cap: none (`open`, quota) / `ns-create` (`gated`). The client generates the **namespace root key** and submits its pubkey; the server records it as delegation root. → `NS-META` | `QUOTA` | `CONFLICT` | `FORBIDDEN`.
**NS META** `NS META <name> <title|description|icon> :<value>` — Cap `ns-admin`.
**NS VISIBILITY** `NS VISIBILITY <name> <tier>` — Cap `ns-admin`; → `private` applies anti-enumeration immediately.
**NS DELEGATE** `NS DELEGATE <name> <account|pubkey> <cap>[,...]` — sugar for `GRANT` at `ns:` scope.
**NS TRANSFER** `NS TRANSFER <name> <account>` — rung-1 succession; signed by current root.
**NS RECOVERY SET** `NS RECOVERY SET <name> <m> <key1,key2,...>` — designate M-of-N quorum (§2.4). Cap: root only. → `NS-META` (`recovery-set=yes` visible to members).
**NS RECOVER** `NS RECOVER <name> <b64-rotation-record>` — submit a quorum-signed (rung 2) or operator-signed (rung 3) rotation; starts the delay window. → `NS-META` announcement | `FORBIDDEN` (bad signatures) | `CONFLICT` (recovery already pending).
**NS RECOVERY CANCEL** `NS RECOVERY CANCEL <name>` — current root vetoes a pending recovery. Root signature only.
**NS DELETE** `NS DELETE <name> <name>` — confirmed; root or operator.
**DISCOVER** `DISCOVER [cursor]` — public directory; `MORE <cursor>` pagination.

### 6.3 Channel commands (C)

**CHANNEL CREATE** `CHANNEL CREATE <#chan> [policy]` — default `retained:90d`. Cap: `chan-create` at `*` (root) / `ns-admin` or `chan-create` at `ns:`. **JOIN never auto-creates.**
**CHANNEL POLICY** `CHANNEL POLICY <#chan> <policy> [purge]` — Cap `policy`. Tightening purges now; loosening applies to new events only; `e2ee` transitions need empty channel or `purge`.
**CHANNEL META** `CHANNEL META <#chan> <topic|view-gated> :<value>` — Cap `pin` / `ns-admin`. → `CHANMETA`.
**CHANNEL DELETE** `CHANNEL DELETE <#chan> <#chan>` — Cap `ns-admin`/operator.
**JOIN** `JOIN <#chan> [invite-ref]` — → `MEMBER` + `POLICY` + `count=` | `NO-SUCH-TARGET` | `BANNED`.
**PART** `PART <#chan> [:reason]`.
**MEMBERS** `MEMBERS <#chan> [cursor]` — paginated; bridge peers see remote members only as they've appeared.
**TYPING** `TYPING <#chan> <start|stop>` — Cap `send`; never stored; rate-limited (1/3 s RECOMMENDED); bridged only under manifest `typing: yes`.
**MARK** `MARK <#chan> <msgid>` — account-scoped read marker, synced via `MARKED`; survives `ephemeral`.

### 6.4 Messaging (C)

**MSG** `MSG <#chan|@user> [:body]` — tags `fmt=md`, `reply-to=`, `thread=`, `attach.N=` (≤10). Cap `send` (+`attach`). Empty body legal iff attachments. **Echo `MESSAGE` (with `msgid` + `label`) is the ack.** Errors: `CAP-REQUIRED`, `TOO-LARGE`, `THROTTLED` (`retry-after=`), `NO-SUCH-TARGET`.
**EDIT** `EDIT <msgid> :<new>` — Cap `edit-own` only (no `edit-any`, deliberately). Accepted only at the msgid's origin network; elsewhere `FORBIDDEN origin`. → `EDITED` broadcast.
**DELETE** `DELETE <msgid>` — Cap `delete-own` | `delete-any`. Tombstone. → `DELETED`.
**REACT / UNREACT** `REACT <msgid> <emoji>` — Unicode emoji ≤32 B or `:shortcode:` (ns emoji sets, open question). Cap `react`. Idempotent. → `REACTION op=add|remove` (live).
**HISTORY** `HISTORY <target> [before=] [after=] [limit=<≤500>] [thread=]` — target: channel or `@user`. Cap: membership / acked manifest bounded by `history` flag. → `BATCH START` … **compacted** events (§12.1) … `BATCH END [truncated]`. `truncated` marks retention gaps; silence about gaps is forbidden.
**STREAM** `STREAM OFFER <media|backfill> <mime> <bytes>` → `STREAM ACCEPT <token>` → data-plane transfer. HISTORY switches to STREAM above ~200 events (RECOMMENDED).

### 6.5 Capabilities & invites

**GRANT** `GRANT <account|pubkey> <scope> <cap>[,...] [expiry=<s>]` — scope `<#chan>` | `ns:<name>` | `*`; requires matching `grant:<cap>` at equal-or-wider scope (chain rule, cryptographic). → `TOKEN`.
**REVOKE** `REVOKE <account|pubkey> <scope> [caps] [epoch]` — stops refresh; `epoch` bumps the scope revocation epoch.
**INVITE MINT** `INVITE MINT <scope> [max-uses=] [expiry=]` — → `INVITED` (`weft://<net>/i/<b64>` link). Cap `invite`.
**INVITE REVOKE** `INVITE REVOKE <invite-id>` — closes counter; redeemed members unaffected.
**INVITE REDEEM** `INVITE REDEEM <b64>` — verifies chain + counter, mints member token **bound to redeemer's key**, auto-joins default channel. Dead invites → `NO-SUCH-TARGET` (indistinct).
Invite tokens = capability tokens with **unbound subject**: one object = single-use / expiring / vanity links; offline-verifiable authorization, never itself a membership credential.

### 6.6 Federation & operator (F)

**BRIDGE PROPOSE** `BRIDGE PROPOSE <scope> <peer> [history=from-epoch|full] [media=mirror|mirror-max:<B>|none] [typing=yes|no]` — snapshot manifest v1. Cap ladder §11.3. Errors: `BLOCKED`, `CAP-REQUIRED`.
**BRIDGE ACCEPT** `<peer> <version>` — live on mutual ack. **BRIDGE ADD** `<peer> <#chan>` — v+1, re-ack. **BRIDGE REMOVE** — v+1, unilateral, immediate. **BRIDGE SEVER** `<peer>` — unilateral teardown. All changes emit `MANIFEST` to affected members — mandatory.
**NETBLOCK** `NETBLOCK ADD <network> [:reason]` / `REMOVE` / `LIST` — Cap `netblock` (`*` scope only). Effects §11.6.
**VOICE** `VOICE JOIN|LEAVE <#chan>` / `VOICE DESC :<sdp>` — §16; feature-gated.


### 6.7 Moderation & reporting (C/NS/N)

**REPORT** `REPORT <msgid> <category> [scope] [:note]`
- **Args**: `msgid` — the reported message (local or bridged replica). `category` — normative set: `spam | harassment | violence | sexual | csam | illegal | self-harm | other` (extensible with `x-` prefix). `scope` — `ns` (namespace moderators, default) or `net` (network operator); categories `csam` and `illegal` are ALWAYS also routed to `net` regardless of scope, because the operator is the legally accountable party. `note` — optional free text ≤1024 B.
- **Cap**: channel membership (you can only report what you can see — view-gating and anti-enumeration apply unchanged: reporting an invisible msgid returns `NO-SUCH-TARGET`).
- **Routing**: the report goes to the **reporter's home network**, always — never directly to a remote network. Handlers are holders of the `reports` capability at the relevant scope (`ns:<name>` for ns-scope, `*` for net-scope); they receive a `REPORT-FILED` event.
- **Responses**: `REPORTED <report-id>` ack to the reporter (with `label`). Errors: `NO-SUCH-TARGET`, `THROTTLED` (reports are rate-limited per account; RECOMMENDED 10/hour), `QUOTA`.
- **Confidentiality**: the reported party is never notified by the protocol and MUST NOT be able to learn the reporter's identity from any protocol surface. Handlers see the reporter's account (accountability against report-flooding); network config MAY anonymize reporter identity toward ns-scope handlers while preserving it for the operator.

**Content states** (marked on the filed report, honestly):
- `verified` — the server still holds the reported event; a **retention hold** is placed (§12.1).
- `unverified` — the msgid is expired or the channel is `ephemeral`; nothing server-side confirms the content. The report is accepted and flagged; handlers weigh it accordingly.
- `reporter-attested` — `e2ee` channel: the server holds only ciphertext. The reporter MAY voluntarily attach the plaintext they saw (`REPORT ... :note` + a data-plane attachment for longer content); it is marked as reporter-provided, not server-verified. This is the honest limit of reporting inside host-blind channels; the alternative (server-readable "reportable e2ee") would break §14's unrepresentability guarantee and is rejected.

**REPORTS LIST** `REPORTS LIST <scope> [status=open|resolved] [cursor]` — paginated queue for handlers. Cap: `reports` at the scope. → `REPORT-FILED` page + `MORE`.
**REPORTS RESOLVE** `REPORTS RESOLVE <report-id> <action> [:note]` — `action`: `dismissed | content-removed | user-actioned | escalated`. Cap: `reports`. Resolving releases the retention hold (after a 7-day grace, RECOMMENDED). `escalated` re-routes an ns-scope report to net scope. → `REPORT-RESOLVED` to scope handlers; the reporter receives a minimal `REPORT-RESOLVED <report-id> <action>` (no handler identity, no note).

---

## 7. Events Reference

| Event | Payload | Notes |
|---|---|---|
| `WELCOME <network>` | `features=`, `attestation=` | → READY |
| `CHALLENGE <nonce>` | | keypair auth |
| `MESSAGE <#chan|@user> <user@net> :body` | `msgid=`, `reply-to=`, `thread=`, `attach.N=`, `fmt=`, `label=` (echo only); **in batches also `edited=<n>`, `edited-at=<ms>`** | echo = ack |
| `EDITED <#chan> <user@net> :new` | own `msgid=`, `edit-of=` | **live only** — compacted out of batches |
| `DELETED <#chan> <msgid>` | `by=` | tombstone; sole survivor of a deleted message in batches |
| `REACTION <#chan> <msgid> <emoji>` | `op=`, `by=` | **live only** |
| `REACTIONS <#chan> <msgid> <emoji> <count>` | `by=` (first ≤20 actors, comma-sep) | **batch summary form** (§12.1) |
| `MEMBER <#chan> <user@net> <join\|part>` | `display=` | |
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
| `BATCH START\|END` | `id=`, `truncated`, **`compacted`** | brackets HISTORY |
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
Ed25519 device keys; home network signs `{pubkey, account, network, expiry, sig}`; verified remotely via `https://<network>/.well-known/weft` (cached). No global identity server. Rotation = superseding attestation; revocation via well-known. Key rotation never evades NETBLOCK (name-keyed).

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
Deterministic CBOR, encode-before-sign (Biscuit = possible upgrade). Delegation via `grant:X`; chains verify to the namespace root key or network key. "Roles" = named token templates; editing re-mints on refresh. Revocation: short expiry + refresh (`TOKEN` events) + per-scope revocation epochs. Standard set: `send, edit-own, delete-own, delete-any, react, pin, invite, kick, ban, policy, view, attach, chan-create, reports, bridge, ns-admin, ns-create, netblock, grant:<cap>` (`netblock`: `*` only; `reports` grantable at `ns:` and `*`). View gating gets full anti-enumeration. **Capability checks precede side effects, always.**

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
8. Custom emoji sets per namespace.
9. Recovery-set privacy: should members see *who* the quorum is, or only that one exists? (Currently: existence only.)
10. Report data retention: how long do resolved reports themselves persist (distinct from content holds)? Legal-compliance minimums vary by jurisdiction — likely network config with a floor.
11. Name. WEFT remains a placeholder.

---

## Appendix A — Decision history

v0.1 core design → v0.2 namespaces + manifest bridging → v0.3 user-owned namespaces, visibility, invites → v0.4 NETBLOCK → v0.5 backfill + `history` flag → v0.6 media, mirroring, WEFT-IRC → v0.7 implementability audit → v0.8 consolidation → v0.9 namespace recovery ladder + message compaction → **v0.10 message reporting: home-network routing, retention holds, honest e2ee/ephemeral limits, bridge forwarding (this document)**.

*Editorial (M0 implementation)*: §7 said "as v0.8" for the `TYPING`/`MARKED`/`PRESENCE`/`POLICY` event payloads, contradicting the "fully self-contained" claim; the table now spells them out as implemented by `weft-proto` (`TYPING <#chan> <user@net> <start|stop>`, `MARKED <#chan> <msgid>`, `PRESENCE <user@net> <status>`, `POLICY <#chan> <policy>`). `CHANMETA` remains deferred (M4).
