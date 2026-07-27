<script lang="ts">
  import { fade } from "svelte/transition";
  import { getApp } from "$lib/context";
  import Avatar from "$lib/components/Avatar.svelte";
  const app = getApp();
  let { target, onclose }: { target: string; onclose: () => void } = $props();

  const isSelf = $derived(target === app.account);
  const handle = $derived(target.includes("@") ? target : `${target}@${app.network}`);
  const status = $derived(isSelf ? app.myStatus : (app.presence[target] ?? "offline"));
  const online = $derived(status !== "offline" && status !== "invisible");
  const bio = $derived(app.bioOf(target));
  const badge = $derived(app.badgeFor(target, app.active));

  const servers = $derived(app.mutualServers(target));
  const rel = $derived(app.friendState(target));

  const STATUS_LABEL: Record<string, string> = {
    online: "Online",
    idle: "Idle",
    away: "Away",
    dnd: "Do Not Disturb",
    busy: "Busy",
    offline: "Offline",
    invisible: "Offline",
  };

  type Tab = "servers" | "friends";
  let tab = $state<Tab>("servers");

  function message() {
    app.openDm(target);
    onclose();
  }
  function call() {
    app.callUser(handle);
    onclose();
  }
  function jumpServer(ns: string) {
    app.selectServer(ns);
    onclose();
  }
  function copyId() {
    navigator.clipboard?.writeText(handle);
    app.toast("Handle copied", "info");
  }
</script>

