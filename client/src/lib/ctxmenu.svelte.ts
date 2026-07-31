// Right-click context-menu builders. Each maps a target (message / channel /
// user / group / category / roster row / list background) to a `CtxItem[]` and
// opens the menu. Pure orchestration over the domain modules — the last big
// block of the old layout container. `ctxMenu` holds the open menu; the layout
// renders it and closes it (Escape / outside click).
import type { Msg, CtxItem } from "$lib/types";
import type { Channel } from "$lib/models/channel.svelte";
import { view } from "$lib/view.svelte";
import { ui } from "$lib/ui.svelte";
import { store } from "$lib/models/store.svelte";
import * as weft from "$lib/weft";
import { toast } from "$lib/toasts.svelte";
import { channels, chanShort, markRead, scopesFor } from "$lib/models/channel.svelte";
import { isMuted, setNotifLevel, scopeKeyOf } from "$lib/notif";
import { openInviteCreate, openInvites } from "$lib/models/invites.svelte";
import { canModerate, canModDelete } from "$lib/models/session.svelte";
import { openChanPerms } from "$lib/chanperms";
import { appConfirm } from "$lib/confirm.svelte";
import { openThread } from "$lib/models/threads.svelte";
import { togglePin, openReport, startEdit, doDelete } from "$lib/composer.svelte";
import { openProfile, openFullProfile, openNickDialog, peerOf } from "$lib/profile.svelte";
import { qualify, acceptFriend, removeFriend, callUser, leaveGroup } from "$lib/models/social.svelte";
import { openDm, closeDm, dmKeyFor } from "$lib/navigation";
import { moderate, banScope, liftMod, denyList } from "$lib/moderation";
import { openCreateChannel, openCreateChannelInCat, openCreateCategory, deleteCategory } from "$lib/channelcreate.svelte";

const activeChannel = () => channels[view.active];

let current = $state<{ x: number; y: number; items: CtxItem[] } | null>(null);
export const ctxMenu = {
  get current() {
    return current;
  },
  close() {
    current = null;
  },
};

export function openCtx(e: MouseEvent, items: CtxItem[]): void {
  e.preventDefault();
  e.stopPropagation(); // don't let a channel/category menu bubble to the list background
  // Raw click point; ContextMenu clamps both axes to the viewport once it can measure.
  current = { x: e.clientX, y: e.clientY, items };
}

export function msgCtx(e: MouseEvent, m: Msg): void {
  if (!m.msgid) return; // nothing actionable without a real msgid
  const mod = canModDelete();
  // System (join/part) lines carry a msgid and are deletable — by the person
  // they're about (its author) or a moderator with delete-any. No other actions.
  if (m.system) {
    if (m.own || mod) openCtx(e, [{ label: "Delete", icon: "delete", danger: true, run: () => doDelete(m) }]);
    return;
  }

  const items: CtxItem[] = [{ label: "Reply", run: () => (ui.replyTo = m) }];
  if (view.active.startsWith("#")) {
    items.push({ label: "Reply in thread", run: () => openThread(m) });
    items.push({ label: activeChannel()?.pinnedIds?.includes(m.msgid) ? "Unpin" : "Pin", run: () => togglePin(m) });
  }
  items.push({ label: "Copy text", run: () => navigator.clipboard?.writeText(m.body) });
  // The full msgid (`network/ULID`) — what HISTORY, reports + the admin lookup take.
  items.push({
    label: "Copy message ID",
    run: () => {
      navigator.clipboard?.writeText(m.msgid!);
      toast("Message ID copied", "ok");
    },
  });
  if (m.own) {
    items.push({ label: "Edit", run: () => startEdit(m) });
    items.push({ label: "Delete", danger: true, run: () => doDelete(m) });
  } else {
    if (mod) items.push({ label: "Delete", icon: "delete", danger: true, run: () => doDelete(m) });
    items.push({ label: "Report", run: () => openReport(m) });
  }
  openCtx(e, items);
}

export function chanCtx(e: MouseEvent, ch: Channel): void {
  const muted = isMuted(ch.name);
  const items: CtxItem[] = [
    { header: `#${chanShort(ch.name)}` },
    { label: "Mark as read", icon: "markread", run: () => markRead(ch.name) },
    {
      label: muted ? "Unmute channel" : "Mute channel",
      icon: muted ? "unmute" : "mute",
      run: () => setNotifLevel(scopeKeyOf(ch.name), muted ? "mentions" : "nothing"),
    },
    { label: "Copy name", icon: "copy", run: () => navigator.clipboard?.writeText(ch.name) },
    { label: "Create invite", icon: "invite", run: () => openInviteCreate(scopesFor()[0]) },
  ];
  // Channel administration is a moderator surface — hidden from non-moderators.
  if (canModerate(ch.name)) {
    items.push(
      { divider: true },
      { header: "Mod Menu", mod: true },
      { label: "Edit permissions", icon: "permissions", run: () => openChanPerms(ch.name) },
      { divider: true },
      {
        label: "Delete channel",
        icon: "delete",
        danger: true,
        run: async () => {
          const name = chanShort(ch.name);
          if (!(await appConfirm(`Delete #${name}? This can't be undone.`, "Delete"))) return;
          weft.channelDelete(ch.name).catch((err) => toast(String(err), "error"));
        },
      },
    );
  }
  openCtx(e, items);
}

