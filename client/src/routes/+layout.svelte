<script lang="ts">
  import "../app.css";
  import { onMount, tick, untrack } from "svelte";
  import { goto } from "$app/navigation";
  import { page } from "$app/state";
  import * as nav from "$lib/nav";
  import { ui } from "$lib/ui.svelte";
  import { conn, attemptReconnect, HOMESERVER_KEY, SAVED_KEY } from "$lib/connection.svelte";
  import {
    handle,
    loadHistory,
    loadSyncCursor,
    hist,
    syncState,
    selectServer,
    goHome,
    fetchRoles,
    fetchGrants,
    fetchNsMembers,
    createRoleAt,
    deleteRoleAt,
    roleFetchQueue,
    sys,
    pendingChanCreate,
  } from "$lib/sync/reducer.svelte";
  import { clock, msgEpoch, msgTime, dayKey, dayLabel, retentionOf } from "$lib/time";
  import { scopeKeyOf, notifLevel, isMuted, serverMuted, notifLevelOf, setNotifLevel } from "$lib/notif";
  import {
    nicks,
    nickKey,
    nicksFetched,
    peerOf,
    initials,
    dotClass,
    avatarUrl,
    displayName,
    nickOf,
    bioOf,
    statusOf,
    friendLabel,
    queryProfile,
  } from "$lib/profile.svelte";
  import { toasts, toast, expectSuccess, confirmSuccess } from "$lib/toasts.svelte";
  import * as weft from "$lib/weft";
  import { EVERYONE_ROLE } from "$lib/constants";
  import type { Msg, CtxItem, ThreadInfo, MentionOpt } from "$lib/types";
  import { Role } from "$lib/models/role.svelte";
  import { cf, emailNudgeKey } from "$lib/models/connect.svelte";
  import { provideApp, type InviteInfo } from "$lib/context";
  import { store, type NotifLevel } from "$lib/models/store.svelte";
  import {
    rolesByScope,
    memberRoles,
    ensureCapsAt,
    capsResolved,
    ensureCaps,
    badgeFor,
    roleScopeOf,
    rolesAt,
    roleById,
    isOwnerAt,
    isStaff,
    rolesOf,
    fedRolesFetched,
    fetchMemberRoles,
    mentionsMe as sessionMentionsMe,
  } from "$lib/models/session.svelte";
  import {
    Channel,
    channels,
    mkMsg,
    nsOf,
    chanShort,
    channelRecord,
    ensureChannel,
    markRead,
    resetChannels,
    layoutCache,
    loadLayoutCache,
    cacheNsCats,
    cacheChanLayout,
    persistDms,
    restoreDms,
  } from "$lib/models/channel.svelte";
  import { Membership } from "$lib/models/membership.svelte";
  import { searchUnicode } from "$lib/shortcodes";
  import * as md from "$lib/markdown";
  import { installLinkGuard } from "$lib/linkguard.svelte";
  import LinkWarningModal from "$lib/components/modals/LinkWarningModal.svelte";
  import ConnectScreen from "$lib/components/ConnectScreen.svelte";
  import Toasts from "$lib/components/Toasts.svelte";
  import ContextMenu from "$lib/components/ContextMenu.svelte";
  import QuickSwitcher from "$lib/components/QuickSwitcher.svelte";
  import CommunityRail from "$lib/components/CommunityRail.svelte";
  import MemberList from "$lib/components/MemberList.svelte";
  import { initVoice, joinVoice, voice } from "$lib/voice.svelte";
  import {
    callMedia,
    connectCallMedia,
    disconnectCallMedia,
    toggleCallMute,
  } from "$lib/callmedia.svelte";
  import VoiceBar from "$lib/components/VoiceBar.svelte";
  import CameraPicker from "$lib/components/modals/CameraPicker.svelte";
  import ScreenPicker from "$lib/components/modals/ScreenPicker.svelte";
  import ScreenShareMenu from "$lib/components/modals/ScreenShareMenu.svelte";
  import { voiceUI } from "$lib/voiceui.svelte";
  import ChannelList from "$lib/components/sidebar/ChannelList.svelte";
  import SidebarHeader from "$lib/components/sidebar/SidebarHeader.svelte";
  import DmList from "$lib/components/sidebar/DmList.svelte";
  import UserFooter from "$lib/components/sidebar/UserFooter.svelte";
  import SidebarInput from "$lib/components/sidebar/SidebarInput.svelte";
  import Lightbox from "$lib/components/chat/Lightbox.svelte";
  import ThreadPanel from "$lib/components/chat/ThreadPanel.svelte";
  import CreateChannelModal from "$lib/components/modals/CreateChannelModal.svelte";
  import CreateCategoryModal from "$lib/components/modals/CreateCategoryModal.svelte";
  import ReportsQueueModal from "$lib/components/modals/ReportsQueueModal.svelte";
  import InviteCreateModal from "$lib/components/modals/InviteCreateModal.svelte";
  import InvitesModal from "$lib/components/modals/InvitesModal.svelte";
  import NewGroupModal from "$lib/components/modals/NewGroupModal.svelte";
  import PinsModal from "$lib/components/modals/PinsModal.svelte";
  import ThreadsModal from "$lib/components/modals/ThreadsModal.svelte";
  import CallOverlay from "$lib/components/CallOverlay.svelte";
  import SearchModal from "$lib/components/modals/SearchModal.svelte";
  import DiscoverModal from "$lib/components/modals/DiscoverModal.svelte";
  import ReportModal from "$lib/components/modals/ReportModal.svelte";
  import ChannelSettings from "$lib/components/modals/ChannelSettings.svelte";
  import ProfileCard from "$lib/components/modals/ProfileCard.svelte";
  import ProfileModal from "$lib/components/modals/ProfileModal.svelte";
  import NicknameModal from "$lib/components/modals/NicknameModal.svelte";
  import ConfirmModal from "$lib/components/modals/ConfirmModal.svelte";
  import UserSettingsModal from "$lib/components/modals/UserSettingsModal.svelte";
  import FederationPanel from "$lib/components/modals/FederationPanel.svelte";
  import ServerSettingsModal from "$lib/components/modals/ServerSettingsModal.svelte";
  import ServerProfileModal from "$lib/components/modals/ServerProfileModal.svelte";
  import NotificationSettingsModal from "$lib/components/modals/NotificationSettingsModal.svelte";

  // ---- connection + form state ----
  // Identity + connection status live on `store.session`; these are read-only
  // views so the ~100 bare `account`/`network`/`status` refs keep working while
  // the setters (connected / logout / reconnect) write `store.session.*`.
  const status = $derived(store.session.status);
  const network = $derived(store.session.network);
  const account = $derived(store.session.account);

  // The connect / login screen state (homeserver + auth inputs + probe results),
  // grouped into one object passed to `ConnectScreen`. See connect.svelte.ts.
  // Web build: the network is the page origin (display-only — the WASM backend
  // derives its WS URL from window.location); desktop: a QUIC host the user
  // types. On web the picker step is skipped (start on "auth").
  cf.host = weft.isWeb && typeof window !== "undefined" ? window.location.host : "127.0.0.1:4433";
  if (weft.isWeb) cf.serverStep = "auth";

  // ---- session lifecycle (Phase 8) ----
  // Reconnect state + `attemptReconnect` → `$lib/connection.svelte` (`conn`).
  let settingsOpen = $state(false);
  // ---- quick switcher (Ctrl+K) ----
  let switcherOpen = $state(false);
  let switcherQuery = $state("");
  let switcherResults = $derived.by(() => {
    const q = switcherQuery.toLowerCase().replace(/^[#@]/, "");
    return Object.values(channels)
      .filter((c) => c.name.toLowerCase().includes(q))
      .sort((a, b) => a.name.localeCompare(b.name))
      .slice(0, 25);
  });
  function switchTo(name: string) {
    switcherOpen = false;
    goto(nav.pathFor(name));
  }
  function globalKey(e: KeyboardEvent) {
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k") {
      e.preventDefault();
      switcherOpen = true;
      switcherQuery = "";
    } else if (e.key === "Escape") {
      switcherOpen = false;
      store.pins.open = false;
      discoverOpen = false;
      settingsOpen = false;
      ui.nsSettingsOpen = false;
      profileTarget = null;
      ctxMenu = null;
      serverMenu = false;
      userMenu = false;
      newChanOpen = false;
      newCatOpen = false;
      ui.chanPerms = null;
    }
  }
  // ---- right-click context menus ----
  let ctxMenu = $state<{ x: number; y: number; items: CtxItem[] } | null>(null);
  function openCtx(e: MouseEvent, items: CtxItem[]) {
    e.preventDefault();
    e.stopPropagation(); // don't let a channel/category menu bubble to the list background
    // Raw click point; ContextMenu clamps both axes to the viewport once it can
    // measure itself (so a tall menu near the bottom edge doesn't overflow).
    ctxMenu = { x: e.clientX, y: e.clientY, items };
  }
  // Can I moderate-delete another member's message in the active channel?
  // `delete-any` at the channel or its namespace. Kicks off a fetch of my own
  // caps so the answer resolves on a subsequent open.
  //
  // NOTE: operator (`*`) status is deliberately NOT consulted for namespaced
  // channels — mirrors the server (context.rs): a network operator's god-mode is
  // web-admin authority, never day-to-day power on someone else's server. At the
  // network level (top-level channels) `nsScope` *is* `*`, so operator power
  // still applies there naturally.
  function canModDelete(): boolean {
    if (!active.startsWith("#")) return false;
    const nsScope = roleScopeOf(active);
    ensureCapsAt(account, active);
    ensureCapsAt(account, nsScope);
    return store.session.can("delete-any", active) || store.session.can("delete-any", nsScope);
  }
  // Do I hold moderation power (mute/ban/kick, or owner) in a channel's server?
  // Same scope rule as `canModDelete`: namespaced channels never consult `*`, so
  // an operator sees no moderation tools on another person's server; top-level
  // channels honor operator caps because their scope *is* `*`. Gates every
  // moderation surface (member list, profile card, context menus).
  function canModerate(channel: string): boolean {
    if (!channel.startsWith("#")) return false;
    const nsScope = roleScopeOf(channel);
    ensureCapsAt(account, channel);
    ensureCapsAt(account, nsScope);
    return store.session.moderates(channel) || store.session.moderates(nsScope);
  }
  // Do I hold a *specific* capability at the active server's scope (`ns:<server>`,
  // or `*` at network level)? Owner/ns-admin (the `owner` flag) implies every cap;
  // operator (`*`) counts only at network level, never inside someone else's
  // namespace. This is the per-permission gate — each server surface checks the
  // exact cap it needs (Create Channel → chan-create, Create Invite → invite, …).
  function serverCap(cap: string): boolean {
    const scope = activeServer ? `ns:${activeServer}` : "*";
    ensureCapsAt(account, scope);
    return store.session.can(cap, scope);
  }
  // Do I hold any `grant:*` delegation cap at the server scope? Gates the Roles
  // tab — creating/assigning roles is capability delegation.
  function serverCanGrant(): boolean {
    const scope = activeServer ? `ns:${activeServer}` : "*";
    ensureCapsAt(account, scope);
    return store.session.canGrant(scope);
  }
  // Server Settings is reachable with any moderation/administration capability —
  // not plain member caps (send/invite). Each tab then gates itself, so a mod
  // sees only the tabs they can act on.
  function canOpenServerSettings(): boolean {
    return (
      isNsOwner(account) ||
      serverCanGrant() ||
      ["ns-admin", "ban", "mute", "kick", "reports", "chan-create", "policy", "manage-nicks"].some(
        serverCap,
      )
    );
  }
  function msgCtx(e: MouseEvent, m: Msg) {
    if (!m.msgid) return; // nothing actionable without a real msgid
    const mod = canModDelete();
    // System (join/part) lines carry a msgid and are deletable — by the person
    // they're about (its author) or a moderator with delete-any. No other
    // actions apply, so offer just Delete.
    if (m.system) {
      if (m.own || mod)
        openCtx(e, [{ label: "Delete", icon: "delete", danger: true, run: () => doDelete(m) }]);
      return;
    }
    const items: CtxItem[] = [{ label: "Reply", run: () => (ui.replyTo = m) }];
    if (active.startsWith("#")) {
      items.push({ label: "Reply in thread", run: () => openThread(m) });
      items.push({
        label: activeChannel?.pinnedIds?.includes(m.msgid) ? "Unpin" : "Pin",
        run: () => togglePin(m),
      });
    }
    items.push({ label: "Copy text", run: () => navigator.clipboard?.writeText(m.body) });
    // The full msgid (`network/ULID`) — what HISTORY, reports and the admin
    // message lookup all take. Copying the bare ULID would lose the origin.
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
      // A moderator can delete anyone's message (delete-any, server-enforced).
      if (mod) items.push({ label: "Delete", icon: "delete", danger: true, run: () => doDelete(m) });
      items.push({ label: "Report", run: () => openReport(m) });
    }
    openCtx(e, items);
  }
  function chanCtx(e: MouseEvent, ch: Channel) {
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
    // Channel administration (edit permissions / delete) is a moderator surface —
    // hidden from non-moderators (server-enforced regardless). Same scope rule as
    // everywhere: no power on another person's server ⇒ no Mod Menu.
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
  // Items adapt to context: a DM shows Close DM (else Message), a channel adds
  // Invite + moderation (only there is the user a server member you can act on),
  // and a friend shows Remove friend.
  function userCtx(e: MouseEvent, name: string) {
    if (peerOf(name) === account) return; // no menu on yourself
    const ref = qualify(name);
    const rel = store.social.friends.get(ref);
    const items: CtxItem[] = [
      { label: "Open profile", icon: "profile", run: () => openFullProfile(name) },
      active === dmKeyFor(name)
        ? { label: "Close DM", icon: "close", run: () => closeDm(name) }
        : { label: "Message", icon: "message", run: () => openDm(name) },
    ];
    // Calling is a friends-only action (§ social layer) — only offer it to a
    // confirmed friend.
    if (rel === "friends") items.push({ label: "Call", icon: "call", run: () => callUser(ref) });
    // §10.3 per-namespace nickname — only meaningful inside a server.
    if (activeServer) items.push({ label: "Set nickname", icon: "nick", run: () => openNickDialog(name) });

    // Friendship: Add when unrelated, Remove when friends, and the sensible
    // action for a pending request either way.
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

    // Invite + moderation only make sense on a server member — i.e. when we're
    // actually viewing one of the server's channels (not the friends/DM view).
    if (active.startsWith("#")) {
      items.push({ divider: true });
      items.push({ label: "Invite to server", icon: "invite", run: inviteToServer });
      // Moderation controls are shown only to actual moderators of *this* server
      // (server-enforced too; hiding them keeps the UI honest — an operator on
      // another person's server has no power here, so no tools).
      if (canModerate(active)) {
        items.push({ header: "Mod Menu", mod: true });
        // Mute/ban at the *namespace* scope so they're server-wide and show up in
        // Server Settings → Bans (banScope() = ns:<server>). Kick is inherently
        // per-channel (it force-parts the active channel).
        items.push({ label: "Mute", icon: "mute", run: () => moderate("mute", name, banScope()) });
        items.push({ label: "Kick", icon: "kick", run: () => moderate("kick", name) });
        items.push({ label: "Ban", icon: "ban", danger: true, run: () => moderate("ban", name, banScope()) });
      }
    }

    openCtx(e, items);
  }
  // The right-click menu for a group DM (in the DM list).
  function groupCtx(e: MouseEvent, id: string) {
    openCtx(e, [
      { label: "Mark as read", icon: "markread", run: () => markRead(id) },
      { label: "Copy group ID", icon: "copy", run: () => navigator.clipboard?.writeText(id) },
      { label: "Leave group", icon: "leave", danger: true, run: () => leaveGroup(id) },
    ]);
  }
  let theme = $state<"dark" | "light">("dark");
  function toggleTheme() {
    theme = theme === "dark" ? "light" : "dark";
    document.documentElement.dataset.theme = theme;
    try {
      localStorage.setItem("weft:theme", theme);
    } catch {
      /* ignore */
    }
  }

  // In-app confirmation (the Tauri webview blocks native window.confirm, so
  // destructive actions must not rely on it). Resolves true/false.
  let confirmState = $state<{ message: string; label: string; resolve: (v: boolean) => void } | null>(null);
  function appConfirm(message: string, label = "Confirm"): Promise<boolean> {
    return new Promise((resolve) => (confirmState = { message, label, resolve }));
  }
  function resolveConfirm(ok: boolean) {
    confirmState?.resolve(ok);
    confirmState = null;
  }

  function logout() {
    conn.manualLogout = true;
    ui.reconnecting = false;
    conn.lastCreds = null;
    userMenu = false;
    settingsOpen = false;
    weft.disconnect().catch(() => {});
    resetChannels();
    goto("/"); // reset the view so the next login lands home, not on a stale URL
    store.servers.clear();
    nsMetaFetched.clear();
    store.resetPresence();
    store.reports.queue.clear();
    // The in-memory skeleton is gone — the next login must do a full sync, not a
    // cursor delta (which would leave the rail empty).
    syncState.synced = false;
    store.session.status = "connect";
  }

  // ---- live data, channel collection + layout cache: `$lib/models/channel.svelte`
  // (channels/mkMsg/ensureChannel/markRead/nsOf/chanShort/layoutCache/…). ----

  // ---- notification preferences (per-user, localStorage) ----
  // Set per **namespace** (`ns:<name>`, or `net` for top-level) in the
  // Notification-pref resolvers (scopeKeyOf / notifLevel / isMuted / serverMuted /
  // notifLevelOf / setNotifLevel) → `$lib/notif`.
  // ---- notification-settings modal (per-namespace) ----
  let notifSettingsOpen = $state(false);
  // The scope the modal edits = the active server (namespace, or the network).
  const notifScopeKey = () => (activeServer ? `ns:${activeServer}` : "net");
  const notifScopeLabel = () => (activeServer ? serverName(activeServer) : network);
  function openNotifSettings() {
    notifSettingsOpen = true;
    serverMenu = false;
  }
  /// §10.5 open the user settings on the verification tab (from the no-email nudge).
  function openVerification() {
    userTab = "verification";
    settingsOpen = true;
    userMenu = false;
  }
  // ---- navigation: derived from the URL (path-based routes, see lib/nav.ts) ----
  // The single source of truth for "what's open" is the route. `active` is the
  // sigil-tagged key (`#ns/chan` | `@peer` | `&group` | ""), `activeServer` the
  // selected namespace, `homeView` whether the sidebar shows DMs. Navigation is
  // `goto(nav.pathFor(...))`; nothing assigns these directly.
  const view = $derived(nav.viewFrom(page.route?.id, page.params));
  const active = $derived(view.active);
  const activeServer = $derived(view.activeServer); // "" = network top-level / home; else a namespace
  const homeView = $derived(view.homeView);
  let joinInput = $state("");
  let composer = $state("");
  let membersVisible = $state(true);
  // ---- servers/namespaces as rail tiles (Phase 6, flavor A) ----
  // `nsOf` / `chanShort` are channel-name helpers imported from the channel model.
  // A user-facing label for any target: `#vanity` for a channel, the peer's
  // display name for a DM, the group label for a group DM.
  const titleOf = (name: string): string => {
    if (name.startsWith("#")) return `#${chanShort(name)}`;
    if (name.startsWith("&")) return groupLabel(name);
    if (name.startsWith("@")) return displayName(peerOf(name));
    return name;
  };
  // ---- DMs + presence (Phase 5) ----
  // The shared client store (singleton) — the identity maps, namespaces, and
  // client prefs. Domain models navigate to it too (see client-model-refactor.md).
  // §10.3 nicks cache + profile/identity helpers → `$lib/profile.svelte`.
  // Pull a server's nicknames once, the first time it's viewed.
  $effect(() => {
    const s = activeServer;
    if (s && !nicksFetched.has(s)) {
      nicksFetched.add(s);
      weft.nicksQuery(`ns:${s}`).catch(() => {});
    }
  });
  // Set a per-namespace nickname (empty clears it). `NICK` verb (§10.3).
  function setNick(scope: string, account: string, value: string) {
    weft.nick(scope, account, value).catch((e) => toast(String(e), "error"));
  }
  const myStatus = $derived(store.session.myStatus);
  // §10.5 the caller's own verification claims, keyed by kind (email/birthday).
  // `VERIFY LIST` streams its claims with no terminator, so we can't know an
  // account has zero claims until the response has had time to arrive. Gate the
  // "no email" nudge on this flag (flipped a beat after login) to avoid flashing
  // the banner at every account that *does* have an email.
  // Whether THIS server can actually mail codes (WELCOME `features=email`, §10.5).
  // The user dismissed the "add an email" nudge — persisted, so it's gone for
  // good on this account (loaded on connect, keyed by host+account).
  // §10.5 nudge: a logged-in account with no email on file can't do a password
  // reset — warn and offer a jump to the verification page. Only when the server
  // can actually send mail, and only until the user dismisses it once.
  let needsEmailWarning = $derived(
    status === "online" &&
      store.session.verificationsLoaded &&
      ui.serverEmailAvailable &&
      !store.session.verifications.email &&
      !ui.emailBannerDismissed,
  );
  function dismissEmailBanner() {
    ui.emailBannerDismissed = true;
    try {
      localStorage.setItem(emailNudgeKey(), "1");
    } catch {
      /* storage unavailable */
    }
  }
  // Footer user menu (presence + settings + logout) and the user-settings page tab.
  let userMenu = $state(false);
  let userTab = $state<"account" | "appearance" | "connection" | "verification">("account");
  let dmInput = $state("");
  // ---- social layer (friends / groups / calls) ----
  // State lives on `store.social` (userrefs are `account@network`, resolved via
  // the Account map at the UI edge). Only the add-friend input box stays local.
  let addFriendInput = $state("");
  // ---- discover dialog (Phase 6) ----
  let discoverOpen = $state(false);
  // Namespace identity (metadata + membership + emoji) lives on interned Server
  // objects in `store.servers` — see docs/architecture/client-model-refactor.md.
  // Namespace ids we've auto-joined on creation, so the reactive auto-join fires
  // once per freshly-created server (see the `ns-meta` handler).
  // Channel creations awaiting their server-minted id. CHANNEL CREATE returns the
  // canonical `#<ns-id>/<chan-id>` asynchronously (as CHANNEL-LAYOUT); until then
  // we can't address the channel, so stash the follow-up actions keyed by
  // `<ns-id>|<slug>` and apply them when the matching layout arrives.
  // My namespace membership is `Server.joined` (from NS-MEMBER). A namespace with
  // **no** channels wouldn't otherwise surface (the rail is derived from
  // channels), so this keeps a just-joined empty server visible + selectable.
  // True while an initial/reconnect SYNC is streaming. SYNC replays my namespace
  // memberships as NS-MEMBER events; during that replay we populate the rail but
  // must NOT auto-navigate (that's only for a *live* join, e.g. creating a server).
  // False until this app session has synced once. A cold start does a full sync
  // (rebuild the skeleton); only a later in-session reconnect replays the cursor.
  // ---- roles / invites / reports (Phase 7) ----
  // §6.7 reports state (queue + filing target) lives on `store.reports`.
  let profileTarget = $state<string | null>(null); // member profile popout
  // ---- §6.5 invites (list menu + create screen) — state on `store.invites`.
  // ---- federation (§11, operator) — state lives on `store.federation` ----
  let federationOpen = $state(false);
  function refreshNetblocks() {
    store.federation.netblocks.clear();
    weft.netblockList().catch((e) => toast(String(e), "error"));
  }
  function openFederation() {
    federationOpen = true;
    settingsOpen = false;
    refreshNetblocks();
  }
  function netblockAdd(nw: string, reason?: string) {
    weft
      .netblockAdd(nw, reason)
      .then(() => setTimeout(refreshNetblocks, 200))
      .catch((e) => toast(String(e), "error"));
  }
  function netblockRemove(nw: string) {
    store.federation.netblocks.delete(nw);
    weft.netblockRemove(nw).catch((e) => toast(String(e), "error"));
  }
  function bridgePropose(scope: string, peer: string, history: string, media: string, typing: boolean) {
    weft.bridgePropose(scope, peer, history, media, typing).catch((e) => toast(String(e), "error"));
  }
  function bridgeAccept(peer: string, version: number) {
    weft.bridgeAccept(peer, version).catch((e) => toast(String(e), "error"));
  }
  function bridgeSever(peer: string) {
    weft.bridgeSever(peer).catch((e) => toast(String(e), "error"));
  }
  // ---- pins + message search (§6.4) — state on `store.pins` / `store.search`
  // (self-contained panels); results stream in as BATCHes, routed by the reducer.
  // ---- threads (§9.4) — side panel + list modal — state on `store.threads`.
  // ---- capability + role reads: `$lib/models/session.svelte` (ensureCapsAt /
  // rolesAt / roleById / rolesOf / isOwnerAt / isStaff / badgeFor / mentionsMe /
  // roleScopeOf); `rolesByScope` / `memberRoles` state live there too. ----
  const isOperator = $derived(store.session.isOperator);
  // Mention test defaults `ns` to the active server (the session helper requires it).
  const mentionsMe = (body: string, ns: string = activeServer) => sessionMentionsMe(body, ns);

  // ---- §6.5 named roles: the fetch/batch machinery (queues + fetchRoles) ----
  // Roles arrive in `r…`-id BATCHes; a queue tracks which scope each answers,
  // so several scopes can be fetched at once (e.g. ns + channel).

  // ---- §6.5 per-subject grants at a scope (channel-permission member
  // overrides). Live on `store.grants`; arrive in `gr…`-id BATCHes with a scope
  // queue (buffered here), mirroring the roles path. ----

  // ---- §6.2 NS INFO MEMBERS: the moderator roster (members + join + roles) ----
  // Arrives as an `ni…`-id BATCH of `ns-member-info` events. The roster + its
  // fetch state (`membersLoading` + streaming `memberBuf`) live on `Server`;
  // `loadingNsMembers` is the reducer's in-flight cursor (which ns to route the
  // streamed rows to — the events don't carry the ns), so an empty roster still
  // flushes.
  /// The *real* owner of the active namespace — the record's owner, NOT anyone
  /// who merely holds ns-admin caps (a network operator holds them everywhere,
  /// but that's web-admin authority, not ownership of this server).
  const isNsOwner = (account: string): boolean =>
    !!activeServer && peerOf(account) === (activeNsMeta?.owner ?? "");
  // Eagerly fetch a member's namespace roles once, so the member list can group
  // by hoisted role without opening each profile. Deduped per (account, scope).
  const memberRolesFetched = new Set<string>();
  function ensureMemberRoles(account: string) {
    const scope = nsRoleScope();
    const key = `${account}|${scope}`;
    if (memberRolesFetched.has(key)) return;
    memberRolesFetched.add(key);
    fetchMemberRoles(account, scope);
  }
  // Eagerly fetch a scope's role *definitions* (names/colors/hoist) once, so the
  // member list can group by hoisted role on open — not only after a profile or
  // the perms modal happens to fetch them. Deduped per scope.
  const rolesFetched = new Set<string>();
  function ensureRoles(scope: string) {
    if (!scope || rolesFetched.has(scope)) return;
    rolesFetched.add(scope);
    fetchRoles(scope);
  }
  // The color to tint an account's name with — their highest assigned role at
  // the active namespace (Discord-style), excluding the implicit @everyone.
  // "" ⇒ no colored role, render in the default text color. Fetches the
  // member's roles + the scope's role defs lazily so it resolves on next paint.
  function nameColor(account: string): string {
    const scope = roleScopeOf(active);
    if (!scope.startsWith("ns:")) return "";
    ensureMemberRoles(account);
    ensureRoles(scope);
    const top = rolesOf(account, scope).find((r) => r.name !== EVERYONE_ROLE);
    return top?.color ?? "";
  }

  let profilePos = $state<{ left: number; top: number } | null>(null);
  function openProfile(handle: string, e?: MouseEvent) {
    // Accept a bare account or a full `account@network` ref (the friends list
    // passes full refs); the card keys on the bare *local* account, keeping a
    // genuinely federated ref whole.
    const at = handle.lastIndexOf("@");
    const target = at > 0 && handle.slice(at + 1) === network ? handle.slice(0, at) : handle;
    profileTarget = target;
    // Anchor the card next to the clicked row (Discord-style); centered fallback.
    const POP_W = 340;
    const POP_H = 360;
    if (e?.currentTarget instanceof HTMLElement) {
      const r = e.currentTarget.getBoundingClientRect();
      let left = r.left - POP_W - 12; // prefer to the left of the row
      if (left < 8) left = r.right + 12; // flip right if no room
      left = Math.max(8, Math.min(left, window.innerWidth - POP_W - 8));
      const top = Math.max(8, Math.min(r.top - 8, window.innerHeight - POP_H - 8));
      profilePos = { left, top };
    } else {
      profilePos = null;
    }
    const scope = roleScopeOf(active);
    queryProfile(target); // §10.3 nick / avatar / bio / custom status
    ensureCaps(target, active); // channel-scope owner/mod badges
    ensureCapsAt(target, scope); // for the owner check
    fetchRoles(scope); // role definitions (names + colors)
    fetchMemberRoles(target, scope); // this member's assigned roles
  }

  // The *full* profile modal (distinct from the anchored ProfileCard popover):
  // a centered dialog with bio, status, mutual servers and quick actions.
  let profileModalTarget = $state<string | null>(null);
  function openFullProfile(handle: string) {
    const at = handle.lastIndexOf("@");
    const target = at > 0 && handle.slice(at + 1) === network ? handle.slice(0, at) : handle;
    profileModalTarget = target;
    profileTarget = null; // close the popover if it was open
    queryProfile(target); // make sure we have their nick / avatar / bio
    ensureCaps(target, active);
  }

  // §10.3 quick "Set nickname" dialog, opened from a user's context menu (own
  // or another member's). Per-namespace, so it targets the active server.
  let nickTarget = $state<string | null>(null);
  function openNickDialog(handle: string) {
    const at = handle.lastIndexOf("@");
    const target = at > 0 && handle.slice(at + 1) === network ? handle.slice(0, at) : handle;
    // Nicks for the active server are already fetched (effect on activeServer),
    // so nickOf(target) is populated for the dialog's prefill.
    nickTarget = target;
  }

  // Servers (namespaces) I share with `target`, derived from the memberships I
  // can already see — a channel of that namespace listing them as a member.
  function mutualServers(target: string): string[] {
    return serverNamespaces.filter((ns) =>
      Object.values(channels).some(
        (c) => c.name.startsWith("#") && nsOf(c.name) === ns && c.members?.some((m) => m.name === target),
      ),
    );
  }
  // Friend helpers for the profile modal: normalize a (possibly bare) handle to
  // the `account@network` friend key, then read state / act on it.
  function friendState(handle: string): "friends" | "incoming" | "outgoing" | "none" {
    return (store.social.friends.get(qualify(peerOf(handle))) as "friends" | "incoming" | "outgoing") ?? "none";
  }
  function friendAction(handle: string, action: "add" | "accept" | "remove") {
    const ref = qualify(peerOf(handle));
    if (action === "add") weft.friendAdd(ref).catch((e) => toast(String(e), "error"));
    else if (action === "accept") acceptFriend(ref);
    else removeFriend(ref);
  }
  function assignRoleTo(acct: string, role: Role) {
    const scope = roleScopeOf(active);
    // Success is confirmed by the resulting ROLE-MEMBER event (see
    // `expectSuccess`); a missing-cap failure never confirms and its ERR toasts.
    expectSuccess(`roles:${acct}|${scope}`, `Roles updated for ${acct}`);
    weft
      .roleAssign(scope, acct, role.id)
      .then(() => fetchMemberRoles(acct, scope)) // ROLES-OF queues after ASSIGN → fresh list
      .catch((e) => toast(String(e), "error"));
  }
  function unassignRoleFrom(acct: string, role: Role) {
    const scope = roleScopeOf(active);
    expectSuccess(`roles:${acct}|${scope}`, `Roles updated for ${acct}`);
    weft
      .roleUnassign(scope, acct, role.id)
      .then(() => fetchMemberRoles(acct, scope))
      .catch((e) => toast(String(e), "error"));
  }
  // ---- namespace admin panel (§6.2 / §2.4 / §6.6) ----
  // §10.3 per-server profile editor (your own nickname on this server).
  let serverProfileOpen = $state(false);
  function openServerProfile() {
    if (activeServer) serverProfileOpen = true;
    serverMenu = false;
  }
  // §6.7 moderation deny-list (mutes + bans) per scope, for the Bans tab —
  // lives on `store.deny`.
  const banScope = () => (activeServer ? `ns:${activeServer}` : "*");
  const denyList = () => store.deny.get(banScope()) ?? [];
  function refreshBans() {
    store.deny.set(banScope(), []); // full refresh; the batch response repopulates
    weft.modList(banScope()).catch((e) => toast(String(e), "error"));
  }
  function liftMod(kind: string, account: string) {
    moderate(kind === "mute" ? "unmute" : "unban", account, banScope());
  }
  // Role editor (§6.6). Roles live at the namespace scope.
  let newRoleName = $state("");
  let newRoleColor = $state("#5865f2");
  let newRoleCaps = $state<string[]>([]);
  let newRoleHoist = $state(false);
  let newRolePingable = $state(false);
  const toggleNewRoleCap = (c: string) =>
    (newRoleCaps = newRoleCaps.includes(c) ? newRoleCaps.filter((x) => x !== c) : [...newRoleCaps, c]);
  const nsRoleScope = () => (activeServer ? `ns:${activeServer}` : "*");
  function createRole() {
    // A role may hold zero permissions (granted later); only a name is required.
    if (!newRoleName.trim()) return;
    // Append at the bottom of the ordered list.
    const position = rolesAt(nsRoleScope()).length;
    createRoleAt(
      nsRoleScope(),
      newRoleName.trim(),
      newRoleColor,
      newRoleCaps.join(","),
      newRoleHoist,
      newRolePingable,
      position,
    )
      .then(() => {
        newRoleName = "";
        newRoleCaps = [];
        newRoleHoist = false;
        newRolePingable = false;
      })
      .catch((e) => toast(String(e), "error"));
  }
  // The implicit @everyone role's current caps at the active server (or []).
  const everyoneCaps = (): string[] =>
    rolesAt(nsRoleScope()).find((r) => r.name === EVERYONE_ROLE)?.caps ?? [];
  // Set the @everyone baseline. Non-empty → upsert the reserved role; empty →
  // delete it (the server rejects an empty cap list, and "no role" = no
  // baseline). It's never assigned or hoisted.
  function setEveryoneCaps(caps: string[]) {
    const scope = nsRoleScope();
    // Non-empty upserts the @everyone role by name (ROLE CREATE matches it by
    // name); empty deletes it — deletion addresses the role by its id (v0.13).
    if (caps.length) {
      createRoleAt(scope, EVERYONE_ROLE, "#99aab5", caps.join(","), false, false, 0).catch((e) =>
        toast(String(e), "error"),
      );
      return;
    }
    // @everyone is the one reserved, per-scope-unique role — safe to resolve by
    // name; delete addresses it by its id.
    const everyone = rolesAt(scope).find((r) => r.name === EVERYONE_ROLE);
    if (everyone) deleteRoleAt(scope, everyone.id).catch((e) => toast(String(e), "error"));
  }
  // Move a role up/down in the ordered list, then persist the new order (§6.5).
  // Addressed by the role id (names aren't unique, v0.13).
  function moveRole(roleId: string, dir: -1 | 1) {
    const scope = nsRoleScope();
    const list = [...rolesAt(scope)];
    const i = list.findIndex((r) => r.id === roleId);
    const j = i + dir;
    if (i < 0 || j < 0 || j >= list.length) return;
    [list[i], list[j]] = [list[j], list[i]];
    roleFetchQueue.push(scope);
    weft.rolesReorder(scope, list.map((r) => r.id)).catch((e) => toast(String(e), "error"));
  }
  // Persist an arbitrary order (drag-and-drop) — positions follow the list of
  // role ids (v0.13).
  function reorderRoles(ids: string[]) {
    const scope = nsRoleScope();
    roleFetchQueue.push(scope);
    weft.rolesReorder(scope, ids).catch((e) => toast(String(e), "error"));
  }
  // Apply a role edit. v0.13: a single ROLE UPDATE addressed by the role's id
  // replaces every field and carries a name change (keeping members + issued
  // caps) — no separate RENAME + upsert (§6.5).
  function saveRole(
    role: Role,
    patch: { name: string; color: string; caps: string[]; hoist: boolean; pingable: boolean },
  ) {
    const scope = nsRoleScope();
    const name = patch.name.trim() || role.name;
    // Zero permissions is valid (a cosmetic/hoist role, or perms granted later).
    roleFetchQueue.push(scope);
    weft
      .roleUpdate(
        scope,
        role.id,
        patch.color,
        patch.caps.join(","),
        patch.hoist,
        patch.pingable,
        role.position,
        name,
      )
      .catch((e) => toast(String(e), "error"));
  }
  function deleteRole(roleId: string) {
    deleteRoleAt(nsRoleScope(), roleId).catch((e) => toast(String(e), "error"));
  }
  function assignRole(roleId: string) {
    const who = nsDelegSubject.trim();
    if (!who) {
      toast("Enter an account first", "error");
      return;
    }
    // Confirmed by the ROLE-MEMBER event; a cap failure never confirms.
    expectSuccess(`roles:${who}|${nsRoleScope()}`, `Roles updated for ${who}`);
    weft.roleAssign(nsRoleScope(), who, roleId).catch((e) => toast(String(e), "error"));
  }

  // In-line role editing for the Members directory. Both mutate the roster
  // optimistically (so the pill appears/disappears at once) and reconcile
  // against the server truth shortly after — a rejected change (missing cap)
  // simply snaps back on the refetch, and its ERR toasts.
  function reconcileRoster(ns: string) {
    setTimeout(() => {
      if (ui.nsSettingsOpen && ui.nsTab === "members") fetchNsMembers(ns);
    }, 500);
  }
  function memberRow(ns: string, account: string): Membership | undefined {
    return store.servers.get(ns)?.member(account);
  }
  // v0.13: addressed by the role id. The roster's `roleIds` is a list of ids
  // (NS-MEMBER-INFO), so the optimistic update adds/removes the same id.
  function assignNsRole(account: string, roleId: string) {
    const scope = nsRoleScope();
    const ns = activeServer;
    const m = memberRow(ns, account);
    if (m && !m.roleIds.includes(roleId)) m.roleIds = [...m.roleIds, roleId];
    expectSuccess(`roles:${account}|${scope}`, `Roles updated for ${account}`);
    weft
      .roleAssign(scope, account, roleId)
      // Refresh BOTH rosters: the settings members tab AND `memberRoles` (which
      // the member-list sidebar groups by hoisted role) — otherwise a hoisted
      // assignment wouldn't regroup the sidebar until the next interaction.
      .then(() => {
        reconcileRoster(ns);
        fetchMemberRoles(account, scope);
      })
      .catch((e) => {
        toast(String(e), "error");
        fetchNsMembers(ns);
      });
  }
  function unassignNsRole(account: string, roleId: string) {
    const scope = nsRoleScope();
    const ns = activeServer;
    const m = memberRow(ns, account);
    if (m) m.roleIds = m.roleIds.filter((r) => r !== roleId);
    expectSuccess(`roles:${account}|${scope}`, `Roles updated for ${account}`);
    weft
      .roleUnassign(scope, account, roleId)
      .then(() => {
        reconcileRoster(ns);
        fetchMemberRoles(account, scope); // regroup the member-list sidebar too
      })
      .catch((e) => {
        toast(String(e), "error");
        fetchNsMembers(ns);
      });
  }
  // Right-click a member row in the directory → namespace-scoped moderation.
  // Mute/ban (and their lifts) key on `ns:<server>` in the deny-list; kick is
  // channel-scoped and so has no place on a server-wide roster.
  function nsMemberCtx(e: MouseEvent, target: string) {
    e.preventDefault();
    const scope = banScope();
    const deny = denyList();
    const muted = deny.some((d) => d.account === target && d.kind === "mute");
    const banned = deny.some((d) => d.account === target && d.kind === "ban");
    const items: CtxItem[] = [{ label: "Open profile", icon: "profile", run: () => openProfile(target) }];

    if (target !== account) {
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
  let nsTitle = $state("");
  let nsDesc = $state("");
  let nsVis = $state("public");
  let nsDelegSubject = $state("");
  let nsNewOwner = $state("");
  let nsRecM = $state(2);
  let nsRecKeys = $state("");
  let myRecoveryKey = $state("");
  let recoveryDoc = $state("");
  // A legacy-shaped view of the active Server's metadata (snake_case field names
  // the modals/banners already read). Undefined until NS-META has landed.
  let activeNsMeta = $derived.by(() => {
    const s = activeServer ? store.servers.get(activeServer) : undefined;
    if (!s || !s.metaLoaded) return undefined;
    return {
      id: s.id,
      name: s.name,
      title: s.title,
      description: s.description,
      owner: s.owner,
      visibility: s.visibility,
      federation: s.federation,
      welcome: s.welcome,
      recovery_eta: s.recoveryEta,
      recovery_rung: s.recoveryRung,
      categories: s.categories,
    };
  });
  // v0.13: a namespace's rail tile / header key is its **id**; its display name
  // is the vanity from NS-META (fall back to the id only if we haven't seen it).
  const serverName = (nsId: string): string => store.servers.get(nsId)?.displayName ?? nsId;
  function showRecoveryKey() {
    weft
      .recoveryPubkey(network, activeServer)
      .then((k) => (myRecoveryKey = k))
      .catch((e) => toast(String(e), "error"));
  }
  function startRecovery() {
    weft
      .recoveryStart(network, activeServer, account)
      .then((doc) => {
        recoveryDoc = doc;
        toast("Recovery started — share this record with your quorum to co-sign");
      })
      .catch((e) => toast(String(e), "error"));
  }
  function cosignRecovery() {
    if (!recoveryDoc.trim()) return;
    weft
      .recoveryCosign(network, activeServer, recoveryDoc.trim())
      .then((doc) => (recoveryDoc = doc))
      .catch((e) => toast(String(e), "error"));
  }
  function submitRecovery() {
    if (recoveryDoc.trim()) weft.nsRecover(activeServer, recoveryDoc.trim()).catch((e) => toast(String(e), "error"));
  }

  const retentionMeta: Record<string, { label: string; cls: string; icon: string }> = {
    ephemeral: { label: "Ephemeral", cls: "ephemeral", icon: '<circle cx="12" cy="12" r="8" stroke-dasharray="3 3"/>' },
    retained: { label: "Retained", cls: "retained", icon: '<rect x="4" y="4" width="16" height="16" rx="2"/><path d="M4 10h16"/>' },
    permanent: { label: "Permanent", cls: "permanent", icon: '<rect x="4" y="4" width="16" height="16" rx="2" fill="currentColor" stroke="none"/>' },
    e2ee: { label: "E2EE · MLS", cls: "e2ee", icon: '<rect x="5" y="11" width="14" height="9" rx="2"/><path d="M8 11V7a4 4 0 0 1 8 0v4"/>' },
  };
  const retentionOrder = ["e2ee", "permanent", "retained", "ephemeral"];

  // Time / ULID-timestamp / day-label / retention helpers → `$lib/time`.

  // ---- history / scrollback (Phase 1) ----
  // History pages buffered per *target channel*, keyed by the messages' own
  // `target`. This is what makes history robust: a page flushes to the channel it
  // names, so a concurrent MEMBERS/roles/… batch can never steal or clobber it,
  // whatever its batch id or arrival order.


  // Fetch a channel's history page. Single-flight (`hist.loading` guard);
  // MessageList calls this on first open (initial) and on scroll-to-top (paging).

  let activeChannel = $derived(active ? channels[active] : undefined);
  let activeIsDm = $derived(active.startsWith("@"));
  let activeIsGroup = $derived(active.startsWith("&"));
  // Namespaces we hold channels in OR are a member of (the latter keeps a
  // channel-less server on the rail) — each becomes a rail tile (flavor A).
  // The rail = every namespace I belong to: one I hold a channel in, or one I'm
  // a recorded member of. `Server.joined` is the join barrier — populated by SYNC
  // and live NS-MEMBER, and (below) by owning a namespace — so a channel-less server
  // (e.g. one I just created) still shows.
  let serverNamespaces = $derived(
    [
      ...new Set([
        ...Object.values(channels)
          .filter((c) => c.name.startsWith("#"))
          .map((c) => nsOf(c.name))
          .filter(Boolean),
        ...[...store.servers.values()].filter((s) => s.joined).map((s) => s.id),
      ]),
    ].sort(),
  );
  // Proactively load NS-META (title/vanity + layout) for every server on the
  // rail we haven't seen it for — so tiles show the right name/initials without
  // waiting for a click. `channels(id)` replies with NS-META + CHANNEL-LAYOUTs.
  const nsMetaFetched = new Set<string>();
  $effect(() => {
    for (const ns of serverNamespaces) {
      if (!store.servers.get(ns)?.metaLoaded && !nsMetaFetched.has(ns)) {
        nsMetaFetched.add(ns);
        weft.channels(ns).catch(() => {});
      }
    }
  });
  // Server-tile unread/mention rollups (so unread in other servers is visible),
  // folded over the server's own channels.
  const serverChannels = (ns: string) =>
    Object.values(channels).filter((c) => nsOf(c.name) === ns && c.name !== active);
  const serverUnread = (ns: string) => serverChannels(ns).some((c) => c.unread);
  const serverMention = (ns: string) => serverChannels(ns).some((c) => c.mention);
  // Total mentions across a server's channels, for the rail's numeric badge.
  const serverMentionCount = (ns: string) =>
    serverChannels(ns).reduce((sum, c) => sum + c.mentionCount, 0);
  // Discord-style grouping for the *active server*: uncategorized channels sit
  // bare at the top (category "", no header), then each CHANNEL-LAYOUT category
  // (position-ordered) in its persisted order.
  let channelGroups = $derived.by(() => {
    const bare: Channel[] = [];
    const groups = new Map<string, Channel[]>();
    // Empty categories the admin created (client-side) show up too.
    for (const cat of store.servers.get(activeServer)?.categories ?? layoutCache[activeServer]?.cats ?? [])
      groups.set(cat, []);
    for (const c of Object.values(channels)) {
      if (!c.name.startsWith("#") || nsOf(c.name) !== activeServer) continue;
      const cat = c.category;
      if (!cat) {
        bare.push(c);
        continue;
      }
      if (!groups.has(cat)) groups.set(cat, []);
      groups.get(cat)!.push(c);
    }

    const byPos = (a: Channel, b: Channel) =>
      (a.position ?? 0) - (b.position ?? 0) || a.name.localeCompare(b.name);
    bare.sort(byPos);
    for (const list of groups.values()) list.sort(byPos);

    const out = bare.length ? [{ category: "", list: bare }] : [];
    for (const [category, list] of groups.entries()) out.push({ category, list });
    return out;
  });

  // Right-click a rail tile: select the server and open its header menu (the
  // same Create Invite / Notification / Server Settings menu as clicking the name).
  function openServerMenu(ns: string) {
    selectServer(ns);
    serverMenu = true;
  }
  // §6.2 leave a namespace: drop membership, navigate home, and forget its
  // channels locally so the rail updates without a reload.
  function nsLeave() {
    const ns = activeServer;
    if (!ns) return;
    serverMenu = false;
    weft.nsLeave(ns).catch((e) => toast(String(e), "error"));
    for (const name of Object.keys(channels)) {
      if (name.startsWith("#") && nsOf(name) === ns) delete channels[name];
    }
    store.servers.delete(ns); // drop the tile now; the NS-MEMBER part echo confirms
    goHome();
  }
  // Fetch a namespace's layout + categories from the server whenever it
  // becomes active (covers reload — the client keeps no category state).
  const layoutFetched = new Set<string>();
  $effect(() => {
    const s = activeServer;
    if (s && !layoutFetched.has(s)) {
      layoutFetched.add(s);
      weft.channels(s).catch(() => layoutFetched.delete(s));
    }
  });

  // ---- §9.4 custom emoji (per namespace) — now `Server.emoji` ----
  const emojiFetched = new Set<string>();
  $effect(() => {
    const s = activeServer;
    if (s && !emojiFetched.has(s)) {
      emojiFetched.add(s);
      weft.emojiList(s).catch(() => emojiFetched.delete(s));
    }
  });
  // The active namespace's custom emoji as an array (for pickers).
  const activeEmoji = $derived(
    [...(activeServer ? (store.servers.get(activeServer)?.emoji ?? []) : [])].map(([name, media]) => ({ name, media })),
  );
  function addEmoji(name: string, media: string) {
    if (!activeServer) return;
    weft.emojiAdd(activeServer, name, media).catch((e) => toast(String(e), "error"));
  }
  function removeEmoji(name: string) {
    if (!activeServer) return;
    weft.emojiRemove(activeServer, name).catch((e) => toast(String(e), "error"));
  }
  // Resolve a `:name:` shortcode to a fetchable image URL in the active
  // namespace, or null if it isn't a custom emoji here.
  const emojiUrlFor = (name: string): string | null => {
    const media = activeServer ? store.servers.get(activeServer)?.emoji.get(name) : undefined;
    return media ? weft.mediaUrl(media) : null;
  };

  // DM conversations (keyed `@peer`), plus any peer we've opened a blank DM with.
  let dmList = $derived(
    Object.values(channels).filter((c) => c.name.startsWith("@") || c.name.startsWith("&")),
  );

  // ---- DM + presence + §10.3 profile helpers (peerOf / dotClass / avatarUrl /
  // displayName / nickOf / bioOf / statusOf / initials) → `$lib/profile.svelte`.
  /** Set (or clear, with "") my own custom status. */
  function setCustomStatus(text: string) {
    weft.profileSet({ status: text }).catch((e) => toast(String(e), "error"));
  }

  function openDm(peer: string) {
    const key = "@" + peer.replace(/^@/, "");
    ensureChannel(key);
    persistDms(); // keep the DM in the list across reconnects
    goto(nav.pathFor(key));
  }
  function startDm() {
    const p = dmInput.trim().replace(/^@/, "");
    dmInput = "";
    if (p) openDm(p);
  }
  // The DM channel key for a user (`@peer`), and whether one is open.
  const dmKeyFor = (name: string) => "@" + peerOf(name);
  const dmOpen = (name: string) => !!channels[dmKeyFor(name)];
  // Close (hide) an open DM — a local-only view action; nothing is deleted
  // server-side. Switch away if it was the open conversation.
  function closeDm(name: string) {
    const key = dmKeyFor(name);
    delete channels[key];
    persistDms();
    if (active === key) goHome();
  }
  // The set of open 1:1 DMs is view state the server doesn't yet track (a
  // server-owned DM list is §18 territory), so we persist it per account so a
  // conversation — and its history on click — survives a reconnect / relaunch.
  // v0.12 SYNC cursor, per account+device (localStorage). Stored on every
  // `sync-end`, replayed on reconnect so `SYNC since=` catches up missed
  // messages + offline edits/reactions in one round trip.

  // "Invite to server" — open the invites panel for the current server, where a
  // shareable link is minted (invites are link-based, §6.5).
  function inviteToServer() {
    openInvites();
  }

  // ---- social layer: friends ----
  // Fully-qualify a typed handle to `account@network` (local network default).
  function qualify(handle: string): string {
    const h = handle.trim().replace(/^@/, "");
    return h.includes("@") ? h : `${h}@${network}`;
  }
  // A friend's short label: bare handle for local, full ref for federated.
  // A friend's local account handle (for DM/profile/presence), if local.
  function friendLocalAccount(user: string): string | null {
    const [acct, net] = user.split("@");
    return net === network ? acct : null;
  }
  const friendList = $derived(
    [...store.social.friends]
      .filter(([, s]) => s === "friends")
      .map(([u]) => u)
      .sort((a, b) => friendLabel(a).localeCompare(friendLabel(b))),
  );
  const incomingRequests = $derived(
    [...store.social.friends].filter(([, s]) => s === "incoming").map(([u]) => u).sort(),
  );
  const outgoingRequests = $derived(
    [...store.social.friends].filter(([, s]) => s === "outgoing").map(([u]) => u).sort(),
  );
  function addFriend() {
    const user = qualify(addFriendInput);
    if (!user || !user.includes("@")) return;
    addFriendInput = "";
    weft.friendAdd(user).catch((e) => toast(String(e), "error"));
  }
  function acceptFriend(user: string) {
    weft.friendAccept(user).catch((e) => toast(String(e), "error"));
  }
  // Unfriend / cancel an outgoing request / decline an incoming one.
  function removeFriend(user: string) {
    weft.friendRemove(user).catch((e) => toast(String(e), "error"));
  }
  // Open a DM with a friend (local friends only for now — DMs are per-network).
  function messageFriend(user: string) {
    const acct = friendLocalAccount(user);
    if (acct) openDm(acct);
  }
  // Show the Friends home screen (home view, no DM selected).
  function openFriends() {
    goto("/");
  }
  // Pressing the DM/home tile lands on the most recently active conversation
  // (DM or group) — or the friends menu if there are none.

  // ---- group DMs ----
  let newGroupInput = $state("");
  // A group's display label: its name, else the member handles (minus self).
  function groupLabel(id: string): string {
    const g = store.social.groups.get(id);
    if (!g) return "Group";
    if (g.name) return g.name;
    const me = `${account}@${network}`;
    const others = g.members.filter((m) => m !== me).map((m) => friendLabel(m));
    return others.length ? others.join(", ") : "Group";
  }
  const groupList = $derived([...store.social.groups.keys()]);
  function createGroup() {
    const members = newGroupInput
      .split(/[,\s]+/)
      .map((h) => qualify(h))
      .filter((h) => h.includes("@"));
    if (!members.length) return;
    newGroupInput = "";
    weft.groupCreate(members).catch((e) => toast(String(e), "error"));
  }
  // The "+" in a DM: pick friends to fold into a group with the current peer.
  let groupPickerOpen = $state(false);
  let groupPickerSeed = $state("");
  let groupPickerPos = $state<{ left: number; top: number } | null>(null);
  function openGroupPicker(e?: MouseEvent) {
    // From a DM, seed the current peer into the group; from the Friends view
    // there's no active peer, so open seedless — pick everyone from scratch.
    groupPickerSeed = activeIsDm ? qualify(peerOf(active)) : "";
    // Anchor the popover just under the button that opened it (speech-bubble
    // style), right-aligned so it stays on-screen; centered fallback otherwise.
    const POP_W = 300;
    if (e?.currentTarget instanceof HTMLElement) {
      const r = e.currentTarget.getBoundingClientRect();
      const left = Math.max(8, Math.min(r.right - POP_W, window.innerWidth - POP_W - 8));
      const top = Math.min(r.bottom + 8, window.innerHeight - 120);
      groupPickerPos = { left, top };
    } else {
      groupPickerPos = null;
    }
    groupPickerOpen = true;
  }
  function createGroupWith(members: string[]) {
    groupPickerOpen = false;
    const uniq = [...new Set(members.map((m) => qualify(m)).filter((m) => m.includes("@")))];
    if (uniq.length < 2) return; // the peer + at least one more
    weft.groupCreate(uniq).catch((e) => toast(String(e), "error"));
  }
  function openGroup(id: string) {
    ensureChannel(id);
    goto(nav.pathFor(id));
  }
  function leaveGroup(id: string) {
    weft.groupLeave(id).catch((e) => toast(String(e), "error"));
  }
  function addToGroup(id: string, handle: string) {
    const user = qualify(handle);
    if (user.includes("@")) weft.groupAdd(id, user).catch((e) => toast(String(e), "error"));
  }
  function startGroupCall(id: string) {
    weft.groupCall(id).catch((e) => toast(String(e), "error"));
  }
  function leaveGroupCall(id: string) {
    weft.groupCallLeave(id).catch(() => {});
    disconnectCallMedia();
    if (store.social.activeGroupCall === id) store.social.activeGroupCall = null;
  }

  // ---- friend calls (1:1) ----
  function callUser(user: string) {
    if (store.social.activeCall) return; // already in a call
    // Calls are a friends-only feature — block any non-friend target (the
    // single gate behind every call entry point: context menu, topbar, profile).
    if (store.social.friends.get(qualify(user)) !== "friends") {
      toast("You can only call friends", "error");
      return;
    }
    weft.call(user).catch((e) => toast(String(e), "error"));
  }
  function acceptCall() {
    const incoming = store.social.incomingCall;
    if (!incoming) return;
    const { from, room } = incoming;
    weft.callAccept(from).catch((e) => toast(String(e), "error"));
    store.social.activeCall = { peer: from, room, state: "active" };
    store.social.incomingCall = null;
  }
  function declineCall() {
    if (!store.social.incomingCall) return;
    weft.callDecline(store.social.incomingCall.from).catch(() => {});
    store.social.incomingCall = null;
  }
  function endCall() {
    if (!store.social.activeCall) return;
    weft.callEnd(store.social.activeCall.peer).catch(() => {});
    disconnectCallMedia();
    store.social.activeCall = null;
  }
  function setStatus(s: string) {
    store.session.myStatus = s;
    userMenu = false;
    weft.presence(s).catch(() => {});
  }

  // ---- event handling ----

  // ---- actions ----
  // Device-key login availability (checked as host/account change).
  $effect(() => {
    const h = cf.host.trim();
    const a = cf.account.trim();
    if (h && a)
      weft
        .hasDeviceKey(h, a)
        .then((v) => (cf.deviceKeyAvailable = v))
        .catch(() => (cf.deviceKeyAvailable = false));
    else cf.deviceKeyAvailable = false;
  });
  function keyLogin() {
    cf.mode = "key";
    doConnect();
  }
  function enrollThisDevice() {
    weft
      .enrollDevice(cf.host.trim(), account)
      .then(() => toast("Device key enrolled — passwordless login is on for next time"))
      .catch((e) => toast(String(e), "error"));
  }

  async function doConnect() {
    if (!cf.account.trim()) return;
    // §6.1 a register email is required only when the homeserver asks for one.
    if (cf.mode === "register" && cf.emailRequired && !cf.email.trim()) {
      cf.authError = "this server requires an email address to register";
      return;
    }
    cf.authError = "";
    cf.authFailed = false;
    store.session.status = "connecting";
    conn.manualLogout = false;
    conn.reconnectAttempts = 0;
    // Held in memory (never persisted) so a mid-session drop can reconnect.
    conn.lastCreds = { host: cf.host.trim(), account: cf.account.trim(), password: cf.password };
    try {
      await weft.connect(
        cf.host.trim(),
        cf.account.trim(),
        cf.password,
        cf.mode,
        cf.mode === "register" ? cf.email.trim() : undefined,
      );
    } catch (err) {
      store.session.status = "connect";
      cf.authError = String(err);
    }
  }

  /// §3.6 probe the current homeserver for its shape (does REGISTER need an
  /// email?). Best-effort: a failure just leaves the email field optional — the
  /// server still enforces its own policy at REGISTER.
  async function probeServer() {
    const h = cf.host.trim();
    if (!h) return;
    cf.probing = true;
    try {
      const info = await weft.probe(h);
      cf.emailRequired = info.emailRequired;
    } catch {
      cf.emailRequired = false;
    } finally {
      cf.probing = false;
    }
  }

  /// Confirm the typed homeserver: persist it as the local default, move to the
  /// login/register step, and probe it for its register-email requirement.
  function chooseServer() {
    const h = cf.host.trim();
    if (!h) return;
    try {
      localStorage.setItem(HOMESERVER_KEY, h);
    } catch {
      /* storage unavailable */
    }
    cf.serverStep = "auth";
    void probeServer();
  }

  /// "Change" on the login screen → back to the homeserver picker.
  function changeServer() {
    cf.authError = "";
    cf.emailRequired = false;
    cf.serverStep = "server";
  }

  function joinNamespace(name: string) {
    weft.nsJoin(name).catch(() => {});
    weft.channels(name).catch(() => {}); // fetch its category layout
  }
  function doJoin() {
    const raw = joinInput.trim();
    if (!raw) return;
    joinInput = "";
    // `#chan` joins one channel; a bare name (or `ns:name`) joins the whole
    // namespace — the server auto-joins every channel we're allowed to see.
    if (raw.startsWith("#")) {
      weft.join(raw).catch((e) => (cf.authError = String(e)));
    } else {
      joinNamespace(raw.replace(/^ns:/, ""));
    }
  }


  /// A capability-gated moderation action (§10.4). These are **server-side**:
  /// the client sends the wire intent and weftd enforces it (BAN/KICK/MUTE are
  /// wired here frontend-first; the weftd verbs land later). Shared by the
  /// slash commands and the member-row buttons.
  // §6.7 moderation. `scope` defaults to the active channel; ban/mute also
  // accept `ns:<name>` or `*` (network). Confirmation arrives as a MODERATED
  // event; a missing-cap failure surfaces as an ERR.
  function moderate(verb: string, user: string, scope?: string, reason?: string) {
    if (!user) return;
    const s = scope ?? active;
    if (!s) return sys("join a channel first");
    weft.moderate(verb, s, user, reason).catch((e) => toast(String(e), "error"));
  }

  /// Slash commands — the primary control surface in the composer.
  function runSlash(input: string) {
    const [raw, ...rest] = input.slice(1).split(/\s+/);
    const cmd = raw.toLowerCase();
    const arg = rest.join(" ").trim();
    switch (cmd) {
      case "ban":
      case "unban":
      case "kick":
      case "mute":
      case "unmute":
        moderate(cmd, arg);
        break;
      case "join":
        if (arg) weft.join(arg.startsWith("#") ? arg : `#${arg}`).catch(() => {});
        break;
      case "part":
      case "leave":
        if (active.startsWith("#")) weft.part(active).catch(() => {});
        break;
      case "create":
        if (arg) weft.channelCreate(arg.startsWith("#") ? arg : `#${arg}`).catch(() => {});
        break;
      case "delete":
        if (active.startsWith("#")) weft.channelDelete(active).catch(() => {});
        break;
      case "topic":
        if (active.startsWith("#")) weft.channelMeta(active, "topic", arg).catch(() => {});
        break;
      case "help":
        sys(
          "/join #chan · /part · /create #chan · /delete · /topic <text> · /ban /unban /kick /mute /unmute <user>",
        );
        break;
      default:
        sys(`unknown command: /${cmd} (try /help)`);
    }
  }

  // ---- §13 media attachments ----
  let pendingAttachments = $state<
    { uri: string; name: string; mime: string; thumb: string | null; width: number | null; height: number | null }[]
  >([]);

  // Upload a batch of files into the pending tray (shared by the picker, paste,
  // and drag-drop). Caps at 10 per message (§13); a failure toasts, not throws.
  async function addFiles(files: Iterable<File>) {
    if (!active) return;
    for (const file of files) {
      if (pendingAttachments.length >= 10) {
        toast("up to 10 attachments per message", "error");
        break;
      }
      try {
        const up = await weft.upload(file);
        pendingAttachments = [
          ...pendingAttachments,
          {
            uri: up.media,
            name: file.name || "pasted-file",
            mime: file.type,
            thumb: up.thumb,
            width: up.width,
            height: up.height,
          },
        ];
      } catch (e) {
        toast(`upload failed: ${e}`, "error");
      }
    }
  }

  function attachFile() {
    const input = document.createElement("input");
    input.type = "file";
    input.multiple = true;
    input.onchange = () => addFiles(Array.from(input.files ?? []));
    input.click();
  }

  // Paste an image/file from the clipboard straight into the tray (§13).
  function pasteFiles(e: ClipboardEvent) {
    const files = Array.from(e.clipboardData?.files ?? []);
    if (files.length) {
      e.preventDefault();
      addFiles(files);
    }
  }

  // Drop files onto the composer/chat area to attach them.
  function dropFiles(e: DragEvent) {
    const files = Array.from(e.dataTransfer?.files ?? []);
    if (files.length) {
      e.preventDefault();
      addFiles(files);
    }
  }

  function removeAttachment(i: number) {
    pendingAttachments = pendingAttachments.filter((_, k) => k !== i);
  }

  function doSend() {
    const text = composer.trim();
    if (text.startsWith("/")) {
      runSlash(text);
      composer = "";
      return;
    }
    // §6.4: empty body is legal when there are attachments.
    if (!text && !pendingAttachments.length) return;
    if (!active) return;
    // Stamp intrinsic image size onto the reference so every recipient (and the
    // history replay) can reserve exact space before the bytes load (§13).
    const attachments = pendingAttachments.map((a) => weft.withMediaDims(a.uri, a.width, a.height));
    const target = active;
    const savedReply = ui.replyTo?.msgid;
    // §9.2/§11.13 optimistic send: show the message immediately as "sending",
    // keyed by a client nonce. The authoritative MESSAGE echoes the nonce back
    // (even when a home-authoritative channel mints it on another network) and
    // reconcile replaces this placeholder — so the send feels instant regardless
    // of federation latency.
    const label = crypto.randomUUID();
    ensureChannel(target).messages.push(
      mkMsg({
        author: account,
        body: text,
        time: clock(),
        ts: Date.now(),
        own: true,
        md: true,
        replyTo: savedReply,
        attachments: attachments.length ? attachments : undefined,
        label,
        pending: true,
      }),
    );
    // Clear the composer optimistically; the placeholder carries the text.
    ui.replyTo = null;
    stopTyping();
    composer = "";
    pendingAttachments = [];
    weft
      .sendMessage(target, text, savedReply, attachments, undefined, label)
      .catch((e) => {
        // The send was rejected (e.g. an over-long body): drop the placeholder,
        // restore the text so it isn't silently eaten, and surface the error.
        const ch = channels[target];
        const i = ch?.messages.findIndex((m) => m.label === label) ?? -1;
        if (ch && i !== -1) ch.messages.splice(i, 1);
        composer = text;
        toast(String(e), "error");
      });
  }

  function composerKey(e: KeyboardEvent) {
    // Mention autocomplete captures navigation/accept/dismiss keys while open.
    if (mentionQuery !== null && mentionMatches.length) {
      const n = mentionMatches.length;
      if (e.key === "ArrowDown") {
        e.preventDefault();
        mentionIndex = (mentionIndex + 1) % n;
        return;
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        mentionIndex = (mentionIndex - 1 + n) % n;
        return;
      } else if (e.key === "Enter" || e.key === "Tab") {
        e.preventDefault();
        pickMention(mentionMatches[Math.min(mentionIndex, n - 1)].name);
        return;
      } else if (e.key === "Escape") {
        e.preventDefault();
        mentionQuery = null;
        return;
      }
    }
    // :emoji: autocomplete captures the same keys while open.
    if (emojiQuery !== null && emojiSuggestions.length) {
      const n = emojiSuggestions.length;
      if (e.key === "ArrowDown") {
        e.preventDefault();
        emojiIndex = (emojiIndex + 1) % n;
        return;
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        emojiIndex = (emojiIndex - 1 + n) % n;
        return;
      } else if (e.key === "Enter" || e.key === "Tab") {
        e.preventDefault();
        pickEmojiSuggestion(emojiSuggestions[Math.min(emojiIndex, n - 1)].name);
        return;
      } else if (e.key === "Escape") {
        e.preventDefault();
        emojiQuery = null;
        return;
      }
    }
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      doSend();
    } else if (e.key === "ArrowUp" && !composer) {
      // Discord-style: edit your last message from an empty composer.
      const mine = activeChannel?.messages.filter((m) => m.own && m.msgid);
      const last = mine?.[mine.length - 1];
      if (last) {
        e.preventDefault();
        startEdit(last);
      }
    }
  }

  // ---- edit / delete (Phase 2) ----
  let editingKey = $state<number | null>(null);
  let editDraft = $state("");

  function startEdit(m: Msg) {
    if (!m.own || !m.msgid) return;
    editingKey = m.key;
    editDraft = m.body;
  }
  function cancelEdit() {
    editingKey = null;
    editDraft = "";
  }
  // Focus the inline editor and put the caret at the end.
  function saveEdit(m: Msg) {
    const body = editDraft.trim();
    if (body && m.msgid && body !== m.body) {
      m.body = body; // optimistic; the EDITED echo confirms
      m.edited = true;
      weft.edit(m.msgid, body).catch(() => {});
    }
    cancelEdit();
  }
  function editKey(e: KeyboardEvent, m: Msg) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      saveEdit(m);
    } else if (e.key === "Escape") {
      e.preventDefault();
      cancelEdit();
    }
  }
  function doDelete(m: Msg) {
    // The DELETED echo drops it (Phase 0 handler) — no optimistic removal.
    if (m.own && m.msgid) weft.del(m.msgid).catch(() => {});
  }

  // ---- reactions (Phase 3) ----
  // Curated emoji, categorized (§ Phase 8 polish).
  let pickerKey = $state<number | null>(null); // message whose picker is open

  // Search the target's in-flight history buffer first (not committed yet), then
  // the channel's messages.


  // Non-optimistic: the server echoes our own REACTION back (like a MSG ack),
  // which drives the count — so toggling can't double-count.
  function toggleReaction(m: Msg, emoji: string) {
    if (!m.msgid) return;
    pickerKey = null;
    const mine = m.reactions?.[emoji]?.mine;
    (mine ? weft.unreact(m.msgid, emoji) : weft.react(m.msgid, emoji)).catch(() => {});
  }

  // ---- markdown (Phase 4 · Tier 1) — rendering lives in `$lib/markdown`; the
  // per-render mention/emoji context (`MdContext`) is built at the ctx boundary. ----
  const mdContext = (): md.MdContext => ({
    account,
    activeServer,
    pingable: rolesAt(`ns:${activeServer}`).filter((r) => r.pingable),
    myRoleIds: new Set(memberRoles[`${account}|ns:${activeServer}`] ?? []),
    emoji: (n) => (activeServer ? store.servers.get(activeServer)?.emoji.get(n) : undefined),
  });


  // ---- replies (Phase 4) ----
  function jumpTo(msgid?: string) {
    if (!msgid) return;
    const m = activeChannel?.messages.find((x) => x.msgid === msgid);
    if (m) document.getElementById(`msg-${m.key}`)?.scrollIntoView({ block: "center" });
  }

  // ---- typing indicators (Phase 4) ----
  let typingLabel = $derived.by(() => {
    const who = activeChannel?.typers ?? [];
    if (!who.length) return "";
    if (who.length === 1) return `${who[0]} is typing…`;
    if (who.length === 2) return `${who[0]} and ${who[1]} are typing…`;
    return "several people are typing…";
  });

  // Announce our own typing while composing, debounced to a stop after idle.
  let typingChannel: string | null = null;
  let typingStop: ReturnType<typeof setTimeout> | undefined;
  function onComposerInput() {
    updateMention();
    updateEmojiSuggest();
    if (!active.startsWith("#")) return;
    if (typingChannel && typingChannel !== active) stopTyping();
    if (!typingChannel) {
      typingChannel = active;
      weft.typing(active, true).catch(() => {});
    }
    clearTimeout(typingStop);
    typingStop = setTimeout(stopTyping, 4000);
  }

  // ---- @-mention autocomplete ----
  let mentionQuery = $state<string | null>(null);
  let mentionMatches = $derived.by<MentionOpt[]>(() => {
    if (mentionQuery === null) return [];
    const q = mentionQuery.toLowerCase();
    const opts: MentionOpt[] = [];
    if ("everyone".startsWith(q)) opts.push({ name: "everyone", kind: "special", display: "everyone" });
    if ("here".startsWith(q)) opts.push({ name: "here", kind: "special", display: "here" });
    // Pingable roles at this server (single-word names — the token can't hold
    // spaces), so members can @-mention them from the composer.
    for (const r of rolesAt(`ns:${activeServer}`))
      if (r.pingable && !/\s/.test(r.name) && r.name.toLowerCase().startsWith(q))
        opts.push({ name: r.name, kind: "role", display: r.name, color: r.color });
    // Members: match the account token OR the resolved display name, and carry
    // the avatar (via `name`), display name, and canonical account@network.
    for (const m of activeChannel?.members ?? []) {
      if (m.name === account) continue;
      const disp = displayName(m.name);
      if (!m.name.toLowerCase().startsWith(q) && !disp.toLowerCase().startsWith(q)) continue;
      const identity = m.name.includes("@") ? m.name : `${m.name}@${network}`;
      opts.push({ name: m.name, kind: "member", display: disp, identity });
    }
    return opts.slice(0, 8);
  });
  // The highlighted row (arrow-key navigable). Reset whenever the query moves,
  // and always clamped to the live match count on read.
  let mentionIndex = $state(0);
  function updateMention() {
    const m = composer.match(/@([a-z0-9._-]*)$/i);
    mentionQuery = m ? m[1] : null;
    mentionIndex = 0;
  }
  function pickMention(name: string) {
    composer = composer.replace(/@[a-z0-9._-]*$/i, `@${name} `);
    mentionQuery = null;
    mentionIndex = 0;
  }

  // ---- :emoji: autocomplete (custom emoji only — unicode has no names) ----
  let emojiQuery = $state<string | null>(null);
  type EmojiSuggestion = { name: string; url: string | null; char?: string };
  const emojiSuggestions = $derived.by<EmojiSuggestion[]>(() => {
    if (emojiQuery === null) return [];
    const q = emojiQuery.toLowerCase();
    const rank = (n: string) => (n.toLowerCase().startsWith(q) ? 0 : 1);
    // This server's custom emoji first (they win a name clash), then standard
    // unicode shortcodes (`:smile:` → 😄).
    const custom: EmojiSuggestion[] = activeEmoji
      .filter((e) => e.name.toLowerCase().includes(q))
      .sort((a, b) => rank(a.name) - rank(b.name) || a.name.localeCompare(b.name))
      .map((e) => ({ name: e.name, url: emojiUrlFor(e.name) }));
    const taken = new Set(custom.map((c) => c.name));
    const unicode: EmojiSuggestion[] = searchUnicode(q)
      .filter((u) => !taken.has(u.name))
      .map((u) => ({ name: u.name, url: null, char: u.char }));
    return [...custom, ...unicode].slice(0, 10);
  });
  let emojiIndex = $state(0);
  function updateEmojiSuggest() {
    // A `:word` at a token boundary — not `http://`, not `12:30`. `+`/`-` allow
    // shortcodes like `:+1:` / `:e-mail:`.
    const m = composer.match(/(?:^|\s):([a-zA-Z0-9_+-]+)$/);
    emojiQuery = m ? m[1] : null;
    emojiIndex = 0;
  }
  function pickEmojiSuggestion(name: string) {
    // Unicode shortcodes insert the character (universal); custom emoji keep the
    // `:name:` form (their image is server-specific).
    const s = emojiSuggestions.find((x) => x.name === name);
    const insert = s?.char ?? `:${name}:`;
    composer = composer.replace(/:[a-zA-Z0-9_+-]*$/, `${insert} `);
    emojiQuery = null;
    emojiIndex = 0;
  }
  function stopTyping() {
    clearTimeout(typingStop);
    if (typingChannel) {
      weft.typing(typingChannel, false).catch(() => {});
      typingChannel = null;
    }
  }

  // On opening a text channel, fetch its roster once (MEMBERS folds in as
  // MEMBER-join rows). History + scroll positioning are owned by the channel's
  // own <MessageList>. `active` is the only tracked dependency; the record is
  // read/written untracked so this can't self-trigger.
  $effect(() => {
    const a = active;
    if (!a.startsWith("#")) return;
    untrack(() => {
      const ch = channels[a];
      if (ch && !ch.rosterLoaded) {
        ch.rosterLoaded = true;
        weft.members(a).catch(() => {});
      }
    });
  });

  // `active` is URL-derived, so a DM/group deep link (or reload) can name a
  // conversation nothing has instantiated yet. Ensure its record exists so the
  // route renders instead of falling through to the empty placeholder.
  $effect(() => {
    const a = active;
    if (!a.startsWith("@") && !a.startsWith("&")) return;
    untrack(() => {
      if (!channels[a]) {
        ensureChannel(a);
        if (a.startsWith("@")) persistDms();
      }
    });
  });

  // ---- unread "New messages" divider (Tier 1) ----
  // Anchored to the read marker as it stood when we opened the channel, so it
  // holds its place while we read (unlike lastRead, which advances) and re-
  // anchors when we switch channels. Defined *before* the auto-mark effect below
  // so it captures lastRead before that effect advances it.
  let newDividerFor = "";
  let newBoundary = $state<number | null>(null); // epoch ms; NEW line before the first newer msg
  $effect(() => {
    const a = active;
    if (a === newDividerFor) return;
    newDividerFor = a;
    newBoundary = untrack(() => {
      const lr = channels[a]?.lastRead;
      return lr ? msgEpoch(lr) : null;
    });
  });
  // The render key of the message the NEW divider sits before, or null.
  const newDividerKey = $derived.by(() => {
    if (newBoundary === null) return null;
    for (const m of activeChannel?.messages ?? []) {
      if (m.system || m.own) continue;
      if (m.ts > newBoundary) return m.key;
    }
    return null;
  });

  // Viewing a channel clears its unread badge and advances the read marker
  // (MARK, synced across our devices — §9.7).
  $effect(() => {
    const ch = activeChannel;
    if (!ch || ch.voice) return;
    markRead(ch.name);
    if (!ch.name.startsWith("#")) return;
    let newest: string | undefined;
    for (let i = ch.messages.length - 1; i >= 0; i--)
      if (ch.messages[i].msgid) {
        newest = ch.messages[i].msgid;
        break;
      }
    if (newest && newest !== ch.lastRead) {
      ch.lastRead = newest;
      weft.mark(ch.name, newest).catch(() => {});
    }
  });

  // ---- discover + channel management (Phase 6) ----
  function openDiscover() {
    discoverOpen = true;
    // Clear the transient browse list (loaded non-member servers) but KEEP the
    // ones I'm in (their metadata drives the rail) and ones interned only as a
    // channel's namespace (metaLoaded=false — a live `Channel.server` edge).
    for (const [id, s] of store.servers) if (s.metaLoaded && !s.joined) store.servers.delete(id);
    nsMetaFetched.clear();
    ui.discoverCursor = null;
    weft.discover().catch(() => {});
  }

  // Capability/invite scopes relevant to what's open: channel → its ns → net.
  function scopesFor(): string[] {
    const s: string[] = [];
    if (active.startsWith("#")) s.push(active);
    const ns = nsOf(active) || activeServer;
    if (ns) s.push(`ns:${ns}`);
    s.push("*");
    return s;
  }

  // Reporting (ReportModal owns its form + submit)
  function openReport(m: Msg) {
    if (m.msgid) store.reports.target = m;
  }
  function openReports() {
    store.reports.open = true;
    store.reports.queue.clear();
    weft.reportsList(activeServer ? `ns:${activeServer}` : "*").catch(() => {});
  }


  // Invites — every entry point opens the creation screen (pick expiry + max
  // uses, then generate), rather than minting a fixed invite immediately.
  function openInviteCreate(scope?: string) {
    store.invites.createScope = scope || scopesFor()[0] || "";
    store.invites.link = null;
    store.invites.id = null;
    store.invites.createOpen = true;
  }
  function mintInvite() {
    openInviteCreate();
  }
  // Mint with the chosen limits — `null` = unlimited uses / never expires. The
  // resulting link arrives on the `invited` event and fills `inviteLink`.
  function generateInvite(maxUses: number | null, expiry: number | null) {
    const scope = store.invites.createScope;
    if (!scope) return;
    weft
      .inviteMint(scope, maxUses ?? undefined, expiry ?? undefined)
      .catch((e) => toast(String(e), "error"));
  }
  // Share an invite link with a friend by dropping it into their DM. Only
  // local-network friends are DM-able (cross-network DMs are out of scope).
  function sendInviteDM(ref: string, link: string) {
    const acct = friendLocalAccount(ref);
    if (!acct) return;
    const target = "@" + acct;
    ensureChannel(target);
    persistDms();
    weft.sendMessage(target, link).catch((e) => toast(String(e), "error"));
  }

  // ---- Discord-style invites menu ----
  function loadInvites(scope: string) {
    store.invites.scope = scope;
    store.invites.list = [];
    store.invites.buf = [];
    store.invites.loading = true;
    weft.inviteList(scope).catch((e) => {
      store.invites.loading = false;
      toast(String(e), "error");
    });
  }
  function openInvites() {
    loadInvites(scopesFor()[0]);
    store.invites.listOpen = true;
  }
  // The Server-Settings Invites tab lists the whole namespace's invites.
  function loadNsInvites() {
    if (activeServer) loadInvites(`ns:${activeServer}`);
  }
  function revokeInvite(id: string) {
    weft.inviteRevoke(id).catch((e) => toast(String(e), "error"));
    store.invites.list = store.invites.list.filter((i) => i.invite_id !== id); // optimistic
  }
  function createInvite() {
    openInviteCreate(store.invites.scope || scopesFor()[0]);
  }
  // Reconstruct the shareable link for an invite (the list doesn't carry it).
  function inviteLinkFor(inv: InviteInfo): string {
    const ns = inv.scope.startsWith("ns:")
      ? inv.scope.slice(3)
      : inv.scope.startsWith("#") && inv.scope.includes("/")
        ? inv.scope.slice(1).split("/")[0]
        : null;
    return ns
      ? `weft://${network}/${ns}/i/${inv.invite_id}`
      : `weft://${network}/i/${inv.invite_id}`;
  }

  // ---- server dropdown (Discord-style header menu) ----
  let serverMenu = $state(false);
  let newChanOpen = $state(false);
  let newChanName = $state("");
  let newChanCategory = $state("");
  let newChanAnnounce = $state(false);
  let newChanRet = $state(""); // "" = server default; else a RETENTION_OPTIONS value
  let newChanVoice = $state(false); // §16 create a voice channel
  function openCreateChannel(prefillName = "") {
    newChanName = prefillName;
    newChanCategory = "";
    newChanAnnounce = false;
    newChanRet = "";
    newChanVoice = false;
    newChanOpen = true;
    serverMenu = false;
  }
  function createChannel() {
    const slug = newChanName.trim().replace(/^#/, "").replace(/\s+/g, "-").toLowerCase();
    // v0.13: channels are `#<ns-id>/<chan-id>` — we send the *desired* vanity as
    // the local segment and the server mints the id. We can't JOIN/META the
    // channel by the name we sent (NO-SUCH-TARGET); instead stash the follow-ups
    // and apply them when CHANNEL-LAYOUT echoes the canonical name (see
    // `reconcileChannelCreate`). Channel creation is server-side only here.
    if (!slug || !activeServer) {
      newChanOpen = false;
      return;
    }
    const full = `#${activeServer}/${slug}`;
    const key = `${activeServer}|${slug}`;
    pendingChanCreate[key] = {
      cat: newChanCategory.trim(),
      announce: newChanAnnounce,
      voice: newChanVoice,
    };
    weft
      .channelCreate(full, newChanVoice ? undefined : newChanRet || undefined, newChanVoice ? "voice" : undefined)
      .catch((e) => {
        delete pendingChanCreate[key];
        toast(String(e), "error");
      });
    newChanOpen = false;
  }
  // Finish a channel-create once the server echoes the canonical name + vanity.

  // ---- categories (Discord-style groupings) ----
  // A category is just a label channels carry (§6.3 CHANNEL META category). An
  // *empty* category has no channel yet, so we remember it client-side (per
  // server) until a channel is dragged in — then the server persists it.
  let newCatOpen = $state(false);
  let newCatName = $state("");
  // Categories are server state (§6.3, on the namespace) — no client copy.
  const nsCategories = () => store.servers.get(activeServer)?.categories ?? [];
  function setCategories(list: string[]) {
    if (activeServer) weft.nsMeta(activeServer, "categories", list.join(",")).catch((e) => toast(String(e), "error"));
  }
  function createCategory() {
    const n = newCatName.trim();
    if (!n || !activeServer) return;
    if (!nsCategories().includes(n)) setCategories([...nsCategories(), n]);
    newCatName = "";
    newCatOpen = false;
  }
  function openCreateCategory() {
    newCatName = "";
    newCatOpen = true;
    serverMenu = false;
  }
  function openCreateChannelInCat(cat: string) {
    newChanName = "";
    newChanCategory = cat; // "" = uncategorized (bare, top-level)
    newChanAnnounce = false;
    newChanRet = "";
    newChanVoice = false;
    newChanOpen = true;
  }
  function deleteCategory(cat: string) {
    // Uncategorize its channels (back to the bare top-level), then drop the category.
    for (const c of Object.values(channels)) {
      if (c.name.startsWith("#") && nsOf(c.name) === activeServer && (c.category || "") === cat) {
        c.category = undefined;
        weft.channelMeta(c.name, "category", "").catch(() => {});
      }
    }
    setCategories(nsCategories().filter((x) => x !== cat));
  }
  function catCtx(e: MouseEvent, cat: string) {
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
  function listCtx(e: MouseEvent) {
    if (!activeServer) return;
    openCtx(e, [
      { label: "Create channel", icon: "channel", run: () => openCreateChannel() },
      { label: "Create category", icon: "folder", run: openCreateCategory },
    ]);
  }
  // ---- category reordering (drag one category header onto another) ----
  // Only named categories are persisted (§6.3 NS categories list), so only they
  // reorder; the bare top-level group ("") stays put.
  let draggingCat = $state<string | null>(null);
  let catDrop = $state<string | null>(null);
  function moveCategory(dragCat: string, targetCat: string) {
    if (dragCat === targetCat || dragCat === "") return;
    const cats = [...nsCategories()];
    const from = cats.indexOf(dragCat);
    if (from < 0) return;
    cats.splice(from, 1);

    let to = cats.indexOf(targetCat);
    if (to < 0) to = cats.length; // dropped on the implicit group → move to the end
    cats.splice(to, 0, dragCat);

    const s = store.servers.get(activeServer);
    if (s) s.categories = cats; // optimistic; the NS-META echo confirms
    setCategories(cats);
  }

  // ---- per-channel permissions (§6.5 grants at #chan scope, §6.7 restricted) ----
  function chanNsScope() {
    const ns = nsOf(ui.chanPerms ?? "");
    return ns ? `ns:${ns}` : "*";
  }
  // A channel-scoped role/@everyone override's caps (channel roles are named
  // after ns roles; `everyone` is the per-channel baseline).
  const chanRoleCaps = (name: string) =>
    rolesAt(ui.chanPerms ?? "").find((r) => r.name === name)?.caps ?? [];
  // Apply a channel role / @everyone target's full cap set (the editor commits
  // a draft, not per-toggle): a non-empty set upserts the channel role, an
  // empty set deletes it. The ROLES refetch inside createRoleAt/deleteRoleAt
  // reconciles the view.
  function setChanRoleCaps(name: string, color: string, caps: string[]) {
    if (!ui.chanPerms) return;
    (caps.length
      ? createRoleAt(ui.chanPerms, name, color, caps.join(","))
      : deleteRoleAt(ui.chanPerms, name)
    ).catch((e) => toast(String(e), "error"));
  }

  // Individual-member overrides at the channel scope (direct GRANTs).
  const chanMemberGrants = () => store.grants.get(ui.chanPerms ?? "") ?? [];
  const chanMemberCaps = (account: string) =>
    chanMemberGrants().find((g) => g.subject === account)?.caps ?? [];
  // Apply a member override's full cap set. record_grant replaces, so we GRANT
  // the new set (or REVOKE the old one when it empties). Optimistic locally.
  function setChanMemberCaps(account: string, caps: string[]) {
    if (!ui.chanPerms) return;
    const scope = ui.chanPerms;
    const prev = chanMemberCaps(account);

    // Re-set the whole entry (SvelteMap values aren't deeply reactive).
    const list = store.grants.get(scope) ?? [];
    const idx = list.findIndex((g) => g.subject === account);
    if (caps.length) {
      if (idx >= 0) store.grants.set(scope, list.map((g, i) => (i === idx ? { ...g, caps } : g)));
      else store.grants.set(scope, [...list, { subject: account, caps }]);
    } else if (idx >= 0) {
      store.grants.set(scope, list.filter((g) => g.subject !== account));
    }

    (caps.length ? weft.grant(account, scope, caps.join(",")) : weft.revoke(account, scope, prev.join(",")))
      .catch((e) => {
        toast(String(e), "error");
        fetchGrants(scope);
      });
  }

  // Remove a whole channel override target. A role override deletes the
  // channel-scoped role; a member override revokes all their channel caps.
  function removeChanRole(name: string) {
    if (!ui.chanPerms) return;
    deleteRoleAt(ui.chanPerms, name).catch((e) => toast(String(e), "error"));
  }
  function removeChanMember(account: string) {
    if (!ui.chanPerms) return;
    const scope = ui.chanPerms;
    const cur = chanMemberCaps(account);
    store.grants.set(scope, (store.grants.get(scope) ?? []).filter((g) => g.subject !== account));
    if (cur.length) weft.revoke(account, scope, cur.join(",")).catch((e) => toast(String(e), "error"));
  }
  function openChanPerms(channel: string) {
    ui.chanPerms = channel;
    fetchRoles(chanNsScope()); // the namespace's roles (the role picker source)
    fetchRoles(channel); // this channel's role + @everyone overrides
    fetchGrants(channel); // this channel's individual-member overrides
  }
  function toggleRestricted() {
    const ch = ui.chanPerms ? channels[ui.chanPerms] : undefined;
    if (!ch || !ui.chanPerms) return;
    const next = !ch.restricted;
    weft
      .channelMeta(ui.chanPerms, "posting", next ? "restricted" : "open")
      .then(() => (ch.restricted = next))
      .catch((e) => toast(String(e), "error"));
  }
  // §6.3 view-gate: when on, the channel is hidden from anyone without the
  // `view` cap (invariant 1 anti-enumeration). Grant `view` per target in the
  // permissions editor to let specific roles/members in.
  function toggleViewGated() {
    const ch = ui.chanPerms ? channels[ui.chanPerms] : undefined;
    if (!ch || !ui.chanPerms) return;
    const next = !ch.viewGated;
    weft
      .channelMeta(ui.chanPerms, "view-gated", next ? "true" : "false")
      .then(() => (ch.viewGated = next))
      .catch((e) => toast(String(e), "error"));
  }

  // ---- admin channel move (drag-and-drop) ----
  let draggingChan = $state<string | null>(null);
  let dropTarget = $state<{ name: string; after: boolean } | null>(null);
  function moveChannel(dragName: string, targetCat: string, anchorName?: string, after = false) {
    const dragged = channels[dragName];
    if (!dragged) return;
    // "" = uncategorized (bare top-level group).
    const storedCat = targetCat;
    dragged.category = storedCat || undefined; // optimistic
    weft.channelMeta(dragName, "category", storedCat).catch((e) => toast(String(e), "error"));
    // Renumber the target category so positions are stable + ordered.
    const list = Object.values(channels)
      .filter(
        (c) =>
          c.name.startsWith("#") &&
          nsOf(c.name) === activeServer &&
          (c.category || "") === targetCat &&
          c.name !== dragName,
      )
      .sort((a, b) => (a.position ?? 0) - (b.position ?? 0) || a.name.localeCompare(b.name));
    let at = anchorName ? list.findIndex((c) => c.name === anchorName) : -1;
    if (at < 0) at = list.length;
    else if (after) at += 1;
    list.splice(at, 0, dragged);
    list.forEach((c, i) => {
      if (c.position !== i) {
        c.position = i;
        weft.channelMeta(c.name, "position", String(i)).catch(() => {});
      }
    });
  }

  // Pins (§6.4)
  function togglePin(m: Msg) {
    if (!m.msgid) return;
    const pinned = activeChannel?.pinnedIds?.includes(m.msgid) ?? false;
    weft.pin(m.msgid, !pinned).catch((e) => toast(String(e), "error"));
  }
  function openPins() {
    if (!active.startsWith("#")) return;
    store.pins.open = true;
    store.pins.list = [];
    store.pins.loadingChannel = active;
    weft.pins(active).catch(() => {});
  }

  // ---- message search (§6.4) — `SearchModal` owns the query/jump; this just
  // opens the panel on the active channel. Both stream server results (routed
  // by the reducer into `store.search` / `store.pins`).
  function openSearch() {
    if (active.startsWith("#")) store.search.begin(active);
  }

  // ---- threads (§9.4) ----
  // How many loaded replies a root has (its thread size), for the indicator.
  const threadCount = (msgid?: string): number =>
    !msgid || !activeChannel ? 0 : activeChannel.messages.filter((m) => m.thread === msgid).length;
  function openThread(root: Msg) {
    if (!root.msgid) return;
    store.threads.root = root;
    store.threads.messages = [root];
    store.threads.composer = "";
    store.threads.loadingRoot = root.msgid;
    weft.history(active, undefined, root.msgid).catch((e) => {
      store.threads.loadingRoot = null;
      toast(String(e), "error");
    });
  }
  function closeThread() {
    store.threads.root = null;
    store.threads.messages = [];
    store.threads.loadingRoot = null;
    store.threads.buf = [];
  }
  function sendThread() {
    const text = store.threads.composer.trim();
    const root = store.threads.root?.msgid;
    if (!text || !root || !active) return;
    weft
      .sendMessage(active, text, undefined, [], root)
      .then(() => (store.threads.composer = ""))
      .catch((e) => toast(String(e), "error"));
  }
  // Main timeline hides thread replies (they live in the thread panel), Discord-style.
  const visibleMessages = $derived(activeChannel?.messages.filter((m) => !m.thread) ?? []);
  // Newest-first, for the `column-reverse` bottom-anchored message list.
  const visibleMessagesReversed = $derived(visibleMessages.slice().reverse());
  // Close the thread panel when the active channel changes.
  let threadChannel = "";
  $effect(() => {
    if (active !== threadChannel) {
      threadChannel = active;
      closeThread();
      store.threads.listOpen = false;
    }
  });

  // ---- threads list (§9.4): all threads in the active channel ----
  function openThreads() {
    if (!active.startsWith("#")) return;
    store.threads.listOpen = true;
    store.threads.list = [];
    store.threads.listBuf = [];
    store.threads.loadingList = true;
    weft.listThreads(active).catch((e) => {
      store.threads.loadingList = false;
      toast(String(e), "error");
    });
  }
  function closeThreads() {
    store.threads.listOpen = false;
  }
  // Open a thread from the list. If its root is already in the timeline, reuse
  // it; otherwise seed a placeholder — the thread HISTORY (which includes the
  // root) replaces it on arrival.
  function openThreadByRoot(info: ThreadInfo) {
    store.threads.listOpen = false;
    const loaded = activeChannel?.messages.find((m) => m.msgid === info.root);
    if (loaded) {
      openThread(loaded);
      return;
    }
    openThread(mkMsg({ author: "", body: "", time: "", ts: 0, own: false, msgid: info.root }));
  }
  // A thread's display name (from THREAD / THREAD-NAMED), for the indicator
  // and the panel title.
  const threadNameFor = (msgid?: string): string | undefined => store.threads.nameFor(msgid);
  // Rename (or, with an empty string, clear the name of) the open thread.
  function renameThread(name: string) {
    const root = store.threads.root?.msgid;
    if (!root || !active) return;
    weft.nameThread(active, root, name.trim()).catch((e) => toast(String(e), "error"));
  }

  // Namespace admin
  function openNsSettings() {
    const meta = store.servers.get(activeServer);
    nsTitle = meta?.title ?? "";
    nsDesc = meta?.description ?? "";
    nsVis = meta?.visibility ?? "public";
    nsDelegSubject = "";
    nsNewOwner = "";
    nsRecKeys = "";
    ui.nsTab = "overview";
    ui.nsSettingsOpen = true;
    fetchRoles(nsRoleScope());
  }
  function saveNsMeta() {
    if (nsTitle.trim()) weft.nsMeta(activeServer, "title", nsTitle.trim()).catch(() => {});
    if (nsDesc.trim()) weft.nsMeta(activeServer, "description", nsDesc.trim()).catch(() => {});
    weft.nsVisibility(activeServer, nsVis).catch(() => {});
  }
  // §11.10 open/close this namespace to on-demand federation (needs public).
  function nsSetFederation(open: boolean) {
    weft.nsMeta(activeServer, "federation", open ? "open" : "closed").catch((e) => toast(String(e), "error"));
  }
  // §6.2 set (or clear, "") the channel that greets new members.
  function nsSetWelcome(channel: string) {
    if (!activeServer) return;
    weft.nsMeta(activeServer, "welcome", channel).catch((e) => toast(String(e), "error"));
  }
  // §11.10 on-demand federation: live "connecting…" state for the trigger. The
  // bridge establishes asynchronously; we surface the namespace when its
  // channels arrive (best-effort), else the banner clears after a grace window.
  let federating = $state<{ target: string; ns: string } | null>(null);
  let federatingTimer: ReturnType<typeof setTimeout> | null = null;
  function federate(target: string, invite?: string) {
    const t = target.trim();
    const slash = t.indexOf("/");
    if (slash < 1) {
      toast("Enter a foreign namespace as network/namespace", "error");
      return;
    }
    const ns = t.slice(slash + 1);
    weft
      .federate(t, invite?.trim() || undefined)
      .then(() => {
        federating = { target: t, ns };
        if (federatingTimer) clearTimeout(federatingTimer);
        federatingTimer = setTimeout(() => (federating = null), 20000);
      })
      .catch((e) => toast(String(e), "error"));
  }
  function cancelFederating() {
    if (federatingTimer) clearTimeout(federatingTimer);
    federating = null;
  }
  // When the bridged namespace's channels surface, open it and clear the banner.
  $effect(() => {
    const f = federating;
    if (!f) return;
    if (Object.keys(channels).some((c) => nsOf(c) === f.ns)) {
      cancelFederating();
      selectServer(f.ns);
    }
  });
  async function doTransfer() {
    const o = nsNewOwner.trim();
    if (o && (await appConfirm(`Transfer ownership of ${activeServer} to ${o}? This is signed by your root key and cannot be undone.`, "Transfer")))
      weft.nsTransfer(network, activeServer, o).catch((e) => (cf.authError = String(e)));
  }
  async function deleteNamespace() {
    if (await appConfirm(`Delete namespace ${activeServer}? This removes all its channels.`, "Delete")) {
      weft.nsDelete(activeServer).catch((e) => toast(String(e), "error"));
      ui.nsSettingsOpen = false;
    }
  }

  // Revoke every outstanding invite for the active namespace (ns-admin, §6.5).
  async function revokeAllInvites() {
    if (!activeServer) return;
    if (!(await appConfirm(`Revoke ALL invites for ${activeServer}? Every existing invite link stops working.`, "Revoke all"))) return;
    weft.inviteRevokeAll(`ns:${activeServer}`).catch(() => {});
    store.invites.list = []; // optimistic — the list is now empty
    toast(`Revoked all invites for ${activeServer}`, "info");
  }

  onMount(() => {
    // Restore the cached layout for instant render before the server refresh.
    loadLayoutCache();
    // Restore theme.
    try {
      if (localStorage.getItem("weft:theme") === "light") {
        theme = "light";
        document.documentElement.dataset.theme = "light";
      }
    } catch {
      /* ignore */
    }
    const un = weft.onWeft(handle);
    // Confirm-before-navigate guard for links in rendered message markdown.
    const uninstallLinkGuard = installLinkGuard();
    // Load client.toml: TLS verification mode + optional default host. The
    // config host only prefills the picker when the user has no saved homeserver.
    weft
      .clientConfig()
      .then((c) => {
        cf.insecure = c.allow_insecure;
        if (
          c.default_host &&
          !weft.isWeb &&
          cf.host === "127.0.0.1:4433" &&
          !localStorage.getItem(HOMESERVER_KEY)
        )
          cf.host = c.default_host;
      })
      .catch(() => {});
    // Restore the saved homeserver + last session. Desktop: a remembered
    // homeserver skips the picker (→ auth step); a full session logs straight
    // back in. Otherwise land on the picker (desktop) / auth (web).
    try {
      if (!weft.isWeb) {
        const savedHost = localStorage.getItem(HOMESERVER_KEY);
        if (savedHost) {
          cf.host = savedHost;
          cf.serverStep = "auth";
        }
      }

      const saved = JSON.parse(localStorage.getItem(SAVED_KEY) ?? "null");
      // On web the network is always the page origin — don't restore a stale host.
      if (saved?.host && !weft.isWeb) cf.host = saved.host;
      if (saved?.account) cf.account = saved.account;

      if (saved?.host && saved?.account && saved?.password) {
        // A full session → log straight back in; no picker, no probe.
        cf.password = saved.password;
        cf.mode = "login";
        cf.serverStep = "auth";
        doConnect();
      } else if (cf.serverStep === "auth") {
        // Have a homeserver but nothing to auto-login with → probe it so the
        // register form knows whether to require an email.
        void probeServer();
      }
    } catch {
      /* ignore */
    }
    return () => {
      un.then((f) => f());
      uninstallLinkGuard();
    };
  });

  // ---- shared context for extracted components (state via getters, actions
  // as refs). Grows as more components are extracted. ----
  provideApp({
    get network() { return network; },
    get account() { return account; },
    get myStatus() { return myStatus; },
    get homeView() { return homeView; },
    get activeServer() { return activeServer; },
    get active() { return active; },
    get activeChannel() { return activeChannel; },
    get activeIsDm() { return activeIsDm; },
    get activeIsGroup() { return activeIsGroup; },
    get serverNamespaces() { return serverNamespaces; },
    serverName,
    get channelGroups() { return channelGroups; },
    get dmList() { return dmList; },
    get activeNsMeta() { return activeNsMeta; },
    // social layer: friends
    get friendList() { return friendList; },
    get incomingRequests() { return incomingRequests; },
    get outgoingRequests() { return outgoingRequests; },
    get addFriendInput() { return addFriendInput; },
    set addFriendInput(v: string) { addFriendInput = v; },
    friendLabel,
    friendLocalAccount,
    addFriend,
    acceptFriend,
    removeFriend,
    messageFriend,
    openFriends,
    // group DMs
    get groupList() { return groupList; },
    get newGroupInput() { return newGroupInput; },
    set newGroupInput(v: string) { newGroupInput = v; },
    groupLabel,
    createGroup,
    openGroupPicker,
    openGroup,
    leaveGroup,
    addToGroup,
    // group calls
    get groupCallRoster() { return store.social.groupCallRoster; },
    get activeGroupCall() { return store.social.activeGroupCall; },
    startGroupCall,
    leaveGroupCall,
    // friend calls
    get incomingCall() { return store.social.incomingCall; },
    get activeCall() { return store.social.activeCall; },
    get callMuted() { return callMedia.muted; },
    get callConnecting() { return callMedia.connecting; },
    callUser,
    acceptCall,
    declineCall,
    endCall,
    toggleCallMute,
    goHome,
    selectServer,
    openServerMenu,
    nsLeave,
    open: (name: string) => { markRead(name); goto(nav.pathFor(name)); },
    // Open a voice channel's stage (switch the main view) and join the call if
    // we're not already in it. Voice channels have no message timeline, so we
    // don't markRead.
    openVoice: (name: string) => {
      if (voice.channel !== name) joinVoice(name);
      goto(nav.pathFor(name));
    },
    openDiscover,
    get channels() { return channels; },
    accountOf: (handle: string) => store.accountOf(handle),
    isMuted,
    serverMuted,
    notifLevelOf,
    setNotifLevel,
    notifScopeKey,
    notifScopeLabel,
    get notifSettingsOpen() { return notifSettingsOpen; },
    set notifSettingsOpen(v: boolean) { notifSettingsOpen = v; },
    openNotifSettings,
    get discoverList() { return [...store.servers.values()].filter((s) => s.metaLoaded); },
    get discoverCursor() { return ui.discoverCursor; },
    scopesFor,
    markRead,
    get draggingChan() { return draggingChan; },
    set draggingChan(v: string | null) { draggingChan = v; },
    get dropTarget() { return dropTarget; },
    set dropTarget(v: { name: string; after: boolean } | null) { dropTarget = v; },
    moveChannel,
    initials,
    avatarUrl,
    displayName,
    bioOf,
    statusOf,
    setCustomStatus,
    queryProfile,
    nickOf,
    setNick,
    chanShort,
    titleOf,
    isNsMember: (nsId: string) => store.servers.get(nsId)?.joined ?? false,
    peerOf,
    dotClass,
    nsOf,
    badgeFor,
    serverUnread,
    serverMention,
    serverMentionCount,
    retentionMeta,
    chanCtx,
    userCtx,
    groupCtx,
    closeDm,
    catCtx,
    listCtx,
    moveCategory,
    get draggingCat() { return draggingCat; },
    set draggingCat(v: string | null) { draggingCat = v; },
    get catDrop() { return catDrop; },
    set catDrop(v: string | null) { catDrop = v; },
    get serverMenu() { return serverMenu; },
    set serverMenu(v: boolean) { serverMenu = v; },
    get userMenu() { return userMenu; },
    set userMenu(v: boolean) { userMenu = v; },
    openCreateChannel,
    openCreateChannelInCat,
    openNsSettings,
    openServerProfile,
    mintInvite,
    // invites menu (Discord-style) — state on `store.invites`
    get invitesList() { return store.invites.list; },
    get invitesScope() { return store.invites.scope; },
    openInvites,
    loadNsInvites,
    revokeInvite,
    createInvite,
    inviteLinkFor,
    // invite creation screen
    get inviteLink() { return store.invites.link; },
    get inviteId() { return store.invites.id; },
    get inviteCreateScope() { return store.invites.createScope; },
    generateInvite,
    sendInviteDM,
    newCat: openCreateCategory,
    openProfile,
    openFullProfile,
    openNickDialog,
    mutualServers,
    friendState,
    friendAction,
    openDm,
    moderate,
    openSettings: () => { userTab = "account"; settingsOpen = true; userMenu = false; },
    toast,
    confirm: appConfirm,
    expectSuccess,
    // chat topbar
    get membersVisible() { return membersVisible; },
    set membersVisible(v: boolean) { membersVisible = v; },
    openPins,
    openReports,
    partActive: () => weft.part(active).catch(() => {}),
    // search + pins panels own their state on `store.search` / `store.pins`.
    openSearch,
    // threads — state on `store.threads`
    get threadRoot() { return store.threads.root; },
    get threadMessages() { return store.threads.messages; },
    get threadComposer() { return store.threads.composer; },
    set threadComposer(v: string) { store.threads.composer = v; },
    get visibleMessages() { return visibleMessages; },
    get visibleMessagesReversed() { return visibleMessagesReversed; },
    threadCount,
    openThread,
    closeThread,
    sendThread,
    // threads list (§9.4)
    get threadsOpen() { return store.threads.listOpen; },
    get threadsList() { return store.threads.list; },
    openThreads,
    closeThreads,
    openThreadByRoot,
    threadNameFor,
    renameThread,
    // custom emoji (§9.4)
    get activeEmoji() { return activeEmoji; },
    addEmoji,
    removeEmoji,
    emojiUrlFor,
    // message list / items
    get loadingHistory() { return hist.loading; },
    get newBoundary() { return newBoundary; },
    channelRecord,
    loadHistory,
    get editingKey() { return editingKey; },
    set editingKey(v: number | null) { editingKey = v; },
    get editDraft() { return editDraft; },
    set editDraft(v: string) { editDraft = v; },
    get pickerKey() { return pickerKey; },
    set pickerKey(v: number | null) { pickerKey = v; },
    get replyTo() { return ui.replyTo; },
    set replyTo(v: Msg | null) { ui.replyTo = v; },
    startEdit,
    saveEdit,
    cancelEdit,
    editKey,
    doDelete,
    openReport,
    togglePin,
    toggleReaction,
    jumpTo,
    msgCtx,
    renderMd: (t: string) => md.renderMd(t, mdContext()),
    mentionsMe,
    dayKey,
    dayLabel,
    get newDividerKey() { return newDividerKey; },
    // composer
    get composer() { return composer; },
    set composer(v: string) { composer = v; },
    composerKey,
    onComposerInput,
    doSend,
    pickMention,
    get emojiQuery() { return emojiQuery; },
    get emojiSuggestions() { return emojiSuggestions; },
    get emojiIndex() { return emojiIndex; },
    set emojiIndex(v: number) { emojiIndex = v; },
    pickEmojiSuggestion,
    get pendingAttachments() { return pendingAttachments; },
    attachFile,
    pasteFiles,
    dropFiles,
    removeAttachment,
    mediaUrl: weft.mediaUrl,
    get mentionQuery() { return mentionQuery; },
    get mentionMatches() { return mentionMatches; },
    get mentionIndex() { return mentionIndex; },
    set mentionIndex(v: number) { mentionIndex = v; },
    get typingLabel() { return typingLabel; },
    // roles (ProfileCard)
    rolesAt,
    rolesOf,
    roleById,
    ensureMemberRoles,
    ensureRoles,
    nsMembers: (ns: string) => store.servers.get(ns)?.members ?? [],
    get nsMembersLoading() { return activeServer ? (store.servers.get(activeServer)?.membersLoading ?? false) : false; },
    fetchNsMembers,
    assignNsRole,
    unassignNsRole,
    nsMemberCtx,
    roleScopeOf,
    isOwnerAt,
    isNsOwner,
    isStaff,
    canModerate,
    serverCap,
    serverCanGrant,
    canOpenServerSettings,
    nameColor,
    assignRoleTo,
    unassignRoleFrom,
    // channel permissions (per-target: @everyone / role / member)
    chanNsScope,
    chanRoleCaps,
    setChanRoleCaps,
    chanMemberGrants,
    chanMemberCaps,
    setChanMemberCaps,
    removeChanRole,
    removeChanMember,
    toggleRestricted,
    toggleViewGated,
    // federation (operator)
    get isOperator() { return isOperator; },
    get netblocks() { return store.federation.netblocks; },
    get manifests() { return store.federation.manifests; },
    openFederation,
    refreshNetblocks,
    netblockAdd,
    netblockRemove,
    bridgePropose,
    bridgeAccept,
    bridgeSever,
    // user settings
    get theme() { return theme; },
    get host() { return cf.host; },
    get reconnecting() { return ui.reconnecting; },
    setStatus,
    toggleTheme,
    enrollThisDevice: enrollThisDevice,
    logout,
    // user settings (page overlay)
    get userTab() { return userTab; },
    set userTab(v: "account" | "appearance" | "connection" | "verification") { userTab = v; },
    get verifications() { return store.session.verifications; },
    // server settings (ns overlay)
    get nsTab() { return ui.nsTab; },
    set nsTab(v: "overview" | "roles" | "members" | "emoji" | "invites" | "bans" | "federation" | "recovery" | "danger") { ui.nsTab = v; },
    denyList,
    refreshBans,
    liftMod,
    get nsTitle() { return nsTitle; },
    set nsTitle(v: string) { nsTitle = v; },
    get nsDesc() { return nsDesc; },
    set nsDesc(v: string) { nsDesc = v; },
    get nsVis() { return nsVis; },
    set nsVis(v: string) { nsVis = v; },
    get newRoleName() { return newRoleName; },
    set newRoleName(v: string) { newRoleName = v; },
    get newRoleColor() { return newRoleColor; },
    set newRoleColor(v: string) { newRoleColor = v; },
    get newRoleCaps() { return newRoleCaps; },
    get newRoleHoist() { return newRoleHoist; },
    set newRoleHoist(v: boolean) { newRoleHoist = v; },
    get newRolePingable() { return newRolePingable; },
    set newRolePingable(v: boolean) { newRolePingable = v; },
    toggleNewRoleCap,
    get nsDelegSubject() { return nsDelegSubject; },
    set nsDelegSubject(v: string) { nsDelegSubject = v; },
    get nsNewOwner() { return nsNewOwner; },
    set nsNewOwner(v: string) { nsNewOwner = v; },
    get nsRecM() { return nsRecM; },
    set nsRecM(v: number) { nsRecM = v; },
    get nsRecKeys() { return nsRecKeys; },
    set nsRecKeys(v: string) { nsRecKeys = v; },
    get myRecoveryKey() { return myRecoveryKey; },
    get recoveryDoc() { return recoveryDoc; },
    set recoveryDoc(v: string) { recoveryDoc = v; },
    nsRoleScope,
    saveNsMeta,
    nsSetFederation,
    nsSetWelcome,
    federate,
    createRole,
    moveRole,
    reorderRoles,
    saveRole,
    deleteRole,
    everyoneCaps,
    setEveryoneCaps,
    assignRole,
    showRecoveryKey,
    startRecovery,
    cosignRecovery,
    submitRecovery,
    doTransfer,
    deleteNamespace,
    revokeAllInvites,
  });

  // SvelteKit renders the active route (the main-area view) as `children`.
  let { children }: { children: import("svelte").Snippet } = $props();
</script>

<svelte:window onkeydown={globalKey} />

{#if status !== "online"}
  <ConnectScreen
    form={cf}
    {status}
    canChangeServer={!weft.isWeb}
    onconnect={doConnect}
    onkeylogin={keyLogin}
    onchooseserver={chooseServer}
    onchangeserver={changeServer}
  />
{:else}
  <!-- ================= MAIN APP ================= -->
  {#if ui.reconnecting}
    <div class="reconnect-banner">Connection lost — ui.reconnecting…</div>
  {:else if needsEmailWarning}
    <div class="email-banner">
      <span>⚠ No email is on file for this account — you won't be able to reset your password.</span>
      <button class="email-banner-btn" onclick={openVerification}>Add email</button>
      <button class="email-banner-close" aria-label="Dismiss" title="Dismiss" onclick={dismissEmailBanner}>✕</button>
    </div>
  {/if}
  <Toasts {toasts} />
  <Lightbox />
  <LinkWarningModal />
  {#if voiceUI.cameraPicker}<CameraPicker />{/if}
  {#if voiceUI.screenPicker}<ScreenPicker />{/if}
  {#if voiceUI.screenMenu}<ScreenShareMenu />{/if}
  <ThreadPanel />
  {#if federating}
    <div class="federating-banner">
      <span class="fed-spinner"></span>
      Connecting to <b>{federating.target}</b>…
      <button class="linkish" onclick={cancelFederating}>dismiss</button>
    </div>
  {/if}
  <ContextMenu menu={ctxMenu} onclose={() => (ctxMenu = null)} />
  {#if switcherOpen}
    <QuickSwitcher
      bind:query={switcherQuery}
      results={switcherResults.map((c) => ({
        name: c.name,
        label: c.name.startsWith("@") ? peerOf(c.name) : chanShort(c.name),
        sigil: c.name.startsWith("@") ? "@" : "#",
        unread: c.unread,
      }))}
      onselect={switchTo}
      onclose={() => (switcherOpen = false)}
    />
  {/if}
  <div
    class="app"
    class:members-collapsed={!membersVisible || activeChannel?.voice}
    class:with-top-banner={needsEmailWarning && !ui.reconnecting}
  >
    <!-- COMMUNITY RAIL -->
    <CommunityRail />

    <!-- SIDEBAR -->
    <aside class="sidebar">
      <SidebarHeader />
      {#if homeView}
        <DmList />
        <SidebarInput bind:value={dmInput} placeholder="message @user…" onenter={startDm} />
      {:else}
        {#key activeServer}
          <ChannelList />
        {/key}
        <SidebarInput bind:value={joinInput} placeholder="join #channel or namespace…" onenter={doJoin} />
      {/if}
      <VoiceBar />
      <UserFooter />
    </aside>

    <!-- MAIN -->
    <main class="main">
      {@render children()}
    </main>

    <!-- MEMBERS -->
    <aside class="members">
      {#if activeChannel && !activeIsDm && !activeChannel.voice}
        <MemberList />
      {/if}
    </aside>

    {#if discoverOpen}
      <DiscoverModal onclose={() => (discoverOpen = false)} />
    {/if}

    {#if store.reports.target}
      <ReportModal target={store.reports.target} onclose={() => (store.reports.target = null)} />
    {/if}


    {#if store.reports.open}
      <ReportsQueueModal onclose={() => (store.reports.open = false)} />
    {/if}

    {#if store.invites.createOpen}
      <InviteCreateModal onclose={() => { store.invites.createOpen = false; store.invites.link = null; store.invites.id = null; }} />
    {/if}

    {#if store.invites.listOpen}
      <InvitesModal onclose={() => (store.invites.listOpen = false)} />
    {/if}

    {#if groupPickerOpen}
      <NewGroupModal
        seed={groupPickerSeed}
        pos={groupPickerPos}
        onclose={() => (groupPickerOpen = false)}
        oncreate={createGroupWith}
      />
    {/if}

    {#if store.pins.open}
      <PinsModal onclose={() => (store.pins.open = false)} />
    {/if}

    {#if store.threads.listOpen}
      <ThreadsModal onclose={() => (store.threads.listOpen = false)} />
    {/if}

    {#if store.search.open}
      <SearchModal onclose={() => (store.search.open = false)} />
    {/if}

    {#if newChanOpen}
      <CreateChannelModal
        bind:name={newChanName}
        bind:category={newChanCategory}
        bind:announce={newChanAnnounce}
        bind:retention={newChanRet}
        bind:voice={newChanVoice}
        {activeServer}
        categories={channelGroups.map((g) => g.category)}
        onclose={() => (newChanOpen = false)}
        oncreate={createChannel}
      />
    {/if}

    {#if newCatOpen}
      <CreateCategoryModal bind:name={newCatName} onclose={() => (newCatOpen = false)} oncreate={createCategory} />
    {/if}

    {#if ui.chanPerms}
      <ChannelSettings channel={ui.chanPerms} onclose={() => (ui.chanPerms = null)} />
    {/if}

    {#if profileTarget}
      <ProfileCard target={profileTarget} pos={profilePos} onclose={() => (profileTarget = null)} />
    {/if}

    {#if nickTarget}
      <NicknameModal target={nickTarget} onclose={() => (nickTarget = null)} />
    {/if}

    {#if confirmState}
      <ConfirmModal message={confirmState.message} confirmLabel={confirmState.label} onresult={resolveConfirm} />
    {/if}

    {#if profileModalTarget}
      <ProfileModal target={profileModalTarget} onclose={() => (profileModalTarget = null)} />
    {/if}

    {#if settingsOpen}
      <UserSettingsModal onclose={() => (settingsOpen = false)} />
    {/if}

    {#if federationOpen}
      <FederationPanel onclose={() => (federationOpen = false)} />
    {/if}

    {#if ui.nsSettingsOpen}
      <ServerSettingsModal onclose={() => (ui.nsSettingsOpen = false)} />
    {/if}

    {#if serverProfileOpen}
      <ServerProfileModal onclose={() => (serverProfileOpen = false)} />
    {/if}

    {#if notifSettingsOpen}
      <NotificationSettingsModal onclose={() => (notifSettingsOpen = false)} />
    {/if}

    <CallOverlay />
  </div>
{/if}
