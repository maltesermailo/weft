// Right-click context-menu builders. Each maps a target (message / channel /
// user / group / category / roster row / list background) to a `CtxItem[]` and
// opens the menu. Pure orchestration over the domain modules — the last big
// block of the old layout container. `ctxMenu` holds the open menu; the layout
// renders it and closes it (Escape / outside click).
import type { Msg, CtxItem } from "$lib/types";
import type { Channel } from "$lib/channels/channel.svelte";
import { view } from "$lib/navigation/view.svelte";
import { ui } from "$lib/ui/ui.svelte";
import { store } from "$lib/store/store.svelte";
import * as weft from "$lib/transport/weft";
import { toast } from "$lib/notifications/toasts.svelte";
import { plugins } from "$lib/plugins/plugins.svelte";
import type { Surface } from "$lib/plugins/sdui";
import { channelStore, scopesFor } from "$lib/channels/channel.svelte";
import { isMuted, setNotifLevel, scopeKeyOf } from "$lib/notifications/notif";


import { openChanPerms } from "$lib/roles/chanperms";
import { appConfirm } from "$lib/ui/confirm.svelte";
import { openThread } from "$lib/messages/threads.svelte";
import { togglePin, openReport, startEdit, doDelete } from "$lib/messages/composer.svelte";
import { peerOf, profileStore } from "$lib/profile/profile.svelte";

import { openDm, closeDm, dmKeyFor, nsLeave } from "$lib/navigation/navigation";
import { moderate, banScope, liftMod, denyList } from "$lib/moderation/moderation";
import { openCreateChannel, openCreateChannelInCat, openCreateCategory, deleteCategory } from "$lib/channels/channelcreate.svelte";
import { openNsSettingsFor, openNotifSettingsFor, openServerProfileFor } from "$lib/namespaces/server.svelte";

const activeChannel = () => channelStore.channels[view.active];

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
  const mod = store.session.canModDelete();
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
  items.push(...pluginItems("context-menu", ["message"], m.msgid));
  openCtx(e, items);
}

/**
 * Plugin-declared context-menu entries (plugin-spec.md §13.1), appended below
 * the built-ins so a plugin can add to a menu without displacing what is there.
 *
 * `ctxRef` tells the plugin what it was invoked on — the msgid here — so it can
 * act without a round trip to ask.
 */
function pluginItems(surface: Surface, contexts: string[], ctxRef?: string): CtxItem[] {
  const matching = plugins
    .actionsFor(surface)
    .filter(({ action }) => contexts.includes(action.context));
  if (matching.length === 0) return [];

  return [
    { divider: true },
    ...matching.map(({ plugin, action }) => ({
      label: action.label,
      run: () => plugins.invoke(plugin, action.id, ctxRef),
    })),
  ];
}

