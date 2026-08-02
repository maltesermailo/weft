// §6.7 moderation deny-list (mutes + bans) event handling. The cache lives on
// `store.deny` (keyed by scope); the Bans tab reads it. `mute`/`ban`
// add-or-replace; `unmute`/`unban` remove; `kick` is transient (no cache entry).
import * as weft from "$lib/transport/weft";
import { store } from "$lib/store/store.svelte";
import { sys } from "$lib/messages/messages.svelte";
import { view } from "$lib/navigation/view.svelte";
import { toast } from "$lib/notifications/toasts.svelte";
import type { HandlerMap } from "$lib/sync/handler-map";

// §6.7 moderation actions. The deny-list cache lives on `store.deny` keyed by
// the covering scope — namespace (`ns:<id>`) when a server is open, else the
// network (`*`).
export const banScope = (): string => (view.activeServer ? `ns:${view.activeServer}` : "*");
export const denyList = () => store.deny.get(banScope()) ?? [];

export function refreshBans(): void {
  // The deny cache is model-owned (client-core): clear the scope via the model,
  // then re-fetch — the MOD LIST batch (MODERATED events) repopulates it.
  weft.modRefresh(banScope()).catch(() => {});
  weft.modList(banScope()).catch((e) => toast(String(e), "error"));
}

// Issue a moderation verb (ban/unban/mute/unmute/kick). Defaults the scope to
// the active channel; a channel is required for the channel-scoped verbs.
export function moderate(verb: string, user: string, scope?: string, reason?: string): void {
  if (!user) return;
  const s = scope ?? view.active;
  if (!s) return sys("join a channel first");
  weft.moderate(verb, s, user, reason).catch((e) => toast(String(e), "error"));
}

export function liftMod(kind: string, account: string): void {
  moderate(kind === "mute" ? "unmute" : "unban", account, banScope());
}

export const moderationHandlers: HandlerMap = {
  // §6.7 the deny-list cache is model-owned (client-core): the model reduces the
  // raw MODERATED event (add-or-replace on mute/ban, remove on unmute/unban, kick
  // transient) and emits this `deny` diff with the scope's full list.
  deny: (e) => {
    store.deny.set(e.scope, e.rows);
  },
};
