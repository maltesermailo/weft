# Code Navigation Guide — weftd

*How to find your way around, written after M3a (134 tests). File sizes and
function names are accurate as of this milestone; line numbers will drift,
so pointers are `file :: function` — grep for the function name.*

## The 30-second map

Requests flow **down** through the layers; events flow **back up**. Every
crate boundary is also a testing boundary.

```
weftd        glue: config, key files, TLS, accept loops     (~450 lines total)
  │
weft-transport   bytes → lines (QUIC framing, WS frames)    never parses verbs
  │
weft-core        the actual server: sessions, actors        never touches sockets
  │         ├── weft-crypto   keys, attestations, argon2    pure, no I/O
  │         └── weft-store    EventStore/AccountStore,      pure logic + memory impl
  │                           §12.1 materialization
weft-proto       the wire: Line grammar, Command, Event     pure, fuzzable
```

Biggest files, where you'll spend most time:
`weft-core/src/session.rs` (~1150 — the protocol brain),
`weft-proto/src/event.rs` (~920) and `command.rs` (~700 — mechanical
parse/serialize arms), `weft-core/src/channel.rs` (~410 — the actor).

## Chain 1: boot — `main` to a listening server

1. `weftd/src/main.rs :: main` — parses argv, loads TOML, calls `start`.
2. `weftd/src/lib.rs :: start` — the whole boot recipe in one function,
   top to bottom: validate network/channel names → load-or-generate the
   signing key (`load_or_generate_key`) → build `ServerCtx` → TLS
   (`load_tls` PEM or `self_signed` rcgen) → QUIC endpoint → spawn accept
   loops (+ optional WS, + optional well-known HTTP).
3. `weft-core/src/context.rs :: ServerCtx::new` — wires the store into
   `Accounts` and hands it to `Registry::spawn`.
4. `weft-core/src/registry.rs :: Registry::spawn` → one
   `channel::spawn` per configured channel — **channel actors already
   exist before the first connection arrives**.

## Chain 2: a connection — accept to session loop

1. `weftd/src/acceptor.rs :: accept_quic` — one spawned task per
   connection; QUIC handshake, then `QuicControlStream::accept` waits for
   the client to open the control stream.
2. Same file, `QuicLines` / `WsLines` — the ~10-line adapters that turn a
   transport stream into weft-core's `ControlStream` trait
   (`weft-core/src/stream.rs`). This is the only place transport and core
   meet.
3. `weft-core/src/session.rs :: run_session` — entry point; makes a
   `Session`, runs it, cleans up (parts channels, flushes the stream).
4. `Session::run` — **the select loop**. Three wake sources, one each:
   inbound line, queued channel event, idle deadline. Everything the
   session ever does starts here.

## Chain 3: inbound — a line becomes an action

Follow a `MSG #general :hi` from socket to actor:

1. `Session::run` → `on_line` — two-stage parse:
   `Line::parse` (grammar, `weft-proto/src/line.rs`) then
   `Request::from_line` (typed verb, `weft-proto/src/command.rs`).
   Parse failures → `on_malformed` (5 strikes/60 s closes).
2. `on_request` — the FSM gate: dispatches on `self.state`
   (`Negotiating | Unauthed | Ready`). Unknown verbs are dropped here,
   before any state logic (§4).
3. `on_ready` — the verb → handler match. Every READY verb has an
   `on_<verb>` method below it in the same file.
4. `on_msg` — session-side checks in order: target kind → attachments →
   empty body → membership → **label dedup** (§9.2, the `dedup` map) →
   `push pending label` → `ChannelHandle::publish`.
5. `weft-core/src/channel.rs :: Actor::handle(Cmd::Publish)` — the single
   writer: `mint()` assigns the msgid (the ONLY place msgids are born),
   `persist()` appends to the store (skipped for ephemeral), `broadcast()`
   fans out.

EDIT/DELETE/REACT take the same shape with one extra hop:
`on_edit`/`on_delete`/`on_react` → `resolve_message` (the shared
origin/existence/tombstone/membership/authorship checks) → actor.

## Chain 4: outbound — an event becomes bytes (the "main to event" chain)

This is the fan-out path; read it once and the concurrency model is clear:

1. `Actor::broadcast` sends `ChannelEvent { origin, event }` into the
   channel's `tokio::broadcast` ring (512 slots).
2. Each member session has a **forwarder task** pumping that ring into the
   session's own bounded queue —
   `weft-core/src/session.rs :: spawn_forwarder` (bottom of the file).
   Lag here becomes `SessionEvent::Lagged` → `ERR SLOW` (§9.2). Forwarders
   are created in `on_join`, aborted in `on_part`/`cleanup`.
3. Back in the select loop, `Session::on_event`:
   - `origin != me` → serialize with **no label** (broadcast copy, §3.5);
   - `origin == me` and it's MESSAGE/EDITED/DELETED/REACTION → pop the
     per-channel `pending` label FIFO → this copy **is the ack** (§9.2),
     and labeled MSG echoes are cached in `dedup` for retry replay.
4. `Reply::serialize` (`weft-proto/src/event.rs`) → `stream.send_line` →
   transport framing (`weft-transport/src/quic.rs` LinesCodec / `ws.rs`
   text frame) → wire.

Why the label FIFO is safe: one mpsc into one actor preserves a session's
own command order across all four event types, so echoes come back in send
order. That argument is written down at `struct Joined` in session.rs.

## Chain 5: HISTORY — the read path (bypasses the actor)

`on_history` (session.rs) → membership + policy checks →
`ctx.events.roots/children` (trait: `weft-store/src/traits.rs`, impl:
`memory.rs`) → **`weft-store/src/materialize.rs :: materialize`** — the
§12.1 pure function, the most invariant-dense code in the repo — → batch
events, every line labeled. Reads never touch the channel actor; only
writes need its ordering.

## Chain 6: auth — UNAUTHED to READY

`session.rs :: on_unauthed` is the seam. REGISTER/AUTH PASSWORD →
`weft-core/src/accounts.rs` (uniformity semantics: dummy-hash for unknown
accounts) → `AccountStore` + `weft-crypto/src/password.rs` (argon2).
AUTH KEY/PROOF → `weft-crypto/src/challenge.rs` (nonce‖network) →
`ctx.mint_attestation` (`context.rs`) → `weft-crypto/src/attestation.rs`.
The public half of the signing key is served by
`weftd/src/wellknown.rs`.

## "I want to change X — where do I go?"

