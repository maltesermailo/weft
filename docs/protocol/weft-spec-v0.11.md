# WEFT Protocol — Specification v0.11 (Consolidated Edition)

*Fully self-contained; supersedes v0.10. **v0.11 is an editorial consolidation** — no wire-behavior change: it adds the previously-missing §11.10 (auto-federation) and §9.0 (invariant registry), folds every appendix-only rule into its home section, and repairs example/grammar inconsistencies. v0.10 added message reporting (§6.7, §11.9, retention holds in §12.1); v0.9 added the namespace recovery ladder (§2.4) and message compaction (§12.1). A client can be written from §0–§10; a server additionally requires §11–§17.*

**WEFT** (working name): a federated chat protocol combining IRC's operational simplicity with Discord's feature semantics. Design goals: small self-host footprint, sovereign networks, explicit consent for every federation act, privacy properties enforced by construction, and a control plane debuggable with netcat.

---

## 0. Conformance & Terminology

The key words **MUST**, **MUST NOT**, **REQUIRED**, **SHALL**, **SHALL NOT**, **SHOULD**, **SHOULD NOT**, **RECOMMENDED**, **MAY**, and **OPTIONAL** are to be interpreted as in RFC 2119 as clarified by RFC 8174: they carry that meaning only when in ALL CAPITALS.

Recurring terms, defined once:

| Term | Meaning |
|---|---|
| **namespace root key** | The client-generated Ed25519 key that owns a namespace (§2.1); every role, moderator token, channel policy, and invite chains from it. |
| **manifest** | The signed document naming exactly which channels a bridge shares, at what version, with what history/media policy (§11.1); the mutually-acked manifest gates all forwarding. |
| **home network** | The single network that mints the ULIDs — and therefore owns the order — for a scope: a channel's namespace-owning network (§11.13), or a group DM's creator network (§9.1, §11.12). |
| **spoke** | Any non-home member network of a cross-network channel or group DM; spokes relay posts to the home and mirror its minted order (§11.13, §11.12). |
| **compaction** | The post-audit-window rewrite of a channel's stored log into its surviving form: final bodies, per-emoji reaction summaries, tombstones (§12.1). |
| **retention hold** | The reporting-placed exemption that keeps an event (and its context) out of both purge and compaction until resolution + grace (§12.1). |
| **materialized view** | The compacted wire form batches carry (`HISTORY`/backfill): one `MESSAGE` per surviving message, `REACTIONS` summaries, `DELETED` tombstones — never edit chains (§12.1). |

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
- **Network**: a sovereign deployment identified by a DNS name (`test.example`). Owns accounts, hosts namespaces and channels, publishes its signing key, is the abuse-accountable party. **No global state**: nothing leaves a network except through an explicitly agreed bridge manifest.
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
- Display strings: UTF-8, NFC on ingest. Raw CR/LF are forbidden inside a line but representable in the **trailing** via the §4 escape table — CR (0x0D) → the two-character sequence `\r`, LF (0x0A) → `\n`, backslash → `\\` — so a message body may be multi-line: it is escaped on serialize and unescaped on parse, never reaching the transport as a raw break. Display names ≤128 B; topics ≤1024 B.

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
S: @features=media,backfill,voice,irc-gw WELCOME test.example :Willkommen
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
- Tag escaping (the IRCv3 message-tags convention): `;` → `\:`, space → `\s`, CR → `\r`, LF → `\n`, `\` → `\\`; unknown escapes drop the backslash; a dangling backslash is an error.
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

| Command | Syntax | Cap | → Result / notes | Example (`→` = direct response) |
|---|---|---|---|---|
| `HELLO` | `HELLO <version>` | — | Negotiates the protocol (§3.6). | `HELLO weft/1` → `@features=media,backfill WELCOME test.example :Willkommen` |
| `REGISTER` | `REGISTER <account> :<password>` | config | Password ≥ 12 B; needs `registration: open` else `FORBIDDEN`. Registration doubles as auth. → `WELCOME` \| `CONFLICT` \| `POLICY`. | `@label=r1 REGISTER ada :correct horse battery staple` → `@label=r1 WELCOME test.example` |
| `AUTH PASSWORD` | `AUTH PASSWORD <account> :<password>` | — | → `WELCOME` \| `AUTH-FAILED` (constant-time, uniform). | `@label=a1 AUTH PASSWORD ada :correct horse battery staple` → `@label=a1 WELCOME test.example` |
| `AUTH KEY` | `AUTH KEY <account> <b64-ed25519-pubkey>` | — | Begins device-key challenge-response (flow below). → `CHALLENGE`. | `AUTH KEY ada <b64-pubkey>` → `CHALLENGE <b64-nonce-32B>` |
| `AUTH PROOF` | `AUTH PROOF <b64-sig>` | — | Answers the challenge, signing `nonce ‖ network-name`. → `@attestation=<b64> WELCOME` \| `AUTH-FAILED`. | `AUTH PROOF <b64-sig>` → `@attestation=<b64> WELCOME test.example` |
| `AUTH ENROLL` | `AUTH ENROLL <b64-pubkey>` | authed | Adds a device to the current account. → `@attestation=<b64> WELCOME`. | `AUTH ENROLL <b64-pubkey>` → `@attestation=<b64> WELCOME test.example` |
| `QUIT` | `QUIT [:reason]` | — | Graceful close. | `QUIT :bye` (connection closes) |
| `PING` / `PONG` | `PING\|PONG [token]` | — | §3.4 keepalive; answering is mandatory. → `PONG`. | `PING 42` → `PONG 42` |
| `PRESENCE` | `PRESENCE <online\|away\|dnd\|invisible>` | — | Same-network visibility only; never bridged; `invisible` renders offline. The **event** side also carries `offline` (a disconnect broadcasts it; §7.1). | `PRESENCE away` → (broadcast) `PRESENCE ada@test.example away` |
| `VERIFY EMAIL` / `BIRTHDAY` | `VERIFY EMAIL <address>` / `VERIFY BIRTHDAY <YYYY-MM-DD>` | authed | Records a verification claim (§10.5): email → `pending` + a mailed one-time code; birthday → self-attested, `confirmed` on the spot. → `VERIFIED`. | `VERIFY EMAIL ada@example.com` → `@state=pending VERIFIED email ada@example.com` |
| `VERIFY CONFIRM` | `VERIFY CONFIRM <kind> <code>` | authed | Proves a pending claim with the mailed code (short-lived, single-use). → `VERIFIED` \| `NO-SUCH-TARGET`. | `VERIFY CONFIRM email 123456` → `@state=confirmed VERIFIED email ada@example.com` |
| `VERIFY LIST` | `VERIFY LIST` | authed | The caller's own claims — **owner-only** (subjects are PII, §10.5). → one `VERIFIED` per claim. | `VERIFY LIST` → `@state=confirmed VERIFIED email ada@example.com` (per claim) |

Device-key auth is a two-step challenge-response binding a device pubkey to the account; `nonce ‖ network-name` in the signed payload prevents cross-network replay:
```
C: AUTH KEY <account> <b64-ed25519-pubkey>
S: CHALLENGE <b64-nonce-32B>
C: AUTH PROOF <b64-sig(nonce ‖ network-name)>
S: @attestation=<b64> WELCOME test.example
```

### 6.2 Namespace commands (NS)

Signed NS verbs (`TRANSFER`, `RECOVERY CANCEL`) carry the root signature in a `@sig=<b64>` tag; `NS CREATE` carries the new root pubkey in `@root=<b64>` (§2.4, §10.4).

| Command | Syntax | Cap | → Result / notes | Example (`→` = direct response) |
|---|---|---|---|---|
| `NS CREATE` | `NS CREATE <name> [public\|unlisted\|private]` | none (`open`+quota) / `ns-create` (`gated`) | Default `unlisted`. Client generates the namespace **root key**, submits its pubkey (recorded as delegation root). → `NS-META` \| `QUOTA` \| `CONFLICT` \| `FORBIDDEN`. | `@root=<b64> NS CREATE gaming public` → `@owner=ada@test.example NS-META gaming public` |
| `NS META` | `NS META <name> <key> :<value>` | `ns-admin` | Keys: `title` / `description` / `icon` (free text); `categories` (comma-separated list — server-authoritative channel groups, see below); `federation` (`open`\|`closed`, §11.10 — `open` requires `public` visibility, else `FORBIDDEN`). → `NS-META`. | `NS META gaming title :Gaming Hub` → `@title=Gaming\sHub NS-META gaming public` |
| `NS VISIBILITY` | `NS VISIBILITY <name> <tier>` | `ns-admin` | → `private` applies anti-enumeration immediately. → `NS-META`. | `NS VISIBILITY gaming unlisted` → `NS-META gaming unlisted` |
| `NS DELEGATE` | `NS DELEGATE <name> <account\|pubkey> <cap>[,…]` | grant chain | Sugar for `GRANT` at `ns:` scope. → `TOKEN`. | `NS DELEGATE gaming bob ns-admin` → `@token=<b64> TOKEN bob ns:gaming` |
| `NS TRANSFER` | `NS TRANSFER <name> <account>` | root key | Rung-1 succession, root-signed. → `NS-META` (new owner). | `@sig=<b64> NS TRANSFER gaming bob` → `@owner=bob@test.example NS-META gaming unlisted` |
| `NS RECOVERY SET` | `NS RECOVERY SET <name> <m> <key1,key2,…>` | root | Designate the M-of-N quorum (§2.4). → `NS-META` (`recovery-set=yes`). | `NS RECOVERY SET gaming 2 <key1>,<key2>,<key3>` → `@recovery-set=yes NS-META gaming public` |
| `NS RECOVER` | `NS RECOVER <name> <b64-rotation-record>` | quorum / operator sig | Rung selection + windows: §2.4 (rung 2 delayed, rung 3 immediate). → `NS-META` \| `FORBIDDEN` (bad sig) \| `CONFLICT` (recovery already pending). | `NS RECOVER gaming <b64-rotation-record>` → `@recovery=pending;recovery-eta=<ms>;recovery-rung=2 NS-META gaming public` |
| `NS RECOVERY CANCEL` | `NS RECOVERY CANCEL <name>` | root key | Current root vetoes a pending recovery. | `@sig=<b64> NS RECOVERY CANCEL gaming` → `NS-META gaming public` (pending cleared) |
| `NS DELETE` | `NS DELETE <name> <name>` | `ns-admin` / operator | Confirmed by repetition. | `NS DELETE gaming gaming` → `@description=deleted NS-META gaming unlisted` |
| `NS JOIN` | `NS JOIN <name>` | membership | Auto-join every channel in the namespace the caller can see — view-gated and banned channels are skipped. → a `MEMBER` + `POLICY` per joined channel; no visible channel → `NO-SUCH-TARGET`. | `NS JOIN gaming` → `@count=1 MEMBER #gaming/general ada@test.example join` + `POLICY #gaming/general retained:90d` (per channel) |
| `DISCOVER` | `DISCOVER [cursor]` | — | Public namespace directory. → `NS-META` per ns + `MORE <cursor>`. | `@label=d1 DISCOVER` → `@label=d1;owner=ada@test.example NS-META gaming public` + `@label=d1 MORE <cursor>` |
| `CHANNELS` | `CHANNELS <name>` | view | Ordered channel layout of a namespace (extension; the response leads with the namespace's `NS-META`, see below). → `CHANNEL-LAYOUT` per channel. | `CHANNELS gaming` → `@category=Text CHANNEL-LAYOUT #gaming/general 0` (per channel) |
| `EMOJI ADD` / `REMOVE` | `EMOJI ADD <ns> <name> <media>` / `EMOJI REMOVE <ns> <name>` | `ns-admin` | Per-namespace custom emoji (§9.4): `name` = 1–32 of `[A-Za-z0-9_]`, `media` = a `weft-media://…` reference to an uploaded image. → `EMOJI` / `EMOJI-REMOVED`. | `EMOJI ADD gaming partyblob weft-media://test.example/<b3-hash>` → `EMOJI gaming partyblob weft-media://test.example/<b3-hash>` |
| `EMOJI LIST` | `EMOJI LIST <ns>` | authed | The namespace's emoji map (emoji aren't secret). → a `BATCH` of `EMOJI`. | `EMOJI LIST gaming` → `@id=b3 BATCH START` … `EMOJI gaming partyblob weft-media://test.example/<b3-hash>` … `@id=b3 BATCH END` |

The `NS RECOVER` rungs in brief (normative text: §2.4): a quorum-signed rotation record starts the 7-day rung-2 window (announced, root-cancellable); a network-key-signed record is rung 3 and **applies immediately** — no window, no pending state — permanently marked operator-initiated in `root-history`.

**Channel layout & server-authoritative categories (extension).**

- Channels carry a `category` (free label) and `position` (integer), set via `CHANNEL META` (§6.3).
- `CHANNELS <ns>` returns the ordered layout as `CHANNEL-LAYOUT` events, sorted by (category, position, name), **led by the namespace's `NS-META`** — so a client renders every group, including empty categories, purely from server state.
- The category *list* itself lives on the namespace: `NS META <ns> categories :<comma-list>`, echoed in NS-META's `cats=` tag.
- A `category`/`position` change **broadcasts `CHANNEL-LAYOUT`** to the channel's members — re-ordering reaches every client, not just the mover. Clients keep no category state of their own.
- Private-namespace layouts are view-gated (invariant 1).

### 6.3 Channel commands (C)

`CHANNEL CREATE`/`DELETE` are confirmed by repeating the name. **JOIN never auto-creates.**

| Command | Syntax | Cap | → Result / notes | Example (`→` = direct response) |
|---|---|---|---|---|
| `CHANNEL CREATE` | `CHANNEL CREATE <#chan> [policy]` | `chan-create` (`*`) / `ns-admin`\|`chan-create` (`ns:`) | Default policy `retained:90d`. → `POLICY`. | `CHANNEL CREATE #gaming/lounge retained:30d` → `POLICY #gaming/lounge retained:30d` |
| `CHANNEL POLICY` | `CHANNEL POLICY <#chan> <policy> [purge]` | `policy` | Tightening purges now; loosening applies to new events only; `e2ee` needs an empty channel or `purge`. → `POLICY`. | `CHANNEL POLICY #gaming/lounge permanent` → `POLICY #gaming/lounge permanent` |
| `CHANNEL META` | `CHANNEL META <#chan> <topic\|view-gated\|posting\|category\|position> :<value>` | `pin` / `ns-admin` | `category`/`position` = the layout extension (§6.2); `posting :restricted` = send-gated posting (§6.7). → `CHANMETA`. | `CHANNEL META #gaming/lounge topic :Hang out` → `CHANMETA #gaming/lounge topic :Hang out` |
| `CHANNEL DELETE` | `CHANNEL DELETE <#chan> <#chan>` | `ns-admin` / operator | → `CHANMETA … deleted`. | `CHANNEL DELETE #gaming/lounge #gaming/lounge` → `CHANMETA #gaming/lounge deleted :` |
| `CHANNEL RENAME` | `CHANNEL RENAME <#old> <#new>` | `ns-admin` / operator | Change a channel's identity within its namespace; server re-keys every scoped record (grants, membership, roles, holds, pins, history). → `CHANNEL-RENAMED <#old> <#new>` (broadcast to members + labeled to actor). | `CHANNEL RENAME #gaming/lounge #gaming/cafe` → `CHANNEL-RENAMED #gaming/lounge #gaming/cafe` |
| `JOIN` | `JOIN <#chan> [invite-ref]` | membership / invite | → `MEMBER` (`count=` tag) + `POLICY` \| `NO-SUCH-TARGET` \| `BANNED`. | `@label=j1 JOIN #gaming/general` → `@count=42;label=j1 MEMBER #gaming/general ada@test.example join` + `POLICY #gaming/general retained:90d` |
| `PART` | `PART <#chan> [:reason]` | — | → `MEMBER … part`. | `PART #gaming/general :later` → `MEMBER #gaming/general ada@test.example part` |
| `MEMBERS` | `MEMBERS <#chan> [cursor]` | membership | The **persistent roster** (offline members included) as a `BATCH` of `MEMBER … join` rows, each followed by a `PRESENCE` line for its dot; accounts deduped across devices. Non-member of an existing channel → `CAP-REQUIRED view`; hidden/nonexistent → `NO-SUCH-TARGET`. Bridge peers see remote members only as they've appeared. | `MEMBERS #gaming/general` → `@id=m1 BATCH START` … `@count=42 MEMBER #gaming/general ada@test.example join` + `PRESENCE ada@test.example online` … `@id=m1 BATCH END` |
| `TYPING` | `TYPING <#chan> <start\|stop>` | `send` | Never stored; rate-limited (1/3 s RECOMMENDED); bridged only under manifest `typing: yes`. | `TYPING #gaming/general start` → (broadcast) `TYPING #gaming/general ada@test.example start` |
| `MARK` | `MARK <#chan> <msgid>` | membership | Account-scoped read marker, synced via `MARKED`; survives `ephemeral`. | `MARK #gaming/general test.example/01J…A` → `MARKED #gaming/general test.example/01J…A` |
| `UNREAD` | `UNREAD [<#chan>]` | membership | Request server-computed unread counts → one `UNREAD-COUNTS` per channel. No channel = every joined channel. Absent channel must be joined, else `NO-SUCH-TARGET`. | `UNREAD #gaming/general` → `UNREAD-COUNTS #gaming/general 3 1` |

