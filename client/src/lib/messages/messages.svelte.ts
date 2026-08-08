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
import { clock, msgTime, msgEpoch } from "$lib/rendering/time";
import { channelStore } from "$lib/channels/channel.svelte";
import { isMuted } from "$lib/notifications/notif";

let msgSeq = 0;
/// Stamp a unique, monotonic render key onto a message (session-local).
export const mkMsg = (m: Omit<Msg, "key">): Msg => ({ ...m, key: msgSeq++ });

/// Post a local-only system line to the active channel (confirmations, notices).
export function sys(body: string): void {
  const active = nav.viewFrom(page.route?.id, page.params).active;
  const ch = channelStore.channels[active];
  if (ch) ch.messages.push(mkMsg({ author: "", body, time: clock(), ts: Date.now(), own: false, system: true }));
}

/// Apply a §7 REACTION delta to a message in place (`mine` tracks my own toggle so
/// the picker highlights correctly). Used by the reducer's `reaction` case as the
/// **history fallback**: the client-core store owns live-message reactions (its
/// `msg-updated` *assigns* the authoritative aggregate, overriding this for those),
/// but pre-session history messages aren't in the store's buffer, so this is the
/// only thing that reflects a reaction on them.
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

/// Map the client-core store's `CoreMsg` (the `msg-*` diff payload) to the render
/// `Msg`: derive `time`/`ts` from the id (the store carries no clock), and drop the
/// synthetic `local:<n>` id of a still-pending echo so it never becomes a real
/// msgid target (read marker / edit / react).
function toRenderMsg(m: weft.CoreMsg): Omit<Msg, "key"> {
  return {
    author: m.author,
    body: m.body,
    system: m.system || undefined,
    own: m.own,
    msgid: m.pending ? undefined : m.id,
    edited: m.edited || undefined,
    md: m.md || undefined,
    replyTo: m.reply_to ?? undefined,
    thread: m.thread ?? undefined,
    bridged: m.bridged || undefined,
    net: m.bridged ? m.network : undefined,
    attachments: m.attachments.length ? m.attachments : undefined,
    reactions: Object.keys(m.reactions).length ? m.reactions : undefined,
    label: m.label ?? undefined,
    pending: m.pending || undefined,
    time: m.pending ? clock() : (msgTime(m.id) ?? clock()),
    ts: m.pending ? Date.now() : (msgEpoch(m.id) ?? Date.now()),
  };
}

/// Update an existing render `Msg` in place from a `CoreMsg` — mutating fields
/// (never replacing the object) keeps its render `key`, so the virtualized list
/// doesn't re-mount / re-measure the row. `author`/`own`/`system`/`bridged`/`net`
/// don't change across an update, so they're left alone.
function assignRenderMsg(dst: Msg, m: weft.CoreMsg): void {
  const s = toRenderMsg(m);
  dst.body = s.body;
  dst.msgid = s.msgid;
  dst.edited = s.edited;
  dst.md = s.md;
  dst.reactions = s.reactions;
  dst.replyTo = s.replyTo;
  dst.thread = s.thread;
  dst.attachments = s.attachments;
  dst.pending = s.pending;
  dst.label = s.label;
  dst.time = s.time;
  dst.ts = s.ts;
}

/// Model-mirror handlers (client-core migration): apply the Rust messages-store
/// diffs onto `Channel.messages` (the store owns the **live tail** — append /
/// local-echo→ack reconcile / edit / react / delete for this-session messages).
/// Older history stays on the reducer's own backfill path (`histByTarget`), which
/// dedups by msgid — so the two never fight. `unread-changed` is the authoritative
/// unread tally, **display-gated TS-side** (active/muted show no badge).
export const messageMirrorHandlers: HandlerMap = {
  "unread-changed": (e) => {
    const ch = channelStore.channels[e.channel];
    if (!ch) return; // never materialize a phantom channel from a stale tally

    const active = nav.viewFrom(page.route?.id, page.params).active;
    if (e.channel === active || isMuted(e.channel)) {
      ch.markRead(); // active/muted stay silent
      return;
    }

    ch.unreadCount = e.count;
    ch.unread = e.count > 0;

    ch.mentionCount = e.mentions;
    ch.mention = e.mentions > 0;
  },
  "msg-appended": (e) => {
    const ch = channelStore.ensure(e.channel);
    // Dedup a real msgid already present (e.g. also carried by a history page).
    if (!e.msg.pending && ch.messages.some((m) => m.msgid === e.msg.id)) return;

    ch.messages.push(mkMsg(toRenderMsg(e.msg)));
  },
  "msg-updated": (e) => {
    const ch = channelStore.channels[e.channel];
    if (!ch) return;

    // Match by the current server msgid, or (reconciling a pending echo whose
    // store id is a synthetic local:<n>) by the shared optimistic label.
    const m = ch.messages.find(
      (x) => (!!x.msgid && x.msgid === e.id) || (x.pending && !!e.msg.label && x.label === e.msg.label),
    );

    if (m) assignRenderMsg(m, e.msg);
    else if (!ch.messages.some((x) => x.msgid === e.msg.id)) ch.messages.push(mkMsg(toRenderMsg(e.msg)));
  },
  "msg-removed": (e) => {
    const ch = channelStore.channels[e.channel];
    if (ch) ch.messages = ch.messages.filter((m) => m.msgid !== e.id);
  },
};

/// The catch-up window size (matches the history page). A channel that wasn't the
/// open one got no live body diffs, so on open we pull this many newest messages.
const WINDOW = 50;

/// M4-scope: pull the store's live-tail window for `name` and reconcile it into
/// `ch.messages` — **upsert** rows already present (catch edits / reactions that
/// landed while the channel wasn't the open one) and **append** any messages it
/// missed. Older history (TS-owned, prepended) is untouched: the store's window
/// only spans the live tail, and existing rows are matched by msgid (or a pending
/// echo by label), never duplicated.
export async function catchUpChannel(name: string): Promise<void> {
  const ch = channelStore.ensure(name);
  const range = await weft.messagesRange(name, undefined, WINDOW);

  for (const cm of range) {
    const existing = ch.messages.find(
      (m) => (!!m.msgid && m.msgid === cm.id) || (cm.pending && m.pending && !!cm.label && m.label === cm.label),
    );

    if (existing) assignRenderMsg(existing, cm);
    else ch.messages.push(mkMsg(toRenderMsg(cm)));
  }
}

/// §6.4 pin wire-event handlers: keep the channel's `pinnedIds` current and, if
/// the Pins panel is open on that channel, refresh/prune it.
export const deliveryHandlers: HandlerMap = {
  // Framework §7a: mark the author's own message as not delivered. It stays in
  // place — it IS stored here, and history will serve it — but it stops claiming
  // the realm has it.
  undelivered: (e) => {
    const ch = channelStore.channels[e.channel];
    const m = ch?.messages.find((x) => x.msgid === e.msgid);
    if (!m) return;

    m.failed = true;
    m.failReason = e.reason ?? "the bridge did not confirm delivery";
  },
};

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