| Change | Touch (in order) |
|---|---|
| New verb/event | `weft-proto` command.rs/event.rs **+ round-trip test first** (CLAUDE.md rule), then session.rs handler |
| Chat-list performance (why switching channels is fast) | Three layers, all client-side: (1) **message data cache** — `Channel.messages` persists in the store, so switching never re-fetches (`historyLoaded` stays true); (2) **memoized parsing** — `renderMd` is an LRU cache (`mdCache`, keyed `activeServer\0body`, cap 4000) so re-mounting a channel renders from cache instead of re-parsing markdown/highlight; cleared on emoji add/remove + role flush (ns-scoped inputs); (3) **true virtualization** — `MessageList.svelte` renders via **@tanstack/svelte-virtual** (headless `createVirtualizer`): constant DOM regardless of history length. WE own the scroll element (`$virtualizer` only in the template; effects/actions read it non-reactively via `get()` to avoid an onChange feedback loop). Data is chronological (oldest→newest); bottom-anchor is native — `scrollEl.scrollTop = scrollHeight` re-asserted over a few rAFs as rows measure in (`pinBottom`); scroll-to-unread lands on the first-unread offset; load-older holds distance-from-bottom (`restoreOlder`) while older pages measure. Row heights are dynamic via `use:measure` → `$virtualizer.measureElement` (cached by msg key, survives prepend). Dividers + the top-of-history indicator render inside each row. **No keep-alive** — the channel route (`{#key active}`) remounts the list per channel; only the active channel's list is mounted (a hidden `display:none` virtualized list can't be measured). **Images**: `Attachment.svelte` reserves the box from stamped dims (`mediaDims`, zero layout shift) + a shimmer held until `onload`, fading in |
| Client routing / which view is shown | Path-based SvelteKit routes (SPA — `adapter-static`, `fallback: index.html`, `ssr=false`; deep links resolve client-side, incl. packaged Tauri). The **container** (connection lifecycle, the `handle(e)` reducer, all `$state`, `provideApp(ctx)`, the chrome — rail/sidebar/members — and every modal overlay) lives in **`client/src/routes/+layout.svelte`**, NOT `+page.svelte`. View state is URL-derived: `active`/`activeServer`/`homeView` = `$derived(nav.viewFrom(page.route?.id, page.params))` (`page` from `$app/state`). Navigation is `goto(nav.pathFor(...))` (`$app/navigation`) — **`client/src/lib/nav.ts`** is the pure URL↔view codec: `/` home, `/c/[server]` (server, no channel), `/c/[server]/[channel]`, `/dm/[peer]`, `/g/[group]`; top-level (network) channels use the `~` server sentinel. Route pages are thin — root `+page.svelte`→`FriendsView`, `c/[server]`→`EmptyHome`, the channel/dm/group routes→shared **`components/chat/ChatView.svelte`**. The reducer's nav side-effects `goto()` too; a dropped connection preserves the view (URL survives reconnect) |
| Client domain state (accounts / channels / servers / permissions) | In-progress refactor from the `+page.svelte` string-keyed record soup to a reactive **object graph** — plan + phases in `docs/architecture/client-model-refactor.md`. Models live in `client/src/lib/models/*.svelte.ts` (`$state` class fields, `SvelteMap` collections). **Landed (Phase 0):** `Account` (global identity: profile + presence) interned by `AppStore.accountOf(handle)` in `store.svelte.ts`; the old parallel `presence`/`profiles` records are gone. `+page.svelte` holds `const store = new AppStore()`; identity helpers (`dotClass`/`avatarUrl`/`displayName`/`bioOf`/`statusOf`/`queryProfile`) are thin views over `store.accountOf(...)`, exposed on `AppCtx` as `accountOf(handle): Account` (NOT `account`, which is the current-user handle string). Components read `app.accountOf(x).presence` etc. **Landed (Phase 1):** `Channel` is a reactive class (`models/channel.svelte.ts`) — the four `unreadMap`/`mentionMap`/`unreadCount`/`mentionCount` maps + `typers` map folded into instance fields (`ch.unread`/`ch.mention`/`ch.unreadCount`/`ch.mentionCount`/`ch.typers`) with `markRead()`/`bump(mentioned)`. `channels: Record<string, Channel>` now stores class instances (Svelte 5 does NOT proxy class instances, so their `$state` fields stay reactive nested in the `$state` record — no SvelteMap migration). `serverUnread`/etc. fold over `Object.values(channels)`; sidebar reads `ch.*` directly. `Channel` type moved out of `types.ts` → re-exported from `context.ts`. Still-string (not yet Account refs): `Msg.author`, `Channel.typers`. **Landed (Phase 2a):** `Server` namespace aggregate (`models/server.svelte.ts`) interned by `AppStore.server(id)` — folded `discovered` (→ `Server.applyMeta` + fields), `memberNs` (→ `Server.joined`), `customEmoji` (→ `Server.emoji` SvelteMap). `serverName`/`isNsMember`/`serverNamespaces`/`activeEmoji`/`emojiUrlFor`/`nsCategories` read `store.servers`; `activeNsMeta` is a legacy-shaped `$derived` adapter (snake_case, so its ~18 call sites are untouched); `AppCtx.discovered`→`discoverList: Server[]` (DiscoverModal migrated). `openDiscover` clears only loaded non-member servers (keeps member metadata + channel-interned servers). **Landed (Phase 2b):** `Membership` (`models/membership.svelte.ts`) — the Server↔Account join (NS INFO MEMBERS roster), named `Membership` to avoid the existing channel-presence `Member` (`{name,origin}`). `account` is an interned `Account` ref; `roleIds`/`joinedMs`/`network`. Folded `nsMembersByNs`→`Server.members` (`Membership[]`) + `Server.member(handle)`; `Channel.server` back-ref set in `ensureChannel`. `AppCtx.nsMembersByNs`→`nsMembers(ns): Membership[]`; `ServerSettingsModal` uses `m.account.name` for the bare handle; dead `MemberInfoC` removed. Deferred to Phase 3: `RoleDefC`→`Role` class + `Membership.roles: Role[]` (still `roleIds`+`roleById`, entangled with `rolesByScope`/`capsFor` across ns/`*`/`#chan`). **Landed (Phase 2c):** `AppStore` is now a **module singleton** (`export const store` in `store.svelte.ts`) — `+page` and the domain models all import the one instance, so models navigate to shared state (the enabler for Phase 3's permission walk). `notifPrefs`→`store.notifPrefs` (SvelteMap, localStorage via `notifAt`/`mutedAt`/`setNotif`, SSR-guarded). Mute is now a graph walk: `Channel.isMuted` (→ `server` → prefs, else `net`), `Server.muteLevel`/`isMuted`; `ChannelList` reads `ch.isMuted`. Store↔model import cycle is lazy-only (getters); `npm run build` ✓. Deferred to Phase 3: `grantsByScope`+`modDeny` onto `Server`. **Landed (Phase 4a/4b):** `Federation` (`models/federation.svelte.ts`) — `netblocks`+`manifests` SvelteMaps on `store.federation` (`ManifestInfo`); `AppCtx.netblocks`/`manifests`→`ReadonlyMap`; `FederationPanel` spreads them. `Social` (`models/social.svelte.ts`) — friends/groups/calls on `store.social` (`friends`/`groups`/`groupCallRoster` SvelteMaps + `incomingCall`/`activeCall`/`activeGroupCall` `$state`); userrefs stay `account@network` strings (resolved at the UI edge via `accountOf`); `group-member` re-`set`s entries (SvelteMap values aren't deeply reactive). Phase 4c (Session/connect-form) deferred — those are scalars (`account`/`network`/`host` read 173/62/27×), not the parallel-map target. **Landed (Phase 3a/3b):** `Role` class (`models/role.svelte.ts`, `RoleDefC` gone) — `$state` fields + `grants(cap)`, `caps` stays `string[]`; `rolesByScope` holds `Role[]`, reducer builds `new Role(…)`. `Server.roles` (+`Server.role(id)`) mirrored from the `ns:<id>` ROLE flush (same array ref as `rolesByScope`); `Membership.roles` resolves `roleIds` via `server.role(id)` — the "members have roles" graph edge. 3b is low-risk (rolesByScope + all gate/display helpers untouched, no component moved). **NOTE the client uses SERVER-resolved caps** (`capsFor[account\|scope]` from `caps` events); gates (`serverCap`/`canModerate`/`serverCanGrant`/`canOpenServerSettings`/`canModDelete`/`isOwnerAt`/`isStaff`/`badgeFor`) walk the SCOPE hierarchy (channel→ns→`*`) over those, NOT roles. **Landed (Phase 3c core):** `Session` (`models/session.svelte.ts`) — `capsFor`→`store.session.caps` (SvelteMap `account|scope`) + `account` (the "me" identity, set on `connected`). Gates are now `Session` methods: `can(cap,scope)`/`moderates(scope)`/`canGrant(scope)`/`ownerAt(a,scope)`/`capsAt(a,scope)`/`isOperator`; `Badge` moved onto the model. The `+page` gate fns (`canModerate`/`serverCap`/`canModDelete`/`serverCanGrant`/`isOwnerAt`/`isStaff`/`badgeFor`) keep their SCOPE-selection + `ensureCapsAt` logic, only the lookup relocated (gating provably identical); AppCtx gate methods stable, no component moved. **Landed (3c-tail):** `grantsByScope`→`store.grants`, `modDeny`→`store.deny` (store-level SvelteMaps + `GrantRow`/`DenyRow`; kept store-level not Server — scope-keyed across ns/`*`/`#chan`; setters re-`set` the entry). `rolesByScope` ns-slice consolidated into `Server.roles` via a `rolesAt(scope)` helper (ns→`Server.roles`, else by-scope); `AppCtx.rolesByScope`→`rolesAt(scope)`; `rolesByScope` now holds only `*`/`#chan`. **Phase 3 COMPLETE.** **Phase 5 REASSESSED — reducer NOT relocated:** `handle(e)` in `+page` is a UI orchestrator (navigation + lifecycle + panels + side-effects + model mutation intermixed), not a pure reducer; moving it to `store.apply()` would invert deps (store→UI) and Svelte 5 `$state` locality blocks a module move while UI/nav state stays in `+page`. The `Collector` buffer-fold is churn over native arrays → skipped; `collector.ts` is effectively unused. `+page` shrinks via **Phase 6** (component extraction), not reducer relocation. **Landed (Phase 6 — Search + Pins panels):** `models/panels.svelte.ts` (`SearchPanel`+`PinsPanel` on `store.search`/`store.pins`) hold open/query/scope/results/loading + the `buf`/`loadingChannel` streaming machinery the +page reducer writes; `SearchModal` owns `runSearch`/`jumpToResult` (reads `store.search`, calls `weft.search`), `PinsModal` reads `store.pins.list`. `AppCtx` lost `searchOpen`/`searchQuery`/`searchScope`/`searchResults`/`searching`/`runSearch`/`jumpToResult`/`pinsList` (kept `openSearch`/`openPins`/`togglePin`). **Landed (Phase 6 — Threads + Invites):** `models/threads.svelte.ts` (`Threads`: side-panel root/messages/composer + list + `names` SvelteMap + streaming bufs) and `models/invites.svelte.ts` (`Invites`: list + create + streaming; owns `InviteInfo`, re-exported by `context.ts`) on `store.threads`/`store.invites`. Reducer routes MESSAGE/THREAD/INVITED/BATCH into them; live thread replies append to `store.threads.messages`. Because these span many components (threads: ThreadPanel/ThreadsModal/MessageItem/ChatTopbar + msg reducer; invites: InviteList/InvitesModal/InviteCreateModal/ServerSettingsModal/SidebarHeader/ChatTopbar), state moved to the models but `AppCtx` getters/setters **delegate** to them → zero component churn (vs Search/Pins where the 2 modals read `store` directly + AppCtx shrank). **Landed (Phase 6 — Reports + roster):** `models/reports.svelte.ts` (`Reports`: queue `SvelteMap<report_id, ReportInfo>` + `open`/`target`; `RESOLVE_ACTIONS`) on `store.reports` — reducer writes it, `ReportsQueueModal` reads `store.reports` + `RESOLVE_ACTIONS` directly (AppCtx shed `reportQueue`/`resolveActions`; kept `openReport`/`openReports`). Roster fetch-state moved onto `Server`: `membersLoading` (reactive spinner gate) + transient `memberBuf` (streamed `ns-member-info` rows); `+page`/layout keeps only `loadingNsMembers` (the reducer in-flight cursor, since the events omit the ns). `AppCtx.nsMembersLoading` reads `store.servers.get(activeServer)?.membersLoading`. Then optional big-view extraction. **Landed (4c):** `ConnectForm` (`models/connect.svelte.ts`) groups the login-screen cluster (mode/host/account/password/email/serverStep/emailRequired/probing/insecure/authError/authFailed/deviceKeyAvailable); held `+page`-local as `const cf` (ephemeral pre-auth UI). `ConnectScreen` now takes ONE `form` prop (mutated by ref, no `bind:`) instead of 13 bindable props. Identity/lifecycle scalars (`account`/`network`/`status`) intentionally left in +page. `Collector<T>` awaits Phase 5. Remaining: **Phase 5** — the `apply()` reducer that finally shrinks `+page.svelte` |
| Client-core model (the Rust parity mirror: `weft-client-core/src/model/`) | Wire events → `AppState::reduce` (`model/mod.rs`) → `StateDiff`s the TS mirror applies. One module per migrated domain (`channels`/`presence`/`moderation`/`reports`/`emoji`/`roles`/`invites`/`federation`/`social`/`threads`/`messages`), each owning state + a `handle(event)->Vec<Diff>` + its diff enum (federation is stateless — a pure event→diff transformer); `reduce` offers each event to every domain (the Rust twin of the TS `domainHandlers` spread). Streamed-list domains (roles/invites/threads-list) key their batch flush on the batch-id prefix (`r`/`il`/`t`) so the reducer's `batch-end` just consumes the boundary. **Split domains** keep side-effects TS: `social` owns friends+groups (calls stay TS — LiveKit/toasts/nav); `threads` owns names+list (the reply panel stays TS — it rides the `b` message-history path). Emitted by **both** wrappers on the same callback right after the raw event (`weft-client-wasm` `JsSink::emit`, `client/src-tauri/src/weft.rs` sink) — TS routes on `kind`; kinds it doesn't own yet are ignored. Host-invoked (local, no wire) commands live on `AppState` + a dispatch arm in each wrapper: `move_channel`/`move_category`/`typing_stop`/`mod_refresh`/`reports_clear`. **Messages capstone** (design: `docs/architecture/client-core-model-migration.md`) — `model/messages.rs` is the per-channel ordered **store** (local-echo→ack reconcile, edit/redact/react, unread/mention tally); a live `MESSAGE` is `ingest`ed by `reduce` (computes `mentioned` via `roles::mentions_me` — the roles domain keeps a small stored copy of defs+memberships for it). Two-tier IPC: `set_open_channels` (subscription scope — body diffs only for open channels, else just `UnreadChanged`), `send_message` (optimistic echo), and the pull `messages_range`. **M3 done; TS still on its own message path until the M4 cutover.** |
| A verb the *client* must send | the full chain, in order: `weft-proto` (command + round-trip test) → `weft-store` trait + `memory.rs`/`postgres.rs` + a case in the shared `tests/backends.rs` contract → `weft-core/src/session/<area>.rs` handler + `session.rs` dispatch → `weft-client-core/src/lib.rs` `build_*` → **both** frontends (`weft-client-wasm/src/lib.rs` dispatch arm *and* a `#[tauri::command]` in `client/src-tauri/src/lib.rs` + its `generate_handler!` entry) → `client/src/lib/weft.ts` wrapper → `+page.svelte` action + `AppCtx` in `context.ts` → the component. Missing either frontend leaves web or desktop silently broken |
| §6.5 roles (define / order / rename / assign) | `weft-core/src/session/roles.rs` (all handlers); store in `RoleStore` (`traits.rs` + both impls; `rename_role` migrates definition **and** membership together); client UI is `client/src/lib/components/modals/RolesTab.svelte` (two-pane: role list + Display/Permissions/Members editor tabs), backed by `saveRole`/`reorderRoles`/`moveRole` in `+page.svelte`. Rename is a store migration, never delete+create — the latter drops every assignment |
| §6.5 channel permissions (per-target editor: @everyone / role / member) | Editor UI = the Permissions tab in `client/src/lib/components/modals/ChannelSettings.svelte` (two-pane, `.cp-*` styles in `app.css`). Three composed mechanisms: **@everyone** = a channel-scoped `everyone` role, enforced by `ctx.actor_has_cap` in `weft-core/src/context.rs` (baseline block resolves the channel `everyone` role alongside `ns:`; test `channel_everyone_role_grants_a_per_channel_baseline`); **role overrides** = channel-scoped roles (`setChanRoleCaps(name,color,caps)` in `+page.svelte` → `createRoleAt`/`deleteRoleAt`, always-propagate in `roles.rs`); **member overrides** = direct grants (`setChanMemberCaps`/`removeChanMember` → `weft.grant`/`revoke`). Both editors (channel perms + `RolesTab`) commit via a draft + the shared `SaveBar.svelte` (Revert/Save, profile-editor pattern), not per-toggle. Enumeration = new **`GRANTS <scope>`** verb: `Command::GrantsAt`/`Event::GrantInfo` (weft-proto) → `on_grants_at` in `roles.rs` (ns-admin gated; filters role-propagated grants via `role_members`; resolves handles→ULID through `account_ulid`; `gr…` BATCH; test `grants_lists_member_overrides_but_not_role_holders`) → `build_grants_at` → wasm `grants_at` + Tauri cmd → `weft.ts grantsAt` → `+page.svelte` (`grant-info` into `grantBuf`, flushed on `gr`-prefixed `batch-end` into `grantsByScope`; `fetchGrants`) → `AppCtx`. **Access gating**: `view` is in `CHAN_CAPS` (grant it per target to admit them); the channel's view-gate flag is the Overview **Private channel** toggle (`toggleViewGated` → `channelMeta(#chan, "view-gated", true/false)`, `viewGated` on the client channel record). Enforcement is `view_gated_denied` in `session.rs` (`Capability::View` at the channel scope; invariant 1 — hidden ≡ absent; tests `view_gated_channel_hides_without_the_view_cap`). Initial flag state: `join_one` (`relay.rs`) pushes a label-less `CHANMETA view-gated`/`posting` after `POLICY` when the flag is set, so the client's toggles open with accurate state (a plain channel pushes nothing; joining a gated/restricted channel emits one extra `CHANMETA` to the joiner only). **Permission-model refinements:** `@everyone` seeded with `send,invite` on `NS CREATE` (`on_ns_create` in `namespaces.rs`); `edit-own`/`delete-own` dropped from the editors (own edit/delete is authorship-only, always allowed); `delete-any` now enforced — `resolve_message` takes an `author_override: Option<Capability>` (`Some(DeleteAny)` for DELETE) so a non-author moderator can delete (test `delete_any_lets_a_moderator_remove_another_members_message`); client `msgCtx` offers Delete on join/part system lines + others' messages via `canModDelete()`; mutes/bans/permission changes no longer post channel system lines (the `moderated`/`token` handlers only update `modDeny` / toast) |
| §6.2 `NS INFO MEMBERS` (moderator roster: members + join time + roles) | `Command::NsInfo`+`NsInfoKind` / `Event::NsMemberInfo` in `weft-proto`; store `RoleStore`-adjacent `MembershipStore::ns_members_joined` (both impls + `tests/backends.rs`); handler `on_ns_info_members` in `weft-core/src/session/namespaces.rs` (cap-gate = any of ns-admin/ban/kick/mute/reports at `ns:<name>`, emits an `ni…` BATCH). Client chain: `build_ns_info_members` → `ns_info_members` (wasm arm + Tauri cmd) → `weft.ts nsInfoMembers` → `+page.svelte` (`ns-member-info` accumulates in `nsMemberBuf`, flushed on the `ni`-prefixed `batch-end` into `nsMembersByNs`; `fetchNsMembers` + `AppCtx`) → the **Members** page in `ServerSettingsModal.svelte`. Join time from the v0.12 `weft_ns_membership.joined_ms` (0 = pre-v0.12 backfill). **Inline moderation on the roster** (no dedicated verbs — reuses existing ones): `assignNsRole`/`unassignNsRole` in `+page.svelte` (optimistic roster mutation + reconcile refetch, over `weft.roleAssign`/`roleUnassign` at `ns:<name>`); `nsMemberCtx` builds the right-click menu over `moderate`/`liftMod` (ns-scope mute/ban + lifts, keyed off `denyList()` populated by `refreshBans()`); owner crown from `activeNsMeta.owner` |
| §6.2 welcome channel (greet new members) | `NamespaceRecord.welcome_channel` (store, migration `0044`, `set_namespace_welcome` mem+PG + contract). Set via `NS META <ns> welcome :<#chan>` (`on_ns_meta` in `namespaces.rs`, ns-admin); `NsMeta` event carries `welcome=` (`ns_meta_event`). On first ns membership (any of NS JOIN / `JOIN #ns/chan` / invite redeem — each first-join-gated), `post_ns_welcome` calls the channel actor's `announce_welcome` (`Cmd::Welcome` → `announce_membership(SENTINEL_ORIGIN, user, "welcome")`), a persistent `system=welcome` line. Client: `+page.svelte` formats the `welcome` system kind as "👋 Welcome, X!"; Server Settings → Overview welcome-channel `<select>` (`nsSetWelcome` → `NS META welcome`); `activeNsMeta.welcome`. Test `ns_welcome_channel_greets_new_members` |
| Social layer: group DMs | `GroupId` = `&<ulid>` target sigil (`Target::Group`) + `Scope::Group` (store key `&<ulid>`). Store: `GroupStore` (mem+PG, migration 0033: `weft_groups`+`weft_group_members`). Messaging rides the **directory** (`Cmd::GroupMsg`/`group_msg`/`deliver_many`) — single-writer ULID mint like DMs, NOT the channel actor. Membership handlers: `weft-core/src/session/groups.rs` (`on_group_create` mints `GroupId(Ulid::new())`, add/remove/leave/name broadcast via `directory.notify`). `on_msg`/`on_history` `Target::Group` = membership-gated, local-member fan-out (cross-network deferred). Client: `groups` state + `createGroup/openGroup/leaveGroup/groupLabel` in `+page.svelte`, groups in `dmList` (`DmList.svelte`), `FriendsView` create input, `ChatTopbar` group branch. |
| Social layer: friends (federation-able) | `FRIEND ADD/ACCEPT/REMOVE` + `FRIENDS` handlers in `weft-core/src/session/friends.rs` (`on_friend_*`); everything keys on `UserRef` (`account@network`) so local + cross-network share one path. Store: `FriendStore` (`traits.rs` + both backends, migration `0032`, symmetric one-row-per-pair with `requested_by`); `ctx.friends`. Same-network delivery = `directory.notify`; **cross-network delivery is deferred** (needs the bridge user-event transport — the §18 cross-network-DM primitive). Proto: `FriendState` enum + `Command::Friend*` + `Event::Friend`/`FriendRemoved`. No existence check (anti-enumeration). **Cross-network**: reuses the §11.10 FSession tunnel. Receive = `on_federated` runs friend cmds as the foreign caller (handlers take a `UserRef` caller, local or federated). Send = `deliver_if_remote` (friends.rs) emits `FriendDeliver` via `ctx.request_friend_deliver` (port like `mirror_tx`); weftd `dialer::spawn_friend_deliver_consumer`/`deliver_friend` dials a fresh authed bridge + tunnels `FSESSION OPEN/CMD` (SSRF-guarded, `auto_bridge=open`). Fire-and-forget; each network keeps its own edge copy. **Client**: `FriendsView.svelte` (home main pane when `homeView && !activeChannel`) + Friends button in `sidebar/DmList.svelte`; `friends` state + `addFriend/acceptFriend/removeFriend/messageFriend/openFriends` in `+page.svelte`; `weft.ts friendAdd/Accept/Remove/listFriends`; client-core `build_friend_*` + `ClientEvent::Friend`. |
| §11.10 auto-federation reachability (public + invite-gated non-public) | Consent gate `on_bridge_request_in` (`session/federation.rs`): `reachable = rec.federation && (public \|\| invite_authorizes_ns(...))` — the invite validated **non-consuming** via `ctx.invites.invite(id)` (scope `ns:<ns>`, unexpired, not exhausted; revoked = absent); wrong/missing invite → uniform `NO-SUCH-TARGET` (invariant 1). The `federation` flag is mandatory in both paths and off by default; the `NS META <ns> federation :open` gate in `session/namespaces.rs` was relaxed to allow any visibility. Invite threads home→peer: proto `Command::Federate`/`BridgeRequest` carry `@invite=` → `on_federate` → `AutoBridgeRequest.invite` (context.rs) → weftd `dialer` (`auto_bridge`/`run_peer_requester`) → `run_bridge_requester` → `OutboundStart::Request(ns, invite)` → `begin_outbound_request` emits `@invite= BRIDGE REQUEST`. Client: `build_federate(target, invite)` → `federate` (wasm/tauri/`weft.ts`) → `DiscoverModal` (a foreign invite link `weft://net/ns/i/<id>` extracts `<id>`; a manual invite field on the foreign row) + the ServerSettings federation toggle (enabled for any visibility). |
| §9.4 threads (name / list) | `THREAD NAME`/`THREADS` handlers in `weft-core/src/session/relay.rs` (`on_thread_name` gates via `can_post` + `find_root`; `on_threads` = `BATCH` of `THREAD`). Store: `EventStore::channel_threads` (aggregates the existing `thread` column) + `set_thread_name` over `weft_thread_names` (migration `0031`). A thread name is metadata keyed by the root msgid — **no** new identity; threads stay "views, not channels". Client: `openThreads`/`renameThread`/`threadNames` in `+page.svelte`, `ThreadsModal.svelte` (list) + `ThreadPanel.svelte` (inline-editable title) |
| Link-preview / unfurl proxy | `weftd/src/unfurl.rs` — `GET /unfurl` (meta JSON) + `GET /unfurl/image` (image bytes), mounted in `lib.rs` (gated on `[unfurl] enabled`). **All fetches are SSRF-guarded** via `dialer::is_dialable` in `resolve_and_guard` (every resolved IP, every redirect hop) — the invariant-13 template is `dialer::fetch_signing_key`. Auth = the `/media` session bearer (`ctx.media_bearer_account`). Meta extraction (`parse_meta`) is a pure, panic-free, tested parser — no HTML-parser dep. Client: `weft.ts unfurl()`/`unfurlImageUrl()` + `LinkPreview.svelte` (rendered per-message in `MessageItem.svelte` for the first http(s) link) |
| CORS on the HTTP data plane | `weftd/src/cors.rs` — one permissive `from_fn` layer (`ACAO: *`, answers `OPTIONS` preflight) on the `/media` and `/unfurl` routers. Safe because those endpoints auth by query-string bearer, not cookies. **Without it, cross-origin uploads (custom `Content-Type`) fail preflight → the client sees `TypeError: Load failed`** (the avatar-upload bug) |
| New ERR code semantics | `weft-proto/src/errcode.rs`, then the `send_err` call sites in session.rs |
| Wire grammar/limits | `weft-proto/src/line.rs` (consts at the top) |
| Session states / idle limits | session.rs consts + `State` enum at the top |
| What gets stored / compaction semantics | `weft-store/src/materialize.rs` (never per-backend!) |
| Storage backend | implement the two traits in `weft-store/src/traits.rs`; `memory.rs` is the reference semantics |
| Channel behavior (ordering, fan-out) | `weft-core/src/channel.rs` |
| Config options | `weftd/src/config.rs` (serde) + `lib.rs :: start` wiring |
| Timeouts/keepalive | transport idle: `weft-transport/src/quic.rs :: transport_config`; app liveness: session.rs consts; client PING: `weft-tui/src/net.rs` |
| Load / throughput testing | `weftd/src/bin/loadtest.rs` — an in-process generator that drives the real session→actor→store→broadcast pipeline (no QUIC) via in-memory `ControlStream`s. `cargo run --release -p weftd --bin loadtest -- --channels 16 --senders-per-channel 1 --messages 20000`. Reports ingest events/s, fan-out deliveries/s, ack-latency percentiles. Per-channel ceiling ≈ single-writer actor rate; aggregate scales with channel count. Use 1 sender/channel for a clean ingest number (multi-sender/channel is a fan-out-contention stress test where broadcast lag drops copies — realistic §9.2 backpressure) |

