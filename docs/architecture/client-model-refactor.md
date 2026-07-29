# Client model refactor — objectify the state

**Status:** planned (design approved 2026-07-29). Approach: **reactive domain
classes**. Migration is incremental; every phase compiles and passes
`npm run check` on its own.

**Scope:** the Tauri/SvelteKit client only (`client/src`). No wire, store, or
server changes. Capability scope keys (`"ns:<id>"`, `"*"`, `#chan`) stay strings
on the wire and in the cap tables — the models *reference* them, they don't
replace the scheme.

---

## 1. The problem

`client/src/routes/+page.svelte` is a ~4655-line "God container." It holds ~40
flat `$state` maps keyed by strings, and one logical entity is smeared across
many parallel `Record`s. The entity's behavior then lives as free functions that
close over those maps, all surfaced through the 485-line `AppCtx` interface
(`lib/context.ts`).

The three worst offenders:

**One channel** is spread across 9 stores (line refs in `+page.svelte`):

| store | line | what it holds |
|---|---|---|
| `channels: Record<string, Channel>` | 455 | the channel record itself |
| `unreadMap` | 484 | has-unread flag |
| `mentionMap` | 485 | has-mention flag |
| `unreadCount` | 487 | unread tally |
| `mentionCount` | 488 | mention tally |
| `notifPrefs` | 509 | notif level (also holds `ns:`/`net` keys) |
| `typers` | 3232 | who's typing here |
| `histByTarget` | 1323 | transient history-page buffer |
| `layoutCache` | 462 | cached category/position |

Rendering one channel tile means indexing five maps and calling three helpers
(`isMuted`, `chanShort`, `retentionOf`).

**One account** across 6: `presence` (583), `profiles` (586), `nicks` (588),
`verifications` (605, own only), `memberRoles` (855), `capsFor` (755) — with
`displayName` / `avatarUrl` / `nickOf` / `dotClass` / `nameColor` as separate
functions.

**One namespace/server** across 8: `discovered` (657), `memberNs` (670),
`customEmoji` (1461), `nsMembersByNs` (803), `rolesByScope["ns:"+id]` (776),
`grantsByScope` (791), `modDeny` (`ns:` slices), `notifPrefs["ns:"+id]` — with
`serverName` / `serverUnread` / `isNsMember` / `isNsOwner` / `serverCap` /
`canModerate` scattered around.

Plus ~8 transient `*Buf` arrays (`roleBuf` 777, `grantBuf` 792, `nsMemberBuf`
804, `pinsBuf` 731, `searchBuf` 739, `threadBuf` 745, `threadsBuf` 749,
`invitesBuf` 693) that accumulate streamed events until a terminal event flushes
them into the maps above.

The ~800-line inbound-event switch (`handle(e)`, lines 1763–2600) mutates all of
this by hand.

## 2. The target shape — an object graph

Svelte 5 reactive **domain classes** — `$state` fields in `.svelte.ts`,
`SvelteMap`/`SvelteSet` from `svelte/reactivity` for collections. But the point
isn't "one class per map." The point is that the classes **hold references to
each other**, so the string-key cross-lookups disappear:

> A **Server** has **Members**; a **Member** *is* an **Account** and *has*
> **Roles**; a **Channel** belongs to a **Server** and has its own per-Member
> **overrides**. "Can this person moderate here?" is answered by walking that
> graph — not by concatenating `"account|scope"` and indexing `capsFor`.

The relationships (▸ = "has / owns", → = "references"):

```
AppStore
 ├─ accounts : Map<handle, Account>          ← the identity map (interning)
 ├─ servers  : Map<id, Server>
 │    Server ▸ roles    : Role[]
 │           ▸ members  : Member[]            (a Server↔Account join)
 │           │    Member → account : Account      (shared instance)
 │           │           ▸ roles   : Role[]       (into Server.roles)
 │           │           ▸ grants  : Cap[]        (direct ns-scope grants)
 │           ▸ channels : Channel[]
 │                Channel → server   : Server
 │                        ▸ messages : Message[]
 │                        │    Message → author : Account
 │                        ▸ overrides: ChannelOverride[]  (per-role / per-member caps)
 │           ▸ emoji, bans
 ├─ dms      : Channel[]                      (server = null)
 ├─ me       : Session → account : Account    (the current user)
 ├─ social   : friends/groups → Account
 └─ federation, ui
```

