<script lang="ts">
  import { getApp } from "$lib/context";
  const app = getApp();
</script>

<div class="sidebar-header">
  {#if app.homeView}
    <p class="comm-name">Direct Messages</p>
  {:else}
    <button class="comm-name-btn" class:open={app.serverMenu} onclick={() => (app.serverMenu = !app.serverMenu)}>
      <span class="comm-head">
        <span class="comm-name">{app.activeNsMeta?.title || app.activeServer || app.network}</span>
        <span class="comm-origin">
          <span class="origin-dot"></span>
          <span>{app.activeServer ? `namespace · ${app.network}` : `${app.network} · connected`}</span>
        </span>
      </span>
      <svg class="hdr-chev" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="m6 9 6 6 6-6" /></svg>
    </button>
    {#if app.serverMenu}
      <button class="ctx-backdrop" aria-label="Close menu" onclick={() => (app.serverMenu = false)}></button>
      <div class="server-menu">
        <button class="sm-item" onclick={() => { app.openInvites(); app.serverMenu = false; }}>
          Create Invite
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2" /><circle cx="9" cy="7" r="4" /><path d="M19 8v6M22 11h-6" /></svg>
        </button>
        <button class="sm-item" onclick={app.openNotifSettings}>
          Notification Settings
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M18 8a6 6 0 0 0-12 0c0 7-3 9-3 9h18s-3-2-3-9" /><path d="M13.7 21a2 2 0 0 1-3.4 0" /></svg>
        </button>
        {#if app.activeServer}
          <button class="sm-item" onclick={() => { app.openNsSettings(); app.serverMenu = false; }}>
            Server Settings
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><circle cx="12" cy="12" r="3" /><path d="M19.4 15a1.7 1.7 0 0 0 .3 1.9l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.9-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1-1.6 1.7 1.7 0 0 0-1.9.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.9 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.6-1 1.7 1.7 0 0 0-.3-1.9l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.9.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.9-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.9V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z" /></svg>
          </button>
        {/if}
        <div class="sm-sep"></div>
        <button class="sm-item" onclick={() => app.openCreateChannel()}>
          Create Channel
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><circle cx="12" cy="12" r="9" /><path d="M12 8v8M8 12h8" /></svg>
        </button>
        <button class="sm-item" onclick={() => app.newCat()}>
          Create Category
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M3 7h7l2 2h9v10a1 1 0 0 1-1 1H3Z" /><path d="M12 13v4M10 15h4" /></svg>
        </button>
        <div class="sm-sep"></div>
        <button class="sm-item" onclick={() => { navigator.clipboard?.writeText(app.activeServer || app.network); app.serverMenu = false; }}>Copy Server ID</button>
      </div>
    {/if}
  {/if}
</div>