## Test map — which suite proves what

| Suite | Command | Proves |
|---|---|---|
| Proto round-trips | `cargo test -p weft-proto` | every wire form parse↔serialize |
| Crypto | `cargo test -p weft-crypto` | sign/verify, replay rejection, expiry |
| Store + materialization | `cargo test -p weft-store` | §12.1 invariants, paging, purge watermark |
| Core (networkless) | `cargo test -p weft-core` | the whole domain over a mock `ControlStream` — FSM, auth, relay, mutations, HISTORY |
| Conformance (black-box) | `cargo test -p weftd` | real QUIC + WS against an in-process server |
| Slow idle regression | `cargo test -p weftd --test conformance -- --ignored` | keepalive survives long quiet gaps |

The layering is the debugging strategy: a failing conformance test with
green core tests means transport/glue; failing core with green proto means
session/actor logic; and so on down.

## Reading order for a newcomer

1. `docs/weft-protocol-spec.md` §3–§9 (client-side sections) — 20 minutes.
2. `weft-proto/src/lib.rs` doc comment, then skim `line.rs`.
3. `weft-core/src/session.rs` top-of-file comment + `Session::run` +
   `on_request` — the FSM shape.
4. `weft-core/src/channel.rs` — the actor; now you know the write path.
5. `weft-store/src/materialize.rs` — read the tests before the code.
6. Everything else on demand via the chains above.

