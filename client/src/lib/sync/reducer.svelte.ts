// The event reducer: `handle(e)` (wire event → store mutation) plus its
// request/batch machinery (fetch queues, history buffering) and the message/
// typing/channel-create helpers it owns. Imports the stores; never the reverse.
import { goto } from "$app/navigation";
import { page } from "$app/state";
import * as nav from "$lib/nav";
import * as weft from "$lib/weft";
import { EVERYONE_ROLE } from "$lib/constants";
import type { Msg } from "$lib/types";
import { store } from "$lib/models/store.svelte";
import { Channel, channels, mkMsg, ensureChannel, channelRecord, nsOf, chanShort, markRead, cacheChanLayout, cacheNsCats, persistDms, restoreDms } from "$lib/models/channel.svelte";
import { Membership } from "$lib/models/membership.svelte";
import { Role } from "$lib/models/role.svelte";
import { rolesByScope, memberRoles, ensureCapsAt, ensureCaps, capsResolved, rolesAt, roleById, rolesOf, roleScopeOf, mentionsMe, fedRolesFetched, fetchMemberRoles } from "$lib/models/session.svelte";
import { cf, emailNudgeKey } from "$lib/models/connect.svelte";
import { conn, attemptReconnect, HOMESERVER_KEY, SAVED_KEY } from "$lib/connection.svelte";
import { ui } from "$lib/ui.svelte";
import * as md from "$lib/markdown";
import { toast, confirmSuccess } from "$lib/toasts.svelte";
import { clock, msgEpoch, msgTime, retentionOf } from "$lib/time";
import { notifLevel, isMuted } from "$lib/notif";
import { queryProfile, friendLabel, nicks, nickKey } from "$lib/profile.svelte";
import { initVoice, joinVoice, voice } from "$lib/voice.svelte";
import { callMedia, connectCallMedia, disconnectCallMedia } from "$lib/callmedia.svelte";

// View state, mirrored from the URL exactly as the layout derives it (so the
// extracted handlers read `active`/`account`/… unchanged).
const view = $derived(nav.viewFrom(page.route?.id, page.params));
const account = $derived(store.session.account);
const network = $derived(store.session.network);
const active = $derived(view.active);
const activeServer = $derived(view.activeServer);
const homeView = $derived(view.homeView);
const activeChannel = $derived(active ? channels[active] : undefined);
const myStatus = $derived(store.session.myStatus);

// Namespaces we've auto-joined this session (dedup for zero-join auto-subscribe).
const autoJoinedNs = new Set<string>();

// Boundary-crossing state the layout still touches: history-load spinner (read
// by the ctx) + sync flags (written by doConnect/logout).
export const hist = $state<{ loading: string | null }>({ loading: null });
export const syncState = { syncing: false, synced: false };

export let verifyLoadTimer: ReturnType<typeof setTimeout> | null = null;

export let pendingChanCreate: Record<string, { cat: string; announce: boolean; voice: boolean }> = {};

export let roleBuf: Role[] = [];

export let roleFetchQueue: string[] = [];

export let currentBatchId = "";

export function fetchRoles(scope: string) {
  if (!scope) return;
  roleFetchQueue.push(scope);
  weft.roles(scope).catch(() => roleFetchQueue.pop());
}

export let grantBuf: { subject: string; caps: string[] }[] = [];

export let grantFetchQueue: string[] = [];

export function fetchGrants(scope: string) {
  if (!scope) return;
  grantFetchQueue.push(scope);
  weft.grantsAt(scope).catch(() => grantFetchQueue.pop());
}

export let loadingNsMembers: string | null = null;

export function fetchNsMembers(ns: string) {
  if (!ns) return;
  loadingNsMembers = ns;
  const srv = store.server(ns);
  srv.membersLoading = true;
  srv.memberBuf = [];
  weft.nsInfoMembers(ns).catch((e) => {
    srv.membersLoading = false;
    loadingNsMembers = null;
    toast(String(e), "error");
  });
}