The **identity map** is the linchpin. `store.account(handle)` always returns the
*same* `Account` instance, so a message author, a server member, a friend, and a
DM peer are one object. Presence arrives once → every surface updates. No more
`presence[a]`, `profiles[a]`, `nicks[scope|a]` kept in sync by hand.

Derived facts become **methods that traverse the graph** instead of functions
that read parallel maps:

```ts
me.memberIn(server).can("mute", channel)   // was: canModerate(channel) reading capsFor
server.member(acct).displayName            // was: nickOf(scope,a) || displayName(a)
server.unreadCount                          // was: folding unreadCount over a name filter
message.author.avatarUrl                    // was: avatarUrl(peerOf(m.author))
```

```
client/src/lib/models/
  account.svelte.ts      Account       identity + profile + presence (global, cross-server)
  role.svelte.ts         Role          a server role definition (caps/color/hoist/…)
  member.svelte.ts       Member        Server↔Account join: roles + direct grants + nick
  channel.svelte.ts      Channel       messages + unread + per-member overrides; → server
  server.svelte.ts       Server        aggregate root: roles ▸ members ▸ channels ▸ emoji/bans
  session.svelte.ts      Session       the current user (me): account + connection + auth form
  social.svelte.ts       Social        friends + groups + calls (→ Account)
  federation.svelte.ts   Federation    netblocks + manifests
  store.svelte.ts        AppStore      identity maps + apply(event) reducer
  collector.ts           Collector<T>  generic accumulate-until-flush helper
```

`+page.svelte` keeps only **view/UI state** (which modal is open, the composer
draft, the active selection, drag state) and wires `store → AppCtx → layout`.
`AppCtx` stays as the migration seam and is thinned phase by phase.

## 3. Model sketches

Grounded in the exact current stores/functions being folded in. These are
sketches, not final signatures.

These are sketches, not final signatures. The through-line: every field that
used to be a string key into another map is now a **reference** to the object
itself.

### Account (`account.svelte.ts`) — global identity

One instance per `account@network`, interned by `store.account(handle)`. Folds
`presence`, `profiles`, `nicks`, and cross-server identity. It knows nothing
about any one server — server-specific facts (roles, nick, caps) live on
`Member`.

```ts
export class Account {
  readonly handle: string;            // canonical account@network (bare if local)
  presence = $state("offline");
  display  = $state<string>();
  avatar   = $state<string>();
  about    = $state("");
  status   = $state("");

  get initials()     { /* initials */ }
  get dotClass()     { return `dot ${this.presence}`; }
  get displayName()  { return this.display || this.handle; }
  get avatarUrl()    { /* §10.3 fetchable URL or null */ }
}
```

### Role (`role.svelte.ts`) — a server role definition

Folds one entry of `rolesByScope`. Caps as a `Set` so `grants(cap)` is O(1).

```ts
export class Role {
  readonly id: string;                // ULID (v0.13); name is a mutable label
  name     = $state("");
  color    = $state("");
  caps     = new SvelteSet<string>();
  hoist    = $state(false);
  pingable = $state(false);
  position = $state(0);

  grants(cap: string) { return this.caps.has(cap); }
}
```

### Member (`member.svelte.ts`) — the Server↔Account join

This is the class the old model was missing. A `Member` is an account's identity
*within one server*: its roles (references into `server.roles`), its direct
ns-scope grants, its nick and join time. Folds `nsMembersByNs`, `memberRoles`,
the `ns:` slice of `nicks`, and the account's `ns:`-scope `capsFor`.