## M3b addendum — new files, new chains

New load-bearing files:
- `weft-core/src/directory.rs` — the account→sessions actor: DM delivery
  and MARK sync. Sessions register in `welcome_authed`, deregister in
  `cleanup`; events arrive via the session's 4th select arm (`on_direct`).
- `weft-core/src/maintenance.rs` — the purge/compaction loop weftd spawns.
- `weft-store/src/compact.rs` — `compaction_plan`, the §12.1 audit-window
  pure function (read its tests first, like materialize).
- `weft-store/src/postgres.rs` + `migrations/` — the sqlx backend. It
  contains **no semantics**: materialize/compaction_plan stay shared, and
  `tests/backends.rs` runs one contract suite against both backends.

Chain 7: a DM — `on_msg(Target::User)` → `Directory::dm` (existence check,
mint, persist, fan out to every session of both accounts) → each session's
`on_direct` (same origin/label echo rule as channels, separate
`pending_direct` FIFO).

Chain 8: boot with Postgres — `weftd::start` → backend match →
`boot()` helper: **upsert config channels → `list_channels()` → registry**
(the store, not the config, is the source of truth) → `spawn_maintenance`.

| Change | Touch |
|---|---|
| Storage schema | new file in `weft-store/migrations/` (never edit applied ones) + both backends + `tests/backends.rs` |
| Compaction semantics | `weft-store/src/compact.rs` only |
| DM behavior | `directory.rs` + session `on_direct`/`on_msg` |
| Verification kinds/flows | store substrate exists (`Verification`); wire flow = spec decision first |

