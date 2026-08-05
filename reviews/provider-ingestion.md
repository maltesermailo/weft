# Provider ingestion — a code walkthrough

How a line like

```
@as=carol@kde.org;msgid=matrix.org/01j2… MSG #01hx…/01hy… :hello from kde
```

sent by a bridge daemon becomes a stored, fanned-out WEFT event — and every gate
it must pass on the way. The code is `crates/weft-core/src/session/plugin.rs`;
the wire contract is `docs/protocol/bridge-session-protocol.md` (§5, §8). Line
numbers are as of 2026-08-06.

The two sentences that generate everything else:

> **A realm is a network.** A bridged realm (matrix.org) is modeled as a WEFT
> network: its users are `user@realm`, it mints its own msgids, and weftd
> ingests them exactly as it ingests a federation peer's.
>
> **A bridge behaves as a federation peer.** Commands travel *to* the
> authority, events come *from* it — so the provider's traffic takes the same
> `ingest_record` path as peer federation, with the same invariants.

---

## 1. Getting to `on_provider_ingest` (dispatch)

A provider session (`State::PluginService`, entered via `AUTH ADAPTER` +
pinned-key proof) routes every line through `on_plugin_service_line` (~L152).
Dispatch order matters:

1. **`@as=` tag present → it's ingestion**, before any verb matching (~L161).
   Ingestion is identified by the *tag*, not the verb — the verbs themselves
   (`MSG`, `REACT`, …) are ordinary client verbs. The tag value must parse as
   `user@network` or the line is refused.
2. No `@as` → the **bridge verbs** (`REALM REGISTER/ASSERT/WITHDRAW`,
   `PROVISION-OK/ERR`, `GRANT`/`REVOKE`/`ROLE *` as `Actor::Provider`), then
   the **plugin events** (`PLUGIN-REGISTER`, `PLUGIN-VIEW`/`-PATCH`/`-RESULT`,
   `NS-META`/`CHANNEL-LAYOUT` assertions, `NS-MEMBER` statements, `SYNC`).

With `@as` set, `on_provider_acting` (~L1012) splits one more way:

- **Moderation verbs** (`MUTE`/`UNMUTE`/`BAN`/`UNBAN`/`KICK`) become
  `Actor::Foreign(sender)` and run through the *ordinary* moderation handlers —
  a foreign moderator is checked against the grants the provider itself issued,
  exactly like a local one. Being foreign confers nothing.
- **Everything else** falls to `on_provider_ingest` (~L1080): the traffic path.

## 2. `on_provider_ingest`, gate by gate

The function is a straight pipeline: find the channel, then five authority
gates, then the shared federation ingest. Any failure is a **drop or a typed
`ERR UNSUPPORTED`** — never a partial write. Nothing is minted on a bad line
(capability checks precede side effects, invariant 4).

### Gate 0 — the DM special case (~L1091)

`MSG @<local-account>` with a minted `@msgid` is a bridged 1:1 DM: stored in
the ordinary `Scope::Dm` keyed by member keys, preserving the realm's msgid.
Two checks here: the msgid must be minted under the **sender's own** network
(a DM has no channel realm to key on), and the target must be local.

### Gate 1 — which channel? (~L1123)

- `MSG` names its channel directly (`Target::Channel`).
- The mutations (`EDIT`/`DELETE`/`REACT`/`UNREACT`) name only a **root msgid**;
  `channel_of_msgid` resolves it via `events.find_root` — the mutation lands
  wherever its root lives. An unknown msgid is a silent drop (the provider
  replaying something we never mirrored is normal, and nobody is waiting).

### Gate 2 — is it a replica the key may speak for? (~L1148)

The channel record must carry an `origin` URI (`matrix://kde.org/community`) —
a **native** channel refuses ingestion outright — and `scheme_authorized(key,
origin.scheme())` must hold: the session's proven key must be pinned in
`[[plugin.remote]]` for that scheme. This is the trust root of the whole path:
*a provider speaks only into rooms of the platform its key was installed for.*

### Gate 3 — the sender rule (~L1173, amended 2026-08-05)

`@as` must be **foreign**:

| Sender | Verdict | Why |
|---|---|---|
| `carol@kde.org` (any foreign network) | ✔ ingest | Rooms are cross-realm: a matrix.org-homed Space has members from kde.org. The trust root is gate 2 (key + scheme), not the sender's domain. |
| `ada@test.example` (local) | ✘ `UNSUPPORTED` | Local identities are anchored by **our auth** — a bridge attributing to one is forgery… |
| `ada@test.example`, mutation verb, realm-origin root | ✔ ingest | …**except** the §8 return path, below. |
| `eve@peer.example` (a known WEFT peer) | ✘ `UNSUPPORTED` (~L1210) | Peer identities are anchored by **their signing keys**; a bridge must not be a side door around peer auth. |

The original rule ("`@as` must live on the bound realm") would have dropped
most participants of any real Matrix room; the widened rule keeps exactly the
two refusals that protect identities anchored elsewhere.

#### The §8 return-path exception (~L1186)

A local user cannot mutate a message the realm minted — that would author
under someone else's origin — so weftd *relays the request to the provider*
(`relay.rs :: MessageRoute::ChannelProvider` → `relay_provider_mut`):

```
weftd → provider:   @as=ada@test.example REACT matrix.org/01j2… 👍
```

