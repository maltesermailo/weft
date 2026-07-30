// Cross-cutting UI / connection-banner state that both the reducer and
// components touch. A module singleton so neither has to smuggle it through the
// AppCtx. (Grows as more modal/overlay flags migrate off the layout.)
import type { Msg } from "$lib/types";

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
  /// The message currently being replied to (composer), or null.
  replyTo: Msg | null;
  /// DISCOVER pagination cursor (null = start / exhausted).
  discoverCursor: string | null;
  /// A connection was lost and a reconnect is in flight (drives the banner).
  reconnecting: boolean;
  /// The homeserver offers §6.1 email registration (from server-info).
  serverEmailAvailable: boolean;
}>({
  chanPerms: null,
  emailBannerDismissed: false,
  nsSettingsOpen: false,
  nsTab: "overview",
  replyTo: null,
  discoverCursor: null,
  reconnecting: false,
  serverEmailAvailable: false,
});
