# WEFT-Matrix — the Matrix adapter (bridge adapter #1)

**Status:** design concept for a separate module (working name `weft-matrix`) — **adapter #1 of the [Foreign-Realm Bridging Framework](foreign-bridge-framework.md)**. The framework doc owns the generic model (foreign-realm addressing, the `State::ForeignBridge` context, provisioning folded into `NS JOIN <uri>` — no new user verb — + reused `NS-META`/`CHANNEL-LAYOUT` structure assertions, realm-keyed NETBLOCK); this doc is the Matrix *binding* — the full-fidelity Matrix analog of the §17 IRC gateway. **Bidirectional:** §3–§16 are the *outbound* half (WEFT namespaces projected as Matrix Spaces, WEFT-homed). The *inbound* half — a remote Matrix **Space → foreign namespace, room → foreign channel** (the §6 mapping, **unchanged**), consumed through the framework's first-class foreign namespace/channel API and keeping native identity (`matrix.org` stays `matrix.org`; users stay `@alice:matrix.org`) — is §20 (this adapter's binding of the framework).

> **§5 is superseded** (owner directive 2026-07-27) by the framework's native-identity model: remote homeservers are **not** laundered into virtual WEFT networks (`matrix-org.mx.test.example`). They keep native Matrix coordinates and are surfaced as first-class *foreign* objects. §5's per-network keys / wildcard DNS+cert / per-network `AUTH BRIDGE` sessions are replaced by per-realm `State::ForeignBridge` connections (one per homeserver, pinned-key authed and realm-bound; framework §3).

**Depends on:** the ns-membership + SYNC redesign (`namespace-membership-sync-v0.12.md`, **shipped**) — hide overrides drive membership mapping (§8); the home-authoritative replica model (`home-authoritative-channels.md`) is reused for the inbound replica.
**Wire impact on core WEFT:** outbound is near-zero (one `bridge:matrix :open` NS-META flag). Inbound rides the framework's core footprint (framework §9) — generic, **not** Matrix-specific.

## 0. Locked decisions (from design review — do not reopen without owner)

| # | Question | Decision |
|---|---|---|
| 1 | Process model | **Standalone appservice daemon** attached to a companion homeserver the operator deploys next to weftd (conduwuit/Tuwunel-class recommended; Synapse supported). Direct Matrix S2S implementation in weftd: long-term aspiration, **out of scope**. |
| 2 | Retention | **Strict permanent-only**: a channel projects iff its policy is `permanent`. No waiver. `retained:*` and `ephemeral` never project. Rationale: §5.2 strictest-policy negotiation — open Matrix federation, viewed as a peer, can honor exactly one policy (`permanent`); it cannot purge, cannot compact, cannot guarantee redaction. |
| 3 | Visibility | **Public namespaces only.** Unlisted/private namespaces produce no Matrix rooms at all (invariant 1: nothing to enumerate). |
| 4 | Categories | **Category → sub-space.** Namespace = top-level Space; each category = child sub-space containing its channels' rooms; ordered via `m.space.child` order keys derived from `position`. |
| 5 | Inbound identity | **Per-homeserver virtual networks.** Each remote Matrix homeserver appears to weftd as a distinct WEFT peer network, so NETBLOCK/attestation-rejection/media-refusal work per Matrix homeserver. |
| 6 | Moderation | **Matrix mods have real power.** Matrix-side moderator actions are translated into `@as` WEFT commands and enforced against WEFT's grant store; WEFT caps project to power levels. Details §10. |
| 7 | E2EE | **Hard-excluded, non-negotiable.** `e2ee` channels get no room (the IRC-gateway `NO-SUCH-TARGET` treatment); a bridge that decrypts breaks invariant 8. |
| 8 | Ephemera & scope | Typing bridged under manifest `typing=yes`. Read receipts **never** bridged. Presence never (locked in core). **Channels only** in v1 — DMs, double-puppeting, voice, report-forwarding: v2 (§21). |
| 9 | Framework & foreign identity | Inbound is the Matrix binding of the **[Foreign-Realm Bridging Framework](foreign-bridge-framework.md)** — foreign realms keep native identity (`matrix.org`, MXIDs, `matrix://` URIs), **not** virtual WEFT networks. **Supersedes decision 5 / §5.** The Space→namespace + room→channel mapping (§6) is unchanged. Owner directive 2026-07-27. |