The provider performs it foreign-side (ada's puppet reacts in the Matrix room)
and **confirms it back through this same ingestion path, attributed to ada**.
That confirmation is the one accepted shape of a local `@as`:

```rust
let confirms_relay = match &cmd {
    Command::Edit { msgid, .. } | Command::Delete { msgid }
    | Command::React { msgid, .. } | Command::Unreact { msgid, .. } =>
        msgid.origin().as_str() == origin.realm(),
    _ => false,
};
```

Both bounds are load-bearing:

- **mutation verbs only** — `MSG` never matches, so no authored content under
  a local name;
- **realm-origin roots only** — `ChannelProvider` relay is only ever chosen
  for realm-origin roots, so this accepts *exactly the class weftd asks for*
  and nothing more. A local user's action on a local-origin message never
  goes through the provider; a provider claiming one is lying.

Without this arm the flip side can never close: the puppet's Matrix echo maps
back to a local account and would be refused forever — the reaction would
exist on Matrix but never appear in ada's own client.

Residual trust, stated honestly: a compromised provider could fabricate
"ada reacted 👍" *inside its own channels*. Accepted deliberately — the
provider already controls everything rendered in its channels, it is
pinned-key authenticated and operator-installed, and it already may state
local users' *membership* (§6). What the gate keeps is escalation beyond its
channels: authored content, and native/peer-anchored state.

### Gate 4 — netblocks, both ends (~L1221)

Invariant 7 is name-keyed and a bridge is not a way back in:

```rust
for blocked in [&realm, &sender.network] { … }
```

- the **channel's realm** blocked → the whole replica goes quiet;
- the **sender's network** blocked → that homeserver's users are silenced in
  *every* bridged room (`NETBLOCK kde.org` bites kde.org users inside a
  matrix.org-homed Space too).

### Gate 5 — the shared federation ingest (~L1240)

`provider_event` (top of the file, ~L42) shapes the command into the event it
will fan out as, and encodes the minting rule mechanically:

| Verb | Minted id? | Shape |
|---|---|---|
| `MSG` | **required** (`@msgid`, `minted()?`) | `Event::Message` — its stored row is keyed by its own id |
| `EDIT` | **required** — an edit is itself a stored event | `Event::Edited { msgid: minted, edit_of: root }` |
| `DELETE` | none — a tombstone is keyed on its root | `Event::Deleted { by: sender }` |
| `REACT`/`UNREACT` | none | `Event::Reaction { op: Add/Remove }` |

A message-bearing line without `@msgid` returns `None` → dropped with no
error. This is why the SDK (`weft-appservice::Realm`) takes the msgid as a
required argument — the failure is otherwise silent.

Then the **same code federation peers use**:

```rust
let Some((_, record)) = super::federation::ingest_record(&realm, &event) …
handle.ingest(self.id, record, event).await;
```

`ingest_record` independently re-verifies that every carried msgid originates
on `realm` (invariant 2 — weftd never re-mints, and a foreign id can't be
smuggled under another origin; this backstops gates 3's realm comparison).
`handle.ingest(self.id, …)` hands it to the channel actor with **our session
id**, which makes the no-ping-pong guard structural: the fan-out skips the
session that ingested, so the provider is never sent its own event back.

## 3. Why a replica can't echo (the multi-origin property)

A replica channel is **multi-origin**: our members' events carry our origin,
the realm's carry theirs. Outbound relay to the provider (§8) forwards *only*
events this network minted (`msgid.origin == our network`) — so an ingested
event is structurally ineligible to cross back, independent of the session-id
skip. Two separate mechanisms, either alone sufficient. (This is also why
`ident::msgid_for` in the Matrix daemon puts the event's real
`origin_server_ts` in the ULID's time bits: the replica's read order sorts by
ULID time across both origins.)

## 4. The verdict table

| Line | Result |
|---|---|
| `@as=carol@kde.org;msgid=matrix.org/… MSG <replica> :hi` | stored + fanned out, sender `carol@kde.org` |
| `@as=carol@kde.org REACT <matrix.org-root> 👍` | reaction by `carol@kde.org` |
| `@as=ada@test.example MSG <replica> :hi` | `ERR UNSUPPORTED` — authored content under a local name |
| `@as=ada@test.example REACT <matrix.org-root> 👍` | **accepted** — §8 relay confirmation |
| `@as=ada@test.example REACT <test.example-root> 👍` | `ERR UNSUPPORTED` — weftd never relays local-origin roots |
| `@as=eve@peer.example;msgid=… MSG <replica> :hi` | `ERR UNSUPPORTED` — peer-anchored identity |
| anything into a native channel | `ERR UNSUPPORTED` — not provider-managed |
| `@msgid=other.realm/…` | dropped by `ingest_record` — invariant 2 |
| sender or realm netblocked | silent drop — invariant 7 |
| `MSG`/`EDIT` without `@msgid` | silent drop — nothing minted |

## 5. Tests that pin this

- `weft-core/tests/session.rs :: provider_ingests_foreign_messages` — the
  happy path, msgid pinning, escaped identities, local-forgery refusal.
- `… :: a_cross_realm_sender_ingests_but_local_and_peer_users_are_refused` —
  both 2026-08-05 amendments: cross-realm accept, local/peer refuse, and the
  §8 confirmation round trip.
- `… :: local_mutations_of_a_bridged_message_relay_to_the_provider` — the
  outbound half (weftd asks; authorization runs *before* the relay).
- `weft-matrix/tests/bridge.rs` — the daemon's side of the same contract
  against a mock homeserver, including puppet-echo suppression.
