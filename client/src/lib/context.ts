// Shared app context (Svelte 5 + context API).
//
// `+page.svelte` is the single stateful container. It builds an `AppCtx`
// object — reactive state exposed via getters/setters, actions + helpers as
// plain function refs (they close over the container's state) — and provides
// it with `setContext(APP, ctx)`. Components read it with `getApp()`.
//
// The interface grows as components are extracted; keep it in sync with the
// object built in the container (TypeScript enforces that the container
// provides everything components consume).

import { getContext, setContext } from "svelte";
import type { Channel, Msg, Member, CtxItem, RoleDefC, ThreadInfo, MentionOpt, MemberInfoC } from "./types";

export type RetentionMeta = { cls: string; label: string; icon: string };
/** One live invite in the Discord-style invites menu (§6.5). */
export type InviteInfo = {
  scope: string;
  invite_id: string;
  creator: string;
  uses_left: number | null;
  used: number;
  expiry: number | null;
};
export type Badge = { owner: boolean; mod: boolean; list: string[] };

export interface AppCtx {
  // ---- identity / connection ----
  readonly network: string;
  readonly account: string;
  readonly myStatus: string;

  // ---- navigation ----
  readonly homeView: boolean;
  readonly activeServer: string;
  readonly active: string;
  readonly activeChannel: Channel | undefined;
  readonly activeIsDm: boolean;
  readonly activeIsGroup: boolean;
  readonly serverNamespaces: string[];
  /// A namespace id's display name (its vanity / title, v0.13) — rail tiles and
  /// headers key by id but show this.
  serverName(nsId: string): string;
  readonly channelGroups: { category: string; list: Channel[] }[];
  readonly dmList: Channel[];
  readonly activeNsMeta:
    | {
        id?: string;
        /// The active namespace's vanity display name (v0.13).
        name?: string;
        title?: string | null;
        owner?: string | null;
        recovery_eta?: number | null;
        recovery_rung?: number | null;
        visibility?: string;
        federation?: boolean;
        welcome?: string | null;
      }
    | undefined;
  // ---- social layer: friends (federation-able; keys are account@network) ----
  readonly friendList: string[];
  readonly incomingRequests: string[];
  readonly outgoingRequests: string[];
  addFriendInput: string;
  /** Short label for a friend: bare handle if local, full ref if federated. */
  friendLabel(user: string): string;
  /** The local account handle for a friend, or null if they're federated. */
  friendLocalAccount(user: string): string | null;
  addFriend(): void;
  acceptFriend(user: string): void;
  removeFriend(user: string): void;
  messageFriend(user: string): void;
  openFriends(): void;
  // ---- group DMs (ids are `&<ulid>`) ----
  readonly groupList: string[];
  newGroupInput: string;
  /** A group's display label (its name, else member handles). */
  groupLabel(id: string): string;
  createGroup(): void;
  /** Open the friend-picker to grow the current DM into a group. */
  openGroupPicker(e?: MouseEvent): void;
  openGroup(id: string): void;
  leaveGroup(id: string): void;
  addToGroup(id: string, handle: string): void;
  // ---- group calls ----
  readonly groupCallRoster: Record<string, string[]>;
  readonly activeGroupCall: string | null;
  startGroupCall(id: string): void;
  leaveGroupCall(id: string): void;
  // ---- friend calls (1:1) ----
  readonly incomingCall: { from: string; room: string } | null;
  readonly activeCall: { peer: string; room: string; state: string } | null;
  readonly callMuted: boolean;
  readonly callConnecting: boolean;
  callUser(user: string): void;
  acceptCall(): void;
  declineCall(): void;
  endCall(): void;
  toggleCallMute(): void;
  goHome(): void;
  selectServer(ns: string): void;
  /** Select a server tile and open its header menu (rail right-click). */
  openServerMenu(ns: string): void;
  /** §6.2 leave the active namespace (drop membership). */
  nsLeave(): void;
  open(name: string): void; // set active + mark read
  openVoice(name: string): void; // open a voice channel's stage + join the call
  openDiscover(): void;
  federate(target: string, invite?: string): void; // §11.10 join a foreign namespace on demand (invite unlocks non-public)