## 1. Terminology & the mapping

Matrix's "server" naming is confusing: what Discord calls a *server*, Matrix calls a **Space** (a room with `type: m.space` containing child rooms). A Matrix *homeserver* is the infrastructure node — the analog of a WEFT network.

```
WEFT network      test.example        ↔  Matrix homeserver   test.example (companion HS, delegated)
WEFT namespace    gaming              ↔  Matrix Space        #gaming:test.example
WEFT category     "Text"              ↔  sub-space           #gaming-text:test.example
WEFT channel      #gaming/general     ↔  child room          #gaming_general:test.example
WEFT account      ada@test.example    ↔  Matrix user         @ada:test.example
Matrix user       @alice:matrix.org   ↔  WEFT account        alice@matrix-org.mx.test.example
```

**Same domain for both protocols.** Matrix delegates via `/.well-known/matrix/server` exactly as WEFT uses `/.well-known/weft`; they coexist on one domain. From any Matrix client's view, `test.example` simply *is* a Matrix homeserver populated with WEFT's users and Spaces — the word "bridge" appears nowhere on the Matrix side.

## 2. Components

```
                         test.example (one domain)
 ┌─────────────┐   WEFT bridge sessions    ┌──────────────┐   appservice API    ┌─────────────────┐
 │   weftd     │◄─────(AUTH BRIDGE,───────►│ weft-matrix  │◄───(txn push /─────►│ companion HS    │
 │ (unchanged) │   one per virtual net)    │   daemon     │    intents/puppets) │ (conduwuit)     │
 └─────────────┘                           └──────────────┘                     └────────┬────────┘
        ▲                                        │ serves /.well-known/weft              │ Matrix S2S
        │ ordinary WEFT clients                  │ for *.mx.test.example                 ▼
                                                                                 open Matrix federation
                                                                                 (matrix.org, …)
```

- The **companion homeserver** is dedicated: registration disabled, its only users are bridge-managed puppets + the appservice bot. This lets the appservice registration claim the full `@*`/`#*` namespaces, so `@ada:test.example` is literally the WEFT account, no prefix.
- **weft-matrix** is simultaneously (a) an appservice to the companion HS and (b) an ordinary WEFT bridge peer to weftd — reusing §11 wholesale: manifests, `@as` homeserver authority, labeled acks, NETBLOCK. weftd does not know Matrix exists.
- Recommended companion HS: **conduwuit/Tuwunel** (single Rust binary, fits the small-footprint goal). Synapse supported for operators who already run one.

## 3. Projection rules

A channel projects to a Matrix room iff **all** hold:

1. its namespace is `public` **and** has projection enabled: `NS META <ns> matrix :open` (new NS-META key, parallel to the `federation` flag; requires `public` visibility, else `FORBIDDEN`) — projection is opt-in per namespace, consistent with §1's "explicit consent for every federation act";
2. its policy is `permanent` (locked decision 2);
3. it is not `e2ee` (locked decision 7);
4. it is not a voice channel (`kind=voice` — voice is out of v1).

Channels failing 2–4 inside a projected namespace are simply absent on the Matrix side. Policy transitions are watched live: `permanent → anything else` tombstones the room (`m.room.tombstone`, no successor) — the projection promise no longer holds; `anything → permanent` creates it. `NS META matrix :closed` tombstones the whole Space tree. `NS VISIBILITY` off `public` implies `matrix :closed`.

Projected Spaces of public namespaces are published to the companion HS's public room directory (they're in `DISCOVER` anyway — no new exposure).

## 4. Outbound identity (WEFT → Matrix)

