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
import type { Msg, Member, CtxItem, ThreadInfo, MentionOpt } from "./types";
import type { Account } from "./models/account.svelte";
import type { Channel } from "./models/channel.svelte";
import type { Server } from "./models/server.svelte";
import type { Membership } from "./models/membership.svelte";
import type { ManifestInfo } from "./models/federation.svelte";
import type { Role } from "./models/role.svelte";
import type { Badge } from "./models/session.svelte";
import type { InviteInfo } from "./models/invites.svelte";

export type RetentionMeta = { cls: string; label: string; icon: string };
export type { InviteInfo } from "./models/invites.svelte";
export type { Badge } from "./models/session.svelte";

export interface AppCtx {
  // ---- identity / connection ----

  // ---- navigation ----
  readonly homeView: boolean;
  readonly activeServer: string;
  readonly active: string;
  // ---- social layer: friends (federation-able; keys are account@network) ----
  addFriendInput: string;
  addFriend(): void;
  acceptFriend(user: string): void;
  removeFriend(user: string): void;
  // ---- group DMs (ids are `&<ulid>`) ----
  newGroupInput: string;
  /** A group's display label (its name, else member handles). */
  createGroup(): void;
  addToGroup(id: string, handle: string): void;
  // ---- group calls ----
  readonly groupCallRoster: ReadonlyMap<string, string[]>;
  readonly activeGroupCall: string | null;
  startGroupCall(id: string): void;
  leaveGroupCall(id: string): void;
  // ---- friend calls (1:1) ----
  readonly incomingCall: { from: string; room: string } | null;
  readonly activeCall: { peer: string; room: string; state: string } | null;
  readonly callMuted: boolean;
  readonly callConnecting: boolean;
  acceptCall(): void;
  declineCall(): void;
  endCall(): void;
  toggleCallMute(): void;
  goHome(): void;
  selectServer(ns: string): void;
  federate(target: string, invite?: string): void; // §11.10 join a foreign namespace on demand (invite unlocks non-public)

  // ---- data ----
  openNotifSettings(): void;
  markRead(name: string): void;

  // ---- helpers ----
  chanShort(n: string): string;
  /// User-facing label for any target — `#vanity` (channel), peer name (DM),
  /// or group label (group DM). Use anywhere a target name is shown.
  /// Am I a member of this namespace (by id)? Discover hides servers I'm in.
  nsOf(n: string): string;
  retentionMeta: Record<string, RetentionMeta>;

  // ---- context menus ----

  // ---- user actions ----

  // ---- server menu / creation ----
  openCreateChannel(prefill?: string): void;
  openCreateChannelInCat(cat: string): void;
  openNsSettings(): void;
  /** Open the per-server profile editor (your own nickname on this server). */
  openServerProfile(): void;
  newCat(): void; // open the create-category modal

  // ---- members ----
  /** Namespaces I share with `target` (from visible memberships). */
  mutualServers(target: string): string[];

  // ---- user footer ----
  openSettings(): void;

  // ---- misc shared ----
  toast(text: string, kind?: string): void;
  /// Ask the user to confirm a destructive action; resolves true if confirmed.
  confirm(message: string, label?: string): Promise<boolean>;
  /// Register a server-confirmed success toast: fires when the matching
  /// confirming event lands (not on send), so cap failures never show success.
  expectSuccess(key: string, message: string): void;

  // ---- chat topbar ----
  // Pins + search panels own their state on `store.pins` / `store.search`; these
  // just open them on the active channel (from the topbar).
  openPins(): void;
  openReports(): void;
  partActive(): void;
  openSearch(): void;


  // ---- message list / items ----
  readonly loadingHistory: string | null;
  /** A channel record by name — each kept-alive MessageList reads its own. */
  channelRecord(name: string): Channel | undefined;
  /** Fetch a channel's history page (first open / paging older). Single-flight. */
  loadHistory(channel: string, initial: boolean): void;
  /** Epoch-ms read boundary the active channel opened at (for the unread jump). */
  readonly newBoundary: number | null;
  replyTo: Msg | null;
  /** Render key of the message the "New messages" divider sits before, or null. */
  readonly newDividerKey: number | null;
  /** Resolve a `weft-media://…` reference to a fetchable URL. */
  mediaUrl(ref: string): string;

  // ---- roles (ProfileCard) ----
  /// Role definitions at a scope — ns roles from the `Server`, `*`/`#chan` by-scope.
  /// Resolve a role **id** to its definition at a scope (v0.13) — member rosters
  /// carry ids, so display maps through this for the name + color.

  // ---- federation (§11, operator) ----
  openFederation(): void;

  // ---- user settings ----

  // ---- user settings (page overlay) ----

  // ---- server settings (ns overlay) ----
  nsTab: "overview" | "roles" | "members" | "emoji" | "invites" | "bans" | "federation" | "recovery" | "danger";
  // §6.7 moderation deny-list (mutes + bans) for the active server.
  nsDelegSubject: string;
  /// Assign the role with this id to the typed delegation subject.
  assignRole(roleId: string): void;
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
export type { Msg, Member, CtxItem, ThreadInfo, MentionOpt };
export type { Account } from "./models/account.svelte";
export type { Channel } from "./models/channel.svelte";
export type { Server } from "./models/server.svelte";
export type { Membership } from "./models/membership.svelte";
export type { Role } from "./models/role.svelte";
