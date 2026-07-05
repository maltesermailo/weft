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
    author: string;
    body: string;
    time: string;
    own: boolean;
    system?: boolean;
  };
  type Channel = {
    name: string;
    retention: string;
    messages: Msg[];
    members: Member[];
  };

  let channels = $state<Record<string, Channel>>({});
  let active = $state("");
  let joinInput = $state("");
  let composer = $state("");
  let membersVisible = $state(true);
  let scrollEl: HTMLDivElement | null = $state(null);

  const retentionMeta: Record<string, { label: string; cls: string; icon: string }> = {
    ephemeral: { label: "Ephemeral", cls: "ephemeral", icon: '<circle cx="12" cy="12" r="8" stroke-dasharray="3 3"/>' },
    retained: { label: "Retained", cls: "retained", icon: '<rect x="4" y="4" width="16" height="16" rx="2"/><path d="M4 10h16"/>' },
    permanent: { label: "Permanent", cls: "permanent", icon: '<rect x="4" y="4" width="16" height="16" rx="2" fill="currentColor" stroke="none"/>' },
    e2ee: { label: "E2EE · MLS", cls: "e2ee", icon: '<rect x="5" y="11" width="14" height="9" rx="2"/><path d="M8 11V7a4 4 0 0 1 8 0v4"/>' },
  };
  const retentionOrder = ["e2ee", "permanent", "retained", "ephemeral"];

  const initials = (s: string) => s.replace(/[^a-z0-9]/gi, "").slice(0, 2).toUpperCase() || "··";
  const clock = () => {
    const d = new Date();
    return `${`${d.getHours()}`.padStart(2, "0")}:${`${d.getMinutes()}`.padStart(2, "0")}`;
  };
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

  let activeChannel = $derived(active ? channels[active] : undefined);
  let groupedChannels = $derived(
    retentionOrder
      .map((r) => ({ retention: r, list: Object.values(channels).filter((c) => c.retention === r) }))
      .filter((g) => g.list.length),
  );

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
          if (e.user === account && !active) active = e.channel;
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
        if (!e.target.startsWith("#")) break; // DMs land later
        ensureChannel(e.target).messages.push({
          author: e.sender,
          body: e.body,
          time: clock(),
          own: e.own,
        });
        break;
      }
      case "edited":
        if (channels[e.target]) {
          ensureChannel(e.target).messages.push({
            author: e.sender,
            body: `(edited) ${e.body}`,
            time: clock(),
            own: false,
          });
        }
        break;
      case "error":
        if (activeChannel) activeChannel.messages.push({ author: "", body: `${e.code}: ${e.text}`, time: clock(), own: false, system: true });
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
    let name = joinInput.trim();
    if (!name) return;
    if (!name.startsWith("#")) name = `#${name}`;
    weft.join(name).catch((e) => (authError = String(e)));
    joinInput = "";
  }

  function sys(body: string) {
    if (activeChannel) activeChannel.messages.push({ author: "", body, time: clock(), own: false, system: true });
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
        if (active) weft.sendRaw(`PART ${active}`).catch(() => {});
        break;
      case "help":
        sys("/ban <user> · /kick <user> · /mute <user> · /unmute <user> · /join #chan · /part");
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
    weft.sendMessage(active, text).catch(() => {});
    composer = "";
  }

  function composerKey(e: KeyboardEvent) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      doSend();
    }
  }

  // Auto-scroll to the newest message when the active channel changes/grows.
  $effect(() => {
    activeChannel?.messages.length;
    active;
    if (scrollEl) queueMicrotask(() => (scrollEl!.scrollTop = scrollEl!.scrollHeight));
  });

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
      <button class="rail-home" title="Direct messages" aria-label="Direct messages">
        <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><path d="M21 11.5a8.38 8.38 0 0 1-.9 3.8 8.5 8.5 0 0 1-7.6 4.7 8.38 8.38 0 0 1-3.8-.9L3 21l1.9-5.7a8.38 8.38 0 0 1-.9-3.8 8.5 8.5 0 0 1 4.7-7.6 8.38 8.38 0 0 1 3.8-.9h.5a8.48 8.48 0 0 1 8 8v.5z" /></svg>
      </button>
      <div class="rail-divider"></div>
      <div class="rail-communities">
        <div class="comm-tile active" title={network}>
          <button>{initials(network)}</button>
          <span class="trust-mark signed" title="Connected network"></span>
        </div>
      </div>
      <button class="rail-add" title="Join or create a network" aria-label="Join or create a network">
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M12 5v14M5 12h14" /></svg>
      </button>
    </nav>

    <!-- SIDEBAR -->
    <aside class="sidebar">
      <div class="sidebar-header">
        <p class="comm-name">{network}</p>
        <div class="comm-origin"><span class="origin-dot"></span><span>{network} · connected</span></div>
      </div>
      <div class="channel-scroll">
        {#each groupedChannels as group (group.retention)}
          {@const meta = retentionMeta[group.retention]}
          <div class="retention-group">
            <div class="retention-label"><span class="dot {meta.cls}"></span>{meta.label}</div>
            {#each group.list as ch (ch.name)}
              <button class="channel-item" class:active={ch.name === active} onclick={() => (active = ch.name)}>
                <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M4 9h16M4 15h16M10 3 8 21M16 3l-2 18" /></svg>
                <span>{ch.name.replace(/^#/, "")}</span>
              </button>
            {/each}
          </div>
        {/each}
        {#if !Object.keys(channels).length}
          <div class="empty-hint">No channels yet.<br />Join one below.</div>
        {/if}
      </div>
      <div class="sidebar-join">
        <input
          bind:value={joinInput}
          placeholder="join #channel…"
          onkeydown={(e) => e.key === "Enter" && doJoin()}
        />
      </div>
      <div class="sidebar-user">
        <div class="avatar">{initials(account)}</div>
        <div class="who">
          <div class="name">{account}</div>
          <div class="key">{account}@{network}</div>
        </div>
      </div>
    </aside>

    <!-- MAIN -->
    <main class="main">
      <div class="chat-topbar">
        {#if activeChannel}
          {@const meta = retentionMeta[activeChannel.retention]}
          <div class="chan-title">
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M4 9h16M4 15h16M10 3 8 21M16 3l-2 18" /></svg>
            <span>{activeChannel.name.replace(/^#/, "")}</span>
          </div>
          <div class="topic"></div>
          <div class="status-chip">
            <span style="display:flex;color:var(--{meta.cls})"><svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7">{@html meta.icon}</svg></span>{meta.label}
          </div>
        {:else}
          <div class="chan-title"><span>no channel</span></div>
          <div class="topic"></div>
        {/if}
        <div class="topbar-actions">
          <button class="icon-btn" title="Toggle member list" aria-label="Toggle member list" onclick={() => (membersVisible = !membersVisible)}>
            <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2" /><circle cx="9" cy="7" r="4" /><path d="M23 21v-2a4 4 0 0 0-3-3.87" /><path d="M16 3.13a4 4 0 0 1 0 7.75" /></svg>
          </button>
        </div>
      </div>

      <div class="message-scroll" bind:this={scrollEl}>
        {#if activeChannel}
          <div class="day-sep">TODAY</div>
          {#each activeChannel.messages as m, i (i)}
            {#if m.system}
              <div class="msg-group"><div style="width:34px;flex-shrink:0"></div><div class="msg-body"><div class="msg-line system">{m.body}</div></div></div>
            {:else}
              <div class="msg-group">
                <div class="avatar">{initials(m.author)}</div>
                <div class="msg-body">
                  <div class="msg-meta">
                    <span class="author">{m.author}</span>
                    {#if m.own}<span class="cap-badge owner">you</span>{/if}
                    <span class="time">{m.time}</span>
                  </div>
                  <div class="msg-line">{m.body}</div>
                </div>
              </div>
            {/if}
          {/each}
        {:else}
          <div class="empty-hint">Join a channel to start talking.</div>
        {/if}
      </div>

      <div class="composer-wrap">
        <div class="composer">
          <textarea
            rows="1"
            placeholder={active ? `Message ${active}…` : "Join a channel first"}
            disabled={!active}
            bind:value={composer}
            onkeydown={composerKey}
          ></textarea>
          <button class="icon-btn" title="Send" aria-label="Send message" onclick={doSend}>
            <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M22 2 11 13" /><path d="M22 2 15 22l-4-9-9-4 20-7z" /></svg>
          </button>
        </div>
        <div class="composer-hint">
          <span><span class="k">Enter</span> send</span>
          <span><span class="k">Shift+Enter</span> newline</span>
        </div>
      </div>
    </main>

    <!-- MEMBERS -->
    <aside class="members">
      {#if activeChannel}
        <div class="member-group-label">Members — {activeChannel.members.length}</div>
        {#each activeChannel.members as m (m.name)}
          <div class="member-row">
            <div class="avatar">{initials(m.name)}<span class="origin-flag {m.origin}"></span></div>
            <span class="mname">{m.name}</span>
            {#if m.name !== account}
              <div class="member-actions">
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
  </div>
{/if}
