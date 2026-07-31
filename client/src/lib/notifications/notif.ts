// Notification-preference reads/writes: thin resolvers over `store.notifPrefs`
// (`Channel.isMuted` / `Server.isMuted` read the same store). Kept out of the
// layout so the reducer can consult mute/level state directly.
import { store, type NotifLevel } from "$lib/store/store.svelte";
import { nsOf } from "$lib/channels/channel.svelte";
import { view } from "$lib/navigation/view.svelte";

/// The notification scope key for a channel: its namespace, or the network.
export const scopeKeyOf = (channel: string): string => {
  const ns = nsOf(channel);
  return ns ? `ns:${ns}` : "net";
};

export const notifLevel = (channel: string): NotifLevel => store.notifAt(scopeKeyOf(channel));
export const isMuted = (channel: string): boolean => store.mutedAt(scopeKeyOf(channel));
export const serverMuted = (ns: string): boolean => store.mutedAt(ns ? `ns:${ns}` : "net");
export const notifLevelOf = (scopeKey: string): NotifLevel => store.notifAt(scopeKey);
export function setNotifLevel(scope: string, level: NotifLevel): void {
  store.setNotif(scope, level);
}

// The scope the Notification Settings modal edits = the active server
// (namespace, or the network) + its display label.
export const notifScopeKey = (): string => (view.activeServer ? `ns:${view.activeServer}` : "net");
export const notifScopeLabel = (): string =>
  view.activeServer ? (store.servers.get(view.activeServer)?.displayName ?? view.activeServer) : store.session.network;