**Membership is durable (normative).** `JOIN` records a persistent `(account, channel)` membership; `PART` and a forced part (kick / channel-scope ban) clear it. On auth the server **auto-rejoins** the account to its stored channels, so channels reappear on reconnect with no client-side re-join. Join/part announcements **dedupe by account** — a second device joining (or an auto-rejoin while another device is online) broadcasts no fresh `MEMBER`, and member counts are distinct-account counts.

**Presence vs. membership (Discord-style).** The roster is the *persistent* membership; online-ness is "holds a live session in the channel". A **disconnect** broadcasts `PRESENCE <user> offline` (membership retained) — only an explicit `PART`/kick/ban emits `MEMBER … part` (roster removal); a reconnect of an existing member broadcasts `PRESENCE <user> online`, not a fresh join. The presence map is live-only (§6.1: never stored, never bridged); a live `invisible` member reads `offline`.

**Unread counts are pushed, not only polled.** A per-channel `UNREAD-COUNTS` follows each `MARKED` in the login snapshot (§9.7), and a fresh count rides the cross-device `MARK` sync to the account's *other* sessions — so badges survive reload/reconnect and stay consistent across devices. What is counted:

- `unread` — real root messages from *other* senders; own messages and join/part system rows are excluded.
- `mentions` — the subset referencing the account (`@account`) or `@everyone`/`@here`; a body-text heuristic — there is no structured mention field.

### 6.4 Messaging (C)

The echoed `MESSAGE` — same `label`, server-assigned `msgid` — is the ack; broadcast copies to other members carry no label (§3.5).