- Every WEFT account is puppeted as `@<account>:test.example` via appservice intents. Display name and avatar from the §10.3 profile blob; kept in sync on `PROFILE` updates.
- Federated WEFT users (from real WEFT peer networks) puppet as `@<account>.<their-network-sanitized>:test.example` — they can't collide with local accounts (`.` is not in the local account grammar §2.3).
- Puppet room membership: **activity-based by default** (`roster = active`): puppets join a room on first message/reaction and on moderator-cap grant; a `roster = full` config mode joins every ns member's puppet for exact rosters (heavy on big namespaces). Honest limit either way, documented.

## 5. Inbound identity (Matrix → WEFT): per-homeserver virtual networks

> **⚠️ SUPERSEDED (2026-07-27).** This section's virtual-network scheme (`matrix.org` →
> `matrix-org.mx.test.example`, per-network derived keys, wildcard DNS+cert, per-network
> `AUTH BRIDGE`) is replaced by the [Foreign-Realm Bridging Framework](foreign-bridge-framework.md):
> Matrix keeps native identity (`@alice:matrix.org`), and each homeserver gets its own
> pinned, realm-bound `State::ForeignBridge` connection (one per realm; framework §3). Retained
> below only as the rationale trail — read §20 + the framework doc for the live design.

- Remote Matrix homeserver `matrix.org` ⇒ virtual WEFT network **`matrix-org.mx.test.example`**; its user `@alice:matrix.org` ⇒ `alice@matrix-org.mx.test.example`.
- **Sanitization (deterministic):** lowercase; `.` → `-`; strip chars outside `[a-z0-9-]`; if the result collides with an already-mapped distinct server name, append `-` + first 6 chars of base32(BLAKE3(server_name)). Mapping table persisted; a name once assigned never changes.
- The bridge holds an Ed25519 signing key per virtual network (derived from one bridge root key + server name — no key-management explosion) and serves `/.well-known/weft` for `*.mx.test.example` (wildcard DNS → bridge HTTP endpoint), so weftd's normal attestation machinery works untouched.
- The bridge opens **one `AUTH BRIDGE` session per virtual network, lazily** — on the first event from a user of that homeserver. §11.11's "F may assert only its own users" is preserved exactly: the `matrix-org.mx.…` session only ever sends `@as=alice …` for matrix.org users. Idle virtual sessions close after a timeout.
- **Payoff:** `NETBLOCK ADD matrix-org.mx.test.example` blocks matrix.org and nothing else. §11.6's four effects map cleanly (§11 below).

## 6. Space structure & metadata

| WEFT | Matrix |
|---|---|
| namespace | Space room; `m.room.name` ← NS-META `title` (fallback: name), `m.room.topic` ← `description`, avatar ← `icon` (mirrored to mxc) |
| category | child sub-space, `m.space.child` on the top Space, order key = category index in `cats=` |
| channel | room under its category's sub-space (uncategorized → directly under the top Space); `m.space.child` order key = zero-padded `position` |
| channel topic | `m.room.topic` |
| channel rename (§6.3) | update `m.room.name` + swap canonical alias (`/` → `_` in aliases) |
| channel delete / NS delete | `m.room.tombstone` (no successor) |
| pins | `m.room.pinned_events` state, mapped through the event-id table |
| custom emoji (§9.4) | v1: reactions carry the literal `:name:` key both ways; image-emoji packs (MSC2545-family) deferred |

`CHANNEL-LAYOUT` broadcasts drive live `m.space.child` re-ordering; clients on both sides see the same sidebar order.

## 7. Event mapping

All projected channels are **WEFT-homed** — the home mints every ULID; the Matrix room is a projection surface. The bridge keeps a persistent bidirectional map `event_id ↔ msgid` (plus `txn_id` dedup both ways).