```ts
export class Member {
  readonly server: Server;            // back-reference
  readonly account: Account;          // the *shared* identity instance
  roles    = $state<Role[]>([]);      // references into server.roles
  grants   = new SvelteSet<string>(); // direct GRANTs at ns scope
  nick     = $state("");
  joinedMs = $state(0);

  get isOwner()    { return this.server.owner === this.account; }
  get displayName(){ return this.nick || this.account.displayName; }
  get color()      { /* highest hoisted role color, else "" */ }

  /** Effective caps here = owner ⇒ all, else roles ∪ direct grants. */
  can(cap: string): boolean {
    return this.isOwner || this.grants.has(cap) || this.roles.some((r) => r.grants(cap));
  }
}
```

### Channel (`channel.svelte.ts`) — belongs to a Server

Folds the `Channel` type + the four unread/mention maps + `typers` +
channel-scoped `notifPrefs` + history flags. `server` is a reference (null for
DMs/groups). Channel-scoped permission overrides (the ChannelSettings per-target
caps) become `ChannelOverride` objects that point at a `Role` or `Member`.

```ts
export class Channel {
  readonly name: string;              // wire id: #ns/chan | @dm | &group
  readonly server: Server | null;     // null ⇒ DM/group
  vanity   = $state<string>();
  retention = $state("");
  messages = $state<Message[]>([]);   // Message.author → Account

  unread = $state(false);   mention = $state(false);
  unreadCount = $state(0);  mentionCount = $state(0);
  notifLevel = $state<NotifLevel>("mentions");
  typers = $state<Account[]>([]);     // references, not names

  category = $state<string>();  position = $state(0);
  restricted = $state(false);   viewGated = $state(false);  voice = $state(false);
  historyLoaded = $state(false);  hasMore = $state(false);  truncated = $state(false);
  overrides = $state<ChannelOverride[]>([]);   // per-role / per-member cap sets

  get isDm()    { return this.name.startsWith("@"); }
  get isMuted() { return this.notifLevel === "nothing"; }
  get short()   { /* chanShort */ }
  get title()   { /* titleOf */ }

  markRead() { this.unread = this.mention = false; this.unreadCount = this.mentionCount = 0; }
  bump(mentioned: boolean) { this.unread = true; this.unreadCount++; if (mentioned) { this.mention = true; this.mentionCount++; } }
}
```

`retentionMeta` / `dayKey` / `dayLabel` / `renderMd` are pure view helpers → a
`lib/format.ts`, not methods.

### Server (`server.svelte.ts`) — the aggregate root

Owns roles, members, channels, emoji, bans. This is where "a server has members
which have roles" lives literally. Folds `discovered`, `memberNs`,
`customEmoji`, `nsMembersByNs`, and the `ns:` slices of `rolesByScope` /
`grantsByScope` / `modDeny`.

```ts
export class Server {
  readonly id: string;                // ns id (v0.13); name is a vanity label
  name = $state("");   title = $state<string | null>(null);
  owner = $state<Account | null>(null);          // a reference, not a handle
  visibility = $state("public");  federation = $state(false);
  welcome = $state<Channel | null>(null);
  joined  = $state(false);            // am I a member (folds memberNs)

  roles    = $state<Role[]>([]);
  members  = new SvelteMap<string, Member>();     // handle -> Member
  channels = $state<Channel[]>([]);
  emoji    = new SvelteMap<string, string>();
  bans     = $state<Ban[]>([]);

  get scope() { return `ns:${this.id}`; }
  role(id: string)         { return this.roles.find((r) => r.id === id); }
  member(a: Account)       { return this.members.get(a.handle); }

  // fold over owned channels — no separate unread bookkeeping
  get unread()       { return this.channels.some((c) => c.unread); }
  get mentionCount() { return this.channels.reduce((n, c) => n + c.mentionCount, 0); }
}
```

`serverUnread` / `serverMention*` are now these getters — the manual
recomputation over a name filter is gone. `isNsOwner` is `server.owner === acct`;
`serverCap` / `canModerate` are `me.memberIn(server)?.can(cap)` (see §3.5).