export function chanCtx(e: MouseEvent, ch: Channel): void {
  const muted = isMuted(ch.name);
  const items: CtxItem[] = [
    { header: `#${channelStore.short(ch.name)}` },
    { label: "Mark as read", icon: "markread", run: () => channelStore.markRead(ch.name) },
    {
      label: muted ? "Unmute channel" : "Mute channel",
      icon: muted ? "unmute" : "mute",
      run: () => setNotifLevel(scopeKeyOf(ch.name), muted ? "mentions" : "nothing"),
    },
    { label: "Copy name", icon: "copy", run: () => navigator.clipboard?.writeText(ch.name) },
    { label: "Create invite", icon: "invite", run: () => store.invites.openInviteCreate(scopesFor()[0]) },
  ];
  // Channel administration is a moderator surface — hidden from non-moderators.
  if (store.session.canModerate(ch.name)) {
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
          const name = channelStore.short(ch.name);
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
  const ref = store.social.qualify(name);
  const rel = store.social.friends.get(ref);
  const items: CtxItem[] = [
    { label: "Open profile", icon: "profile", run: () => profileStore.openFullProfile(name) },
    view.active === dmKeyFor(name)
      ? { label: "Close DM", icon: "close", run: () => closeDm(name) }
      : { label: "Message", icon: "message", run: () => openDm(name) },
  ];
  // Calling is a friends-only action.
  if (rel === "friends") items.push({ label: "Call", icon: "call", run: () => store.social.callUser(ref) });
  // §10.3 per-namespace nickname — only meaningful inside a server.
  if (view.activeServer) items.push({ label: "Set nickname", icon: "nick", run: () => profileStore.openNickDialog(name) });

  if (rel === "friends")
    items.push({ label: "Remove friend", icon: "removefriend", danger: true, run: () => store.social.removeFriend(ref) });
  else if (rel === "incoming")
    items.push({ label: "Accept friend request", icon: "accept", run: () => store.social.acceptFriend(ref) });
  else if (rel === "outgoing")
    items.push({ label: "Cancel friend request", icon: "cancel", run: () => store.social.removeFriend(ref) });
  else
    items.push({
      label: "Add friend",
      icon: "addfriend",
      run: () => weft.friendAdd(ref).catch((err) => toast(String(err), "error")),
    });

  // Invite + moderation only make sense on a server member — i.e. viewing a channel.
  if (view.active.startsWith("#")) {
    items.push({ divider: true });
    items.push({ label: "Invite to server", icon: "invite", run: () => store.invites.openInvites() });
    if (store.session.canModerate(view.active)) {
      items.push({ header: "Mod Menu", mod: true });
      // Mute/ban at the namespace scope (server-wide, in Server Settings → Bans);
      // kick is per-channel (force-parts the active channel).
      items.push({ label: "Mute", icon: "mute", run: () => moderate("mute", name, banScope()) });
      items.push({ label: "Kick", icon: "kick", run: () => moderate("kick", name) });
      items.push({ label: "Ban", icon: "ban", danger: true, run: () => moderate("ban", name, banScope()) });
    }
  }
  // §13.2 a member/user action's ctx-ref is the qualified `user@net` — the
  // plugin resolves any further scope itself (a bridge's kick, for instance,
  // asks which channel, since the ref carries none).
  items.push(...pluginItems("context-menu", ["member", "user"], ref));
  openCtx(e, items);
}

// The right-click menu for a group DM (in the DM list).
export function groupCtx(e: MouseEvent, id: string): void {
  openCtx(e, [
    { label: "Mark as read", icon: "markread", run: () => channelStore.markRead(id) },
    { label: "Copy group ID", icon: "copy", run: () => navigator.clipboard?.writeText(id) },
    { label: "Leave group", icon: "leave", danger: true, run: () => store.social.leaveGroup(id) },
  ]);
}

// Right-click a member row in the Server-Settings directory → namespace-scoped
// moderation (mute/ban key on `ns:<server>`; kick has no place on a roster).
export function nsMemberCtx(e: MouseEvent, target: string): void {
  // (plugin entries are appended at the end, below the built-ins)
  e.preventDefault();
  const scope = banScope();
  const deny = denyList();
  const muted = deny.some((d) => d.account === target && d.kind === "mute");
  const banned = deny.some((d) => d.account === target && d.kind === "ban");
  const items: CtxItem[] = [{ label: "Open profile", icon: "profile", run: () => profileStore.openProfile(target) }];

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
  items.push(...pluginItems("context-menu", ["member", "user"], store.social.qualify(target)));
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

/// Right-click a rail tile. Its own menu, acting on `ns` explicitly — it used to
/// call `openServerMenu`, which *switched to* the namespace and dropped the
/// sidebar header open, so right-clicking a tile navigated you somewhere you only
/// wanted to inspect.
///
/// A **locked** namespace (provider-managed, bridge disconnected) offers only
/// Leave. Everything else here would be refused by weftd while the provider is
/// down, and leaving is purely local membership, so it still works — which is what
/// makes the tile actionable rather than a dead end.
///
/// The items are the ones that can name a namespace explicitly. Creating channels
/// and opening Server Settings stay on the sidebar header, because they act on the
/// *active* server: offering them here would have them quietly apply to whichever
/// namespace is open rather than the one you right-clicked.
export function serverCtx(e: MouseEvent, ns: string): void {
  const server = store.servers.get(ns);
  const name = server?.displayName ?? ns;
  const scope = `ns:${ns}`;

  if (server?.providerOnline === false) {
    openCtx(e, [
      { header: `${name} — bridge offline` },
      { label: "Leave server", icon: "leave", danger: true, run: () => void nsLeave(ns) },
    ]);
    return;
  }

  const muted = store.mutedAt(scope);
  // `nsCap`, not `can`: it ensures this scope's caps are fetched. `can` alone is
  // false for a namespace whose caps never loaded, which is every namespace you
  // have not opened.
  const canInvite = store.session.nsCap(ns, "invite");
  const canAdmin = store.session.nsCap(ns, "ns-admin");
  const canCreate = store.session.nsCap(ns, "chan-create");

  // The same set as the sidebar header's menu, but every entry names `ns`
  // explicitly, so none of them needs the namespace to be open first. That was
  // the whole problem: they all read `view.activeServer`, so acting on a tile
  // meant navigating to it (or silently editing whichever server was open).
  const items: CtxItem[] = [];

  if (canInvite) items.push({ label: "Create Invite", icon: "invite", run: () => store.invites.mintInvite(ns) });
  items.push({ label: "Notification Settings", icon: "bell", run: () => openNotifSettingsFor(ns) });
  if (canAdmin) items.push({ label: "Edit Server Profile", icon: "user", run: () => openServerProfileFor(ns) });
  // Same reachability rule as the header: owner, any grant delegation, or any
  // moderation/administration cap — not `ns-admin` alone.
  if (store.session.nsCanOpenSettings(ns))
    items.push({ label: "Server Settings", icon: "settings", run: () => openNsSettingsFor(ns) });

  if (canCreate) {
    items.push({ divider: true });
    items.push({ label: "Create Channel", icon: "channel", run: () => openCreateChannel(undefined, ns) });
    items.push({ label: "Create Category", icon: "folder", run: () => openCreateCategory(ns) });
  }

  items.push(...pluginItems("server-menu", ["namespace", "none"], ns));

  items.push({ divider: true });
  items.push({
    label: muted ? "Unmute server" : "Mute server",
    icon: "bell",
    run: () => setNotifLevel(scope, muted ? "mentions" : "nothing"),
  });
  items.push({ label: "Copy Server ID", icon: "copy", run: () => void navigator.clipboard?.writeText(ns) });

  // The owner cannot leave their own namespace — they transfer or delete it.
  // Compared against *this* namespace's owner, not `isNsOwner`, which answers for
  // whichever server is currently open.
  if (server?.owner !== store.session.account) {
    items.push({ divider: true });
    items.push({ label: "Leave Server", icon: "leave", danger: true, run: () => void nsLeave(ns) });
  }

  openCtx(e, items);
}

// Right-click the empty channel-list background (Discord-style) → create.
export function listCtx(e: MouseEvent): void {
  if (!view.activeServer) return;
  openCtx(e, [
    { label: "Create channel", icon: "channel", run: () => openCreateChannel() },
    { label: "Create category", icon: "folder", run: openCreateCategory },
    // §13.1 the `channel-list` surface: the namespace is what a plugin acts on
    // from the sidebar background.
    ...pluginItems("channel-list", ["namespace", "none"], view.activeServer),
  ]);
}
