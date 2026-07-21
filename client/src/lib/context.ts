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
import type { Channel, Msg, Member, CtxItem, RoleDefC } from "./types";

export type RetentionMeta = { cls: string; label: string; icon: string };
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
  readonly serverNamespaces: string[];
  readonly channelGroups: { category: string; list: Channel[] }[];
  readonly dmList: Channel[];
  readonly activeNsMeta:
    | {
        title?: string | null;
        recovery_eta?: number | null;
        recovery_rung?: number | null;
        visibility?: string;
        federation?: boolean;
      }
    | undefined;
  goHome(): void;
  selectServer(ns: string): void;
  open(name: string): void; // set active + mark read
  openDiscover(): void;
  federate(target: string): void; // §11.10 join a foreign namespace on demand

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

  // ---- helpers ----
  initials(n: string): string;
  /** §10.3 a fetchable avatar URL for an account, or null → render initials. */
  avatarUrl(account: string): string | null;
  /** §10.3 an account's display name, falling back to the canonical handle. */
  displayName(account: string): string;
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
  memberCtx(e: MouseEvent, name: string): void;
  catCtx(e: MouseEvent, cat: string): void;

  // ---- server menu / creation ----
  serverMenu: boolean;
  userMenu: boolean;
  openCreateChannel(prefill?: string): void;
  openCreateChannelInCat(cat: string): void;
  openNsSettings(): void;
  mintInvite(): void;
  newCat(): void; // open the create-category modal

  // ---- members ----
  openProfile(name: string, e?: MouseEvent): void;
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

  // ---- message list / items ----
  readonly loadingHistory: string | null;
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
  readonly mentionMatches: string[];
  readonly typingLabel: string;

  // ---- roles (ProfileCard) ----
  readonly rolesByScope: Record<string, RoleDefC[]>;
  rolesOf(account: string, scope: string): RoleDefC[];
  roleScopeOf(channel: string): string;
  isOwnerAt(account: string, scope: string): boolean;
  assignRoleTo(acct: string, role: RoleDefC): void;
  unassignRoleFrom(acct: string, role: RoleDefC): void;

  // ---- channel permissions (ChannelSettings modal — role-based only) ----
  chanNsScope(): string;
  chanRoleCaps(name: string): string[];
  toggleChanRoleCap(role: RoleDefC, cap: string): void;
  toggleRestricted(): void;

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
  nsTab: "overview" | "roles" | "members" | "bans" | "federation" | "recovery" | "danger";
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
  createRole(): void;
  deleteRole(name: string): void;
  assignRole(name: string): void;
  showRecoveryKey(): void;
  startRecovery(): void;
  cosignRecovery(): void;
  submitRecovery(): void;
  doTransfer(): void;
  deleteNamespace(): void;
}

const APP = Symbol("weft-app");

export function provideApp(ctx: AppCtx): void {
  setContext(APP, ctx);
}

export function getApp(): AppCtx {
  return getContext(APP);
}

// Re-export commonly used types for component convenience.
export type { Channel, Msg, Member, CtxItem, RoleDefC };
