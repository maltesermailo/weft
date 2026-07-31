// Message operations split out of the channels model: render-key stamping,
// local system lines, §7 reaction deltas, and the §6.4 pin wire-event handlers.
// Depends on the channels model (channel store + channelStore.ensure) — one direction,
// channels never imports messages.
import { page } from "$app/state";
import type { Msg } from "$lib/types";
import type { HandlerMap } from "$lib/sync/handler-map";
import { store } from "$lib/store/store.svelte";
import * as weft from "$lib/transport/weft";
import * as nav from "$lib/navigation/nav";
import { clock } from "$lib/rendering/time";
import { channelStore } from "$lib/channels/channel.svelte";

let msgSeq = 0;
/// Stamp a unique, monotonic render key onto a message (session-local).
export const mkMsg = (m: Omit<Msg, "key">): Msg => ({ ...m, key: msgSeq++ });

/// Post a local-only system line to the active channel (confirmations, notices).
export function sys(body: string): void {
  const active = nav.viewFrom(page.route?.id, page.params).active;
  const ch = channelStore.channels[active];
  if (ch) ch.messages.push(mkMsg({ author: "", body, time: clock(), ts: Date.now(), own: false, system: true }));
}

/// Apply a §7 REACTION/REACTIONS delta to a message in place (`mine` tracks my
/// own toggle so the picker highlights correctly).
export function applyReaction(m: Msg, emoji: string, op: string, by: string): void {
  m.reactions ??= {};
  const cur = m.reactions[emoji] ?? { count: 0, mine: false };
  if (op === "add") {
    cur.count += 1;
    if (by === store.session.account) cur.mine = true;
  } else {
    cur.count -= 1;
    if (by === store.session.account) cur.mine = false;
  }
  if (cur.count <= 0) delete m.reactions[emoji];
  else m.reactions[emoji] = cur;
}

/// §6.4 pin wire-event handlers: keep the channel's `pinnedIds` current and, if
/// the Pins panel is open on that channel, refresh/prune it.
export const pinsHandlers: HandlerMap = {
  pinned: (e) => {
    const ch = channelStore.ensure(e.channel);
    ch.pinnedIds = [...(ch.pinnedIds ?? []).filter((id) => id !== e.msgid), e.msgid];
    const active = nav.viewFrom(page.route?.id, page.params).active;
    if (store.pins.open && active === e.channel) weft.pins(e.channel).catch(() => {});
  },
  unpinned: (e) => {
    const ch = channelStore.channels[e.channel];
    if (ch) ch.pinnedIds = (ch.pinnedIds ?? []).filter((id) => id !== e.msgid);
    const active = nav.viewFrom(page.route?.id, page.params).active;
    if (store.pins.open && active === e.channel)
      store.pins.list = store.pins.list.filter((m) => m.msgid !== e.msgid);
  },
};
