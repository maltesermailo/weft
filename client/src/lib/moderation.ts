// §6.7 moderation deny-list (mutes + bans) event handling. The cache lives on
// `store.deny` (keyed by scope); the Bans tab reads it. `mute`/`ban`
// add-or-replace; `unmute`/`unban` remove; `kick` is transient (no cache entry).
import * as weft from "$lib/weft";
import { store } from "$lib/models/store.svelte";
import { sys } from "$lib/models/channel.svelte";
import { view } from "$lib/view.svelte";
import { toast } from "$lib/toasts.svelte";
import type { HandlerMap } from "$lib/sync/handler-map";

// §6.7 moderation actions. The deny-list cache lives on `store.deny` keyed by
// the covering scope — namespace (`ns:<id>`) when a server is open, else the
// network (`*`).
export const banScope = (): string => (view.activeServer ? `ns:${view.activeServer}` : "*");
export const denyList = () => store.deny.get(banScope()) ?? [];

export function refreshBans(): void {
  store.deny.set(banScope(), []); // full refresh; the batch response repopulates
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
  moderated: (e) => {
    if (e.action === "mute" || e.action === "ban") {
      const list = store.deny.get(e.scope) ?? [];
      const i = list.findIndex((r) => r.account === e.account && r.kind === e.action);
      const rec = { account: e.account, kind: e.action, by: e.by, reason: e.reason };
      store.deny.set(e.scope, i >= 0 ? list.map((r, j) => (j === i ? rec : r)) : [...list, rec]);
    } else if (e.action === "unmute" || e.action === "unban") {
      const kind = e.action === "unmute" ? "mute" : "ban";
      const cur = store.deny.get(e.scope);
      if (cur) store.deny.set(e.scope, cur.filter((r) => !(r.account === e.account && r.kind === kind)));
    }
    // `kick` is transient — no deny-list entry. Moderation is surfaced in Server
    // Settings + by the target losing access, never as a channel system line.
  },
};