// The right-click menu for any user, anywhere (member list, friends, DMs).
export function userCtx(e: MouseEvent, name: string): void {
  if (peerOf(name) === store.session.account) return; // no menu on yourself
  const ref = qualify(name);
  const rel = store.social.friends.get(ref);
  const items: CtxItem[] = [
    { label: "Open profile", icon: "profile", run: () => openFullProfile(name) },
    view.active === dmKeyFor(name)
      ? { label: "Close DM", icon: "close", run: () => closeDm(name) }
      : { label: "Message", icon: "message", run: () => openDm(name) },
  ];
  // Calling is a friends-only action.
  if (rel === "friends") items.push({ label: "Call", icon: "call", run: () => callUser(ref) });
  // §10.3 per-namespace nickname — only meaningful inside a server.
  if (view.activeServer) items.push({ label: "Set nickname", icon: "nick", run: () => openNickDialog(name) });

  if (rel === "friends")
    items.push({ label: "Remove friend", icon: "removefriend", danger: true, run: () => removeFriend(ref) });
  else if (rel === "incoming")
    items.push({ label: "Accept friend request", icon: "accept", run: () => acceptFriend(ref) });
  else if (rel === "outgoing")
    items.push({ label: "Cancel friend request", icon: "cancel", run: () => removeFriend(ref) });
  else
    items.push({
      label: "Add friend",
      icon: "addfriend",
      run: () => weft.friendAdd(ref).catch((err) => toast(String(err), "error")),
    });

  // Invite + moderation only make sense on a server member — i.e. viewing a channel.
  if (view.active.startsWith("#")) {
    items.push({ divider: true });
    items.push({ label: "Invite to server", icon: "invite", run: () => openInvites() });
    if (canModerate(view.active)) {
      items.push({ header: "Mod Menu", mod: true });
      // Mute/ban at the namespace scope (server-wide, in Server Settings → Bans);
      // kick is per-channel (force-parts the active channel).
      items.push({ label: "Mute", icon: "mute", run: () => moderate("mute", name, banScope()) });
      items.push({ label: "Kick", icon: "kick", run: () => moderate("kick", name) });
      items.push({ label: "Ban", icon: "ban", danger: true, run: () => moderate("ban", name, banScope()) });
    }
  }
  openCtx(e, items);
}

// The right-click menu for a group DM (in the DM list).
export function groupCtx(e: MouseEvent, id: string): void {
  openCtx(e, [
    { label: "Mark as read", icon: "markread", run: () => markRead(id) },
    { label: "Copy group ID", icon: "copy", run: () => navigator.clipboard?.writeText(id) },
    { label: "Leave group", icon: "leave", danger: true, run: () => leaveGroup(id) },
  ]);
}

// Right-click a member row in the Server-Settings directory → namespace-scoped
// moderation (mute/ban key on `ns:<server>`; kick has no place on a roster).
export function nsMemberCtx(e: MouseEvent, target: string): void {
  e.preventDefault();
  const scope = banScope();
  const deny = denyList();
  const muted = deny.some((d) => d.account === target && d.kind === "mute");
  const banned = deny.some((d) => d.account === target && d.kind === "ban");
  const items: CtxItem[] = [{ label: "Open profile", icon: "profile", run: () => openProfile(target) }];

  if (target !== store.session.account) {
    items.push({ divider: true });
    items.push({ header: "Moderation", mod: true });
    items.push(
      muted
        ? { label: "Unmute", icon: "mute", run: () => liftMod("mute", target) }
        : { label: "Mute", icon: "mute", run: () => moderate("mute", target, scope) },
    );
    items.push(
      banned
        ? { label: "Unban", icon: "ban", run: () => liftMod("ban", target) }
        : { label: "Ban", icon: "ban", danger: true, run: () => moderate("ban", target, scope) },
    );
  }
  openCtx(e, items);
}

export function catCtx(e: MouseEvent, cat: string): void {
  const items: CtxItem[] = [
    { label: "Create channel", icon: "channel", run: () => openCreateChannelInCat(cat) },
    { label: "Create category", icon: "folder", run: openCreateCategory },
  ];
  // The bare top-level group ("") is implicit (uncategorized) — not deletable.
  if (cat !== "") {
    items.push({ divider: true });
    items.push({ label: "Delete category", icon: "delete", danger: true, run: () => deleteCategory(cat) });
  }
  openCtx(e, items);
}

// Right-click the empty channel-list background (Discord-style) → create.
export function listCtx(e: MouseEvent): void {
  if (!view.activeServer) return;
  openCtx(e, [
    { label: "Create channel", icon: "channel", run: () => openCreateChannel() },
    { label: "Create category", icon: "folder", run: openCreateCategory },
  ]);
}
