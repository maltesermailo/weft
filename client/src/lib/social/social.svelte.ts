// The client domain model — see docs/architecture/client-model-refactor.md.
import { SvelteMap } from "svelte/reactivity";
import { goto } from "$app/navigation";
import type { HandlerMap } from "$lib/sync/handler-map";
import { store } from "$lib/store/store.svelte";
import * as weft from "$lib/transport/weft";
import { channels, ensureChannel } from "$lib/channels/channel.svelte";
import { toast } from "$lib/notifications/toasts.svelte";
import { friendLabel, peerOf } from "$lib/profile/profile.svelte";
import { connectCallMedia, disconnectCallMedia } from "$lib/voice/callmedia.svelte";
import { view } from "$lib/navigation/view.svelte";

// Friend/group roster views, derived from the interned friends+groups maps.
// Exposed as a getter object (Svelte 5 can't export a `$derived` binding) so
// components read the live value directly instead of routing through AppCtx.
const _friends = $derived(
  [...store.social.friends]
    .filter(([, s]) => s === "friends")
    .map(([u]) => u)
    .sort((a, b) => friendLabel(a).localeCompare(friendLabel(b))),
);
const _incoming = $derived(
  [...store.social.friends].filter(([, s]) => s === "incoming").map(([u]) => u).sort(),
);
const _outgoing = $derived(
  [...store.social.friends].filter(([, s]) => s === "outgoing").map(([u]) => u).sort(),
);
const _groups = $derived([...store.social.groups.keys()]);

export const roster = {
  get friends() { return _friends; },
  get incoming() { return _incoming; },
  get outgoing() { return _outgoing; },
  get groups() { return _groups; },
};

/// A friend ref's local account handle (bare, no `@network`), or null when the
/// friend lives on another network — cross-network DMs are out of scope.
export function friendLocalAccount(user: string): string | null {
  const [acct, net] = user.split("@");
  return net === store.session.network ? acct : null;
}

/// Fully-qualify a typed handle to `account@network` (local network default).
export function qualify(handle: string): string {
  const h = handle.trim().replace(/^@/, "");
  return h.includes("@") ? h : `${h}@${store.session.network}`;
}

export function acceptFriend(user: string): void {
  weft.friendAccept(user).catch((e) => toast(String(e), "error"));
}

// Start a 1:1 call. Calls are a friends-only feature — the single gate behind
// every call entry point (context menu, topbar, profile).
export function callUser(user: string): void {
  if (store.social.activeCall) return; // already in a call
  if (store.social.friends.get(qualify(user)) !== "friends") {
    toast("You can only call friends", "error");
    return;
  }
  weft.call(user).catch((e) => toast(String(e), "error"));
}
// Unfriend / cancel an outgoing request / decline an incoming one.
export function removeFriend(user: string): void {
  weft.friendRemove(user).catch((e) => toast(String(e), "error"));
}

// ---- group-DM friend picker (NewGroupModal) ----
export const groupPicker = $state<{ open: boolean; seed: string; pos: { left: number; top: number } | null }>({
  open: false,
  seed: "",
  pos: null,
});

const GROUP_POP_W = 300;

// Open the friend picker to grow a DM into a group. From a DM, seed the current
// peer; from the Friends view (no active peer), open seedless. Anchor under the
// button that opened it, right-aligned so it stays on-screen.
export function openGroupPicker(e?: MouseEvent): void {
  groupPicker.seed = view.active.startsWith("@") ? qualify(peerOf(view.active)) : "";

  if (e?.currentTarget instanceof HTMLElement) {
    const r = e.currentTarget.getBoundingClientRect();
    const left = Math.max(8, Math.min(r.right - GROUP_POP_W, window.innerWidth - GROUP_POP_W - 8));
    const top = Math.min(r.bottom + 8, window.innerHeight - 120);
    groupPicker.pos = { left, top };
  } else {
    groupPicker.pos = null;
  }

  groupPicker.open = true;
}

export function createGroupWith(members: string[]): void {
  groupPicker.open = false;
  const uniq = [...new Set(members.map((m) => qualify(m)).filter((m) => m.includes("@")))];
  if (uniq.length < 2) return; // the peer + at least one more
  weft.groupCreate(uniq).catch((e) => toast(String(e), "error"));
}

export function leaveGroup(id: string): void {
  weft.groupLeave(id).catch((e) => toast(String(e), "error"));
}

/// A group DM's display label: its name, else the member handles (minus self).
export function groupLabel(id: string): string {
  const g = store.social.groups.get(id);
  if (!g) return "Group";
  if (g.name) return g.name;

  const me = `${store.session.account}@${store.session.network}`;
  const others = g.members.filter((m) => m !== me).map((m) => friendLabel(m));
  return others.length ? others.join(", ") : "Group";
}

/// My friendship state with a (possibly bare) handle.
export function friendState(handle: string): "friends" | "incoming" | "outgoing" | "none" {
  return (store.social.friends.get(qualify(peerOf(handle))) as "friends" | "incoming" | "outgoing") ?? "none";
}
/// Act on a friendship from the profile surfaces: add / accept / remove.
export function friendAction(handle: string, action: "add" | "accept" | "remove"): void {
  const ref = qualify(peerOf(handle));
  if (action === "add") weft.friendAdd(ref).catch((e) => toast(String(e), "error"));
  else if (action === "accept") acceptFriend(ref);
  else removeFriend(ref);
}

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