**WEFT → Matrix** (bridge receives fanned-out §7 events on its bridge sessions, emits as the author's puppet):

| WEFT event | Matrix event |
|---|---|
| `MESSAGE` (`fmt=md`) | `m.room.message` `m.text`; CommonMark → `formatted_body` HTML |
| `MESSAGE` with `attach.N=` | `m.image`/`m.video`/`m.file` per mime (media flow §12) |
| `reply-to=` | `m.in_reply_to` rich reply |
| `thread=` | `m.thread` relation (native — no IRC-style flattening) |
| `EDITED` | `m.replace` |
| `DELETED` | `m.room.redaction` |
| `REACTION add/remove` | `m.reaction` annotation / redact own reaction |
| `TYPING` (manifest `typing=yes`) | typing EDU |
| `MEMBER join/part` | puppet join/leave (per roster mode §4) |
| `MODERATED ban/kick/mute` | §10 |

**Matrix → WEFT** (bridge receives appservice transactions, relays as `@as` commands on the sender's virtual-network session; the home mints; own-puppet events filtered out):

| Matrix event | `@as` command |
|---|---|
| `m.room.message` | `MSG` (HTML → CommonMark best-effort; unconvertible → plaintext body) |
| `m.replace` | `EDIT <mapped msgid>` |
| `m.room.redaction` (author) | `DELETE` (`delete-own` path) |
| `m.room.redaction` (moderator) | `DELETE` — requires `delete-any` on the actor's virtual account (§10); missing cap → drop + revert notice |
| `m.reaction` / its redaction | `REACT` / `UNREACT` |
| reply / `m.thread` | `reply-to=` / `thread=` tags |
| media messages | download → hash → `MEDIA BLOCK` check → `STREAM` upload → `attach.N=` (§12) |
| `m.room.member` | membership mapping §8 |
| power-level / ban events | §10 |

**Ordering (honest limit):** the WEFT total order is the home's mint order of relayed commands (appservice transaction arrival); the Matrix room's own order is its DAG/stream order on the companion HS. These can diverge slightly for near-simultaneous cross-side posts. WEFT order is authoritative for WEFT clients; Matrix clients see Matrix order. No re-ordering window in v1.

## 8. Membership mapping (uses the ns-membership model)

Matrix membership is per-room; WEFT membership is per-namespace + hide overrides. The mapping is exact:

- Remote user's **first room join** in a projected namespace → `@as NS JOIN <ns>` + hide overrides on every *other* projected channel (member of the server, visible in this one room).
- Each **additional room join** → `@as JOIN <#chan>` (clear that hide).
- **Room leave** → `@as PART <#chan>` (set the hide).
- Leaving the **last** joined room → `@as NS LEAVE <ns>`.
- Joining the **Space itself** is cosmetic on Matrix and maps to nothing (Matrix space-join doesn't auto-join rooms; we mirror that).
- Non-projected channels (retained/e2ee/voice) are hidden from the virtual account by a standing hide — a Matrix user's WEFT presence never exceeds what Matrix can see.
- WEFT-side bans/kicks of a virtual account project back as Matrix kick/ban of the real user (§10).

## 9. Rooms are bridge-created and bridge-controlled

The appservice bot creates every projected room: PL 100 for the bot, `m.federate` left **true** (open federation is the point of projection — the permanent-only rule is what makes that honest), encryption **never** enabled. State-control guard: the bot's PL layout prevents non-authorized users from changing `m.room.encryption`, aliases, or tombstones; if a state race ever lands an `m.room.encryption` event anyway, the bridge tombstones the room immediately (a projected room silently going encrypted would strand WEFT-side members).

## 10. Moderation & authority — one grant store, two UIs

**Principle: WEFT's capability store is the single source of truth. The Matrix room's power levels are a *projection* of it, and Matrix-side moderation actions are just another client surface issuing `@as` commands against it.** Enforcement stays purely token-based (§6.5.1 spirit).

- **WEFT → Matrix (projection):** ns-owner/`ns-admin` → PL 100 (under the bot); `ban`+`kick`+`delete-any` holders → PL 50; `posting :restricted` → `events_default: 50` with `send`-cap holders raised to 50; `MUTE` → per-user PL below `events_default`; `BAN` → Matrix ban; `KICK` → kick; `UNBAN`/`UNMUTE` symmetric. Recomputed on every `TOKEN`/`MODERATED`/`ROLE` event the bridge sees.
- **Matrix → WEFT (authority):** a Matrix user's moderation act becomes an `@as` command from their virtual account and succeeds iff WEFT granted that account the cap:
    - mod-redaction → `@as DELETE` (needs `delete-any`)
    - Matrix ban/kick → `@as BAN` / `@as KICK` (needs `ban`/`kick`)
    - PL change on another user → `@as GRANT`/`REVOKE` of the corresponding caps (needs `grant:<cap>`)
    - If the WEFT check fails (`CAP-REQUIRED`), the bridge **reverts** the Matrix-side state change and notices the actor. No side-channel authority: a matrix.org admin has exactly the power some WEFT grant gave `their-account@matrix-org.mx.test.example` — assignable via ordinary `GRANT`/`ROLE ASSIGN` (foreign subjects are already legal, §10.4), or via the Matrix PL UI by someone who holds `grant:<cap>`.
- **NETBLOCK mapping (§11.6's four effects):** blocking `<hs>.mx.test.example` ⇒ (1) that virtual bridge session is refused; (2) the bridge sets **`m.room.server_acl`** denying that homeserver in every projected room — compliant servers stop accepting its users' events, the Matrix-native analog of severing; (3) its users' `@as` commands are dropped at weftd (attestation rejection, automatic); (4) its media is no longer fetched or mirrored (bridge-enforced). Name-keyed as always.

## 11. Reports

v1: a WEFT `REPORT` against Matrix-authored content is handled entirely locally (§6.7) — the content is home-minted, so local moderation (`delete-any` → projects as redaction; attestation-level blocking of the virtual account) needs nobody's permission, per §11.9. No forwarding into Matrix's report API (it's weak and unactionable); `REPORT-FORWARD` to virtual networks is a v2 item. Retention holds apply to the WEFT copy; the spec sentence: *holds and report confidentiality protect local state — the Matrix side is best-effort redaction* (see §14).

## 12. Media

- **WEFT → Matrix:** bridge fetches the blob with its service bearer, uploads to the companion HS media repo once (dedup by BLAKE3 → mxc map), references the mxc. Remote homeservers fetch from the companion HS — standard Matrix media federation.
- **Matrix → WEFT:** bridge downloads via the companion HS (authenticated media), BLAKE3-hashes, checks the `MEDIA BLOCK` list (blocked → drop + notice, never uploaded), enforces WEFT size limits (§13 RECOMMENDED caps), uploads via `STREAM`, attaches `weft-media://`.
- `MEDIA BLOCK` issued later: WEFT-side deletion is automatic (core behavior); the bridge additionally redacts the mapped Matrix events and quarantines the mxc on the companion HS (best-effort beyond it — see honest limits).

## 13. E2EE exclusion (restating for the spec text)

`e2ee` channels: no room, no alias, `NO-SUCH-TARGET` on any Matrix-side probe — identical to §17's IRC treatment, mandated by invariant 8. Inbound direction cannot arise: the bridge never creates encrypted rooms and tombstones any room that becomes one (§9). This is a MUST, not a config knob.

## 14. Honest limits (must appear in the spec section, WEFT style)

1. **Redaction is a polite request.** A WEFT `DELETE` projects as a redaction; spec-compliant remote homeservers strip content, but nothing enforces compliance, and even compliant ones keep the event skeleton. Permanent-only projection means no *retention* promise is broken — but "deleted" on a projected channel means *best-effort deleted* on remote Matrix servers.
2. **Order divergence** at sub-second granularity between the WEFT total order and the Matrix room order (§7).
3. **Roster fidelity** in `roster = active` mode: Matrix member lists show active WEFT participants, not the full derived roster.
4. **Holds & reporter confidentiality are local.** Invariants 11/12 hold on the WEFT network; the Matrix side sees only redactions.
5. **Formatting round-trips are best-effort** (CommonMark ↔ Matrix HTML): unconvertible constructs degrade to plaintext, never dropped messages.

## 15. Ephemera

Typing: bridged both ways when the (bridge-internal) manifest says `typing=yes` — config default `yes` for projected namespaces. Read receipts: never bridged (WEFT `MARK` is private; Matrix receipts are public — bridging would leak read state). Presence: never (core lock).

## 16. Deployment & config

- Reference `docker-compose` (**shipped**: `deploy/weftd/`, bridge behind the
  its own stack next to weftd's — see `deploy/weft-matrix/README.md`).
  The reference stack runs **Synapse**, not conduwuit, for one reason: appservice
  registration must be declarative (`app_service_config_files`, generated from
  `weft-matrix.toml` into a shared volume so the tokens exist in one place).
  conduwuit registers appservices through its admin room — a manual step in a chat
  window — so it stays supported by the daemon but unautomated by the deployment.
- Original sketch: weftd + conduwuit + weft-matrix + a front proxy serving `/.well-known/weft`, `/.well-known/matrix/{server,client}` on the apex and `/.well-known/weft` on `*.mx.<domain>` (wildcard DNS + wildcard cert).
- `weft-matrix.toml` sketch: `[matrix] hs_url, as_token, hs_token, domain`; `[weft] endpoint, bridge_root_key, virtual_suffix = "mx.test.example"`; `[projection] roster = active|full`; `[media] max_inbound_bytes`; `[limits] idle_session_timeout`.
- Appservice registration file: `@*` and `#*` namespaces (exclusive), generated by `weft-matrix generate-registration`.

## 17. Changes required in weftd core (keep this list minimal — it's the module boundary)

**Matrix-adapter-specific core changes are now just the outbound projection flag** — everything the *inbound* direction needs is the generic [Foreign-Realm Bridging Framework](foreign-bridge-framework.md) §9 footprint (`State::ForeignBridge`, `<scheme>://` scopes + foreign-account types, `NS JOIN <uri>` provisioning, the adapter-side `REALM ASSERT`/`WITHDRAW` handshake, realm-keyed NETBLOCK, foreign-object store tables), which is shared by every future adapter and contains no Matrix code.

1. **Outbound projection consent flag:** `NS META <ns> bridge:matrix :open|closed` (the generalized form of the framework's `bridge:<scheme>` opt-in, framework §5) — echoed as a `bridge=matrix:open` tag, `open` requires `public` visibility. weftd stores/broadcasts it; only the adapter interprets it.
2. ~~Pinned-suffix bridge trust~~ **(removed — superseded by §5's supersede note):** no `[bridge] pinned_suffix`, no virtual-suffix keys, no wildcard DNS/cert. Replaced by the framework's `[[foreign_bridge]] scheme, pubkey` pinned adapter connection (framework §3/§9.1).
3. Everything else — identity, `@as`/`@realm` authority, NETBLOCK, media, membership verbs — is either an existing surface or lands once in the framework, not per adapter.

## 18. Implementation plan (phases)

1. **Skeleton:** appservice registration + txn ingestion + puppet intents; virtual-network key derivation + well-known server + lazy `AUTH BRIDGE`; weftd pinned-suffix support; `event_id ↔ msgid` store.
2. **Projection:** Space/sub-space/room provisioning from NS-META + CHANNEL-LAYOUT; the projection-rule watcher (policy/visibility/matrix-flag transitions → create/tombstone).
3. **Messages:** MSG/EDIT/DELETE/REACT both ways, replies, threads, dedup, echo filtering.
4. **Membership:** §8 mapping onto ns-membership + hide overrides; roster modes.
5. **Media:** both directions + block-list enforcement + later-block redaction.
6. **Moderation:** PL projection, `@as` authority path + revert, NETBLOCK → server ACL.
7. **Hardening:** typing, pins, rename/tombstone races, reconnect/backfill of missed txns (appservice txn replay + WEFT `@as HISTORY` catch-up), idle session GC.

## 19. Acceptance tests

1. `NS META gaming matrix :open` on a public ns with one `permanent` channel → Space + sub-space + room exist, alias correct, order keys match `position`; the ns's `retained:90d` channel has **no** room.
2. Channel policy `permanent → retained:30d` → room tombstoned; back to `permanent` → fresh room.
3. `e2ee` channel in a projected ns: no room, alias probe → not found; enabling encryption on a projected room via forced state → room tombstoned.
4. Matrix user's first room join → ns membership + hides; second room join clears one hide; leaving all rooms → `NS LEAVE`. WEFT-side sidebar of the virtual account matches their Matrix rooms exactly.
5. Round-trip fidelity: md message with reply + thread + edit + reaction WEFT→Matrix→(second Matrix user replies)→WEFT; all relations resolve through the id map both ways.
6. Mod-redaction by a Matrix user **without** `delete-any` → WEFT unchanged, Matrix state reverted + notice; grant `delete-any` to the virtual account → same action succeeds and fans out as `DELETED`.
7. `NETBLOCK matrix-org.mx.test.example` → virtual session refused, server ACL set in every projected room, subsequent matrix.org events dropped, media fetch refused. `REMOVE` restores.
8. `MEDIA BLOCK <hash>` after a Matrix image was bridged → mapped Matrix events redacted, mxc quarantined, re-upload of same bytes from either side dead on arrival.
9. Duplicate appservice txn replay + duplicate WEFT event delivery → no double posts (dedup both directions).
10. Kill the bridge for an hour under traffic on both sides → on restart, txn replay + `@as HISTORY` catch-up converge both rooms with no loss and no duplicates.

## 20. Inbound binding — Matrix Spaces/rooms as foreign namespaces

The Matrix binding of the [Foreign-Realm Bridging Framework](foreign-bridge-framework.md) (framework §5/§10). The framework owns the generic mechanics; here is how Matrix fills the slots. **Nothing about the Space/room → namespace/channel mapping changes** — §6 stands; these objects are simply handled first-class as *foreign* namespaces/channels rather than disguised as a virtual network.

- **Mapping (unchanged, §6):** Matrix Space → foreign namespace; child room → foreign channel; category → sub-space/category; a standalone (non-Space) room → a foreign namespace with a single channel. Addressed `matrix://<hs>/<space>[/<room>]` (e.g. `matrix://matrix.org/gaming/general`).
- **Identity (native):** foreign accounts keep their MXIDs (`@alice:matrix.org`); the foreign namespace's origin is `(matrix, matrix.org)`. Our users appear inside the remote room as their companion-HS puppet `@<account>:test.example` (federated out via Matrix S2S) — §4's outward puppeting, now into a *remote* room the companion HS has joined.
- **Join** (`NS JOIN matrix://matrix.org/gaming` — provisioning folded into `NS JOIN`, framework §4): first contact routes to the adapter, which resolves the alias, joins the Space/room via the companion HS (Matrix S2S), enumerates children, and asserts the tree via `NS-META`/`CHANNEL-LAYOUT` in its bound realm (framework §3.1). An unjoinable target, or a *room* URI naming an encrypted room → `NO-SUCH-TARGET` (invariant 1; e2ee §13); a joinable **space** always provisions, even with zero bridgeable rooms — Spaces exist without chats and map like an empty namespace, its encrypted/voice rooms simply absent (owner directive 2026-08-06). Once provisioned it is listed in `DISCOVER`, so later joiners use an ordinary `NS JOIN`; leaving is `NS LEAVE`/`PART` on the URI — **no Matrix-specific or foreign-specific verbs**.
- **Events:** the §7 table runs in the inbound sense — remote events arrive as `@scheme=matrix;realm=matrix.org;as=@bob:matrix.org …` assertions (weftd mints the replica ULIDs); our users' posts/edits/reactions are relayed out and puppet-applied in the remote room.
- **Membership:** the §8 mapping, foreign-addressed — remote users *and* our own `NS JOIN <uri>`-provisioned users share one derived roster (framework §6).
- **Retention/visibility:** bounded replica `retained:<config>`, never `permanent`/`e2ee`; listed in `DISCOVER` (subject to the Space's own visibility) and client-badged "Matrix · matrix.org" (framework §6).
- **Authority (honest limit):** matrix.org is the social home of a *consumed* Space — WEFT caps govern only our replica + our users' relay; `NETBLOCK REALM matrix://matrix.org` is the escape hatch (framework §7). (For *projected* WEFT namespaces, §10 still applies — WEFT is authoritative.)

## 20a. State recovery — the database is a cache (owner requirement 2026-08-06)

**What happens if the daemon's database is deleted?** Almost nothing is lost,
because three properties were designed in rather than bolted on:

1. **Structure ids are deterministic** (`ident::stable_ulid` from the Matrix room
   id) and weftd *pins* what the adapter mints — re-asserting a room reproduces
   the same namespace and channels instead of orphaning them.
2. **Matrix is a database we already have.** Which rooms we bridge, who is in
   them, the power levels, whose DM a room is — all readable room state, marked
   with `dev.weft.space` / `dev.weft.dm` at creation.
3. **Msgids are recoverable.** An ingested one is deterministic from
   `(realm, event_id, origin_server_ts)`; one we minted is stamped onto the
   Matrix event as `dev.weft.msgid`. So the link map rebuilds *on demand* — a
   mutation naming an unknown event resolves by reading that one event — rather
   than needing an eager replay of every room.

Exactly one thing cannot be derived: the **bridging ban list**. weftd sends each
ban once and deliberately keeps no record (§11), and Matrix has no opinion about
it — so it lives in the bot's Matrix **account data** (`dev.weft.bans`), the
adapter's own durable notebook, which survives our database precisely because it
is not in it.

`recover()` runs automatically at boot when the store is empty (a no-op on a
fresh deploy) and is idempotent, so it is safe to repeat. It reports what it
found *and what it could not classify* — an unclaimed room is the operator's cue.

### The bot console

The appservice bot doubles as an operator console for the residue a machine
cannot infer (a puppet or DM room created by an older build, before the markers
existed). Authorization is a **config allowlist** (`[matrix] admins`), not a
Matrix power level: power in a room says what you may do to that room, not who
may re-point this bridge's internal state, and a room admin on a *consumed*
space is a stranger to us. An empty allowlist disables the console.

```
!weft status                                  what this bridge believes it bridges
!weft recover                                 rebuild state from Matrix (safe to repeat)
!weft attach-puppet <mxid> <ulid> [name]      re-point a puppet whose marker is missing
!weft attach-dm <weft-account> <mxid>         re-point *this* room as a DM
!weft help
```

`attach-dm` acts on the room it was typed in rather than taking a room id: an
operator can see which room they are in, and a mistyped id would silently hijack
another conversation.

## 21. Deferred (v2+)

Double-puppeting (link accounts on both sides); DMs (Matrix DM ↔ two-member WEFT group); voice (MatrixRTC ↔ LiveKit); `REPORT-FORWARD` into virtual networks; image custom-emoji packs; direct S2S federation in weftd (retiring the companion HS).

## 22. Confirm with owner (calls I made without an explicit answer)

1. Projection is **opt-in per namespace** via `NS META matrix :open` rather than automatic for every public ns — chosen for §1's explicit-consent goal. Confirm.
2. Lazy per-homeserver `AUTH BRIDGE` sessions + the pinned-suffix root key (§5/§17.1) as the trust mechanism. Confirm.
3. `roster = active` as default (§4). Confirm.
4. Sanitization + collision-hash scheme for virtual network names (§5). Confirm.
5. Matrix Space-join alone maps to nothing (§8). Confirm.