  // ---- data ----
  readonly channels: Record<string, Channel>;
  readonly presence: Record<string, string>;
  readonly unreadMap: Record<string, boolean>;
  readonly mentionMap: Record<string, boolean>;
  readonly unreadCount: Record<string, number>;
  readonly mentionCount: Record<string, number>;
  /** Notifications silenced for this channel (level "nothing"). */
  isMuted(channel: string): boolean;
  /** Notifications silenced for this whole server/namespace. */
  serverMuted(ns: string): boolean;
  /** Notification level for a scope key ("ns:<name>" or "net"). */
  notifLevelOf(scopeKey: string): string;
  /** Set the notification level for a scope key. */
  setNotifLevel(scopeKey: string, level: string): void;
  /** The active namespace's scope key + display label (for the modal). */
  notifScopeKey(): string;
  notifScopeLabel(): string;
  notifSettingsOpen: boolean;
  openNotifSettings(): void;
  readonly discovered: Record<
    string,
    {
      /// Immutable namespace id (v0.13) — the map is keyed by it; join/address
      /// by this, display by `name`.
      id: string;
      name: string;
      title?: string | null;
      description?: string | null;
      visibility: string;
      owner?: string | null;
      categories?: string[];
    }
  >;
  readonly discoverCursor: string | null;
  scopesFor(): string[];
  markRead(name: string): void;

  // ---- drag/drop (channel move) ----
  draggingChan: string | null;
  dropTarget: { name: string; after: boolean } | null;
  moveChannel(dragName: string, targetCat: string, anchorName?: string, after?: boolean): void;

  // ---- drag/drop (category reorder) ----
  draggingCat: string | null;
  catDrop: string | null;
  moveCategory(dragCat: string, targetCat: string): void;

  // ---- helpers ----
  initials(n: string): string;
  /** §10.3 a fetchable avatar URL for an account, or null → render initials. */
  avatarUrl(account: string): string | null;
  /** §10.3 an account's display name, falling back to the canonical handle. */
  displayName(account: string): string;
  /** §10.3 an account's free-text bio, or "" if unset. */
  bioOf(account: string): string;
  /** §10.3 an account's custom status, or "" if unset. */
  statusOf(account: string): string;
  /** Set (or clear, with "") my own custom status (§10.3). */
  setCustomStatus(text: string): void;
  /** Fetch an account's profile if not already cached (deduped). */
  queryProfile(account: string): void;
  /** §10.3 an account's server nickname at the active server, or "" if unset. */
  nickOf(account: string): string;
  /** Set a per-namespace nickname (empty clears). Own → `nick`, other → `manage-nicks`. */
  setNick(scope: string, account: string, nick: string): void;
  chanShort(n: string): string;
  peerOf(n: string): string;
  dotClass(acct: string): string;
  nsOf(n: string): string;
  badgeFor(account: string, scope: string): Badge | undefined;
  serverUnread(ns: string): boolean;
  serverMention(ns: string): boolean;
  serverMentionCount(ns: string): number;
  retentionMeta: Record<string, RetentionMeta>;

  // ---- context menus ----
  chanCtx(e: MouseEvent, ch: Channel): void;
  userCtx(e: MouseEvent, name: string): void;
  groupCtx(e: MouseEvent, id: string): void;
  catCtx(e: MouseEvent, cat: string): void;
  listCtx(e: MouseEvent): void;

  // ---- user actions ----
  closeDm(name: string): void;

  // ---- server menu / creation ----
  serverMenu: boolean;
  userMenu: boolean;
  openCreateChannel(prefill?: string): void;
  openCreateChannelInCat(cat: string): void;
  openNsSettings(): void;
  /** Open the per-server profile editor (your own nickname on this server). */
  openServerProfile(): void;
  mintInvite(): void;
  // ---- invites menu (Discord-style: list, creator, revoke) ----
  readonly invitesList: InviteInfo[];
  readonly invitesScope: string;
  openInvites(): void;
  loadNsInvites(): void;
  revokeInvite(id: string): void;
  createInvite(): void;
  inviteLinkFor(inv: InviteInfo): string;
  // ---- invite creation screen (expiry + max-uses, incl. unlimited) ----
  readonly inviteLink: string | null;
  readonly inviteId: string | null;
  readonly inviteCreateScope: string;
  generateInvite(maxUses: number | null, expiry: number | null): void;
  sendInviteDM(ref: string, link: string): void;
  newCat(): void; // open the create-category modal

