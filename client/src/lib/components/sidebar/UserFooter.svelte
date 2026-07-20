<script lang="ts">
  import { getApp } from "$lib/context";
  import Avatar from "$lib/components/Avatar.svelte";
  const app = getApp();

  const STATUSES: { value: string; label: string }[] = [
    { value: "online", label: "Online" },
    { value: "away", label: "Idle" },
    { value: "dnd", label: "Do Not Disturb" },
    { value: "invisible", label: "Invisible" },
  ];
</script>

<div class="sidebar-user-wrap">
  {#if app.userMenu}
    <button class="ctx-backdrop" aria-label="Close menu" onclick={() => (app.userMenu = false)}></button>
    <div class="user-menu">
      <div class="um-head">
        <span class="avatar status-avatar">
          <Avatar account={app.account} />
          <span class="dot {app.myStatus} corner"></span>
        </span>
        <span class="who">
          <span class="name">{app.account}</span>
          <span class="key">{app.network}</span>
        </span>
      </div>
      <div class="sm-sep"></div>
      {#each STATUSES as s (s.value)}
        <button class="sm-item" class:active={app.myStatus === s.value} onclick={() => app.setStatus(s.value)}>
          <span class="um-status"><span class="dot {s.value}"></span>{s.label}</span>
          {#if app.myStatus === s.value}<span class="um-check">✓</span>{/if}
        </button>
      {/each}
      <div class="sm-sep"></div>
      <button class="sm-item" onclick={app.openSettings}>
        User Settings
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><circle cx="12" cy="12" r="3" /><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" /></svg>
      </button>
      <button class="sm-item danger" onclick={app.logout}>
        Log out
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4" /><path d="m16 17 5-5-5-5" /><path d="M21 12H9" /></svg>
      </button>
    </div>
  {/if}

  <button class="sidebar-user" class:open={app.userMenu} title="User menu" onclick={() => (app.userMenu = !app.userMenu)}>
    <span class="avatar status-avatar">
      {app.initials(app.account)}
      <span class="dot {app.myStatus} corner"></span>
    </span>
    <span class="who">
      <span class="name">{app.account}</span>
      <span class="key">{app.myStatus}</span>
    </span>
    <svg class="user-gear" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="m6 9 6 6 6-6" /></svg>
  </button>
</div>