## M-prof addendum — §10.3 display profiles (nick + avatar)

A profile = display name + avatar (the avatar's BLAKE3 hash → a `weft-media://`
blob) + two local-only free-text fields, **bio** (`@about=`, ≤512 B) and
**custom status** (`@status=`, ≤128 B). Both ride the `PROFILE` line unsigned and
are stripped at the federation boundary (the `..` in `federation.rs`'s
`Event::Profile` destructure); each follows the same present-sets/absent-leaves
partial-update rule as display/avatar. Custom status shows inline in the member
list (`MemberList.svelte` `.mstatus`) and on the profile card/modal; set it via
the shortcut modal layered over the user footer (`UserFooter.svelte` +
`app.setCustomStatus`/`statusOf`). New load-bearing pieces:
- `weft-crypto/src/profile.rs` — `SignedProfile` (home-network-key-signed CBOR,
  avatar-hash-bound; models `manifest.rs`). Used at federation (M-prof-5).
- `weft-store` — `ProfileStore` + `ProfileRecord` (`kind`-less per-account row),
  migration 0022; `avatar_exists` powers the fetch gate + GC skip.
- `weft-core/src/session/profile.rs` — `on_profile_set` (partial update →
  `ctx.profiles.set_profile` → labeled ack + `announce_as` to co-members) and
  `on_profiles_query`. `ctx.profiles` is the port; `ServerCtx::may_fetch` lets any
  authed session fetch an avatar blob (§10.3 semi-public); `maintenance ::
  gc_orphan_blobs` skips avatar hashes so avatars aren't GC'd.