<div class="pm-wrap" transition:fade|global={{ duration: 150 }}>
  <button class="pm-backdrop" aria-label="Close" onclick={onclose}></button>
  <div class="pm-modal" role="dialog" aria-modal="true">
    <button class="pm-close" aria-label="Close" onclick={onclose}>
      <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M18 6 6 18M6 6l12 12" /></svg>
    </button>

    <!-- Left: identity card -->
    <div class="pm-card">
      <div class="pm-banner"></div>
      <div class="pm-avwrap">
        <div class="pm-av">
          <Avatar account={target} />
          <span class="pm-status {status}" class:on={online} title={STATUS_LABEL[status] ?? "Offline"}></span>
        </div>
      </div>

      <div class="pm-card-body">
        <div class="pm-nameline">
          <span class="pm-name">{app.displayName(target)}</span>
          {#if badge?.owner}<span class="cap-badge owner">owner</span>
          {:else if badge?.mod}<span class="cap-badge mod">mod</span>{/if}
        </div>
        <button class="pm-handle" title="Copy handle" onclick={copyId}>
          {handle}
          <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><rect x="9" y="9" width="13" height="13" rx="2" /><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" /></svg>
        </button>
        <div class="pm-statusline"><span class="pm-sdot {status}" class:on={online}></span>{STATUS_LABEL[status] ?? "Offline"}</div>
        {#if app.statusOf(target)}<div class="pm-custom-status">{app.statusOf(target)}</div>{/if}

        {#if !isSelf}
          <div class="pm-actions">
            <button class="pm-btn primary" onclick={message}>
              <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" /></svg>
              Message
            </button>
            {#if rel === "friends"}
              <button class="pm-btn" onclick={() => app.friendAction(target, "remove")}>Friends ✓</button>
            {:else if rel === "incoming"}
              <button class="pm-btn accent" onclick={() => app.friendAction(target, "accept")}>Accept request</button>
            {:else if rel === "outgoing"}
              <button class="pm-btn" onclick={() => app.friendAction(target, "remove")}>Requested</button>
            {:else}
              <button class="pm-icon" title="Add friend" aria-label="Add friend" onclick={() => app.friendAction(target, "add")}>
                <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2" /><circle cx="9" cy="7" r="4" /><line x1="19" y1="8" x2="19" y2="14" /><line x1="22" y1="11" x2="16" y2="11" /></svg>
              </button>
            {/if}
            <button class="pm-icon" title="Call" aria-label="Call" onclick={call}>
              <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 16.92v3a2 2 0 0 1-2.18 2 19.79 19.79 0 0 1-8.63-3.07 19.5 19.5 0 0 1-6-6 19.79 19.79 0 0 1-3.07-8.67A2 2 0 0 1 4.11 2h3a2 2 0 0 1 2 1.72 12.84 12.84 0 0 0 .7 2.81 2 2 0 0 1-.45 2.11L8.09 9.91a16 16 0 0 0 6 6l1.27-1.27a2 2 0 0 1 2.11-.45 12.84 12.84 0 0 0 2.81.7A2 2 0 0 1 22 16.92z" /></svg>
            </button>
          </div>
        {/if}

        {#if bio}
          <div class="pm-section">
            <div class="pm-label">About me</div>
            <p class="pm-bio">{bio}</p>
          </div>
        {/if}
      </div>
    </div>

    <!-- Right: mutual servers / friends -->
    <div class="pm-right">
      <div class="pm-tabs" role="tablist">
        <button class="pm-tab" role="tab" aria-selected={tab === "servers"} onclick={() => (tab = "servers")}>
          {servers.length} Mutual Server{servers.length === 1 ? "" : "s"}
        </button>
        <button class="pm-tab" role="tab" aria-selected={tab === "friends"} onclick={() => (tab = "friends")}>
          Mutual Friends
        </button>
      </div>

      {#if tab === "servers"}
        {#if servers.length}
          <div class="pm-list">
            {#each servers as ns (ns)}
              <button class="pm-server" onclick={() => jumpServer(ns)}>
                <span class="pm-server-icon">{app.initials(ns)}</span>
                <span class="pm-server-name">{ns}</span>
                <svg class="pm-chev" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="m9 18 6-6-6-6" /></svg>
              </button>
            {/each}
          </div>
        {:else}
          <div class="pm-empty">
            <div class="pm-empty-emoji">🪐</div>
            <p>No servers in common{isSelf ? "" : ` with ${app.displayName(target)}`}.</p>
          </div>
        {/if}
      {:else}
        <div class="pm-empty">
          <div class="pm-empty-emoji">🤝</div>
          <h3>Mutual friends coming soon</h3>
          <p>Shared friendships will appear here once the server can compute them privately.</p>
        </div>
      {/if}
    </div>
  </div>
</div>

<style>
  .pm-wrap {
    position: fixed;
    inset: 0;
    z-index: 620;
    display: grid;
    place-items: center;
    padding: 24px;
  }
  .pm-backdrop {
    position: absolute;
    inset: 0;
    border: none;
    background: rgba(0, 0, 0, 0.6);
    cursor: default;
  }
  .pm-modal {
    position: relative;
    z-index: 1;
    width: min(880px, 100%);
    max-height: calc(100vh - 48px);
    display: grid;
    grid-template-columns: 340px 1fr;
    background: var(--bg-void, #0f1013);
    border: 1px solid var(--border-hair-strong);
    border-radius: 12px;
    box-shadow: 0 24px 80px rgba(0, 0, 0, 0.65);
    padding: 24px 24px 24px 32px;
    gap: 8px;
    overflow: hidden;
  }
  .pm-close {
    position: absolute;
    top: 14px;
    right: 14px;
    z-index: 3;
    width: 32px;
    height: 32px;
    border: none;
    border-radius: 7px;
    background: transparent;
    color: var(--text-muted);
    cursor: pointer;
    display: grid;
    place-items: center;
  }
  .pm-close:hover {
    background: var(--bg-hover);
    color: var(--text-primary);
  }

  /* Left card */
  .pm-card {
    background: var(--bg-panel, #1a1b1e);
    border-radius: 10px;
    overflow: hidden;
    align-self: start;
    min-height: 0;
    max-height: calc(100vh - 96px);
    overflow-y: auto;
  }
  .pm-banner {
    height: 100px;
    background: linear-gradient(135deg, var(--accent, #5865f2), color-mix(in srgb, var(--accent) 50%, #000));
  }
  .pm-avwrap {
    padding: 0 20px;
    margin-top: -52px;
  }
  .pm-av {
    position: relative;
    width: 104px;
    height: 104px;
    border-radius: 50%;
    border: 6px solid var(--bg-panel, #1a1b1e);
    /* Centered initials fallback when there's no uploaded picture. */
    display: flex;
    align-items: center;
    justify-content: center;
    background: var(--accent, #5865f2);
    color: #fff;
    font-size: 40px;
    font-weight: 600;
    text-transform: uppercase;
  }
  .pm-av :global(img) {
    width: 100%;
    height: 100%;
    border-radius: 50%;
    object-fit: cover;
  }
  .pm-status {
    position: absolute;
    right: 0;
    bottom: 2px;
    width: 26px;
    height: 26px;
    border-radius: 50%;
    background: var(--bg-panel, #1a1b1e);
    display: grid;
    place-items: center;
  }
  .pm-status::after {
    content: "";
    width: 14px;
    height: 14px;
    border-radius: 50%;
    background: #80848e;
  }
  .pm-status.online::after {
    background: #3ba55d;
  }
  .pm-status.idle::after,
  .pm-status.away::after {
    background: #faa61a;
  }
  .pm-status.dnd::after,
  .pm-status.busy::after {
    background: #ed4245;
  }
  .pm-card-body {
    padding: 12px 20px 24px;
  }
  .pm-nameline {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .pm-name {
    font-size: 22px;
    font-weight: 700;
    letter-spacing: -0.01em;
    color: var(--text-primary);
  }
  .pm-handle {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    margin-top: 3px;
    padding: 0;
    border: none;
    background: none;
    font: inherit;
    font-size: 13px;
    font-family: var(--font-mono);
    color: var(--text-body, #b5bac1);
    cursor: pointer;
  }
  .pm-handle svg {
    opacity: 0;
    transition: opacity 0.12s;
  }
  .pm-handle:hover svg {
    opacity: 0.7;
  }
  .pm-statusline {
    display: flex;
    align-items: center;
    gap: 6px;
    margin-top: 8px;
    font-size: 12px;
    color: var(--text-secondary);
  }
  .pm-custom-status {
    margin-top: 8px;
    font-size: 14px;
    color: var(--text-primary);
    word-break: break-word;
  }
  .pm-sdot {
    width: 9px;
    height: 9px;
    border-radius: 50%;
    background: #80848e;
  }
  .pm-sdot.online {
    background: #3ba55d;
  }
  .pm-sdot.idle,
  .pm-sdot.away {
    background: #faa61a;
  }
  .pm-sdot.dnd,
  .pm-sdot.busy {
    background: #ed4245;
  }

  .pm-actions {
    display: flex;
    gap: 8px;
    margin: 16px 0 4px;
  }
  .pm-btn {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    height: 36px;
    padding: 0 14px;
    border: none;
    border-radius: 6px;
    background: var(--btn-secondary, #2b2d31);
    color: var(--text-primary);
    font: inherit;
    font-size: 14px;
    font-weight: 500;
    cursor: pointer;
  }
  .pm-btn:hover {
    background: var(--bg-hover, #3a3c41);
  }
  .pm-btn.primary {
    flex: 1;
    justify-content: center;
    background: var(--accent, #5865f2);
    color: #fff;
  }
  .pm-btn.primary:hover {
    background: color-mix(in srgb, var(--accent) 85%, #000);
  }
  .pm-btn.accent {
    background: var(--accent, #5865f2);
    color: #fff;
  }
  .pm-btn.accent:hover {
    background: color-mix(in srgb, var(--accent) 85%, #000);
  }
  .pm-icon {
    width: 36px;
    height: 36px;
    flex-shrink: 0;
    display: grid;
    place-items: center;
    border: none;
    border-radius: 6px;
    background: var(--btn-secondary, #2b2d31);
    color: var(--text-body, #b5bac1);
    cursor: pointer;
  }
  .pm-icon:hover {
    background: var(--bg-hover, #3a3c41);
    color: var(--text-primary);
  }

  .pm-section {
    margin-top: 18px;
  }
  .pm-label {
    font-size: 12px;
    font-weight: 700;
    color: var(--text-muted);
    margin-bottom: 6px;
  }
  .pm-bio {
    font-size: 14px;
    line-height: 1.4;
    color: var(--text-body, #dbdee1);
    white-space: pre-wrap;
    word-break: break-word;
  }

  /* Right panel */
  .pm-right {
    display: flex;
    flex-direction: column;
    min-height: 360px;
    padding: 10px 8px 0 32px;
  }
  .pm-tabs {
    display: flex;
    gap: 24px;
    border-bottom: 1px solid var(--border-hair, #2e3035);
    flex-shrink: 0;
  }
  .pm-tab {
    position: relative;
    padding: 2px 0 12px;
    border: none;
    background: none;
    font: inherit;
    font-size: 15px;
    color: var(--text-muted);
    cursor: pointer;
  }
  .pm-tab:hover {
    color: var(--text-body, #dbdee1);
  }
  .pm-tab[aria-selected="true"] {
    color: var(--text-primary);
    font-weight: 500;
  }
  .pm-tab[aria-selected="true"]::after {
    content: "";
    position: absolute;
    left: 0;
    right: 0;
    bottom: -1px;
    height: 2px;
    border-radius: 2px;
    background: var(--text-primary);
  }

  .pm-list {
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: 12px 0;
    overflow-y: auto;
  }
  .pm-server {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 8px 10px;
    border: none;
    border-radius: 8px;
    background: none;
    cursor: pointer;
    text-align: left;
  }
  .pm-server:hover {
    background: var(--bg-hover);
  }
  .pm-server-icon {
    width: 38px;
    height: 38px;
    flex-shrink: 0;
    border-radius: 11px;
    display: grid;
    place-items: center;
    font-size: 12px;
    font-weight: 800;
    color: #fff;
    background: linear-gradient(135deg, var(--accent, #5865f2), color-mix(in srgb, var(--accent) 55%, #000));
  }
  .pm-server-name {
    flex: 1;
    font-size: 14px;
    font-weight: 600;
    color: var(--text-primary);
  }
  .pm-chev {
    color: var(--text-faint);
  }

  .pm-empty {
    flex: 1;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    text-align: center;
    gap: 10px;
    padding: 20px 40px 60px;
  }
  .pm-empty-emoji {
    font-size: 34px;
  }
  .pm-empty h3 {
    font-size: 16px;
    font-weight: 600;
    color: var(--text-primary);
  }
  .pm-empty p {
    font-size: 14px;
    line-height: 1.4;
    color: var(--text-body, #b5bac1);
    max-width: 300px;
  }

  @media (max-width: 760px) {
    .pm-modal {
      grid-template-columns: 1fr;
      padding: 20px;
    }
    .pm-right {
      padding: 20px 4px 0;
      min-height: 260px;
    }
  }
</style>