  // ---- members ----
  openProfile(name: string, e?: MouseEvent): void;
  /** Open the full-profile modal (bio, status, mutual servers, actions). */
  openFullProfile(name: string): void;
  /** §10.3 open the quick "Set nickname" dialog for a member (own or other). */
  openNickDialog(name: string): void;
  /** Namespaces I share with `target` (from visible memberships). */
  mutualServers(target: string): string[];
  /** My friendship state with `name` (bare or qualified handle). */
  friendState(name: string): "friends" | "incoming" | "outgoing" | "none";
  /** Act on a friendship: add / accept a request / remove. */
  friendAction(name: string, action: "add" | "accept" | "remove"): void;
  openDm(name: string): void;
  moderate(kind: string, name: string, scope?: string, reason?: string): void;

  // ---- user footer ----
  openSettings(): void;

  // ---- misc shared ----
  toast(text: string, kind?: string): void;
  /// Register a server-confirmed success toast: fires when the matching
  /// confirming event lands (not on send), so cap failures never show success.
  expectSuccess(key: string, message: string): void;
  readonly reportQueue: Record<
    string,
    { report_id: string; msgid: string; category: string; state: string; reporter?: string | null }
  >;
  readonly pinsList: Msg[];
  readonly resolveActions: string[];

  // ---- chat topbar ----
  membersVisible: boolean;
  openPins(): void;
  openReports(): void;
  partActive(): void;

  // ---- message search (§6.4) ----
  searchOpen: boolean;
  readonly searchQuery: string;
  readonly searchScope: string;
  readonly searchResults: Msg[];
  readonly searching: boolean;
  openSearch(): void;
  runSearch(query: string): void;
  jumpToResult(m: Msg): void;

  // ---- threads (§9.4) ----
  readonly threadRoot: Msg | null;
  readonly threadMessages: Msg[];
  threadComposer: string;
  /** The active channel's messages excluding thread replies (main timeline). */
  readonly visibleMessages: Msg[];
  readonly visibleMessagesReversed: Msg[];
  /** Number of loaded replies in a message's thread (for the indicator). */
  threadCount(msgid?: string): number;
  openThread(root: Msg): void;
  closeThread(): void;
  sendThread(): void;
  // ---- threads list (§9.4): all threads in the active channel ----
  readonly threadsOpen: boolean;
  readonly threadsList: ThreadInfo[];
  openThreads(): void;
  closeThreads(): void;
  openThreadByRoot(info: ThreadInfo): void;
  /** A thread's display name (root msgid → name), if named. */
  threadNameFor(msgid?: string): string | undefined;
  /** Rename (empty string clears) the currently open thread. */
  renameThread(name: string): void;

  // ---- custom emoji (§9.4) ----
  /** The active namespace's custom emoji as {name, media ref}. */
  readonly activeEmoji: { name: string; media: string }[];
  addEmoji(name: string, media: string): void;
  removeEmoji(name: string): void;
  /** A `:name:` shortcode → image URL in the active namespace, or null. */
  emojiUrlFor(name: string): string | null;

  // ---- message list / items ----
  readonly loadingHistory: string | null;
  /** A channel record by name — each kept-alive MessageList reads its own. */
  channelRecord(name: string): Channel | undefined;
  /** Fetch a channel's history page (first open / paging older). Single-flight. */
  loadHistory(channel: string, initial: boolean): void;
  /** Epoch-ms read boundary the active channel opened at (for the unread jump). */
  readonly newBoundary: number | null;
  editingKey: number | null;
  editDraft: string;
  pickerKey: number | null;
  replyTo: Msg | null;
  startEdit(m: Msg): void;
  saveEdit(m: Msg): void;
  cancelEdit(): void;
  editKey(e: KeyboardEvent, m: Msg): void;
  doDelete(m: Msg): void;
  openReport(m: Msg): void;
  togglePin(m: Msg): void;
  toggleReaction(m: Msg, emoji: string): void;
  jumpTo(msgid?: string): void;
  msgCtx(e: MouseEvent, m: Msg): void;
  renderMd(body: string): string;
  mentionsMe(body: string): boolean;
  /** Day-bucket key (start-of-day epoch ms) for grouping messages under a date divider. */
  dayKey(ts: number): number;
  /** Human date-divider label ("Today" / "Yesterday" / "Monday, July 21, 2026"). */
  dayLabel(ts: number): string;
  /** Render key of the message the "New messages" divider sits before, or null. */
  readonly newDividerKey: number | null;

