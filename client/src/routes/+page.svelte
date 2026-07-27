<script lang="ts">
  import { onMount, tick, untrack } from "svelte";
  import * as weft from "$lib/weft";
  import { EVERYONE_ROLE } from "$lib/constants";
  import type { Msg, Channel, CtxItem, RoleDefC, ThreadInfo, MentionOpt, MemberInfoC } from "$lib/types";
  import { provideApp } from "$lib/context";
  import { highlightCode } from "$lib/highlight";
  import { shortcodeToChar, searchUnicode } from "$lib/shortcodes";
  import { installLinkGuard } from "$lib/linkguard.svelte";
  import LinkWarningModal from "$lib/components/modals/LinkWarningModal.svelte";
  import ConnectScreen from "$lib/components/ConnectScreen.svelte";
  import Toasts from "$lib/components/Toasts.svelte";
  import ContextMenu from "$lib/components/ContextMenu.svelte";
  import QuickSwitcher from "$lib/components/QuickSwitcher.svelte";
  import CommunityRail from "$lib/components/CommunityRail.svelte";
  import EmptyHome from "$lib/components/EmptyHome.svelte";
  import FriendsView from "$lib/components/FriendsView.svelte";
  import MemberList from "$lib/components/MemberList.svelte";
  import { initVoice, joinVoice, voice } from "$lib/voice.svelte";
  import {
    callMedia,
    connectCallMedia,
    disconnectCallMedia,
    toggleCallMute,
  } from "$lib/callmedia.svelte";
  import VoiceBar from "$lib/components/VoiceBar.svelte";
  import VoiceStage from "$lib/components/chat/VoiceStage.svelte";
  import CameraPicker from "$lib/components/modals/CameraPicker.svelte";
  import ScreenPicker from "$lib/components/modals/ScreenPicker.svelte";
  import ScreenShareMenu from "$lib/components/modals/ScreenShareMenu.svelte";
  import { voiceUI } from "$lib/voiceui.svelte";
  import ChannelList from "$lib/components/sidebar/ChannelList.svelte";
  import SidebarHeader from "$lib/components/sidebar/SidebarHeader.svelte";
  import DmList from "$lib/components/sidebar/DmList.svelte";
  import UserFooter from "$lib/components/sidebar/UserFooter.svelte";
  import SidebarInput from "$lib/components/sidebar/SidebarInput.svelte";
  import ChatTopbar from "$lib/components/chat/ChatTopbar.svelte";
  import MessageList from "$lib/components/chat/MessageList.svelte";
  import Composer from "$lib/components/chat/Composer.svelte";
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
  import UserSettingsModal from "$lib/components/modals/UserSettingsModal.svelte";
  import FederationPanel from "$lib/components/modals/FederationPanel.svelte";
  import ServerSettingsModal from "$lib/components/modals/ServerSettingsModal.svelte";
  import ServerProfileModal from "$lib/components/modals/ServerProfileModal.svelte";
  import NotificationSettingsModal from "$lib/components/modals/NotificationSettingsModal.svelte";

  // ---- connection + form state ----
  type Status = "connect" | "connecting" | "online";
  let status = $state<Status>("connect");
  let network = $state("");
  let account = $state("");
  let authError = $state("");
  // AUTH-FAILED is followed by the server closing the stream; this flag lets the
  // `closed` handler keep the specific auth reason instead of clobbering it with
  // a generic "connection closed".
  let authFailed = false;

  let mode = $state<weft.Mode>("login");
  // Web build: the network is wherever the page was served from (same-origin,
  // P3 embed); desktop: a QUIC host the user types. The web value is display-only
  // — the WASM backend derives its WS URL from window.location regardless.
  let host = $state(
    weft.isWeb && typeof window !== "undefined" ? window.location.host : "127.0.0.1:4433",
  );
  let formAccount = $state("");
  let formPassword = $state("");
  // client.toml: TLS mode (verified by default) + optional prefill host.
  let insecureMode = $state(false);

  // ---- session lifecycle (Phase 8) ----
  const SAVED_KEY = "weft:last-connect";
  let lastCreds: { host: string; account: string; password: string } | null = null;
  let manualLogout = false;
  let reconnecting = $state(false);
  let reconnectAttempts = 0;
  let toasts = $state<{ id: number; text: string; kind: string }[]>([]);
  let toastSeq = 0;
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
    if (name.startsWith("@")) homeView = true;
    else homeView = false;
    active = name;
  }
  function globalKey(e: KeyboardEvent) {
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k") {
      e.preventDefault();
      switcherOpen = true;
      switcherQuery = "";
    } else if (e.key === "Escape") {
      switcherOpen = false;
      pinsOpen = false;
      discoverOpen = false;
      settingsOpen = false;
      nsSettingsOpen = false;
      profileTarget = null;
      ctxMenu = null;
      serverMenu = false;
      userMenu = false;
      newChanOpen = false;
      newCatOpen = false;
      chanPermsCh = null;
    }
  }
  // ---- right-click context menus ----
  let ctxMenu = $state<{ x: number; y: number; items: CtxItem[] } | null>(null);
  function openCtx(e: MouseEvent, items: CtxItem[]) {
    e.preventDefault();
    e.stopPropagation(); // don't let a channel/category menu bubble to the list background
    ctxMenu = { x: Math.min(e.clientX, window.innerWidth - 220), y: e.clientY, items };
  }
  function msgCtx(e: MouseEvent, m: Msg) {
    if (m.system || !m.msgid) return;
    const items: CtxItem[] = [{ label: "Reply", run: () => (replyTo = m) }];
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
      items.push({ label: "Report", run: () => openReport(m) });
    }
    openCtx(e, items);
  }
  function chanCtx(e: MouseEvent, ch: Channel) {
    const muted = isMuted(ch.name);
    openCtx(e, [
      { header: ch.name },
      { label: "Mark as read", icon: "markread", run: () => markRead(ch.name) },
      {
        label: muted ? "Unmute channel" : "Mute channel",
        icon: muted ? "unmute" : "mute",
        run: () => setNotifLevel(scopeKeyOf(ch.name), muted ? "mentions" : "nothing"),
      },
      { label: "Copy name", icon: "copy", run: () => navigator.clipboard?.writeText(ch.name) },
      { label: "Create invite", icon: "invite", run: () => openInviteCreate(scopesFor()[0]) },
      { divider: true },
      { header: "Mod Menu", mod: true },
      { label: "Edit permissions", icon: "permissions", run: () => openChanPerms(ch.name) },
      { divider: true },
      {
        label: "Delete channel",
        icon: "delete",
        danger: true,
        run: () => weft.channelDelete(ch.name).catch((err) => toast(String(err), "error")),
      },
    ]);
  }
  // The right-click menu for any user, anywhere (member list, friends, DMs).
  // Items adapt to context: a DM shows Close DM (else Message), a channel adds
  // Invite + moderation (only there is the user a server member you can act on),
  // and a friend shows Remove friend.
  function userCtx(e: MouseEvent, name: string) {
    if (peerOf(name) === account) return; // no menu on yourself
    const ref = qualify(name);
    const items: CtxItem[] = [
      { label: "Open profile", icon: "profile", run: () => openProfile(name) },
      dmOpen(name)
        ? { label: "Close DM", icon: "close", run: () => closeDm(name) }
        : { label: "Message", icon: "message", run: () => openDm(name) },
      { label: "Call", icon: "call", run: () => callUser(ref) },
    ];

    // Friendship: Add when unrelated, Remove when friends, and the sensible
    // action for a pending request either way.
    const rel = friends[ref];
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
      items.push({ header: "Mod Menu", mod: true });
      items.push({ label: "Mute", icon: "mute", run: () => moderate("mute", name) });
      items.push({ label: "Kick", icon: "kick", run: () => moderate("kick", name) });
      items.push({ label: "Ban", icon: "ban", danger: true, run: () => moderate("ban", name) });
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

  function toast(text: string, kind = "info") {
    const id = toastSeq++;
    toasts = [...toasts, { id, text, kind }];
    setTimeout(() => (toasts = toasts.filter((t) => t.id !== id)), 4500);
  }

  // ---- server-confirmed success toasts ----
  // A weft call resolves on *send*, not on server confirmation, so we can't
  // toast success in `.then()` (a missing-cap failure arrives later as an ERR
  // event). Instead an action registers an expected key here; when the matching
  // confirming event lands, `confirmSuccess` fires the toast. Unmatched keys
  // simply expire — a failure just never confirms (and its ERR toasts).
  let pendingSuccess = $state<Record<string, string>>({});
  function expectSuccess(key: string, message: string) {
    pendingSuccess[key] = message;
    // Don't leave a stale expectation if the action silently fails.
    setTimeout(() => delete pendingSuccess[key], 6000);
  }
  function confirmSuccess(key: string) {
    const m = pendingSuccess[key];
    if (m) {
      delete pendingSuccess[key];
      toast(m, "success");
    }
  }

  function attemptReconnect() {
    if (!lastCreds) {
      status = "connect";
      return;
    }
    reconnecting = true;
    const delay = Math.min(1500 * 2 ** reconnectAttempts, 15000);
    reconnectAttempts++;
    setTimeout(() => {
      if (!reconnecting) return; // logged out meanwhile
      // Reconnect always uses login — the account already exists.
      weft.connect(lastCreds!.host, lastCreds!.account, lastCreds!.password, "login").catch(() =>
        attemptReconnect(),
      );
    }, delay);
  }

  function logout() {
    manualLogout = true;
    reconnecting = false;
    lastCreds = null;
    userMenu = false;
    settingsOpen = false;
    weft.disconnect().catch(() => {});
    channels = {};
    keptChannels = [];
    active = "";
    activeServer = "";
    homeView = true;
    discovered = {};
    presence = {};
    reportQueue = {};
    status = "connect";
  }

  // ---- live data (types in $lib/types) ----
  let msgSeq = 0;
  const mkMsg = (m: Omit<Msg, "key">): Msg => ({ ...m, key: msgSeq++ });

  let channels = $state<Record<string, Channel>>({});

  // ---- layout cache (server-authoritative, cached for instant reload) ----
  // Per namespace: the category list + each channel's category/position. The
  // server is the source of truth; this is a cache shown immediately on reload
  // (Discord-style) and refreshed by the CHANNELS fetch.
  type NsLayout = { cats: string[]; chans: Record<string, { category?: string; position?: number }> };
  let layoutCache = $state<Record<string, NsLayout>>({});
  function saveLayoutCache() {
    try {
      localStorage.setItem("weft:layout", JSON.stringify(layoutCache));
    } catch {
      /* ignore */
    }
  }
  function cacheNsCats(ns: string, cats: string[]) {
    (layoutCache[ns] ??= { cats: [], chans: {} }).cats = cats;
    saveLayoutCache();
  }
  function cacheChanLayout(chanName: string, category: string | undefined, position: number) {
    const ns = nsOf(chanName);
    if (!ns) return;
    ((layoutCache[ns] ??= { cats: [], chans: {} }).chans[chanName] = { category, position });
    saveLayoutCache();
  }

  // Unread / mention state kept in top-level reactive maps (keyed by channel
  // name) rather than per-channel fields — guarantees the sidebar re-renders
  // when a badge clears, independent of the channelGroups derivation.
  let unreadMap = $state<Record<string, boolean>>({});
  let mentionMap = $state<Record<string, boolean>>({});
  // Numeric unread / mention tallies (Tier 1) — the badges show counts, not dots.
  let unreadCount = $state<Record<string, number>>({});
  let mentionCount = $state<Record<string, number>>({});
  function markRead(name: string) {
    if (unreadMap[name]) unreadMap[name] = false;
    if (mentionMap[name]) mentionMap[name] = false;
    if (unreadCount[name]) unreadCount[name] = 0;
    if (mentionCount[name]) mentionCount[name] = 0;
  }

  // ---- notification preferences (per-user, localStorage) ----
  // Set per **namespace** (`ns:<name>`, or `net` for top-level) in the
  // Notification Settings modal — not per channel. Effective level =
  // namespace ?? "mentions" (the default keeps "only DMs/@mentions ping").
  type NotifLevel = "all" | "mentions" | "nothing";
  const NOTIF_KEY = "weft:notif-prefs";
  const loadNotifPrefs = (): Record<string, NotifLevel> => {
    try {
      return JSON.parse(localStorage.getItem(NOTIF_KEY) ?? "{}");
    } catch {
      return {};
    }
  };
  let notifPrefs = $state<Record<string, NotifLevel>>(loadNotifPrefs());
  // The namespace scope key for a channel (or the network for top-level).
  const scopeKeyOf = (channel: string) => {
    const ns = nsOf(channel);
    return ns ? `ns:${ns}` : "net";
  };
  const notifLevel = (channel: string): NotifLevel =>
    notifPrefs[scopeKeyOf(channel)] ?? "mentions";
  const isMuted = (channel: string) => notifLevel(channel) === "nothing";
  const serverMuted = (ns: string) => (notifPrefs[ns ? `ns:${ns}` : "net"] ?? "mentions") === "nothing";
  const notifLevelOf = (scopeKey: string): NotifLevel => notifPrefs[scopeKey] ?? "mentions";
  function setNotifLevel(scope: string, level: NotifLevel) {
    notifPrefs[scope] = level;
    notifPrefs = { ...notifPrefs };
    try {
      localStorage.setItem(NOTIF_KEY, JSON.stringify(notifPrefs));
    } catch {
      /* private mode — in-memory only */
    }
  }
  // ---- notification-settings modal (per-namespace) ----
  let notifSettingsOpen = $state(false);
  // The scope the modal edits = the active server (namespace, or the network).
  const notifScopeKey = () => (activeServer ? `ns:${activeServer}` : "net");
  const notifScopeLabel = () => activeServer || network;
  function openNotifSettings() {
    notifSettingsOpen = true;
    serverMenu = false;
  }
  let active = $state("");
  let joinInput = $state("");
  let composer = $state("");
  let membersVisible = $state(true);
  // ---- kept-alive message lists (Discord-style instant switching) ----
  // Each recently-opened text channel keeps its own self-contained <MessageList>
  // mounted (hidden when inactive), so switching back is instant — DOM built,
  // images decoded, scroll position preserved. All scroll mechanics live inside
  // the component; here we just track which channels stay mounted.
  const KEEP_ALIVE_MAX = 6;
  let keptChannels = $state<string[]>([]);
  // ---- servers/namespaces as rail tiles (Phase 6, flavor A) ----
  let activeServer = $state(""); // "" = network top-level channels; else a namespace
  // "#gaming/general" → "gaming"; top-level "#general" → "".
  const nsOf = (name: string) => name.match(/^#([^/]+)\//)?.[1] ?? "";
  // Short channel label under a server tile: "#gaming/general" → "general".
  const chanShort = (name: string) => name.replace(/^#[^/]+\//, "").replace(/^#/, "");
  // ---- DMs + presence (Phase 5) ----
  let homeView = $state(true); // sidebar shows DMs; namespaces are the only servers
  let presence = $state<Record<string, string>>({}); // account → status
  // §10.3 account → display profile (nick + avatar hash). Filled from PROFILE
  // events (broadcast on change) + on-demand PROFILES queries.
  let profiles = $state<Record<string, { display?: string; avatar?: string; about?: string; status?: string }>>({});
  // §10.3 per-namespace display names (server nicknames), keyed "scope|account".
  let nicks = $state<Record<string, string>>({});
  const nickKey = (scope: string, account: string) => `${scope}|${account}`;
  const nicksFetched = new Set<string>();
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
  let myStatus = $state("online");
  // §10.5 the caller's own verification claims, keyed by kind (email/birthday).
  let verifications = $state<Record<string, { subject: string; state: string }>>({});
  // Footer user menu (presence + settings + logout) and the user-settings page tab.
  let userMenu = $state(false);
  let userTab = $state<"account" | "appearance" | "connection" | "verification">("account");
  let dmInput = $state("");
  // ---- social layer: friends (federation-able; keyed by full account@network) ----
  // userref → "friends" | "incoming" | "outgoing"
  let friends = $state<Record<string, string>>({});
  let addFriendInput = $state("");
  // group DMs: group id (`&<ulid>`) → { name?, members (userrefs) }
  let groups = $state<Record<string, { name?: string; members: string[] }>>({});
  // friend calls (1:1): incoming ring + the active call, if any.
  let incomingCall = $state<{ from: string; room: string } | null>(null);
  let activeCall = $state<{ peer: string; room: string; state: string } | null>(null);
  // Group DM calls: gid → members currently in the call, and the gid we're in.
  let groupCallRoster = $state<Record<string, string[]>>({});
  let activeGroupCall = $state<string | null>(null);
  // ---- discover dialog (Phase 6) ----
  let discoverOpen = $state(false);
  let discovered = $state<Record<string, Extract<weft.WeftEvent, { kind: "ns-meta" }>>>({});
  let discoverCursor = $state<string | null>(null);
  // ---- roles / invites / reports (Phase 7) ----
  const RESOLVE_ACTIONS = ["dismissed", "content-removed", "user-actioned", "escalated"];
  let reportTarget = $state<Msg | null>(null); // message being reported (ReportModal)
  let reportsOpen = $state(false);
  let reportQueue = $state<Record<string, Extract<weft.WeftEvent, { kind: "report-filed" }>>>({});
  let profileTarget = $state<string | null>(null); // member profile popout
  let inviteLink = $state<string | null>(null);
  let inviteId = $state<string | null>(null); // for INVITE REVOKE
  let inviteCreateOpen = $state(false); // the invite-creation screen
  let inviteCreateScope = $state(""); // scope the create screen mints at
  // Discord-style invites menu: the live invites at a scope, each revocable.
  type InviteInfo = Extract<weft.WeftEvent, { kind: "invite-info" }>;
  let invitesOpen = $state(false);
  let invitesScope = $state("");
  let invitesList = $state<InviteInfo[]>([]);
  let invitesBuf: InviteInfo[] = [];
  let loadingInvites = false;
  // ---- federation (§11, operator) ----
  let federationOpen = $state(false);
  let netblocks = $state<Record<string, string | null>>({}); // network → reason
  let manifests = $state<Record<string, Extract<weft.WeftEvent, { kind: "manifest" }>>>({});
  function refreshNetblocks() {
    netblocks = {};
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
    delete netblocks[nw];
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
  // ---- pins (§6.4) ----
  let pinsOpen = $state(false);
  let pinsList = $state<Msg[]>([]);
  let loadingPins: string | null = null;
  let pinsBuf: Msg[] = [];
  // ---- message search (§6.4) — results arrive as a BATCH like pins ----
  let searchOpen = $state(false);
  let searchQuery = $state("");
  let searchScope = $state(""); // the channel searched
  let searchResults = $state<Msg[]>([]);
  let searching = $state(false);
  let loadingSearch: string | null = null; // channel whose result batch is inbound
  let searchBuf: Msg[] = [];
  // ---- threads (§9.4) — a side panel showing one thread (root + replies) ----
  let threadRoot = $state<Msg | null>(null);
  let threadMessages = $state<Msg[]>([]);
  let threadComposer = $state("");
  let loadingThread: string | null = null; // root msgid whose thread batch is inbound
  let threadBuf: Msg[] = [];
  // ---- threads list (§9.4) — a channel's threads, arriving as a BATCH ----
  let threadsOpen = $state(false);
  let threadsList = $state<ThreadInfo[]>([]);
  let threadsBuf: ThreadInfo[] = [];
  let loadingThreads = false;
  // Root msgid → display name, kept live from THREAD / THREAD-NAMED events so
  // the inline indicator and the thread panel title show the name everywhere.
  let threadNames = $state<Record<string, string>>({});
  // ---- capability badges (§10.4 CAPS), keyed `account|scope` ----
  let capsFor = $state<Record<string, { owner: boolean; mod: boolean; list: string[] }>>({});
  const capsInflight = new Set<string>();
  function ensureCapsAt(account: string, scope: string) {
    if (!scope || !account) return;
    const key = `${account}|${scope}`;
    if (key in capsFor || capsInflight.has(key)) return;
    capsInflight.add(key);
    weft.caps(account, scope).catch(() => capsInflight.delete(key));
  }
  const ensureCaps = (account: string, channel: string) =>
    channel.startsWith("#") && ensureCapsAt(account, channel);
  const badgeFor = (account: string, channel: string) => capsFor[`${account}|${channel}`];
  const isOperator = $derived(capsFor[`${account}|*`]?.owner ?? false);
  /// The role/authority scope for the active view: the namespace if we're in
  /// one, else global.
  const roleScopeOf = (channel: string) => {
    const ns = nsOf(channel);
    return ns ? `ns:${ns}` : "*";
  };

  // ---- §6.5 named roles (capability-token bundles), keyed by scope ----
  let rolesByScope = $state<Record<string, RoleDefC[]>>({});
  let roleBuf: RoleDefC[] = [];
  // Roles arrive in `r…`-id BATCHes; a queue tracks which scope each answers,
  // so several scopes can be fetched at once (e.g. ns + channel).
  let roleFetchQueue: string[] = [];
  let currentBatchId = "";
  function fetchRoles(scope: string) {
    if (!scope) return;
    roleFetchQueue.push(scope);
    weft.roles(scope).catch(() => roleFetchQueue.pop());
  }

  // ---- §6.2 NS INFO MEMBERS: the moderator roster (members + join + roles) ----
  // Arrives as an `ni…`-id BATCH of `ns-member-info` events. `loadingNsMembers`
  // records which namespace is in flight so an empty roster still flushes.
  let nsMembersByNs = $state<Record<string, MemberInfoC[]>>({});
  let nsMemberBuf: MemberInfoC[] = [];
  let loadingNsMembers: string | null = null;
  let nsMembersLoading = $state(false);
  function fetchNsMembers(ns: string) {
    if (!ns) return;
    loadingNsMembers = ns;
    nsMembersLoading = true;
    weft.nsInfoMembers(ns).catch((e) => {
      nsMembersLoading = false;
      loadingNsMembers = null;
      toast(String(e), "error");
    });
  }
  function createRoleAt(
    scope: string,
    name: string,
    color: string,
    caps: string,
    hoist = false,
    pingable = false,
    position = 0,
  ) {
    roleFetchQueue.push(scope);
    return weft.roleCreate(scope, color, caps, hoist, pingable, position, name);
  }
  function deleteRoleAt(scope: string, name: string) {
    roleFetchQueue.push(scope);
    return weft.roleDelete(scope, name);
  }
  /// Is this account the owner/operator at the scope (implicit all-caps)?
  const isOwnerAt = (account: string, scope: string) =>
    capsFor[`${account}|${scope}`]?.owner ?? false;
  // Explicit role membership (§6.5) keyed `account|scope`, from ROLE-MEMBER —
  // a role is worn because it was assigned, never inferred from caps.
  let memberRoles = $state<Record<string, string[]>>({});
  // §11.11 federated authors whose roles we've already fetched (`who|scope`).
  const fedRolesFetched = new Set<string>();
  function fetchMemberRoles(account: string, scope: string) {
    weft.rolesOfAccount(scope, account).catch(() => {});
  }
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
  /// The role definitions an account is assigned at a scope.
  function rolesOf(account: string, scope: string): RoleDefC[] {
    const names = new Set(memberRoles[`${account}|${scope}`] ?? []);
    return (rolesByScope[scope] ?? []).filter((r) => names.has(r.name));
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
    return (friends[qualify(peerOf(handle))] as "friends" | "incoming" | "outgoing") ?? "none";
  }
  function friendAction(handle: string, action: "add" | "accept" | "remove") {
    const ref = qualify(peerOf(handle));
    if (action === "add") weft.friendAdd(ref).catch((e) => toast(String(e), "error"));
    else if (action === "accept") acceptFriend(ref);
    else removeFriend(ref);
  }
  function assignRoleTo(acct: string, role: RoleDefC) {
    const scope = roleScopeOf(active);
    // Success is confirmed by the resulting ROLE-MEMBER event (see
    // `expectSuccess`); a missing-cap failure never confirms and its ERR toasts.
    expectSuccess(`roles:${acct}|${scope}`, `Roles updated for ${acct}`);
    weft
      .roleAssign(scope, acct, role.name)
      .then(() => fetchMemberRoles(acct, scope)) // ROLES-OF queues after ASSIGN → fresh list
      .catch((e) => toast(String(e), "error"));
  }
  function unassignRoleFrom(acct: string, role: RoleDefC) {
    const scope = roleScopeOf(active);
    expectSuccess(`roles:${acct}|${scope}`, `Roles updated for ${acct}`);
    weft
      .roleUnassign(scope, acct, role.name)
      .then(() => fetchMemberRoles(acct, scope))
      .catch((e) => toast(String(e), "error"));
  }
  // ---- namespace admin panel (§6.2 / §2.4 / §6.6) ----
  let nsSettingsOpen = $state(false);
  // §10.3 per-server profile editor (your own nickname on this server).
  let serverProfileOpen = $state(false);
  function openServerProfile() {
    if (activeServer) serverProfileOpen = true;
    serverMenu = false;
  }
  let nsTab = $state<
    | "overview"
    | "roles"
    | "members"
    | "emoji"
    | "invites"
    | "bans"
    | "federation"
    | "recovery"
    | "danger"
  >("overview");
  // §6.7 moderation deny-list (mutes + bans) per scope, for the Bans tab.
  let modDeny = $state<
    Record<string, { account: string; kind: string; by?: string | null; reason?: string | null }[]>
  >({});
  const banScope = () => (activeServer ? `ns:${activeServer}` : "*");
  const denyList = () => modDeny[banScope()] ?? [];
  function refreshBans() {
    modDeny[banScope()] = []; // full refresh; the batch response repopulates
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
    if (!newRoleName.trim() || !newRoleCaps.length) return;
    // Append at the bottom of the ordered list.
    const position = rolesByScope[nsRoleScope()]?.length ?? 0;
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
    (rolesByScope[nsRoleScope()] ?? []).find((r) => r.name === EVERYONE_ROLE)?.caps ?? [];
  // Set the @everyone baseline. Non-empty → upsert the reserved role; empty →
  // delete it (the server rejects an empty cap list, and "no role" = no
  // baseline). It's never assigned or hoisted.
  function setEveryoneCaps(caps: string[]) {
    const scope = nsRoleScope();
    const p = caps.length
      ? createRoleAt(scope, EVERYONE_ROLE, "#99aab5", caps.join(","), false, false, 0)
      : deleteRoleAt(scope, EVERYONE_ROLE);
    p.catch((e) => toast(String(e), "error"));
  }
  // Move a role up/down in the ordered list, then persist the new order (§6.5).
  function moveRole(name: string, dir: -1 | 1) {
    const scope = nsRoleScope();
    const list = [...(rolesByScope[scope] ?? [])];
    const i = list.findIndex((r) => r.name === name);
    const j = i + dir;
    if (i < 0 || j < 0 || j >= list.length) return;
    [list[i], list[j]] = [list[j], list[i]];
    roleFetchQueue.push(scope);
    weft.rolesReorder(scope, list.map((r) => r.name)).catch((e) => toast(String(e), "error"));
  }
  // Persist an arbitrary order (drag-and-drop) — positions follow the list.
  function reorderRoles(names: string[]) {
    const scope = nsRoleScope();
    roleFetchQueue.push(scope);
    weft.rolesReorder(scope, names).catch((e) => toast(String(e), "error"));
  }
  // Apply a role edit. A changed name goes through ROLE RENAME so the role keeps
  // its members and issued caps; the rest rides the ordinary upsert (§6.5).
  function saveRole(
    role: RoleDefC,
    patch: { name: string; color: string; caps: string[]; hoist: boolean; pingable: boolean },
  ) {
    const scope = nsRoleScope();
    const name = patch.name.trim() || role.name;
    if (!patch.caps.length) {
      toast("A role needs at least one permission", "error");
      return;
    }
    const upsert = () =>
      createRoleAt(scope, name, patch.color, patch.caps.join(","), patch.hoist, patch.pingable, role.position);

    if (name !== role.name) {
      roleFetchQueue.push(scope);
      weft
        .roleRename(scope, role.name, name)
        .then(upsert)
        .catch((e) => toast(String(e), "error"));
    } else {
      upsert().catch((e) => toast(String(e), "error"));
    }
  }
  function deleteRole(name: string) {
    deleteRoleAt(nsRoleScope(), name).catch((e) => toast(String(e), "error"));
  }
  function assignRole(name: string) {
    const who = nsDelegSubject.trim();
    if (!who) {
      toast("Enter an account first", "error");
      return;
    }
    // Confirmed by the ROLE-MEMBER event; a cap failure never confirms.
    expectSuccess(`roles:${who}|${nsRoleScope()}`, `Roles updated for ${who}`);
    weft.roleAssign(nsRoleScope(), who, name).catch((e) => toast(String(e), "error"));
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
  let activeNsMeta = $derived(activeServer ? discovered[activeServer] : undefined);
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

  const initials = (s: string) => s.replace(/[^a-z0-9]/gi, "").slice(0, 2).toUpperCase() || "··";
  const hhmm = (d: Date) =>
    `${`${d.getHours()}`.padStart(2, "0")}:${`${d.getMinutes()}`.padStart(2, "0")}`;
  const clock = () => hhmm(new Date());

  // A msgid is `network/<ULID>`; the ULID's first 10 Crockford-base32 chars
  // encode its 48-bit ms timestamp. Gives correct times for backfilled history
  // (Phase 1), not just live arrival.
  const CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
  // Decode a msgid's ULID timestamp to epoch ms, or null if it isn't a ULID.
  function msgEpoch(msgid: string | undefined): number | null {
    const ulid = msgid?.split("/").pop() ?? "";
    if (ulid.length < 10) return null;
    let ms = 0;
    for (let i = 0; i < 10; i++) {
      const v = CROCKFORD.indexOf(ulid[i].toUpperCase());
      if (v < 0) return null;
      ms = ms * 32 + v;
    }
    return ms;
  }
  function msgTime(msgid: string): string {
    const ms = msgEpoch(msgid);
    return ms === null ? clock() : hhmm(new Date(ms));
  }
  // ---- day separators (Tier 1) ----
  const startOfDay = (d: Date) => new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
  const dayKey = (ts: number) => startOfDay(new Date(ts));
  function dayLabel(ts: number): string {
    const diff = Math.round((startOfDay(new Date()) - dayKey(ts)) / 86_400_000);
    if (diff === 0) return "Today";
    if (diff === 1) return "Yesterday";
    return new Date(ts).toLocaleDateString(undefined, {
      weekday: "long",
      month: "long",
      day: "numeric",
      year: "numeric",
    });
  }
  const retentionOf = (policy: string) => {
    if (policy.startsWith("retained")) return "retained";
    if (["ephemeral", "permanent", "e2ee"].includes(policy)) return policy;
    return "retained";
  };

  function ensureChannel(name: string): Channel {
    if (!channels[name]) {
      channels[name] = { name, retention: "retained", messages: [], members: [] };
      // Seed layout from the cache so groups/order render instantly on reload.
      const ns = nsOf(name);
      const cached = ns ? layoutCache[ns]?.chans[name] : undefined;
      if (cached) {
        channels[name].category = cached.category;
        channels[name].position = cached.position;
      }
    }
    return channels[name];
  }

  // ---- history / scrollback (Phase 1) ----
  const HISTORY_LIMIT = 50;
  let loadingHistory = $state<string | null>(null); // channel being backfilled
  // History pages buffered per *target channel*, keyed by the messages' own
  // `target`. This is what makes history robust: a page flushes to the channel it
  // names, so a concurrent MEMBERS/roles/… batch can never steal or clobber it,
  // whatever its batch id or arrival order.
  let histByTarget: Record<string, Msg[]> = {};

  const oldestMsgid = (ch?: Channel) => ch?.messages.find((m) => m.msgid)?.msgid;

  // Fetch a channel's history page. Single-flight (`loadingHistory` guard);
  // MessageList calls this on first open (initial) and on scroll-to-top (paging).
  function loadHistory(target: string, initial: boolean) {
    // Channels (`#`), DMs (`@`), and group DMs (`&`) all backfill; one at a time.
    if (
      loadingHistory ||
      !(target.startsWith("#") || target.startsWith("@") || target.startsWith("&"))
    )
      return;
    loadingHistory = target;
    histByTarget[target] = [];
    const before = initial ? undefined : oldestMsgid(channels[target]);
    weft.history(target, before).catch(() => {
      loadingHistory = null; // don't wedge paging if the fetch never lands
    });
  }

  let activeChannel = $derived(active ? channels[active] : undefined);
  // A channel record by name — each kept-alive MessageList reads its own.
  const channelRecord = (name: string): Channel | undefined => channels[name];
  let activeIsDm = $derived(active.startsWith("@"));
  let activeIsGroup = $derived(active.startsWith("&"));
  // Namespaces we hold channels in — each becomes a rail tile (flavor A).
  let serverNamespaces = $derived(
    [
      ...new Set(
        Object.values(channels)
          .filter((c) => c.name.startsWith("#"))
          .map((c) => nsOf(c.name))
          .filter(Boolean),
      ),
    ].sort(),
  );
  // Server-tile unread/mention rollups (so unread in other servers is visible).
  const serverUnread = (ns: string) =>
    Object.keys(unreadMap).some((n) => unreadMap[n] && nsOf(n) === ns && n !== active);
  const serverMention = (ns: string) =>
    Object.keys(mentionMap).some((n) => mentionMap[n] && nsOf(n) === ns && n !== active);
  // Total mentions across a server's channels, for the rail's numeric badge.
  const serverMentionCount = (ns: string) =>
    Object.keys(mentionCount).reduce(
      (sum, n) => (nsOf(n) === ns && n !== active ? sum + (mentionCount[n] ?? 0) : sum),
      0,
    );
  // Discord-style grouping for the *active server*: uncategorized channels sit
  // bare at the top (category "", no header), then each CHANNEL-LAYOUT category
  // (position-ordered) in its persisted order.
  let channelGroups = $derived.by(() => {
    const bare: Channel[] = [];
    const groups = new Map<string, Channel[]>();
    // Empty categories the admin created (client-side) show up too.
    for (const cat of discovered[activeServer]?.categories ?? layoutCache[activeServer]?.cats ?? [])
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

  function selectServer(ns: string) {
    homeView = false;
    activeServer = ns;
    // Land on a channel in this server if the current one isn't in it.
    if (!active.startsWith("#") || nsOf(active) !== ns) {
      const first = Object.values(channels)
        .filter((c) => c.name.startsWith("#") && nsOf(c.name) === ns)
        .sort((a, b) => (a.position ?? 0) - (b.position ?? 0) || a.name.localeCompare(b.name))[0];
      active = first?.name ?? "";
    }
  }
  // Right-click a rail tile: select the server and open its header menu (the
  // same Create Invite / Notification / Server Settings menu as clicking the name).
  function openServerMenu(ns: string) {
    selectServer(ns);
    serverMenu = true;
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

  // ---- §9.4 custom emoji, keyed namespace → (name → media ref) ----
  let customEmoji = $state<Record<string, Record<string, string>>>({});
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
    Object.entries(customEmoji[activeServer] ?? {}).map(([name, media]) => ({ name, media })),
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
    const media = customEmoji[activeServer]?.[name];
    return media ? weft.mediaUrl(media) : null;
  };

  // DM conversations (keyed `@peer`), plus any peer we've opened a blank DM with.
  let dmList = $derived(
    Object.values(channels).filter((c) => c.name.startsWith("@") || c.name.startsWith("&")),
  );

  // ---- DM + presence helpers ----
  const peerOf = (key: string) => key.replace(/^@/, "");
  const dotClass = (acct: string) => `dot ${presence[acct] ?? "offline"}`;

  // ---- §10.3 profile helpers ----
  /** A fetchable avatar URL for an account, or null → render initials. */
  const avatarUrl = (acct: string): string | null => {
    const a = profiles[peerOf(acct)]?.avatar;
    return a ? weft.avatarUrl(a) : null;
  };
  /** An account's display name — the active server's nickname if set, else the
   *  global display name, else the bare account part (§10.3: the canonical
   *  handle is always shown separately). */
  const displayName = (acct: string): string => {
    const key = peerOf(acct);
    const nick = activeServer ? nicks[nickKey(`ns:${activeServer}`, key)] : undefined;
    return nick || profiles[key]?.display || key.split("@")[0];
  };
  /** An account's server nickname at the active server, or "" (for editors). */
  const nickOf = (acct: string): string =>
    (activeServer ? nicks[nickKey(`ns:${activeServer}`, peerOf(acct))] : "") ?? "";
  /** An account's free-text bio (§10.3), or "" if unset. */
  const bioOf = (acct: string): string => profiles[peerOf(acct)]?.about ?? "";
  /** An account's custom status (§10.3), or "" if unset. */
  const statusOf = (acct: string): string => profiles[peerOf(acct)]?.status ?? "";
  /** Set (or clear, with "") my own custom status. */
  function setCustomStatus(text: string) {
    weft.profileSet({ status: text }).catch((e) => toast(String(e), "error"));
  }
  /** Fetch a profile we don't have yet (deduped; own + co-members). */
  function queryProfile(acct: string) {
    const a = peerOf(acct);
    if (a && profiles[a] === undefined) {
      profiles[a] = {}; // mark requested so we don't re-query
      weft.profilesQuery([a]).catch(() => {});
    }
  }

  function openDm(peer: string) {
    const key = "@" + peer.replace(/^@/, "");
    ensureChannel(key);
    persistDms(); // keep the DM in the list across reconnects
    homeView = true;
    active = key;
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
    if (active === key) active = Object.keys(channels)[0] ?? "";
  }
  // The set of open 1:1 DMs is view state the server doesn't yet track (a
  // server-owned DM list is §18 territory), so we persist it per account so a
  // conversation — and its history on click — survives a reconnect / relaunch.
  const dmStoreKey = () => `weft:dms:${account}@${network}`;
  // v0.12 SYNC cursor, per account+device (localStorage). Stored on every
  // `sync-end`, replayed on reconnect so `SYNC since=` catches up missed
  // messages + offline edits/reactions in one round trip.
  const syncCursorKey = () => `weft:sync:${account}@${network}`;
  function loadSyncCursor(): string | undefined {
    try {
      return localStorage.getItem(syncCursorKey()) ?? undefined;
    } catch {
      return undefined;
    }
  }
  function persistDms() {
    try {
      const keys = Object.keys(channels).filter((k) => k.startsWith("@"));
      localStorage.setItem(dmStoreKey(), JSON.stringify(keys));
    } catch {
      /* storage unavailable */
    }
  }
  function restoreDms() {
    try {
      const keys: string[] = JSON.parse(localStorage.getItem(dmStoreKey()) ?? "[]");
      for (const k of keys) if (k.startsWith("@")) ensureChannel(k);
    } catch {
      /* storage unavailable */
    }
  }

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
  function friendLabel(user: string): string {
    const [acct, net] = user.split("@");
    return net === network ? displayName(acct) : user;
  }
  // A friend's local account handle (for DM/profile/presence), if local.
  function friendLocalAccount(user: string): string | null {
    const [acct, net] = user.split("@");
    return net === network ? acct : null;
  }
  const friendList = $derived(
    Object.entries(friends)
      .filter(([, s]) => s === "friends")
      .map(([u]) => u)
      .sort((a, b) => friendLabel(a).localeCompare(friendLabel(b))),
  );
  const incomingRequests = $derived(
    Object.entries(friends).filter(([, s]) => s === "incoming").map(([u]) => u).sort(),
  );
  const outgoingRequests = $derived(
    Object.entries(friends).filter(([, s]) => s === "outgoing").map(([u]) => u).sort(),
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
    homeView = true;
    active = "";
  }
  // Pressing the DM/home tile lands on the most recently active conversation
  // (DM or group) — or the friends menu if there are none.
  function goHome() {
    homeView = true;
    const convos = dmList;
    if (!convos.length) {
      active = "";
      return;
    }
    active = convos.reduce((a, b) =>
      (b.messages.at(-1)?.ts ?? 0) >= (a.messages.at(-1)?.ts ?? 0) ? b : a,
    ).name;
  }

  // ---- group DMs ----
  let newGroupInput = $state("");
  // A group's display label: its name, else the member handles (minus self).
  function groupLabel(id: string): string {
    const g = groups[id];
    if (!g) return "Group";
    if (g.name) return g.name;
    const me = `${account}@${network}`;
    const others = g.members.filter((m) => m !== me).map((m) => friendLabel(m));
    return others.length ? others.join(", ") : "Group";
  }
  const groupList = $derived(Object.keys(groups));
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
    homeView = true;
    active = id;
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
    if (activeGroupCall === id) activeGroupCall = null;
  }

  // ---- friend calls (1:1) ----
  function callUser(user: string) {
    if (activeCall) return; // already in a call
    weft.call(user).catch((e) => toast(String(e), "error"));
  }
  function acceptCall() {
    if (!incomingCall) return;
    const { from, room } = incomingCall;
    weft.callAccept(from).catch((e) => toast(String(e), "error"));
    activeCall = { peer: from, room, state: "active" };
    incomingCall = null;
  }
  function declineCall() {
    if (!incomingCall) return;
    weft.callDecline(incomingCall.from).catch(() => {});
    incomingCall = null;
  }
  function endCall() {
    if (!activeCall) return;
    weft.callEnd(activeCall.peer).catch(() => {});
    disconnectCallMedia();
    activeCall = null;
  }
  function setStatus(s: string) {
    myStatus = s;
    userMenu = false;
    weft.presence(s).catch(() => {});
  }

  // ---- event handling ----
  function handle(e: weft.WeftEvent) {
    switch (e.kind) {
      case "connected":
        network = e.network;
        account = e.account;
        status = "online";
        authError = "";
        reconnecting = false;
        reconnectAttempts = 0;
        ensureCapsAt(account, "*"); // learn operator status (federation gating)
        initVoice(account); // §16 wire the voice controller to the event stream
        queryProfile(account); // §10.3 load our own profile
        weft.verifyList().catch(() => {}); // §10.5 load our verification claims
        friends = {};
        groups = {};
        weft.listFriends().catch(() => {}); // social layer: load friends + requests
        weft.listGroups().catch(() => {}); // and group DMs
        restoreDms(); // re-open the 1:1 DMs from last session (history loads on click)
        // Clear any half-finished history load from before a reconnect — its
        // BATCH will never arrive, so a stale guard would block every new load.
        loadingHistory = null;
        histByTarget = {};
        // Remember creds so the next launch logs straight back in. NOTE: this
        // includes the password in localStorage — a dev convenience; the
        // hardening is OS-keychain storage in the backend.
        try {
          localStorage.setItem(
            SAVED_KEY,
            JSON.stringify({ host, account: formAccount.trim(), password: formPassword }),
          );
        } catch {
          /* storage unavailable */
        }
        // A returning session is auto-rejoined to its channels by the server
        // (persistent membership, §6.3). A brand-new account joins nothing —
        // it's not forced into the seeded server; the empty-home screen guides
        // it to Discover / create / join instead.
        //
        // §6.9 SYNC (v0.12): pull the skeleton + a cursor on fresh login, or the
        // delta of everything missed since our stored cursor on reconnect. The
        // materialized rows flow through the ordinary message/edit/reaction
        // handlers (upsert by msgid); `sync-end` gives the next cursor.
        weft.sync(loadSyncCursor()).catch(() => {});
        break;
      case "media-token":
        weft.setMediaBearer(e.token); // §13 fetch bearer for /media URLs
        break;
      case "auth-failed":
        reconnecting = false;
        lastCreds = null;
        status = "connect";
        authError = e.reason;
        authFailed = true;
        break;
      case "closed":
        if (manualLogout) {
          manualLogout = false;
          break;
        }
        // AUTH-FAILED already closed the stream (§3.6) and set a specific
        // reason — don't overwrite it with the generic close message.
        if (authFailed) {
          authFailed = false;
          break;
        }
        // Unexpected drop while online → keep the UI and auto-reconnect.
        if (lastCreds && (status === "online" || reconnecting)) {
          attemptReconnect();
        } else {
          status = "connect";
          authError = e.reason;
        }
        break;
      case "policy":
        ensureChannel(e.channel).retention = retentionOf(e.policy);
        confirmSuccess(`policy:${e.channel}`);
        break;
      case "member": {
        const ch = ensureChannel(e.channel);
        // Roster only — the Discord-style "joined"/"left" line is a persistent
        // system MESSAGE the server emits alongside this event (see "message").
        if (e.action === "join") {
          if (!ch.members.some((m) => m.name === e.user)) {
            ch.members.push({ name: e.user, origin: e.network === network ? "local" : "federated" });
          }
          ensureCaps(e.user, e.channel); // for the roster badge
          queryProfile(e.user); // §10.3 learn their display name + avatar

          if (e.user === account) {
            // Jump to a channel we just joined only when we're actually browsing
            // a server (not on the Friends/DMs home). This keeps startup — where
            // the server auto-rejoins our channels — on the home view instead of
            // yanking us into whichever channel's join event lands first.
            if (!active && !homeView) active = e.channel;
            // Presence is broadcast to shared channels only, so re-announce
            // ours whenever we join one (lets its members see our status).
            weft.presence(myStatus).catch(() => {});
          } else {
            // Mark a just-joined member online (they announce, but a peer that
            // was already here won't have — best effort with this model).
            presence[e.user] ??= "online";
          }
        } else {
          ch.members = ch.members.filter((m) => m.name !== e.user);
          if (e.user === account) {
            delete channels[e.channel];
            if (active === e.channel) active = Object.keys(channels)[0] ?? "";
          }
        }
        break;
      }
      case "message": {
        // Channels key by name; DMs (`@to`) key by the *peer* — the other
        // party — so both sides land in one conversation.
        let key: string;
        if (e.target.startsWith("#")) key = e.target;
        else if (e.target.startsWith("@")) key = "@" + (e.own ? e.target.slice(1) : e.sender);
        else if (e.target.startsWith("&")) key = e.target; // group DM: keyed by id
        else break;
        // Server-generated system messages (join/part, …) — a persistent line
        // that rides the normal message + history path, rendered Discord-style.
        const who = e.network === network ? e.sender : `${e.sender}@${e.network}`;
        if (!e.system) queryProfile(who); // §10.3 the sender's avatar + display name
        const systemBody = e.system
          ? e.system === "join"
            ? `${who} joined`
            : e.system === "part"
              ? `${who} left`
              : `${who} ${e.system}`
          : null;
        const msg = mkMsg({
          author: e.sender,
          body: systemBody ?? e.body,
          system: e.system ? true : undefined,
          time: msgTime(e.msgid),
          ts: msgEpoch(e.msgid) ?? Date.now(),
          own: e.own && !e.system,
          msgid: e.msgid,
          edited: e.edited,
          md: e.md && !e.system,
          replyTo: e.reply_to ?? undefined,
          thread: e.thread ?? undefined,
          bridged: e.network !== network,
          net: !e.system && e.network !== network ? e.network : undefined,
          attachments: e.attachments?.length ? e.attachments : undefined,
        });
        // Batch messages buffer until BATCH END. A SEARCH batch routes to the
        // search buffer, a PINS batch (loadingPins) to the pins buffer, else a
        // HISTORY batch to the history buffer.
        if (e.history) {
          if (loadingThread) threadBuf.push(msg);
          else if (loadingSearch) searchBuf.push(msg);
          else if (loadingPins) pinsBuf.push(msg);
          else (histByTarget[e.target] ??= []).push(msg); // route to the page's own channel
          break;
        }
        // A DM we haven't got open yet (someone messaged us) → persist it so the
        // conversation survives a reconnect.
        const newDm = key.startsWith("@") && !channels[key];
        const ch = ensureChannel(key);
        if (newDm) persistDms();
        // §3.5/§11.13 optimistic reconcile: our own echoed message carrying our
        // label replaces the pending placeholder we showed on send, rather than
        // adding a duplicate. Works identically for a local send and for one a
        // home-authoritative channel minted elsewhere (our server re-attaches the
        // label to the mirrored copy).
        if (e.own && e.label) {
          const idx = ch.messages.findIndex((m) => m.pending && m.label === e.label);
          if (idx !== -1) {
            ch.messages.splice(idx, 1, msg);
            const ti = threadMessages.findIndex((m) => m.pending && m.label === e.label);
            if (ti !== -1) threadMessages = threadMessages.map((m, i) => (i === ti ? msg : m));
            break;
          }
        }
        // Upsert by msgid (v0.12 SYNC apply rule): a re-delivered message —
        // history backfill, or a reconnect delta carrying an offline edit —
        // replaces the existing copy in place with the final body + edited
        // state, preserving accumulated reactions (which arrive as own events).
        if (e.msgid) {
          const idx = ch.messages.findIndex((m) => m.msgid === e.msgid);
          if (idx !== -1) {
            msg.reactions = ch.messages[idx].reactions;
            ch.messages.splice(idx, 1, msg);
            break;
          }
        }
        ch.messages.push(msg);
        // If this is a live reply in the open thread, show it in the panel too.
        if (
          threadRoot &&
          key === active &&
          msg.thread === threadRoot.msgid &&
          !threadMessages.some((m) => m.msgid === msg.msgid)
        ) {
          threadMessages = [...threadMessages, msg];
        }
        if (key.startsWith("#")) {
          if (e.network !== network) {
            // §11.11 recognition: fetch a federated author's roles here (once)
            // so the timeline can show their role color, keyed account@network.
            const who = `${e.sender}@${e.network}`;
            const rscope = roleScopeOf(key);
            const fk = `${who}|${rscope}`;
            if (!fedRolesFetched.has(fk)) {
              fedRolesFetched.add(fk);
              fetchMemberRoles(who, rscope);
            }
          } else {
            ensureCaps(e.sender, key); // for the author badge
          }
        }
        const pinged = !e.own && mentionsMe(e.body, nsOf(key));
        const level = notifLevel(key);
        // A muted scope shows no unread indicator; others tally unread/mentions.
        if (!e.own && key !== active && level !== "nothing") {
          unreadMap[key] = true;
          unreadCount[key] = (unreadCount[key] ?? 0) + 1;
          if (pinged) {
            mentionMap[key] = true;
            mentionCount[key] = (mentionCount[key] ?? 0) + 1;
          }
        }
        // Desktop notification while unfocused, gated by the scope's level:
        // "all" → every message, "mentions" → DMs/@mentions only, "nothing" → none.
        if (!e.own && !document.hasFocus()) {
          const dm = e.target.startsWith("@");
          const notify = level === "all" || (level === "mentions" && (dm || pinged));
          // Qualify a foreign sender so the notification isn't ambiguous.
          const who = e.network !== network ? `${e.sender}@${e.network}` : e.sender;
          if (notify)
            weft.notify(
              dm ? `DM from ${who}` : `${who} in ${chanShort(key)}`,
              e.body.slice(0, 140),
            );
        }
        break;
      }
      case "profile": {
        // §10.3 a display profile (nick + avatar). Key local users by their bare
        // handle, federated users by `account@network` (so same-name users on
        // different networks don't collide).
        const key = e.network === network ? e.account : `${e.account}@${e.network}`;
        profiles[key] = {
          display: e.display ?? undefined,
          avatar: e.avatar ?? undefined,
          about: e.about ?? undefined,
          status: e.status ?? undefined,
        };
        break;
      }
      case "nick": {
        // §10.3 a per-namespace server nickname (empty = cleared).
        const acct = e.network === network ? e.account : `${e.account}@${e.network}`;
        const key = nickKey(e.scope, acct);
        if (e.nick) nicks[key] = e.nick;
        else delete nicks[key];
        nicks = { ...nicks }; // re-trigger derivations (delete isn't tracked)
        break;
      }
      case "verified":
        // §10.5 one of our own verification claims (email/birthday).
        verifications[e.claim_kind] = { subject: e.subject, state: e.state };
        break;
      case "presence":
        presence[e.user] = e.status;
        break;
      case "marked": {
        // Read-marker sync from another device (§9.7).
        const ch = channels[e.channel];
        if (ch) ch.lastRead = e.msgid;
        markRead(e.channel);
        break;
      }
      case "unread-counts": {
        // Server-authoritative unread tally (§6.3) — the login snapshot and
        // cross-device MARK pushes override the client's live tally, so counts
        // survive reload/reconnect and stay in sync across devices. The channel
        // being viewed is read (auto-mark handles it); muted scopes stay silent.
        if (e.channel !== active && !isMuted(e.channel)) {
          unreadCount[e.channel] = e.unread;
          unreadMap[e.channel] = e.unread > 0;
          mentionCount[e.channel] = e.mentions;
          mentionMap[e.channel] = e.mentions > 0;
        }
        break;
      }
      case "sync-end": {
        // §6.9 store the new cursor for this device's next reconnect delta.
        try {
          localStorage.setItem(syncCursorKey(), e.cursor);
        } catch {
          /* storage unavailable */
        }
        break;
      }
      case "ns-member": {
        // §7.4 namespace-level join/part. Rosters + the sidebar are driven by
        // the per-channel MEMBER/LAYOUT events; this is the ns-level marker
        // (the acting client's ack). No dedicated UI state to update yet.
        break;
      }
      case "chan-sync":
        // §7.9 per-channel SYNC header — previews are withheld in v1, so there's
        // nothing to apply; the `reset` flag lands with the body-stream work.
        break;
      case "emoji": {
        // §9.4 a namespace custom emoji (from EMOJI LIST or a live add).
        (customEmoji[e.namespace] ??= {})[e.name] = e.media;
        customEmoji = { ...customEmoji };
        break;
      }
      case "emoji-removed": {
        if (customEmoji[e.namespace]) {
          delete customEmoji[e.namespace][e.name];
          customEmoji = { ...customEmoji };
        }
        break;
      }
      case "chanmeta": {
        const c = ensureChannel(e.channel);
        if (e.key === "topic") c.topic = e.value;
        else if (e.key === "posting") c.restricted = e.value === "restricted";
        else if (e.key === "category") c.category = e.value || undefined;
        else if (e.key === "position") c.position = parseInt(e.value, 10) || 0;
        if (e.key === "category" || e.key === "position") cacheChanLayout(e.channel, c.category, c.position ?? 0);
        break;
      }
      case "pinned": {
        const ch = ensureChannel(e.channel);
        ch.pinnedIds = [...(ch.pinnedIds ?? []).filter((id) => id !== e.msgid), e.msgid];
        if (pinsOpen && active === e.channel) weft.pins(e.channel).catch(() => {}); // refresh panel
        break;
      }
      case "unpinned": {
        const ch = channels[e.channel];
        if (ch) ch.pinnedIds = (ch.pinnedIds ?? []).filter((id) => id !== e.msgid);
        if (pinsOpen && active === e.channel) pinsList = pinsList.filter((m) => m.msgid !== e.msgid);
        break;
      }
      case "thread": {
        if (e.name) threadNames[e.root] = e.name;
        else delete threadNames[e.root];
        if (loadingThreads)
          threadsBuf.push({ root: e.root, name: e.name ?? undefined, replies: e.replies, last: e.last ?? undefined });
        break;
      }
      case "thread-named": {
        if (e.name) threadNames[e.root] = e.name;
        else delete threadNames[e.root];
        // Reflect a live rename in an open threads list.
        const i = threadsList.findIndex((t) => t.root === e.root);
        if (i >= 0) threadsList[i] = { ...threadsList[i], name: e.name ?? undefined };
        break;
      }
      case "friend":
        friends[e.user] = e.state;
        // A fresh incoming request is worth a nudge.
        if (e.state === "incoming") toast(`Friend request from ${e.user}`, "info");
        break;
      case "friend-removed":
        delete friends[e.user];
        break;
      case "group": {
        groups[e.id] = { name: e.name ?? undefined, members: e.members };
        ensureChannel(e.id); // a conversation entry so it lists + holds messages
        break;
      }
      case "group-member": {
        const g = groups[e.group];
        if (!g) break;
        const me = `${account}@${network}`;
        if (e.action === "join") {
          if (!g.members.includes(e.user)) g.members = [...g.members, e.user];
        } else {
          g.members = g.members.filter((m) => m !== e.user);
          // If *we* left, drop the conversation.
          if (e.user === me) {
            delete groups[e.group];
            delete channels[e.group];
            if (active === e.group) active = "";
          }
        }
        break;
      }
      case "call-ring":
        incomingCall = { from: e.from, room: e.room };
        break;
      case "call-state":
        if (e.state === "ringing") {
          activeCall = { peer: e.user, room: "", state: "ringing" };
        } else if (e.state === "active") {
          incomingCall = null;
          activeCall = { peer: e.user, room: activeCall?.room ?? "", state: "active" };
          // Audio (LiveKit) connects on the CALL-MEDIA credential that follows.
        } else {
          if (e.state === "busy") toast(`${friendLabel(e.user)} is busy`, "info");
          else if (e.state === "declined") toast(`${friendLabel(e.user)} declined the call`, "info");
          if (incomingCall?.from === e.user) incomingCall = null;
          if (activeCall?.peer === e.user) {
            activeCall = null;
            disconnectCallMedia();
          }
        }
        break;
      case "call-media":
        // The server authorized the call and minted our media credential — join
        // the LiveKit room so audio flows. Works for both a 1:1 call (activeCall)
        // and a group call (activeGroupCall) — the credential is the same shape.
        void connectCallMedia(e.endpoint, e.token);
        break;
      case "group-call-state": {
        const roster = groupCallRoster[e.group] ?? [];
        const me = `${account}@${network}`;
        if (e.state === "active") {
          if (!roster.includes(e.user)) groupCallRoster[e.group] = [...roster, e.user];
          if (e.user === me) activeGroupCall = e.group;
        } else {
          const next = roster.filter((u) => u !== e.user);
          if (next.length) groupCallRoster[e.group] = next;
          else delete groupCallRoster[e.group];
          if (e.user === me && activeGroupCall === e.group) {
            activeGroupCall = null;
            disconnectCallMedia();
          }
        }
        break;
      }
      case "caps": {
        const set = e.caps ? e.caps.split(",") : [];
        capsFor[`${e.account}|${e.scope}`] = {
          owner: set.includes("ns-admin") || set.includes("netblock"),
          mod: set.includes("mute") || set.includes("ban") || set.includes("kick"),
          list: set,
        };
        capsInflight.delete(`${e.account}|${e.scope}`);
        confirmSuccess(`caps:${e.account}|${e.scope}`);
        break;
      }
      case "role":
        roleBuf.push({
          name: e.name,
          color: e.color,
          caps: e.caps ? e.caps.split(",") : [],
          hoist: e.hoist,
          pingable: e.pingable,
          position: e.position,
        });
        break;
      case "role-member":
        memberRoles[`${e.account}|${e.scope}`] = e.roles ? e.roles.split(",") : [];
        confirmSuccess(`roles:${e.account}|${e.scope}`);
        break;
      case "ns-member-info":
        nsMemberBuf.push({
          account: e.user,
          network: e.network,
          joinedMs: e.joined_ms,
          roles: e.roles ?? [],
        });
        break;
      case "channel-layout": {
        const ch = ensureChannel(e.channel);
        ch.category = e.category ?? undefined;
        ch.position = e.position;
        ch.voice = e.channel_kind === "voice"; // §16 render as a voice channel
        cacheChanLayout(e.channel, ch.category, e.position);
        break;
      }
      case "channel-renamed": {
        // Re-key local state to the new identity (idempotent — this arrives as
        // a broadcast plus a labeled copy to the initiator).
        const cur = channels[e.old];
        if (cur) {
          cur.name = e.new;
          channels[e.new] = cur;
          delete channels[e.old];
          for (const map of [unreadMap, mentionMap, unreadCount, mentionCount] as Record<
            string,
            boolean | number
          >[]) {
            if (map[e.old] !== undefined) {
              map[e.new] = map[e.old];
              delete map[e.old];
            }
          }
          if (notifPrefs[e.old] !== undefined) {
            notifPrefs[e.new] = notifPrefs[e.old];
            delete notifPrefs[e.old];
          }
          cacheChanLayout(e.new, cur.category, cur.position ?? 0);
          if (active === e.old) active = e.new;
          if (chanPermsCh === e.old) chanPermsCh = e.new;
          // The actor was respawned under the new name — re-subscribe.
          weft.join(e.new).catch(() => {});
        }
        confirmSuccess(`rename:${e.new}`);
        break;
      }
      case "ns-meta":
        discovered[e.name] = e;
        cacheNsCats(e.name, e.categories ?? []);
        break;
      case "more":
        discoverCursor = e.cursor;
        break;
      case "manifest":
        // A bridge's channel set/state (§11). `severed`/`removed` drops it.
        if (e.state === "severed" || e.state === "removed") delete manifests[e.peer];
        else manifests[e.peer] = e;
        break;
      case "netblocked":
        netblocks[e.network] = e.reason;
        break;
      case "token":
        sys(`✓ permissions updated for ${e.subject} @ ${e.scope}`);
        break;
      case "invited":
        if (e.max_uses === 0) {
          // A revoke echo (INVITED … max-uses=0) — close it + drop from the menu.
          if (inviteId === e.invite_id) {
            inviteLink = null;
            inviteId = null;
          }
          invitesList = invitesList.filter((i) => i.invite_id !== e.invite_id);
        } else {
          inviteLink = e.link ?? e.invite_id;
          inviteId = e.invite_id;
          // A freshly-minted invite: reflect it live wherever the list is shown —
          // the standalone menu or the Server-Settings Invites tab.
          const listShown = invitesOpen || (nsSettingsOpen && nsTab === "invites");
          if (listShown && e.scope === invitesScope) weft.inviteList(invitesScope).catch(() => {});
        }
        break;
      case "invite-info":
        if (loadingInvites) invitesBuf.push(e);
        break;
      case "reported":
        sys(`✓ report filed (${e.report_id})`);
        break;
      case "report-filed":
        reportQueue[e.report_id] = e;
        break;
      case "report-resolved":
        delete reportQueue[e.report_id];
        sys(`✓ report ${e.report_id} resolved: ${e.action}`);
        break;
      case "typing":
        if (e.user !== account) setTyping(e.channel, e.user, e.state === "start");
        break;
      case "reaction": {
        // Live increment/decrement (§7). During a batch the target may still
        // be buffered, so search there too.
        const m = findMsg(e.target, e.msgid);
        if (m) applyReaction(m, e.emoji, e.op, e.by);
        break;
      }
      case "reactions": {
        // Compacted summary from history (§12.1) — set the aggregate directly.
        const m = findMsg(e.target, e.msgid);
        if (m) {
          m.reactions ??= {};
          m.reactions[e.emoji] = { count: e.count, mine: e.by.includes(account) };
        }
        break;
      }
      case "batch-start":
        currentBatchId = e.id; // `r…` = a ROLES batch (see below)
        break; // messages between here and batch-end are buffered above
      case "batch-end": {
        // A MODLIST batch only refreshed the deny-list cache (handled per
        // "moderated" event above) — nothing to flush here.
        if (currentBatchId.startsWith("mod")) {
          currentBatchId = "";
          break;
        }
        // A MEMBERS roster batch (`m…`): each MEMBER folded in live already, so
        // there's nothing to flush here. Crucially it must NOT fall through to the
        // history branch, or it would steal the in-flight HISTORY's `loadingHistory`
        // and the real page would be discarded. (Checked after `mod`, which also
        // starts with "m".)
        if (currentBatchId.startsWith("m")) {
          currentBatchId = "";
          break;
        }
        // NS INFO MEMBERS roster (`ni…`). Checked before the `r…` role branch
        // since neither prefix overlaps, and flushed by the requested namespace
        // so an empty roster still lands.
        if (currentBatchId.startsWith("ni")) {
          if (loadingNsMembers) nsMembersByNs[loadingNsMembers] = nsMemberBuf;
          nsMemberBuf = [];
          loadingNsMembers = null;
          nsMembersLoading = false;
          currentBatchId = "";
          break;
        }
        if (currentBatchId.startsWith("r")) {
          const scope = roleFetchQueue.shift();
          // Keep roles in position order (server sorts, but be safe).
          roleBuf.sort((a, b) => a.position - b.position || a.name.localeCompare(b.name));
          if (scope) rolesByScope[scope] = roleBuf;
          roleBuf = [];
          currentBatchId = "";
          break;
        }
        if (loadingThread) {
          threadMessages = threadBuf;
          threadBuf = [];
          loadingThread = null;
          break;
        }
        if (loadingSearch) {
          searchResults = searchBuf;
          searchBuf = [];
          loadingSearch = null;
          searching = false;
          break;
        }
        if (loadingPins) {
          const ch = channels[loadingPins];
          if (ch) ch.pinnedIds = pinsBuf.map((m) => m.msgid).filter(Boolean) as string[];
          pinsList = pinsBuf;
          pinsBuf = [];
          loadingPins = null;
          break;
        }
        if (loadingThreads) {
          // Newest activity first (last-activity msgid sorts by its ULID).
          threadsBuf.sort((a, b) => (b.last ?? "").localeCompare(a.last ?? ""));
          threadsList = threadsBuf;
          threadsBuf = [];
          loadingThreads = false;
          break;
        }
        if (loadingInvites) {
          invitesList = invitesBuf;
          invitesBuf = [];
          loadingInvites = false;
          break;
        }
        // Flush every channel that accumulated a history page. Each page goes to
        // the channel its messages name (`target`), so this is correct no matter
        // which batch's END fired or whether `loadingHistory` was cleared — a
        // stray batch can't lose a page. The requested channel is always flushed
        // (an empty page still marks it loaded, so we stop re-requesting).
        const requested = loadingHistory;
        const targets = new Set(Object.keys(histByTarget));
        if (requested) targets.add(requested);
        for (const t of targets) {
          const buf = histByTarget[t] ?? [];
          delete histByTarget[t];
          const ch = ensureChannel(t);
          const seen = new Set(ch.messages.map((m) => m.msgid).filter(Boolean));
          const older = buf.filter((m) => !m.msgid || !seen.has(m.msgid));
          ch.messages = [...older, ...ch.messages];
          ch.historyLoaded = true;
          if (t === requested) {
            ch.truncated = e.truncated;
            ch.hasMore = !e.truncated && buf.length >= HISTORY_LIMIT;
          }
        }
        loadingHistory = null;
        // If the reader switched to another conversation while this page was in
        // flight, its initial load was single-flight-blocked — kick it now. (The
        // channel's own MessageList positions itself once its page lands; paging
        // older needs no scroll adjustment — the column-reverse bottom is anchored.)
        const cur = channels[active];
        if (active !== requested && cur && !cur.voice && !cur.historyLoaded) {
          loadHistory(active, true);
        }
        break;
      }
      case "deleted": {
        // §7 tombstone — drop the message so it doesn't linger.
        const ch = channels[e.target];
        if (ch) ch.messages = ch.messages.filter((m) => m.msgid !== e.msgid);
        break;
      }
      case "edited": {
        // Update the original message in place (§7 edit-of).
        const m = channels[e.target]?.messages.find((x) => x.msgid === e.edit_of);
        if (m) {
          m.body = e.body;
          m.edited = true;
        }
        break;
      }
      case "moderated": {
        // Keep the deny-list cache current (for the Bans tab). A MODLIST reply
        // arrives inside a `mod`-batch; live actions arrive bare. `mute`/`ban`
        // add-or-replace; `unmute`/`unban` remove; `kick` is transient.
        if (e.action === "mute" || e.action === "ban") {
          const list = (modDeny[e.scope] ??= []);
          const i = list.findIndex((r) => r.account === e.account && r.kind === e.action);
          const rec = { account: e.account, kind: e.action, by: e.by, reason: e.reason };
          if (i >= 0) list[i] = rec;
          else list.push(rec);
        } else if (e.action === "unmute" || e.action === "unban") {
          const kind = e.action === "unmute" ? "mute" : "ban";
          if (modDeny[e.scope])
            modDeny[e.scope] = modDeny[e.scope].filter(
              (r) => !(r.account === e.account && r.kind === kind),
            );
        }
        // A list response shouldn't also post system lines in the timeline.
        if (currentBatchId.startsWith("mod")) break;
        // Surface the action as a system line in the affected channel. A
        // federated moderator (§11.11 homeserver authority) is attributed with
        // their @network and flagged — the "acting on H via F" affordance.
        const ch = e.scope.startsWith("#") ? ensureChannel(e.scope) : activeChannel;
        const fed = e.by && e.by.includes("@") && e.by.split("@")[1] !== network;
        const who = e.by ? ` by ${e.by}${fed ? " (via federation)" : ""}` : "";
        const why = e.reason ? ` (${e.reason})` : "";
        ch?.messages.push(mkMsg({ author: "", body: `${e.account} ${e.action}d${who} — ${e.scope}${why}`, time: clock(), ts: Date.now(), own: false, system: true }));
        break;
      }
      case "error":
        toast(`${e.code}: ${e.text}`, "error");
        break;
    }
  }

  // ---- actions ----
  // Device-key login availability (checked as host/account change).
  let deviceKeyAvailable = $state(false);
  $effect(() => {
    const h = host.trim();
    const a = formAccount.trim();
    if (h && a)
      weft
        .hasDeviceKey(h, a)
        .then((v) => (deviceKeyAvailable = v))
        .catch(() => (deviceKeyAvailable = false));
    else deviceKeyAvailable = false;
  });
  function keyLogin() {
    mode = "key";
    doConnect();
  }
  function enrollThisDevice() {
    weft
      .enrollDevice(host.trim(), account)
      .then(() => toast("Device key enrolled — passwordless login is on for next time"))
      .catch((e) => toast(String(e), "error"));
  }

  async function doConnect() {
    if (!formAccount.trim()) return;
    authError = "";
    authFailed = false;
    status = "connecting";
    manualLogout = false;
    reconnectAttempts = 0;
    // Held in memory (never persisted) so a mid-session drop can reconnect.
    lastCreds = { host: host.trim(), account: formAccount.trim(), password: formPassword };
    try {
      await weft.connect(host.trim(), formAccount.trim(), formPassword, mode);
    } catch (err) {
      status = "connect";
      authError = String(err);
    }
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
      weft.join(raw).catch((e) => (authError = String(e)));
    } else {
      joinNamespace(raw.replace(/^ns:/, ""));
    }
  }

  function sys(body: string) {
    if (activeChannel)
      activeChannel.messages.push(mkMsg({ author: "", body, time: clock(), ts: Date.now(), own: false, system: true }));
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
    const savedReply = replyTo?.msgid;
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
    replyTo = null;
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
  function findMsg(target: string, msgid: string): Msg | undefined {
    return (
      histByTarget[target]?.find((m) => m.msgid === msgid) ??
      channels[target]?.messages.find((m) => m.msgid === msgid)
    );
  }

  function applyReaction(m: Msg, emoji: string, op: string, by: string) {
    m.reactions ??= {};
    const cur = m.reactions[emoji] ?? { count: 0, mine: false };
    if (op === "add") {
      cur.count += 1;
      if (by === account) cur.mine = true;
    } else {
      cur.count -= 1;
      if (by === account) cur.mine = false;
    }
    if (cur.count <= 0) delete m.reactions[emoji];
    else m.reactions[emoji] = cur;
  }

  // Non-optimistic: the server echoes our own REACTION back (like a MSG ack),
  // which drives the count — so toggling can't double-count.
  function toggleReaction(m: Msg, emoji: string) {
    if (!m.msgid) return;
    pickerKey = null;
    const mine = m.reactions?.[emoji]?.mine;
    (mine ? weft.unreact(m.msgid, emoji) : weft.react(m.msgid, emoji)).catch(() => {});
  }

  // ---- markdown (Phase 4 · Tier 1) ----
  // Escape-first: safe to feed {@html} because HTML is neutralised before any
  // markdown token is turned back into a tag. Quotes are escaped too — the link
  // rewriters interpolate a captured URL into `href="${url}"`, and a URL char
  // class permits `"`, so without this a body like `https://x/"onfocus="…` would
  // break out of the attribute and inject an event handler (attribute-injection
  // XSS → the Tauri command bridge on desktop). Escaping here fixes it at the
  // root, for every attribute interpolation, not just links.
  const escapeHtml = (s: string) =>
    s
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#39;");

  // Inline formatting for a single run of text (no fenced/block constructs).
  // Code spans and links are stashed to placeholders BEFORE emphasis runs, so
  // markdown characters inside a URL or code span (snake_case, a*b, …) can't be
  // mangled into <em>/<strong>. \x00…\x00 is used as the placeholder delimiter
  // because a NUL can never occur in a chat line.
  function renderInline(text: string): string {
    const stash: string[] = [];
    const keep = (html: string) => {
      const i = stash.length;
      stash.push(html);
      return `\x00T${i}\x00`;
    };

    let s = escapeHtml(text);

    // Inline code — verbatim, highest precedence.
    s = s.replace(/`([^`]+)`/g, (_m, c: string) => keep(`<code>${c}</code>`));

    // Masked link [text](url) then bare URL — stashed so emphasis can't touch
    // the URL. `data-mdlink` marks them for the click-through confirm guard.
    s = s.replace(
      /\[([^\]]+)\]\((https?:\/\/[^\s)]+)\)/g,
      (_m, txt: string, url: string) =>
        keep(`<a href="${url}" target="_blank" rel="noopener noreferrer" data-mdlink="1">${txt}</a>`),
    );
    s = s.replace(
      /(^|\s)(https?:\/\/[^\s<]+)/g,
      (_m, pre: string, url: string) =>
        pre + keep(`<a href="${url}" target="_blank" rel="noopener noreferrer" data-mdlink="1">${url}</a>`),
    );

    // Emphasis: ***bold-italic*** → **bold** → __bold__ → *italic* → _italic_ → ~~strike~~.
    s = s.replace(/\*\*\*([^*]+)\*\*\*/g, "<strong><em>$1</em></strong>");
    s = s.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
    s = s.replace(/__([^_]+)__/g, "<strong>$1</strong>");
    s = s.replace(/(^|[^*])\*([^*\n]+)\*/g, "$1<em>$2</em>");
    // _italic_ only at word boundaries, so snake_case is left alone.
    s = s.replace(/(^|[^\w])_([^_\n]+)_(?=[^\w]|$)/g, "$1<em>$2</em>");
    s = s.replace(/~~([^~]+)~~/g, "<del>$1</del>");

    // ||spoiler|| → click-to-reveal (revealed by a delegated handler in the list).
    s = s.replace(
      /\|\|([\s\S]+?)\|\|/g,
      '<span class="spoiler" role="button" tabindex="0" title="Spoiler — click to reveal">$1</span>',
    );
    // @mentions → pills; a mention of me / @everyone / @here / a pingable role
    // I hold highlights. Role pills carry the role's color.
    const pingable = (rolesByScope[`ns:${activeServer}`] ?? []).filter((r) => r.pingable);
    const myRoles = new Set(memberRoles[`${account}|ns:${activeServer}`] ?? []);
    s = s.replace(/@(everyone|here|[a-z0-9][a-z0-9._-]*)/gi, (_full, name: string) => {
      const lower = name.toLowerCase();
      const role = pingable.find((r) => r.name.toLowerCase() === lower);
      const me =
        name === account ||
        lower === "everyone" ||
        lower === "here" ||
        (!!role && myRoles.has(role.name));
      // Colors ride the wire, so only emit ones matching a strict hex pattern —
      // never interpolate arbitrary text into a style attribute.
      const style = role && /^#[0-9a-fA-F]{3,8}$/.test(role.color) ? ` style="color:${role.color}"` : "";
      return `<span class="mention${me ? " me" : ""}"${style}>@${name}</span>`;
    });
    // :name: → this server's custom emoji (an inline image) if it exists, else a
    // standard unicode emoji (`:smile:` → 😄); an unknown shortcode stays literal.
    s = s.replace(/:([a-zA-Z0-9_+-]+):/g, (full, name: string) => {
      const media = customEmoji[activeServer]?.[name];
      if (media) {
        const url = weft.mediaUrl(media).replace(/&/g, "&amp;").replace(/"/g, "&quot;");
        return `<img class="custom-emoji" src="${url}" alt=":${name}:" title=":${name}:" />`;
      }
      return shortcodeToChar(name) ?? full;
    });

    // Restore stashed code spans / links.
    s = s.replace(/\x00T(\d+)\x00/g, (_m, i: string) => stash[+i]);
    return s;
  }

  // Full render: lift out ``` / ~~~ fenced code blocks (verbatim, highlighted),
  // parse block-level constructs (headings, block quotes, lists, rules) line by
  // line, inline-format the rest, then splice the code blocks back in.
  function renderMd(text: string): string {
    const blocks: { lang: string; code: string }[] = [];
    const lifted = text.replace(
      /(?:```|~~~)([a-zA-Z0-9+#.-]*)\n?([\s\S]*?)(?:```|~~~)/g,
      (_m, lang: string, code: string) => {
        const i = blocks.length;
        blocks.push({ lang: lang.trim(), code: code.replace(/\n$/, "") });
        return `\x00CB${i}\x00`;
      },
    );

    const lines = lifted.split("\n");
    const pieces: { block: boolean; html: string }[] = [];
    const cbOnly = /^\s*\x00CB\d+\x00\s*$/;
    let i = 0;
    while (i < lines.length) {
      const line = lines[i];

      // A fenced-code placeholder alone on its line is a block.
      if (cbOnly.test(line)) {
        pieces.push({ block: true, html: line.trim() });
        i++;
        continue;
      }
      // ATX headings # / ## / ### (h1–h3, Discord-style).
      const h = line.match(/^(#{1,3})\s+(.*)$/);
      if (h) {
        const lvl = h[1].length;
        pieces.push({ block: true, html: `<h${lvl} class="md-h md-h${lvl}">${renderInline(h[2])}</h${lvl}>` });
        i++;
        continue;
      }
      // Thematic break: ---, ***, ___ (three or more).
      if (/^\s*([-*_])(?:\s*\1){2,}\s*$/.test(line)) {
        pieces.push({ block: true, html: `<hr class="md-hr" />` });
        i++;
        continue;
      }
      // Block quote: `>>> ` quotes the rest of the message; `> ` quotes a run.
      const tri = line.match(/^>>>\s?(.*)$/);
      if (tri) {
        const rest = [tri[1], ...lines.slice(i + 1)];
        pieces.push({
          block: true,
          html: `<blockquote class="md-quote">${rest.map((l) => renderInline(l)).join("<br>")}</blockquote>`,
        });
        break;
      }
      if (/^>\s?/.test(line)) {
        const buf: string[] = [];
        while (i < lines.length && /^>\s?/.test(lines[i])) {
          buf.push(lines[i].replace(/^>\s?/, ""));
          i++;
        }
        pieces.push({
          block: true,
          html: `<blockquote class="md-quote">${buf.map((l) => renderInline(l)).join("<br>")}</blockquote>`,
        });
        continue;
      }
      // Unordered list: -, *, + .
      if (/^\s*[-*+]\s+/.test(line)) {
        const items: string[] = [];
        while (i < lines.length && /^\s*[-*+]\s+/.test(lines[i])) {
          items.push(lines[i].replace(/^\s*[-*+]\s+/, ""));
          i++;
        }
        pieces.push({
          block: true,
          html: `<ul class="md-list">${items.map((it) => `<li>${renderInline(it)}</li>`).join("")}</ul>`,
        });
        continue;
      }
      // Ordered list: 1. / 1) .
      if (/^\s*\d+[.)]\s+/.test(line)) {
        const items: string[] = [];
        while (i < lines.length && /^\s*\d+[.)]\s+/.test(lines[i])) {
          items.push(lines[i].replace(/^\s*\d+[.)]\s+/, ""));
          i++;
        }
        pieces.push({
          block: true,
          html: `<ol class="md-list">${items.map((it) => `<li>${renderInline(it)}</li>`).join("")}</ol>`,
        });
        continue;
      }

      // Plain line.
      pieces.push({ block: false, html: renderInline(line) });
      i++;
    }

    // Assemble: consecutive plain lines keep their newline (rendered by the
    // container's pre-wrap); block elements bring their own separation.
    let s = "";
    for (let k = 0; k < pieces.length; k++) {
      if (k > 0 && !pieces[k].block && !pieces[k - 1].block) s += "\n";
      s += pieces[k].html;
    }

    // Splice fenced code blocks back in, highlighted.
    s = s.replace(/\x00CB(\d+)\x00/g, (_m, i: string) => {
      const b = blocks[+i];
      const label = b.lang ? `<span class="code-lang">${escapeHtml(b.lang)}</span>` : "";
      return `<pre class="code-block hljs">${label}<code>${highlightCode(b.code, b.lang)}</code></pre>`;
    });
    return s;
  }
  // Does a body mention the current account, @everyone/@here, or a pingable
  // role the account holds at `ns` (the message's server; defaults to active)?
  const mentionsMe = (body: string, ns: string = activeServer) => {
    if (!account) return false;
    if (new RegExp(`@${account}\\b`, "i").test(body) || /@(everyone|here)\b/i.test(body)) return true;
    const scope = ns ? `ns:${ns}` : "*";
    const mine = memberRoles[`${account}|${scope}`] ?? [];
    const pingable = new Set(
      (rolesByScope[scope] ?? []).filter((r) => r.pingable).map((r) => r.name.toLowerCase()),
    );
    return mine.some(
      (r) =>
        pingable.has(r.toLowerCase()) &&
        new RegExp(`@${r.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}\\b`, "i").test(body),
    );
  };

  // ---- replies (Phase 4) ----
  let replyTo = $state<Msg | null>(null);
  function jumpTo(msgid?: string) {
    if (!msgid) return;
    const m = activeChannel?.messages.find((x) => x.msgid === msgid);
    if (m) document.getElementById(`msg-${m.key}`)?.scrollIntoView({ block: "center" });
  }

  // ---- typing indicators (Phase 4) ----
  let typers = $state<Record<string, string[]>>({}); // channel -> accounts typing
  const typingTimers = new Map<string, ReturnType<typeof setTimeout>>();
  function setTyping(channel: string, user: string, active: boolean) {
    const key = `${channel}\u0000${user}`;
    clearTimeout(typingTimers.get(key));
    typers[channel] ??= [];
    if (active) {
      if (!typers[channel].includes(user)) typers[channel] = [...typers[channel], user];
      // Fallback expiry in case a `stop` is lost.
      typingTimers.set(key, setTimeout(() => setTyping(channel, user, false), 6000));
    } else {
      typers[channel] = typers[channel].filter((u) => u !== user);
      typingTimers.delete(key);
    }
  }
  let typingLabel = $derived.by(() => {
    const who = active ? (typers[active] ?? []) : [];
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
    for (const r of rolesByScope[`ns:${activeServer}`] ?? [])
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

  // On opening a text channel: keep it in the mounted set (most-recent first,
  // capped) so a return to it is instant, and fetch its roster once. History
  // load + scroll positioning are owned by the channel's own <MessageList>. Only
  // `active` is a dependency here — `keptChannels` is read/written untracked, so
  // this can never self-trigger (the trap that broke the last attempt).
  $effect(() => {
    const a = active;
    if (!a) return;
    untrack(() => {
      const ch = channels[a];
      if (!ch || ch.voice) return; // voice has no message list
      if (keptChannels[0] !== a) {
        keptChannels = [a, ...keptChannels.filter((c) => c !== a)].slice(0, KEEP_ALIVE_MAX);
      }
      // Fetch the full roster once (MEMBERS folds in as MEMBER-join rows).
      if (a.startsWith("#") && !ch.rosterLoaded) {
        ch.rosterLoaded = true;
        weft.members(a).catch(() => {});
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

  // Opening a channel selects its server tile (keeps the rail in sync with
  // auto-joins and sidebar clicks).
  $effect(() => {
    if (active.startsWith("#")) activeServer = nsOf(active);
  });

  // ---- discover + channel management (Phase 6) ----
  function openDiscover() {
    discoverOpen = true;
    discovered = {};
    discoverCursor = null;
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
    if (m.msgid) reportTarget = m;
  }
  function openReports() {
    reportsOpen = true;
    reportQueue = {};
    weft.reportsList(activeServer ? `ns:${activeServer}` : "*").catch(() => {});
  }


  // Invites — every entry point opens the creation screen (pick expiry + max
  // uses, then generate), rather than minting a fixed invite immediately.
  function openInviteCreate(scope?: string) {
    inviteCreateScope = scope || scopesFor()[0] || "";
    inviteLink = null;
    inviteId = null;
    inviteCreateOpen = true;
  }
  function mintInvite() {
    openInviteCreate();
  }
  // Mint with the chosen limits — `null` = unlimited uses / never expires. The
  // resulting link arrives on the `invited` event and fills `inviteLink`.
  function generateInvite(maxUses: number | null, expiry: number | null) {
    if (!inviteCreateScope) return;
    weft
      .inviteMint(inviteCreateScope, maxUses ?? undefined, expiry ?? undefined)
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
    invitesScope = scope;
    invitesList = [];
    invitesBuf = [];
    loadingInvites = true;
    weft.inviteList(invitesScope).catch((e) => {
      loadingInvites = false;
      toast(String(e), "error");
    });
  }
  function openInvites() {
    loadInvites(scopesFor()[0]);
    invitesOpen = true;
  }
  // The Server-Settings Invites tab lists the whole namespace's invites.
  function loadNsInvites() {
    if (activeServer) loadInvites(`ns:${activeServer}`);
  }
  function revokeInvite(id: string) {
    weft.inviteRevoke(id).catch((e) => toast(String(e), "error"));
    invitesList = invitesList.filter((i) => i.invite_id !== id); // optimistic
  }
  function createInvite() {
    openInviteCreate(invitesScope || scopesFor()[0]);
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
    if (!slug) return;
    const full = activeServer ? `#${activeServer}/${slug}` : `#${slug}`;
    const cat = newChanCategory.trim();
    const voice = newChanVoice;
    weft
      .channelCreate(full, voice ? undefined : newChanRet || undefined, voice ? "voice" : undefined)
      // We just created it, so the server won't tell us its kind — record it
      // locally so the sidebar shows a voice channel (joined via VOICE, not text).
      .then(() => {
        ensureChannel(full).voice = voice;
      })
      // Voice channels aren't text-joinable — don't JOIN (that's NO-SUCH-TARGET).
      .then(() => (voice ? undefined : weft.join(full)))
      .then(() => (cat ? weft.channelMeta(full, "category", cat) : undefined))
      // Announcement channel: everyone can view, only members with the `send`
      // capability may post (§6.7 restricted posting). N/A to voice.
      .then(() =>
        !voice && newChanAnnounce ? weft.channelMeta(full, "posting", "restricted") : undefined,
      )
      .then(() => (newChanOpen = false))
      .catch((e) => toast(String(e), "error"));
  }

  // ---- categories (Discord-style groupings) ----
  // A category is just a label channels carry (§6.3 CHANNEL META category). An
  // *empty* category has no channel yet, so we remember it client-side (per
  // server) until a channel is dragged in — then the server persists it.
  let newCatOpen = $state(false);
  let newCatName = $state("");
  // Categories are server state (§6.3, on the namespace) — no client copy.
  const nsCategories = () => discovered[activeServer]?.categories ?? [];
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

    const meta = discovered[activeServer];
    if (meta) meta.categories = cats; // optimistic; the NS-META echo confirms
    setCategories(cats);
  }

  // ---- per-channel permissions (§6.5 grants at #chan scope, §6.7 restricted) ----
  let chanPermsCh = $state<string | null>(null);
  function chanNsScope() {
    const ns = nsOf(chanPermsCh ?? "");
    return ns ? `ns:${ns}` : "*";
  }
  const chanRoleCaps = (name: string) =>
    (rolesByScope[chanPermsCh ?? ""] ?? []).find((r) => r.name === name)?.caps ?? [];
  function toggleChanRoleCap(role: RoleDefC, cap: string) {
    if (!chanPermsCh) return;
    const cur = chanRoleCaps(role.name);
    const next = cur.includes(cap) ? cur.filter((c) => c !== cap) : [...cur, cap];
    (next.length
      ? createRoleAt(chanPermsCh, role.name, role.color, next.join(","))
      : deleteRoleAt(chanPermsCh, role.name)
    ).catch((e) => toast(String(e), "error"));
  }
  function openChanPerms(channel: string) {
    chanPermsCh = channel;
    fetchRoles(chanNsScope()); // the namespace's roles
    fetchRoles(channel); // this channel's role-permissions
  }
  function toggleRestricted() {
    const ch = chanPermsCh ? channels[chanPermsCh] : undefined;
    if (!ch || !chanPermsCh) return;
    const next = !ch.restricted;
    weft
      .channelMeta(chanPermsCh, "posting", next ? "restricted" : "open")
      .then(() => (ch.restricted = next))
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
    pinsOpen = true;
    pinsList = [];
    loadingPins = active;
    weft.pins(active).catch(() => {});
  }

  // ---- message search (§6.4) ----
  function openSearch() {
    if (!active.startsWith("#")) return;
    searchQuery = "";
    searchResults = [];
    searchScope = active;
    searchOpen = true;
  }
  function runSearch(query: string) {
    const q = query.trim();
    if (!q || !active.startsWith("#")) return;
    searchQuery = q;
    searchScope = active;
    searchResults = [];
    searchBuf = [];
    searching = true;
    loadingSearch = active;
    weft.search(active, q).catch((e) => {
      loadingSearch = null;
      searching = false;
      toast(String(e), "error");
    });
  }
  function jumpToResult(m: Msg) {
    searchOpen = false;
    jumpTo(m.msgid); // best-effort: scrolls if the message is loaded in the timeline
  }

  // ---- threads (§9.4) ----
  // How many loaded replies a root has (its thread size), for the indicator.
  const threadCount = (msgid?: string): number =>
    !msgid || !activeChannel ? 0 : activeChannel.messages.filter((m) => m.thread === msgid).length;
  function openThread(root: Msg) {
    if (!root.msgid) return;
    threadRoot = root;
    threadMessages = [root];
    threadComposer = "";
    loadingThread = root.msgid;
    weft.history(active, undefined, root.msgid).catch((e) => {
      loadingThread = null;
      toast(String(e), "error");
    });
  }
  function closeThread() {
    threadRoot = null;
    threadMessages = [];
    loadingThread = null;
    threadBuf = [];
  }
  function sendThread() {
    const text = threadComposer.trim();
    if (!text || !threadRoot?.msgid || !active) return;
    weft
      .sendMessage(active, text, undefined, [], threadRoot.msgid)
      .then(() => (threadComposer = ""))
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
      threadsOpen = false;
    }
  });

  // ---- threads list (§9.4): all threads in the active channel ----
  function openThreads() {
    if (!active.startsWith("#")) return;
    threadsOpen = true;
    threadsList = [];
    threadsBuf = [];
    loadingThreads = true;
    weft.listThreads(active).catch((e) => {
      loadingThreads = false;
      toast(String(e), "error");
    });
  }
  function closeThreads() {
    threadsOpen = false;
  }
  // Open a thread from the list. If its root is already in the timeline, reuse
  // it; otherwise seed a placeholder — the thread HISTORY (which includes the
  // root) replaces it on arrival.
  function openThreadByRoot(info: ThreadInfo) {
    threadsOpen = false;
    const loaded = activeChannel?.messages.find((m) => m.msgid === info.root);
    if (loaded) {
      openThread(loaded);
      return;
    }
    openThread(mkMsg({ author: "", body: "", time: "", ts: 0, own: false, msgid: info.root }));
  }
  // A thread's display name (from THREAD / THREAD-NAMED), for the indicator
  // and the panel title.
  const threadNameFor = (msgid?: string): string | undefined => (msgid ? threadNames[msgid] : undefined);
  // Rename (or, with an empty string, clear the name of) the open thread.
  function renameThread(name: string) {
    if (!threadRoot?.msgid || !active) return;
    weft.nameThread(active, threadRoot.msgid, name.trim()).catch((e) => toast(String(e), "error"));
  }

  // Namespace admin
  function openNsSettings() {
    const meta = discovered[activeServer];
    nsTitle = meta?.title ?? "";
    nsDesc = meta?.description ?? "";
    nsVis = meta?.visibility ?? "public";
    nsDelegSubject = "";
    nsNewOwner = "";
    nsRecKeys = "";
    nsTab = "overview";
    nsSettingsOpen = true;
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
  function doTransfer() {
    const o = nsNewOwner.trim();
    if (o && confirm(`Transfer ownership of ${activeServer} to ${o}? This is signed by your root key and cannot be undone.`))
      weft.nsTransfer(network, activeServer, o).catch((e) => (authError = String(e)));
  }
  function deleteNamespace() {
    if (confirm(`Delete namespace ${activeServer}? This removes all its channels.`)) {
      weft.nsDelete(activeServer).catch(() => {});
      nsSettingsOpen = false;
    }
  }

  // Revoke every outstanding invite for the active namespace (ns-admin, §6.5).
  function revokeAllInvites() {
    if (!activeServer) return;
    if (!confirm(`Revoke ALL invites for ${activeServer}? Every existing invite link stops working.`)) return;
    weft.inviteRevokeAll(`ns:${activeServer}`).catch(() => {});
    invitesList = []; // optimistic — the list is now empty
    toast(`Revoked all invites for ${activeServer}`, "info");
  }

  onMount(() => {
    // Restore the cached layout for instant render before the server refresh.
    try {
      layoutCache = JSON.parse(localStorage.getItem("weft:layout") ?? "{}");
    } catch {
      layoutCache = {};
    }
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
    // Load client.toml: TLS verification mode + optional default host.
    weft
      .clientConfig()
      .then((c) => {
        insecureMode = c.allow_insecure;
        if (c.default_host && host === "127.0.0.1:4433") host = c.default_host;
      })
      .catch(() => {});
    // Restore the last session and log straight back in (login mode — the
    // account already exists).
    try {
      const saved = JSON.parse(localStorage.getItem(SAVED_KEY) ?? "null");
      // On web the network is always the page origin — don't restore a stale host.
      if (saved?.host && !weft.isWeb) host = saved.host;
      if (saved?.account) formAccount = saved.account;
      if (saved?.host && saved?.account && saved?.password) {
        formPassword = saved.password;
        mode = "login";
        doConnect();
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
    get groupCallRoster() { return groupCallRoster; },
    get activeGroupCall() { return activeGroupCall; },
    startGroupCall,
    leaveGroupCall,
    // friend calls
    get incomingCall() { return incomingCall; },
    get activeCall() { return activeCall; },
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
    open: (name: string) => { active = name; markRead(name); },
    // Open a voice channel's stage (switch the main view) and join the call if
    // we're not already in it. Voice channels have no message timeline, so we
    // don't markRead.
    openVoice: (name: string) => {
      active = name;
      if (voice.channel !== name) joinVoice(name);
    },
    openDiscover,
    get channels() { return channels; },
    get presence() { return presence; },
    get unreadMap() { return unreadMap; },
    get mentionMap() { return mentionMap; },
    get unreadCount() { return unreadCount; },
    get mentionCount() { return mentionCount; },
    isMuted,
    serverMuted,
    notifLevelOf,
    setNotifLevel,
    notifScopeKey,
    notifScopeLabel,
    get notifSettingsOpen() { return notifSettingsOpen; },
    set notifSettingsOpen(v: boolean) { notifSettingsOpen = v; },
    openNotifSettings,
    get discovered() { return discovered; },
    get discoverCursor() { return discoverCursor; },
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
    // invites menu (Discord-style)
    get invitesList() { return invitesList; },
    get invitesScope() { return invitesScope; },
    openInvites,
    loadNsInvites,
    revokeInvite,
    createInvite,
    inviteLinkFor,
    // invite creation screen
    get inviteLink() { return inviteLink; },
    get inviteId() { return inviteId; },
    get inviteCreateScope() { return inviteCreateScope; },
    generateInvite,
    sendInviteDM,
    newCat: openCreateCategory,
    openProfile,
    openFullProfile,
    mutualServers,
    friendState,
    friendAction,
    openDm,
    moderate,
    openSettings: () => { userTab = "account"; settingsOpen = true; userMenu = false; },
    toast,
    expectSuccess,
    get reportQueue() { return reportQueue; },
    get pinsList() { return pinsList; },
    resolveActions: RESOLVE_ACTIONS,
    // chat topbar
    get membersVisible() { return membersVisible; },
    set membersVisible(v: boolean) { membersVisible = v; },
    openPins,
    openReports,
    partActive: () => weft.part(active).catch(() => {}),
    // search
    get searchOpen() { return searchOpen; },
    set searchOpen(v: boolean) { searchOpen = v; },
    get searchQuery() { return searchQuery; },
    get searchScope() { return searchScope; },
    get searchResults() { return searchResults; },
    get searching() { return searching; },
    openSearch,
    runSearch,
    jumpToResult,
    // threads
    get threadRoot() { return threadRoot; },
    get threadMessages() { return threadMessages; },
    get threadComposer() { return threadComposer; },
    set threadComposer(v: string) { threadComposer = v; },
    get visibleMessages() { return visibleMessages; },
    get visibleMessagesReversed() { return visibleMessagesReversed; },
    threadCount,
    openThread,
    closeThread,
    sendThread,
    // threads list (§9.4)
    get threadsOpen() { return threadsOpen; },
    get threadsList() { return threadsList; },
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
    get loadingHistory() { return loadingHistory; },
    get newBoundary() { return newBoundary; },
    channelRecord,
    loadHistory,
    get editingKey() { return editingKey; },
    set editingKey(v: number | null) { editingKey = v; },
    get editDraft() { return editDraft; },
    set editDraft(v: string) { editDraft = v; },
    get pickerKey() { return pickerKey; },
    set pickerKey(v: number | null) { pickerKey = v; },
    get replyTo() { return replyTo; },
    set replyTo(v: Msg | null) { replyTo = v; },
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
    renderMd,
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
    get rolesByScope() { return rolesByScope; },
    rolesOf,
    ensureMemberRoles,
    ensureRoles,
    get nsMembersByNs() { return nsMembersByNs; },
    get nsMembersLoading() { return nsMembersLoading; },
    fetchNsMembers,
    roleScopeOf,
    isOwnerAt,
    assignRoleTo,
    unassignRoleFrom,
    // channel permissions (role-based only)
    chanNsScope,
    chanRoleCaps,
    toggleChanRoleCap,
    toggleRestricted,
    // federation (operator)
    get isOperator() { return isOperator; },
    get netblocks() { return netblocks; },
    get manifests() { return manifests; },
    openFederation,
    refreshNetblocks,
    netblockAdd,
    netblockRemove,
    bridgePropose,
    bridgeAccept,
    bridgeSever,
    // user settings
    get theme() { return theme; },
    get host() { return host; },
    get reconnecting() { return reconnecting; },
    setStatus,
    toggleTheme,
    enrollThisDevice: enrollThisDevice,
    logout,
    // user settings (page overlay)
    get userTab() { return userTab; },
    set userTab(v: "account" | "appearance" | "connection" | "verification") { userTab = v; },
    get verifications() { return verifications; },
    // server settings (ns overlay)
    get nsTab() { return nsTab; },
    set nsTab(v: "overview" | "roles" | "members" | "emoji" | "invites" | "bans" | "federation" | "recovery" | "danger") { nsTab = v; },
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
</script>

<svelte:window onkeydown={globalKey} />

{#if status !== "online"}
  <ConnectScreen
    bind:mode
    bind:host
    bind:formAccount
    bind:formPassword
    {status}
    {authError}
    {deviceKeyAvailable}
    insecure={insecureMode}
    onconnect={doConnect}
    onkeylogin={keyLogin}
  />
{:else}
  <!-- ================= MAIN APP ================= -->
  {#if reconnecting}
    <div class="reconnect-banner">Connection lost — reconnecting…</div>
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
        unread: !!unreadMap[c.name],
      }))}
      onselect={switchTo}
      onclose={() => (switcherOpen = false)}
    />
  {/if}
  <div class="app" class:members-collapsed={!membersVisible || activeChannel?.voice}>
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
      {#if homeView && !activeChannel}
        <FriendsView />
      {:else if !activeChannel && !homeView}
        <EmptyHome />
      {:else if activeChannel?.voice}
        <VoiceStage />
      {:else}
        <ChatTopbar />

        <div class="msg-area">
          <!-- One self-contained, kept-alive list per recently-opened channel;
               only the active one is shown. Each owns its scroll, scrollbar and
               skeleton — switching back is instant. -->
          {#each keptChannels as ch (ch)}
            <MessageList channel={ch} active={ch === active} />
          {/each}
        </div>
        <Composer />
      {/if}
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

    {#if reportTarget}
      <ReportModal target={reportTarget} onclose={() => (reportTarget = null)} />
    {/if}


    {#if reportsOpen}
      <ReportsQueueModal onclose={() => (reportsOpen = false)} />
    {/if}

    {#if inviteCreateOpen}
      <InviteCreateModal onclose={() => { inviteCreateOpen = false; inviteLink = null; inviteId = null; }} />
    {/if}

    {#if invitesOpen}
      <InvitesModal onclose={() => (invitesOpen = false)} />
    {/if}

    {#if groupPickerOpen}
      <NewGroupModal
        seed={groupPickerSeed}
        pos={groupPickerPos}
        onclose={() => (groupPickerOpen = false)}
        oncreate={createGroupWith}
      />
    {/if}

    {#if pinsOpen}
      <PinsModal onclose={() => (pinsOpen = false)} />
    {/if}

    {#if threadsOpen}
      <ThreadsModal onclose={() => (threadsOpen = false)} />
    {/if}

    {#if searchOpen}
      <SearchModal onclose={() => (searchOpen = false)} />
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

    {#if chanPermsCh}
      <ChannelSettings channel={chanPermsCh} onclose={() => (chanPermsCh = null)} />
    {/if}

    {#if profileTarget}
      <ProfileCard target={profileTarget} pos={profilePos} onclose={() => (profileTarget = null)} />
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

    {#if nsSettingsOpen}
      <ServerSettingsModal onclose={() => (nsSettingsOpen = false)} />
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