| Change | Touch |
|---|---|
| Profile wire form | `weft-proto` command.rs (`PROFILE SET`/`PROFILES`) + event.rs (`PROFILE`) **+ round-trip test first** |
| Profile storage | `weft-store` `ProfileStore`/`ProfileRecord` (mem + PG + migration + contract) |
| Profile authz/broadcast | `weft-core/src/session/profile.rs`; avatar fetch gate in `context.rs :: may_fetch`; GC skip in `maintenance.rs` |
| Profile federation | send: `session/federation.rs :: on_bridge_event` (signs + forwards `PROFILE sig=…`); receive: `on_bridge_line` routes `PROFILE`→`ingest_bridged`→`ingest_profile` (verify vs peer key + mirror avatar). `SignedProfile` in weft-crypto; `Event::Profile` carries a `UserRef` |
| Avatar rendering (client) | `Avatar.svelte` (image-or-initials, uses `app.avatarUrl`); `+page.svelte` `profiles` store + `avatarUrl`/`displayName`/`queryProfile`; edit in `UserSettingsModal` (`weft.profileSet` + `upload()`); wrappers in `weft.ts` (`profileSet`/`profilesQuery`/`avatarUrl`) |

## M-voice addendum — §16 WEFT-RT voice signaling (M-voice-0/1a)

Voice is a **projection over the same session/actor machinery**, not a new
server. The media plane (an SFU) is separate — see below.

New load-bearing files:
- `weft-proto/src/command.rs` + `event.rs` — the `VOICE JOIN/LEAVE/DESC/CAND`
  verbs and `VOICE OFFER`/`VOICE STATE`/`VOICE DESC`/`VOICE CAND` events. `DESC`
  is symmetric (command = client offer, event = SFU answer); raw SDP rides the
  trailing (CR/LF auto-escaped, same as a message body — no base64).
- `weft-core/src/voice.rs` — the **`VoiceBackend` port** (the pluggable-SFU
  seam): `Arc<dyn VoiceBackend>` (async-trait) with `join`/`describe`/
  `candidate`/`leave`. Held as an optional `OnceLock` on `ServerCtx`; weftd
  installs one via `set_voice_backend` (like the mirror/backfill sink ports).
  `None` = zero-voice server → voice verbs answer `UNSUPPORTED`.
- `weft-core/src/session/voice.rs` — the handlers (`Session::on_voice_*`).

**Voice channels are a distinct kind** (`ChannelKind` in weft-proto; a `kind`
column, migration 0021; `ChannelRecord.kind`). Voice channels are **voice-only**:
`relay.rs :: join_one` rejects a text JOIN to a `Voice` channel (→ NO-SUCH-TARGET,
which is also the IRC-invisibility guarantee — no weft-irc code). Kind is set at
`CHANNEL CREATE #chan voice` / `[[channels]]` config and advertised in
`CHANNEL-LAYOUT` (`kind=voice`).

Chain 9: a voice join — `session.rs` dispatch → `voice::on_voice_join`:
`registry.get` + `channel_kind == Voice` (else NO-SUCH-TARGET) → M7 `is_moderated`
ban/mute → `voice_caps` (`listen`/`speak` on a restricted channel) — **all
authority before the backend** (invariant 4) — → `ctx.voice_backend().join()` →
**`handle.subscribe()` + `spawn_forwarder`** (a voice channel isn't text-joined,
so the session *subscribes* to the broadcast for `VOICE STATE`, tracked in
`self.voice: HashMap<ChannelName, VoiceRoom>`) → `VOICE OFFER` (labeled ack) →
`announce_voice_state` → `ChannelHandle::announce_as(self.id, …)` (the actor's own
copy is skipped, the `Cmd::SetPolicy` pattern). `VOICE DESC` relays the SDP to the
backend and returns its answer. Disconnect: `cleanup` → `teardown_voice` per room
(aborts the forwarder + SFU-leaves).

The **SFU media engine is not here** — `weft-core` never touches a socket. The
`WebrtcSfu` (webrtc-rs) implementing `VoiceBackend` lives in the `weft-rt` crate
(below) and owns the UDP/DTLS/ICE; `on_voice_*` only carry SDP/ICE to it.

