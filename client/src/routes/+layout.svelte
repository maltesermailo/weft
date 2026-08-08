<script lang="ts">
  import type { NsTab } from "$lib/ui/ui.svelte";
  import "../app.css";
  import { onMount, untrack } from "svelte";
  import { goto } from "$app/navigation";
  import { page } from "$app/state";
  import * as nav from "$lib/navigation/nav";
  import { ui } from "$lib/ui/ui.svelte";
  import { conn, attemptReconnect, HOMESERVER_KEY, SAVED_KEY, nsMetaFetched, logout, doConnect, keyLogin, chooseServer, changeServer, probeServer } from "$lib/connection/connection.svelte";
  import { selectServer, goHome } from "$lib/navigation/navigation";
  import { handle, loadHistory, hist } from "$lib/sync/reducer.svelte";
  import { trackBackground } from "$lib/sync/joinErrors";
  import { msgEpoch } from "$lib/rendering/time";
  import { scopeKeyOf, notifLevel, isMuted, setNotifLevel } from "$lib/notifications/notif";
  import { peerOf, initials, dotClass, avatarUrl, bioOf, statusOf, profileStore } from "$lib/profile/profile.svelte";
  import { toasts, toast, expectSuccess } from "$lib/notifications/toasts.svelte";
  import * as weft from "$lib/transport/weft";
  
  import * as media from "$lib/media/media";
  import type { Msg } from "$lib/types";
  
  import { cf } from "$lib/session/connect.svelte";
  import { provideApp } from "$lib/ui/context";
  import { store } from "$lib/store/store.svelte";
  
  import { moderate } from "$lib/moderation/moderation";
  import { nsAdmin, openNsSettingsFor, openNotifSettingsFor, openServerProfileFor } from "$lib/namespaces/server.svelte";
  
  
  
  
  import { closeThread } from "$lib/messages/threads.svelte";
  import { vm } from "$lib/navigation/viewmodel.svelte";
  
  import AppShell from "$lib/components/AppShell.svelte";
  
  import { appConfirm } from "$lib/ui/confirm.svelte";
  import { openCreateChannel, openCreateChannelInCat, openCreateCategory } from "$lib/channels/channelcreate.svelte";
  
import { roleScopeOf, roleStore } from "$lib/roles/roles.svelte";
  import { channelStore, Channel, nsOf } from "$lib/channels/channel.svelte";