export function createRoleAt(
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

export function deleteRoleAt(scope: string, roleId: string) {
  roleFetchQueue.push(scope);
  return weft.roleDelete(scope, roleId);
}

export const HISTORY_LIMIT = 50;

export let histByTarget: Record<string, Msg[]> = {};

export const oldestMsgid = (ch?: Channel) => ch?.messages.find((m) => m.msgid)?.msgid;

export function loadHistory(target: string, initial: boolean) {
  // Channels (`#`), DMs (`@`), and group DMs (`&`) all backfill; one at a time.
  if (
    hist.loading ||
    !(target.startsWith("#") || target.startsWith("@") || target.startsWith("&"))
  )
    return;
  hist.loading = target;
  histByTarget[target] = [];
  const before = initial ? undefined : oldestMsgid(channels[target]);
  weft.history(target, before).catch(() => {
    hist.loading = null; // don't wedge paging if the fetch never lands
  });
}

export function selectServer(ns: string) {
  if (active.startsWith("#") && nsOf(active) === ns) return; // already in this server
  // Land on the first channel in this server, else its empty view.
  const first = Object.values(channels)
    .filter((c) => c.name.startsWith("#") && nsOf(c.name) === ns)
    .sort((a, b) => (a.position ?? 0) - (b.position ?? 0) || a.name.localeCompare(b.name))[0];
  goto(nav.pathFor(first?.name ?? "", ns));
}

export const syncCursorKey = () => `weft:sync:${account}@${network}`;

export function loadSyncCursor(): string | undefined {
  try {
    return localStorage.getItem(syncCursorKey()) ?? undefined;
  } catch {
    return undefined;
  }
}

export function goHome() {
  const convos = Object.values(channels).filter((c) => c.name.startsWith("@") || c.name.startsWith("&"));
  if (!convos.length) {
    goto("/");
    return;
  }
  const recent = convos.reduce((a, b) =>
    (b.messages.at(-1)?.ts ?? 0) >= (a.messages.at(-1)?.ts ?? 0) ? b : a,
  );
  goto(nav.pathFor(recent.name));
}

export function handle(e: weft.WeftEvent) {
  switch (e.kind) {
    case "connected":
      store.session.network = e.network;
      store.session.account = e.account; // the "me" identity for the cap gates
      store.session.status = "online";
      cf.authError = "";
      ui.reconnecting = false;
      conn.reconnectAttempts = 0;
      ensureCapsAt(account, "*"); // learn operator status (federation gating)
      initVoice(account); // §16 wire the voice controller to the event stream
      queryProfile(account); // §10.3 load our own profile
      // §10.5 (re)load our verification claims. Reset the cache + the "loaded"
      // gate so a reconnect re-evaluates the no-email nudge cleanly; flip the
      // gate a beat later, once the streamed claims have had time to land.
      store.session.verifications = {};
      store.session.verificationsLoaded = false;
      if (verifyLoadTimer) clearTimeout(verifyLoadTimer);
      verifyLoadTimer = setTimeout(() => (store.session.verificationsLoaded = true), 2000);
      weft.verifyList().catch(() => {});
      // Restore whether this account already dismissed the "add email" nudge.
      try {
        ui.emailBannerDismissed = localStorage.getItem(emailNudgeKey()) === "1";
      } catch {
        ui.emailBannerDismissed = false;
      }
      store.social.friends.clear();
      store.social.groups.clear();
      weft.listFriends().catch(() => {}); // social layer: load friends + requests
      weft.listGroups().catch(() => {}); // and group DMs
      restoreDms(); // re-open the 1:1 DMs from last session (history loads on click)
      // Clear any half-finished history load from before a reconnect — its
      // BATCH will never arrive, so a stale guard would block every new load.
      hist.loading = null;
      histByTarget = {};
      // Remember creds so the next launch logs straight back in. NOTE: this
      // includes the password in localStorage — a dev convenience; the
      // hardening is OS-keychain storage in the backend.
      try {
        localStorage.setItem(
          SAVED_KEY,
          JSON.stringify({ host: cf.host, account: cf.account.trim(), password: cf.password }),
        );
        // Remember the homeserver as the local default (desktop) so the picker
        // is pre-filled and skippable next launch. On web it's the origin.
        if (!weft.isWeb) localStorage.setItem(HOMESERVER_KEY, cf.host);
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
      //
      // The cursor delta assumes the client still holds its skeleton in memory —
      // it only re-sends missed messages, NOT the namespace/channel roster. So a
      // cold start (fresh app launch, empty in-memory state) MUST do a full sync,
      // or the rail comes up empty; only an in-session reconnect replays the
      // cursor.
      syncState.syncing = true;
      const syncCursor = syncState.synced ? loadSyncCursor() : undefined;
      syncState.synced = true;
      weft.sync(syncCursor).catch(() => (syncState.syncing = false));
      break;
    case "server-info":
      // §3.6 the negotiation WELCOME, seen before auth — remember whether this
      // homeserver requires a register email (form shaping) and whether it can
      // actually mail codes at all (gates the no-email nudge).
      cf.emailRequired = e.email_required;
      ui.serverEmailAvailable = e.email_available;
      break;
    case "media-token":
      weft.setMediaBearer(e.token); // §13 fetch bearer for /media URLs
      break;
    case "auth-failed":
      ui.reconnecting = false;
      conn.lastCreds = null;
      store.session.status = "connect";
      cf.authError = e.reason;
      cf.authFailed = true;
      break;
    case "closed":
      // A probe tears its own handshake-only connection down; that close is
      // expected and must not touch the connect screen's state.
      if (cf.probing) break;
      if (conn.manualLogout) {
        conn.manualLogout = false;
        break;
      }
      // AUTH-FAILED already closed the stream (§3.6) and set a specific
      // reason — don't overwrite it with the generic close message.
      if (cf.authFailed) {
        cf.authFailed = false;
        break;
      }
      // Unexpected drop while online → keep the UI and auto-reconnect.
      if (conn.lastCreds && (status === "online" || ui.reconnecting)) {
        attemptReconnect();
      } else if (status === "connecting") {
        // A user-initiated attempt failed before authenticating.
        store.session.status = "connect";
        cf.authError = e.reason;
      }
      // Otherwise the socket was idle (e.g. a probe's own teardown) — a close
      // there carries no user-facing meaning, so leave the screen untouched.
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
          if (!active && !homeView) goto(nav.pathFor(e.channel));
          // Presence is broadcast to shared channels only, so re-announce
          // ours whenever we join one (lets its members see our status).
          weft.presence(myStatus).catch(() => {});
        } else {
          // Mark a just-joined member online (they announce, but a peer that
          // was already here won't have — best effort with this model).
          store.accountOf(e.user).presence ??= "online";
        }
      } else {
        ch.members = ch.members.filter((m) => m.name !== e.user);
        if (e.user === account) {
          delete channels[e.channel];
          if (active === e.channel) goto(nav.pathFor(Object.keys(channels)[0] ?? ""));
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
            : e.system === "welcome"
              ? `👋 Welcome, ${who}!`
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
      // Batch messages buffer until BATCH END. A SEARCH / PINS / thread batch
      // routes to its panel model; else the HISTORY batch to the per-channel
      // history buffer.
      if (e.history) {
        if (store.threads.loadingRoot) store.threads.buf.push(msg);
        else if (store.search.loadingChannel) store.search.buf.push(msg);
        else if (store.pins.loadingChannel) store.pins.buf.push(msg);
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
          const ti = store.threads.messages.findIndex((m) => m.pending && m.label === e.label);
          if (ti !== -1)
            store.threads.messages = store.threads.messages.map((m, i) => (i === ti ? msg : m));
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
        store.threads.root &&
        key === active &&
        msg.thread === store.threads.root.msgid &&
        !store.threads.messages.some((m) => m.msgid === msg.msgid)
      ) {
        store.threads.messages = [...store.threads.messages, msg];
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
        ch.bump(pinged);
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
      const acc = store.accountOf(key);
      acc.display = e.display ?? undefined;
      acc.avatar = e.avatar ?? undefined;
      acc.about = e.about ?? "";
      acc.status = e.status ?? "";
      acc.requested = true;
      break;
    }
    case "nick": {
      // §10.3 a per-namespace server nickname (empty = cleared).
      const acct = e.network === network ? e.account : `${e.account}@${e.network}`;
      const key = nickKey(e.scope, acct);
      if (e.nick) nicks.set(key, e.nick);
      else nicks.delete(key);
      break;
    }
    case "verified":
      // §10.5 one of our own verification claims (email/birthday).
      store.session.verifications[e.claim_kind] = { subject: e.subject, state: e.state };
      break;
    case "presence":
      store.accountOf(e.user).presence = e.status;
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
      //
      // Only update a channel we actually have. The auth snapshot streams a
      // count for every persisted read marker, including stale ones for
      // deleted / no-longer-accessible channels; materializing those would pop
      // a phantom rail tile (raw ULID name, NO-SUCH-TARGET on click). Real
      // channels get their count from SYNC, which re-sends UNREAD-COUNTS right
      // after each CHANNEL-LAYOUT — so guarding here loses nothing.
      const ch = channels[e.channel];
      if (ch && e.channel !== active && !isMuted(e.channel)) {
        ch.unreadCount = e.unread;
        ch.unread = e.unread > 0;
        ch.mentionCount = e.mentions;
        ch.mention = e.mentions > 0;
      }
      break;
    }
    case "sync-end": {
      syncState.syncing = false;
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
      // the per-channel MEMBER/LAYOUT events; this ns-level marker additionally
      // tracks *my own* membership so a channel-less server still appears in the
      // rail (and, on join, gets auto-selected below).
      if (e.user === account) {
        if (e.action === "join") {
          store.server(e.namespace).joined = true;
          // React visibly to a *live* join (create/join a server) by landing on
          // it. During SYNC we're just restoring the rail — don't hijack the
          // view by jumping to the last-restored namespace.
          if (!syncState.syncing && activeServer !== e.namespace) selectServer(e.namespace);
        } else {
          const s = store.servers.get(e.namespace);
          if (s) s.joined = false;
        }
      }
      break;
    }
    case "chan-sync":
      // §7.9 per-channel SYNC header — previews are withheld in v1, so there's
      // nothing to apply; the `reset` flag lands with the body-stream work.
      break;
    case "emoji": {
      // §9.4 a namespace custom emoji (from EMOJI LIST or a live add).
      store.server(e.namespace).emoji.set(e.name, e.media);
      md.clearMdCache(); // rendered `:name:` may change
      break;
    }
    case "emoji-removed": {
      store.servers.get(e.namespace)?.emoji.delete(e.name);
      md.clearMdCache();
      break;
    }
    case "chanmeta": {
      // §6.3 CHANNEL DELETE confirms with `deleted` — drop the channel from
      // every local view (do NOT ensureChannel first, or it'd be re-created).
      if (e.key === "deleted") {
        delete channels[e.channel]; // unread/mention tallies ride the instance
        if (active === e.channel) goto(nav.pathFor("", activeServer));
        break;
      }
      const c = ensureChannel(e.channel);
      if (e.key === "topic") c.topic = e.value;
      else if (e.key === "posting") c.restricted = e.value === "restricted";
      else if (e.key === "view-gated") c.viewGated = e.value === "true";
      else if (e.key === "category") c.category = e.value || undefined;
      else if (e.key === "position") c.position = parseInt(e.value, 10) || 0;
      if (e.key === "category" || e.key === "position") cacheChanLayout(e.channel, c.category, c.position ?? 0);
      break;
    }
    case "pinned": {
      const ch = ensureChannel(e.channel);
      ch.pinnedIds = [...(ch.pinnedIds ?? []).filter((id) => id !== e.msgid), e.msgid];
      if (store.pins.open && active === e.channel) weft.pins(e.channel).catch(() => {}); // refresh panel
      break;
    }
    case "unpinned": {
      const ch = channels[e.channel];
      if (ch) ch.pinnedIds = (ch.pinnedIds ?? []).filter((id) => id !== e.msgid);
      if (store.pins.open && active === e.channel)
        store.pins.list = store.pins.list.filter((m) => m.msgid !== e.msgid);
      break;
    }
    case "thread": {
      if (e.name) store.threads.names.set(e.root, e.name);
      else store.threads.names.delete(e.root);
      if (store.threads.loadingList)
        store.threads.listBuf.push({ root: e.root, name: e.name ?? undefined, replies: e.replies, last: e.last ?? undefined });
      break;
    }
    case "thread-named": {
      if (e.name) store.threads.names.set(e.root, e.name);
      else store.threads.names.delete(e.root);
      // Reflect a live rename in an open threads list.
      const i = store.threads.list.findIndex((t) => t.root === e.root);
      if (i >= 0) store.threads.list[i] = { ...store.threads.list[i], name: e.name ?? undefined };
      break;
    }
    case "friend":
      store.social.friends.set(e.user, e.state);
      // A fresh incoming request is worth a nudge.
      if (e.state === "incoming") toast(`Friend request from ${e.user}`, "info");
      break;
    case "friend-removed":
      store.social.friends.delete(e.user);
      break;
    case "group": {
      store.social.groups.set(e.id, { name: e.name ?? undefined, members: e.members });
      ensureChannel(e.id); // a conversation entry so it lists + holds messages
      break;
    }
    case "group-member": {
      const g = store.social.groups.get(e.group);
      if (!g) break;
      const me = `${account}@${network}`;
      // SvelteMap values aren't deeply reactive — re-set the entry on change.
      if (e.action === "join") {
        if (!g.members.includes(e.user))
          store.social.groups.set(e.group, { ...g, members: [...g.members, e.user] });
      } else if (e.user === me) {
        // If *we* left, drop the conversation.
        store.social.groups.delete(e.group);
        delete channels[e.group];
        if (active === e.group) goto("/");
      } else {
        store.social.groups.set(e.group, { ...g, members: g.members.filter((m) => m !== e.user) });
      }
      break;
    }
    case "call-ring":
      store.social.incomingCall = { from: e.from, room: e.room };
      break;
    case "call-state":
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
      break;
    case "call-media":
      // The server authorized the call and minted our media credential — join
      // the LiveKit room so audio flows. Works for both a 1:1 call (activeCall)
      // and a group call (activeGroupCall) — the credential is the same shape.
      void connectCallMedia(e.endpoint, e.token);
      break;
    case "group-call-state": {
      const roster = store.social.groupCallRoster.get(e.group) ?? [];
      const me = `${account}@${network}`;
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
      break;
    }
    case "caps": {
      const set = e.caps ? e.caps.split(",") : [];
      store.session.caps.set(`${e.account}|${e.scope}`, {
        owner: set.includes("ns-admin") || set.includes("netblock"),
        mod: set.includes("mute") || set.includes("ban") || set.includes("kick"),
        list: set,
      });
      capsResolved(e.account, e.scope);
      confirmSuccess(`caps:${e.account}|${e.scope}`);
      break;
    }
    case "role":
      roleBuf.push(
        new Role({
          id: e.role,
          name: e.name,
          color: e.color,
          caps: e.caps ? e.caps.split(",") : [],
          hoist: e.hoist,
          pingable: e.pingable,
          position: e.position,
        }),
      );
      break;
    case "role-member":
      memberRoles[`${e.account}|${e.scope}`] = e.roles ? e.roles.split(",") : [];
      confirmSuccess(`roles:${e.account}|${e.scope}`);
      break;
    case "ns-member-info":
      if (loadingNsMembers)
        store.server(loadingNsMembers).memberBuf.push({
          user: e.user,
          network: e.network,
          joinedMs: e.joined_ms,
          roles: e.roles ?? [],
        });
      break;
    case "grant-info":
      grantBuf.push({ subject: e.subject, caps: e.caps ? e.caps.split(",") : [] });
      break;
    case "channel-layout": {
      const ch = ensureChannel(e.channel);
      ch.category = e.category ?? undefined;
      ch.position = e.position;
      ch.voice = e.channel_kind === "voice"; // §16 render as a voice channel
      if (e.vanity) ch.vanity = e.vanity; // v0.13 display name; wire name is ids
      cacheChanLayout(e.channel, ch.category, e.position);
      reconcileChannelCreate(e.channel, e.vanity); // finish a pending create
      break;
    }
    case "channel-renamed": {
      // Re-key local state to the new identity (idempotent — this arrives as
      // a broadcast plus a labeled copy to the initiator).
      const cur = channels[e.old];
      if (cur) {
        cur.name = e.new;
        channels[e.new] = cur; // unread/mention tallies ride the instance
        delete channels[e.old];
        cacheChanLayout(e.new, cur.category, cur.position ?? 0);
        if (active === e.old) goto(nav.pathFor(e.new), { replaceState: true });
        if (ui.chanPerms === e.old) ui.chanPerms = e.new;
        // The actor was respawned under the new name — re-subscribe.
        weft.join(e.new).catch(() => {});
      }
      confirmSuccess(`rename:${e.new}`);
      break;
    }
    case "ns-meta":
      // v0.13: namespaces are keyed by their immutable **id** everywhere the
      // client addresses them (channels `#<id>/…`, scopes `ns:<id>`, the rail
      // tile is `nsOf(channel)` = the id). The vanity `e.name` is display only.
      // §6.2 deletion marker (owner cleared + description "deleted"): drop the
      // namespace from every local view instead of storing a tombstone —
      // otherwise a deleted server would linger in the rail/channel list.
      if (e.owner === null && e.description === "deleted") {
        store.servers.delete(e.id);
        for (const name of Object.keys(channels)) {
          if (name.startsWith("#") && nsOf(name) === e.id) delete channels[name];
        }
        if (activeServer === e.id) goHome();
        break;
      }
      const srv = store.server(e.id);
      srv.applyMeta(e);
      cacheNsCats(e.id, e.categories ?? []);
      // Owning a namespace is membership — the owner can't leave it (only
      // transfer or delete). Record it so a server I just created appears in the
      // rail the instant its NS-META returns, with no channels and no Discover
      // round-trip, and stays put across Discover's transient resets.
      if (e.owner === account) srv.joined = true;
      // A namespace I own but hold no channels in is one I just created (the
      // server seeds its `#general`): auto-join so I'm subscribed to it live —
      // no client-side channel creation needed.
      if (
        e.owner === account &&
        !autoJoinedNs.has(e.id) &&
        !Object.values(channels).some((c) => c.name.startsWith("#") && nsOf(c.name) === e.id)
      ) {
        autoJoinedNs.add(e.id);
        weft.nsJoin(e.id).catch(() => {});
      }
      break;
    case "more":
      ui.discoverCursor = e.cursor;
      break;
    case "manifest":
      // A bridge's channel set/state (§11). `severed`/`removed` drops it.
      if (e.state === "severed" || e.state === "removed") store.federation.manifests.delete(e.peer);
      else
        store.federation.manifests.set(e.peer, {
          peer: e.peer,
          version: e.version,
          state: e.state,
          channels: e.channels,
          history: e.history,
          media: e.media,
          typing: e.typing,
        });
      break;
    case "netblocked":
      store.federation.netblocks.set(e.network, e.reason);
      break;
    case "token":
      // A permission change is confirmed with a transient toast, never a
      // channel system line (those are for people, not admin bookkeeping).
      toast(`Permissions updated for ${e.subject}`, "info");
      break;
    case "invited":
      if (e.max_uses === 0) {
        // A revoke echo (INVITED … max-uses=0) — close it + drop from the menu.
        if (store.invites.id === e.invite_id) {
          store.invites.link = null;
          store.invites.id = null;
        }
        store.invites.list = store.invites.list.filter((i) => i.invite_id !== e.invite_id);
      } else {
        store.invites.link = e.link ?? e.invite_id;
        store.invites.id = e.invite_id;
        // A freshly-minted invite: reflect it live wherever the list is shown —
        // the standalone menu or the Server-Settings Invites tab.
        const listShown = store.invites.listOpen || (ui.nsSettingsOpen && ui.nsTab === "invites");
        if (listShown && e.scope === store.invites.scope) weft.inviteList(store.invites.scope).catch(() => {});
      }
      break;
    case "invite-info":
      if (store.invites.loading) store.invites.buf.push(e);
      break;
    case "reported":
      sys(`✓ report filed (${e.report_id})`);
      break;
    case "report-filed":
      store.reports.queue.set(e.report_id, {
        report_id: e.report_id,
        msgid: e.msgid,
        category: e.category,
        state: e.state,
        reporter: e.reporter,
      });
      break;
    case "report-resolved":
      store.reports.queue.delete(e.report_id);
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
      // history branch, or it would steal the in-flight HISTORY's `hist.loading`
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
        if (loadingNsMembers) {
          const srv = store.server(loadingNsMembers);
          srv.members = srv.memberBuf.map((r) => {
            const handle = r.network === network ? r.user : `${r.user}@${r.network}`;
            const m = new Membership(srv, store.accountOf(handle));
            m.network = r.network;
            m.joinedMs = r.joinedMs;
            m.roleIds = r.roles;
            return m;
          });
          srv.memberBuf = [];
          srv.membersLoading = false;
        }
        loadingNsMembers = null;
        currentBatchId = "";
        break;
      }
      // GRANTS batch (`gr…`) — channel-permission member overrides. Checked
      // before the `r…` role branch (neither prefix overlaps: "gr" ≠ "r").
      if (currentBatchId.startsWith("gr")) {
        const scope = grantFetchQueue.shift();
        if (scope) store.grants.set(scope, grantBuf);
        grantBuf = [];
        currentBatchId = "";
        break;
      }
      if (currentBatchId.startsWith("r")) {
        const scope = roleFetchQueue.shift();
        // Keep roles in position order (server sorts, but be safe).
        roleBuf.sort((a, b) => a.position - b.position || a.name.localeCompare(b.name));
        if (scope) {
          // Single source per scope: ns roles on the Server, others by-scope.
          if (scope.startsWith("ns:")) store.server(scope.slice(3)).roles = roleBuf;
          else rolesByScope[scope] = roleBuf;
          md.clearMdCache(); // role names/colors feed mention rendering
        }
        roleBuf = [];
        currentBatchId = "";
        break;
      }
      if (store.threads.loadingRoot) {
        store.threads.messages = store.threads.buf;
        store.threads.buf = [];
        store.threads.loadingRoot = null;
        break;
      }
      if (store.search.loadingChannel) {
        store.search.results = store.search.buf;
        store.search.buf = [];
        store.search.loadingChannel = null;
        store.search.loading = false;
        break;
      }
      if (store.pins.loadingChannel) {
        const ch = channels[store.pins.loadingChannel];
        if (ch) ch.pinnedIds = store.pins.buf.map((m) => m.msgid).filter(Boolean) as string[];
        store.pins.list = store.pins.buf;
        store.pins.buf = [];
        store.pins.loadingChannel = null;
        break;
      }
      if (store.threads.loadingList) {
        // Newest activity first (last-activity msgid sorts by its ULID).
        store.threads.listBuf.sort((a, b) => (b.last ?? "").localeCompare(a.last ?? ""));
        store.threads.list = store.threads.listBuf;
        store.threads.listBuf = [];
        store.threads.loadingList = false;
        break;
      }
      if (store.invites.loading) {
        store.invites.list = store.invites.buf;
        store.invites.buf = [];
        store.invites.loading = false;
        break;
      }
      // Flush every channel that accumulated a history page. Each page goes to
      // the channel its messages name (`target`), so this is correct no matter
      // which batch's END fired or whether `hist.loading` was cleared — a
      // stray batch can't lose a page. The requested channel is always flushed
      // (an empty page still marks it loaded, so we stop re-requesting).
      const requested = hist.loading;
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
      hist.loading = null;
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
        const list = store.deny.get(e.scope) ?? [];
        const i = list.findIndex((r) => r.account === e.account && r.kind === e.action);
        const rec = { account: e.account, kind: e.action, by: e.by, reason: e.reason };
        store.deny.set(e.scope, i >= 0 ? list.map((r, j) => (j === i ? rec : r)) : [...list, rec]);
      } else if (e.action === "unmute" || e.action === "unban") {
        const kind = e.action === "unmute" ? "mute" : "ban";
        const cur = store.deny.get(e.scope);
        if (cur)
          store.deny.set(
            e.scope,
            cur.filter((r) => !(r.account === e.account && r.kind === kind)),
          );
      }
      // Moderation is reflected in Server Settings (the deny-list above) and
      // by the target losing access — never as a channel system line, which
      // would be timeline noise broadcast to every member.
      break;
    }
    case "error":
      toast(`${e.code}: ${e.text}`, "error");
      break;
  }
}

