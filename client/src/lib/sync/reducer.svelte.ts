// The event reducer: `handle(e)` (wire event → store mutation) plus its
// request/batch machinery (fetch queues, history buffering) and the message/
// typing/channel-create helpers it owns. Imports the stores; never the reverse.
import { page } from "$app/state";
import * as nav from "$lib/nav";
import * as weft from "$lib/weft";
import type { Msg } from "$lib/types";
import { store } from "$lib/models/store.svelte";
import { Channel, channels, mkMsg, ensureChannel, nsOf, chanShort, markRead, cacheNsCats, persistDms, restoreDms, applyReaction, pinsHandlers } from "$lib/models/channel.svelte";
import { rosterFetchTarget } from "$lib/models/server.svelte";
import { federationHandlers } from "$lib/models/federation.svelte";
import { socialHandlers } from "$lib/models/social.svelte";
import { sessionHandlers } from "$lib/models/session.svelte";
import { threadsHandlers } from "$lib/models/threads.svelte";
import { invitesHandlers } from "$lib/models/invites.svelte";
import { accountHandlers } from "$lib/models/account.svelte";
import { profileHandlers } from "$lib/profile.svelte";
import { serverHandlers } from "$lib/models/server.svelte";
import { reportsHandlers } from "$lib/models/reports.svelte";
import { moderationHandlers } from "$lib/moderation";
import { channelHandlers } from "$lib/sync/channel-handlers";
import type { HandlerMap } from "$lib/sync/handler-map";
import { goHome } from "$lib/navigation";
import { rolesByScope, ensureCapsAt, ensureCaps, roleScopeOf, mentionsMe, fedRolesFetched, fetchMemberRoles, roleBuf, roleFetchQueue, grantBuf, grantFetchQueue } from "$lib/models/session.svelte";
import { cf, emailNudgeKey } from "$lib/models/connect.svelte";
import { conn, attemptReconnect, HOMESERVER_KEY, SAVED_KEY, syncCursorKey, loadSyncCursor, syncState } from "$lib/connection.svelte";
import { ui } from "$lib/ui.svelte";
import * as md from "$lib/markdown";
import { toast, confirmSuccess } from "$lib/toasts.svelte";
import { msgEpoch, msgTime, retentionOf } from "$lib/time";
import { notifLevel, isMuted } from "$lib/notif";
import { queryProfile } from "$lib/profile.svelte";
import { initVoice, voice } from "$lib/voice.svelte";

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

export let verifyLoadTimer: ReturnType<typeof setTimeout> | null = null;

export let currentBatchId = "";





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




// The EventHandler registry: domains own their event handling; the switch below
// holds the core sync/ingest + cross-cutting cases not yet migrated.
const domainHandlers: HandlerMap = {
  ...federationHandlers,
  ...socialHandlers,
  ...sessionHandlers,
  ...threadsHandlers,
  ...invitesHandlers,
  ...accountHandlers,
  ...pinsHandlers,
  ...profileHandlers,
  ...serverHandlers,
  ...reportsHandlers,
  ...moderationHandlers,
  ...channelHandlers,
};

export function handle(e: weft.WeftEvent) {
  const domainHandler = domainHandlers[e.kind] as ((ev: weft.WeftEvent) => void) | undefined;
  if (domainHandler) {
    domainHandler(e);
    return;
  }
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
    case "ns-member-info": {
      const t = rosterFetchTarget();
      if (t)
        store.server(t).memberBuf.push({
          user: e.user,
          network: e.network,
          joinedMs: e.joined_ms,
          roles: e.roles ?? [],
        });
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
    case "token":
      // A permission change is confirmed with a transient toast, never a
      // channel system line (those are for people, not admin bookkeeping).
      toast(`Permissions updated for ${e.subject}`, "info");
      break;
    case "typing":
      if (e.user !== account) ensureChannel(e.channel).setTyping(e.user, e.state === "start");
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
        const t = rosterFetchTarget();
        if (t) store.server(t).applyMembers(network);
        currentBatchId = "";
        break;
      }
      // GRANTS batch (`gr…`) — channel-permission member overrides. Checked
      // before the `r…` role branch (neither prefix overlaps: "gr" ≠ "r").
      if (currentBatchId.startsWith("gr")) {
        const scope = grantFetchQueue.shift();
        // Store a COPY — `grantBuf` is a reused module buffer we clear next line.
        if (scope) store.grants.set(scope, grantBuf.slice());
        grantBuf.length = 0;
        currentBatchId = "";
        break;
      }
      if (currentBatchId.startsWith("r")) {
        const scope = roleFetchQueue.shift();
        // Keep roles in position order (server sorts, but be safe).
        roleBuf.sort((a, b) => a.position - b.position || a.name.localeCompare(b.name));
        if (scope) {
          // Single source per scope: ns roles on the Server, others by-scope. Store
          // a COPY — `roleBuf` is a reused module buffer cleared just below (else the
          // clear would empty the very array we just assigned).
          if (scope.startsWith("ns:")) store.server(scope.slice(3)).roles = roleBuf.slice();
          else rolesByScope[scope] = roleBuf.slice();
          md.clearMdCache(); // role names/colors feed mention rendering
        }
        roleBuf.length = 0;
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
    case "error":
      toast(`${e.code}: ${e.text}`, "error");
      break;
  }
}

export function findMsg(target: string, msgid: string): Msg | undefined {
  return (
    histByTarget[target]?.find((m) => m.msgid === msgid) ??
    channels[target]?.messages.find((m) => m.msgid === msgid)
  );
}



