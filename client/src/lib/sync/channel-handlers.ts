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
    // The member LIST is model-owned (applied by the `roster` mirror handler);
    // this handler keeps the cross-domain side-effects. The "joined"/"left" line
    // is a persistent system MESSAGE, not maintained here.
    if (e.action === "join") {
      channelStore.ensure(e.channel); // ensure the instance the roster diff populates
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
    } else if (e.user === me()) {
      // Self-part = leave: drop the channel locally + navigate away. The roster
      // diff that also arrives no-ops (the instance is gone before it lands).
      delete channelStore.channels[e.channel];
      if (view.active === e.channel) goto(nav.pathFor(Object.keys(channelStore.channels)[0] ?? ""));
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
    // §6.3 CHANNEL DELETE. Removal of the local record is model-owned (applied by
    // `channelMirrorHandlers` via `chan-removed`); everything else
    // (topic/posting/view-gated/category/position) rides `chan-state`. This
    // handler owns only the nav side-effect — leave the deleted view.
    if (e.key === "deleted" && view.active === e.channel) goto(nav.pathFor("", view.activeServer));
  },
  "channel-layout": (e) => {
    // category/position/voice/vanity are model-owned (applied by chan-state);
    // this handler only finishes a pending create (subscribe + navigate).
    channelStore.reconcileCreate(e.channel, e.vanity);
  },
  "channel-renamed": (e) => {
    // The record re-key (+ vanity clear + persisted-layout re-key) is model-owned
    // (applied by `channelMirrorHandlers` via `chan-renamed`). This handler owns
    // the side-effects: navigation, the ChannelPermissions modal re-target, the
    // actor re-subscribe, and the toast. Idempotent — the event arrives as a
    // broadcast plus a labeled copy to the initiator (the guards no-op on the
    // second pass, and JOIN/toast are safe to repeat).
    if (view.active === e.old) goto(nav.pathFor(e.new), { replaceState: true });
    if (ui.chanPerms === e.old) ui.chanPerms = e.new;
    weft.join(e.new).catch(() => {}); // actor respawned under the new name — re-subscribe
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
  "chan-renamed": (e) => {
    // Re-key the Channel instance old→new — its unread/mention tallies + messages
    // ride the instance, so move it rather than recreate. Clear the stale vanity
    // (the model already did; `channelStore.short` falls back to the new slug
    // until a later layout arrives). No-op if we don't hold the old record.
    const cur = channelStore.channels[e.old];
    if (!cur) return;
    cur.name = e.new;
    cur.vanity = undefined;
    channelStore.channels[e.new] = cur;
    delete channelStore.channels[e.old];
  },
  "chan-removed": (e) => {
    delete channelStore.channels[e.name];
  },
  "cat-list": (e) => {
    // §6.3 the namespace's ordered category list — model-owned (from NS-META /
    // a drag-reorder / the cache seed). Apply onto the Server so the sidebar
    // groups + orders its category headers.
    store.server(e.ns).categories = e.categories;
  },
  roster: (e) => {
    // §6.3 the channel's member list — model-owned. Resolve each member's
    // local/federated origin here (needs the session's home network). Update only
    // an existing instance: a self-part deletes the instance first, so this then
    // no-ops instead of resurrecting a ghost.
    const ch = channelStore.channels[e.channel];
    if (!ch) return;
    ch.members = e.members.map((m) => ({
      name: m.account,
      origin: m.network === store.session.network ? "local" : "federated",
    }));
  },
  typers: (e) => {
    // §4 the channel's "currently typing" set — model-owned (the host still runs
    // the 6s fallback-expiry timer). Existing instance only.
    const ch = channelStore.channels[e.channel];
    if (ch) ch.typers = e.users;
  },
};