export function sys(body: string) {
  if (activeChannel)
    activeChannel.messages.push(mkMsg({ author: "", body, time: clock(), ts: Date.now(), own: false, system: true }));
}

export function findMsg(target: string, msgid: string): Msg | undefined {
  return (
    histByTarget[target]?.find((m) => m.msgid === msgid) ??
    channels[target]?.messages.find((m) => m.msgid === msgid)
  );
}

export function applyReaction(m: Msg, emoji: string, op: string, by: string) {
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

export const typingTimers = new Map<string, ReturnType<typeof setTimeout>>();

export function setTyping(channel: string, user: string, active: boolean) {
  const key = `${channel}\u0000${user}`;
  clearTimeout(typingTimers.get(key));
  const ch = ensureChannel(channel);
  if (active) {
    if (!ch.typers.includes(user)) ch.typers = [...ch.typers, user];
    // Fallback expiry in case a `stop` is lost.
    typingTimers.set(key, setTimeout(() => setTyping(channel, user, false), 6000));
  } else {
    ch.typers = ch.typers.filter((u) => u !== user);
    typingTimers.delete(key);
  }
}

export function reconcileChannelCreate(canonical: string, vanity: string) {
  const ns = nsOf(canonical);
  if (!ns || !vanity) return;
  const key = `${ns}|${vanity}`;
  const pending = pendingChanCreate[key];
  if (!pending) return;
  delete pendingChanCreate[key];

  const ch = ensureChannel(canonical);
  ch.voice = pending.voice;
  // Subscribe (text) so live messages arrive; voice rooms are entered via VOICE.
  if (!pending.voice) weft.join(canonical).catch(() => {});
  if (pending.cat) weft.channelMeta(canonical, "category", pending.cat).catch((e) => toast(String(e), "error"));
  // Announcement channel: view-open, post-restricted to `send` holders (§6.7).
  if (!pending.voice && pending.announce)
    weft.channelMeta(canonical, "posting", "restricted").catch((e) => toast(String(e), "error"));
  // Jump to the new channel (a voice room is *entered* by clicking it, so just
  // navigate here rather than auto-joining the call).
  if (!pending.voice) markRead(canonical);
  goto(nav.pathFor(canonical));
}
