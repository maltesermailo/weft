# Refactor Phase 1 — Inventory & Domain Map (READ-ONLY)

Scope: the **Tauri client app** only — `client/src/**` (115 TS/Svelte files, ~19.5k lines)
and `client/src-tauri/src/**` (7 Rust files, ~2.5k lines). This is what "UI/Business Split
(Tauri)" + "migrate business logic to Rust" targets. The weftd *server* workspace is out of
scope.

Method: seven parallel readers walked every file against one fixed 20-domain vocabulary and a
4-layer scheme (`ui` / `business` / `glue` / `mixed`), so the sub-tables reconcile without
naming drift. No code was changed.

Layer definitions used:
- **ui** — rendering, component/form state, formatting for display, local event handling.
- **business** — rules, validation, persistence, IO, protocol, sends a WEFT verb, media/WebRTC,
  mutates domain state.
- **glue** — `invoke()`/`listen()`/`app.emit()` IPC wiring, the Svelte context bridge.
- **mixed** — business + ui in one body.

---

## Headline findings

1. **The Rust backend is already ~pure glue.** ~110 of ~123 `#[tauri::command]`s in `lib.rs`
   are a one-line `conn.send(build_verb(...))` passthrough with no emit and only `Conn` state.
   The TS `weft.ts` wrappers mirror them 1:1. So "move verb-sending to Rust" is a no-op — it's
   *already* in Rust as glue. The genuinely migratable TS business logic is elsewhere (see #2).

2. **The real TS business logic clusters in five places**, not in the verb wrappers:
   - the **inbound reducer** (`sync/reducer.svelte.ts` + the per-domain `*Handlers` maps) —
     event → store mutation;
   - **persistence** to `localStorage` (layout cache, DM cache, notif prefs, SYNC cursor, theme,
     email-nudge, ns-category cache);
   - **media IO** (`upload`/`unfurl`/`mediaUrl`/`avatarUrl`/`pullBackfill`) — HTTP `fetch` from TS;
   - **rendering** (markdown/highlight/emoji/time) — deterministic, but feeds `innerHTML`;
   - **voice** — already largely native on desktop; the web/LiveKit-JS path is browser-bound.

3. **Two domains straddle a real boundary** (the only classification splits worth a decision):
   - **calls** (1:1 + group) are filed under `voice` on the TS side but `social` on the Rust side.
   - **media** (upload/unfurl/mediaUrl/avatar/dims) is filed under `messages` but is arguably its
     own domain and the strongest Rust-migration candidate.

4. **Business logic still hiding inside view components** (leftover from the God-component era):
   `AppShell` (startDm/doJoin/joinNamespace/email-nudge), `MessageList` (the whole
   virtualization/paging/anchor state machine), `MemberList` (Discord hoisted-role grouping),
   and a long tail of modal handlers that send verbs directly. Several are in **inline template
   handlers** with no named function (`ProfileCard`: moderate/setNick/assignRole; `ServerSettings`:
   recovery/federation/bans; `PinsModal`/`ReportsQueueModal`: unpin/resolve). These matter for
   Phase 3 step 4 (split).

---

## Domain catalog (20 domains)

No domain has < 3 methods, so there are **no merge candidates on the count criterion**. Counts
are approximate (±, from the tables below). "Layer skew" notes where a domain is dominated by one
layer.

| Domain | ~Methods | One-line responsibility | Primary files | Layer skew |
|---|---|---|---|---|
| **messages** | ~100 | Compose/send/edit/react/history/pins/threads + media refs + the message-list state machine | composer.svelte.ts, threads.svelte, channel.svelte (msg parts), weft.ts, chat/MessageList | business-heavy; big ui tail in components |
| **voice** | ~130 | Voice + screenshare + camera media plane (native + LiveKit-JS + WebRTC), 1:1/group call media | voice.svelte.ts, callmedia.svelte.ts, voice_native.rs, screencap.rs, voice pickers | business/glue; the media-plane monolith |
| **roles** | ~95 | Role defs + per-scope capability grants/revokes + role assignment + reorder | session.svelte (roles), chanperms.ts, RolesTab, ChannelSettings, weft.ts | almost all business |
| **namespaces** | ~75 | Server/ns model, NS-META, visibility, recovery ladder, custom emoji, discover | server.svelte, weft.ts, keys.rs (ns keys), DiscoverModal, ServerSettingsModal | business |
| **channels** | ~65 | Channel records, layout/categories, create/rename/policy/delete, drag-order | channel.svelte, channelcreate.svelte.ts, channel-handlers.ts, ChannelSettings | business |
| **profile** | ~65 | Display names, nicknames, avatars, custom status, §10.5 verification | profile.svelte.ts, account.svelte, weft.ts, UserSettingsModal | business + ui getters |
| **social** | ~65 | Friends + group DMs (federation-able) | social.svelte, weft.ts, FriendsView, NewGroupModal | business |
| **session** | ~47 | Identity/auth/device-keys/caps-held/operator gates/login lifecycle | session.svelte, connection.svelte.ts, keys.rs, config.rs, lib.rs | business |
| **ui** | ~42 | Modals/menus/theme/toasts/confirm/lightbox/link-guard/context-bridge/drag | ui.svelte.ts, ctxmenu.svelte.ts, confirm.svelte.ts, context.ts | all ui |
| **navigation** | ~40 | URL↔view mapping, routing, target selection, rail/switcher | navigation.ts, nav.ts, view.svelte, viewmodel.svelte.ts | business (nav) + ui (derived) |
| **federation** | ~34 | Bridges, netblocks, on-demand federate, manifests | federation.svelte, weft.ts, FederationPanel, DiscoverModal | business |
| **invites** | ~31 | Invite mint/redeem/revoke/list + link building + DM delivery | invites.svelte, weft.ts, InviteCreateModal | business |
| **rendering** | ~26 | Markdown/code-highlight/emoji-shortcode/time formatting | markdown.ts, highlight.ts, mdhighlight.ts, time.ts, shortcodes.ts | business (pure) + ui |
| **moderation** | ~25 | Mute/ban/kick deny-list + reports queue | moderation.ts, reports.svelte, weft.ts, ProfileCard, ReportModal | business |
| **membership** | ~21 | NS membership records + roster fetch/apply | server.svelte (members), membership.svelte, weft.ts | business + ui getters |
| **connection** | ~20 | Transport lifecycle: connect/probe/reconnect/dial, the Rust connection loop | connection.svelte.ts, weft.ts, weft.rs, lib.rs | business + glue |
| **notifications** | ~16 | Notification prefs + toasts + desktop-notify | notif.ts, toasts.svelte.ts, weft.ts | business + ui |
| **sync** | ~13 | Inbound event dispatch + streamed-row collector + SYNC cursor | reducer.svelte.ts, collector.ts, connection.svelte.ts | business |
| **store** | ~8 | Root reactive container: intern accounts/servers, notif lookup | store.svelte.ts | business |
| **transport** | ~8 | Raw wire-line send + IPC boundary + WASM/QUIC connect | weft.ts (invoke/onWeft), weft.rs, lib.rs (send_raw) | glue |

Layer distribution across the whole app (approx): **business ~45%**, **ui ~38%**, **glue ~12%**,
**mixed ~5%**. Business concentrates in the `.svelte.ts` models/orchestrators; ui concentrates in
`components/**` and getter objects; glue is `weft.ts` + the Rust commands + `$effect` prefetchers.

---

## Boundary decisions I need from you (the "ambiguous" list)

These are the only classifications where I refused to guess. None block writing the inventory;
they affect how Phase 2 draws module lines.

1. **`calls` — its own domain, or fold into voice/social?** Today: TS files call logic under
   `voice` (media) + `social` (ring/state), Rust under `social`. 1:1 + group calls touch signaling
   (social), media (voice/LiveKit), and presence. **My recommendation:** keep `calls` as a distinct
   domain (it has ~20 methods and a clean verb set: CALL/CALL-ACCEPT/DECLINE/END + GROUP-CALL). That
   pulls call-media out of the voice monolith.

2. **`media` — split out of `messages`?** `upload`/`unfurl`/`unfurlImageUrl`/`mediaUrl`/`mediaHash`/
   `avatarUrl`/`pullBackfill` are HTTP-IO helpers, not message logic. **Recommendation:** yes — make
   `media` its own domain; it's ~8 methods and the single best Rust-migration target (Phase 3).

3. **Wire-event handlers (`*Handlers.*`) — domain axis or `sync` axis?** The reducer already
   dispatches to per-domain handler maps (`sessionHandlers`, `socialHandlers`, `channelHandlers`,
   `profileHandlers`, `moderationHandlers`, `federationHandlers`, `reportsHandlers`, …), and each
   lives in its domain's model file. **Recommendation:** keep them classified by **domain** (matches
   the existing registry design + Phase-2 goal of one module per domain); `sync` owns only the
   dispatcher (`handle`) + collector + cursor. (This is what task #57 "per-domain handler registry"
   already anticipated.)

