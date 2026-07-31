<script lang="ts">
  import { userCtx } from "$lib/ui/ctxmenu.svelte";
  import { messageFriend } from "$lib/navigation/navigation";
  import { roster, friendLocalAccount, openGroupPicker, callUser } from "$lib/social/social.svelte";
  import { store } from "$lib/store/store.svelte";
  import { friendLabel } from "$lib/profile/profile.svelte";
  import { getApp } from "$lib/ui/context";
  import Avatar from "$lib/components/Avatar.svelte";
  const app = getApp();

  type Tab = "online" | "all" | "pending" | "add";
  let tab = $state<Tab>("online");

  // Local friends get a presence dot + avatar by handle; federated friends
  // render by their full `account@network` ref (no presence).
  const avatarAccount = (user: string) => friendLocalAccount(user) ?? user;
  const statusOf = (user: string) => {
    const acct = friendLocalAccount(user);
    return acct ? (store.accountOf(acct).presence ?? "offline") : "offline";
  };
  const isOnline = (user: string) => {
    const s = statusOf(user);
    return s !== "offline" && s !== "invisible";
  };
  const STATUS: Record<string, string> = {
    online: "Online",
    idle: "Idle",
    dnd: "Do Not Disturb",
    offline: "Offline",
    invisible: "Offline",
  };
  const subtitle = (user: string) => {
    const acct = friendLocalAccount(user);
    return acct ? (STATUS[statusOf(user)] ?? "Offline") : user;
  };

  const online = $derived(roster.friends.filter(isOnline));
  const pendingCount = $derived(roster.incoming.length);

  function onAddKey(e: KeyboardEvent) {
    if (e.key === "Enter") {
      e.preventDefault();
      app.addFriend();
    }
  }
</script>