The media plane — `weft-rt` (M-voice-1b), a **`members`-but-not-`default-members`**
crate (webrtc 0.17.1; only built with weftd's `voice` feature):
- `weft-rt/src/sfu.rs` — `WebrtcSfu` (the reference `VoiceBackend`). One shared
  `webrtc::API` (MediaEngine+Opus, pinned UDP range); a `rooms:
  Mutex<HashMap<ChannelName, Room>>`, each `Room` = per-session PeerConnections +
  per-session `TrackLocalStaticRTP` publishers. `join` sets `on_track` → mirror
  inbound Opus into a local track + pump RTP to it (webrtc rewrites SSRC/PT per
  subscriber binding = verbatim fan-out). `describe` = **`add_track` the existing
  publishers BEFORE `set_remote_description`** (the ordering that binds the
  sender — the reverse leaves it paused and forwards zero bytes) → non-trickle
  gather+answer. Tests (`weft-rt/tests/sfu.rs`) drive real webrtc client PCs over
  loopback (host ICE, no STUN); one asserts a gathered answer, one asserts Opus
  actually forwards publisher→subscriber.

weftd wiring (M-voice-1c): the `voice` Cargo feature gates the optional
`weft-rt` dep (default build pulls no webrtc). `weftd/src/lib.rs ::
build_voice_sfu` (two `#[cfg]` arms) constructs the SFU from `[voice]` config
(`weftd/src/config.rs :: Voice`); `start` advertises `features=voice` iff it came
up, then `ctx.set_voice_backend` installs it. Conformance:
`tests/conformance/main.rs` — `voice_disabled_by_default_is_unsupported` (always)
+ `voice_enabled_signaling_over_quic` (`#[cfg(feature = "voice")]`, run with
`--features voice`).

| Change | Touch |
|---|---|
| Channel kind (text/voice) | `weft-proto :: ChannelKind` + `CHANNEL CREATE`/`CHANNEL-LAYOUT`; store `kind` column (new migration) + `ChannelRecord`; the `join_one` reject + `on_voice_join` gate in weft-core |
| Voice signaling authz | `weft-core/src/session/voice.rs` (never the SFU) |
| Voice roster / snapshot / live-mute | `ServerCtx.voice_rooms` + `voice_room_join`/`leave`/`voice_set_muted` (context.rs); snapshot in `on_voice_join`; `mute_in_voice` (voice.rs) called from `on_moderate`; SFU drop = `WebrtcSfu::set_muted` (per-publisher `AtomicBool`) |
| The SFU seam / a new backend | implement `VoiceBackend` (`weft-core/src/voice.rs`); native default in `weft-rt` |
| LiveKit voice backend (M-lk-0) | `weft_core::LiveKitBackend` (voice.rs) mints via the `LiveKitAdmin` port; weftd's `LiveKitSigner` (`weftd/src/livekit.rs`) uses `livekit-api`'s `AccessToken`/`VideoGrants`; selected by `[voice] backend="livekit"` in `build_voice_backend` (weftd/lib.rs); `VOICE OFFER` `mode`/`room` carry it |
| LiveKit client (M-lk-1) | `client/src/lib/voice.svelte.ts` branches on `mode`: `onLiveKitOffer` dynamically imports `livekit-client`, connects a `Room`, mirrors roster/active-speaker/mute from Room events; `onWebrtcOffer` = the old SFU path. Same `voice` `$state` + `VoiceBar.svelte` for both |
| LiveKit moderation (M-lk-2) | `LiveKitAdmin` async `set_participant_muted`/`remove_participant` (voice.rs); `LiveKitBackend` session→(room,identity) map routes `set_muted`/`leave`; ban/kick → `eject_channel_voice` (session/voice.rs) + `ctx.voice_eject_account`; weftd `LiveKitSigner` impl = `RoomClient.update_participant`/`remove_participant` |
| Federated voice foundation (M-lk-3a) | Manifest `voice`-mode = a `voice: bool` mirroring `typing` (crypto `manifest.rs`, proto `Event::Manifest`/`BRIDGE PROPOSE`, core `bridge::build_manifest`); crypto `SignedVoiceRelayGrant` (`weft-crypto/src/voice.rs`); `VOICE REQUEST`/`VOICE GRANT` verbs; gating in `on_voice_request_in` (session/federation.rs) using `bridge::is_forwardable` + manifest voice flag + `VoiceBackend::relay_grant` |
| Federated voice relay lifecycle (M-lk-3b) | `VoiceRelay` trait + `RelaySpec` (weft-core `voice.rs`); `ServerCtx.voice_relays` refcount + `relay_acquire`/`relay_release`/`relay_drop_peer` (context.rs); `SEVER`/`NETBLOCK` teardown in `on_bridge_sever_in`/`on_netblock_add`; weftd no-op `LogRelay` (`weftd/src/livekit.rs`). **Real libwebrtc media driver = deferred deployment dep** |
| Account verification (§10.5) | `VERIFY EMAIL/CONFIRM/BIRTHDAY/LIST` handlers in `weft-core/src/session/verify.rs`; `Mailer` port (`weft-core/src/mailer.rs`); code store + `verify_send_code`/`verify_check_code` in context.rs; claims via `Accounts` → `AccountStore.upsert/confirm_verification`; weftd `SmtpMailer`/`LogMailer` (`weftd/src/mailer.rs`, `lettre` + `[smtp]` config); client `verify*` in weft.ts |
| Voice wire form | `weft-proto` command.rs/event.rs **+ round-trip test first** |
| Voice config / enabling | `weftd/src/config.rs :: Voice` + `lib.rs :: build_voice_sfu`; the `voice` feature in `weftd/Cargo.toml` |
| The SFU media engine (forwarding, codecs, ICE) | `weft-rt/src/sfu.rs` — run its tests with `cargo test -p weft-rt` |
| Web voice UI / browser WebRTC | `client/src/lib/voice.svelte.ts` (the `$state` controller: getUserMedia + RTCPeerConnection + the JOIN→OFFER→DESC handshake) + `components/VoiceBar.svelte`; wired in `routes/+page.svelte` (`initVoice` on connect, `<VoiceBar>` in the members aside) |
| Web voice wire glue | `weft-client-core/src/lib.rs` (`ClientEvent::Voice*` + `build_voice_*`) + `weft-client-wasm/src/lib.rs` dispatch + `client/src/lib/weft.ts` (`WeftEvent` union + `voice*` wrappers) |
| Desktop voice (Tauri) | webview WebRTC — reuses `voice.svelte.ts`; `client/src-tauri/src/lib.rs` `voice_*` commands + `grant_media_permission` (`with_webview`, Linux WebKitGTK) + `Info.plist` mic string. Audio quality knobs (AEC/NS/AGC + Opus FEC/DTX) in `voice.svelte.ts` |

## WEFT Console addendum — `weft-admin` (the operator web panel)

Operator-only web admin (`docs/web-admin-panel-plan.md`). An axum router +
embedded SPA over the store roles — never speaks the wire protocol. weftd mounts
it on the HTTP listener (`[admin] enabled`), sharing the in-process stores +
live registry.

| Change | Touch |
|---|---|
| A new admin endpoint | `weft-admin/src/handlers.rs :: routes()` (all under `/admin/api/v1/*`) + a handler fn; reads go straight to a store role on `AdminState`, live actions via the `Live` port. Responses are typed `#[derive(Serialize)]` structs in `weft-admin/src/dto.rs` (add one + a `From<StoreRecord>`), never ad-hoc `json!` |
| Admin auth / session cookie | `weft-admin/src/auth.rs` (HMAC over `account\|exp`; `require_admin` middleware authenticates + injects the acting `Account` **and** its `AdminScopes`) |
| Admin RBAC (WC2) | `auth::AdminScope` (`admin.read/moderate/destroy/federation/keys`) + `admin_scopes()` (operators→all; else `admin`-scope capability grants by ULID, `*`/`admin.*`→all). Middleware enforces the `admin.read` baseline; each write handler calls `require(&scopes, AdminScope::…)` → 403. Delegate an admin via `GRANT admin admin.moderate <account>`. `/me` returns held scopes; the SPA hides controls via `can()` |
| Account soft-delete (WC3) | `AccountStore::schedule_deletion`/`cancel_deletion`/`deletion_scheduled`/`due_deletions` (migration `0024`, `purge_at_ms`); `DELETE /accounts/:name?confirm=<name>` schedules (typed-name), `POST /accounts/:name/restore` cancels; finalized by `weft_core::maintenance::purge_due_deletions` (in the maintenance loop). Grace = `AdminState.delete_grace_ms` from `[admin] delete_grace_days`. SPA danger-zone in `openUser` |
| Lookup depth (WC4) | User detail adds devices (`AccountStore::devices` → `device_fingerprint`), a flags card, and "find related" (`AccountStore::accounts_by_email_domain` → `account_detail.related`). Channel detail = `GET /channels/:name/detail` (policy + `MembershipStore::members`), SPA `openChannel`. DM-thread browse = `GET /dms/:a/:b/messages` (`browse_dm`, `Scope::dm`); e2ee gate via `AdminState.dm_policy` → `dto::ThreadBrowse.unavailable`, SPA `browseDm`. Deferred: IP-pivot, join-path, media footprint, per-peer replication |
| Federation ops (WC5) | Peer detail = `GET /peers/:name/detail` (`peer_detail`): parses `weft_crypto::SignedManifest::from_b64` → pinned key `fingerprint_hex` + `verified`, shared channels, history/media/typing/voice; + `is_netblocked`. SPA `openPeer`. Sever/re-weave reuse the NETBLOCK endpoints (a netblock *is* the §11.6 sever). Deferred: RTT/handshake, transit queue, force-re-handshake, key-rotation TOFU review |
| Trust & keys (WC6) | Token inspector = `POST /tokens/inspect` (`inspect_tokens`): `weft_crypto::Token::from_b64` per link → issuer/subject/scope/caps/epoch/expiry + `expired`/`rooted`/`parent_linked`/`revoked` (vs `scope_epoch`); SPA `inspectTokens`. Revocations = `GET`/`POST /revocations` (`scope_epoch` + `bump_epoch`, audited); SPA `revocations` screen. Both `admin.keys`. Deferred (E2EE): device registry, MLS leaves, propagation status |
| Account suspend (WC7) | `AccountStore::set_suspended`/`is_suspended` (migration `0025`). **Enforced at `weft-core session/auth.rs :: welcome_authed`** (the single AUTH chokepoint) → uniform AUTH-FAILED; also blocks the admin panel login. Admin `POST /accounts/:name/suspend`\|`/unsuspend` (`admin.moderate`, audited, no-self-suspend); `Accounts::set_suspended`/`is_suspended` passthroughs. SPA "Account moderation" card in `openUser`. Wire test: conformance `suspended_account_cannot_authenticate`. Deferred: forced live-session logout, shadow-limit, room actions |
| A live action (kick/eject, delete-any) | the `Live` trait (`weft-admin/src/lib.rs`); weftd's adapter = `LiveRegistry` (`weftd/src/lib.rs`) over the channel registry |
| Web admin action parity (roles, moderation) | Moderation (mute/ban/kick) = `POST /api/v1/moderation` (`moderate` handler, `admin.moderate`, `ModerationStore` + `Live::eject` for kick/channel-ban). **Role assignment** = `POST /api/v1/namespaces/:name/roles` (`assign_ns_role`, `admin.moderate`) — store-direct parity with the client's `ROLE ASSIGN`/`UNASSIGN`: `roles.assign_role`/`unassign_role` **plus** `caps.record_grant`/`revoke_grants` of the role's caps (enforcement reads grant records, keyed by the member's ULID via `account_ulid`), including same-named channel-role caps (`channel_role_caps` helper mirrors `Session::channel_role_caps`). `namespace_detail`'s `members` now carry each member's roles (`dto::NamespaceMember`). SPA (`ui/index.html`): Members tab has an "Assign a role" card + per-member role chips with a ✕ (`assignRole`/`unassignRole` click handlers → the endpoint). Test: `admin_assigns_and_unassigns_a_namespace_role` |
| Web admin reports screen (actions + media/emoji) | `report_detail` (`ReportDetail`) now returns each message's `attachments` (media hashes, from `meta.attachments`), the scope namespace's custom `emoji` map (name→hash, via `EmojiStore` newly on `AdminState`), and the reported message's `author`. SPA `msgCard` renders attachments as `<img src="/media/<hash>">` (with an on-error download-link fallback), `:name:` in bodies + reaction emoji as `<img class="emoji">` via the map (`renderBody`/`renderReact`), and `openReport` adds an "Act on this report" card: delete message + mute/ban/kick the author (`modReport` handler → `/moderation`). Requires `/media` reachable from the panel's origin |
| Web admin list search + dedicated port | Every list screen has a `searchBar()` → global `filterList()` (client-side text filter over `tbody tr` + `.rowlist > div`). Dedicated admin listener: `[listen] admin` (`config::Listen.admin`) — when set, `weftd::start` serves the admin router + `/media` on its own port (`spawn_http` helper; `Server.admin_addr`) instead of merging into the shared http/https app, so it can be firewalled off the public surface |
| Operator authority scope (network-only, not per-namespace) | `ServerCtx::actor_has_cap` in `weft-core/src/context.rs`: the operator short-circuit fires only when `scope_namespace(scope).is_none()` (i.e. `*` or a top-level channel), never inside a user namespace (`ns:`/`#ns/chan`). Owner short-circuit (owner of the ns) is unchanged. Cross-namespace operator power is web-admin-only (store-direct); recovery rung 3 is network-key-signed. Client surfaces operators as a **Staff** badge (`app.isStaff` = caps at `*`), never owner; owner shows only in Server Settings (`ownerAccount === m.account`). Tests: `owner_cannot_leave_their_namespace`, `ns_scope_mute_covers_a_namespaced_channel` (owner-moderates). **NS LEAVE** wired client-side (`build_ns_leave` → `ns_leave` → `weft.ts nsLeave` → SidebarHeader "Leave Server", hidden for the owner; `on_ns_leave` rejects the owner). Role-colored names via `app.nameColor` |
| The SPA | `weft-admin/ui/index.html` (`include_str!`; single `const API = "/admin/api/v1"` fetch base). Design target: `design/admin/` (`weft.css` + templates) |
| Audit trail (WC1) | `AuditStore` (`weft-store/src/traits.rs`) + `AuditEntry`/`AuditRecord`/`audit_hash` (shared pure blake3 chain, `types.rs`), mem + PG (advisory-lock append) + migration `0023_audit`; every write handler emits via `handlers.rs :: audit()` (payload digested, never raw); `GET /admin/api/v1/audit`. Contract: `backends.rs` audit block; e2e: `weft-admin/tests/api.rs :: write_actions_land_in_the_audit_log` |
| A new store role on the panel | add the `Arc<dyn …>` field to `AdminState` + its `from_store` bound (`weft-admin/src/lib.rs`), and to weftd's generic store bound in `run`/`serve` (`weftd/src/lib.rs`) |
| Cutting a live session (forced logout) | each `Session` owns a `close: CancellationToken` registered with the account directory (`weft-core/src/directory.rs`, `SessionEntry`); `ServerCtx::disconnect_account` cancels every token for an account. Sessions exit via the normal `cleanup`, so a cut looks like an ordinary disconnect (presence offline + voice leave, membership retained) — not a `MEMBER part` |
| Who may do what in the panel (WC2) | scopes are `auth::AdminScope`; a request's set comes from `auth::admin_scopes` (operator ⇒ all, else the `admin`-scope grant keyed by account ULID). Write handlers gate with `require(&scopes, …)`. **Changing** permissions is gated by `auth::is_operator` instead — a delegated `admin.*` grant holds every scope, so a scope gate would allow self-promotion |

## M-social-fed addendum — cross-network friends, group DMs, calls

The line-133 "cross-network deferred" note is superseded: the social layer now federates end-to-end. Spec: §6.8 (social commands), §11.12 (the group tunnel). Flow docs: `docs/weft-federation-flows.md` (§9 social), `docs/weft-protocol-flows.md` (§13).

| I want to change… | Where |
|---|---|
| The federation conduit (FriendDeliver) | `ServerCtx::request_friend_deliver(FriendDeliver{peer, from, line})` (`context.rs`) → weftd dialer opens `FSESSION OPEN <from>` + `CMD :<line>` on the peer bridge; receiver reconstructs `from@<sender-net>` and dispatches via `session/federation.rs :: on_federated`. Fire-and-forget (no reply routing). |
| Home-authoritative group ordering | `session/groups.rs :: group_home(group)` = creator's network = sole ULID writer (§9.1). Same-net → `directory.group_msg`; cross-net home → `group_mint` + `fanout_group_message`; spoke → `GroupRelay{msgid:None}` relayed to home. |
| Group message federation | `on_group_relay` (`groups.rs`): `@id` absent = spoke→home mint+fanout; present = home→member `group_ingest(origin,…)`. Membership sync = `GroupSync`/`on_group_sync` (reconciles diff, parts removed locals) via `propagate_group`. |
| Group edit/delete/react federation | `GroupMut`/`apply_group_mutation`/`relay_group_mut`/`on_group_mut` (`groups.rs`); `GroupMutKind` in `directory.rs`. |
| Group backfill (node-down recovery) | `GroupBackfill` verb. Spoke: `on_history` `Target::Group` → `request_group_backfill` (cursor = latest local msgid) → tunnel to home. Home: `on_group_backfill` → `roots(after:cursor)` → replay as `GroupRelay{msgid:Some}` ingests (idempotent; non-members get nothing). |
| Spoke-poster labelled echo | `ctx.group_echoes: Mutex<HashMap<token → (session, created_ms)>>` (`context.rs`); `register_group_echo` (TTL-swept, `GROUP_ECHO_TTL_MS`) / `take_group_echo`. Home echoes `@echo=` only to the sender's network; spoke delivers via `group_ingest(origin=session)` so the pending label attaches. |
| Cross-network calls (1:1 + group) | `Call`/`GroupCall` signaling in `session.rs`/`federation.rs`; media = LiveKit cascade relay (`voice.rs :: VoiceRelay`, `RelaySpec{peer, key, remote/local url·room·token}`, `relay_acquire/release/drop_peer`). Group call mesh: `GroupCallRoster`/`broadcast_roster`/`on_group_call_roster`. |
| Group attachment mirroring | `ServerCtx::mirror_group_attachments(group, meta, msgid)` (`federation.rs`) → `MirrorRequest` from the blob's origin network (§11.8). |

Tests: `weft-core/tests/session.rs` — `cross_network_group_message_*`, `cross_network_group_edit_*`, `cross_network_group_mutation_*`, `cross_network_group_membership_changes_propagate`, `federated_group_sync_*`, `spoke_poster_gets_a_labelled_echo`, `spoke_requests_group_backfill_on_history`, `home_serves_group_backfill_replaying_missed_messages`, `group_call_*`, `federated_group_call_*`.


## Routing addendum — path-based SvelteKit routes (client)

The Tauri/Svelte client used to be one ~4600-line `+page.svelte` God component with
in-`$state` view switching. It is now a SvelteKit SPA with real routes:

- **`src/routes/+layout.svelte`** — the container. Holds the connection, the `handle(e)`
  event reducer, all domain/UI `$state`, `provideApp(ctx)` (the `AppCtx` seam every
  component reads via `getApp()`), and the persistent chrome (`CommunityRail`, sidebar,
  members aside) + every modal overlay. Renders `{@render children()}` inside `<main>` when
  `status === "online"` (else the `ConnectScreen` gate).
- **`src/lib/nav.ts`** — pure `pathFor(active, activeServer)` / `viewFrom(routeId, params)`
  codec. The single source of truth for "what's open" is the URL; `active`/`activeServer`/
  `homeView` are `$derived` from `page` (`$app/state`). Nothing assigns them — nav is
  `goto(nav.pathFor(...))`.
- **Route pages** (thin, read `getApp()`): `+page.svelte`→`FriendsView` (`/`);
  `c/[server]/+page.svelte`→`EmptyHome`; `c/[server]/[channel]`, `dm/[peer]`, `g/[group]`
  → `components/chat/ChatView.svelte` (voice → `VoiceStage`, else topbar + `MessageList` +
  `Composer`). Top-level channels route as `/c/~/[channel]`.
- **No keep-alive** — the channel route `{#key active}` remounts `MessageList` per channel
  (cheap: history + roster stay cached in the `Channel` record). A deep-linked/reloaded
  channel is masked by the connect gate until sync lands.