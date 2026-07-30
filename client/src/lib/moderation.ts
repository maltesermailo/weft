// §6.7 moderation deny-list (mutes + bans) event handling. The cache lives on
// `store.deny` (keyed by scope); the Bans tab reads it. `mute`/`ban`
// add-or-replace; `unmute`/`unban` remove; `kick` is transient (no cache entry).
import { store } from "$lib/models/store.svelte";
import type { HandlerMap } from "$lib/sync/handler-map";

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