### Session (`session.svelte.ts`) — me + connection

The current user and the connection/auth surface. Its `account` is the same
interned `Account` everyone else references, so "my presence/avatar/status" needs
no special case.

```ts
export class Session {
  account = $state<Account | null>(null);
  network = $state("");   host = $state("");
  reconnecting = $state(false);   insecureMode = $state(false);
  // connect-form: formAccount/formPassword/formEmail/emailRequired/probing/…
  verifications = new SvelteMap<string, { subject: string; state: string }>();
  theme = $state("dark");

  memberIn(server: Server) { return this.account ? server.member(this.account) : undefined; }
  can(cap: string, channel?: Channel) { /* channel override → member → operator(*) */ }
  get isOperator() { /* caps at "*" */ }
}
```

`Session.can(cap, channel)` is the single authority walk that replaces
`serverCap` / `canModerate` / `serverCanGrant` / `canOpenServerSettings`: check
the channel's `overrides` for my roles/self, then my `Member.can`, then operator
(`*`) at network level.

### Social & Federation

- **Social** (`social.svelte.ts`) — `friends`/`groups`/`groupCallRoster` and the
  1:1 call state, all keyed by `Account` references. `friendList` /
  `incomingRequests` / `outgoingRequests` become getters.
- **Federation** (`federation.svelte.ts`) — `netblocks`, `manifests`, and the
  operator actions (`netblockAdd`, `bridgePropose`, …).

### AppStore (`store.svelte.ts`)

Holds the **identity maps** and get-or-create interning (so every reference
resolves to one shared instance), plus the reducer.

```ts
export class AppStore {
  accounts = new SvelteMap<string, Account>();   // the identity map
  servers  = new SvelteMap<string, Server>();
  dms      = $state<Channel[]>([]);              // server-less channels
  me         = new Session();
  social     = new Social();
  federation = new Federation();

  /** Intern: always returns the SAME Account for a handle. */
  account(handle: string): Account { /* get-or-create */ }
  server(id: string): Server       { /* get-or-create */ }
  channel(name: string): Channel | undefined { /* server?.channels or dms */ }

  apply(e: ClientEvent) { /* the reducer — replaces handle(e); dispatches to model methods */ }
}
```

The old `capsFor: Record<"account|scope", …>` and `channels: Record<name,…>`
top-level maps are **gone** — caps live on `Member`/`ChannelOverride`, channels
live under their `Server`.

### Collector (`collector.ts`)

The `*Buf` + `*FetchQueue` pattern (accumulate streamed rows, flush on the
terminal event) becomes one small generic instead of eight ad-hoc pairs:

```ts
export class Collector<T> {
  private buf: T[] = [];
  push(row: T) { this.buf.push(row); }
  flush(): T[] { const out = this.buf; this.buf = []; return out; }
}
```

## 4. Migration phases

Ordered by payoff and independence. Each is a self-contained PR-sized unit that
keeps `npm run check` green; `AppCtx` getters become thin adapters over the new
models, then components migrate to reading models directly and the adapters are
deleted.

The order builds the graph **bottom-up**: identity first (so references have
something to point at), then the Server aggregate, then permissions-as-traversal,
then the reducer that wires it all from events.

- [ ] **Phase 0 — scaffolding + `Account` identity map.** Create `lib/models/`,
      add `svelte/reactivity`, land `Collector<T>` and an `AppStore` with the
      `account(handle)` interning map wired into `provideApp`. Fold
      `presence`/`profiles`/`nicks`(global) onto `Account`; move `displayName`/
      `avatarUrl`/`dotClass`/`initials` onto it. Migrate `Avatar`, `ProfileCard`,
      `ProfileModal` to take an `Account`. This is the foundation everything
      references.