  // ---- composer ----
  composer: string;
  composerKey(e: KeyboardEvent): void;
  onComposerInput(): void;
  doSend(): void;
  pickMention(name: string): void;
  // ---- media (§13) ----
  readonly pendingAttachments: { uri: string; name: string; mime: string; thumb: string | null }[];
  attachFile(): void;
  /** Attach image/files pasted into the composer. */
  pasteFiles(e: ClipboardEvent): void;
  /** Attach files dropped onto the composer/chat area. */
  dropFiles(e: DragEvent): void;
  removeAttachment(i: number): void;
  /** Resolve a `weft-media://…` reference to a fetchable URL. */
  mediaUrl(ref: string): string;
  readonly mentionQuery: string | null;
  readonly mentionMatches: MentionOpt[];
  /** The highlighted mention row (arrow-key/hover navigable). */
  mentionIndex: number;
  /** `:emoji:` autocomplete: the current `:query`, or null. */
  readonly emojiQuery: string | null;
  readonly emojiSuggestions: { name: string; url: string | null; char?: string }[];
  /** The highlighted emoji row (arrow-key/hover navigable). */
  emojiIndex: number;
  pickEmojiSuggestion(name: string): void;
  readonly typingLabel: string;

  // ---- roles (ProfileCard) ----
  readonly rolesByScope: Record<string, RoleDefC[]>;
  rolesOf(account: string, scope: string): RoleDefC[];
  /// Resolve a role **id** to its definition at a scope (v0.13) — member rosters
  /// carry ids, so display maps through this for the name + color.
  roleById(scope: string, id: string): RoleDefC | undefined;
  ensureMemberRoles(account: string): void;
  ensureRoles(scope: string): void;
  roleScopeOf(channel: string): string;
  isOwnerAt(account: string, scope: string): boolean;
  /** The real owner of the active namespace (not merely an ns-admin holder). */
  isNsOwner(account: string): boolean;
  /** Network staff (operator) — surfaced as a "Staff" badge, never ownership. */
  isStaff(account: string): boolean;
  /** Do I hold moderation power (mute/ban/kick or owner) in this channel's
   * server? Namespaced channels never consult operator (`*`) caps — mirrors the
   * server, so an operator sees no mod tools on someone else's namespace. */
  canModerate(channel: string): boolean;
  /** Do I hold a specific capability at the active server's scope? Owner/ns-admin
   * implies all; operator (`*`) counts only at network level. The per-permission
   * gate for server-menu actions and Server Settings tabs. */
  serverCap(cap: string): boolean;
  /** Do I hold any `grant:*` delegation cap at the server scope? Gates Roles. */
  serverCanGrant(): boolean;
  /** Is Server Settings reachable — do I hold any moderation/admin cap (not just
   * plain member caps)? Individual tabs gate themselves. */
  canOpenServerSettings(): boolean;
  /** An account's highest-role color at the active namespace, or "" (default). */
  nameColor(account: string): string;
  assignRoleTo(acct: string, role: RoleDefC): void;
  unassignRoleFrom(acct: string, role: RoleDefC): void;
  /** §6.2 NS INFO MEMBERS: the moderator roster per namespace (once fetched). */
  readonly nsMembersByNs: Record<string, MemberInfoC[]>;
  /** A roster fetch is in flight. */
  readonly nsMembersLoading: boolean;
  /** Fetch the moderator roster for a namespace (cap-gated server-side). */
  fetchNsMembers(ns: string): void;
  /** Assign / unassign a namespace-scoped role in-line from the roster. */
  assignNsRole(account: string, role: string): void;
  unassignNsRole(account: string, role: string): void;
  /** Right-click a roster row → namespace-scoped moderation menu. */
  nsMemberCtx(e: MouseEvent, account: string): void;

