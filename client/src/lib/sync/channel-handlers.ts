// Channel/namespace-membership + channel-meta wire-event handlers. These
// orchestrate across domains (roster + caps + profile + navigation), so they
// live in a sync-layer handler module rather than on the Channel model itself —
// keeping the model free of nav/caps/profile imports (and cycles).
import { goto } from "$app/navigation";
import * as nav from "$lib/nav";
import * as weft from "$lib/weft";
import type { HandlerMap } from "$lib/sync/handler-map";
import { store } from "$lib/models/store.svelte";
import { channels, ensureChannel, cacheChanLayout, reconcileChannelCreate } from "$lib/models/channel.svelte";
import { ensureCaps } from "$lib/models/session.svelte";
import { queryProfile } from "$lib/profile.svelte";
import { selectServer } from "$lib/navigation";
import { syncState } from "$lib/connection.svelte";
import { view } from "$lib/view.svelte";
import { ui } from "$lib/ui.svelte";
import { confirmSuccess } from "$lib/toasts.svelte";

const me = (): string => store.session.account;

export const channelHandlers: HandlerMap = {
  member: (e) => {
    const ch = ensureChannel(e.channel);
    // Roster only — the "joined"/"left" line is a persistent system MESSAGE.
    if (e.action === "join") {
      if (!ch.members.some((m) => m.name === e.user))
        ch.members.push({ name: e.user, origin: e.network === store.session.network ? "local" : "federated" });
      ensureCaps(e.user, e.channel); // roster badge
      queryProfile(e.user); // §10.3 display name + avatar
      if (e.user === me()) {
                // Jump to a just-joined channel only when browsing a server (not the
        // Friends/DMs home) — keeps startup auto-rejoins from yanking the view.
        if (!view.active && !view.homeView) goto(nav.pathFor(e.channel));
        weft.presence(store.session.myStatus).catch(() => {}); // re-announce to the new channel
      } else {
        store.accountOf(e.user).presence ??= "online"; // best-effort online mark
      }
    } else {
      ch.members = ch.members.filter((m) => m.name !== e.user);
      if (e.user === me()) {
        delete channels[e.channel];
        if (view.active === e.channel) goto(nav.pathFor(Object.keys(channels)[0] ?? ""));
      }
    }
  },
  "ns-member": (e) => {
    // §7.4 namespace-level join/part — tracks *my own* membership so a
    // channel-less server still shows on the rail (+ auto-selects on live join).
    if (e.user !== me()) return;
    if (e.action === "join") {
      store.server(e.namespace).joined = true;
      if (!syncState.syncing && view.activeServer !== e.namespace) selectServer(e.namespace);
    } else {
      const s = store.servers.get(e.namespace);
      if (s) s.joined = false;
    }
  },
  "chan-sync": () => {
    // §7.9 per-channel SYNC header — previews withheld in v1, nothing to apply.
  },
  chanmeta: (e) => {
    // §6.3 CHANNEL DELETE confirms with `deleted` — drop from every local view
    // (do NOT ensureChannel first, or it'd be re-created).
    if (e.key === "deleted") {
      delete channels[e.channel];
      if (view.active === e.channel) goto(nav.pathFor("", view.activeServer));
      return;
    }
    const c = ensureChannel(e.channel);
    if (e.key === "topic") c.topic = e.value;
    else if (e.key === "posting") c.restricted = e.value === "restricted";
    else if (e.key === "view-gated") c.viewGated = e.value === "true";
    else if (e.key === "category") c.category = e.value || undefined;
    else if (e.key === "position") c.position = parseInt(e.value, 10) || 0;
    if (e.key === "category" || e.key === "position") cacheChanLayout(e.channel, c.category, c.position ?? 0);
  },
  "channel-layout": (e) => {
    const ch = ensureChannel(e.channel);
    ch.category = e.category ?? undefined;
    ch.position = e.position;
    ch.voice = e.channel_kind === "voice"; // §16 render as a voice channel
    if (e.vanity) ch.vanity = e.vanity; // v0.13 display name; wire name is ids
    cacheChanLayout(e.channel, ch.category, e.position);
    reconcileChannelCreate(e.channel, e.vanity); // finish a pending create
  },
  "channel-renamed": (e) => {
    // Re-key local state to the new identity (idempotent — arrives as a
    // broadcast plus a labeled copy to the initiator).
    const cur = channels[e.old];
    if (cur) {
      cur.name = e.new;
      // The server sends no live channel-layout on rename, so the old `vanity`
      // would linger (showing the stale name until reload). Clear it → chanShort
      // falls back to the new wire name's segment; a later layout sets the real one.
      cur.vanity = undefined;
      channels[e.new] = cur; // unread/mention tallies ride the instance
      delete channels[e.old];
      cacheChanLayout(e.new, cur.category, cur.position ?? 0);
      if (view.active === e.old) goto(nav.pathFor(e.new), { replaceState: true });
      if (ui.chanPerms === e.old) ui.chanPerms = e.new;
      weft.join(e.new).catch(() => {}); // actor respawned under the new name — re-subscribe
    }
    confirmSuccess(`rename:${e.new}`);
  },
};