<div class="fv">
  <!-- Topbar: title + tabs + new-group -->
  <header class="fv-top">
    <span class="fv-ttl">
      <svg width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2" /><circle cx="9" cy="7" r="4" /><path d="M23 21v-2a4 4 0 0 0-3-3.87M16 3.13a4 4 0 0 1 0 7.75" /></svg>
      Friends
    </span>
    <span class="fv-div"></span>
    <nav class="fv-tabs">
      <button class="fv-tab" class:on={tab === "online"} onclick={() => (tab = "online")}>Online</button>
      <button class="fv-tab" class:on={tab === "all"} onclick={() => (tab = "all")}>All</button>
      <button class="fv-tab" class:on={tab === "pending"} onclick={() => (tab = "pending")}>
        Pending{#if pendingCount}<span class="fv-badge">{pendingCount}</span>{/if}
      </button>
      <button class="fv-tab add" class:on={tab === "add"} onclick={() => (tab = "add")}>Add Friend</button>
    </nav>
    <button class="fv-icon" title="New group DM" aria-label="New group DM" onclick={openGroupPicker}>
      <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2" /><circle cx="9" cy="7" r="4" /><path d="M23 21v-2a4 4 0 0 0-3-3.87M16 3.13a4 4 0 0 1 0 7.75" /><line x1="20" y1="8" x2="20" y2="14" /><line x1="17" y1="11" x2="23" y2="11" /></svg>
    </button>
  </header>

  <div class="fv-body">
    {#if tab === "add"}
      <div class="fv-add-panel">
        <h3>Add Friend</h3>
        <p>Add a friend by their handle. Friends can live on other networks too — use <code>name@network</code>.</p>
        <div class="fv-add-wrap">
          <input
            placeholder="Enter a handle or account@network"
            bind:value={app.addFriendInput}
            onkeydown={onAddKey}
          />
          <button class="fv-add-btn" disabled={!app.addFriendInput.trim()} onclick={app.addFriend}>
            Send Friend Request
          </button>
        </div>
      </div>
    {:else if tab === "pending"}
      {#if roster.incoming.length}
        <div class="fv-count">Incoming — {roster.incoming.length}</div>
        {#each roster.incoming as user (user)}
          <div class="fv-row" oncontextmenu={(e) => userCtx(e, user)} role="listitem">
            <span class="fv-av"><Avatar account={avatarAccount(user)} /></span>
            <span class="fv-meta">
              <span class="fv-name">{friendLabel(user)}</span>
              <span class="fv-sub">Incoming friend request</span>
            </span>
            <span class="fv-acts">
              <button class="fv-act ok" title="Accept" aria-label="Accept" onclick={() => app.acceptFriend(user)}>
                <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><polyline points="20 6 9 17 4 12" /></svg>
              </button>
              <button class="fv-act no" title="Decline" aria-label="Decline" onclick={() => app.removeFriend(user)}>
                <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round"><line x1="18" y1="6" x2="6" y2="18" /><line x1="6" y1="6" x2="18" y2="18" /></svg>
              </button>
            </span>
          </div>
        {/each}
      {/if}
      {#if roster.outgoing.length}
        <div class="fv-count" style="margin-top:16px">Sent — {roster.outgoing.length}</div>
        {#each roster.outgoing as user (user)}
          <div class="fv-row" role="listitem">
            <span class="fv-av"><Avatar account={avatarAccount(user)} /></span>
            <span class="fv-meta">
              <span class="fv-name">{friendLabel(user)}</span>
              <span class="fv-sub">Outgoing friend request</span>
            </span>
            <span class="fv-acts">
              <button class="fv-act no" title="Cancel" aria-label="Cancel" onclick={() => app.removeFriend(user)}>
                <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round"><line x1="18" y1="6" x2="6" y2="18" /><line x1="6" y1="6" x2="18" y2="18" /></svg>
              </button>
            </span>
          </div>
        {/each}
      {/if}
      {#if !roster.incoming.length && !roster.outgoing.length}
        <div class="fv-empty">
          <svg width="72" height="72" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1"><path d="M22 16.92v3a2 2 0 0 1-2.18 2 19.79 19.79 0 0 1-8.63-3.07A19.5 19.5 0 0 1 3.07 9.8" /><path d="M1 1l22 22" /></svg>
          <p>No pending requests</p>
          <span>Sent and received friend requests show up here.</span>
        </div>
      {/if}
    {:else}
      <!-- online | all -->
      {@const list = tab === "online" ? online : roster.friends}
      {#if list.length}
        <div class="fv-count">{tab === "online" ? "Online" : "All Friends"} — {list.length}</div>
        {#each list as user (user)}
          <div class="fv-row" oncontextmenu={(e) => userCtx(e, user)} role="listitem">
            <span class="fv-av">
              <Avatar account={avatarAccount(user)} />
              {#if friendLocalAccount(user)}<span class="fv-dot {statusOf(user)}"></span>{/if}
            </span>
            <span class="fv-meta">
              <span class="fv-name">{friendLabel(user)}</span>
              <span class="fv-sub">{subtitle(user)}</span>
            </span>
            <span class="fv-acts">
              {#if friendLocalAccount(user)}
                <button class="fv-act" title="Message" aria-label="Message" onclick={() => messageFriend(user)}>
                  <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" /></svg>
                </button>
              {/if}
              <button class="fv-act call" title="Call" aria-label="Call" disabled={!!app.activeCall} onclick={() => callUser(user)}>
                <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 16.92v3a2 2 0 0 1-2.18 2 19.79 19.79 0 0 1-8.63-3.07 19.5 19.5 0 0 1-6-6 19.79 19.79 0 0 1-3.07-8.67A2 2 0 0 1 4.11 2h3a2 2 0 0 1 2 1.72 12.84 12.84 0 0 0 .7 2.81 2 2 0 0 1-.45 2.11L8.09 9.91a16 16 0 0 0 6 6l1.27-1.27a2 2 0 0 1 2.11-.45 12.84 12.84 0 0 0 2.81.7A2 2 0 0 1 22 16.92z" /></svg>
              </button>
              <button class="fv-act" title="More" aria-label="More" onclick={(e) => userCtx(e, user)}>
                <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><circle cx="12" cy="5" r="1" /><circle cx="12" cy="12" r="1" /><circle cx="12" cy="19" r="1" /></svg>
              </button>
            </span>
          </div>
        {/each}
      {:else}
        <div class="fv-empty">
          <svg width="72" height="72" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1"><path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2" /><circle cx="9" cy="7" r="4" /><path d="M23 21v-2a4 4 0 0 0-3-3.87M16 3.13a4 4 0 0 1 0 7.75" /></svg>
          <p>{tab === "online" ? "No one's around" : "No friends yet"}</p>
          <span>Add someone with <strong>Add Friend</strong> — they can be on another network too.</span>
        </div>
      {/if}
    {/if}
  </div>
</div>

<style>
  .fv {
    display: flex;
    flex-direction: column;
    height: 100%;
    min-height: 0;
  }
  .fv-top {
    display: flex;
    align-items: center;
    gap: 12px;
    height: 48px;
    padding: 0 16px;
    flex-shrink: 0;
    border-bottom: 1px solid var(--border-hair);
  }
  .fv-ttl {
    display: flex;
    align-items: center;
    gap: 9px;
    font-weight: 700;
    font-size: 15px;
    color: var(--text-primary);
  }
  .fv-ttl svg {
    color: var(--text-muted);
  }
  .fv-div {
    width: 1px;
    height: 22px;
    background: var(--border-hair-strong);
  }
  .fv-tabs {
    display: flex;
    gap: 2px;
  }
  .fv-tab {
    padding: 5px 10px;
    border-radius: var(--radius-md);
    border: none;
    background: none;
    font: inherit;
    font-size: 13px;
    font-weight: 500;
    color: var(--text-muted);
    cursor: pointer;
    display: inline-flex;
    align-items: center;
    gap: 5px;
    transition:
      background 0.1s,
      color 0.1s;
  }
  .fv-tab:hover {
    background: var(--bg-hover);
    color: var(--text-primary);
  }
  .fv-tab.on {
    background: var(--bg-hover);
    color: var(--text-primary);
  }
  .fv-tab.add {
    background: #3ba55d;
    color: #fff;
    font-weight: 600;
  }
  .fv-tab.add:hover,
  .fv-tab.add.on {
    background: #359553;
  }
  .fv-badge {
    background: var(--danger);
    color: #fff;
    font-size: 10px;
    font-weight: 700;
    border-radius: 10px;
    padding: 0 5px;
    line-height: 1.5;
  }
  .fv-icon {
    margin-left: auto;
    width: 32px;
    height: 32px;
    border-radius: var(--radius-md);
    border: none;
    background: none;
    color: var(--text-muted);
    cursor: pointer;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    transition:
      background 0.1s,
      color 0.1s;
  }
  .fv-icon:hover {
    background: var(--bg-hover);
    color: var(--text-primary);
  }
  .fv-body {
    flex: 1;
    overflow-y: auto;
    padding: 16px 20px;
    display: flex;
    flex-direction: column;
  }
  .fv-count {
    font-size: 12px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--text-muted);
    margin-bottom: 6px;
    padding: 0 4px;
  }
  .fv-row {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 9px 12px;
    border-radius: var(--radius-md);
    cursor: pointer;
    border-top: 1px solid var(--border-hair);
    transition: background 0.1s;
  }
  .fv-row:first-of-type {
    border-top: none;
  }
  .fv-row:hover {
    background: var(--bg-hover);
    border-top-color: transparent;
  }
  .fv-av {
    position: relative;
    flex-shrink: 0;
    width: 40px;
    height: 40px;
    display: flex;
    /* Centered initials fallback when there's no uploaded picture. */
    align-items: center;
    justify-content: center;
    border-radius: 50%;
    background: var(--accent, #5865f2);
    color: #fff;
    font-size: 15px;
    font-weight: 600;
    text-transform: uppercase;
  }
  .fv-av :global(img) {
    width: 40px;
    height: 40px;
    border-radius: 50%;
    object-fit: cover;
  }
  .fv-dot {
    position: absolute;
    bottom: -1px;
    right: -1px;
    width: 13px;
    height: 13px;
    border-radius: 50%;
    border: 3px solid var(--bg-void);
    background: #80848e;
  }
  .fv-dot.online {
    background: #3ba55d;
  }
  .fv-dot.idle {
    background: #f0b232;
  }
  .fv-dot.dnd {
    background: var(--danger);
  }
  .fv-meta {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
  }
  .fv-name {
    font-size: 14px;
    font-weight: 600;
    color: var(--text-primary);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .fv-sub {
    font-size: 12px;
    color: var(--text-muted);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .fv-acts {
    display: flex;
    gap: 6px;
    flex-shrink: 0;
  }
  .fv-act {
    width: 36px;
    height: 36px;
    border-radius: 50%;
    border: none;
    background: var(--bg-panel);
    color: var(--text-muted);
    cursor: pointer;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    transition:
      background 0.12s,
      color 0.12s;
    flex-shrink: 0;
  }
  .fv-act:hover {
    background: var(--bg-panel-raised);
    color: var(--text-primary);
  }
  .fv-act.call:hover {
    color: #3ba55d;
  }
  .fv-act.ok:hover {
    background: rgba(59, 165, 93, 0.15);
    color: #3ba55d;
  }
  .fv-act.no:hover {
    background: rgba(217, 104, 95, 0.15);
    color: var(--danger);
  }
  .fv-act:disabled {
    opacity: 0.4;
    cursor: default;
  }
  .fv-empty {
    flex: 1;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 12px;
    padding: 40px;
    text-align: center;
  }
  .fv-empty svg {
    color: var(--text-faint);
    opacity: 0.5;
  }
  .fv-empty p {
    font-size: 16px;
    font-weight: 600;
    color: var(--text-muted);
  }
  .fv-empty span {
    font-size: 13px;
    color: var(--text-faint);
    line-height: 1.5;
    max-width: 320px;
  }
  /* Add-friend panel */
  .fv-add-panel {
    max-width: 660px;
  }
  .fv-add-panel h3 {
    font-size: 20px;
    font-weight: 700;
    color: var(--text-primary);
    margin-bottom: 6px;
  }
  .fv-add-panel p {
    font-size: 13px;
    color: var(--text-muted);
    margin-bottom: 20px;
    line-height: 1.5;
  }
  .fv-add-panel code {
    font-family: var(--font-mono);
    font-size: 12px;
    color: var(--text-secondary);
  }
  .fv-add-wrap {
    display: flex;
    align-items: center;
    gap: 8px;
    background: var(--bg-panel);
    border: 1px solid var(--border-hair-strong);
    border-radius: 10px;
    padding: 4px 4px 4px 14px;
    transition: border-color 0.15s;
  }
  .fv-add-wrap:focus-within {
    border-color: var(--accent);
  }
  .fv-add-wrap input {
    flex: 1;
    background: none;
    border: none;
    outline: none;
    font: inherit;
    font-size: 14px;
    color: var(--text-primary);
    padding: 8px 0;
  }
  .fv-add-btn {
    background: var(--accent);
    color: #fff;
    border: none;
    border-radius: var(--radius-md);
    padding: 8px 16px;
    font: inherit;
    font-size: 13px;
    font-weight: 600;
    cursor: pointer;
    white-space: nowrap;
    transition: filter 0.12s;
  }
  .fv-add-btn:hover {
    filter: brightness(1.1);
  }
  .fv-add-btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
</style>
