<script lang="ts">
  import { onMount, untrack } from "svelte";
  import * as weft from "$lib/weft";
  import type { Msg, Channel, CtxItem, RoleDefC } from "$lib/types";
  import { provideApp } from "$lib/context";
  import ConnectScreen from "$lib/components/ConnectScreen.svelte";
  import Toasts from "$lib/components/Toasts.svelte";
  import ContextMenu from "$lib/components/ContextMenu.svelte";
  import QuickSwitcher from "$lib/components/QuickSwitcher.svelte";
  import CommunityRail from "$lib/components/CommunityRail.svelte";
  import EmptyHome from "$lib/components/EmptyHome.svelte";
  import MemberList from "$lib/components/MemberList.svelte";
  import { initVoice } from "$lib/voice.svelte";
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
  import InviteLinkModal from "$lib/components/modals/InviteLinkModal.svelte";
  import PinsModal from "$lib/components/modals/PinsModal.svelte";
  import SearchModal from "$lib/components/modals/SearchModal.svelte";
  import DiscoverModal from "$lib/components/modals/DiscoverModal.svelte";
  import ReportModal from "$lib/components/modals/ReportModal.svelte";
  import ChannelSettings from "$lib/components/modals/ChannelSettings.svelte";
  import ProfileCard from "$lib/components/modals/ProfileCard.svelte";
  import UserSettingsModal from "$lib/components/modals/UserSettingsModal.svelte";
  import FederationPanel from "$lib/components/modals/FederationPanel.svelte";
  import ServerSettingsModal from "$lib/components/modals/ServerSettingsModal.svelte";
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
    ctxMenu = { x: Math.min(e.clientX, window.innerWidth - 190), y: e.clientY, items };
  }
  function msgCtx(e: MouseEvent, m: Msg) {
    if (m.system || !m.msgid) return;
    const items: CtxItem[] = [{ label: "Reply", run: () => (replyTo = m) }];
    if (active.startsWith("#"))
      items.push({
        label: activeChannel?.pinnedIds?.includes(m.msgid) ? "Unpin" : "Pin",
        run: () => togglePin(m),
      });
    items.push({ label: "Copy text", run: () => navigator.clipboard?.writeText(m.body) });
    if (m.own) {
      items.push({ label: "Edit", run: () => startEdit(m) });
      items.push({ label: "Delete", danger: true, run: () => doDelete(m) });
    } else {
      items.push({ label: "Report", run: () => openReport(m) });
    }
    openCtx(e, items);
  }
  function chanCtx(e: MouseEvent, ch: Channel) {
    openCtx(e, [
      { label: "Mark as read", run: () => markRead(ch.name) },
      { label: "Permissions", run: () => openChanPerms(ch.name) },
      { label: "Copy name", run: () => navigator.clipboard?.writeText(ch.name) },
      { label: "Leave", danger: true, run: () => weft.part(ch.name).catch(() => {}) },
    ]);
  }
  function memberCtx(e: MouseEvent, name: string) {
    if (name === account) return;
    openCtx(e, [
      { label: "Message", run: () => openDm(name) },
      { label: "Profile", run: () => openProfile(name) },
      { label: "Mute", run: () => moderate("mute", name) },
      { label: "Ban", danger: true, run: () => moderate("ban", name) },
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
    active = "";
    activeServer = "";
    homeView = false;
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
  let scrollEl: HTMLDivElement | null = $state(null);
  // ---- servers/namespaces as rail tiles (Phase 6, flavor A) ----
  let activeServer = $state(""); // "" = network top-level channels; else a namespace
  // "#gaming/general" → "gaming"; top-level "#general" → "".
  const nsOf = (name: string) => name.match(/^#([^/]+)\//)?.[1] ?? "";
  // Short channel label under a server tile: "#gaming/general" → "general".
  const chanShort = (name: string) => name.replace(/^#[^/]+\//, "").replace(/^#/, "");
  // ---- DMs + presence (Phase 5) ----
  let homeView = $state(false); // sidebar shows DMs instead of channels
  let presence = $state<Record<string, string>>({}); // account → status
  // §10.3 account → display profile (nick + avatar hash). Filled from PROFILE
  // events (broadcast on change) + on-demand PROFILES queries.
  let profiles = $state<Record<string, { display?: string; avatar?: string }>>({});
  let myStatus = $state("online");
  // §10.5 the caller's own verification claims, keyed by kind (email/birthday).
  let verifications = $state<Record<string, { subject: string; state: string }>>({});
  // Footer user menu (presence + settings + logout) and the user-settings page tab.
  let userMenu = $state(false);
  let userTab = $state<"account" | "appearance" | "connection" | "verification">("account");
  let dmInput = $state("");
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
  function createRoleAt(scope: string, name: string, color: string, caps: string) {
    roleFetchQueue.push(scope);
    return weft.roleCreate(scope, color, caps, name);
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
  /// The role definitions an account is assigned at a scope.
  function rolesOf(account: string, scope: string): RoleDefC[] {
    const names = new Set(memberRoles[`${account}|${scope}`] ?? []);
    return (rolesByScope[scope] ?? []).filter((r) => names.has(r.name));
  }

  let profilePos = $state<{ left: number; top: number } | null>(null);
  function openProfile(account: string, e?: MouseEvent) {
    profileTarget = account;
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
    ensureCaps(account, active); // channel-scope owner/mod badges
    ensureCapsAt(account, scope); // for the owner check
    fetchRoles(scope); // role definitions (names + colors)
    fetchMemberRoles(account, scope); // this member's assigned roles
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
  let nsTab = $state<
    "overview" | "roles" | "members" | "emoji" | "bans" | "federation" | "recovery" | "danger"
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
  const toggleNewRoleCap = (c: string) =>
    (newRoleCaps = newRoleCaps.includes(c) ? newRoleCaps.filter((x) => x !== c) : [...newRoleCaps, c]);
  const nsRoleScope = () => (activeServer ? `ns:${activeServer}` : "*");
  function createRole() {
    if (!newRoleName.trim() || !newRoleCaps.length) return;
    createRoleAt(nsRoleScope(), newRoleName.trim(), newRoleColor, newRoleCaps.join(","))
      .then(() => {
        newRoleName = "";
        newRoleCaps = [];
      })
      .catch((e) => toast(String(e), "error"));
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
  let stickBottom = $state(true); // is the view pinned to the newest message?
  let loadingInitial = false; // this in-flight load is the first page
  let historyBuf: Msg[] = []; // batch messages, buffered until BATCH END
  let preScrollHeight = 0; // scrollHeight before a scroll-up prepend

  const oldestMsgid = (ch?: Channel) => ch?.messages.find((m) => m.msgid)?.msgid;

  function loadHistory(target: string, initial: boolean) {
    // Channels (`#`) and DMs (`@`) both backfill; one load at a time.
    if (loadingHistory || !(target.startsWith("#") || target.startsWith("@"))) return;
    loadingHistory = target;
    loadingInitial = initial;
    historyBuf = [];
    const before = initial ? undefined : oldestMsgid(channels[target]);
    if (!initial) preScrollHeight = scrollEl?.scrollHeight ?? 0;
    weft.history(target, before).catch(() => (loadingHistory = null));
  }

  function onScroll() {
    if (!scrollEl) return;
    stickBottom = scrollEl.scrollHeight - scrollEl.scrollTop - scrollEl.clientHeight < 60;
    // Near the top with more upstream → page older.
    if (scrollEl.scrollTop < 80 && activeChannel?.hasMore) loadHistory(active, false);
  }

  let activeChannel = $derived(active ? channels[active] : undefined);
  let activeIsDm = $derived(active.startsWith("@"));
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
  // Discord-style grouping for the *active server*: by CHANNEL-LAYOUT category
  // (position-ordered), uncategorized under "Channels".
  let channelGroups = $derived.by(() => {
    const groups = new Map<string, Channel[]>();
    // Empty categories the admin created (client-side) show up too.
    for (const cat of discovered[activeServer]?.categories ?? layoutCache[activeServer]?.cats ?? [])
      groups.set(cat, []);
    for (const c of Object.values(channels)) {
      if (!c.name.startsWith("#") || nsOf(c.name) !== activeServer) continue;
      const cat = c.category || "Channels";
      if (!groups.has(cat)) groups.set(cat, []);
      groups.get(cat)!.push(c);
    }
    for (const list of groups.values())
      list.sort((a, b) => (a.position ?? 0) - (b.position ?? 0) || a.name.localeCompare(b.name));
    return [...groups.entries()].map(([category, list]) => ({ category, list }));
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
  let dmList = $derived(Object.values(channels).filter((c) => c.name.startsWith("@")));

  // ---- DM + presence helpers ----
  const peerOf = (key: string) => key.replace(/^@/, "");
  const dotClass = (acct: string) => `dot ${presence[acct] ?? "offline"}`;

  // ---- §10.3 profile helpers ----
  /** A fetchable avatar URL for an account, or null → render initials. */
  const avatarUrl = (acct: string): string | null => {
    const a = profiles[peerOf(acct)]?.avatar;
    return a ? weft.avatarUrl(a) : null;
  };
  /** An account's display name, falling back to the bare account part (§10.3:
   *  the canonical handle is always shown separately). */
  const displayName = (acct: string): string => {
    const key = peerOf(acct);
    return profiles[key]?.display || key.split("@")[0];
  };
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
    homeView = true;
    active = key;
  }
  function startDm() {
    const p = dmInput.trim().replace(/^@/, "");
    dmInput = "";
    if (p) openDm(p);
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
            if (!active) active = e.channel;
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
          else historyBuf.push(msg);
          break;
        }
        const ch = ensureChannel(key);
        // Dedupe: history backfill may re-deliver a live message.
        if (e.msgid && ch.messages.some((m) => m.msgid === e.msgid)) break;
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
        const pinged = !e.own && mentionsMe(e.body);
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
        };
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
        roleBuf.push({ name: e.name, color: e.color, caps: e.caps ? e.caps.split(",") : [] });
        break;
      case "role-member":
        memberRoles[`${e.account}|${e.scope}`] = e.roles ? e.roles.split(",") : [];
        confirmSuccess(`roles:${e.account}|${e.scope}`);
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
          // A revoke echo (INVITED … max-uses=0) — close, don't reopen.
          if (inviteId === e.invite_id) {
            inviteLink = null;
            inviteId = null;
          }
        } else {
          inviteLink = e.link ?? e.invite_id;
          inviteId = e.invite_id;
        }
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
        if (currentBatchId.startsWith("r")) {
          const scope = roleFetchQueue.shift();
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
        const target = loadingHistory;
        if (!target) break;
        const ch = ensureChannel(target);
        const seen = new Set(ch.messages.map((m) => m.msgid).filter(Boolean));
        const older = historyBuf.filter((m) => !m.msgid || !seen.has(m.msgid));
        ch.messages = [...older, ...ch.messages];
        ch.historyLoaded = true;
        ch.truncated = e.truncated;
        ch.hasMore = !e.truncated && historyBuf.length >= HISTORY_LIMIT;
        const initial = loadingInitial;
        const prev = preScrollHeight;
        historyBuf = [];
        loadingHistory = null;
        // Restore scroll after the DOM re-renders: bottom on first load, or
        // keep the reader's position when paging older.
        queueMicrotask(() => {
          if (!scrollEl) return;
          if (initial) scrollEl.scrollTop = scrollEl.scrollHeight;
          else scrollEl.scrollTop += scrollEl.scrollHeight - prev;
        });
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
  let pendingAttachments = $state<{ uri: string; name: string; mime: string; thumb: string | null }[]>([]);

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
          { uri: up.media, name: file.name || "pasted-file", mime: file.type, thumb: up.thumb },
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
    const attachments = pendingAttachments.map((a) => a.uri);
    // Clear only once the send is accepted, so a failure surfaces instead of
    // silently eating the message (e.g. an over-long body).
    weft
      .sendMessage(active, text, replyTo?.msgid, attachments)
      .then(() => {
        replyTo = null;
        stopTyping();
        composer = "";
        pendingAttachments = [];
      })
      .catch((e) => toast(String(e), "error"));
  }

  function composerKey(e: KeyboardEvent) {
    // Mention autocomplete captures Enter/Tab/Escape while open.
    if (mentionQuery !== null && mentionMatches.length) {
      if (e.key === "Enter" || e.key === "Tab") {
        e.preventDefault();
        pickMention(mentionMatches[0]);
        return;
      } else if (e.key === "Escape") {
        e.preventDefault();
        mentionQuery = null;
        return;
      }
    }
    // :emoji: autocomplete captures the same keys while open.
    if (emojiQuery !== null && emojiSuggestions.length) {
      if (e.key === "Enter" || e.key === "Tab") {
        e.preventDefault();
        pickEmojiSuggestion(emojiSuggestions[0].name);
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

  // Search the batch buffer first (target may not be committed yet), then the
  // channel's messages.
  function findMsg(target: string, msgid: string): Msg | undefined {
    return (
      historyBuf.find((m) => m.msgid === msgid) ??
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
  // markdown token is turned back into a tag.
  const escapeHtml = (s: string) =>
    s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  // Inline formatting for a run of text with no fenced blocks.
  function renderInline(text: string): string {
    let s = escapeHtml(text);
    s = s.replace(/`([^`]+)`/g, (_, c) => `<code>${c}</code>`);
    s = s.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
    s = s.replace(/__([^_]+)__/g, "<strong>$1</strong>");
    s = s.replace(/(^|[^*])\*([^*]+)\*/g, "$1<em>$2</em>");
    s = s.replace(/~~([^~]+)~~/g, "<del>$1</del>");
    s = s.replace(
      /\[([^\]]+)\]\((https?:\/\/[^\s)]+)\)/g,
      '<a href="$2" target="_blank" rel="noopener noreferrer">$1</a>',
    );
    s = s.replace(
      /(^|\s)(https?:\/\/[^\s<]+)/g,
      '$1<a href="$2" target="_blank" rel="noopener noreferrer">$2</a>',
    );
    // ||spoiler|| → click-to-reveal (revealed by a delegated handler in the list).
    s = s.replace(
      /\|\|([\s\S]+?)\|\|/g,
      '<span class="spoiler" role="button" tabindex="0" title="Spoiler — click to reveal">$1</span>',
    );
    // @mentions → pills; a mention of me / @everyone / @here highlights.
    s = s.replace(/@(everyone|here|[a-z0-9][a-z0-9._-]*)/gi, (_full, name: string) => {
      const me = name === account || name === "everyone" || name === "here";
      return `<span class="mention${me ? " me" : ""}">@${name}</span>`;
    });
    // §9.4 :name: custom emoji (active namespace) → an inline image; unknown
    // shortcodes are left as literal text.
    s = s.replace(/:([a-zA-Z0-9_]+):/g, (full, name: string) => {
      const media = customEmoji[activeServer]?.[name];
      if (!media) return full;
      const url = weft.mediaUrl(media).replace(/&/g, "&amp;").replace(/"/g, "&quot;");
      return `<img class="custom-emoji" src="${url}" alt=":${name}:" title=":${name}:" />`;
    });
    return s;
  }
  // Full render: lift out ``` / ~~~ fenced code blocks (their contents are
  // rendered verbatim), inline-format the rest, then splice the blocks back in.
  function renderMd(text: string): string {
    const blocks: { lang: string; code: string }[] = [];
    const lifted = text.replace(
      /(?:```|~~~)([a-zA-Z0-9+#.-]*)\n?([\s\S]*?)(?:```|~~~)/g,
      (_m, lang: string, code: string) => {
        const i = blocks.length;
        blocks.push({ lang: lang.trim(), code: code.replace(/\n$/, "") });
        return ` CB${i} `;
      },
    );

    let s = renderInline(lifted);

    s = s.replace(/ CB(\d+) /g, (_m, i: string) => {
      const b = blocks[+i];
      const label = b.lang ? `<span class="code-lang">${escapeHtml(b.lang)}</span>` : "";
      return `<pre class="code-block">${label}<code>${escapeHtml(b.code)}</code></pre>`;
    });
    return s;
  }
  // Does a body mention the current account (or everyone/here)?
  const mentionsMe = (body: string) =>
    !!account && (new RegExp(`@${account}\\b`, "i").test(body) || /@(everyone|here)\b/i.test(body));

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
    const key = `${channel} ${user}`;
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
  let mentionMatches = $derived.by(() => {
    if (mentionQuery === null) return [];
    const q = mentionQuery.toLowerCase();
    const names: string[] = [];
    if ("everyone".startsWith(q)) names.push("everyone");
    if ("here".startsWith(q)) names.push("here");
    for (const m of activeChannel?.members ?? [])
      if (m.name !== account && m.name.toLowerCase().startsWith(q)) names.push(m.name);
    return names.slice(0, 8);
  });
  function updateMention() {
    const m = composer.match(/@([a-z0-9._-]*)$/i);
    mentionQuery = m ? m[1] : null;
  }
  function pickMention(name: string) {
    composer = composer.replace(/@[a-z0-9._-]*$/i, `@${name} `);
    mentionQuery = null;
  }

  // ---- :emoji: autocomplete (custom emoji only — unicode has no names) ----
  let emojiQuery = $state<string | null>(null);
  const emojiSuggestions = $derived.by(() => {
    if (emojiQuery === null) return [];
    const q = emojiQuery.toLowerCase();
    const rank = (n: string) => (n.toLowerCase().startsWith(q) ? 0 : 1);
    return activeEmoji
      .filter((e) => e.name.toLowerCase().includes(q))
      .sort((a, b) => rank(a.name) - rank(b.name) || a.name.localeCompare(b.name))
      .slice(0, 8)
      .map((e) => ({ name: e.name, url: emojiUrlFor(e.name) }));
  });
  function updateEmojiSuggest() {
    // A `:word` at a token boundary — not `http://`, not `12:30`.
    const m = composer.match(/(?:^|\s):([a-zA-Z0-9_]+)$/);
    emojiQuery = m ? m[1] : null;
  }
  function pickEmojiSuggestion(name: string) {
    composer = composer.replace(/:[a-zA-Z0-9_]+$/, `:${name}: `);
    emojiQuery = null;
  }
  function stopTyping() {
    clearTimeout(typingStop);
    if (typingChannel) {
      weft.typing(typingChannel, false).catch(() => {});
      typingChannel = null;
    }
  }

  // Keep the newest message in view only while pinned to the bottom — a
  // history prepend (reader scrolled up) must not yank them down.
  $effect(() => {
    activeChannel?.messages.length;
    if (scrollEl && stickBottom) {
      queueMicrotask(() => (scrollEl!.scrollTop = scrollEl!.scrollHeight));
    }
  });

  // On opening a channel, pin to bottom and backfill its first page once.
  $effect(() => {
    const a = active;
    if (!a) return;
    stickBottom = true;
    const ch = channels[a];
    if (ch && !ch.historyLoaded) loadHistory(a, true);
    // Fetch the full roster once (MEMBERS folds in as MEMBER-join rows). The
    // guard stops the self-row in the snapshot from re-triggering us.
    if (ch && a.startsWith("#") && !ch.rosterLoaded) {
      ch.rosterLoaded = true;
      weft.members(a).catch(() => {});
    }
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
    if (!ch) return;
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


  // Invites
  function mintInvite() {
    weft.inviteMint(scopesFor()[0]).catch(() => {});
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
  function openCreateChannelInCat(cat: string) {
    newChanName = "";
    newChanCategory = cat === "Channels" ? "" : cat;
    newChanAnnounce = false;
    newChanRet = "";
    newChanVoice = false;
    newChanOpen = true;
  }
  function deleteCategory(cat: string) {
    // Move its channels back to the default group, then drop the category.
    for (const c of Object.values(channels)) {
      if (c.name.startsWith("#") && nsOf(c.name) === activeServer && (c.category || "Channels") === cat) {
        c.category = undefined;
        weft.channelMeta(c.name, "category", "").catch(() => {});
      }
    }
    setCategories(nsCategories().filter((x) => x !== cat));
  }
  function catCtx(e: MouseEvent, cat: string) {
    if (cat === "Channels") return; // the default group isn't deletable
    openCtx(e, [
      { label: "Create channel here", run: () => openCreateChannelInCat(cat) },
      { label: "Delete category", danger: true, run: () => deleteCategory(cat) },
    ]);
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
    // The default "Channels" group is uncategorized (empty category).
    const storedCat = targetCat === "Channels" ? "" : targetCat;
    dragged.category = storedCat || undefined; // optimistic
    weft.channelMeta(dragName, "category", storedCat).catch((e) => toast(String(e), "error"));
    // Renumber the target category so positions are stable + ordered.
    const list = Object.values(channels)
      .filter(
        (c) =>
          c.name.startsWith("#") &&
          nsOf(c.name) === activeServer &&
          (c.category || "Channels") === targetCat &&
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
  // Close the thread panel when the active channel changes.
  let threadChannel = "";
  $effect(() => {
    if (active !== threadChannel) {
      threadChannel = active;
      closeThread();
    }
  });

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
  function federate(target: string) {
    const t = target.trim();
    const slash = t.indexOf("/");
    if (slash < 1) {
      toast("Enter a foreign namespace as network/namespace", "error");
      return;
    }
    const ns = t.slice(slash + 1);
    weft
      .federate(t)
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
    get serverNamespaces() { return serverNamespaces; },
    get channelGroups() { return channelGroups; },
    get dmList() { return dmList; },
    get activeNsMeta() { return activeNsMeta; },
    goHome: () => (homeView = true),
    selectServer,
    openServerMenu,
    open: (name: string) => { active = name; markRead(name); },
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
    memberCtx,
    catCtx,
    get serverMenu() { return serverMenu; },
    set serverMenu(v: boolean) { serverMenu = v; },
    get userMenu() { return userMenu; },
    set userMenu(v: boolean) { userMenu = v; },
    openCreateChannel,
    openCreateChannelInCat,
    openNsSettings,
    mintInvite,
    newCat: () => { newCatName = ""; newCatOpen = true; serverMenu = false; },
    openProfile,
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
    threadCount,
    openThread,
    closeThread,
    sendThread,
    // custom emoji (§9.4)
    get activeEmoji() { return activeEmoji; },
    addEmoji,
    removeEmoji,
    emojiUrlFor,
    // message list / items
    get loadingHistory() { return loadingHistory; },
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
    pickEmojiSuggestion,
    get pendingAttachments() { return pendingAttachments; },
    attachFile,
    pasteFiles,
    dropFiles,
    removeAttachment,
    mediaUrl: weft.mediaUrl,
    get mentionQuery() { return mentionQuery; },
    get mentionMatches() { return mentionMatches; },
    get typingLabel() { return typingLabel; },
    // roles (ProfileCard)
    get rolesByScope() { return rolesByScope; },
    rolesOf,
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
    set nsTab(v: "overview" | "roles" | "members" | "emoji" | "bans" | "federation" | "recovery" | "danger") { nsTab = v; },
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
    deleteRole,
    assignRole,
    showRecoveryKey,
    startRecovery,
    cosignRecovery,
    submitRecovery,
    doTransfer,
    deleteNamespace,
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
  <div class="app" class:members-collapsed={!membersVisible}>
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
      <UserFooter />
    </aside>

    <!-- MAIN -->
    <main class="main">
      {#if !activeChannel && !homeView}
        <EmptyHome />
      {:else}
        <ChatTopbar />

        <MessageList bind:scrollEl onscroll={onScroll} />
        <Composer />
      {/if}
    </main>

    <!-- MEMBERS -->
    <aside class="members">
      {#if activeChannel && !activeIsDm}
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

    {#if inviteLink}
      <InviteLinkModal link={inviteLink} id={inviteId} onclose={() => (inviteLink = null)} />
    {/if}

    {#if pinsOpen}
      <PinsModal onclose={() => (pinsOpen = false)} />
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

    {#if settingsOpen}
      <UserSettingsModal onclose={() => (settingsOpen = false)} />
    {/if}

    {#if federationOpen}
      <FederationPanel onclose={() => (federationOpen = false)} />
    {/if}

    {#if nsSettingsOpen}
      <ServerSettingsModal onclose={() => (nsSettingsOpen = false)} />
    {/if}

    {#if notifSettingsOpen}
      <NotificationSettingsModal onclose={() => (notifSettingsOpen = false)} />
    {/if}
  </div>
{/if}