- [x] **Phase 1 — `Channel`** (biggest standalone win). ✅ 2026-07-29.
      `Channel` is now a reactive class (`models/channel.svelte.ts`); the four
      `unreadMap`/`mentionMap`/`unreadCount`/`mentionCount` maps + the `typers`
      map folded into instance fields (`unread`/`mention`/`unreadCount`/
      `mentionCount`/`typers`) with `markRead()`/`bump()` methods.
      `+page.svelte`'s `channels` record now holds class instances (Svelte 5
      leaves them unproxied, so their `$state` fields stay reactive nested in the
      `$state` record — no `SvelteMap` migration needed, ~40 call sites
      untouched). `serverUnread`/`serverMention`/`serverMentionCount` fold over
      `Object.values(channels)`; `markRead(name)` → `channels[name]?.markRead()`;
      rename/delete/`AppCtx` adapters dropped; `ChannelList`/`DmList`/
      QuickSwitcher read `ch.*`. **Deferred:** `messages[].author` / `typers`
      staying `string` handles (→ `Account` refs land with the Message model);
      mute (`notifPrefs`) is a per-namespace setting → moves to `Server`, not
      `Channel`.
- **Phase 2 — `Server` ▸ `Member` ▸ `Role` graph.** The core of this revision.
      Split into sub-phases because it spans namespace identity, membership,
      emoji, roster, roles, grants, caps, and mute.
  - [x] **2a — `Server` namespace aggregate.** ✅ 2026-07-29.
        `models/server.svelte.ts` — the namespace aggregate, interned by
        `AppStore.server(id)`. Folded `discovered` (→ `applyMeta` + fields),
        `memberNs` (→ `Server.joined`), and `customEmoji` (→ `Server.emoji`
        `SvelteMap`). `serverName`/`isNsMember`/`serverNamespaces`/`activeEmoji`/
        `emojiUrlFor`/`nsCategories` now read `store.servers`; `activeNsMeta` is a
        thin legacy-shaped `$derived` adapter (snake_case fields the modals
        already read) so ~18 call sites stay untouched. `AppCtx.discovered`
        (record) → `discoverList: Server[]`; `DiscoverModal` migrated. Bonus:
        `openDiscover` no longer wipes member-server metadata (clears only
        non-joined servers), removing the transient blank-rail flash.
  - [x] **2b — `Membership` roster + `Channel`↔`Server` edge.** ✅ 2026-07-29.
        `models/membership.svelte.ts` — the Server↔Account join (§6.2 NS INFO
        MEMBERS roster), named `Membership` to avoid colliding with the existing
        channel-presence `Member` (`{name, origin}`). `account` is a shared
        interned `Account` ref; `roleIds` are the ns-scoped role ids;
        `joinedMs`/`network`. Folded `nsMembersByNs` → `Server.members`
        (`Membership[]`) + `Server.member(handle)`. `Channel.server` back-ref set
        in `ensureChannel` (the upward graph edge). Reducer flush builds
        `Membership`s (interning the `Account`); `memberRow`/`assignNsRole`/
        `unassignNsRole` operate on `Server.members`. `AppCtx.nsMembersByNs`
        (record) → `nsMembers(ns): Membership[]`; `ServerSettingsModal` migrated
        (`m.account.name` for the bare handle). Dead `MemberInfoC` type removed.
        **Deferred to Phase 3** (entangled with the caps tables): the
        `RoleDefC`→`Role` class conversion and `Membership.roles: Role[]` (direct
        refs instead of `roleIds` + external `roleById`).
  - [x] **2c — mute onto `Server` (via a store singleton).** ✅ 2026-07-29.
        `AppStore` is now a **module singleton** (`export const store`), so models
        can navigate to shared state without a ref threaded through every ctor —
        the enabling pattern for `Channel.isMuted` here and the Phase 3
        permission walk. `notifPrefs` folded into `store.notifPrefs` (`SvelteMap`,
        localStorage-backed via `notifAt`/`mutedAt`/`setNotif`, SSR-guarded).
        Added `Server.muteLevel`/`Server.isMuted` and `Channel.isMuted` getters —
        mute is now a graph walk (channel → `server` → prefs, else `net` scope).
        `ChannelList` reads `ch.isMuted`; `+page` mute helpers are thin store
        wrappers; removed the `notifPrefs` `$state` map + dead `channel-renamed`
        migration. The store↔model import cycle is lazy-only (getters) and
        bundles clean (`npm run build` ✓). **Deferred to Phase 3:** grants
        (`grantsByScope`) + deny-list (`modDeny`) — they're scope-keyed
        permission/moderation state that restructures with the caps walk.