  // ---- channel permissions (ChannelSettings modal — per-target overrides) ----
  chanNsScope(): string;
  /** A channel role / @everyone target's caps (by role name). */
  chanRoleCaps(name: string): string[];
  /** Commit a channel role / @everyone target's full cap set (upsert/delete). */
  setChanRoleCaps(name: string, color: string, caps: string[]): void;
  /** Individual-member overrides at the channel scope (direct grants). */
  chanMemberGrants(): { subject: string; caps: string[] }[];
  chanMemberCaps(account: string): string[];
  /** Commit a member override's full cap set (grant/revoke). */
  setChanMemberCaps(account: string, caps: string[]): void;
  /** Remove a whole override target (delete channel role / revoke member). */
  removeChanRole(name: string): void;
  removeChanMember(account: string): void;
  toggleRestricted(): void;
  /** §6.3 toggle view-gating: hide the channel from anyone without `view`. */
  toggleViewGated(): void;

  // ---- federation (§11, operator) ----
  readonly isOperator: boolean;
  readonly netblocks: Record<string, string | null>;
  readonly manifests: Record<
    string,
    {
      peer: string;
      version: number;
      state: string;
      channels: string[];
      history: string;
      media: string;
      typing: boolean;
    }
  >;
  openFederation(): void;
  refreshNetblocks(): void;
  netblockAdd(network: string, reason?: string): void;
  netblockRemove(network: string): void;
  bridgePropose(scope: string, peer: string, history: string, media: string, typing: boolean): void;
  bridgeAccept(peer: string, version: number): void;
  bridgeSever(peer: string): void;

  // ---- user settings ----
  readonly theme: string;
  readonly host: string;
  readonly reconnecting: boolean;
  setStatus(s: string): void;
  toggleTheme(): void;
  enrollThisDevice(): void;
  logout(): void;

  // ---- user settings (page overlay) ----
  userTab: "account" | "appearance" | "connection" | "verification";
  /** §10.5 the caller's own verification claims, keyed by kind (email/birthday). */
  readonly verifications: Record<string, { subject: string; state: string }>;

  // ---- server settings (ns overlay) ----
  nsTab: "overview" | "roles" | "members" | "emoji" | "invites" | "bans" | "federation" | "recovery" | "danger";
  // §6.7 moderation deny-list (mutes + bans) for the active server.
  denyList(): { account: string; kind: string; by?: string | null; reason?: string | null }[];
  refreshBans(): void;
  liftMod(kind: string, account: string): void;
  nsTitle: string;
  nsDesc: string;
  nsVis: string;
  newRoleName: string;
  newRoleColor: string;
  readonly newRoleCaps: string[];
  newRoleHoist: boolean;
  newRolePingable: boolean;
  toggleNewRoleCap(c: string): void;
  nsDelegSubject: string;
  nsNewOwner: string;
  nsRecM: number;
  nsRecKeys: string;
  readonly myRecoveryKey: string;
  recoveryDoc: string;
  nsRoleScope(): string;
  saveNsMeta(): void;
  nsSetFederation(open: boolean): void;
  /** §6.2 set (or clear, "") the namespace's welcome channel. */
  nsSetWelcome(channel: string): void;
  createRole(): void;
  /// Move a role up/down by its **id** (v0.13 — names aren't unique).
  moveRole(roleId: string, dir: -1 | 1): void;
  /// Persist an arbitrary role order (drag-and-drop) — a list of role **ids**.
  reorderRoles(ids: string[]): void;
  /// Apply an edit to an existing role (by id). A changed `name` renames in
  /// place, so the role keeps its members and issued caps (§6.5).
  saveRole(role: RoleDefC, patch: { name: string; color: string; caps: string[]; hoist: boolean; pingable: boolean }): void;
  /// Delete a role by its id.
  deleteRole(roleId: string): void;
  /** The implicit @everyone role's current caps at the active server. */
  everyoneCaps(): string[];
  /** Set the @everyone baseline caps ([] clears it). */
  setEveryoneCaps(caps: string[]): void;
  /// Assign the role with this id to the typed delegation subject.
  assignRole(roleId: string): void;
  showRecoveryKey(): void;
  startRecovery(): void;
  cosignRecovery(): void;
  submitRecovery(): void;
  doTransfer(): void;
  deleteNamespace(): void;
  revokeAllInvites(): void;
}

const APP = Symbol("weft-app");

export function provideApp(ctx: AppCtx): void {
  setContext(APP, ctx);
}

export function getApp(): AppCtx {
  return getContext(APP);
}

// Re-export commonly used types for component convenience.
export type { Channel, Msg, Member, CtxItem, RoleDefC, ThreadInfo, MentionOpt };
