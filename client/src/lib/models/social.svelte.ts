// The client domain model — see docs/architecture/client-model-refactor.md.
import { SvelteMap } from "svelte/reactivity";
import { goto } from "$app/navigation";
import type { HandlerMap } from "$lib/sync/handler-map";
import { store } from "./store.svelte";
import { channels, ensureChannel } from "./channel.svelte";
import { toast } from "$lib/toasts.svelte";
import { friendLabel } from "$lib/profile.svelte";
import { connectCallMedia, disconnectCallMedia } from "$lib/callmedia.svelte";
import { view } from "$lib/view.svelte";

/// A live 1:1 call: the peer userref + its LiveKit room and state.
export interface ActiveCall {
  peer: string;
  room: string;
  state: string;
}
/// An incoming call ring.
export interface IncomingCall {
  from: string;
  room: string;
}
/// A group DM: an optional name + member userrefs.
export interface GroupInfo {
  name?: string;
  members: string[];
}

/**
 * The social layer (federation-able): friends, group DMs, and calls. Users are
 * `account@network` userrefs, resolved through the Account identity map at the
 * UI boundary (e.g. `<Avatar>`). Replaces the parallel `friends` / `groups` /
 * `groupCallRoster` records + the call `$state` fields that lived in
 * `+page.svelte`.
 */
export class Social {
  /// Friend userref → relationship ("friends" | "incoming" | "outgoing").
  readonly friends = new SvelteMap<string, string>();
  /// Group DM id (`&<ulid>`) → group.
  readonly groups = new SvelteMap<string, GroupInfo>();
  /// Group call id → the userrefs currently in the call.
  readonly groupCallRoster = new SvelteMap<string, string[]>();

  /// Incoming 1:1 call ring, if any.
  incomingCall = $state<IncomingCall | null>(null);
  /// The active 1:1 call, if any.
  activeCall = $state<ActiveCall | null>(null);
  /// The group call I'm currently in, if any.
  activeGroupCall = $state<string | null>(null);
}

const meRef = (): string => `${store.session.account}@${store.session.network}`;

/// This domain's wire-event handlers (friends / group DMs / calls).
export const socialHandlers: HandlerMap = {
  friend: (e) => {
    store.social.friends.set(e.user, e.state);
    if (e.state === "incoming") toast(`Friend request from ${e.user}`, "info"); // a fresh request is worth a nudge
  },
  "friend-removed": (e) => store.social.friends.delete(e.user),
  group: (e) => {
    store.social.groups.set(e.id, { name: e.name ?? undefined, members: e.members });
    ensureChannel(e.id); // a conversation entry so it lists + holds messages
  },
  "group-member": (e) => {
    const g = store.social.groups.get(e.group);
    if (!g) return;
    const me = meRef();
    // SvelteMap values aren't deeply reactive — re-set the entry on change.
    if (e.action === "join") {
      if (!g.members.includes(e.user)) store.social.groups.set(e.group, { ...g, members: [...g.members, e.user] });
    } else if (e.user === me) {
      // If *we* left, drop the conversation.
      store.social.groups.delete(e.group);
      delete channels[e.group];
      if (view.active === e.group) goto("/");
    } else {
      store.social.groups.set(e.group, { ...g, members: g.members.filter((m) => m !== e.user) });
    }
  },
  "call-ring": (e) => {
    store.social.incomingCall = { from: e.from, room: e.room };
  },
  "call-state": (e) => {
    if (e.state === "ringing") {
      store.social.activeCall = { peer: e.user, room: "", state: "ringing" };
    } else if (e.state === "active") {
      store.social.incomingCall = null;
      store.social.activeCall = { peer: e.user, room: store.social.activeCall?.room ?? "", state: "active" };
      // Audio (LiveKit) connects on the CALL-MEDIA credential that follows.
    } else {
      if (e.state === "busy") toast(`${friendLabel(e.user)} is busy`, "info");
      else if (e.state === "declined") toast(`${friendLabel(e.user)} declined the call`, "info");
      if (store.social.incomingCall?.from === e.user) store.social.incomingCall = null;
      if (store.social.activeCall?.peer === e.user) {
        store.social.activeCall = null;
        disconnectCallMedia();
      }
    }
  },
  "call-media": (e) => {
    // Server authorized the call + minted our media credential — join LiveKit.
    void connectCallMedia(e.endpoint, e.token);
  },
  "group-call-state": (e) => {
    const roster = store.social.groupCallRoster.get(e.group) ?? [];
    const me = meRef();
    if (e.state === "active") {
      if (!roster.includes(e.user)) store.social.groupCallRoster.set(e.group, [...roster, e.user]);
      if (e.user === me) store.social.activeGroupCall = e.group;
    } else {
      const next = roster.filter((u) => u !== e.user);
      if (next.length) store.social.groupCallRoster.set(e.group, next);
      else store.social.groupCallRoster.delete(e.group);
      if (e.user === me && store.social.activeGroupCall === e.group) {
        store.social.activeGroupCall = null;
        disconnectCallMedia();
      }
    }
  },
};
