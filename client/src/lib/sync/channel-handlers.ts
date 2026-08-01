// Channel/namespace-membership + channel-meta wire-event handlers. These
// orchestrate across domains (roster + caps + profile + navigation), so they
// live in a sync-layer handler module rather than on the Channel model itself —
// keeping the model free of nav/caps/profile imports (and cycles).
import { goto } from "$app/navigation";
import * as nav from "$lib/navigation/nav";
import * as weft from "$lib/transport/weft";
import type { HandlerMap } from "$lib/sync/handler-map";
import { store } from "$lib/store/store.svelte";
import { channelStore } from "$lib/channels/channel.svelte";

import { profileStore } from "$lib/profile/profile.svelte";
import { selectServer } from "$lib/navigation/navigation";
import { syncState } from "$lib/connection/connection.svelte";
import { view } from "$lib/navigation/view.svelte";
import { ui } from "$lib/ui/ui.svelte";
import { confirmSuccess } from "$lib/notifications/toasts.svelte";

const me = (): string => store.session.account;

export const channelHandlers: HandlerMap = {
  member: (e) => {
    const ch = channelStore.ensure(e.channel);
    // Roster only — the "joined"/"left" line is a persistent system MESSAGE.
    if (e.action === "join") {
      if (!ch.members.some((m) => m.name === e.user))
        ch.members.push({ name: e.user, origin: e.network === store.session.network ? "local" : "federated" });
      store.session.ensureCaps(e.user, e.channel); // roster badge
      profileStore.queryProfile(e.user); // §10.3 display name + avatar
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
        delete channelStore.channels[e.channel];
        if (view.active === e.channel) goto(nav.pathFor(Object.keys(channelStore.channels)[0] ?? ""));
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
    // §6.3 CHANNEL DELETE → drop from every local view. Everything else
    // (topic/posting/view-gated/category/position) is model-owned → applied by
    // `channelMirrorHandlers` (chan-state).
    if (e.key === "deleted") {
      delete channelStore.channels[e.channel];
      if (view.active === e.channel) goto(nav.pathFor("", view.activeServer));
    }
  },
  "channel-layout": (e) => {
    // category/position/voice/vanity are model-owned (applied by chan-state);
    // this handler only finishes a pending create (subscribe + navigate).
    channelStore.reconcileCreate(e.channel, e.vanity);
  },
  "channel-renamed": (e) => {
    // Re-key local state to the new identity (idempotent — arrives as a
    // broadcast plus a labeled copy to the initiator).
    const cur = channelStore.channels[e.old];
    if (cur) {
      cur.name = e.new;
      // The server sends no live channel-layout on rename, so the old `vanity`
      // would linger (showing the stale name until reload). Clear it → channelStore.short
      // falls back to the new wire name's segment; a later layout sets the real one.
      cur.vanity = undefined;
      channelStore.channels[e.new] = cur; // unread/mention tallies ride the instance
      delete channelStore.channels[e.old];
      channelStore.cacheChanLayout(e.new, cur.category, cur.position ?? 0);
      if (view.active === e.old) goto(nav.pathFor(e.new), { replaceState: true });
      if (ui.chanPerms === e.old) ui.chanPerms = e.new;
      weft.join(e.new).catch(() => {}); // actor respawned under the new name — re-subscribe
    }
    confirmSuccess(`rename:${e.new}`);
  },
};

/// Model-mirror handlers (client-core migration): apply the Rust `chan-state`
/// diff — the channel-metadata fields the model now owns (topic / posting /
/// view-gated / voice / vanity) — onto the local `Channel` record. Registered in
/// the reducer next to `channelHandlers`. `category`/`position` (layout) stay in
/// `channelHandlers` until the layout+persistence slice.
export const channelMirrorHandlers: HandlerMap = {
  "chan-state": (e) => {
    const ch = channelStore.ensure(e.name);
    ch.voice = e.voice;
    ch.vanity = e.vanity || undefined; // model sends "" for unset; keep TS undefined
    ch.topic = e.topic ?? undefined; // model sends null for unset
    ch.restricted = e.restricted;
    ch.viewGated = e.view_gated;
    ch.category = e.category ?? undefined; // layout — model-owned + persisted
    ch.position = e.position;
  },
};