4. **Login lifecycle — `session` or `connection`?** `doConnect`/`keyLogin`/`enrollThisDevice`/
   `logout`/`setStatus` live in `connection.svelte.ts` but are auth. **Recommendation:** classify by
   *intent* → `session` (auth/identity), leaving `connection` for pure transport
   (connect/probe/reconnect/dial). They can still physically live in one file if you prefer.

5. **The giant `handle` reducer switch** — I listed it as one `sync`/business row with the covered
   event kinds noted. For Phase 2, do you want it decomposed case-by-case into the per-domain
   handler maps (continuing the existing pattern), or left as one switch? (This is a Phase-2 move,
   not a Phase-1 classification — flagging so it's on the radar.)

6. **Media string-helpers `mediaDims`/`withMediaDims`/`msgTime`** — decode-then-format hybrids I
   tagged `ui`/`mixed`. Fine as ui (display sizing), or force to their business sibling? Low stakes.

---

## Full inventory

The complete Method | File | Domain | Layer | Notes table, grouped by the seven read areas.
(Phase 2 will re-sort these by domain into one module-per-domain layout; kept area-ordered here
because that is how they were read and verified.)

### Area 1 — Models core (`src/lib/models/**`)

| Method | File | Domain | Layer | Notes |
|---|---|---|---|---|
| Account.name (get) | models/account.svelte.ts | profile | ui | strips `@network` from handle |
| Account.initials (get) | models/account.svelte.ts | profile | ui | two-letter monogram |
| Account.dotClass (get) | models/account.svelte.ts | profile | ui | presence dot class |
| Account.displayName (get) | models/account.svelte.ts | profile | ui | display name or name |
| Account.avatarUrl (get) | models/account.svelte.ts | profile | ui | builds avatar URL |
| accountHandlers.presence | models/account.svelte.ts | profile | business | presence event → Account |
| Channel.isDm/isGroup/nsId (get) | models/channel.svelte.ts | channels | ui | sigil/regex predicates |
| Channel.isMuted (get) | models/channel.svelte.ts | channels | business | walks to Server/store.mutedAt |
| Channel.markRead | models/channel.svelte.ts | channels | business | clear unread/mention |
| Channel.setTyping | models/channel.svelte.ts | channels | business | typing state + 6s timers |
| Channel.bump | models/channel.svelte.ts | channels | business | tally new msg/mention |
| mkMsg | models/channel.svelte.ts | messages | business | stamps monotonic render key |
| nsOf | models/channel.svelte.ts | channels | business | regex ns id from name |
| scopesFor | models/channel.svelte.ts | roles | business | covering scopes for target |
| nsCategories | models/channel.svelte.ts | channels | business | active server categories |
| setCategories | models/channel.svelte.ts | channels | business | sends NS META categories |
| moveChannel | models/channel.svelte.ts | channels | business | sends CHANNEL META cat/pos |
| moveCategory | models/channel.svelte.ts | channels | business | reorder cats; sends NS META |
| chanShort | models/channel.svelte.ts | channels | ui | short channel label |
| channelRecord | models/channel.svelte.ts | channels | business | lookup by name |
| ensureChannel | models/channel.svelte.ts | channels | business | intern channel, seed Server edge |
| markRead (fn) | models/channel.svelte.ts | channels | business | clear unread by name |
| sys | models/channel.svelte.ts | messages | business | push local system line |
| applyReaction | models/channel.svelte.ts | messages | business | apply REACTION delta |
| resetChannels | models/channel.svelte.ts | channels | business | clear all on logout |
| saveLayoutCache/loadLayoutCache | models/channel.svelte.ts | channels | business | localStorage layout persist |
| cacheNsCats/cacheChanLayout | models/channel.svelte.ts | channels | business | localStorage layout cache |
| reconcileChannelCreate | models/channel.svelte.ts | channels | business | post-create JOIN/META + nav |
| dmStoreKey/persistDms/restoreDms | models/channel.svelte.ts | channels | business | DM localStorage persistence |
| pinsHandlers.pinned/unpinned | models/channel.svelte.ts | messages | business | pinnedIds update; sends PINS |
| Collector.push/flush/size | models/collector.ts | sync | business | streamed-row batch buffer |
| emailNudgeKey | models/connect.svelte.ts | session | business | per-account localStorage key |
| Federation.applyManifest/applyNetblock | models/federation.svelte.ts | federation | business | event → maps |
| federationHandlers.manifest/netblocked | models/federation.svelte.ts | federation | business | route to Federation |
| refreshNetblocks | models/federation.svelte.ts | federation | business | sends NETBLOCK LIST |
| netblockAdd/netblockRemove | models/federation.svelte.ts | federation | business | sends NETBLOCK ADD/REMOVE |
| bridgePropose/bridgeAccept/bridgeSever | models/federation.svelte.ts | federation | business | sends BRIDGE verbs |
| invitesHandlers.invited/invite-info | models/invites.svelte.ts | invites | business | echo + buffer rows |
| openInviteCreate/mintInvite/createInvite | models/invites.svelte.ts | invites | business | open create screen |
| generateInvite | models/invites.svelte.ts | invites | business | sends INVITE MINT |
| sendInviteDM | models/invites.svelte.ts | invites | business | sends MSG w/ invite link |
| loadInvites/openInvites/loadNsInvites | models/invites.svelte.ts | invites | business | sends INVITE LIST |
| revokeInvite | models/invites.svelte.ts | invites | business | sends INVITE REVOKE |
| inviteLinkFor | models/invites.svelte.ts | invites | business | build `weft://` link |
| Membership.roles (get) | models/membership.svelte.ts | membership | business | resolve roleIds vs server roles |
| SearchPanel.begin | models/panels.svelte.ts | messages | business | reset+open search on channel |
| reportsHandlers.reported/report-filed/report-resolved | models/reports.svelte.ts | moderation | business | queue mutations + system lines |
| Role.grants | models/role.svelte.ts | roles | business | does role carry a cap |
| saveNsMeta | models/server.svelte.ts | namespaces | business | sends NS META + VISIBILITY |
| nsSetFederation | models/server.svelte.ts | federation | business | sends NS META federation |
| nsSetWelcome | models/server.svelte.ts | namespaces | business | sends NS META welcome |
| showRecoveryKey/startRecovery/cosignRecovery/submitRecovery | models/server.svelte.ts | namespaces | business | recovery ladder verbs |
| activeEmoji/addEmoji/removeEmoji/emojiUrlFor | models/server.svelte.ts | namespaces | business | custom emoji (EMOJI verbs) |
| rosterFetchTarget | models/server.svelte.ts | membership | business | current streaming roster ns |
| Server.displayName (get) | models/server.svelte.ts | namespaces | ui | title/name/id fallback |
| Server.scope/muteLevel/isMuted (get) | models/server.svelte.ts | namespaces | business | scope string + mute level |
| Server.member/fetchMembers/applyMembers | models/server.svelte.ts | membership | business | roster (NS INFO MEMBERS) |
| Server.role | models/server.svelte.ts | roles | business | role def by id |
| Server.applyMeta | models/server.svelte.ts | namespaces | business | absorb NS-META event |
| serverHandlers.emoji/emoji-removed | models/server.svelte.ts | namespaces | business | emoji event → map + md cache |
| Session.capsAt/ownerAt/can/moderates/canGrant/isOperator | models/session.svelte.ts | session | business | capability gate checks |
| ensureCapsAt/capsResolved/ensureCaps/badgeFor | models/session.svelte.ts | session | business | CAPS fetch + badge |
| isOwnerAt/isStaff/isNsOwner | models/session.svelte.ts | session | business | owner/operator checks |
| serverCap/serverCanGrant/canOpenServerSettings | models/session.svelte.ts | session | business | server-scope gates |
| roleScopeOf/rolesAt/roleById/nsRoleScope | models/session.svelte.ts | roles | business | role scope resolution |
| rolesOf/fetchMemberRoles/fetchRoles/fetchGrants | models/session.svelte.ts | roles | business | ROLES/GRANTS fetch |
| createRoleAt/deleteRoleAt/createRole/saveRole/deleteRole | models/session.svelte.ts | roles | business | ROLE CRUD verbs |
| toggleNewRoleCap/everyoneCaps/setEveryoneCaps | models/session.svelte.ts | roles | business | @everyone + draft caps |
| moveRole/reorderRoles | models/session.svelte.ts | roles | business | ROLES REORDER |
| ensureMemberRoles/ensureRoles/nameColor | models/session.svelte.ts | roles | business | lazy role fetch + color |
| assignRoleTo/unassignRoleFrom/assignNsRole/unassignNsRole | models/session.svelte.ts | roles | business | ROLE ASSIGN/UNASSIGN |
| reconcileRoster/memberRow | models/session.svelte.ts | membership | business | roster refetch/lookup |
| canModDelete/canModerate | models/session.svelte.ts | moderation | business | mod-power gates |
| mentionsMe | models/session.svelte.ts | messages | business | body mentions me/role/@everyone |
| sessionHandlers.caps/verified | models/session.svelte.ts | session | business | CAPS + §10.5 events |
| sessionHandlers.role/role-member/grant-info | models/session.svelte.ts | roles | business | buffer role/grant rows |
| roster.friends/incoming/outgoing/groups (get) | models/social.svelte.ts | social | ui | derived roster exposure |
| friendLocalAccount/qualify/meRef | models/social.svelte.ts | social | business | handle normalization |
| acceptFriend/removeFriend/friendAction | models/social.svelte.ts | social | business | FRIEND verbs |
| callUser | models/social.svelte.ts | voice/**calls?** | business | friends-gate; sends CALL |
| openGroupPicker | models/social.svelte.ts | social | mixed | positions popover + seeds peer |
| createGroupWith/leaveGroup | models/social.svelte.ts | social | business | GROUP CREATE/LEAVE |
| groupLabel | models/social.svelte.ts | social | ui | group display label |
| friendState | models/social.svelte.ts | social | business | friendship state |
| socialHandlers.friend/friend-removed/group/group-member | models/social.svelte.ts | social | business | friend/group events |
| socialHandlers.call-ring/call-state/call-media/group-call-state | models/social.svelte.ts | voice/**calls?** | business/glue | call events + LiveKit connect |
| AppStore.loadNotif/notifAt/mutedAt/setNotif | models/store.svelte.ts | store | business | notif prefs (localStorage) |
| AppStore.accountOf/server/resetPresence | models/store.svelte.ts | store | business | intern + presence reset |
| Threads.nameFor / threadsHandlers.thread/thread-named | models/threads.svelte.ts | messages | business | thread buffer + rename |
| activeChannel/threadCount/threadNameFor | models/threads.svelte.ts | messages | business | thread lookups |
| openThread/closeThread/sendThread/renameThread | models/threads.svelte.ts | messages | business | thread panel + MSG/NAME verbs |
| openThreads/closeThreads/openThreadByRoot | models/threads.svelte.ts | messages | business | thread list modal (THREADS) |

### Area 2 — Sync + protocol glue (`weft.ts`, `actions.ts`, `sync/**`)

| Method | File | Domain | Layer | Notes |
|---|---|---|---|---|
| pullBackfill | weft.ts | media/**?** | glue | fetch `/backfill`, replay lines |
| ensureWasm/invoke/onWeft | weft.ts | transport | glue | WASM load, IPC entry, `listen("weft")` |
| connect/probe/clientConfig/disconnect | weft.ts | connection | business/glue | invoke connect/probe/disconnect |
| enrollDevice/hasDeviceKey/presence | weft.ts | session | business | device key + presence |
| notify | weft.ts | notifications | glue | desktop notification |
| join/typing/part/channels | weft.ts | channels | business | JOIN/TYPING/PART/CHANNELS |
| channelCreate/channelPolicy/channelRename/channelDelete/channelMeta | weft.ts | channels | business | channel CRUD verbs |
| profileSet/nick/nicksQuery/profilesQuery | weft.ts | profile | business | §10.3 profile/nick |
| verifyEmail/verifyBirthday/verifyConfirm/verifyList | weft.ts | profile | business | §10.5 verification |
| voiceJoin/voiceLeave/voiceDesc/voiceCand | weft.ts | voice | business | §16 signaling |
| nsJoin/nsCreate/nsMeta/nsVisibility/nsDelegate/nsDelete/nsLeave | weft.ts | namespaces | business | NS verbs |
| nsRecoverySet/nsTransfer/nsRecoveryCancel/recoveryPubkey/recoveryStart/recoveryCosign/nsRecover | weft.ts | namespaces | business | recovery ladder |
| federate | weft.ts | federation | business | §11.10 FEDERATE |
| history/edit/del/react/unreact/sendMessage | weft.ts | messages | business | message verbs |
| setMediaBearer/mediaOrigin/setMediaBase | weft.ts | media/**?** | business | media token/base state |
| upload/unfurl/unfurlImageUrl/mediaUrl/mediaHash/avatarUrl | weft.ts | media/**?** | business | HTTP media IO/refs |
| mediaDims/withMediaDims | weft.ts | media/**?** | ui | `#WxH` sizing helpers |
| mark/pin/pins/search/listThreads/nameThread | weft.ts | messages | business | mark/pin/search/thread verbs |
| members/nsInfoMembers | weft.ts | membership | business | MEMBERS / NS INFO MEMBERS |
| friendAdd/friendAccept/friendRemove/listFriends | weft.ts | social | business | FRIEND verbs |
| groupCreate/groupAdd/groupRemove/groupLeave/groupName/listGroups | weft.ts | social | business | GROUP verbs |
| groupCall/groupCallLeave/call/callAccept/callDecline/callEnd | weft.ts | voice/**calls?** | business | call verbs |
| emojiAdd/emojiRemove/emojiList | weft.ts | namespaces | business | EMOJI verbs |
| caps/grant/revoke/roles/grantsAt | weft.ts | roles | business | cap/grant/role verbs |
| roleCreate/rolesReorder/roleDelete/roleUpdate/roleAssign/roleUnassign/rolesOfAccount | weft.ts | roles | business | role CRUD/assign verbs |
| inviteMint/inviteRedeem/inviteRevoke/inviteRevokeAll/inviteList | weft.ts | invites | business | invite verbs |
| moderate/report/reportsList/modList/reportsResolve | weft.ts | moderation | business | mod + report verbs |
| netblockAdd/netblockRemove/netblockList/bridgePropose/bridgeAccept/bridgeSever | weft.ts | federation | business | netblock/bridge verbs |
| discover | weft.ts | namespaces | business | DISCOVER |
| sendRaw | weft.ts | transport | business | raw wire line |
| sync | weft.ts | sync | business | sends SYNC line |
| autofocus | actions.ts | ui | ui | focus action |
| spoilerReveal | actions.ts | rendering | ui | spoiler reveal action |
| oldestMsgid/loadHistory | sync/reducer.svelte.ts | messages | business | paging cursor + single-flight backfill |
| handle | sync/reducer.svelte.ts | sync | business | dispatch to domain maps else giant switch (see note) |
| findMsg | sync/reducer.svelte.ts | sync | business | locate Msg in buffer/channel |
| channelHandlers.member/ns-member | sync/channel-handlers.ts | membership | business | roster + self-join nav |
| channelHandlers.chan-sync | sync/channel-handlers.ts | sync | business | §7.9 no-op |
| channelHandlers.chanmeta/channel-layout/channel-renamed | sync/channel-handlers.ts | channels | business | topic/layout/re-key |
| HandlerMap (type) | sync/handler-map.ts | sync | business | handler-map type only |

`handle` giant-switch covers (not owned by a domain map): connected, server-info, media-token,
auth-failed, closed, policy, message, marked, unread-counts, sync-end, ns-member-info, ns-meta,
more, token, typing, reaction, reactions, batch-start, batch-end, deleted, edited, error.

### Area 3 — Orchestrator modules (`composer`, `ctxmenu`, `chanperms`, `moderation`, `connection`, `nav*`, `channelcreate`, `confirm`, `mdrender`, `viewmodel`, `view`, `profile`, `notif`)

| Method | File | Domain | Layer | Notes |
|---|---|---|---|---|
| compose (state) / composeView (getter) | composer.svelte.ts | messages | ui | draft state + autocomplete views |
| activeChannel / _mentionMatches / _emojiSuggestions / _typingLabel | composer.svelte.ts | messages | ui/business | derived autocomplete/typing |
| addFiles/pasteFiles/dropFiles/removeAttachment/attachFile | composer.svelte.ts | messages | business/mixed | upload via weft.upload |
| runSlash | composer.svelte.ts | messages | business | join/part/channelCreate/Delete/Meta |
| doSend | composer.svelte.ts | messages | business | optimistic; weft.sendMessage |
| composerKey/editKey/jumpTo | composer.svelte.ts | messages | ui | keyboard nav / scroll |
| stopTyping/onComposerInput | composer.svelte.ts | messages | business | weft.typing |
| updateMention/pickMention/updateEmojiSuggest/pickEmojiSuggestion | composer.svelte.ts | messages | business | autocomplete parsing/insert |
| startEdit/cancelEdit/saveEdit | composer.svelte.ts | messages | business | inline edit; weft.edit |
| doDelete/toggleReaction/togglePin | composer.svelte.ts | messages | business | weft.del/react/pin |
| openReport | composer.svelte.ts | messages | business | sets store.reports.target |
| activeChannel | ctxmenu.svelte.ts | channels | business | active Channel resolve |
| ctxMenu (getter)/openCtx | ctxmenu.svelte.ts | ui | ui | open/close/position menu |
| msgCtx/chanCtx/userCtx/groupCtx/nsMemberCtx/catCtx/listCtx | ctxmenu.svelte.ts | ui | ui | menu builders (verbs inside item closures) |
| chanNsScope/chanRoleCaps/chanMemberGrants/chanMemberCaps | chanperms.ts | roles | business | channel perm reads |
| setChanRoleCaps/setChanMemberCaps | chanperms.ts | roles | business | GRANT/REVOKE (via role helpers) |
| removeChanRole/removeChanMember | chanperms.ts | roles | business | REVOKE |
| openChanPerms | chanperms.ts | roles | business | fetchRoles/fetchGrants |
| toggleRestricted/toggleViewGated | chanperms.ts | roles | business | weft.channelMeta |
| banScope/denyList | moderation.ts | moderation | business | scope + deny-list |
| refreshBans/moderate/liftMod | moderation.ts | moderation | business | weft.modList/moderate |
| moderationHandlers.moderated | moderation.ts | moderation | business | MODERATED → store.deny |
| syncState/syncCursorKey/loadSyncCursor | connection.svelte.ts | sync | business | SYNC cursor state |
| conn (state)/attemptReconnect/nsMetaFetched | connection.svelte.ts | connection | business | reconnect + creds |
| setStatus | connection.svelte.ts | session | business | weft.presence |
| keyLogin/enrollThisDevice/doConnect/logout | connection.svelte.ts | session | business | auth lifecycle |
| probeServer/chooseServer/changeServer | connection.svelte.ts | connection | business/ui | homeserver probe/pick |
| dmKeyFor/openDm/closeDm/messageFriend | navigation.ts | navigation | business | DM routing |
| open/openGroup/openFriends/openServerMenu/openVoice/openDiscover | navigation.ts | navigation | business | target routing (+ side effects) |
| nsLeave/selectServer/goHome | navigation.ts | navigation | business | ns leave / server / home nav |
| pathFor/viewFrom | nav.ts | navigation | business | pure URL↔view (unit-testable) |
| chanDraft/catDraft (state)/resetChan | channelcreate.svelte.ts | channels | ui | create drafts |
| openCreateChannel/openCreateChannelInCat/openCreateCategory | channelcreate.svelte.ts | channels | ui | open modals |
| createChannel/createCategory/deleteCategory | channelcreate.svelte.ts | channels | business | weft.channelCreate/Meta |
| confirmDialog (getter)/appConfirm/resolveConfirm | confirm.svelte.ts | ui | ui | promise confirm dialog |
| mdContext/renderMd | mdrender.svelte.ts | rendering | business | ambient MdContext render |
| vm (getter) + _activeChannel/_serverNamespaces/_dmList/serverChannels | viewmodel.svelte.ts | navigation | ui | derived rail/view |
| _activeNsMeta/serverName | viewmodel.svelte.ts | namespaces | ui | active ns meta / name |
| _channelGroups | viewmodel.svelte.ts | channels | ui | Discord channel grouping |
| serverUnread/serverMention/serverMentionCount/titleOf | viewmodel.svelte.ts | navigation | ui | rail rollups + labels |
| isNsMember/nsMembers/nsMembersLoading/fetchNsMembers | viewmodel.svelte.ts | membership | ui/business | membership reads |
| v/view (getter) | view.svelte.ts | navigation | ui | View from URL |
| nicks/nickKey/nicksFetched | profile.svelte.ts | profile | business | nick cache + dedup |
| activeServer | profile.svelte.ts | navigation | business | activeServer from URL |
| peerOf/localTarget | profile.svelte.ts | profile | business | handle normalization |
| initials/dotClass/avatarUrl | profile.svelte.ts | profile | ui | avatar/presence display |
| displayName/nickOf/bioOf/statusOf/friendLabel | profile.svelte.ts | profile | business | name/bio/status resolution |
| queryProfile | profile.svelte.ts | profile | business | weft.profilesQuery (deduped) |
| profileHandlers.profile/nick | profile.svelte.ts | profile | business | PROFILE/NICK events |
| setNick/setCustomStatus | profile.svelte.ts | profile | business | weft.nick/profileSet |
| openProfile/openFullProfile/openNickDialog | profile.svelte.ts | profile | mixed/business | popover + fetch caps/roles |
| scopeKeyOf/notifLevel/isMuted/serverMuted/notifLevelOf | notif.ts | notifications | business | notif level lookups |
| setNotifLevel/notifScopeKey/notifScopeLabel | notif.ts | notifications | business | set + scope labels |

### Area 4 — UI-state, utils, voice (`ui`, `toasts`, `lightbox`, `linkguard`, `voice`, `callmedia`, `voiceui`, `markdown`, `highlight`, `mdhighlight`, `emoji`, `shortcodes`, `time`, `constants`, `types`, `context`)

| Method | File | Domain | Layer | Notes |
|---|---|---|---|---|
| ui (state) | ui.svelte.ts | ui | ui | modal/overlay/drag flags |
| toggleTheme | ui.svelte.ts | ui | mixed | theme + dataset + localStorage |
| toasts (state)/toast | toasts.svelte.ts | notifications | ui | transient toast list |
| expectSuccess/confirmSuccess | toasts.svelte.ts | notifications | business | server-confirmed success toasts |
| lightbox (state)/openLightbox/closeLightbox | lightbox.svelte.ts | ui | ui | image viewer |
| linkPrompt (state)/askLink/closeLink | linkguard.svelte.ts | ui | ui | link-confirm dialog |
| openConfirmed | linkguard.svelte.ts | ui | mixed | synthetic anchor nav (DOM) |
| installLinkGuard | linkguard.svelte.ts | ui | glue | delegated document click listener |
| voice/voiceRosters (state) | voice.svelte.ts | voice | business | voice model + rosters |
| setVideoTrack/clearVideoTrack/nativeVideoUrl | voice.svelte.ts | voice | business/ui | LiveKit track map |
| initVoice | voice.svelte.ts | voice | glue | onWeft + Tauri listen |
| joinVoice/leaveVoice/toggleMute/toggleDeafen | voice.svelte.ts | voice | business | join/leave/mute |
| startCamera/stopCamera/startScreenShare/stopScreenShare | voice.svelte.ts | voice | business | LiveKit camera/screen |
| startNativeVoiceScreenshare/stopNativeVoiceScreenshare/listNativeCameras/startNativeVoiceCamera/stopNativeVoiceCamera | voice.svelte.ts | voice | glue | invoke voice_native_* |
| startNativeScreenShare/publishCanvasStream/stopNativeScreenShare | voice.svelte.ts | voice | business | canvas capture → LiveKit |
| attachVideo/detachVideo/applyDeafen | voice.svelte.ts | voice | business | DOM `<video>` / audio mute |
| teardown/onVoiceEvent/onOffer/onState | voice.svelte.ts | voice | business | media-plane dispatch |
| onNativeLiveKit | voice.svelte.ts | voice | glue | invoke voice_native_connect |
| onLiveKitOffer/upsertParticipant/onSpeakers | voice.svelte.ts | voice | business | LiveKit room wiring |
| onWebrtcOffer/onAnswer/onCandidate/waitIceComplete | voice.svelte.ts | voice | business | WebRTC non-trickle (browser-bound) |
| withOpusQuality/iceConfig | voice.svelte.ts | voice | business | pure SDP/RTCConfig (portable) |
| callMedia (state)/connectCallMedia | callmedia.svelte.ts | voice/**calls?** | business | 1:1 call media (LiveKit) |
| connectNative | callmedia.svelte.ts | voice/**calls?** | glue | invoke voice_native_connect + listen |
| toggleCallMute/disconnectCallMedia/teardown | callmedia.svelte.ts | voice/**calls?** | business | call media control |
| voiceUI (state)/openScreenMenu | voiceui.svelte.ts | voice | ui | picker flags + popover pos |
| escapeHtml/renderInline/clearMdCache/renderMd/renderMdRaw | markdown.ts | rendering | business | markdown→HTML (XSS-critical) |
| escapeHtml/highlightCode | highlight.ts | rendering | business | highlight.js |
| esc/hlInline/hlLine/highlightComposer | mdhighlight.ts | rendering | ui/business | composer live highlight |
| EMOJI/QUICK_EMOJI (const) | emoji.ts | rendering | ui | curated emoji data |
| shortcodeToChar/searchUnicode/charName | shortcodes.ts | rendering | business | node-emoji lookups |
| hhmm/clock/startOfDay/dayKey/dayLabel | time.ts | rendering | ui | time/date formatting |
| msgEpoch/retentionOf | time.ts | rendering | business | ULID→epoch, retention parse |
| msgTime | time.ts | rendering | mixed | ULID→HH:MM |
| (constants) | constants.ts | roles | business | caps/report/retention/color options |
| (types) | types.ts | store | business | Member/Msg/etc types |
| provideApp/getApp | context.ts | ui | glue | Svelte context bridge |
| (AppCtx interface) | context.ts | ui | business | container↔component contract |

### Area 5 — Components + routes (`components/**` non-modal, `sidebar/**`, `chat/**`, `routes/**`)

| Method | File | Domain | Layer | Notes |
|---|---|---|---|---|
| — | Avatar.svelte | profile | ui | derived lookup only |
| label | CallOverlay.svelte | voice/**calls?** | ui | friendLabel wrapper |
| — | CapChecklist.svelte | roles | ui | props checklist |
| — | CommunityRail.svelte | namespaces | ui | reads vm |
| — | ConnectScreen.svelte | connection | ui | binds form |
| ($effect reposition) | ContextMenu.svelte | ui | ui | viewport clamp |
| — | EmptyHome.svelte | navigation | ui | routes to Discover |
| avatarAccount/statusOf/isOnline/subtitle/onAddKey | FriendsView.svelte | social | ui | presence + labels |
| copy/expiryLabel | InviteList.svelte | invites | ui | clipboard + time |
| presenceOf/isOnline | MemberList.svelte | membership | ui | presence |
| ($effect ensureRoles) | MemberList.svelte | roles | glue | fetch role defs/profiles |
| primaryHoist / groups ($derived.by) | MemberList.svelte | roles | mixed | **Discord hoisted-role grouping algo** |
| — | QuickSwitcher.svelte | navigation | ui | bindable query |
| — | SaveBar.svelte | ui | ui | props dock |
| — | Toasts.svelte | notifications | ui | renders toast list |
| openStage | VoiceBar.svelte | voice | ui | open voice channel |
| camClick/screenClick | VoiceBar.svelte | voice | mixed | desktop/web branch → controller |
| — | AppModals.svelte | ui | ui | modal-stack projection |
| switcherResults ($derived) | AppShell.svelte | navigation | ui | switcher filter |
| switchTo | AppShell.svelte | navigation | glue | goto(pathFor) |
| globalKey | AppShell.svelte | ui | ui | Ctrl+K / Escape |
| startDm | AppShell.svelte | messages | business | **parses @handle, opens DM (verb in view)** |
| joinNamespace | AppShell.svelte | namespaces | business | **nsJoin + channels (verbs in view)** |
| doJoin | AppShell.svelte | channels | business | **parse #chan vs ns, join (verb in view)** |
| needsEmailWarning ($derived) | AppShell.svelte | session | business | §6.1 email-nudge rule |
| openVerification | AppShell.svelte | profile | ui | opens settings tab |
| dismissEmailBanner | AppShell.svelte | session | mixed | flag + localStorage |
| box ($derived.by) / ($effect complete-check) | chat/Attachment.svelte | messages | ui | §13 image-fit math |
| onMount probe | chat/Attachment.svelte | media/**?** | glue | Range-fetch to downgrade media kind |
| — | chat/ChatTopbar.svelte | channels | ui | inline handlers |
| — | chat/ChatView.svelte | channels | ui | routes voice/chat/empty |
| keepInView/autosize/syncScroll/($effect) | chat/Composer.svelte | ui | ui | textarea grow + overlay sync |
| insertEmoji | chat/Composer.svelte | messages | ui | append to compose.text |
| onDragOver/onDrop | chat/Composer.svelte | messages | ui/glue | drag highlight → dropFiles |
| sections/visible ($derived.by) / jump | chat/EmojiPicker.svelte | messages | ui | emoji sections + search |
| onKey | chat/Lightbox.svelte | ui | ui | Escape closes |
| imageSrc ($derived) | chat/LinkPreview.svelte | messages | ui | unfurl image url |
| ($effect unfurl) | chat/LinkPreview.svelte | media/**?** | glue | fetch unfurl (stale-guard) |
| firstLink ($derived.by) | chat/MessageItem.svelte | messages | ui | first URL extract |
| onScroll/pinBottom/restoreOlder/measure/unreadIndex/positionOpen | chat/MessageList.svelte | messages | business | **virtualization/anchor/paging state machine** |
| raf/getKey/v + 4×$effect | chat/MessageList.svelte | messages | ui/business | load-first-page, count-sync, position, stick |
| onKey/startRename/commitRename/nameKey | chat/ThreadPanel.svelte | messages | ui/glue | thread reply/rename |
| camClick/screenClick | chat/VoiceStage.svelte | voice | mixed | desktop/web → controller |
| tiles ($derived.by)/bindVideo | chat/VoiceStage.svelte | voice | ui/glue | roster select + LiveKit attach |
| rosterOf | sidebar/ChannelList.svelte | voice | ui | live-vs-preview roster |
| (inline drag handlers) | sidebar/ChannelList.svelte | channels | business | drag-drop → moveChannel/moveCategory |
| — | sidebar/DmList.svelte | social | ui | inline nav/ctx |
| — (inline) | sidebar/SidebarHeader.svelte | namespaces | mixed | mintInvite/nsLeave/clipboard/menus in markup |
| — | sidebar/SidebarInput.svelte | ui | ui | bindable + Enter |
| openStatusModal/saveStatus/clearStatus/focusInput | sidebar/UserFooter.svelte | profile | ui/glue | custom status |
| — (5 route files) | routes/**/+page.svelte | navigation | ui | mount view fragment |
| openNotifSettings | routes/+layout.svelte | notifications | ui | open modal |
| ($effect nicks fetch) | routes/+layout.svelte | profile | glue | prefetch server nicks |
| mutualServers | routes/+layout.svelte | namespaces | business | shared-ns derivation |
| openFederation | routes/+layout.svelte | federation | glue | open panel + refreshNetblocks |
| openServerProfile | routes/+layout.svelte | profile | ui | open editor |
| assignRole | routes/+layout.svelte | roles | business | expectSuccess + roleAssign |
| ($effect rail ns-meta / layout / emoji) | routes/+layout.svelte | namespaces/channels/messages | glue | prefetchers |
| inviteToServer | routes/+layout.svelte | invites | glue | openInvites |
| addFriend/createGroup/addToGroup | routes/+layout.svelte | social | business | qualify + FRIEND/GROUP verbs |
| startGroupCall/leaveGroupCall/acceptCall/declineCall/endCall | routes/+layout.svelte | voice/**calls?** | business | call verbs + media |
| ($effect device-key / roster) | routes/+layout.svelte | connection/membership | glue | probes/fetches |
| ($effect ensure DM record) | routes/+layout.svelte | channels | business | ensureChannel + persistDms (deep-link) |
| ($effect new-divider) / newDividerKey ($derived.by) | routes/+layout.svelte | messages | business | unread boundary |
| ($effect auto-mark read) | routes/+layout.svelte | messages | business | markRead + MARK (§9.7) |
| openReports/openPins/openSearch | routes/+layout.svelte | moderation/messages | business/glue | open panels + verbs |
| ($effect thread-close) | routes/+layout.svelte | messages | glue | closeThread on channel change |
| openNsSettings | routes/+layout.svelte | namespaces | business | seed nsAdmin + fetchRoles |
| federate/cancelFederating/($effect federation-arrive) | routes/+layout.svelte | federation | business/ui | FEDERATE + banner + arrive-detect |
| doTransfer/deleteNamespace | routes/+layout.svelte | namespaces | business | confirm + nsTransfer/nsDelete |
| revokeAllInvites | routes/+layout.svelte | invites | business | confirm + inviteRevokeAll |
| onMount | routes/+layout.svelte | session | mixed | onWeft(handle) wire + config + restore + doConnect |
| provideApp {…} | routes/+layout.svelte | ui | glue | AppCtx assembly |

### Area 6 — Modal components (`components/modals/**`)

| Method | File | Domain | Layer | Notes |
|---|---|---|---|---|
| loadDevices/preview/stopPreview/choose/cancel | modals/CameraPicker.svelte | voice | glue/ui | enumerate + getUserMedia preview |
| start/turnOff | modals/CameraPicker.svelte | voice | business | start/stopCamera |
| sameCaps/persistedCaps/toggleCap/revertPerms/addRole/addMember/roleColor | modals/ChannelSettings.svelte | roles | ui | draft cap editing |
| savePerms/removeRoleTarget/removeMemberTarget | modals/ChannelSettings.svelte | roles | business | GRANT/REVOKE |
| doRename | modals/ChannelSettings.svelte | channels | mixed | validate slug + channelRename |
| saveTopic/setRetention | modals/ChannelSettings.svelte | channels | business | channelMeta/channelPolicy |
| deleteChannel | modals/ChannelSettings.svelte | channels | mixed | confirm + channelDelete |
| — | modals/ConfirmModal.svelte | ui | ui | promise dialog |
| — | modals/CreateCategoryModal.svelte | ui | ui | bindable + oncreate |
| — | modals/CreateChannelModal.svelte | ui | ui | bindable + oncreate |
| initials | modals/DiscoverModal.svelte | ui | ui | name→initials |
| joinNamespace/createNamespace | modals/DiscoverModal.svelte | namespaces | business/mixed | nsJoin+channels / nsCreate |
| connectForeign | modals/DiscoverModal.svelte | federation | mixed | parse invite + federate |
| doRedeem | modals/DiscoverModal.svelte | invites | mixed | federate vs inviteRedeem |
| addBlock/propose | modals/FederationPanel.svelte | federation | business | NETBLOCK ADD / BRIDGE PROPOSE |
| copy | modals/InviteCreateModal.svelte | invites | ui | clipboard |
| generate/revoke | modals/InviteCreateModal.svelte | invites | business | INVITE MINT/REVOKE |
| statusOf/isOnline/toggle | modals/InviteCreateModal.svelte | social | ui | friend picker |
| send | modals/InviteCreateModal.svelte | invites | business | sendInviteDM (MSG DM) |
| — | modals/InvitesModal.svelte | invites | ui | wraps InviteList |
| onkeydown | modals/LinkWarningModal.svelte | ui | ui | Escape closes |
| toggle/avatarAccount | modals/NewGroupModal.svelte | social | ui | group picker |
| focusInput | modals/NicknameModal.svelte | ui | ui | focus action |
| save | modals/NicknameModal.svelte | profile | business | setNick (§10.3) |
| — (inline setNotifLevel) | modals/NotificationSettingsModal.svelte | notifications | ui | local-device pref |
| — (inline pin false) | modals/PinsModal.svelte | messages | ui | **unpin inline in template** |
| — (inline moderate/setNick/assignRole) | modals/ProfileCard.svelte | moderation/roles | ui | **MUTE/BAN/KICK + role assign in markup** |
| message/jumpServer/copyId | modals/ProfileModal.svelte | navigation/ui | ui | openDm / selectServer / clipboard |
| call | modals/ProfileModal.svelte | voice/**calls?** | business | callUser |
| submit | modals/ReportModal.svelte | moderation | mixed | scope-derive + REPORT |
| — (inline reportsResolve) | modals/ReportsQueueModal.svelte | moderation | ui | **REPORTS RESOLVE inline** |
| load/loadThumbs | modals/ScreenPicker.svelte | voice | glue | invoke list/thumb capture sources |
| pick/stopSharing | modals/ScreenPicker.svelte | voice | business | start/stopNativeVoiceScreenshare |
| cancel | modals/ScreenPicker.svelte | voice | ui | close |
| close/changeSource | modals/ScreenShareMenu.svelte | voice | ui | menu |
| stop/setQuality | modals/ScreenShareMenu.svelte | voice | business | stop/re-publish share |
| submit | modals/SearchModal.svelte | messages | mixed | store.search + weft.search (§6.4) |
| jumpToResult | modals/SearchModal.svelte | navigation | ui | jumpTo(msgid) |
| sameCaps/pick/pickEveryone/pickNew/toggleDraftCap/toggleEveryoneCap/revertSelection/onDragStart/onDragOver | modals/RolesTab.svelte | roles | ui | role draft editing |
| save/saveSelection/create/remove | modals/RolesTab.svelte | roles | business | saveRole/setEveryoneCaps/createRole/deleteRole |
| onDrop/onRowKey | modals/RolesTab.svelte | roles | business | reorderRoles/moveRole (persist) |
| save/clearNick | modals/ServerProfileModal.svelte | profile | business | setNick (own ns) |
| roleColor/roleName/fmtJoined/unheldRoles | modals/ServerSettingsModal.svelte | roles/membership | ui | display helpers |
| proposeBridge | modals/ServerSettingsModal.svelte | federation | business | BRIDGE PROPOSE (ns) |
| pickEmojiImage/submitEmoji/cancelEmoji | modals/ServerSettingsModal.svelte | namespaces | business/mixed | upload + addEmoji |
| — (many inline) | modals/ServerSettingsModal.svelte | namespaces/moderation/federation/roles | mixed | **recovery/federation/welcome/bans/bridge/role-assign in markup** |
| preview | modals/ThreadsModal.svelte | messages | ui | root preview text |
| sendCode/confirmEmail/saveBirthday | modals/UserSettingsModal.svelte | profile | business | §10.5 VERIFY verbs |
| onAvatarPicked/saveProfile | modals/UserSettingsModal.svelte | profile | mixed | upload + PROFILE SET + PRESENCE |
| revertProfile | modals/UserSettingsModal.svelte | profile | ui | reset draft |

### Area 7 — Rust backend (`src-tauri/src/**`)

| Method | File | Domain | Layer | Notes |
|---|---|---|---|---|
| main | main.rs | session | glue | calls run() |
| Conn::send | lib.rs | connection | glue | locks Mutex sender; core of every command |
| connect [cmd] | lib.rs | connection | glue | spawns run_connection; loads config+device key |
| client_config [cmd] | lib.rs | session | glue | returns config JSON |
| enroll_device [cmd] | lib.rs | session | glue | device key → AUTH ENROLL |
| has_device_key [cmd] | lib.rs | session | glue | keys::has_device |
| disconnect [cmd] | lib.rs | connection | glue | drops tx |
| ~95 thin verb commands [cmd] | lib.rs | (per verb) | glue | **each = conn.send(build_verb(...)); Conn state only** |
| ns_create [cmd] | lib.rs | namespaces | glue | generates root key locally, sends pubkey |
| ns_transfer / ns_recovery_cancel [cmd] | lib.rs | namespaces | mixed | loads root key, signs locally, sends |
| recovery_pubkey/recovery_start/recovery_cosign [cmd] | lib.rs | namespaces | mixed | crypto only, **no send** (returns b64) |
| send_raw [cmd] | lib.rs | transport | glue | raw wire-line escape hatch |
| grant_media_permission | lib.rs | voice | business | webview setup: Linux mic / macOS screen-capture |
| enable_wkwebview_screen_capture / enable_matching | lib.rs | voice | business | macOS ObjC WKPreferences (getDisplayMedia) |
| run | lib.rs | session | glue | **Tauri builder: .manage(), setup hook, invoke_handler!** |
| ClientConfig / path / load | config.rs | session | business | serde config load (secure defaults) |
| keys_dir/sanitize/key_path/device_path/enroll_device/load_device/has_device/write_secret | keys.rs | session | business | device keypair persistence (0600) |
| generate_ns_key/store_ns_key/recovery_key/load_ns_key | keys.rs | namespaces | business | ns root + recovery keys |
| TauriSink::emit | weft.rs | connection | glue | **app.emit("weft", event)** — sole inbound bridge |
| resolve | weft.rs | connection | business | DNS lookup_host |
| run_connection | weft.rs | connection | business | connection loop: HELLO, auth FSM, keepalive, relay, emit |
| send / connect (weft) | weft.rs | transport | business | QUIC send_line / endpoint open (verified vs insecure) |
| jpeg_data_url / grab / stop_running | screencap.rs | voice | business | frame capture + JPEG encode |
| list_capture_sources / capture_source_thumb [cmd] | screencap.rs | voice | business | enumerate monitors/windows; no state |
| start_capture [cmd] | screencap.rs | voice | business | **Channel<String> on_frame** — streams JPEG frames |
| stop_capture [cmd] | screencap.rs | voice | glue | CaptureState stop flag |
| MicProc::new/process_f32/process_i16/feed | voice_native.rs | voice | business | mic downmix→resample→RNNoise |
| build_mic_stream/start_mic_capture | voice_native.rs | voice | business | cpal stream → LiveKit source |
| roster/source_kind/jpeg_data_url_rgba/scale_capture/rgba_to_i420 | voice_native.rs | voice | business | roster + frame transforms |
| remote_video_task | voice_native.rs | voice | mixed | I420→RGBA→JPEG, emit voice-native-frame |
| voice_native_connect [cmd] | voice_native.rs | voice | mixed | LiveKit join+publish; emits state/roster/frame |
| emit_self_frame | voice_native.rs | voice | glue | emit voice-native-frame (self preview) |
| voice_native_start_screenshare/start_camera [cmd] | voice_native.rs | voice | mixed | publish track; emit self-preview frames |
| voice_native_stop_screenshare/stop_camera [cmd] | voice_native.rs | voice | mixed | unpublish; emit voice-native-frame-end |
| voice_native_list_cameras [cmd] | voice_native.rs | voice | business | nokhwa query |
| voice_native_set_muted [cmd] | voice_native.rs | voice | mixed | mute; emit voice-native-roster |
| voice_native_disconnect [cmd] | voice_native.rs | voice | glue | teardown session |

---

## Existing IPC surface (Phase-3 baseline)

**Commands** (~123): ~110 in `lib.rs` (mostly thin `conn.send` verb wrappers), 4 in `screencap.rs`
(`list_capture_sources`, `capture_source_thumb`, `start_capture`, `stop_capture`), 9 in
`voice_native.rs` (`voice_native_connect/set_muted/disconnect/start_screenshare/stop_screenshare/
list_cameras/start_camera/stop_camera`).

**Events emitted from Rust** (`app.emit`):
- `"weft"` — the single inbound-protocol channel; payload = `ClientEvent` enum (Connected, Closed,
  and every server-pushed event) from the portable `weft_client_core` crate.
- `"voice-native-state"` — `&str` connected/disconnected.
- `"voice-native-roster"` — `Vec<RosterEntry>`.
- `"voice-native-frame"` — `VideoFrameMsg { user, source, data }`.
- `"voice-native-frame-end"` — `VideoEndMsg { user, source }`.

**Channels**: `screencap::start_capture` takes `on_frame: Channel<String>` (base64 JPEG data URLs).
Only `tauri::ipc::Channel` on the Rust side.

**Managed state** (`.manage()`):
- `Conn { tx: Mutex<Option<mpsc::UnboundedSender<String>>> }` — outbound sender, touched by ~105 commands.
- `screencap::CaptureState { running: Mutex<Option<Arc<AtomicBool>>> }` — capture stop-flag.
- `voice_native::NativeVoice = tokio::Mutex<Option<Session>>` — active LiveKit session.

`AppHandle` (not managed state) is injected into config/key/recovery/voice commands for file access
+ event emit.

---

## Proposed domain set (FINAL — decided)

**21 domains** = the 20 base + **`media`** (split-out approved). Decisions taken:
- **`calls` — NOT a separate domain.** 1:1 + group calls stay folded: call *media* under `voice`,
  call *ring/state/signaling* under `social` (matches the Rust side, which files calls under social).
  In the tables above, rows tagged `voice/**calls?**` resolve to `voice` (media) or `social`
  (ring/state) per that split.
- **`media` — its own domain.** upload/unfurl/mediaUrl/avatarUrl/backfill/dims move out of `messages`.
  Rows tagged `media/**?**` resolve to `media`. Strongest Rust-migration target for Phase 3.
- **Wire-event handlers classified by domain** (not `sync`). Each `*Handlers.*` stays in its domain
  module; `sync` owns only the dispatcher (`handle`), collector, and cursor.
- **Login lifecycle → `session`** by intent (doConnect/keyLogin/enrollThisDevice/logout/setStatus),
  leaving `connection` for pure transport. (May still physically live in connection.svelte.ts.)

Every domain clears the 3-method bar — no merges.

One-liners:
1. **session** — identity, auth, device keys, capability-held gates, login lifecycle.
2. **connection** — transport lifecycle: connect/probe/reconnect/dial; the Rust connection loop.
3. **transport** — raw wire-line send + IPC boundary (invoke/onWeft/emit) + QUIC/WASM connect.
4. **sync** — inbound event dispatch, streamed-row collector, SYNC cursor.
5. **store** — root reactive container: intern accounts/servers, notif lookup.
6. **channels** — channel records, layout/categories, create/rename/policy/delete, drag-order.
7. **messages** — compose/send/edit/react/history/pins/threads + the message-list state machine.
8. **media** — upload/unfurl/mediaUrl/avatar/backfill HTTP IO + media refs.
9. **namespaces** — server/ns model, NS-META, visibility, recovery ladder, custom emoji, discover.
10. **membership** — NS membership records + roster fetch/apply.
11. **roles** — role defs + per-scope capability grants/revokes + assignment + reorder.
12. **moderation** — mute/ban/kick deny-list + reports queue.
13. **social** — friends + group DMs (federation-able) + call ring/state/signaling.
14. **voice** — voice/screenshare/camera media plane (native + LiveKit + WebRTC) + call media.
15. **federation** — bridges, netblocks, on-demand federate, manifests.
16. **invites** — invite mint/redeem/revoke/list + link building + DM delivery.
17. **notifications** — notification prefs + toasts + desktop-notify.
18. **navigation** — URL↔view mapping, routing, target selection, rail/switcher.
19. **profile** — display names, nicknames, avatars, custom status, §10.5 verification.
20. **rendering** — markdown/code-highlight/emoji-shortcode/time formatting.
21. **ui** — modals/menus/theme/toasts/confirm/lightbox/link-guard/context-bridge/drag.

**STOP — Phase 1 complete. Awaiting explicit approval to begin Phase 2 (mechanical moves).**

---

## Phase 2 — result (DONE, green)

Full `lib/<domain>/` relocation executed. `lib/models/` removed; only `constants.ts`
+ `types.ts` remain flat at `lib/` root (shared, cross-cutting — not a domain).
`check` 0/0 + `build` ✓ after every step. 103 files touched (45 renames).

Steps: (P2.0) normalized all relative imports → `$lib/…` absolute; (relocation) moved every
logic module into its domain folder; (splits) `channel → channels + messages` (mkMsg/sys/
applyReaction/pinsHandlers → `messages/messages.svelte`), `session → session + roles`
(role defs/editor/assignment + `rolesHandlers` → `roles/roles.svelte`; gates/caps stay),
`weft.ts` kept whole as `transport/weft.ts` + `media` domain extracted to `media/media.ts`.

Decisions applied: calls stay folded (voice+social); media is its own domain; handlers by
domain (`rolesHandlers` registered in the reducer); login lifecycle classified session.
`weft.ts` NOT shattered — it is the transport/protocol client.

---

# Phase 3a — UI/business split + Rust-migration classification (ANALYSIS, no code)

## Framing (what the empirical grounding shows)

- **Zero `setInterval`** anywhere in the TS. The inbound plane is already fully event-driven:
  one `"weft"` event from Rust (`TauriSink`) drives the reducer; voice already streams via a
  Rust `Channel` + `voice-native-*` events. **There are no TS polling loops to convert.**
- **Exactly one TS-driven connection loop**: `attemptReconnect` (exponential-backoff `setTimeout`
  → `weft.connect`, creds in `conn.lastCreds`). Every other `setTimeout` is an ephemeral UI timer
  (typing-stop, toast auto-remove, roster reconcile, verify-load gate) — not a candidate.
- **Exactly 3 `fetch` sites**: `pullBackfill` (web/WASM path), `media.upload`, `media.unfurl`.
- **localStorage** is all client-local cache/prefs (theme, creds, sync cursor, layout cache, DM
  list, notif prefs, email-nudge) — already persists across reloads.
- The Rust backend is already ~pure glue; **the protocol migration already happened** (every verb
  is a Rust command). So the remaining TS "business" is overwhelmingly **optimistic-UI + reactive-
  store mutation + web-dual-path + DOM/render** code that *correctly* stays in the webview.

**Net: the honest migration surface is small and specific.** Below, the dominant buckets are the
"keep" ones (with the reason they can't/shouldn't move); the short Move / Event / Split lists are
the only items that need your per-item approval.

## Keep-in-TS buckets (the majority — cannot or should-not move)

| Bucket | What's in it | Why it stays |
|---|---|---|
| **Reactive-store-owning** | the whole reducer + every `*Handlers` map (session/roles/social/channel/federation/invites/profile/reports/moderation/server/threads/pins/account), all model classes (Channel/Server/Session/Account/Membership/Role), viewmodels | mutates the reactive Svelte `$state` graph the UI renders. Moving = inverting the whole client model into Rust and making Svelte a dumb view — a total rearchitecture, out of scope, anti-KISS. **CANNOT move.** |
| **Optimistic-UI verb senders** | doSend, saveEdit, doDelete, toggleReaction, togglePin, moderate/liftMod, createRole/saveRole/assignNsRole/…, netblock/bridge actions, friendAction/createGroupWith, invite actions, ns-meta/recovery actions | body = optimistic store push + `weft.X()` (already a Rust command) + label dedup. The protocol work is *already in Rust*; the TS part is reactive UI. Round-tripping it back adds nothing. **Keep.** |
| **Pure render/format helpers** | markdown (`renderMd`/`renderInline`/`renderMdRaw`/`highlightCode`/`escapeHtml`), URL builders (`mediaUrl`/`mediaHash`/`avatarUrl`/`unfurlImageUrl`/`mediaDims`), time (`msgEpoch`/`msgTime`/`retentionOf`), emoji shortcodes | called per-message at render, feed `innerHTML`, need reactive MdContext (emoji/mentions). Deterministic but **chatty per-render + context-coupled** — IPC round-trip per message is a net loss. **Keep** (spec's explicit "don't cross IPC per-render" rule). |
| **Web-dual / no-Rust-on-web** | `pullBackfill` (WASM `feed_line`), the `invoke`/`ensureWasm`/WASM-vs-Tauri abstraction, `isWeb`/`IS_TAURI` branches | the client also ships as a **web WASM build with no Rust backend**. This code is the web path. **Cannot move.** |
| **DOM / lifecycle / WebRTC** | voice web path (`onWebrtcOffer`/`onLiveKitOffer`/`getUserMedia`/`attachVideo`/`applyDeafen`), composer DOM extraction (`pasteFiles`/`dropFiles` read clipboard/drag events), nav (`goto`), popover geometry | bound to `window`/DOM/WebRTC/component lifecycle. **Cannot move.** (Native desktop voice is already in Rust — `voice_native.rs`.) |
| **localStorage caches** | layout cache, DM list, notif prefs, sync cursor, theme, email-nudge, creds | already persist across reloads; not shared across devices. Moving to a Rust file = IPC for zero benefit. **Keep (YAGNI).** |

## → Move to Rust (candidates — need approval)

Both are **desktop-only wins** (the web build must keep its `fetch`, since there's no Rust there —
the existing `invoke` abstraction would branch: Tauri command on desktop, `fetch` on web).

| # | Method | Target surface | Benefit | Cost / caveat |
|---|---|---|---|---|
| M1 | `media.unfurl(url)` | `#[tauri::command] async unfurl(url) -> LinkPreview \| null` | bearer token never exposed in JS; no webview CORS/media-base dependence; Rust reuses the connection's host | must keep the web `fetch` path; two impls of one call |
| M2 | `media.upload(file)` | `#[tauri::command] async upload_media(bytes/path) -> UploadResult` | same (no media-base config, no cross-origin fetch from webview); on desktop a file-drop already gives a **path**, so bytes needn't cross IPC | for picker/clipboard uploads the bytes DO cross IPC once (webview→Rust); web keeps `fetch` |

**Recommendation:** M1 is the cleaner win (small JSON, clear security benefit). M2 is worth it only
if the desktop media-base friction is actually biting; otherwise defer (YAGNI). Both are optional.

## → Convert to Rust-dispatched event (candidate — need approval)

| # | Method | Proposal | Payload |
|---|---|---|---|
| E1 | `attemptReconnect` (TS backoff loop) | Move the reconnect backoff into Rust `run_connection`: on transport close (not manual logout), Rust retries with backoff using creds retained in `Conn` state, and **emits a `connection-state` event**. TS drops the loop → just `listen("connection-state")` → set `ui.reconnecting` / `store.session.status`. | `{ state: "connecting"\|"online"\|"reconnecting"\|"closed", attempt?: number }` |

Benefit: the connection lifecycle (already Rust-owned via `run_connection`) stops being half-driven
by a TS timer; one source of truth. Caveat: Rust must retain last creds (extend `Conn`), and the
**web/WASM build still needs its own TS reconnect** (no Rust) — so this is desktop-only + web keeps
the loop. That dual-path cost makes E1 **borderline**; I lean *defer* unless you want it.

## → Split (mixed methods) — worth it?

Most `mixed` methods are UI-orchestration that merely also touch persistence or fire a verb; the
"pure" half is trivial (a slug regex, a localStorage line), so splitting them adds indirection for
no real separation (anti-KISS). The only ones with a genuinely reusable pure half:

| Method | Pure half (could share) | UI half (stays) | Verdict |
|---|---|---|---|
| `doRename` (ChannelSettings) | slug validation | `weft.channelRename` + modal state | pure half too small — **keep whole** |
| `toggleTheme` | — | dom dataset + localStorage + state | **keep** |
| `saveProfile`/`onAvatarPicked` | — | diff + upload + local pending | **keep** |

**Verdict: no split is worth doing.** (Flagging explicitly rather than manufacturing splits.)

## Proposed new IPC surface (if M1/M2/E1 approved)

- **Commands:** `unfurl(url) -> Option<LinkPreview>` (M1); `upload_media(...) -> UploadResult` (M2).
  Both desktop-only; the `invoke` wrapper branches to `fetch` on web.
- **Events:** `connection-state { state, attempt? }` (E1).
- **Managed state:** extend `Conn` to retain `lastCreds` for Rust-side reconnect (E1).
- **Channels:** none needed.
- Payload types (`LinkPreview`, `UploadResult`) already exist in TS (`media.ts`) — mirror as serde
  structs; no new codegen dep.

## Bottom line for your approval

The disciplined answer is that **very little should move** — the architecture already put the
protocol in Rust, and the rest is reactive-UI/web-dual/render code that belongs in the webview.
The only real candidates are **M1 (unfurl → Rust)**, **M2 (upload → Rust, optional)**, and
**E1 (reconnect → Rust event, borderline)**. Approve/veto each individually.

**STOP — Phase 3a analysis complete. Awaiting per-item approval before any 3b implementation.**

---

# Phase 4 — file-structure convention (definitions → classes → operations → events) + manager classes

Convention: each domain file ordered **definitions → classes(with methods) → operations → events**;
stateful domains get a manager/store class holding the collection + its operations as methods;
stateless-utility domains (rendering, navigation, transport, media, ui-helpers) get **ordering only**
(free functions kept — no empty wrapper classes).

**Pilot ✅ `channels/channel.svelte.ts` — green (check 0/0, build ✓), 18 files changed.**
Introduced `class ChannelStore` (fields `channels`/`layoutCache`/`pendingChanCreate` + methods
get/ensure/markRead/reset/short/moveChannel/moveCategory/setCategories/nsCategories/cacheNsCats/
cacheChanLayout/loadLayout/saveLayout(private)/reconcileCreate/persistDms/restoreDms/dmStoreKey(private))
+ `export const channelStore`. Free ops kept: `nsOf`, `scopesFor`. Consumers: `ensureChannel(x)`→
`channelStore.ensure(x)`, `channels[x]`→`channelStore.channels[x]`, etc. AppCtx provideApp wrappers
rebound (`markRead`/`chanShort`/`channelRecord` → arrow wrappers, to keep `this`).

**HOLD**: awaiting Jannik's runtime verification of the pilot (module `$state`→class-field `$state`
reactivity can't be statically confirmed) before rolling out the remaining 17 domains.

**Rollout plan:**
- manager-class + reorder (stateful): roles, profile, moderation, notifications (new class);
  store, session, social, namespaces, membership, federation, invites, voice (fold ops into existing class).
- ordering only (stateless): rendering, navigation, transport, media, ui.

## Phase 4 — result (rollout done, green)

File convention (definitions → classes → operations → events) + manager classes applied.
Refined rule: **a domain gets a manager class only if it owns an interned collection**; a
single-object state domain (ui/toasts/voice) or a stateless one stays $state-object + free ops.

- **New manager class:** channels (ChannelStore), roles (RoleStore), profile (ProfileStore),
  namespaces (NsAdmin — the settings draft + admin ops; Server stays the per-instance record).
- **Folded free ops into the existing class:** federation (Federation), invites (Invites),
  social (Social), session (Session — caps machinery + permission gates).
- **No-op / already compliant:** moderation, notif, toasts, membership, store (AppStore),
  rendering, navigation, transport, media, ui, voice.

Recurring hazard handled each domain: a former free-fn passed as a callback (`onclick={x}`,
provideApp shorthand, `.some(fn)`, render props) becomes an **unbound method** that loses `this` —
TS doesn't catch it; wrapped each in an arrow. `check` 0/0 + `build` ✓ after every domain.

**Open item — voice:** 40+ operations, but they drive a single WebRTC/LiveKit session (like `ui`,
not a collection manager). Left ordering-only; converting the whole media plane to a class is
high-risk for modest benefit. Decision pending.