- [ ] **Phase 3 — roles + permissions as traversal.** Now also absorbs the
      pieces deferred from Phase 2: convert `RoleDefC` → `Role` class (with
      `grants(cap)`), give `Server.roles` + `Membership.roles: Role[]` (direct
      refs), and fold `grantsByScope` + `modDeny` onto `Server`. Then introduce
      `ChannelOverride` + `Session.can(cap, channel)` / `Membership.can(cap)`;
      delete `capsFor` and the `serverCap`/`canModerate`/`serverCanGrant`/
      `canOpenServerSettings`/`badgeFor` functions, repointing every gate at the
      walk (models reach the caps via the `store` singleton — established in 2c).
      This is the change that most directly removes "convoluted" string-key
      permission logic.
- [ ] **Phase 4 — `Session` + `Social` + `Federation`.** Fold the connect-form
      `$state` soup, friends/groups/calls (→ `Account` refs), netblocks/manifests.
- [ ] **Phase 5 — `AppStore.apply` reducer.** Move the ~800-line `handle(e)`
      switch into `store.apply(event)` dispatching to model methods; replace the
      `*Buf`/`*FetchQueue` pairs with `Collector`. This is the phase that
      actually shrinks `+page.svelte`.
- [ ] **Phase 6 — component extraction, now unblocked.** With the graph in
      context, split the remaining large views (chat pane, rail, sidebar, modal
      cluster) so they read models directly. `+page.svelte` collapses toward a
      thin shell wiring `store → layout`. Continues the in-progress
      componentization.

## 5. Decisions & invariants to preserve

- **Scope strings stay on the wire; caps resolve through the graph.** Commands
  and the cap tables still speak `"ns:<id>"` / `"*"` / `#chan` — that's the
  protocol. What changes is the *client*: instead of a `capsFor["account|scope"]`
  map, authority is a walk (`ChannelOverride` → `Member` roles+grants →
  operator `*`). `Server.scope` produces the wire string when a command needs it.
- **Objects are interned, never duplicated.** Anything that names an account
  (message author, member, friend, DM peer, typing indicator) holds the one
  `Account` from `store.account(handle)`. The reducer resolves handles → refs at
  the edge; the rest of the app never re-looks-up by string.
- **View state ≠ domain state.** Modal-open flags, composer draft, active
  selection, drag/drop, editing keys stay in the container (or a small
  `UiState`), never on domain models.
- **Pure helpers stay pure.** `renderMd`, `dayLabel`, `retentionMeta`,
  `initials` → `lib/format.ts`; not methods.
- **`AppCtx` is the seam, not the destination.** It shrinks every phase; the end
  state has components importing models from context, with `AppCtx` reduced to
  cross-cutting actions (`toast`, `confirm`, `expectSuccess`) and navigation.
- **localStorage keys unchanged** (`weft:sync:…`, `weft:dms:…`,
  `weft:email-nudge-dismissed:…`, notif prefs) — persistence format is not part
  of this refactor.

## 6. Verification per phase

1. `npm run check` (svelte-check) — 0 errors, 0 warnings.
2. Manual smoke of the touched surface (rail unread dots, channel open/mark-read,
   member list, server settings) — no server rebuild needed; client-only.
3. Update `reviews/code-navigation.md` when a model lands (per repo convention).

## 7. Non-goals

No wire/proto/store/server changes. No new dependencies. No change to the sync
protocol, persistence format, or capability scheme. Voice/media state
(`voice.svelte.ts`, `callmedia.svelte.ts`) is left as-is except where
`SocialModel` absorbs the 1:1 call state in Phase 4.
