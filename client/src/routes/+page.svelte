<script lang="ts">
  import { onMount } from "svelte";
  import * as weft from "$lib/weft";

  // ---- connection + form state ----
  type Status = "connect" | "connecting" | "online";
  let status = $state<Status>("connect");
  let network = $state("");
  let account = $state("");
  let authError = $state("");

  let mode = $state<weft.Mode>("login");
  let host = $state("127.0.0.1:4433");
  let formAccount = $state("");
  let formPassword = $state("");

  // ---- live data ----
  type Member = { name: string; origin: "local" | "federated" };
  type Msg = {
    /// Stable render key (msgids aren't on system lines, and prepending
    /// history shifts array indices — so keying by index would misrender).
    key: number;
    author: string;
    body: string;
    time: string;
    own: boolean;
    system?: boolean;
    /// Origin msgid — the target for edit / delete / react / reply. Absent on
    /// system lines.
    msgid?: string;
    /// Shows the "(edited)" marker.
    edited?: boolean;
    /// emoji → aggregate count + whether *I* reacted.
    reactions?: Record<string, { count: number; mine: boolean }>;
    /// Render body as markdown (§9.4 `fmt=md`).
    md?: boolean;
    /// msgid this replies to (§9.3).
    replyTo?: string;
  };
  let msgSeq = 0;
  const mkMsg = (m: Omit<Msg, "key">): Msg => ({ ...m, key: msgSeq++ });
  type Channel = {
    name: string;
    retention: string;
    messages: Msg[];
    members: Member[];
    /// History backfill (Phase 1).
    historyLoaded?: boolean;
    hasMore?: boolean; // older pages available upstream
    truncated?: boolean; // a retention gap at the top (§6.4)
    /// Channel management + layout (Phase 6).
    topic?: string;
    unread?: boolean;
    lastRead?: string; // newest msgid we've marked read
    category?: string; // CHANNEL-LAYOUT grouping
    position?: number;
    rosterLoaded?: boolean; // MEMBERS snapshot fetched
  };

  let channels = $state<Record<string, Channel>>({});
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
  let myStatus = $state("online");
  let statusMenu = $state(false);
  let dmInput = $state("");
  // ---- discover dialog (Phase 6) ----
  let discoverOpen = $state(false);
  let discovered = $state<Record<string, Extract<weft.WeftEvent, { kind: "ns-meta" }>>>({});
  let discoverCursor = $state<string | null>(null);
  let discoverName = $state("");
  let redeemInput = $state("");
  let createName = $state("");
  let createVis = $state("public");
  // ---- roles / invites / reports (Phase 7) ----
  const REPORT_CATEGORIES = [
    "spam",
    "harassment",
    "violence",
    "sexual",
    "csam",
    "illegal",
    "self-harm",
    "other",
  ];
  const CAPS = ["send", "react", "pin", "invite", "mute", "ban", "kick", "view", "reports"];
  const RESOLVE_ACTIONS = ["dismissed", "content-removed", "user-actioned", "escalated"];
  let reportTarget = $state<Msg | null>(null);
  let reportCategory = $state("spam");
  let reportNote = $state("");
  let reportScope = $state("ns");
  let reportsOpen = $state(false);
  let reportQueue = $state<Record<string, Extract<weft.WeftEvent, { kind: "report-filed" }>>>({});
  let rolesTarget = $state<string | null>(null);
  let roleScope = $state("");
  let roleCap = $state("send");
  let inviteLink = $state<string | null>(null);
  // ---- namespace admin panel (§6.2 / §2.4) ----
  let nsSettingsOpen = $state(false);
  let nsTitle = $state("");
  let nsDesc = $state("");
  let nsVis = $state("public");
  let nsDelegSubject = $state("");
  let nsDelegCaps = $state("mute,kick");
  let nsNewOwner = $state("");
  let nsRecM = $state(2);
  let nsRecKeys = $state("");
  let activeNsMeta = $derived(activeServer ? discovered[activeServer] : undefined);

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
  function msgTime(msgid: string): string {
    const ulid = msgid.split("/").pop() ?? "";
    if (ulid.length < 10) return clock();
    let ms = 0;
    for (let i = 0; i < 10; i++) {
      const v = CROCKFORD.indexOf(ulid[i].toUpperCase());
      if (v < 0) return clock();
      ms = ms * 32 + v;
    }
    return hhmm(new Date(ms));
  }
  const retentionOf = (policy: string) => {
    if (policy.startsWith("retained")) return "retained";
    if (["ephemeral", "permanent", "e2ee"].includes(policy)) return policy;
    return "retained";
  };

  function ensureChannel(name: string): Channel {
    if (!channels[name]) {
      channels[name] = { name, retention: "retained", messages: [], members: [] };
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
  // Discord-style grouping for the *active server*: by CHANNEL-LAYOUT category
  // (position-ordered), uncategorized under "Channels".
  let channelGroups = $derived.by(() => {
    const groups = new Map<string, Channel[]>();
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
  // DM conversations (keyed `@peer`), plus any peer we've opened a blank DM with.
  let dmList = $derived(Object.values(channels).filter((c) => c.name.startsWith("@")));

  // ---- DM + presence helpers ----
  const peerOf = (key: string) => key.replace(/^@/, "");
  const dotClass = (acct: string) => `dot ${presence[acct] ?? "offline"}`;

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
    statusMenu = false;
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
        weft.join("#general"); // a sensible default landing channel
        break;
      case "auth-failed":
        status = "connect";
        authError = e.reason;
        break;
      case "closed":
        if (status === "online") authError = e.reason;
        status = "connect";
        break;
      case "policy":
        ensureChannel(e.channel).retention = retentionOf(e.policy);
        break;
      case "member": {
        const ch = ensureChannel(e.channel);
        if (e.action === "join") {
          if (!ch.members.some((m) => m.name === e.user)) {
            ch.members.push({ name: e.user, origin: e.network === network ? "local" : "federated" });
          }
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
        const msg = mkMsg({
          author: e.sender,
          body: e.body,
          time: msgTime(e.msgid),
          own: e.own,
          msgid: e.msgid,
          edited: e.edited,
          md: e.md,
          replyTo: e.reply_to ?? undefined,
        });
        // History-batch messages buffer until BATCH END, then prepend in order.
        if (e.history) {
          historyBuf.push(msg);
          break;
        }
        const ch = ensureChannel(key);
        // Dedupe: history backfill may re-deliver a live message.
        if (e.msgid && ch.messages.some((m) => m.msgid === e.msgid)) break;
        ch.messages.push(msg);
        if (!e.own && key !== active) ch.unread = true;
        break;
      }
      case "presence":
        presence[e.user] = e.status;
        break;
      case "marked": {
        // Read-marker sync from another device (§9.7).
        const ch = channels[e.channel];
        if (ch) {
          ch.lastRead = e.msgid;
          ch.unread = false;
        }
        break;
      }
      case "chanmeta":
        if (e.key === "topic") ensureChannel(e.channel).topic = e.value;
        break;
      case "channel-layout": {
        const ch = ensureChannel(e.channel);
        ch.category = e.category ?? undefined;
        ch.position = e.position;
        break;
      }
      case "ns-meta":
        discovered[e.name] = e;
        break;
      case "more":
        discoverCursor = e.cursor;
        break;
      case "token":
        sys(`✓ permissions updated for ${e.subject} @ ${e.scope}`);
        break;
      case "invited":
        inviteLink = e.link ?? e.invite_id;
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
        break; // messages between here and batch-end are buffered above
      case "batch-end": {
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
        // Surface the action as a system line in the affected channel.
        const ch = e.scope.startsWith("#") ? ensureChannel(e.scope) : activeChannel;
        const who = e.by ? ` by ${e.by}` : "";
        const why = e.reason ? ` (${e.reason})` : "";
        ch?.messages.push(mkMsg({ author: "", body: `${e.account} ${e.action}d${who} — ${e.scope}${why}`, time: clock(), own: false, system: true }));
        break;
      }
      case "error":
        if (activeChannel) activeChannel.messages.push(mkMsg({ author: "", body: `${e.code}: ${e.text}`, time: clock(), own: false, system: true }));
        break;
    }
  }

  // ---- actions ----
  async function doConnect() {
    if (!formAccount.trim()) return;
    authError = "";
    status = "connecting";
    try {
      await weft.connect(host.trim(), formAccount.trim(), formPassword, mode);
    } catch (err) {
      status = "connect";
      authError = String(err);
    }
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
      activeChannel.messages.push(mkMsg({ author: "", body, time: clock(), own: false, system: true }));
  }

  /// A capability-gated moderation action (§10.4). These are **server-side**:
  /// the client sends the wire intent and weftd enforces it (BAN/KICK/MUTE are
  /// wired here frontend-first; the weftd verbs land later). Shared by the
  /// slash commands and the member-row buttons.
  function moderate(verb: string, user: string) {
    if (!user) return sys(`usage: /${verb} <account>`);
    if (!active) return sys("join a channel first");
    weft.sendRaw(`${verb.toUpperCase()} ${active} ${user}`).catch(() => {});
    sys(`${verb} requested for ${user} on ${active} (pending server support)`);
  }

  /// Slash commands — the primary control surface in the composer.
  function runSlash(input: string) {
    const [raw, ...rest] = input.slice(1).split(/\s+/);
    const cmd = raw.toLowerCase();
    const arg = rest.join(" ").trim();
    switch (cmd) {
      case "ban":
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
          "/join #chan · /part · /create #chan · /delete · /topic <text> · /ban /kick /mute /unmute <user>",
        );
        break;
      default:
        sys(`unknown command: /${cmd} (try /help)`);
    }
  }

  function doSend() {
    const text = composer.trim();
    if (!text) return;
    if (text.startsWith("/")) {
      runSlash(text);
      composer = "";
      return;
    }
    if (!active) return;
    weft.sendMessage(active, text, replyTo?.msgid).catch(() => {});
    replyTo = null;
    stopTyping();
    composer = "";
  }

  function composerKey(e: KeyboardEvent) {
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
  function autofocus(node: HTMLTextAreaElement) {
    node.focus();
    node.selectionStart = node.selectionEnd = node.value.length;
  }
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
  const QUICK_EMOJI = ["👍", "❤️", "😂", "🎉", "😮", "😢", "🔥", "👀"];
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

  // ---- markdown (Phase 4) ----
  // Inline-only, escape-first: safe to feed {@html} because HTML is neutralised
  // before any markdown token is turned back into a tag.
  const escapeHtml = (s: string) =>
    s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  function renderMd(text: string): string {
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
    return s;
  }

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
    if (!active.startsWith("#")) return;
    if (typingChannel && typingChannel !== active) stopTyping();
    if (!typingChannel) {
      typingChannel = active;
      weft.typing(active, true).catch(() => {});
    }
    clearTimeout(typingStop);
    typingStop = setTimeout(stopTyping, 4000);
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

  // Viewing a channel clears its unread badge and advances the read marker
  // (MARK, synced across our devices — §9.7).
  $effect(() => {
    const ch = activeChannel;
    if (!ch) return;
    ch.unread = false;
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
  function joinNamespace(name: string) {
    weft.nsJoin(name).catch(() => {});
    weft.channels(name).catch(() => {}); // fetch its category layout
    discoverOpen = false;
  }
  function joinNamespaceInput() {
    const n = discoverName.trim().replace(/^@?/, "");
    discoverName = "";
    if (n) joinNamespace(n);
  }
  function doRedeem() {
    const t = redeemInput.trim();
    redeemInput = "";
    if (t) weft.inviteRedeem(t).catch(() => {});
    discoverOpen = false;
  }
  async function createNamespace() {
    const name = createName.trim().replace(/^@/, "");
    if (!name) return;
    createName = "";
    try {
      // Root keypair is generated + stored on-device (secret never leaves the
      // backend); then a default channel so the namespace is usable at once.
      await weft.nsCreate(network, name, createVis);
      await weft.channelCreate(`#${name}/general`);
      await weft.join(`#${name}/general`);
      discoverOpen = false;
    } catch (e) {
      authError = String(e);
    }
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

  // Reporting
  function openReport(m: Msg) {
    if (!m.msgid) return;
    reportTarget = m;
    reportCategory = "spam";
    reportNote = "";
    reportScope = nsOf(active) || activeServer ? "ns" : "net";
  }
  function submitReport() {
    if (reportTarget?.msgid)
      weft.report(reportTarget.msgid, reportCategory, reportScope, reportNote || undefined).catch(() => {});
    reportTarget = null;
  }
  function openReports() {
    reportsOpen = true;
    reportQueue = {};
    weft.reportsList(activeServer ? `ns:${activeServer}` : "*").catch(() => {});
  }

  // Roles
  function openRoles(member: string) {
    rolesTarget = member;
    roleScope = scopesFor()[0];
    roleCap = "send";
  }

  // Invites
  function mintInvite() {
    weft.inviteMint(scopesFor()[0]).catch(() => {});
  }

  // Namespace admin
  function openNsSettings() {
    const meta = discovered[activeServer];
    nsTitle = meta?.title ?? "";
    nsDesc = meta?.description ?? "";
    nsVis = meta?.visibility ?? "public";
    nsDelegSubject = "";
    nsDelegCaps = "mute,kick";
    nsNewOwner = "";
    nsRecKeys = "";
    nsSettingsOpen = true;
  }
  function saveNsMeta() {
    if (nsTitle.trim()) weft.nsMeta(activeServer, "title", nsTitle.trim()).catch(() => {});
    if (nsDesc.trim()) weft.nsMeta(activeServer, "description", nsDesc.trim()).catch(() => {});
    weft.nsVisibility(activeServer, nsVis).catch(() => {});
  }
  function doDelegate() {
    const s = nsDelegSubject.trim();
    if (s && nsDelegCaps.trim()) weft.nsDelegate(activeServer, s, nsDelegCaps.trim()).catch(() => {});
  }
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
    const un = weft.onWeft(handle);
    return () => {
      un.then((f) => f());
    };
  });
</script>

{#if status !== "online"}
  <!-- ================= CONNECT / LOGIN / REGISTER ================= -->
  <div class="connect-screen">
    <form class="connect-card" onsubmit={(e) => { e.preventDefault(); doConnect(); }}>
      <h1>WEFT</h1>
      <p class="sub">{mode === "login" ? "log in to a network" : "register a new account"}</p>

      <div style="display:flex;gap:8px;margin-bottom:4px">
        <button type="button" class="channel-item" style="justify-content:center;{mode === 'login' ? 'color:var(--text-primary);background:var(--bg-panel-raised)' : ''}" onclick={() => (mode = "login")}>Log in</button>
        <button type="button" class="channel-item" style="justify-content:center;{mode === 'register' ? 'color:var(--text-primary);background:var(--bg-panel-raised)' : ''}" onclick={() => (mode = "register")}>Register</button>
      </div>

      <label for="host">Network</label>
      <input id="host" bind:value={host} placeholder="127.0.0.1:4433" autocomplete="off" />
      <label for="acct">Account</label>
      <input id="acct" bind:value={formAccount} placeholder="ada" autocomplete="off" />
      <label for="pw">Password</label>
      <input id="pw" type="password" bind:value={formPassword} placeholder={mode === "register" ? "min 12 characters" : "your password"} autocomplete="off" />

      <button type="submit" disabled={status === "connecting" || !formAccount.trim()}>
        {status === "connecting" ? "connecting…" : mode === "login" ? "Log in" : "Create account"}
      </button>
      {#if authError}<div class="err">{authError}</div>{/if}
    </form>
  </div>
{:else}
  <!-- ================= MAIN APP ================= -->
  <div class="app" class:members-collapsed={!membersVisible}>
    <!-- COMMUNITY RAIL -->
    <nav class="warp-rail" aria-label="Networks">
      <button class="rail-home" class:active={homeView} title="Direct messages" aria-label="Direct messages" onclick={() => (homeView = true)}>
        <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><path d="M21 11.5a8.38 8.38 0 0 1-.9 3.8 8.5 8.5 0 0 1-7.6 4.7 8.38 8.38 0 0 1-3.8-.9L3 21l1.9-5.7a8.38 8.38 0 0 1-.9-3.8 8.5 8.5 0 0 1 4.7-7.6 8.38 8.38 0 0 1 3.8-.9h.5a8.48 8.48 0 0 1 8 8v.5z" /></svg>
      </button>
      <div class="rail-divider"></div>
      <div class="rail-communities">
        <div class="comm-tile" class:active={!homeView && activeServer === ""} title={network}>
          <button onclick={() => selectServer("")}>{initials(network)}</button>
          <span class="trust-mark signed" title="Connected network"></span>
        </div>
        {#each serverNamespaces as ns (ns)}
          <div class="comm-tile" class:active={!homeView && activeServer === ns} title={ns}>
            <button onclick={() => selectServer(ns)}>{initials(ns)}</button>
          </div>
        {/each}
      </div>
      <button class="rail-add" title="Discover namespaces" aria-label="Discover namespaces" onclick={openDiscover}>
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M12 5v14M5 12h14" /></svg>
      </button>
    </nav>

    <!-- SIDEBAR -->
    <aside class="sidebar">
      <div class="sidebar-header">
        {#if homeView}
          <p class="comm-name">Direct Messages</p>
        {:else}
          <p class="comm-name">{activeNsMeta?.title || activeServer || network}</p>
          <div class="comm-origin">
            <span class="origin-dot"></span>
            <span>{activeServer ? `namespace · ${network}` : `${network} · connected`}</span>
          </div>
          {#if activeServer}
            <button class="hdr-gear" title="Namespace settings" aria-label="Namespace settings" onclick={openNsSettings}>
              <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><circle cx="12" cy="12" r="3" /><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" /></svg>
            </button>
          {/if}
        {/if}
      </div>
      {#if homeView}
        <div class="channel-scroll">
          {#each dmList as ch (ch.name)}
            <button class="channel-item dm" class:active={ch.name === active} onclick={() => (active = ch.name)}>
              <span class="avatar sm">{initials(peerOf(ch.name))}</span>
              <span>{peerOf(ch.name)}</span>
              <span class={dotClass(peerOf(ch.name))}></span>
            </button>
          {/each}
          {#if !dmList.length}
            <div class="empty-hint">No conversations yet.<br />Message someone below.</div>
          {/if}
        </div>
        <div class="sidebar-join">
          <input
            bind:value={dmInput}
            placeholder="message @user…"
            onkeydown={(e) => e.key === "Enter" && startDm()}
          />
        </div>
      {:else}
        <div class="channel-scroll">
          {#each channelGroups as group (group.category)}
            <div class="retention-group">
              <div class="retention-label">{group.category}</div>
              {#each group.list as ch (ch.name)}
                {@const meta = retentionMeta[ch.retention]}
                <button class="channel-item" class:active={ch.name === active} class:unread={ch.unread} onclick={() => (active = ch.name)}>
                  <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M4 9h16M4 15h16M10 3 8 21M16 3l-2 18" /></svg>
                  <span class="ci-name">{chanShort(ch.name)}</span>
                  {#if ch.unread}<span class="unread-dot"></span>{/if}
                  <span class="dot {meta.cls} chan-ret" title={meta.label}></span>
                </button>
              {/each}
            </div>
          {/each}
          {#if !channelGroups.length}
            <div class="empty-hint">No channels yet.<br />Join one below.</div>
          {/if}
        </div>
        <div class="sidebar-join">
          <input
            bind:value={joinInput}
            placeholder="join #channel or namespace…"
            onkeydown={(e) => e.key === "Enter" && doJoin()}
          />
        </div>
      {/if}
      <div class="sidebar-user">
        <button class="avatar status-avatar" title="Set status" onclick={() => (statusMenu = !statusMenu)}>
          {initials(account)}
          <span class="dot {myStatus} corner"></span>
        </button>
        <div class="who">
          <div class="name">{account}</div>
          <div class="key">{myStatus}</div>
        </div>
        {#if statusMenu}
          <div class="status-menu">
            {#each ["online", "away", "dnd", "invisible"] as s (s)}
              <button onclick={() => setStatus(s)}><span class="dot {s}"></span>{s}</button>
            {/each}
          </div>
        {/if}
      </div>
    </aside>

    <!-- MAIN -->
    <main class="main">
      <div class="chat-topbar">
        {#if activeChannel && activeIsDm}
          <div class="chan-title">
            <span class={dotClass(peerOf(active))}></span>
            <span>{peerOf(active)}</span>
          </div>
          <div class="topic">{presence[peerOf(active)] ?? "offline"}</div>
        {:else if activeChannel}
          {@const meta = retentionMeta[activeChannel.retention]}
          <div class="chan-title">
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M4 9h16M4 15h16M10 3 8 21M16 3l-2 18" /></svg>
            <span>{chanShort(activeChannel.name)}</span>
          </div>
          <div class="topic">{activeChannel.topic ?? ""}</div>
          <div class="status-chip">
            <span style="display:flex;color:var(--{meta.cls})"><svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7">{@html meta.icon}</svg></span>{meta.label}
          </div>
        {:else}
          <div class="chan-title"><span>no channel</span></div>
          <div class="topic"></div>
        {/if}
        <div class="topbar-actions">
          {#if activeChannel && !activeIsDm}
            <button class="icon-btn" title="Invite" aria-label="Invite" onclick={mintInvite}>
              <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2" /><circle cx="9" cy="7" r="4" /><line x1="19" y1="8" x2="19" y2="14" /><line x1="22" y1="11" x2="16" y2="11" /></svg>
            </button>
            <button class="icon-btn" title="Reports queue" aria-label="Reports queue" onclick={openReports}>
              <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M4 15s1-1 4-1 5 2 8 2 4-1 4-1V3s-1 1-4 1-5-2-8-2-4 1-4 1z" /><line x1="4" y1="22" x2="4" y2="15" /></svg>
            </button>
            <button class="icon-btn" title="Leave channel" aria-label="Leave channel" onclick={() => weft.part(active).catch(() => {})}>
              <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4" /><path d="m16 17 5-5-5-5" /><path d="M21 12H9" /></svg>
            </button>
          {/if}
          <button class="icon-btn" title="Toggle member list" aria-label="Toggle member list" onclick={() => (membersVisible = !membersVisible)}>
            <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2" /><circle cx="9" cy="7" r="4" /><path d="M23 21v-2a4 4 0 0 0-3-3.87" /><path d="M16 3.13a4 4 0 0 1 0 7.75" /></svg>
          </button>
        </div>
      </div>

      <div class="message-scroll" bind:this={scrollEl} onscroll={onScroll}>
        {#if activeChannel}
          {#if loadingHistory === active}
            <div class="day-sep">loading history…</div>
          {:else if activeChannel.truncated}
            <div class="day-sep">older messages have expired</div>
          {:else if activeChannel.historyLoaded && !activeChannel.hasMore}
            <div class="day-sep">beginning of {activeChannel.name}</div>
          {/if}
          {#each activeChannel.messages as m (m.key)}
            {#if m.system}
              <div class="msg-group"><div style="width:34px;flex-shrink:0"></div><div class="msg-body"><div class="msg-line system">{m.body}</div></div></div>
            {:else}
              <div class="msg-group" id="msg-{m.key}">
                <div class="avatar">{initials(m.author)}</div>
                <div class="msg-body">
                  {#if m.replyTo}
                    {@const rep = activeChannel.messages.find((x) => x.msgid === m.replyTo)}
                    <button class="reply-quote" onclick={() => jumpTo(m.replyTo)}>
                      <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M9 17 4 12l5-5" /><path d="M20 18v-2a4 4 0 0 0-4-4H4" /></svg>
                      {#if rep}<span class="rq-author">{rep.author}</span><span class="rq-body">{rep.body.slice(0, 90)}</span>{:else}<span class="rq-body">an earlier message</span>{/if}
                    </button>
                  {/if}
                  <div class="msg-meta">
                    <span class="author">{m.author}</span>
                    {#if m.own}<span class="cap-badge owner">you</span>{/if}
                    <span class="time">{m.time}</span>
                  </div>
                  {#if editingKey === m.key}
                    <textarea
                      class="edit-box"
                      rows="1"
                      bind:value={editDraft}
                      onkeydown={(e) => editKey(e, m)}
                      use:autofocus
                    ></textarea>
                    <div class="edit-hint">escape to <button class="linkish" onclick={cancelEdit}>cancel</button> · enter to <button class="linkish" onclick={() => saveEdit(m)}>save</button></div>
                  {:else}
                    <div class="msg-line">{#if m.md}{@html renderMd(m.body)}{:else}{m.body}{/if}{#if m.edited}<span class="edited-tag" title="edited">(edited)</span>{/if}</div>
                  {/if}
                  {#if m.reactions && Object.keys(m.reactions).length}
                    <div class="reactions">
                      {#each Object.entries(m.reactions) as [emoji, r] (emoji)}
                        <button class="reaction" class:mine={r.mine} onclick={() => toggleReaction(m, emoji)}>
                          <span>{emoji}</span><span class="count">{r.count}</span>
                        </button>
                      {/each}
                    </div>
                  {/if}
                </div>
                {#if m.msgid && editingKey !== m.key}
                  <div class="msg-actions">
                    <button class="msg-act" title="React" aria-label="React" onclick={() => (pickerKey = pickerKey === m.key ? null : m.key)}>
                      <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><circle cx="12" cy="12" r="9" /><path d="M8 14s1.5 2 4 2 4-2 4-2" /><path d="M9 9h.01M15 9h.01" /></svg>
                    </button>
                    <button class="msg-act" title="Reply" aria-label="Reply" onclick={() => (replyTo = m)}>
                      <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M9 17 4 12l5-5" /><path d="M20 18v-2a4 4 0 0 0-4-4H4" /></svg>
                    </button>
                    {#if m.own}
                      <button class="msg-act" title="Edit" aria-label="Edit" onclick={() => startEdit(m)}>
                        <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M12 20h9" /><path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4Z" /></svg>
                      </button>
                      <button class="msg-act danger" title="Delete" aria-label="Delete" onclick={() => doDelete(m)}>
                        <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M3 6h18M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2m2 0v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6" /></svg>
                      </button>
                    {:else}
                      <button class="msg-act" title="Report" aria-label="Report" onclick={() => openReport(m)}>
                        <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M4 15s1-1 4-1 5 2 8 2 4-1 4-1V3s-1 1-4 1-5-2-8-2-4 1-4 1z" /><line x1="4" y1="22" x2="4" y2="15" /></svg>
                      </button>
                    {/if}
                  </div>
                  {#if pickerKey === m.key}
                    <div class="emoji-picker">
                      {#each QUICK_EMOJI as emoji (emoji)}
                        <button class="emoji-opt" onclick={() => toggleReaction(m, emoji)}>{emoji}</button>
                      {/each}
                    </div>
                  {/if}
                {/if}
              </div>
            {/if}
          {/each}
        {:else}
          <div class="empty-hint">Join a channel to start talking.</div>
        {/if}
      </div>

      <div class="composer-wrap">
        {#if replyTo}
          <div class="reply-bar">
            <span>replying to <b>{replyTo.author}</b></span>
            <button class="linkish" onclick={() => (replyTo = null)} aria-label="Cancel reply">✕</button>
          </div>
        {/if}
        <div class="composer">
          <textarea
            rows="1"
            placeholder={active ? `Message ${active}…` : "Join a channel first"}
            disabled={!active}
            bind:value={composer}
            onkeydown={composerKey}
            oninput={onComposerInput}
          ></textarea>
          <button class="icon-btn" title="Send" aria-label="Send message" onclick={doSend}>
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M22 2 11 13" /><path d="M22 2 15 22l-4-9-9-4 20-7z" /></svg>
          </button>
        </div>
        <div class="composer-hint">
          {#if typingLabel}
            <span class="typing">{typingLabel}</span>
          {:else}
            <span><span class="k">Enter</span> send</span>
            <span><span class="k">Shift+Enter</span> newline</span>
          {/if}
        </div>
      </div>
    </main>

    <!-- MEMBERS -->
    <aside class="members">
      {#if activeChannel && !activeIsDm}
        <div class="member-group-label">Members — {activeChannel.members.length}</div>
        {#each activeChannel.members as m (m.name)}
          <div class="member-row">
            <div class="avatar">{initials(m.name)}<span class="origin-flag {m.origin}"></span></div>
            {#if m.name !== account}
              <button class="mname mlink" onclick={() => openDm(m.name)}><span class={dotClass(m.name)}></span>{m.name}</button>
            {:else}
              <span class="mname"><span class="dot {myStatus}"></span>{m.name}</span>
            {/if}
            {#if m.name !== account}
              <div class="member-actions">
                <button class="mod-btn" title="Message {m.name}" aria-label="Message {m.name}" onclick={() => openDm(m.name)}>
                  <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" /></svg>
                </button>
                <button class="mod-btn" title="Roles for {m.name}" aria-label="Roles for {m.name}" onclick={() => openRoles(m.name)}>
                  <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6z" /><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" /></svg>
                </button>
                <button class="mod-btn" title="Mute {m.name}" aria-label="Mute {m.name}" onclick={() => moderate("mute", m.name)}>
                  <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M11 5 6 9H2v6h4l5 4V5z" /><line x1="23" y1="9" x2="17" y2="15" /><line x1="17" y1="9" x2="23" y2="15" /></svg>
                </button>
                <button class="mod-btn danger" title="Ban {m.name}" aria-label="Ban {m.name}" onclick={() => moderate("ban", m.name)}>
                  <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><circle cx="12" cy="12" r="10" /><line x1="4.9" y1="4.9" x2="19.1" y2="19.1" /></svg>
                </button>
              </div>
            {/if}
          </div>
        {/each}
      {/if}
    </aside>

    {#if discoverOpen}
      <div class="modal-wrap">
        <button class="modal-backdrop" aria-label="Close" onclick={() => (discoverOpen = false)}></button>
        <div class="modal" role="dialog" aria-modal="true">
          <div class="modal-head">
            <h2>Discover namespaces</h2>
            <button class="linkish" aria-label="Close" onclick={() => (discoverOpen = false)}>✕</button>
          </div>
          <div class="modal-join">
            <input
              bind:value={discoverName}
              placeholder="join a namespace by name…"
              onkeydown={(e) => e.key === "Enter" && joinNamespaceInput()}
            />
            <button onclick={joinNamespaceInput}>Join</button>
          </div>
          <div class="modal-join">
            <input
              bind:value={redeemInput}
              placeholder="redeem an invite link…"
              onkeydown={(e) => e.key === "Enter" && doRedeem()}
            />
            <button onclick={doRedeem}>Redeem</button>
          </div>
          <div class="modal-join">
            <input
              bind:value={createName}
              placeholder="create a namespace…"
              onkeydown={(e) => e.key === "Enter" && createNamespace()}
            />
            <select bind:value={createVis} aria-label="Visibility">
              <option value="public">public</option>
              <option value="unlisted">unlisted</option>
              <option value="private">private</option>
            </select>
            <button onclick={createNamespace}>Create</button>
          </div>
          <div class="modal-list">
            {#each Object.values(discovered) as ns (ns.name)}
              <div class="ns-card">
                <div class="ns-info">
                  <div class="ns-name">{ns.title || ns.name}</div>
                  <div class="ns-desc">
                    {ns.description || `@${ns.name}`} · {ns.visibility}{ns.owner ? ` · ${ns.owner}` : ""}
                  </div>
                </div>
                <button onclick={() => joinNamespace(ns.name)}>Join</button>
              </div>
            {:else}
              <div class="empty-hint">No public namespaces found.</div>
            {/each}
            {#if discoverCursor}
              <button class="linkish load-more" onclick={() => weft.discover(discoverCursor ?? undefined)}>Load more…</button>
            {/if}
          </div>
        </div>
      </div>
    {/if}

    {#if reportTarget}
      <div class="modal-wrap">
        <button class="modal-backdrop" aria-label="Close" onclick={() => (reportTarget = null)}></button>
        <div class="modal" role="dialog" aria-modal="true">
          <div class="modal-head">
            <h2>Report message</h2>
            <button class="linkish" aria-label="Close" onclick={() => (reportTarget = null)}>✕</button>
          </div>
          <p class="modal-sub">from <b>{reportTarget.author}</b> — “{reportTarget.body.slice(0, 80)}”</p>
          <label class="fld">Category
            <select bind:value={reportCategory}>
              {#each REPORT_CATEGORIES as c (c)}<option value={c}>{c}</option>{/each}
            </select>
          </label>
          <label class="fld">Route to
            <select bind:value={reportScope}>
              <option value="ns">namespace moderators</option>
              <option value="net">network operators</option>
            </select>
          </label>
          <label class="fld">Note (optional)
            <input bind:value={reportNote} placeholder="context for the moderators…" />
          </label>
          <div class="modal-actions">
            <button class="linkish" onclick={() => (reportTarget = null)}>Cancel</button>
            <button class="danger-btn" onclick={submitReport}>Submit report</button>
          </div>
        </div>
      </div>
    {/if}

    {#if rolesTarget}
      <div class="modal-wrap">
        <button class="modal-backdrop" aria-label="Close" onclick={() => (rolesTarget = null)}></button>
        <div class="modal" role="dialog" aria-modal="true">
          <div class="modal-head">
            <h2>Roles — {rolesTarget}</h2>
            <button class="linkish" aria-label="Close" onclick={() => (rolesTarget = null)}>✕</button>
          </div>
          <label class="fld">Scope
            <select bind:value={roleScope}>
              {#each scopesFor() as s (s)}<option value={s}>{s}</option>{/each}
            </select>
          </label>
          <label class="fld">Capability
            <select bind:value={roleCap}>
              {#each CAPS as c (c)}<option value={c}>{c}</option>{/each}
            </select>
          </label>
          <div class="modal-actions">
            <button class="danger-btn" onclick={() => rolesTarget && weft.revoke(rolesTarget, roleScope, roleCap).catch(() => {})}>Revoke</button>
            <button class="ok-btn" onclick={() => rolesTarget && weft.grant(rolesTarget, roleScope, roleCap).catch(() => {})}>Grant</button>
          </div>
          <p class="modal-sub">Grants are additive; revoking bumps the scope's epoch.</p>
        </div>
      </div>
    {/if}

    {#if reportsOpen}
      <div class="modal-wrap">
        <button class="modal-backdrop" aria-label="Close" onclick={() => (reportsOpen = false)}></button>
        <div class="modal" role="dialog" aria-modal="true">
          <div class="modal-head">
            <h2>Reports — {activeServer ? `ns:${activeServer}` : "network"}</h2>
            <button class="linkish" aria-label="Close" onclick={() => (reportsOpen = false)}>✕</button>
          </div>
          <div class="modal-list">
            {#each Object.values(reportQueue) as r (r.report_id)}
              <div class="ns-card report-card">
                <div class="ns-info">
                  <div class="ns-name">{r.category} <span class="rep-state {r.state}">{r.state}</span></div>
                  <div class="ns-desc">{r.report_id} · {r.msgid.slice(0, 16)}…{r.reporter ? ` · by ${r.reporter}` : ""}</div>
                </div>
                <select onchange={(e) => weft.reportsResolve(r.report_id, e.currentTarget.value).catch(() => {})}>
                  <option value="">resolve…</option>
                  {#each RESOLVE_ACTIONS as a (a)}<option value={a}>{a}</option>{/each}
                </select>
              </div>
            {:else}
              <div class="empty-hint">No open reports.</div>
            {/each}
          </div>
        </div>
      </div>
    {/if}

    {#if inviteLink}
      <div class="modal-wrap">
        <button class="modal-backdrop" aria-label="Close" onclick={() => (inviteLink = null)}></button>
        <div class="modal" role="dialog" aria-modal="true">
          <div class="modal-head">
            <h2>Invite link</h2>
            <button class="linkish" aria-label="Close" onclick={() => (inviteLink = null)}>✕</button>
          </div>
          <p class="modal-sub">Share this to let someone join:</p>
          <div class="modal-join">
            <input readonly value={inviteLink} />
            <button onclick={() => navigator.clipboard?.writeText(inviteLink ?? "")}>Copy</button>
          </div>
        </div>
      </div>
    {/if}

    {#if nsSettingsOpen}
      <div class="modal-wrap">
        <button class="modal-backdrop" aria-label="Close" onclick={() => (nsSettingsOpen = false)}></button>
        <div class="modal ns-settings" role="dialog" aria-modal="true">
          <div class="modal-head">
            <h2>Namespace settings — {activeServer}</h2>
            <button class="linkish" aria-label="Close" onclick={() => (nsSettingsOpen = false)}>✕</button>
          </div>
          <div class="modal-list">
            {#if activeNsMeta?.recovery_eta}
              <div class="ns-card recovery-pending">
                <div class="ns-info">
                  <div class="ns-name">⚠ Recovery pending (rung {activeNsMeta.recovery_rung})</div>
                  <div class="ns-desc">A root rotation is scheduled. As the live owner you can veto it.</div>
                </div>
                <button class="danger-btn" onclick={() => weft.nsRecoveryCancel(network, activeServer).catch((e) => (authError = String(e)))}>Cancel recovery</button>
              </div>
            {/if}

            <div class="settings-sec">
              <h3>Profile</h3>
              <label class="fld">Title <input bind:value={nsTitle} placeholder="display name" /></label>
              <label class="fld">Description <input bind:value={nsDesc} placeholder="what's this namespace about" /></label>
              <label class="fld">Visibility
                <select bind:value={nsVis}>
                  <option value="public">public</option>
                  <option value="unlisted">unlisted</option>
                  <option value="private">private</option>
                </select>
              </label>
              <div class="modal-actions"><button class="ok-btn" onclick={saveNsMeta}>Save profile</button></div>
            </div>

            <div class="settings-sec">
              <h3>Delegate roles</h3>
              <label class="fld">Account <input bind:value={nsDelegSubject} placeholder="account" /></label>
              <label class="fld">Capabilities <input bind:value={nsDelegCaps} placeholder="mute,kick,…" /></label>
              <div class="modal-actions"><button class="ok-btn" onclick={doDelegate}>Delegate</button></div>
            </div>

            <div class="settings-sec">
              <h3>Recovery quorum (§2.4)</h3>
              <label class="fld">Threshold M <input type="number" min="1" bind:value={nsRecM} /></label>
              <label class="fld">Quorum keys (comma-separated b64 pubkeys)
                <input bind:value={nsRecKeys} placeholder="key1,key2,key3" />
              </label>
              <div class="modal-actions"><button class="ok-btn" onclick={() => nsRecKeys.trim() && weft.nsRecoverySet(activeServer, nsRecM, nsRecKeys.trim()).catch(() => {})}>Set recovery quorum</button></div>
            </div>

            <div class="settings-sec danger-sec">
              <h3>Danger zone</h3>
              <label class="fld">Transfer ownership to <input bind:value={nsNewOwner} placeholder="account" /></label>
              <div class="modal-actions">
                <button class="danger-btn" onclick={deleteNamespace}>Delete namespace</button>
                <button class="danger-btn" onclick={doTransfer}>Transfer (root-signed)</button>
              </div>
            </div>
          </div>
        </div>
      </div>
    {/if}
  </div>
{/if}