import { mkMsg, catchUpChannel } from "$lib/messages/messages.svelte";
  import { installLinkGuard } from "$lib/ui/linkguard.svelte";
  import LinkWarningModal from "$lib/components/modals/LinkWarningModal.svelte";
  import ConnectScreen from "$lib/components/ConnectScreen.svelte";
  import Toasts from "$lib/components/Toasts.svelte";
  import ContextMenu from "$lib/components/ContextMenu.svelte";
  import QuickSwitcher from "$lib/components/QuickSwitcher.svelte";
  import CommunityRail from "$lib/components/CommunityRail.svelte";
  import MemberList from "$lib/components/MemberList.svelte";
  import { voice } from "$lib/voice/voice.svelte";
  import { callMedia, disconnectCallMedia, toggleCallMute } from "$lib/voice/callmedia.svelte";
  import VoiceBar from "$lib/components/VoiceBar.svelte";
  import CameraPicker from "$lib/components/modals/CameraPicker.svelte";
  import ScreenPicker from "$lib/components/modals/ScreenPicker.svelte";
  import ScreenShareMenu from "$lib/components/modals/ScreenShareMenu.svelte";
  
  import ChannelList from "$lib/components/sidebar/ChannelList.svelte";
  import SidebarHeader from "$lib/components/sidebar/SidebarHeader.svelte";
  import DmList from "$lib/components/sidebar/DmList.svelte";
  import UserFooter from "$lib/components/sidebar/UserFooter.svelte";
  import SidebarInput from "$lib/components/sidebar/SidebarInput.svelte";
  import Lightbox from "$lib/components/chat/Lightbox.svelte";
  import ThreadPanel from "$lib/components/chat/ThreadPanel.svelte";

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
  // ---- right-click context menus ----
  // Can I moderate-delete another member's message in the active channel?
  // `delete-any` at the channel or its namespace. Kicks off a fetch of my own
  // caps so the answer resolves on a subsequent open.
  //
  // NOTE: operator (`*`) status is deliberately NOT consulted for namespaced
  // channels — mirrors the server (context.rs): a network operator's god-mode is
  // web-admin authority, never day-to-day power on someone else's server. At the
  // network level (top-level channels) `nsScope` *is* `*`, so operator power
  // still applies there naturally.
  // The right-click menu for any user, anywhere (member list, friends, DMs).
  // Items adapt to context: a DM shows Close DM (else Message), a channel adds
  // Invite + moderation (only there is the user a server member you can act on),
  // and a friend shows Remove friend.
  // The right-click menu for a group DM (in the DM list).

  // In-app confirmation (the Tauri webview blocks native window.confirm, so
  // destructive actions must not rely on it). Resolves true/false.

  // ---- live data, channel collection: `$lib/channels/channel.svelte`
  // (channels/mkMsg/channelStore.ensure/channelStore.markRead/nsOf/channelStore.short/…). ----

  // ---- notification preferences (per-user, localStorage) ----
  // Set per **namespace** (`ns:<name>`, or `net` for top-level) in the
  // Notification-pref resolvers (scopeKeyOf / notifLevel / isMuted / serverMuted /
  // notifLevelOf / setNotifLevel) → `$lib/notif`.
  // ---- notification-settings modal (per-namespace) ----
  // These three live in `namespaces/server.svelte` so the rail's context menu can
  // call them too; the AppCtx just forwards. Omitting the target means "the active
  // namespace", which is the sidebar-header case.
  const openNotifSettings = openNotifSettingsFor;
  /// §10.5 open the user settings on the verification tab (from the no-email nudge).
  // ---- navigation: derived from the URL (path-based routes, see lib/nav.ts) ----
  // The single source of truth for "what's open" is the route. `active` is the
  // sigil-tagged key (`#ns/chan` | `@peer` | `&group` | ""), `activeServer` the
  // selected namespace, `homeView` whether the sidebar shows DMs. Navigation is
  // `goto(nav.pathFor(...))`; nothing assigns these directly.
  const view = $derived(nav.viewFrom(page.route?.id, page.params));
  const active = $derived(view.active);
  const activeServer = $derived(view.activeServer); // "" = network top-level / home; else a namespace
  const homeView = $derived(view.homeView);
  // ---- servers/namespaces as rail tiles (Phase 6, flavor A) ----
  // `nsOf` / `channelStore.short` are channel-name helpers imported from the channel model.
  // A user-facing label for any target: `#vanity` for a channel, the peer's
  // display name for a DM, the group label for a group DM.
  // ---- DMs + presence (Phase 5) ----
  // The shared client store (singleton) — the identity maps, namespaces, and
  // client prefs. Domain models navigate to it too (see client-model-refactor.md).
  // §10.3 profileStore.nicks cache + profile/identity helpers → `$lib/profile.svelte`.
  // Pull a server's nicknames once, the first time it's viewed.
  $effect(() => {
    const s = activeServer;
    if (s && !profileStore.nicksFetched.has(s)) {
      profileStore.nicksFetched.add(s);
      weft.nicksQuery(`ns:${s}`).catch(() => {});
    }
  });
  // Set a per-namespace nickname (empty clears it). `NICK` verb (§10.3).
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
  // Footer user menu (presence + settings + logout) and the user-settings page tab.
  // ---- social layer (friends / groups / calls) ----
  // State lives on `store.social` (userrefs are `account@network`, resolved via
  // the Account map at the UI edge). Only the add-friend input box stays local.
  let addFriendInput = $state("");
  // ---- discover dialog (Phase 6) ----
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
  // ---- §6.5 invites (list menu + create screen) — state on `store.invites`.
  // ---- federation (§11, operator) — state lives on `store.federation` ----
  // Opens the operator §11 Federation panel (UI orchestration stays here; the
  // panel's actions live on `$lib/federation/federation.svelte`).
  function openFederation() {
    ui.federationOpen = true;
    ui.settingsOpen = false;
    store.federation.refreshNetblocks();
  }
  // ---- pins + message search (§6.4) — state on `store.pins` / `store.search`
  // (self-contained panels); results stream in as BATCHes, routed by the reducer.
  // ---- threads (§9.4) — side panel + list modal — state on `store.threads`.
  // ---- capability + role reads: `$lib/session/session.svelte` (ensureCapsAt /
  // rolesAt / roleById / rolesOf / store.session.isOwnerAt / store.session.isStaff / badgeFor / mentionsMe /
  // roleScopeOf); `rolesByScope` / `memberRoles` state live there too. ----

  // ---- §6.5 named roles: the fetch/batch machinery (queues + roleStore.fetchRoles) ----
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
  // Servers (namespaces) I share with `target`, derived from the memberships I
  // can already see — a channel of that namespace listing them as a member.
  function mutualServers(target: string): string[] {
    return vm.serverNamespaces.filter((ns) =>
      Object.values(channelStore.channels).some(
        (c) => c.name.startsWith("#") && nsOf(c.name) === ns && c.members?.some((m) => m.name === target),
      ),
    );
  }
  // Friend helpers for the profile modal: normalize a (possibly bare) handle to
  // the `account@network` friend key, then read state / act on it.
  // ---- namespace admin panel (§6.2 / §2.4 / §6.6) ----
  // §10.3 per-server profile editor (your own nickname on this server).
  const openServerProfile = openServerProfileFor;
  // §6.7 moderation deny-list (mutes + bans) per scope, for the Bans tab —
  // lives on `store.deny`.
  function assignRole(roleId: string) {
    const who = nsDelegSubject.trim();
    if (!who) {
      toast("Enter an account first", "error");
      return;
    }
    // Confirmed by the ROLE-MEMBER event; a cap failure never confirms.
    expectSuccess(`roles:${who}|${roleStore.nsRoleScope()}`, `Roles updated for ${who}`);
    weft.roleAssign(roleStore.nsRoleScope(), who, roleId).catch((e) => toast(String(e), "error"));
  }

  // Right-click a member row in the directory → namespace-scoped moderation.
  // Mute/ban (and their lifts) key on `ns:<server>` in the deny-list; kick is
  // channel-scoped and so has no place on a server-wide roster.
  let nsDelegSubject = $state("");
  // A legacy-shaped view of the active Server's metadata (snake_case field names
  // the modals/banners already read). Undefined until NS-META has landed.
  const retentionMeta: Record<string, { label: string; cls: string; icon: string }> = {
    ephemeral: { label: "Ephemeral", cls: "ephemeral", icon: '<circle cx="12" cy="12" r="8" stroke-dasharray="3 3"/>' },
    retained: { label: "Retained", cls: "retained", icon: '<rect x="4" y="4" width="16" height="16" rx="2"/><path d="M4 10h16"/>' },
    permanent: { label: "Permanent", cls: "permanent", icon: '<rect x="4" y="4" width="16" height="16" rx="2" fill="currentColor" stroke="none"/>' },
    e2ee: { label: "E2EE · MLS", cls: "e2ee", icon: '<rect x="5" y="11" width="14" height="9" rx="2"/><path d="M8 11V7a4 4 0 0 1 8 0v4"/>' },
  };

  // Time / ULID-timestamp / day-label / retention helpers → `$lib/time`.

  // ---- history / scrollback (Phase 1) ----
  // History pages buffered per *target channel*, keyed by the messages' own
  // `target`. This is what makes history robust: a page flushes to the channel it
  // names, so a concurrent MEMBERS/roles/… batch can never steal or clobber it,
  // whatever its batch id or arrival order.

  // Fetch a channel's history page. Single-flight (`hist.loading` guard);
  // MessageList calls this on first open (initial) and on scroll-to-top (paging).

  // Namespaces we hold channels in OR are a member of (the latter keeps a
  // channel-less server on the rail) — each becomes a rail tile (flavor A).
  // The rail = every namespace I belong to: one I hold a channel in, or one I'm
  // a recorded member of. `Server.joined` is the join barrier — populated by SYNC
  // and live NS-MEMBER, and (below) by owning a namespace — so a channel-less server
  // (e.g. one I just created) still shows.
  // Proactively load NS-META (title/vanity + layout) for every server on the
  // rail we haven't seen it for — so tiles show the right name/initials without
  // waiting for a click. `channels(id)` replies with NS-META + CHANNEL-LAYOUTs.
  $effect(() => {
    for (const ns of vm.serverNamespaces) {
      if (!store.servers.get(ns)?.metaLoaded && !nsMetaFetched.has(ns)) {
        nsMetaFetched.add(ns);
        weft.channels(ns).catch(() => {});
      }
    }
  });
  // Server-tile unread/mention rollups (so unread in other servers is visible),
  // folded over the server's own channels.
  // Discord-style grouping for the *active server*: uncategorized channels sit
  // bare at the top (category "", no header), then each CHANNEL-LAYOUT category
  // (position-ordered) in its persisted order.

  // Right-click a rail tile: select the server and open its header menu (the
  // same Create Invite / Notification / Server Settings menu as clicking the name).
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

  // §9 liveness: the namespace you are *in* loses its bridge. It cannot serve
  // history, take a message, or answer a roster from here on, so sitting in it
  // shows a room that silently does nothing. Leave for DMs, which always work.
  //
  // Only `false` — `null` means native (nothing governs it, never offline), and
  // waiting for a value would bounce every namespace on connect.
  $effect(() => {
    const s = activeServer;
    if (s && store.servers.get(s)?.providerOnline === false) {
      toast(`${vm.serverName(s)} is unavailable — its bridge is disconnected`, "info");
      goHome();
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

  // DM conversations (keyed `@peer`), plus any peer we've opened a blank DM with.

  // ---- DM + presence + §10.3 profile helpers (peerOf / dotClass / avatarUrl /
  // profileStore.displayName / profileStore.nickOf / bioOf / statusOf / initials) → `$lib/profile.svelte`.
  /** Set (or clear, with "") my own custom status. */
  // The set of open 1:1 DMs is view state the server doesn't yet track (a
  // server-owned DM list is §18 territory), so we persist it per account so a
  // conversation — and its history on click — survives a reconnect / relaunch.
  // v0.12 SYNC cursor, per account+device (localStorage). Stored on every
  // `sync-end`, replayed on reconnect so `SYNC since=` catches up missed
  // messages + offline edits/reactions in one round trip.

  // "Invite to server" — open the invites panel for the current server, where a
  // shareable link is minted (invites are link-based, §6.5).
  function inviteToServer() {
    store.invites.openInvites();
  }

  // ---- social layer: friends ----
  // A friend's short label: bare handle for local, full ref for federated.
  // A friend's local account handle (for DM/profile/presence), if local.
  function addFriend() {
    const user = store.social.qualify(addFriendInput);
    if (!user || !user.includes("@")) return;
    addFriendInput = "";
    weft.friendAdd(user).catch((e) => toast(String(e), "error"));
  }
  // Show the Friends home screen (home view, no DM selected).
  // ---- group DMs ----
  let newGroupInput = $state("");
  // A group's display label: its name, else the member handles (minus self).
  function createGroup() {
    const members = newGroupInput
      .split(/[,\s]+/)
      .map((h) => store.social.qualify(h))
      .filter((h) => h.includes("@"));
    if (!members.length) return;
    newGroupInput = "";
    weft.groupCreate(members).catch((e) => toast(String(e), "error"));
  }
  function addToGroup(id: string, handle: string) {
    const user = store.social.qualify(handle);
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

  /// A capability-gated moderation action (§10.4). These are **server-side**:
  /// the client sends the wire intent and weftd enforces it (BAN/KICK/MUTE are
  /// wired here frontend-first; the weftd verbs land later). Shared by the
  /// slash commands and the member-row buttons.
  // §6.7 moderation. `scope` defaults to the active channel; ban/mute also
  // accept `ns:<name>` or `*` (network). Confirmation arrives as a MODERATED
  // event; a missing-cap failure surfaces as an ERR.

  // On opening a text channel, fetch its roster once (MEMBERS folds in as
  // MEMBER-join rows). History + scroll positioning are owned by the channel's
  // own <MessageList>. `active` is the only tracked dependency; the record is
  // read/written untracked so this can't self-trigger.
  $effect(() => {
    const a = active;
    if (!a.startsWith("#")) return;
    untrack(() => {
      const ch = channelStore.channels[a];
      // Voice channels aren't in the server's runtime `joined` set, so a MEMBERS
      // fetch answers CAP-REQUIRED ("join the channel first"); their roster comes
      // from voice-state instead. Skip them.
      //
      // Labelled as background: a text channel can answer CAP-REQUIRED too, when
      // our belief that we're joined is stale (a bridged namespace whose provider
      // never asserted our membership). That's ours to swallow — the user only
      // opened a channel.
      if (ch && !ch.voice && !ch.rosterLoaded) {
        ch.rosterLoaded = true;
        weft.members(a, trackBackground()).catch(() => {});
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
      if (!channelStore.channels[a]) {
        channelStore.ensure(a);
        if (a.startsWith("@")) channelStore.persistDms();
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
      const lr = channelStore.channels[a]?.lastRead;
      return lr ? msgEpoch(lr) : null;
    });
  });
  // The render key of the message the NEW divider sits before, or null.
  const newDividerKey = $derived.by(() => {
    if (newBoundary === null) return null;
    for (const m of vm.activeChannel?.messages ?? []) {
      if (m.system || m.own) continue;
      if (m.ts > newBoundary) return m.key;
    }
    return null;
  });

  // Viewing a channel clears its unread badge and advances the read marker
  // (MARK, synced across our devices — §9.7).
  $effect(() => {
    const ch = vm.activeChannel;
    if (!ch || ch.voice) return;
    channelStore.markRead(ch.name);
    if (!ch.name.startsWith("#")) return;
    let newest: string | undefined;
    for (let i = ch.messages.length - 1; i >= 0; i--)
      if (ch.messages[i].msgid) {
        newest = ch.messages[i].msgid;
        break;
      }
    if (newest && newest !== ch.lastRead) {
      ch.lastRead = newest;
      // Background-labelled: marking read is automatic, so an unjoined
      // channel's CAP-REQUIRED must not surface as a toast.
      weft.mark(ch.name, newest, trackBackground()).catch(() => {});
    }
  });

  // client-core M4-scope: the messages store pushes live body diffs only for the
  // OPEN channel — scope its subscription to the active view, and pull that
  // channel's window on open so any messages / edits / reactions that landed while
  // it was in the background catch up (the store buffered them; scoping suppressed
  // their live diffs). Home / voice views subscribe to nothing.
  $effect(() => {
    weft.setOpenChannels(active ? [active] : []).catch(() => {});
    if (active.startsWith("#") || active.startsWith("@") || active.startsWith("&")) {
      void catchUpChannel(active);
    }
  });

  // ---- discover + channel management (Phase 6) ----
  // Reporting (ReportModal owns its form + submit)
  function openReports() {
    store.reports.open = true;
    // The queue is model-owned (client-core): clear it via the model, then
    // re-fetch — the REPORTS LIST batch (REPORT-FILED events) repopulates it.
    weft.reportsClear().catch(() => {});
    weft.reportsList(activeServer ? `ns:${activeServer}` : "*").catch(() => {});
  }

  // ---- server dropdown (Discord-style header menu) ----
  // Right-click the empty channel-list background (Discord-style) → create.
  // Pins (§6.4)
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
  // Close the thread panel when the active channel changes.
  let threadChannel = "";
  $effect(() => {
    if (active !== threadChannel) {
      threadChannel = active;
      closeThread();
      store.threads.listOpen = false;
    }
  });

  // Namespace admin
  function openNsSettings(target?: string) {
    nsDelegSubject = ""; // layout-local draft; the rest is the shared opener
    openNsSettingsFor(target);
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
    if (Object.keys(channelStore.channels).some((c) => nsOf(c) === f.ns)) {
      cancelFederating();
      selectServer(f.ns);
    }
  });
  async function doTransfer() {
    const o = nsAdmin.newOwner.trim();
    if (o && (await appConfirm(`Transfer ownership of ${vm.serverName(activeServer)} to ${o}? This is signed by your root key and cannot be undone.`, "Transfer")))
      weft.nsTransfer(network, activeServer, o).catch((e) => (cf.authError = String(e)));
  }
  async function deleteNamespace() {
    if (await appConfirm(`Delete namespace ${vm.serverName(activeServer)}? This removes all its channels.`, "Delete")) {
      weft.nsDelete(activeServer).catch((e) => toast(String(e), "error"));
      ui.nsSettingsOpen = false;
    }
  }

  // Revoke every outstanding invite for the active namespace (ns-admin, §6.5).
  async function revokeAllInvites() {
    if (!activeServer) return;
    if (!(await appConfirm(`Revoke ALL invites for ${vm.serverName(activeServer)}? Every existing invite link stops working.`, "Revoke all"))) return;
    weft.inviteRevokeAll(`ns:${activeServer}`).catch(() => {});
    store.invites.list = []; // optimistic — the list is now empty
    toast(`Revoked all invites for ${vm.serverName(activeServer)}`, "info");
  }

  onMount(() => {
    // (Channel layout + category lists restore in the client-core model, seeded on
    // connect from the `weft:chan-layout` blob — no TS layout cache to load here.)
    // Restore theme.
    try {
      if (localStorage.getItem("weft:theme") === "light") {
        ui.theme = "light";
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
    get homeView() { return homeView; },
    get activeServer() { return activeServer; },
    get active() { return active; },
    // social layer: friends
    get addFriendInput() { return addFriendInput; },
    set addFriendInput(v: string) { addFriendInput = v; },
    addFriend,
    acceptFriend: (u: string) => store.social.acceptFriend(u),
    removeFriend: (u: string) => store.social.removeFriend(u),
    // group DMs
    get newGroupInput() { return newGroupInput; },
    set newGroupInput(v: string) { newGroupInput = v; },
    createGroup,
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
    acceptCall,
    declineCall,
    endCall,
    toggleCallMute,
    goHome,
    selectServer,
    openNotifSettings,
    markRead: (n: string) => channelStore.markRead(n),
    chanShort: (n: string) => channelStore.short(n),
    nsOf,
    retentionMeta,
    openCreateChannel,
    openCreateChannelInCat,
    openNsSettings,
    openServerProfile,
    // invites menu (Discord-style) — state on `store.invites`
    // invite creation screen
    newCat: openCreateCategory,
    mutualServers,
    openSettings: () => { ui.userTab = "account"; ui.settingsOpen = true; ui.userMenu = false; },
    toast,
    confirm: appConfirm,
    expectSuccess,
    // chat topbar
    openPins,
    openReports,
    partActive: () => weft.part(active).catch(() => {}),
    // search + pins panels own their state on `store.search` / `store.pins`.
    openSearch,
    // message list / items
    get loadingHistory() { return hist.loading; },
    get newBoundary() { return newBoundary; },
    channelRecord: (n: string) => channelStore.get(n),
    loadHistory,
    get replyTo() { return ui.replyTo; },
    set replyTo(v: Msg | null) { ui.replyTo = v; },
    get newDividerKey() { return newDividerKey; },
    mediaUrl: media.mediaUrl,
    // roles (ProfileCard)
    // channel permissions (per-target: @everyone / role / member)
    // federation (operator)
    openFederation,
    // user settings
    // user settings (page overlay)
    // server settings (ns overlay)
    get nsTab() { return ui.nsTab; },
    set nsTab(v: NsTab) { ui.nsTab = v; },
    get nsDelegSubject() { return nsDelegSubject; },
    set nsDelegSubject(v: string) { nsDelegSubject = v; },
    federate,
    assignRole,
    doTransfer,
    deleteNamespace,
    revokeAllInvites,
  });

  // SvelteKit renders the active route (the main-area view) as `children`.
  let { children }: { children: import("svelte").Snippet } = $props();
</script>

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
  <AppShell {children} {federating} oncancelfederating={cancelFederating} />
{/if}
