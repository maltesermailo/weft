// Cross-cutting UI / connection-banner state that both the reducer and
// components touch. A module singleton so neither has to smuggle it through the
// AppCtx. (Grows as more modal/overlay flags migrate off the layout.)
import type { Msg } from "$lib/types";

/// The User Settings overlay tabs.
export type UserTab = "account" | "appearance" | "connection" | "verification";

/// The Server Settings modal tabs (§6.2).
export type NsTab =
  | "overview"
  | "roles"
  | "members"
  | "emoji"
  | "invites"
  | "bans"
  | "federation"
  | "recovery"
  | "danger";

export const ui = $state<{
  /// The channel whose ChannelSettings (permissions) modal is open, or null.
  /// The reducer re-keys this when that channel is renamed.
  chanPerms: string | null;
  /// The §6.1 "no email on file" banner has been dismissed this session.
  emailBannerDismissed: boolean;
  /// Server Settings modal: open + active tab.
  nsSettingsOpen: boolean;
  nsTab: NsTab;
  /// User Settings overlay active tab.
  userTab: UserTab;
  /// The message currently being replied to (composer), or null.
  replyTo: Msg | null;
  /// DISCOVER pagination cursor (null = start / exhausted).
  discoverCursor: string | null;
  /// A connection was lost and a reconnect is in flight (drives the banner).
  reconnecting: boolean;
  /// The homeserver offers §6.1 email registration (from server-info).
  serverEmailAvailable: boolean;
  /// The active color theme.
  theme: "dark" | "light";
  /// The members sidebar is shown (chat topbar toggle).
  membersVisible: boolean;
  /// The operator §11 Federation panel is open.
  federationOpen: boolean;
  /// The anchored ProfileCard popover's target account (null = closed) + its
  /// on-screen position (null = centered fallback).
  profileTarget: string | null;
  profilePos: { left: number; top: number } | null;
  /// The centered full-profile modal's target account (null = closed).
  profileModalTarget: string | null;
  /// The §10.3 "Set nickname" dialog's target account (null = closed).
  nickTarget: string | null;
  /// The Notification Settings modal is open.
  notifSettingsOpen: boolean;
  /// The Discover (browse namespaces) modal is open.
  discoverOpen: boolean;
  /// The User Settings overlay is open.
  settingsOpen: boolean;
  /// The per-server profile (own nickname) editor is open.
  serverProfileOpen: boolean;
  /// The server-header dropdown menu + the user-footer dropdown menu.
  serverMenu: boolean;
  userMenu: boolean;
  /// Channel-list drag/drop (Discord-style reorder): the channel being dragged +
  /// its hovered drop anchor, and the category being dragged + its drop target.
  draggingChan: string | null;
  dropTarget: { name: string; after: boolean } | null;
  draggingCat: string | null;
  catDrop: string | null;
}>({
  chanPerms: null,
  emailBannerDismissed: false,
  nsSettingsOpen: false,
  nsTab: "overview",
  userTab: "account",
  replyTo: null,
  discoverCursor: null,
  reconnecting: false,
  serverEmailAvailable: false,
  theme: "dark",
  membersVisible: true,
  federationOpen: false,
  profileTarget: null,
  profilePos: null,
  profileModalTarget: null,
  nickTarget: null,
  notifSettingsOpen: false,
  discoverOpen: false,
  settingsOpen: false,
  serverProfileOpen: false,
  serverMenu: false,
  userMenu: false,
  draggingChan: null,
  dropTarget: null,
  draggingCat: null,
  catDrop: null,
});

/// Flip + persist the color theme.
export function toggleTheme(): void {
  ui.theme = ui.theme === "dark" ? "light" : "dark";
  document.documentElement.dataset.theme = ui.theme;
  try {
    localStorage.setItem("weft:theme", ui.theme);
  } catch {
    /* storage unavailable */
  }
}