| Command | Syntax | Cap | → Result / notes | Example (`→` = direct response) |
|---|---|---|---|---|
| `MSG` | `MSG <#chan\|@user> [:body]` + tags `fmt=md` `reply-to=` `thread=` `attach.N=` (≤10) | `send` (+`attach`) | Empty body legal iff attachments. **The echoed `MESSAGE` (with `msgid` + `label`) is the ack.** → `MESSAGE`; errors `CAP-REQUIRED` `TOO-LARGE` `THROTTLED` (`retry-after=`) `NO-SUCH-TARGET`. | `@label=x MSG #gaming/general :gg` → `@label=x;msgid=test.example/01J…A MESSAGE #gaming/general ada@test.example :gg` |
| `EDIT` | `EDIT <msgid> :<new>` | `edit-own` | No `edit-any` (deliberate). Honored only at the msgid's origin network; elsewhere `FORBIDDEN origin`. → `EDITED`. | `EDIT test.example/01J…A :gg all` → `@edit-of=test.example/01J…A;msgid=test.example/01J…E EDITED #gaming/general ada@test.example :gg all` |
| `DELETE` | `DELETE <msgid>` | `delete-own` \| `delete-any` | Tombstone. → `DELETED`. | `DELETE test.example/01J…A` → `@by=ada@test.example DELETED #gaming/general test.example/01J…A` |
| `REACT` / `UNREACT` | `REACT <msgid> <emoji>` | `react` | Unicode emoji ≤ 32 B; shortcodes travel **bare** (leading `:` collides with the §4 trailing marker — §18 #8). Idempotent. → `REACTION op=add\|remove` (live). | `REACT test.example/01J…A 🎉` → `@by=ada@test.example;op=add REACTION #gaming/general test.example/01J…A 🎉` |
| `HISTORY` | `HISTORY <target> [before=] [after=] [limit=≤500] [thread=]` | membership / acked manifest | `key=value` middle params, any order, unknown keys ignored; target = channel or `@user`. → `BATCH START` … **compacted** events (§12.1) … `BATCH END [truncated]`. `truncated` marks gaps — silence about them is forbidden. | `@label=h1 HISTORY #gaming/general limit=50` → `@id=b1;label=h1 BATCH START` … (compacted events) … `@compacted;id=b1;label=h1 BATCH END` |
| `PIN` / `UNPIN` | `PIN <msgid>` | `pin` | Pin/unpin a message in its channel (resolved from the msgid). → `PINNED` (`by=` tag — the local account) / `UNPINNED`, broadcast to members. | `PIN test.example/01J…A` → `@by=ada PINNED #gaming/general test.example/01J…A` |
| `PINS` | `PINS <#chan>` | membership | The pinned messages. → `BATCH START` … `MESSAGE` per pin … `BATCH END`. | `PINS #gaming/general` → `@id=b5 BATCH START` … `MESSAGE` per pin … `@id=b5 BATCH END` |
| `SEARCH` | `SEARCH <#chan> :<query>` | membership | Message search in a channel. → `BATCH START` … `MESSAGE` per match (newest-first, ≤50) … `BATCH END`. | `SEARCH #gaming/general :gg` → `@id=b6 BATCH START` … `MESSAGE` per match … `@id=b6 BATCH END` |
| `THREADS` | `THREADS <#chan>` | membership | The channel's threads (roots with ≥1 reply), most-recently-active first (§9.4). → a `BATCH` of `THREAD`. | `THREADS #gaming/general` → `@id=b4 BATCH START` … `@replies=4 THREAD #gaming/general test.example/01J…A :Bug triage` … `@id=b4 BATCH END` |
| `THREAD NAME` | `THREAD NAME <#chan> <root> [:name]` | `can_post` (§6.7) | Set — or, with the trailing omitted/empty, clear — a thread's display name (§9.4); the root must exist, else `NO-SUCH-TARGET`. → `THREAD-NAMED` broadcast. | `THREAD NAME #gaming/general test.example/01J…A :Bug triage` → `THREAD-NAMED #gaming/general test.example/01J…A :Bug triage` |
| `STREAM` | `STREAM OFFER <media\|backfill> <mime> <bytes>` | — | → `STREAM ACCEPT <token>` → data-plane transfer. HISTORY switches to STREAM above ~200 events (RECOMMENDED). | `STREAM OFFER media image/png 20480` → `STREAM ACCEPT s_9f3c…` |

### 6.5 Capabilities & invites (§10.4)

| Command | Syntax | Cap | → Result / notes | Example (`→` = direct response) |
|---|---|---|---|---|
| `GRANT` | `GRANT <account\|pubkey\|user@net> <scope> <cap>[,…] [expiry=<s>]` | `grant:<cap>` at ≥ scope | Scope `<#chan>` \| `ns:<name>` \| `*`; the chain rule is cryptographic. → `TOKEN`. | `GRANT bob #gaming/general send,react expiry=86400` → `@token=<b64> TOKEN bob #gaming/general` |
| `REVOKE` | `REVOKE <account\|pubkey\|user@net> <scope> [caps=<list>] [epoch]` | grant chain | Stops refresh; a bare `epoch` number bumps the scope revocation epoch. → `TOKEN` (remaining caps). | `REVOKE bob #gaming/general caps=react` → `@token=<b64> TOKEN bob #gaming/general` (remaining caps) |
| `INVITE MINT` | `INVITE MINT <scope> [max-uses=] [expiry=]` | `invite` | → `INVITED` (`@token=`, link `weft://<net>/<ns>/i/<b64>` — the namespace is embedded so a *foreign* redeemer can auto-federate to it, §11.10; top-level channels have no `<ns>` and use `weft://<net>/i/<b64>`). | `INVITE MINT ns:gaming max-uses=10 expiry=604800` → `@max-uses=10;token=<b64> INVITED ns:gaming iv_01J… :weft://test.example/gaming/i/<b64>` |
| `INVITE REVOKE` | `INVITE REVOKE <invite-id>` | `invite` | Closes the counter; already-redeemed members unaffected. | `INVITE REVOKE iv_01J…` (counter closed) |
| `INVITE REVOKE-ALL` | `INVITE REVOKE-ALL <scope>` | `invite` | Bulk-closes every invite for the scope's namespace (`ns:<name>` + its `#<ns>/<chan>` scopes) in one shot. → an `INVITED` ack with `invite-id=*`, `max-uses=0`. Already-redeemed members unaffected. | `INVITE REVOKE-ALL ns:gaming` → `INVITED` ack (`invite-id=*`, `max-uses=0`) |
| `INVITE REDEEM` | `INVITE REDEEM <b64>` | — | Verifies chain + counter, mints a member token **bound to the redeemer's key**, auto-joins the default channel. Dead invites → `NO-SUCH-TARGET` (indistinct). | `INVITE REDEEM <b64>` → `@count=43 MEMBER #gaming/general bob@test.example join` |
| `CAPS` | `CAPS <account> <scope>` | — (public) | An account's **effective** capabilities at a scope (operators/ns-owners expand to all); caps aren't secret — powers client badges. → `CAPS`. | `CAPS bob ns:gaming` → `CAPS bob ns:gaming :send,react,invite` |

Invite tokens are capability tokens with an **unbound subject**: one object serves single-use / expiring / vanity links — offline-verifiable authorization, never itself a membership credential.

#### 6.5.1 Roles — named capability-token bundles

A **role** is a named, colored bundle of capability tokens at a scope — `(scope, name, color, caps)` — giving clients human-readable labels over §10.4 capabilities. Three rules define the model:

- **Enforcement stays purely token-based.** Assigning a role grants exactly its `caps` as ordinary tokens; every permission check is a pure capability-token check — no role tables in the *enforcement* path.
- **Membership is explicit, never derived.** An account wears a role because it was *assigned* (`ROLE ASSIGN` / `ROLE UNASSIGN`, recorded server-side) — never because its caps happen to be a superset of the bundle. Deriving membership from caps was rejected: it wrongly marks owners/operators (who hold every cap implicitly) as wearing every role, and can't distinguish a coincidental cap match from an intended assignment.
- **The assignment record is display metadata.** It drives rendering and propagation; it is never consulted for a permission decision.

| Command | Syntax | Cap | → Result / notes | Example (`→` = direct response) |
|---|---|---|---|---|
| `ROLE CREATE` | `ROLE CREATE <scope> <color> <cap>[,…] [hoist=0\|1] [pos=<n>] :<name>` | `ns-admin` at scope | Define/replace a role (upsert on `(scope, name)`). `color` is a display hint (e.g. `#e8b93d`); optional `hoist=` (Discord-style "display members separately in the member list") + `pos=` (sort position, ascending) are key=value middle params defaulting to `0`; `name` (may contain spaces) rides the trailing. → updated `ROLES` batch. | `ROLE CREATE ns:gaming #e8b93d send,react hoist=1 pos=0 :Speaker` → `ROLE ns:gaming #e8b93d send,react hoist=1 pos=0 :Speaker` |
| `ROLE REORDER` | `ROLE REORDER <scope> :<name1,name2,…>` | `ns-admin` at scope | Set each named role's `pos` to its index in the list. → updated `ROLES` batch. | `ROLE REORDER ns:gaming :Speaker,Regular` |
| `ROLE DELETE` | `ROLE DELETE <scope> :<name>` | `ns-admin` at scope | Remove a definition **and all its assignments**. Already-granted tokens are unaffected (revoke separately). → updated `ROLES` batch. | `ROLE DELETE ns:gaming :Speaker` |
| `ROLE RENAME` | `ROLE RENAME <scope> :<old>,<new>` | `ns-admin` at scope | Change a role's display name **in place**, carrying its definition *and every assignment* (rationale below). Absent `<old>` → `NO-SUCH-TARGET`; a `<new>` naming a live role → `POLICY`. → updated `ROLES` batch. | `ROLE RENAME ns:gaming :Regular,Member` (assignments carried) |
| `ROLE ASSIGN` | `ROLE ASSIGN <scope> <account> :<name>` | `grant:<cap>` for each cap | Record membership + grant the role's tokens (identical authority + `TOKEN` path as `GRANT`). At a **namespace** scope also propagates channel role-permissions (below). | `ROLE ASSIGN ns:gaming bob :Speaker` → `@token=<b64> TOKEN bob ns:gaming` |
| `ROLE UNASSIGN` | `ROLE UNASSIGN <scope> <account> :<name>` | `ns-admin` at scope | Drop membership + revoke the role's caps (bundle + its channel-role caps). → `ROLE-MEMBER`. | `ROLE UNASSIGN ns:gaming bob :Speaker` → `ROLE-MEMBER ns:gaming bob :` |
| `ROLES` | `ROLES <scope>` | — (public) | → a `BATCH` of `ROLE <scope> <color> <caps> hoist=0\|1 pos=<n> :<name>` (definitions, position-ordered). | `ROLES ns:gaming` → `@id=b7 BATCH START` … `ROLE ns:gaming #e8b93d send,react hoist=1 pos=0 :Speaker` … `@id=b7 BATCH END` |
| `ROLES-OF` | `ROLES-OF <scope> <account>` | — (public) | The roles an account is assigned at a scope → `ROLE-MEMBER <scope> <account> :<comma-names>`. | `ROLES-OF ns:gaming bob` → `ROLE-MEMBER ns:gaming bob :Speaker` |

The `ROLE` event carries a definition; the `ROLE-MEMBER` event carries an account's explicit assignments. Clients render pills from the intersection.

**Why `ROLE RENAME` is server-side.**

- Roles are keyed by `(scope, name)`: a client-side delete + re-create would silently drop every assignment — so the rename is one server-side migration carrying the definition *and* all membership rows together.
- Already-granted tokens need no migration: a role's authority is its `caps`, which a rename leaves untouched (consistent with `ROLE DELETE` also leaving granted tokens alone).
- Both names ride the trailing as a comma pair (the `ROLE REORDER` convention) — a role name may contain spaces but **not** a comma; `old` as a middle param would have made spaced names unrenameable.
- Merging two bundles under one name is not a rename (hence `POLICY`), and the cap check precedes both existence probes (invariant 4) — neither can be used to enumerate roles.

**Role channel-permissions.** Two roles of the **same name** — one at a namespace, one at a channel — compose to give the Discord "role X has permission Y *in channel Z*" override, without a rules engine:

- A role `Speaker` at `ns:s` carries namespace-wide caps; a role `Speaker` at `#s/stage` (same name) carries caps *for that channel only*.
- Assigning the namespace role grants both: `ROLE ASSIGN ns:s <account> :Speaker` grants the `ns:s` bundle **and** every same-named channel role's caps on `#s/*`.
- Editing a channel role re-grants it to every current member of the namespace role **immediately** (through the membership records) — so a newly-added channel permission reaches existing holders with no re-assignment.

Enforcement stays token-based (§10.4): a namespace scope covers its channels; a channel scope covers only itself.

### 6.6 Federation & operator (F)

Bridge sessions authenticate with `AUTH BRIDGE` (§11.2). Every bridge change emits `MANIFEST` to affected members — mandatory (§11.5). The proposing side carries the signed manifest in a `@manifest=<b64>` tag.

Commands marked *(bridge)* run only inside an authenticated bridge session; the rest are ordinary operator/user commands.

| Command | Syntax | Cap | → Result / notes | Example (`→` = direct response) |
|---|---|---|---|---|
| `AUTH BRIDGE` | `AUTH BRIDGE <peer-network> <b64-pubkey>` | pinned / accept-any | Opens a bridge session — challenge-response as `AUTH KEY`, verified against the peer's network key (§11.2). | `AUTH BRIDGE peer.example <b64-pubkey>` → `CHALLENGE <b64-nonce>` (then `AUTH PROOF <b64-sig>` → `WELCOME`; the session is now a bridge session) |
| `BRIDGE PROPOSE` *(bridge)* | `BRIDGE PROPOSE <scope> <peer> [history=from-epoch\|full] [media=mirror\|mirror-max:<B>\|none] [typing=yes\|no]` | ladder §11.3 | Snapshot manifest v1; omitted params default strictest-safe (`history=from-epoch`, `media=none`, `typing=no`). → `MANIFEST`; errors `BLOCKED` `CAP-REQUIRED`. | `@manifest=<b64> BRIDGE PROPOSE ns:gaming peer.example history=full media=mirror` → `@channels=#gaming/general;history=full;media=mirror;typing=no;voice=no MANIFEST peer.example 1 live` |
| `BRIDGE ACCEPT` *(bridge)* | `BRIDGE ACCEPT <peer> <version>` | ladder | Live on mutual ack. | `BRIDGE ACCEPT peer.example 1` |
| `BRIDGE ADD` *(bridge)* | `BRIDGE ADD <peer> <#chan>` | ladder | v+1, requires re-ack before forwarding. | `BRIDGE ADD peer.example #gaming/clips` |
| `BRIDGE REMOVE` *(bridge)* | `BRIDGE REMOVE <peer> <#chan>` | ladder | v+1, unilateral, immediate. | `BRIDGE REMOVE peer.example #gaming/clips` |
| `BRIDGE SEVER` *(bridge)* | `BRIDGE SEVER <peer>` | ladder | Unilateral teardown. | `BRIDGE SEVER peer.example` |
| `BRIDGE REQUEST` *(bridge)* | `BRIDGE REQUEST <ns>` | bridge session | §11.10 — ask the peer to offer a manifest for one of *its* namespaces; offered iff auto-federation-reachable, always with `history=full` (rationale in §11.10); else `NO-SUCH-TARGET` \| `BLOCKED`. | `BRIDGE REQUEST gaming` → `@manifest=<b64> BRIDGE PROPOSE ns:gaming peer.example history=full media=none typing=no` \| `NO-SUCH-TARGET` |
| `FEDERATE` | `FEDERATE <network>/<namespace>` | membership; `auto_bridge` open | §11.10 — a local user asks their **home** network to auto-establish an on-demand bridge to a foreign namespace. Gated on NETBLOCK + a per-account cooldown; the bridge lands asynchronously (→ `MANIFEST` on the affected channels). Errors `UNSUPPORTED` (auto-federation off / self-network) `BLOCKED` `THROTTLED`. | `FEDERATE peer.example/gaming` → (ack) then `MANIFEST` on the channels (async) |
| `NETBLOCK` | `NETBLOCK ADD <network> [:reason]` / `REMOVE <network>` / `LIST` | `netblock` (`*` only) | Effects §11.6. → `NETBLOCKED`. | `NETBLOCK ADD evil.example :abuse` → `NETBLOCKED evil.example` |
| `MEDIA` | `MEDIA BLOCK <hash> [:reason]` / `UNBLOCK <hash>` / `BLOCKS` | `media-block` (`*` only) | §13 hash moderation: block deletes the blob + thumbnail and rejects re-upload + mirror (content = identity). → `MEDIA-BLOCKED`. | `MEDIA BLOCK <b3-hash> :csam` → `MEDIA-BLOCKED <b3-hash>` |
| `REPORT-FORWARD` *(bridge)* | `REPORT-FORWARD <report-id> <msgid> <category> [:note]` | bridge session | Forward a report to the origin over the bridge; reporter identity stripped (§11.9). Bridge-session-only. | `REPORT-FORWARD r_01J… peer.example/01J…A harassment :context` |
| `FSESSION` *(bridge)* | `FSESSION <fsid> OPEN <account>` / `CMD :<line>` / `REPLY :<line>` / `CLOSE` | bridge session | §11.11 — multiplex a federated user's **control** session over the bridge (homeserver authority). `F` opens/relays; `H` attributes each `CMD` to `account@F` and enforces against its own grants. Carries commands + their direct replies only (broadcast events ride the mirror); the user never connects to `H` (IP non-exposure). Bridge-session-only. | `FSESSION 1 CMD :GRANT bob ns:gaming send` → `FSESSION 1 REPLY :@token=<b64> TOKEN bob ns:gaming` |
| `VOICE` | `VOICE JOIN\|LEAVE <#chan>` / `VOICE DESC <#chan> :<sdp>` | feature-gated | §16 — `JOIN` answers with a `VOICE OFFER` (endpoint + short-lived media token); `DESC` is the SDP exchange. | `VOICE JOIN #gaming/stage` → `@mode=livekit VOICE OFFER #gaming/stage <token> :wss://sfu.test.example` |

### 6.7 Moderation & reporting (C/NS/N)

| Command | Syntax | Cap | → Result / notes | Example (`→` = direct response) |
|---|---|---|---|---|
| `REPORT` | `REPORT <msgid> <category> [scope] [:note]` | membership | Routed to the reporter's home network. → `REPORTED <report-id>`; errors `NO-SUCH-TARGET` `THROTTLED` (10/hr RECOMMENDED) `QUOTA`. | `REPORT test.example/01J…B harassment ns :being rude` → `REPORTED r_01J…` |
| `REPORTS LIST` | `REPORTS LIST <scope> [status=open\|resolved] [cursor]` | `reports` at scope | The handler queue. → `REPORT-FILED` page + `MORE`. `scope` is the concrete cap scope (`ns:<name>` or `*`). | `REPORTS LIST ns:gaming status=open` → `@scope=ns;state=verified REPORT-FILED r_01J… test.example/01J…B harassment` + `MORE <cursor>` |
| `REPORTS RESOLVE` | `REPORTS RESOLVE <report-id> <action> [:note]` | `reports` | Releases the retention hold after a 7-day grace (RECOMMENDED). → `REPORT-RESOLVED`. | `REPORTS RESOLVE r_01J… content-removed :removed` → `@by=ada@test.example;note=removed REPORT-RESOLVED r_01J… content-removed` |
| `MODLIST` | `MODLIST <scope>` | `mute` **or** `ban` at scope | List the current deny-list (mutes + bans) at a scope. → a `BATCH` of `MODERATED` (one per live entry, `by=`/`reason=` populated); non-moderators get `CAP-REQUIRED`. | `MODLIST ns:gaming` → `@id=b2 BATCH START` … `@by=ada@test.example;reason=spam MODERATED ns:gaming bob mute` … `@id=b2 BATCH END` |
| `MUTE` / `UNMUTE` | `MUTE <scope> <account> [:reason]` | `mute` at scope | Deny/allow `send`. `scope` = `#chan\|ns:<name>\|*` (a `*` mute is network-wide). → `MODERATED`. | `MUTE #gaming/general bob :spamming` → `@by=ada@test.example;reason=spamming MODERATED #gaming/general bob mute` |
| `BAN` / `UNBAN` | `BAN <scope> <account> [:reason]` | `ban` at scope | Deny/allow join + send; a fresh channel-scope ban force-parts the target. → `MODERATED`; blocked joins get `BANNED`. | `BAN ns:gaming bob :repeated` → `@by=ada@test.example;reason=repeated MODERATED ns:gaming bob ban` (+ `MEMBER … part`) |
| `KICK` | `KICK <#chan> <account> [:reason]` | `kick` | Force-part (no persistent state — may rejoin). → `MODERATED`. | `KICK #gaming/general bob :cool off` → `@by=ada@test.example;reason=cool\soff MODERATED #gaming/general bob kick` |

**How posting permission composes.** A message is allowed only when all three of these hold:

```
can_post  =  not muted   AND   not banned   AND   (channel is open   OR   sender holds `send`)
```

Two independent surfaces feed that check:

- **Deny-list (mute / ban)** — per-account blocks keyed by `(scope, account)`. A block applies to a channel if its scope *covers* that channel: the channel itself (`#chan`), its namespace (`ns:<name>`), or the whole network (`*`). That covering rule is also *who moderates what* — a `*` block is set by network moderators, `ns:` by namespace moderators, `#chan` by channel moderators.
- **Restricted posting** — `CHANNEL META <#chan> posting :restricted` flips a channel from open to send-gated. Posting then requires the `send` capability, so `GRANT send` / `REVOKE send` (+ epoch, §10.4) decides who may speak — e.g. an announcements channel.

A **mute always denies**, whatever the posting mode. Kick and ban broadcast a `MEMBER … part` so the target sees the removal; the acting moderator gets a `MODERATED <scope> <account> <mute\|unmute\|ban\|unban\|kick>` echo (`by=`/`reason=` tags).

**`REPORT` arguments.**

| Argument | Values | Notes |
|---|---|---|
| `category` | `spam` · `harassment` · `violence` · `sexual` · `csam` · `illegal` · `self-harm` · `other` | Normative set; extensible with an `x-` prefix. |
| `scope` | `ns` (default) \| `net` | Routing hint: namespace moderators vs. the network operator. `csam` and `illegal` are ALWAYS *also* routed to `net` — the legally accountable party. |
| `note` | free text | Optional, ≤ 1024 B. |

You can only report what you can see: reporting is membership-gated, and an invisible or absent msgid returns `NO-SUCH-TARGET` (anti-enumeration unchanged). Handlers are the holders of the `reports` cap at the concrete scope (`ns:<name>` or `*`).

**Where a report lands:**

- `ns`-scope on a namespaced channel → the namespace owner;
- `ns`-scope on a top-level channel or a DM → the operators;
- `net`-scope → the operators;
- `csam` / `illegal` → **also** the operators, always.

**Honest limitation:** the live `REPORT-FILED` push reaches a queue's *default* handlers (ns owner / operators); delegated `reports`-cap holders fetch via `REPORTS LIST` — pull, not push.

**`REPORTS RESOLVE` actions:** `dismissed` · `content-removed` · `user-actioned` · `escalated`.

- `escalated` re-routes an ns-scope report up to net scope — the report stays open, holds intact.
- Handlers receive the full `REPORT-RESOLVED` (`by=`, `note=`); the reporter receives the minimal form — no handler identity, no note.

**Content states** (marked honestly on the filed report):

| State | Meaning |
|---|---|
| `verified` | The server still holds the reported event; a **retention hold** is placed (§12.1). |
| `unverified` | The msgid is expired or the channel is `ephemeral` — nothing server-side confirms the content. Accepted and flagged; handlers weigh it accordingly. |
| `reporter-attested` | `e2ee` channel: the server holds only ciphertext. The reporter MAY voluntarily attach the plaintext they saw (marked reporter-provided, not server-verified). The alternative — server-readable "reportable e2ee" — would break §14's unrepresentability guarantee and is rejected. |

**Confidentiality.** The reported party is never notified and MUST NOT learn the reporter's identity from any protocol surface. Handlers see the reporter's account (accountability against report-flooding); a network MAY anonymize the reporter toward ns-scope handlers while preserving it for the operator.

### 6.8 Social layer — friends, group DMs, calls (S)

The social layer is keyed on **`user@network`** so every relationship federates. These are *account*-level (not channel/namespace) surfaces: a friendship, a group DM, and a call are properties of the accounts involved, and the same-network path works standalone while the cross-network path rides the group-federation tunnel (§11.12). Conceptual flow diagrams (**non-normative** supplements): `weft-protocol-flows.md` §13, `weft-federation-flows.md`.

**Friends.** A symmetric relationship with a pending → accepted handshake.

| Command | Syntax | Cap | → Result / notes | Example (`→` = direct response) |
|---|---|---|---|---|
| `FRIEND ADD` | `FRIEND ADD <user@net>` | — | Sends (or, if they already requested you, accepts) a friend request. → `FRIEND <user> outgoing\|friends`. | `FRIEND ADD bob@peer.example` → `FRIEND bob@peer.example outgoing` |
| `FRIEND ACCEPT` | `FRIEND ACCEPT <user@net>` | — | Accepts an inbound (`incoming`) request. → `FRIEND <user> friends`. | `FRIEND ACCEPT carol@test.example` → `FRIEND carol@test.example friends` |
| `FRIEND REMOVE` | `FRIEND REMOVE <user@net>` | — | Removes a friend or cancels/declines a request. → `FRIEND-REMOVED <user>`. | `FRIEND REMOVE bob@peer.example` → `FRIEND-REMOVED bob@peer.example` |
| `FRIENDS` | `FRIENDS` | — | Roster snapshot. → a `FRIEND <user> <friends\|incoming\|outgoing>` per relationship. | `FRIENDS` → `FRIEND carol@test.example friends` (per relationship) |

**Group DMs.** An ad-hoc, named, multi-party conversation whose members are full `user@network` references. Group messages form their own history scope, minted single-writer like DMs (§9.1); the group's **home** = its creator's network (§11.12). Ordinary `MSG &<group>`, `EDIT`/`DELETE`/`REACT`, and `HISTORY &<group>` all apply to a group target.

| Command | Syntax | Cap | → Result / notes | Example (`→` = direct response) |
|---|---|---|---|---|
| `GROUP CREATE` | `GROUP CREATE <user@net> [<user@net> …]` | — | Creates a group with the caller + listed members (≥1 member required). → `GROUP <&id> :name` + `GROUP-MEMBER` to each. | `GROUP CREATE bob@peer.example carol@test.example` → `GROUP &01J…G :ada@test.example bob@peer.example carol@test.example` |
| `GROUP ADD` | `GROUP ADD <&id> <user@net>` | member | Adds a member. → `GROUP-MEMBER <&id> <user> join`. | `GROUP ADD &01J…G dave@test.example` → `GROUP-MEMBER &01J…G dave@test.example join` |
| `GROUP REMOVE` | `GROUP REMOVE <&id> <user@net>` | member | Removes a member. → `GROUP-MEMBER <&id> <user> part`. | `GROUP REMOVE &01J…G dave@test.example` → `GROUP-MEMBER &01J…G dave@test.example part` |
| `GROUP LEAVE` | `GROUP LEAVE <&id>` | member | The caller leaves. → `GROUP-MEMBER … part`. | `GROUP LEAVE &01J…G` → `GROUP-MEMBER &01J…G ada@test.example part` |
| `GROUP NAME` | `GROUP NAME <&id> [:name]` | member | Sets/clears the group name (empty trailing clears). → `GROUP <&id> :name`. | `GROUP NAME &01J…G :Weekend Crew` → `@name=Weekend\sCrew GROUP &01J…G :ada@test.example bob@peer.example` |
| `GROUPS` | `GROUPS` | — | The caller's group list. → a `GROUP` per membership. | `GROUPS` → `@name=Weekend\sCrew GROUP &01J…G :ada@test.example bob@peer.example` (per group) |

Membership changes on a group with remote members propagate to every member network via `GROUP SYNC` (§11.12).

**Calls (1:1 & group).** Signaling is protocol; media is LiveKit (§16). A call's `CALL-MEDIA` credential is minted **per participant** and delivered only to that participant — never broadcast. Cross-network calls bridge room-to-room through a relay so client IPs never cross (§11.12, §16).

| Command | Syntax | Cap | → Result / notes | Example (`→` = direct response) |
|---|---|---|---|---|
| `CALL` | `CALL <user@net>` | friends | Places a 1:1 call. → `CALL-RING` to callee; `CALL-STATE ringing` to caller. | `CALL bob@peer.example` → `CALL-STATE bob@peer.example ringing` (callee gets `CALL-RING`) |
| `CALL ACCEPT` | `CALL ACCEPT <user@net>` | — | Answers a ringing call. → `CALL-STATE active` + `CALL-MEDIA` to each party. | `CALL ACCEPT ada@test.example` → `CALL-STATE ada@test.example active` + `@mode=livekit CALL-MEDIA …` |
| `CALL DECLINE` | `CALL DECLINE <user@net>` | — | Rejects. → `CALL-STATE declined`. | `CALL DECLINE ada@test.example` → `CALL-STATE ada@test.example declined` |
| `CALL END` | `CALL END <user@net>` | — | Hangs up. → `CALL-STATE ended`. | `CALL END bob@peer.example` → `CALL-STATE bob@peer.example ended` |
| `GROUP CALL` | `GROUP CALL <&id>` | member | Starts or joins the group's voice call. → `GROUP-CALL <&id> <self> active` + `CALL-MEDIA` + roster; other members are rung. | `GROUP CALL &01J…G` → `GROUP-CALL &01J…G ada@test.example active` + `@mode=livekit CALL-MEDIA …` |
| `GROUP HANGUP` | `GROUP HANGUP <&id>` | member | Leaves the group call. → `GROUP-CALL <&id> <self> ended`. | `GROUP HANGUP &01J…G` → `GROUP-CALL &01J…G ada@test.example ended` |

The federated forms of `CALL`/`GROUP CALL` carry the callee/host network's pre-minted LiveKit credential as `room=`/`token=`/`endpoint=` tags (an internal relay detail, §11.12); a client never sets those.

---

## 7. Events Reference

Events are the server→client half of the protocol; a client **MUST ignore any event it does not recognize** (forward-compat, §4). Events are grouped below by concern. The **Example** column is a concrete wire line; `…` between lines abbreviates omitted events. A *direct* response echoes the request `label` (§3.5) — shown where relevant; *broadcast* copies never do.

**Key=value convention (normative).** *Commands* carry optional `key=value` pairs as **middle params**, shown in their Syntax (`HISTORY limit=`, `GRANT expiry=`, `INVITE MINT max-uses=`, `ROLE CREATE hoist=`). *Events* carry them as **tags** (`@key=value`, before the verb, §4) — every `key=` in a Payload/tags column below is a tag unless it appears in the event's Syntax as a middle param. The sole event-side exception is `ROLE`, whose `hoist=`/`pos=` echo the command's middle-param form.

### 7.1 Session & identity

| Event | Payload / tags | Example |
|---|---|---|
| `WELCOME <network>` | `features=`, `attestation=` — enters READY | `@features=media,backfill,voice WELCOME test.example :Willkommen` |
| `CHALLENGE <nonce>` | keypair auth nonce (§6.1) | `CHALLENGE <b64-nonce-32B>` |
| `PONG [token]` | keepalive answer (§3.4) | `PONG 42` |
| `PRESENCE <user@net> <status>` | `online\|away\|dnd\|invisible\|offline`; never bridged. A disconnect broadcasts `offline` (membership retained, §6.3); reconnect broadcasts `online`; a live `invisible` member renders `offline`. | `PRESENCE ada@test.example away` |
| `MEDIA TOKEN <bearer>` | per-session media fetch bearer, pushed after auth (§13) | `MEDIA TOKEN <bearer>` |
| `VERIFIED <kind> <subject>` | `state=pending\|confirmed`; a verification claim — `email`/`birthday`/… (§10.5). Owner-only (subjects are PII). | `@state=confirmed VERIFIED email ada@example.com` |

### 7.2 Messaging & mutations

| Event | Payload / tags | Example |
|---|---|---|
| `MESSAGE <#chan\|@user> <user@net> :body` | `msgid=`, `reply-to=`, `thread=`, `attach.N=`, `fmt=`, `label=` (echo only); **in batches** `edited=<n>`, `edited-at=<ms>` | `@label=x;msgid=test.example/01J…A MESSAGE #gaming/general ada@test.example :gg` |
| `EDITED <#chan\|@user> <user@net> :new` | `msgid=` (the edit's own id), `edit-of=` (the root) — **live only** (compacted out of batches) | `@edit-of=test.example/01J…A;msgid=test.example/01J…E EDITED #gaming/general ada@test.example :gg all` |
| `DELETED <#chan\|@user> <msgid>` | `by=` — tombstone; the sole survivor in batches | `@by=ada@test.example DELETED #gaming/general test.example/01J…A` |
| `REACTION <#chan\|@user> <msgid> <emoji>` | `op=add\|remove`, `by=` — **live only** | `@by=ada@test.example;op=add REACTION #gaming/general test.example/01J…A 🎉` |
| `REACTIONS <#chan\|@user> <msgid> <emoji> <count>` | `by=` (first ≤20 actors, comma-sep) — **batch summary form** (§12.1) | `@by=ada@test.example,bob@test.example REACTIONS #gaming/general test.example/01J…A 🎉 3` |

### 7.3 Membership, presence & reads

| Event | Payload / tags | Example |
|---|---|---|
| `MEMBER <#chan> <user@net> <join\|part>` | `display=`, `count=` (members after the change) | `@count=42 MEMBER #gaming/general ada@test.example join` |
| `TYPING <#chan> <user@net> <start\|stop>` | never stored; bridged only under manifest `typing:yes` | `TYPING #gaming/general ada@test.example start` |
| `MARKED <#chan> <msgid>` | read-marker sync to the account's own sessions | `MARKED #gaming/general test.example/01J…A` |
| `UNREAD-COUNTS <#chan> <unread> <mentions>` | server-computed tally since the marker; pushed on login + `MARK` | `UNREAD-COUNTS #gaming/general 3 1` |
| `POLICY <#chan> <policy>` | sent on join and on policy change (§5.2) | `POLICY #gaming/general retained:90d` |

### 7.4 Namespace & channel

| Event | Payload / tags | Example |
|---|---|---|
| `NS-META <ns> <visibility>` | `owner=`, `title=`, `description=`, `icon=`, `cats=`, `federation=`, `recovery-set=`, `recovery=pending`, `recovery-eta=`, `recovery-rung=`, `root-history` | `@owner=ada@test.example;title=Gaming\sHub NS-META gaming public` |
| `CHANMETA <#chan> <key> :<value>` | key ∈ `topic`/`view-gated`/`posting`/`category`/`position`/`deleted` | `CHANMETA #gaming/general topic :Welcome` |
| `CHANNEL-LAYOUT <#chan> <position>` | `category=`, `kind=` (`voice` for voice channels, §16) — ordered layout (§6.2) | `@category=Text CHANNEL-LAYOUT #gaming/general 0` |
| `CHANNEL-RENAMED <#old> <#new>` | broadcast to members + labeled to the actor | `CHANNEL-RENAMED #gaming/lounge #gaming/cafe` |
| `PINNED` / `UNPINNED <#chan> <msgid>` | `by=` on `PINNED` (the pinning **account**, local) | `@by=ada PINNED #gaming/general test.example/01J…A` |
| `THREAD <#chan> <root> [:name]` | `replies=<n>`, `last=<msgid>` — from `THREADS` (§9.4) | `@last=test.example/01J…Z;replies=4 THREAD #gaming/general test.example/01J…A :Bug triage` |
| `THREAD-NAMED <#chan> <root> [:name]` | live thread (re)label | `THREAD-NAMED #gaming/general test.example/01J…A :Bug triage` |
| `EMOJI` / `EMOJI-REMOVED <ns> <name> [<media>]` | per-namespace custom emoji map (§6.2, §9.4) | `EMOJI gaming partyblob weft-media://test.example/<b3-hash>` |

### 7.5 Capabilities, invites & roles

| Event | Payload / tags | Example |
|---|---|---|
| `TOKEN <subject> <scope>` | `@token=<b64>`, `expiry=` — the signed cap token from `GRANT`/`REVOKE`/`ROLE ASSIGN` (§10.4) | `@token=<b64-cap-token> TOKEN bob #gaming/general` |
| `INVITED <scope> <invite-id>` | `@token=<b64>` (required), `max-uses=`, `expiry=`; redeem link in the trailing | `@token=<b64> INVITED ns:gaming iv_01J… max-uses=10 :weft://test.example/gaming/i/<b64>` |
| `ROLE <scope> <color> <caps> :<name>` | `hoist=`, `pos=` — a role definition | `ROLE ns:gaming #e8b93d send,react hoist=1 pos=0 :Speaker` |
| `ROLE-MEMBER <scope> <account> :<names>` | an account's explicit role assignments | `ROLE-MEMBER ns:gaming bob :Speaker` |
| `CAPS <account> <scope> :<caps>` | effective caps at a scope (public; badges) | `CAPS bob ns:gaming :send,react,invite` |

### 7.6 Federation & operator

| Event | Payload / tags | Example |
|---|---|---|
| `MANIFEST <peer> <version> <state>` | state ∈ `live\|added\|removed\|severed`; tags `channels=`, `history=`, `media=`, `typing=`, `voice=`; announced to affected members on any bridge change (§11.5) | `@channels=#gaming/general;history=from-epoch;media=mirror;typing=no;voice=no MANIFEST peer.example 2 live` |
| `NETBLOCKED <network>` | `:reason` — the four §11.6 effects fired | `NETBLOCKED evil.example :abuse` |
| `MEDIA-BLOCKED <hash>` | `:reason` — hash moderation (§13) | `MEDIA-BLOCKED <b3-hash> :csam` |

### 7.7 Moderation & reports

| Event | Payload / tags | Example |
|---|---|---|
| `MODERATED <scope> <account> <action>` | `mute\|unmute\|ban\|unban\|kick`; `by=`, `reason=` — to the acting moderator (a join blocked by a ban returns the `ERR BANNED` code, §8) | `@by=ada@test.example;reason=spam MODERATED #gaming/general bob mute` |
| `REPORTED <report-id>` | `label=` — ack to the reporter | `REPORTED r_01J…` |
| `REPORT-FILED <report-id> <msgid> <category>` | `state=verified\|unverified\|reporter-attested`, `reporter=` (per config), `scope=` — to `reports` holders | `@scope=ns;state=verified REPORT-FILED r_01J… test.example/01J…B harassment` |
| `REPORT-RESOLVED <report-id> <action>` | handlers get `by=`/`note=`; the reporter gets the minimal form | `@by=ada@test.example REPORT-RESOLVED r_01J… content-removed` |

### 7.8 Social layer (§6.8)

| Event | Payload / tags | Example |
|---|---|---|
| `FRIEND <user@net> <state>` | `friends\|incoming\|outgoing`; pushed on any change | `FRIEND bob@peer.example friends` |
| `FRIEND-REMOVED <user@net>` | a friendship or pending request ended | `FRIEND-REMOVED bob@peer.example` |
| `GROUP <&id> :<members>` | `name=` tag; members space-separated in the trailing — group roster snapshot | `@name=Weekend\sCrew GROUP &01J…G :ada@test.example bob@peer.example carol@test.example` |
| `GROUP-MEMBER <&id> <user@net> <join\|part>` | group membership change, to members | `GROUP-MEMBER &01J…G dave@test.example join` |
| `CALL-RING <from@net> <room>` | incoming 1:1 call; `room` = the ad-hoc voice room | `CALL-RING ada@test.example call:01J…R` |
| `CALL-STATE <user@net> <state>` | `ringing\|active\|declined\|ended\|busy` | `CALL-STATE bob@peer.example active` |
| `CALL-MEDIA <room> <token> :<endpoint>` | `mode=livekit`; **per-participant**, never broadcast; absent when signaling-only | `@mode=livekit CALL-MEDIA call:01J…R <token> :wss://sfu.test.example` |
| `GROUP-CALL <&id> <user@net> <active\|ended>` | a member's presence in the group call | `GROUP-CALL &01J…G ada@test.example active` |

### 7.9 History & data pages

| Event | Payload / tags | Example |
|---|---|---|
| `BATCH START` / `BATCH END` | `id=` on both; `truncated`, `compacted` (valueless flags) on END; every line of a batch echoes the request label (§3.5) | `@id=b1 BATCH START` … `@compacted;id=b1;truncated BATCH END` |
| `STREAM ACCEPT <token>` | data-plane handoff (large HISTORY / media, §6.4/§13) | `STREAM ACCEPT s_9f3c…` |
| `MORE <cursor>` | pagination continuation (DISCOVER / REPORTS LIST / …) | `MORE <cursor>` |

### 7.10 Voice

| Event | Payload / tags | Example |
|---|---|---|
| `VOICE OFFER <#chan> <token> [:endpoint]` | `mode=` (`livekit`; omitted = `webrtc`), `room=` — the media grant answering `VOICE JOIN` (§16): endpoint + short-lived token | `@mode=livekit VOICE OFFER #gaming/stage <token> :wss://sfu.test.example` |
| `VOICE DESC <#chan> :<sdp>` | the SFU's SDP answer (§16) | `VOICE DESC #gaming/stage :<sdp>` |

### 7.11 Errors & control

| Event | Payload / tags | Example |
|---|---|---|
| `ERR <CODE> [ctx] :text` | `label=`, `retry-after=`, `max=` — the error registry is §8 | `@label=x ERR NO-SUCH-TARGET #gaming/secret :no such target` |

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

### 9.0 Protocol invariants (normative)

The document cites these by number ("invariant N"). Each is normative wherever the surface it names exists, and each is meant to be enforced **as a test**, not just as code. Numbers 5–7 and 10 are cited nowhere in this document and are left `[reserved]` rather than reconstructed.

| # | Invariant | Statement |
|---|---|---|
| 1 | **Anti-enumeration** | "A private thing you're not in" MUST be indistinguishable from "does not exist": one code (`NO-SUCH-TARGET`, §8), one timing envelope — covering nonexistent, private, view-gated, expired/foreign msgids, and dead invites (§2.2). The same uniformity extends to the data plane: a bad media bearer, a non-member fetch, an absent blob, and a spent backfill token are one not-found (§13, §11.7). The presence of hidden things never leaks. |
| 2 | **Origin authority** | A message belongs to its origin: `msgid = <origin>/<ULID>` (§5.1). Bridged events keep their origin msgids (never re-minted); events on a bridge are accepted only when `msgid` origin = the authenticated peer. For a channel the origin is its **home** (§11.13): the home is the sole minter and enforces `EDIT`/`DELETE` by **authored-by** (the relay leg vouches `sender@net`), while spoke replicas honor only home-origin events (`FORBIDDEN origin` elsewhere) (§11.4). Backfilled events are verified exactly like live traffic (§11.7). |
| 3 | **Forwarding gate** | A channel is forwardable to a peer iff it appears in **both** the last mutually-acked manifest snapshot and the current one; the same gate applies to ingestion and to backfill (§11.1, §11.7). Forwarding outside it is a protocol violation, not a soft failure. |
| 4 | **Caps before effects** | Capability checks precede side effects — and precede existence probes, so a permission check can never be used to enumerate hidden things (§10.4; e.g. the §6.5.1 `ROLE RENAME` error order). |
| 5–7 | *[reserved — recover from repo history or retire]* | Not cited in this document. |
| 8 | **E2EE host-blindness survives everything** | Server-readable plaintext for an `e2ee` channel is *unrepresentable* (§5.2, §14): no server search, embeds, thumbnails, or compaction — and recovery (§2.4) restores administration, never history. |
| 9 | **No silent root rotation** | Every namespace root rotation is announced (`NS-META`); delayed rungs add a mandatory public window and a current-root veto; rung 3 drops the window but is network-key-authorized and permanently audit-marked operator-initiated in `root-history` (§2.4). |
| 10 | *[reserved — recover from repo history or retire]* | Not cited in this document. |
| 11 | **Holds outrank retention** | Reported content and its context are exempt from purge **and** compaction until resolution plus grace; holds are invisible on every protocol surface and travel with their content (e.g. across a channel rename) (§12.1, §6.3). |
| 12 | **Reporter confidentiality** | The reported party never learns the reporter's identity from any protocol surface; bridge-forwarded reports strip it by default (§6.7, §11.9). |
| 13 | **SSRF classifier guard** | Every server-side fetch of a user-influenced URL (auto-federation dial + well-known fetch, unfurl proxy) MUST classify every resolved address before connecting and refuse non-public targets; testable as a pure classifier (§11.10, §13). |

### 9.1 Ordering
Per-channel **total order** = the **home actor's** ULID order; bridged replicas preserve it. A channel's **home** is the network that owns its namespace, and that network is the **sole ULID writer** for the channel (§11.13) — exactly as a group DM's home is its creator's network (§11.12). Remote members' posts are relayed to the home to be minted into the one order; they are never minted independently by a spoke. No cross-channel guarantees. DMs: total order per (network, pair).

### 9.2 Delivery & acks
- **Send:** `MSG` + `label` → the echoed `MESSAGE` (same label, assigned msgid) *is* the ack. No echo → resend with the **same** label; servers dedup `(session, label)` for 5 minutes → effectively exactly-once.
- **Receive:** dedup by msgid.
- **Backpressure:** a lagging client gets `SLOW` + a forced HISTORY resync; never unbounded buffering.

### 9.3 Message model (event sourcing)
Edits/deletes/reactions are new events referencing the original msgid — never in-place mutation — **on the live path**; storage and batches use the compacted materialization (§12.1). Replies: `reply-to=`. **Threads are views, not channels**: `thread=` tag, no separate membership, `HISTORY thread=` filter.

### 9.4 Rich content
UTF-8, optional `fmt=md` (CommonMark subset); oversize → `TOO-LARGE`, never truncation. Link embeds are server-generated sub-events — clients never implicitly fetch third-party URLs (the server-side unfurl proxy, §13, exists for exactly this reason).

**Threads** are views, not channels (§9.3):

- A reply is an ordinary channel `MESSAGE` carrying `thread=<root>` — it broadcasts to the channel like any message, so every member and bridge sees it; clients MAY hide replies from the main timeline (an "N replies" indicator) as presentation.
- `HISTORY <#chan> thread=<root>` returns just the thread (root + replies, oldest-first).
- A thread may carry an optional **display name** — metadata keyed by the root msgid, never a new identity — set/cleared via `THREAD NAME`, listed via `THREADS` (§6.4); naming is authorized by the same rule as posting (`can_post`, §6.7).

**Custom emoji** are per-namespace (`EMOJI ADD/REMOVE/LIST`, §6.2): clients render `:name:` as an inline image in bodies **and reactions** — a custom-emoji reaction's key is the literal `:name:` string, so the reaction model is unchanged.

### 9.5 DMs (v1)
`MSG @user`, same network only; network-config retention (default `permanent`); both accounts, all devices; `HISTORY @user` symmetric; edits/deletes/reactions/replies yes, threads no.

**Cross-network note (honest).** True 1:1 cross-network DMs remain deferred (§18 #7: consent + routing without a channel manifest). In practice, a **two-member cross-network group DM** (§6.8, §11.12) already carries the conversation — the group tunnel is the current cross-network path. What stays deferred is the 1:1 DM *semantics*: the default-`permanent` retention rule, the symmetric `HISTORY @user` surface, and the no-threads rule above are specified for same-network DMs only.

### 9.6 Time
Server-stamped via ULIDs; client clocks untrusted.

### 9.7 Client reconnect (RECOMMENDED)
1. Reconnect with jittered backoff (1 → 60 s), then `HELLO` → `AUTH KEY`.
2. The server replays `MEMBER`/`POLICY` snapshots — membership is server-side (§6.3).
3. Per channel: `HISTORY after=<last msgid>`; render `truncated` as a visible gap.
4. Resend unacked labels (§9.2 dedup makes this safe).
5. The `MARKED` snapshot restores read state; each marked channel is followed by an `UNREAD-COUNTS`, so badges survive the reconnect.

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
{ "protocol": "weft/1", "network": "test.example", "signing-key": "<b64-ed25519-pubkey>" }
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

- **Encoding:** deterministic CBOR, encode-before-sign (Biscuit is a possible future upgrade).
- **Delegation:** via `grant:X`; chains verify up to the namespace root key or the network key.
- **Roles** (§6.5.1) are named token templates; editing a role re-mints on refresh. A role's holder may be a **foreign `account@network`** — membership and granted caps key by that subject, so a partner network's user can wear a role here (§6.5).
- **Revocation:** short expiry + refresh (`TOKEN` events) + per-scope revocation epochs.
- **Standard capability set:** `send`, `edit-own`, `delete-own`, `delete-any`, `react`, `pin`, `invite`, `kick`, `ban`, `mute`, `policy`, `view`, `attach`, `chan-create`, `reports`, `bridge`, `ns-admin`, `ns-create`, `netblock`, `media-block`, `grant:<cap>`.
- **Scope floors:** `netblock` / `media-block` at `*` only; `reports` grantable at `ns:` and `*`; `mute`/`ban`/`kick` at `#chan`/`ns:`/`*` — the moderation tiers (§6.7).
- View gating gets full anti-enumeration (invariant 1). **Capability checks precede side effects, always** (invariant 4).

### 10.5 Account verification (email / age)

Accounts carry **verification claims** — `(kind, subject, state)` where `kind` is an open namespace (`email`, `birthday`, …), `subject` is what's claimed (an address, a birth date), and `state` is `pending` | `confirmed`. Two proof models:

- **Server-proven (`email`):** `VERIFY EMAIL <address>` records a `pending` claim and mails a one-time code; `VERIFY CONFIRM email <code>` proves it (`confirmed`). The code is short-lived (15 min), single-use, in-memory (a restart just means re-request).
- **Self-attested (`birthday`):** `VERIFY BIRTHDAY <YYYY-MM-DD>` records + `confirms` on the spot — honestly self-declared, not server-proven (a server cannot verify age without an ID provider, §18).

`VERIFY LIST` returns the caller's own claims (one `VERIFIED <kind> <subject>` per claim, `@state=`). **Subjects are PII** (email address, birth date) → returned **only to the owner's own session**, never broadcast. This is **badge-only**: claims do not gate channel/cap access yet (an age-gate is a later policy extension). Mail delivery is a deployment concern; a server with no mailer configured still records the claim (a development server may surface the code out-of-band).

---

## 11. Federation — Scoped Bridging

**Tunnels at a glance.** Two networks share **one** authenticated bridge session (§11.2 — `AUTH BRIDGE`, ALPN `weft/1` over QUIC stream 0 or WS). Every control-plane tunnel below is multiplexed on that single link; media rides two *separate* planes. Each tunnel is one-directional in intent (the return of an effect is usually a **fresh** delivery, not a threaded reply — see the social layer, §11.12):

```
              NETWORK  H                                     NETWORK  P
  ┌─ one bridge session ─── AUTH BRIDGE (proves the network key) ───────────────┐
  │                                                                            │
  │        manifest control        ◄───── PROPOSE / ACCEPT / REQUEST ─────►    │  §11.1
  │                                        ADD / REMOVE / SEVER  (+ MANIFEST)
  │                                                                            │
  │        event mirror            ──── H-origin events ──────────────────►    │  §11.4
  │        (one hop, local-origin) ◄──────────────── P-origin events ─────     │       MESSAGE/EDITED/
  │                                                                            │       DELETED/REACTION/PROFILE
  │        history backfill        ──── HISTORY ──────────────────────────►    │  §11.7
  │                                ◄──────────── BATCH / STREAM ───────────     │       bounded scrollback
  │                                                                            │
  │        report forwarding       ──── REPORT-FORWARD ───────────────────►    │  §11.9  (reporter stripped)
  │                                                                            │
  │        FSESSION — admin        ──── OPEN / CMD ───────────────────────►    │  §11.11 foreign user's
  │        (homeserver authority)  ◄──────────────────── REPLY ───────────     │       control/admin cmds
  │                                                                            │
  │        FSESSION — social       ──── OPEN / CMD ───────────────────────►    │  §11.12 FRIEND*/CALL*/
  │        (friend-delivery, 1-way)    (fire-and-forget; effects return as         GROUP SYNC/RELAY/
  │                                     a NEW reverse delivery)                    MUT/BACKFILL/ROSTER
  └────────────────────────────────────────────────────────────────────────────┘
  ═══════════════════════ separate data / media planes ══════════════════════════
           media mirror           ──── MIRROR <hash> (self-auth) ────────►       §11.8  blob bytes (pull)
           voice relay            ◄═══════════ audio (LiveKit cascade) ══════►    §16    IP-safe, server↔server
```

| Tunnel | Direction | Carries | Frames / verbs | Gate | § |
|---|---|---|---|---|---|
| **Bridge session** | ↔ base link | everything below | `AUTH BRIDGE` + `CHALLENGE`/`PROOF` | peer proves its **network key** (pinned or accept-any) | 11.2 |
| **Manifest control** | ↔ either side proposes | the shared channel/namespace set + history/media policy | `BRIDGE PROPOSE`/`ACCEPT`/`REQUEST`/`ADD`/`REMOVE`/`SEVER`; `MANIFEST` to members | signed manifest, scope-authority-signed | 11.1 |
| **Event mirror** | → each way (home-origin only) | live channel events, fanned out by the home | `MESSAGE`/`EDITED`/`DELETED`/`REACTION`/`PROFILE`… | manifest-gated ∩ acked; **one hop from the home**; origin preserved | 11.4 |
| **Channel relay** | spoke → home (mint), home → spokes (ingest) | a spoke member's channel post/mutation, sent to the home to be minted into the one order | `CHANNEL RELAY`/`MUT`/`BACKFILL` (`@echo` ack; `@id` absent = mint, present = ingest) | home = namespace owner's network is sole ULID writer; authored-by vouched by sender's network key | 11.13 |
| **History backfill** | pull (req→origin, data←) | bounded scrollback for a shared channel | `HISTORY` → `BATCH` \| `STREAM ACCEPT`+`BACKFILL` | acked manifest ∧ `history` flag ∧ origin retention | 11.7 |
| **Report forwarding** | → home→origin | a forwarded report | `REPORT-FORWARD` | reporter identity stripped (invariant 12) | 11.9 |
| **FSESSION — admin** | → `CMD`, ← `REPLY` | a foreign user's control/admin commands (moderation, `GRANT`/`REVOKE`, ns/channel admin, invites, roles, reports) | `FSESSION OPEN`/`CMD`/`REPLY`/`CLOSE` | the foreign actor `account@F`, checked against **H's** grant store (homeserver authority) | 11.11 |
| **FSESSION — social** | → one-way (fire-and-forget) | friends, calls, the group tunnel | `FSESSION OPEN`/`CMD` carrying `FRIEND*`/`CALL*`/`GROUP SYNC/RELAY/MUT/BACKFILL/ROSTER` | same homeserver authority; return = a new reverse delivery | 11.12 |
| **Media mirror** | pull (req→origin, bytes←) | content-addressed blob bytes | `MIRROR <requester-net> <hash> <sig>` (self-authenticating) | requester proves its network key; BLAKE3-verified | 11.8 |
| **Voice relay** | ↔ audio | real-time audio, room-to-room | LiveKit cascade leg (server↔server) | separate media plane; clients never cross networks | 16 |

The rest of this section specifies each in turn.

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

A bridge session is opened with **`AUTH BRIDGE <peer-network> <b64-network-pubkey>`** on the same acceptor path as clients: the peer asserts its network signing key and proves control via the §6.1 `CHALLENGE`/`AUTH PROOF` flow (signing `nonce ‖ our-network`). Success yields a **bridge session, not an account session**, bound to the proven key — manifests received on it verify against that key.

Two configurable trust modes: **pinned** (default/closed) accepts only configured peers whose asserted key matches the pin; **accept-any** (open federation) accepts any non-blocked network on the key it proves control of — trust-on-first-use, with `NETBLOCK` (§11.6) as the escape hatch. A pin always wins over accept-any. Every failure funnels to the uniform `AUTH-FAILED` (no peer-existence oracle).

The `bridge` *capability* plays no role in session authentication — it is the §11.3 authorization to **propose** channel-scope manifests.

### 11.3 Authorization ladder
Proposing a manifest requires authority proportional to its blast radius: a single `#channel` needs a `bridge` capability holder; an `ns:<name>` scope needs the namespace root; a `*` (whole-network) scope needs the network signing key itself. The wider the scope a bridge shares, the stronger the signature that must stand behind it — blast radius is priced in signatures. The ladder is enforced *locally* on the proposing side; the wire manifest is uniformly **network-key-signed**, so the peer verifies it against the signer's well-known key.

### 11.4 Event flow
Origin msgids + attestations intact, verified against the origin's well-known key. **A channel's origin is its home** (the namespace owner's network, §11.13): the home mints every message into one total order and is the single point that fans it out one hop to each spoke. A remote member's post reaches the home over the **relay leg** (`CHANNEL RELAY`, §11.13), not the mirror; the mirror then carries the home-minted event. Because the home is the origin, a home→spoke copy is **one hop from origin** — this is what lets a channel with members on three or more networks deliver every message to everyone (a non-home spoke's post could otherwise never reach a second spoke without the forbidden transitivity). EDIT/DELETE are honored by the home on an **authored-by** basis (the relaying network vouches `sender@net`; the home applies iff the sender authored the target or holds the moderation cap) and minted into the same order — not gated on a per-spoke msgid origin. Retention → strictest. `e2ee` bridges only pass-through MLS. Per-user attestation blocks without touching the manifest. **No transitivity — one hop from origin, loops structurally impossible, no shared state to resolve.**

### 11.5 Visibility interaction
Private/unlisted namespaces may bridge (root-signed only); their manifests are confidential — peers MUST NOT list their channels. `MANIFEST` notification to members on any audience change.

### 11.6 NETBLOCK
The operator's blocklist of remote networks — each entry `{network, private reason, added, actor}`. Blocking a network fires **all four effects (normative)**:

1. Bridge **proposals are rejected**, both directions (`ERR BLOCKED`).
2. Existing **manifests are severed** — members get `MANIFEST`, owners get `NETBLOCKED`.
3. The network's **attestations are rejected** everywhere: AUTH, ingestion, invite redemption.
4. Its **media is no longer fetched or mirrored**.

The block is **name-keyed**, so key rotation never evades it — evasion requires a new domain. Authority: the network key or the `netblock` cap. List visibility is configurable (`blocklist_visibility: operators|members|public`). Namespace owners cannot override a netblock but may keep narrower denylists (extension). Because federation is non-transitive, one block is total isolation — no propagation machinery exists or is needed.

### 11.7 Federated history backfill
Bridge peers use ordinary `HISTORY` over the bridge session. A request is served **iff all three hold**:

- the channel is in the mutually-acked manifest (invariant 3);
- the range is within the manifest's `history` flag — `from-epoch` serves nothing before the manifest's `created` timestamp (a cheap ULID compare);
- the origin's own retention still holds the data.

Backfilled events are verified like live traffic and stored under the negotiated policy (**not a retention loophole**); only the **compacted materialized view** is served (§12.1) — backfill is not an undelete oracle.

**Bulk transfer.** When a served page exceeds ~200 events, the server answers with `STREAM ACCEPT <token>` instead of an inline `BATCH`; the requester pulls the serialized batch over the data plane — `BACKFILL <token>` (QUIC bidi) or `GET /backfill?t=<token>` (HTTP) — as newline-delimited event lines, folded exactly like an inline batch. The token is one-time; a failed pull is retried by re-issuing the `HISTORY` (a fresh token), so the server holds no cursor state.

**Reconnect.** `HISTORY after=<last stored>` per channel; expired ranges are marked `truncated` — never silent. Flipping `history=full` is a manifest amendment: version bump → re-ack → `MANIFEST` to members (the notification is built in).

**Lazy federated pull.** Bulk backfill is fetched **on client demand** — never eagerly on bridge-up; a federated scrollback nobody asks to see is never pulled:

1. A local client's `HISTORY` for a forwardable channel runs out of local scrollback (a short page).
2. The bridge asks the peer for that same window, **deduped per `(channel, before)` cursor**.
3. The pulled lines feed back through ordinary bridged ingestion (invariants 2, 3), broadcast to members, and persist — the next page serves locally.

Pre-bridge scrollback requires `history=full` (`from-epoch` serves only post-manifest history) — which is why auto-federation always offers `history=full` (§11.10).

### 11.8 Media across bridges
Referenced blobs **mirrored** (fetched over bridge data plane, BLAKE3-verified — substitution detectable). Rationale: clients only talk to home; hotlinking leaks reader IPs and breaks on origin outage. Bounded by manifest `media`; `none` renders unavailable-by-policy, never silent. Backfilled media rides `history`. Mirrors obey receiver retention **and receiver hash blocklist**.

**Mirror pull (concrete).** On ingesting a bridged message whose attachment URI has a *foreign* origin:

1. The receiver **records the reference locally** — its members are gated and can fetch — then pulls the blob over the **same authenticated bridge connection**, on a data-plane bidi stream: `MIRROR <requester-network> <b3-hash> <sig>` → `OK <mime> <len>` + bytes, or `ERR nosuch`.
2. `sig` is the **requester network's** signing key over `hash‖requester‖origin` (domain-separated), so the request is *self-authenticating*: the origin serves iff a network it already federates with (a known peer key) proves control of that key — and it never needs to correlate the data-plane stream with a control-plane session (no origin↔member correlation).
3. The receiver **verifies the returned bytes** hash to the requested `b3-hash` before storing (content addressing — the origin cannot substitute).
4. Any failure — unknown requester, bad signature, absent blob — is the uniform `ERR nosuch` (invariant 1: presence never leaks).

The pull is eager (fired on ingest); a receiver with no live connection to the origin records the reference and skips the fetch until one exists.


### 11.9 Reports and federation

- A report always lands at the reporter's home network (§6.7). For a bridged message, the home network can act **locally** without anyone's permission: local redaction of its replica (its storage, its rules — analogous to the receiver-side hash blocklist in §11.8) and attestation-level blocking of the sender.
- The home network MAY additionally **forward** the report to the origin network over the bridge session (`REPORT-FORWARD <report-id> <msgid> <category> [:note]`, bridge-session-only verb). Forwarding strips the reporter's identity by default — the origin receives a network-attributed report ("test.example forwarded a harassment report against your msgid X"). Origin networks treat forwarded reports as net-scope, `unverified`-at-minimum input; they are free to ignore them, and chronic ignoring is what `NETBLOCK` is for.
- Report queues, resolutions, and holds NEVER replicate across bridges; there is no federated moderation state, only forwarded signals.

### 11.10 Auto-federation (on-demand bridging)

Federation can be established **without operator ceremony**: a local user referencing a foreign namespace — `FEDERATE <network>/<namespace>` (§6.6), or a `weft://<network>/…` invite link whose network is not the user's home (§6.5) — triggers the **home** network to establish the bridge itself. Outbound auto-establishment is governed by network configuration (`auto_bridge = open | off`): `off` disables the trigger (`FEDERATE` answers `UNSUPPORTED`) and leaves inbound bridging (§11.2) unchanged.

**Reachability — the foreign side's consent.** A namespace is *auto-federation-reachable* iff it is `public` **and** its `federation` flag is `open` (`NS META <ns> federation :open`, §6.2 — `open` requires `public` visibility).

- Anything else — absent, private, unlisted, or `federation: closed` — answers the uniform `NO-SUCH-TARGET` (invariant 1: a reachability probe learns nothing an existence probe couldn't).
- A netblocked requester gets `BLOCKED` (§11.6).
- Consent is structural: no request can compel a bridge — the foreign network offers its own signed manifest, or nothing.

**Triggers.**
1. **Explicit** — `FEDERATE <network>/<namespace>` (§6.6): a user asks their home network to go get a namespace it does not carry. This is deliberately a separate verb from `NS JOIN`: joining what already exists locally and causing an outbound dial have different failure surfaces (SSRF, netblock, dial failure, policy-off), and the dial should be explicit.
2. **Invite redemption** — invite links embed the namespace (`weft://<net>/<ns>/i/<b64>`, §6.5) precisely so a *foreign* redeemer can auto-federate to it before redeeming. [TODO: unspecified — confirm with owner: whether the server auto-routes a foreign `INVITE REDEEM` through this flow, or the client issues the explicit `FEDERATE` first.]

**Gates (home side), checked before any dial:** `auto_bridge` is `open` (else `UNSUPPORTED`, also returned for a self-network target); the target network is not netblocked (§11.6, else `BLOCKED`); a **per-account cooldown** bounds trigger frequency (else `THROTTLED`). [TODO: unspecified — confirm with owner: the cooldown duration is implementation-chosen; no normative floor is stated.]

**Flow** (home `H`, foreign `F`, namespace `N`):
1. If a live `H↔F` bridge already covers `N`, reuse it — join, done.
2. **Resolve `F`:** fetch `https://<F>/.well-known/weft` (§10.2) for `F`'s network signing key. The fetch is TLS-verified and SSRF-guarded (below).
3. **Dial:** connect over QUIC (ALPN `weft/1`) and open a bridge session — `AUTH BRIDGE`, proving `H`'s network key (§11.2).
4. **Request:** `BRIDGE REQUEST <N>` (§6.6). If `N` is reachable, `F` signs `N`'s manifest (scope authority = `F`, §11.1) **with `history=full`** — so the joiner can backfill the namespace's *existing* scrollback (§11.7), not just post-manifest traffic — and replies `BRIDGE PROPOSE`; else `NO-SUCH-TARGET`.
5. **Accept:** `H` verifies the manifest against `F`'s key and auto-accepts (`BRIDGE ACCEPT`); the bridge is live, `N`'s channels mirror into `H` (§11.4), and affected members get `MANIFEST` (§11.5). The trigger's outcome is **asynchronous** — the `FEDERATE` ack precedes the landing `MANIFEST`.

**SSRF guard (normative — invariant 13).** The home network dials an address derived from a *user-supplied name*; that name MUST NOT be able to reach internal infrastructure. Every server-side fetch this flow performs — the well-known fetch and the QUIC dial — and every other server-side fetch of a user-influenced URL (e.g. the §13 unfurl proxy) MUST:

- resolve the host and classify **every** resolved address *before* connecting, refusing non-public classes: loopback, private ranges (RFC 1918), CGNAT (`100.64/10`), link-local, ULA (`fc00::/7`), cloud-metadata addresses, and IPv4-mapped forms of these;
- strip URL userinfo before host extraction (`https://trusted@169.254.169.254/` must not smuggle an internal target);
- connect to the **verified IP** — no re-resolution between check and connect (no DNS-rebinding window);
- for HTTP fetches, re-run the guard on every redirect hop (≤5) and bound response size and time.

The guard MUST be implementable and testable as a pure address-classification function, separate from the dial path.

**Visibility.** An auto-established bridge is an ordinary bridge: announced via `MANIFEST` to affected members (§11.5), visible on the network's federation surface, severable (`BRIDGE SEVER`) and blockable (`NETBLOCK`) like any other. Nothing about this path is silent.

[TODO: unspecified — confirm with owner: the standing (non-normative) amendment draft `docs/code/auto-federation-spec-amendment.md` additionally proposes sever-on-idle teardown when the last local member leaves, auto-rejoin re-triggering the flow after a sever, global dial-rate caps with per-domain backoff, and an explicit "`e2ee` namespaces are never auto-bridged" rule. None of these appears in v0.10 text; adopt or drop in a design pass.]

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

- **Content rides the home relay; control rides the session.** A federated
  member's channel *content* — `MSG`/`EDIT`/`DELETE`/`REACT` — is relayed to the
  channel's **home** and minted there into the single total order (`CHANNEL RELAY`,
  §11.13); the home then mirrors it one hop to every spoke. The **author** travels
  as `<sender@net>` on the relay leg (attributed by `F`'s authenticated network
  key, so `F` may vouch only its own users — §11.4), while the msgid origin is the
  home. Only **control/admin** actions (moderation, `GRANT`/`REVOKE`, channel and
  namespace administration, invites, role assignment, report handling) travel as
  commands over a **federation session** and are enforced against `H`'s grants for
  `account@F`; they never mint content.

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

### 11.12 Social layer over federation — the group tunnel

The social layer (§6.8) federates over a generic one-way **friend-delivery conduit**:
"deliver this control line to that peer, attributed to this local account." A network
opens `FSESSION <fsid> OPEN <from>` + `CMD :<line>` on the `F↔H` bridge (§11.11); the
receiver reconstructs `from@<sender-network>` (the bridge authenticated the sender's
network key) and dispatches it. It is fire-and-forget — acks return as ordinary events,
not tunnelled replies. This is the same homeserver-authority mechanism as §11.11, reused
for account-level relationships instead of namespace administration. Conceptual diagrams (**non-normative**): `weft-federation-flows.md` §9.

**Home-authoritative ordering (normative).** A group DM with members on more than one
network has a single **home** = the group creator's network. The home is the **sole
ULID writer** for the group's messages (§9.1); every other member network **mirrors**
that order. This is what makes a total order well-defined when several networks post into
one group. A message a member's own network minted-elsewhere is identified by its origin
msgid and is never re-minted (origin authority, §11.4).

The following verbs are **federation-internal** (bridge-session-only; a client can never
send them — the server emits them). All ride the friend-delivery conduit:

| Verb | Syntax (+ tags) | Direction | Meaning |
|---|---|---|---|
| `GROUP SYNC` | `GROUP SYNC <&id> <creator@net> [<member@net> …]` + tag `name=` | home → members | The authoritative membership snapshot; receivers reconcile the diff (add/remove) and part removed local members. Sent on create + every membership/name change. |
| `GROUP RELAY` | `GROUP RELAY <&id> <sender@net> :<body>` + tags `id=<msgid>`, `echo=<token>`, message meta (`reply-to=`/`thread=`/`attach.N=`) | both | `id=` **absent** = a spoke relayed a member's post to the home → the home mints + fans out; `id=` **present** = a home-minted message → the member ingests + delivers locally. |
| `GROUP MUT` | `GROUP MUT <&id> <sender@net> <root-msgid> <op> [:arg]` + tag `id=<msgid>` | both | A message mutation (`op` ∈ `edit`\|`delete`\|`react-add`\|`react-remove`; `arg` = body/emoji). `id=` absent = spoke → home (relay to mint); present = home-minted → member ingests. |
| `GROUP BACKFILL` | `GROUP BACKFILL <&id>` + tag `after=<msgid>` | member → home | Recovery pull: replay every group message after the member's cursor (or all, when `after=` is absent). The home answers with `GROUP RELAY` (`id=` present) ingests. Idempotent on msgid; a non-member network gets nothing (anti-enumeration). |
| `GROUP CALL` | `GROUP CALL <&id>` + tags `room=`, `token=`, `endpoint=` | home → members | Rings remote members; the media tags carry the ringing network's relay leg (§16). |
| `GROUP ROSTER` | `GROUP ROSTER <&id> <user@net> <active\|ended>` + tag `reply=yes` | mesh | Group-call roster gossip across member networks; `reply=yes` requests the peer's roster back. |

**The echo token.** When a spoke poster's `MSG &<group>` is relayed to the home, the
spoke attaches an opaque `@echo=<token>` and remembers it against the poster's session.
The home echoes the token back **only on the copy to the poster's network**; that spoke
then delivers the home-minted message as the poster's *own* (labelled) message — so the
send is acked (§3.5) even though the home minted the id. Tokens are swept on a TTL
(≈60 s): if the home never answers, the message still arrives later via `GROUP BACKFILL`,
just without the interactive label.

**Attachments.** A cross-network group message carrying a `weft-media://` attachment
triggers a mirror pull from the blob's **origin** network (§11.8), so local members can
fetch it.

### 11.13 Channels over federation — the channel tunnel

**Home-authoritative ordering (normative).** A channel with members on more than one
network has a single **home** = the network that owns its namespace. The home is the
**sole ULID writer** for the channel (§9.1); every other member network (a **spoke**)
runs a **replica** that mirrors the home's order and holds a bounded tail of pending
local posts. A remote member's `MSG`/`EDIT`/`DELETE`/`REACT` is **relayed to the home
to be minted** — a spoke never mints channel content independently. This is the group
model (§11.12) applied to channels; it is what makes a channel's total order well-defined
across networks, and — because the home becomes the single origin — what lets a post from
one spoke reach *another* spoke at all (one hop from the home; a spoke→spoke forward would
be the forbidden transitivity of §11.4).

**Message flow between two servers.** A member on a spoke posts; the post rides the **relay
leg** to the home to be minted, and the home-minted message returns over the **event mirror**.
Both legs multiplex on the one authenticated bridge session (§11.2) — the user never connects
to the home.

```
   NETWORK  S  (spoke — alice is a member)          NETWORK  H  (home — owns #ns/chan)
 ┌────────────────────────────────────────┐       ┌────────────────────────────────────────┐
 │ alice: MSG #ns/chan :hi  (nonce=N)      │       │                                        │
 │   │                                     │       │                                        │
 │   ▼  (1) optimistic echo — show "hi"    │       │                                        │
 │       now, pending, keyed by nonce N    │       │                                        │
 │   │                                     │       │                                        │
 │   ▼        relay leg  (spoke → home)     │ ────▶ │ (2) mint msgid = H/<ULID>              │
 │  CHANNEL RELAY #ns/chan alice@S :hi     │       │     single ULID writer = the order     │
 │  nonce=N   (no @id  =  "mint this")     │       │   │                                    │
 │                                         │       │   ├─▶ (3) deliver to H's local members  │
 │                                         │       │   │                                    │
 │  (5) reconcile: match nonce=N →         │ ◀──── │ (4) mirror one hop (home-origin)       │
 │      replace pending with H/<ULID>      │       │  MESSAGE @msgid=H/<ULID> nonce=N :hi    │
 │      (now confirmed, in canonical order)│       │     event mirror (home → every spoke)  │
 └────────────────────────────────────────┘       └────────────────────────────────────────┘
        └───────── one authenticated bridge session (AUTH BRIDGE, network key) ─────────┘

 Home-network member:  MSG #ns/chan :hi → H mints locally + delivers.  No relay, no wait.
 Reads (HISTORY):      served from S's local replica.  No round-trip to H.
 Recovery:             on reconnect S sends CHANNEL BACKFILL; H replays missed messages as
                       CHANNEL RELAY with @id present — the only time the relay leg carries
                       a minted id.  Live delivery always rides the event mirror above.
```

The two legs, and how they map to the tunnels-at-a-glance table (§11.2): the **relay leg** is
the `CHANNEL RELAY` row (spoke → home, `@id` absent = mint request); the return is the
ordinary **event mirror** row (home → every spoke, one hop, origin preserved).

**Example — the lines on the wire.** `alice@s.example` posts to `#gaming/general`, whose home
is `h.example`. The relay leg carries `CHANNEL RELAY` inside the `FSESSION` conduit over the
one bridge; the return is a bare `MESSAGE` on the same bridge:

```
 client → S   @nonce=n-8F3A;fmt=md MSG #gaming/general :hi everyone
              (S renders it at once — pending, keyed by nonce n-8F3A)

 S → H        relay leg — CHANNEL RELAY tunnelled in FSESSION (fire-and-forget):
              FSESSION 1 OPEN alice
              FSESSION 1 CMD :@nonce=n-8F3A;fmt=md CHANNEL RELAY #gaming/general alice@s.example :hi everyone
              (no @id  =  "mint this")

 H            mints  msgid = h.example/01ARZ3NDEKTSV4RRFFQ69G5FAV, delivers to H's locals

 H → S        event mirror — a bare line on the bridge, home-origin, nonce carried:
              @msgid=h.example/01ARZ3NDEKTSV4RRFFQ69G5FAV;nonce=n-8F3A;fmt=md MESSAGE #gaming/general alice@s.example :hi everyone

 S → client   the same MESSAGE; client matches nonce=n-8F3A → replaces the pending copy
              with the confirmed h.example/01ARZ3NDEKTSV4RRFFQ69G5FAV (now in canonical order)
```

Recovery, after `S` was unreachable — `S` pulls, `H` replays with `@id` **present** (the one
time the relay leg carries a minted id; live delivery always rides the mirror above):

```
 S → H        FSESSION 1 CMD :@after=h.example/01ARZ3NDEKTSV4RRFFQ69G5FAV CHANNEL BACKFILL #gaming/general
 H → S        FSESSION 1 CMD :@id=h.example/01ARZ3NDEKTSV4RRFFQ69G5FAV CHANNEL RELAY #gaming/general alice@s.example :hi everyone
```

The following verbs are **federation-internal** (bridge-session-only; a client can never
send them — the server emits them). Channel membership is carried by the manifest (§11.1)
+ the `MEMBER` mirror, so there is no `SYNC` analog.

| Verb | Syntax | Direction | Meaning |
|---|---|---|---|
| `CHANNEL RELAY` | `CHANNEL RELAY <#ns/chan> <sender@net> [@id=<msgid>] [@echo=<token>] [msg-meta] :body` | both | `@id` **absent** = a spoke relayed a member's post to the home → the home mints + fans out; `@id` **present** = a home-minted message → the spoke ingests + delivers locally. |
| `CHANNEL MUT` | `CHANNEL MUT <#ns/chan> <sender@net> <root-msgid> <op> [@id=<msgid>] [:arg]` | both | A message mutation (`op` ∈ `edit`\|`delete`\|`react-add`\|`react-remove`; `arg` = body/emoji). `@id` absent = spoke → home (relay to apply + mint into order); present = home-applied → spoke ingests. The home applies iff `sender` authored the target or holds the moderation cap (§11.4). |
| `CHANNEL BACKFILL` | `CHANNEL BACKFILL <#ns/chan> [@after=<msgid>]` | spoke → home | Recovery pull after a home outage or reconnect: replay every channel event after the cursor (or all). The home answers with `CHANNEL RELAY` (`@id` present) ingests. Idempotent on msgid; a non-member / unmanifested network gets nothing (anti-enumeration, §11.1). |

**The nonce — optimistic reconcile.** A client stamps each `MSG` with an opaque `nonce` (a
`MsgMeta` tag, §9.2) and renders the message immediately as *pending*. The home carries that
nonce through the mint onto the authoritative `MESSAGE`, which reaches the poster's network
over the **event mirror** — so **every one of the sender's devices** (not just the posting
session) reconciles its pending copy by matching the nonce and swaps in the home-minted id.
This is what makes reconcile airtight across devices — a per-session ack token could not. If
the home is slow or was unreachable, the message simply reconciles whenever it lands (live,
or later via `CHANNEL BACKFILL`). Live home→spoke delivery is always the ordinary event
mirror (§11.4); `CHANNEL RELAY` with `@id` present is used only for backfill replay.

**Staying fast (non-normative rationale).** Home-authority adds a round-trip only to a
cross-network post's *finalization*, not to its appearance or to reads: (a) the spoke
renders the poster's own message **optimistically** at once and reconciles it to the
home-minted copy by matching the `nonce`; (b) `HISTORY` and scrollback are served from the
**local replica** with no round-trip; (c) the relay leg is fire-and-forget and pipelined,
so posts do not head-of-line-block on confirmation; (d) a member whose network *is* the
home mints locally with no relay at all — the common case. See
`docs/architecture/home-authoritative-channels.md`.

**Availability.** Reads are served from the replica even while the home is unreachable.
Posts are queued in a bounded outbox (invariant 6 backpressure) and stay visible as
*pending*; on the home's recovery the spoke replays them via `CHANNEL BACKFILL` and they
mint into order — nothing is lost. If the home is permanently gone the replica is frozen
read-only at its last-mirrored state.

**Attachments** behave as for groups: a `weft-media://` attachment triggers a mirror pull
from the blob's origin network (§11.8).

### 11.14 Federation command reference

Every command here is exchanged **server-to-server** over a bridge session (§11.2) and is
**invisible on a single-network server** — a client can neither send nor receive it (the
client-facing command surface is §6). The one exception is `FEDERATE`, a user's request to
their *own* server, listed last. This is a **non-normative index**: the normative syntax,
gating, and state transitions live in the cited subsection.

*Direction:* `S→H` = spoke → home · `H→S` = home → spokes · `↔` = either peer · `P↔P` = peer
↔ peer · `→members` = fan-out to local members · `client→home` = a user to their own server.

**Bridge session & homeserver authority (§11.2, §11.11).**

| Command | Syntax | Direction | Purpose |
|---|---|---|---|
| `AUTH BRIDGE` | `AUTH BRIDGE <peer-network> <b64-key>` | ↔ | Open a bridge; the peer proves control of its network key (answered by `CHALLENGE` → `AUTH PROOF`, §6.1). Pinned or accept-any. |
| `FSESSION` | `FSESSION <fsid> OPEN <account> \| CMD :<line> \| REPLY :<line> \| CLOSE` | ↔ | Multiplex a federated user's control/admin session; `H` attributes each `CMD` to `account@peer` and enforces against its own grants. Also the fire-and-forget conduit the channel + group tunnels ride. |

**Manifest lifecycle (§11.1, §11.3).**

| Command | Syntax | Direction | Purpose |
|---|---|---|---|
| `BRIDGE PROPOSE` | `BRIDGE PROPOSE <scope> <peer> [history=] [media=] [typing=] [voice=]` (+ signed manifest) | ↔ | Offer a scope-authority-signed manifest for a namespace. |
| `BRIDGE ACCEPT` | `BRIDGE ACCEPT <peer> <version>` | ↔ | Mutual ack → the bridge goes live at `version`. |
| `BRIDGE ADD` | `BRIDGE ADD <peer> <#chan>` | ↔ | Amend the shared set (v+1, requires re-ack). |
| `BRIDGE REMOVE` | `BRIDGE REMOVE <peer> <#chan>` | ↔ | Drop a channel (v+1, unilateral, immediate). |
| `BRIDGE SEVER` | `BRIDGE SEVER <peer>` | ↔ | Tear the bridge down (unilateral). |
| `BRIDGE REQUEST` | `BRIDGE REQUEST <ns>` | S→H | Ask a peer to offer a manifest for one of its namespaces (auto-federation, §11.10); answered with `BRIDGE PROPOSE` iff reachable, else `NO-SUCH-TARGET`. |

**Channel tunnel — home-authoritative content (§11.13).**

| Command | Syntax | Direction | Purpose |
|---|---|---|---|
| `CHANNEL RELAY` | `CHANNEL RELAY <#ns/chan> <sender@net> [@id=] [@echo=] [msg-meta] :body` | S→H mint / H→S ingest | `@id` absent = relay a member's post to the home to be minted; `@id` present = a home-minted message to ingest (backfill replay). |
| `CHANNEL MUT` | `CHANNEL MUT <#ns/chan> <sender@net> <root> <op> [@id=] [:arg]` | S→H apply / H→S ingest | A mutation (`edit`/`delete`/`react-add`/`react-remove`); the home applies iff `sender` authored the target (§11.4). |
| `CHANNEL BACKFILL` | `CHANNEL BACKFILL <#ns/chan> [@after=]` | S→H | Recovery pull; the home replays message roots after the cursor as `CHANNEL RELAY` (`@id` present). |

**Group tunnel — home-authoritative content (§11.12).**

| Command | Syntax | Direction | Purpose |
|---|---|---|---|
| `GROUP SYNC` | `GROUP SYNC <&id> <creator@net> [<member@net>…]` | H→members | Authoritative membership snapshot; receivers reconcile. |
| `GROUP RELAY` | `GROUP RELAY <&id> <sender@net> [@id=] [@echo=] [msg-meta] :body` | S→H / H→S | Group message; `@id` absent = spoke→home mint, present = home→member ingest. |
| `GROUP MUT` | `GROUP MUT <&id> <sender@net> <root> <op> [@id=] [:arg]` | S→H / H→S | Group message mutation (same forms as `CHANNEL MUT`). |
| `GROUP BACKFILL` | `GROUP BACKFILL <&id> [@after=]` | member→home | Recovery replay of missed group messages. |
| `GROUP CALL` | `GROUP CALL <&id> [@room= @token= @endpoint=]` | H→members | Ring remote members; media tags carry the ringing network's relay leg (§16). |
| `GROUP ROSTER` | `GROUP ROSTER <&id> <user@net> <active\|ended> [@reply=yes]` | mesh | Group-call roster gossip across member networks. |

**History & media backfill (§11.7, §11.8).**

| Command | Syntax | Direction | Purpose |
|---|---|---|---|
| `HISTORY` | `HISTORY <#chan> [before=] [after=] [limit=]` (the §6.4 verb, over the bridge) | S→H | Federated scrollback pull; answered by `BATCH` or a `STREAM ACCEPT` offer, bounded by the manifest `history` flag + origin retention. |
| `MIRROR` | `MIRROR <requester-net> <hash> <sig>` (media data plane) | ↔ pull | Fetch content-addressed blob bytes from the origin network; self-authenticating, BLAKE3-verified. |

**Moderation & safety (§11.6, §11.9).**

| Command | Syntax | Direction | Purpose |
|---|---|---|---|
| `NETBLOCK ADD` | `NETBLOCK ADD <network> [:reason]` | local (op) | Block a remote network; fires all four §11.6 effects. Cap `netblock` at `*`. |
| `NETBLOCK REMOVE` | `NETBLOCK REMOVE <network>` | local (op) | Lift the block. |
| `NETBLOCK LIST` | `NETBLOCK LIST` | local (op) | The operator's blocklist. |
| `REPORT-FORWARD` | `REPORT-FORWARD <report-id> <msgid> <category> [:note]` | P↔P | Forward a report to the content's origin network; reporter identity stripped (invariant 12). |

**Voice, and the one client-initiated command.**

| Command | Syntax | Direction | Purpose |
|---|---|---|---|
| `VOICE REQUEST` | `VOICE REQUEST <scope> <#chan>` | S→H | Bridge-only request for a federated voice room's relay credential (§16). |
| `FEDERATE` | `FEDERATE <network>/<namespace>` | client→home | A user asks their own server to auto-establish a bridge to a foreign namespace; the server runs the `BRIDGE REQUEST` flow (§11.10). The only federation command a client sends. |

### 11.15 Federation event reference

Events a server emits as part of federation. The bridge-handshake and manifest/block events
travel **server-to-server** or **to local members**; the **mirrored content** events are the
ordinary §7 events carried over the bridge with their origin msgids intact (§11.4). None
originate from a client. Non-normative index; event shapes are normative in the cited section.

**Bridge handshake (§11.2).**

| Event | Syntax | Direction | Emitted when |
|---|---|---|---|
| `CHALLENGE` | `CHALLENGE <b64-nonce>` | H→peer | On `AUTH BRIDGE`, to be signed with the peer's network key. |
| `WELCOME` | `WELCOME <network> [:motd]` (`features=`, `attestation=`) | H→peer | On a verified bridge `AUTH PROOF` — the session enters the bridge state. |

**Manifest & block state (§11.5, §11.6).**

| Event | Syntax | Direction | Emitted when |
|---|---|---|---|
| `MANIFEST` | `MANIFEST <peer> <version> <state>` (`channels=` `history=` `media=` `typing=` `voice=`; `state` ∈ `live\|added\|removed\|severed`) | →members | A bridge's audience changes (accept / add / remove / sever), so members learn what is now shared (§6.6). |
| `NETBLOCKED` | `NETBLOCKED <network> [:reason]` | →owners | A manifest is severed by a netblock (§11.6). |

**Mirrored content — the §7 events, origin-preserved (§11.4).**

| Event | Syntax | Direction | Emitted when |
|---|---|---|---|
| `MESSAGE` / `EDITED` / `DELETED` / `REACTION` | shapes in §7 | H→S (mirror, one hop) | Live channel events fanned out by the home; `msgid` origin = the home, `nonce` echoed for reconcile (§11.13). |
| `PROFILE` | signed profile update (§10.3) | ↔ mirror | A federated user's profile crosses the bridge with its account-scoped signature. |

**Backfill data (§11.7).**

| Event | Syntax | Direction | Emitted when |
|---|---|---|---|
| `BATCH` | `BATCH START` / `BATCH END` with `id=` (+ `truncated` / `compacted`) | H→S | A federated `HISTORY` page (the compacted view). |
| `STREAM ACCEPT` | `STREAM ACCEPT <token>` | H→S | A large backfill page is offered as a data-plane stream; the puller drains it and re-ingests each line. |

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

**Retention holds (reporting interplay).** Filing a `verified` report places a hold on the reported event and its context (RECOMMENDED: ±25 surrounding events in the channel):

- Held events are exempt from **both** compaction and retention purge — including in `retained:<d>` channels, and including pre-edit bodies still inside the audit window at filing time — until the report resolves plus a 7-day grace.
- Holds are **invisible** to ordinary members: no protocol surface reveals that a message is under report.
- `ephemeral` channels store nothing, so nothing can be held (hence `unverified`); `e2ee` holds preserve ciphertext blobs only.

**Effects elsewhere:**
- Backfill (§11.7) automatically benefits: bridge catch-up transfers shrink by the edit/reaction churn factor, and the existing "materialized view only" rule becomes precisely specified rather than implied.
- `MARK`/read logic unaffected (markers reference surviving msgids; a marker on a compacted-away edit event resolves to its `edit-of` root).
- E2EE channels: the server cannot compact ciphertext (it can't see event relations inside); e2ee compaction is client-side during device sync — normative non-goal for servers.
- Moderation implication, stated honestly: after the audit window, pre-edit content is **gone on this network**. Networks wanting longer audit trails raise `compact-after`; the protocol default favors the "edits eventually really disappear" privacy expectation.

---

## 13. Media

**Model.** Media is content-addressed: a blob's identity is its BLAKE3 hash, referenced as `weft-media://<origin-network>/<b3-hash>` with `{mime, bytes, w, h, duration?}` metadata; identical bytes collapse to one blob (dedup by construction). Posting: `attach.N=` tags (≤10), `attach-meta=`; bare media = empty trailing + tags. Fetching is **home-network only** — a client never fetches from a foreign network (that is what §11.8 mirroring is for). E2EE: the client encrypts before upload; no server thumbnails; host-blindness extends to attachments.

**Upload.** `STREAM OFFER media <mime> <bytes>` (checks `attach` + size config; RECOMMENDED 25 MiB image / 500 MiB video) → `STREAM ACCEPT <token>` → data-plane transfer → the server hashes and stores. The server probes image dimensions and derives a small thumbnail as its own auto-referenced blob.

**Transfer surfaces (one blob store, three doors).** All share the `STREAM OFFER` → `STREAM ACCEPT <token>` grant flow:

1. **QUIC data-plane bidi framing** — `PUT <upload-token>`, `GET <bearer> <hash> [range]`, and the §11.8 `MIRROR <requester-net> <hash> <sig>`.
2. **HTTP** — `POST /media` (upload; OFFER token or session bearer) and `GET /media/<hash>?t=<bearer>` (Range-capable, so video is ranged/segmented fetch; live A/V is WEFT-RT, §16).
3. **`BACKFILL <token>`** — the bulk pull for large history pages (§11.7).

**Fetch authorization.** Right after auth the server pushes a per-session **bearer** as a `MEDIA TOKEN` event (§7.1); fetches are membership-gated by it. A bad bearer, a non-member fetch, and an absent blob are **one uniform not-found** (invariant 1).

**Bookkeeping.** Blobs are refcounted against the events (and avatars/emoji) that reference them; orphans are collected after a grace period (§12).

**Moderation.** Hash-level blocking (`MEDIA BLOCK`, §6.6): blocking deletes the blob + its thumbnail and rejects re-upload *and* mirror of the same bytes — content = identity, so re-uploads are dead on arrival.

**Link-preview (unfurl) proxy.** Clients never fetch third-party URLs (§9.4); the server fetches on their behalf, so the origin host never sees the viewer:

- `GET /unfurl?url=<href>&t=<bearer>` — the page's OpenGraph/meta preview as JSON (`url`, `title`, `description`, `image`, `site_name`).
- `GET /unfurl/image?url=<href>&t=<bearer>` — proxies the preview image bytes.

Both require the same session bearer as `/media` (never an open proxy) and are **SSRF-guarded per §11.10 / invariant 13**: resolve → classify every address → pin the verified IP; re-check each redirect hop (≤5); strip userinfo. Fetches are size- and time-bounded; non-HTML/non-image results yield an empty preview. A network MAY disable unfurling.

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

Optional server-side RFC 2812 + IRCv3 gateway (`:6697` TLS); the gateway *is* the home network.

| IRC surface | WEFT surface |
|---|---|
| `NICK` / SASL | display name / `AUTH` |
| `JOIN #ns/chan` | valid natively (`/` is a legal chanstring char) |
| `PRIVMSG` (+ `draft/reply`) | `MSG` (`reply-to=`) |
| `TAGMSG +draft/react` | `REACT` |
| `server-time` / `msgid` tags | ULIDs / origin msgids |
| `chathistory` / `batch` | `HISTORY` / `BATCH` |
| `MODE` | coarse, read-mostly projection |
| `KICK` / `TOPIC` | capability-checked (§6.7, §6.3) |
| `LIST` | `DISCOVER` |
| invites | `/msg WeftServ REDEEM` |

**Degradations (normative):**

- Edits/deletes render as `* edited:` / `* message deleted` text fallbacks — IRC users can't edit.
- Threads flatten to a `[thread 01H…]` prefix.
- Media becomes short-lived tokened HTTPS URLs.
- **e2ee channels are invisible** (the `NO-SUCH-TARGET` treatment).
- 8 KiB WEFT lines split to 512 B IRC lines.

Purpose: the likely operator audience is on IRC today — day-one irssi/WeeChat usability. The gateway is a projection, not a lossy translator.

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

v0.1 core design → v0.2 namespaces + manifest bridging → v0.3 user-owned namespaces, visibility, invites → v0.4 NETBLOCK → v0.5 backfill + `history` flag → v0.6 media, mirroring, WEFT-IRC → v0.7 implementability audit → v0.8 consolidation → v0.9 namespace recovery ladder + message compaction → v0.10 message reporting: home-network routing, retention holds, honest e2ee/ephemeral limits, bridge forwarding → **v0.11 editorial consolidation (this document)**.

Each entry: what changed, why, and where it now lives. Implementation detail lives in Appendix B.

### Foundational milestones (M0–M3a)

- *M0 (editorial):* §7's "as v0.8" payload placeholders spelled out (`TYPING`/`MARKED`/`PRESENCE`/`POLICY`) — the self-containment claim now holds. → §7.
- *M1:* keepalive RECOMMENDED interval lowered 60 s → **10 s** to match contemporary chat clients ("2 missed = dead" ≈ 30 s liveness). → §3.4.
- *M2:* the `AUTH ENROLL` response defined; the `/.well-known/weft` JSON document pinned. → §6.1, §10.2.
- *M3a:* `HISTORY` key=value middle params pinned; `REACT` shortcodes travel bare (§18 #8); mutation targets widened to `<#chan|@user>`; every batch line echoes the request label. → §6.4, §7.9.

### M4 — capabilities, namespaces, moderation, recovery

- *M4a:* the loose `GRANT`/`REVOKE`/`CHANNEL *`/`INVITE *` syntax pinned and their response events defined (`TOKEN`/`POLICY`/`CHANMETA`/`INVITED`). → §6.3, §6.5, §7.4–§7.5. (Operator bootstrap + grant-table fast path: Appendix B.)
- *M4-5 (namespaces + layout):* NS verb responses pinned (`@root=` on CREATE; NS-META tags); the channel-layout extension — categories/position, server-authoritative category list, layout-change broadcast. → §6.2, §6.3, §7.4.
- *M4c (reporting):* routing hint (`ns|net`) vs. concrete handler scope; content states; refcounted holds on root ± 25 context, exempt from purge + compaction until resolution + 7-day grace (invariant 11); the push-to-default-handlers limit stated honestly. → §6.7, §12.1.
- *M4-6 (recovery ladder):* signed NS verbs (`@sig=`), rung selection by whose signatures verify, delay windows + root veto, permanent `root-history`. Rung 3's original 30-day delay was later zeroed — see the rung-3 entry below. → §2.4, §6.2.

### M5 — federation

- *M5a–c:* `AUTH BRIDGE` (network-key challenge-response; pinned / accept-any trust modes), `@manifest=` on PROPOSE with strictest-safe defaults, `MANIFEST`/`NETBLOCKED` payloads, the invariant-3 forwarding gate, and network-key session trust (per-device attestations on bridged lines remain a noted refinement). → §11.2–§11.6, §7.6.
- *M5d + auto-federation:* the verified outbound dialer, `BRIDGE REQUEST`/`FEDERATE`, the per-namespace `federation` flag, well-known key fetch, and the SSRF classifier (invariant 13) — consolidated as §11.10 in v0.11. → §11.10, §6.2, §6.6.

### Media (M-media)

- *M-media (data plane + mirroring):* three transfer surfaces over one BLAKE3 blob store; the per-session `MEDIA TOKEN` bearer with one uniform not-found (invariant 1); the self-authenticating `MIRROR` pull; thumbnails + refcounted GC. → §13, §11.8. (Deferred: the manifest `media`-mode gate on mirroring and `mirror-max` — §18 #5.)
- *Unfurl proxy:* server-side link previews so clients never fetch third-party URLs; bearer-gated, SSRF-guarded exactly like §11.10, size/time-bounded. → §13. (CORS/webview notes: Appendix B.)
- *M-media-4 (backfill over STREAM):* pages > 200 events upgrade to `STREAM ACCEPT` + a one-time `BACKFILL <token>` pull; federated backfill is lazy, deduped per (channel, before); auto-federation offers `history=full` so a joiner can reach pre-bridge scrollback. → §6.4, §11.7, §11.10.
- *M-media-5 (hash moderation):* the `media-block` cap + `MEDIA BLOCK/UNBLOCK/BLOCKS`; a block deletes bytes + thumbnail and kills re-upload *and* mirror (content = identity). → §6.6, §13.

### Gateways & cross-network identity (M6/M7)

- *M6 (WEFT-IRC subset):* the gateway ships registration, JOIN/PART (incl. `#ns/chan`), PRIVMSG, NAMES, LIST, PING, QUIT, with edits/deletes/reactions degraded to text; SASL, IRCv3 tags, chathistory, TAGMSG, and MODE/TOPIC/KICK projection deferred. → §17; shipped-subset detail: Appendix B.
- *M7 (moderation):* mute/ban/kick + `MODERATED`; the deny-list checked against covering scopes; `posting :restricted` send-gating; the `can_post` composition. → §6.7.
- *Identity & federation sessions:* account ULIDs as the stable grant key (rename-safe); token subject v2 (`pubkey | account-ULID | account@network | UNBOUND`, v1 refused); foreign subjects on `GRANT`/`ROLE`; `FSESSION` homeserver authority with IP non-exposure. → §10.1, §10.4, §6.5, §11.11.

### Verification (M-verify)

- *M-verify:* the `VERIFY` family — email = mailed one-time code, birthday = self-attested, **badge-only** (no access gating) — with owner-only `VERIFIED` (subjects are PII). → §6.1, §10.5.

### Client-parity & operational amendments

- *Persistent membership:* durable `JOIN` + auto-rejoin on auth + per-account announcement dedup (Discord model). → §6.3.
- *PIN / CAPS / MEMBERS:* pins (+ `PINS` batch); the effective-caps query; the roster served as a batch with a `PRESENCE` line per member. → §6.4, §6.5, §6.3.
- *Channel rename:* one atomic re-key of every channel-scoped record — holds travel with content (invariant 11); old-scope delegated tokens stop matching (an epoch-bump effect), re-delegate at the new name. → §6.3.
- *Namespace bulk-join:* `NS JOIN` joins every visible channel in one round-trip; no visible channel → uniform `NO-SUCH-TARGET`. → §6.2.
- *Presence liveness + MODLIST:* the `offline` status; disconnect ≠ part; reconnect ≠ re-join; `MODLIST` lists the deny-list. → §6.1, §6.3, §6.7, §7.1.
- *Unread counts:* server-computed `UNREAD-COUNTS` pushed on login and on cross-device `MARK`; the `@account`/`@everyone` mention heuristic. → §6.3, §7.3.
- *Search:* `SEARCH` → a `BATCH` of `MESSAGE` (≤50, newest-first), reusing the PINS/HISTORY shape. → §6.4. (Reference substring semantics: Appendix B.)
- *Threads + naming/listing:* the `thread=` tag + `HISTORY thread=` filter; `THREAD NAME` / `THREADS`; hiding replies from the timeline is client presentation, the wire keeps them in the channel. → §6.4, §9.4.
- *Custom emoji:* `EMOJI ADD/REMOVE/LIST` per namespace; `:name:` renders inline in bodies and reactions (the reaction key is the literal `:name:` string). → §6.2, §9.4.
- *Operators in Postgres:* operator authority moved from config to a store flag + CLI; **no wire change** — operational only. → Appendix B.
- *Role display + in-place rename:* `hoist=`/`pos=` + `ROLE REORDER`; `ROLE RENAME` migrates the definition and every assignment server-side. → §6.5.1.
- *Rung 3 is immediate (supersedes the M4-6 30-day window):* a delay defends against a *lost key*, not a *live adversary* — the window's veto belonged to exactly the party being removed. What is given up is stated plainly: the delay + veto half of invariant 9, and a compromised network key can now seize a namespace instantly — accepted because an operator already hosts the data and holds every cap at `*`. What is kept carries the accountability: network-key authorization, the announcement, and the permanent operator-initiated `root-history` mark; e2ee remains the real boundary (invariant 8). → §2.4.

### Social layer (M-social)

- *M-social:* friends, group DMs, and calls (§6.8) + the federation-internal group tunnel — home-authoritative ordering, echo-token acks, `GROUP BACKFILL` recovery (§11.12); per-participant `CALL-MEDIA`; cross-network call audio bridges room-to-room via a server-side relay (IP non-exposure, §16). Deferred (§18 territory): a friend-request rate/abuse model, group size bounds, per-device attestation on federated social commands.

### v0.11 — editorial consolidation

- *v0.11:* **no wire-behavior change.** Adds §0 (conformance + terminology), §9.0 (invariant registry; 5–7/10 reserved), §11.10 (auto-federation, reconstructed from scattered v0.10 text); rewrites §11.2 to the network-key model (the "bridge capability token" line was stale); makes every example §4-grammar-true (tags before the verb); promotes appendix-only verbs (`VERIFY`, `EMOJI`, `THREADS`, `THREAD NAME`, `CAPS`, `MODLIST`) and behaviors into their home sections; moves implementation identifiers to Appendix B. Change log: `CHANGES-v0.11.md`; open judgment calls: `DECISIONS-NEEDED-v0.11.md`.

---

## Appendix B — Reference-implementation notes (non-normative)

Where the reference implementation (weftd) keeps things — useful to contributors, meaningless for wire conformance. **Renaming anything here never changes the protocol.**

- **Storage.** PostgreSQL behind storage traits (an in-memory backend runs the same contract tests). Notable migrations: `0009` moderation deny-list + the channel `restricted` flag, `0010` pins, `0011` persistent membership, `0020` media blocklist, `0026` account `operator` flag, `0027` emoji, `0031` thread names. Search is case-insensitive substring on both backends for identical semantics (a Postgres `tsvector` upgrade is a noted refinement); unread counts reuse the event rows (no migration).
- **Operators.** Operator authority is a per-account store flag managed by a `weftd admin` CLI (`create`/`grant`/`revoke`/`list`, direct-to-Postgres — the bootstrap admin is created this way); the config `operators` list survives only as a deprecated seed. The check reads the store live, so changes need no restart.
- **Invites.** Implemented as server-side id + counter records; §6.5's offline-verifiable unbound-token object is the design target (a federation concern).
- **Mail.** `VERIFY EMAIL` delivery is SMTP configured in weftd (`[smtp]`); an unconfigured development server records the claim and logs the code.
- **Unfurl.** Toggle `[unfurl] enabled` (default on). The HTML meta extractor is a pure, dependency-free parser; a permissive CORS layer fronts `/media` and `/unfurl` (desktop-webview uploads send non-simple preflights).
- **Constants.** History→STREAM threshold **200** events (`HISTORY_STREAM_THRESHOLD`); report-hold context radius **25** (`HOLD_RADIUS`); rung-3 delay **0** (`RECOVERY_DELAY_RUNG3_SECS`) — applied inline rather than parked with an already-elapsed ETA, which would leave the namespace in the abuser's hands until the next maintenance tick.
- **IRC gateway shipped subset.** Registration (`NICK`/`USER`/`PASS` → `HELLO` + `AUTH`, auto-`REGISTER` on first `AUTH-FAILED`; a ≥12 B `PASS` is the WEFT password), `JOIN`/`PART` incl. `#ns/chan`, `PRIVMSG`/`NOTICE` ↔ `MSG` (bare nick = DM; own echo suppressed), `NAMES` (fills from observed joins), `LIST` ← `DISCOVER`, `PING`/`QUIT`, MOTD; edits/deletes/reactions degraded to text. Deferred: SASL, IRCv3 tags, chathistory, TAGMSG, MODE/TOPIC/KICK projection, 8 KiB↔512 B line splitting, the e2ee-invisible treatment. Enabled by `[listen] irc` (plaintext; TLS termination is the operator's).
