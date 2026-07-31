<script lang="ts">
  import { store } from "$lib/models/store.svelte";
  import { vm } from "$lib/viewmodel.svelte";
  import { setStatus, logout } from "$lib/connection.svelte";
  import { ui } from "$lib/ui.svelte";
  import { initials, statusOf, openNickDialog, setCustomStatus } from "$lib/profile.svelte";
  import { fade } from "svelte/transition";
  import { getApp } from "$lib/context";
  import Avatar from "$lib/components/Avatar.svelte";
  const app = getApp();

  const STATUSES: { value: string; label: string }[] = [
    { value: "online", label: "Online" },
    { value: "away", label: "Idle" },
    { value: "dnd", label: "Do Not Disturb" },
    { value: "invisible", label: "Invisible" },
  ];

  // Custom-status modal, layered on top of the user menu.
  let statusModal = $state(false);
  let statusDraft = $state("");
  const myCustom = $derived(statusOf(store.session.account));
  function openStatusModal() {
    statusDraft = myCustom;
    statusModal = true;
  }
  function saveStatus() {
    setCustomStatus(statusDraft.trim());
    statusModal = false;
    ui.userMenu = false;
  }
  function clearStatus() {
    setCustomStatus("");
    statusDraft = "";
    statusModal = false;
    ui.userMenu = false;
  }
  function focusInput(node: HTMLInputElement) {
    node.focus();
    node.select();
  }
</script>

<svelte:window
  onkeydown={(e) => {
    if (statusModal && e.key === "Escape") statusModal = false;
  }}
/>

<div class="sidebar-user-wrap">
  {#if ui.userMenu}
    <button class="ctx-backdrop" aria-label="Close menu" onclick={() => (ui.userMenu = false)}></button>
    <div class="user-menu">
      <div class="um-head">
        <span class="avatar status-avatar">
          <Avatar account={store.session.account} />
          <span class="dot {store.session.myStatus} corner"></span>
        </span>
        <span class="who">
          <span class="name">{store.session.account}</span>
          <span class="key">{myCustom || store.session.network}</span>
        </span>
      </div>
      <div class="sm-sep"></div>
      {#each STATUSES as s (s.value)}
        <button class="sm-item" class:active={store.session.myStatus === s.value} onclick={() => setStatus(s.value)}>
          <span class="um-status"><span class="dot {s.value}"></span>{s.label}</span>
          {#if store.session.myStatus === s.value}<span class="um-check">✓</span>{/if}
        </button>
      {/each}
      <div class="sm-sep"></div>
      <button class="sm-item" onclick={openStatusModal}>
        <span class="um-status">
          <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><circle cx="12" cy="12" r="9" /><path d="M8 14s1.5 2 4 2 4-2 4-2" /><path d="M9 9h.01M15 9h.01" /></svg>
          {myCustom ? "Edit custom status" : "Set a custom status"}
        </span>
        {#if myCustom}<span class="um-clear" role="button" tabindex="0" title="Clear status" onclick={(e) => { e.stopPropagation(); clearStatus(); }} onkeydown={(e) => e.key === "Enter" && (e.stopPropagation(), clearStatus())}>✕</span>{/if}
      </button>
      {#if app.activeServer}
        <button class="sm-item" onclick={() => { ui.userMenu = false; openNickDialog(store.session.account); }}>
          <span class="um-status">
            <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M4 21v-2a4 4 0 0 1 4-4h4" /><circle cx="10" cy="7" r="4" /><path d="M16 19l2 2 4-4" /></svg>
            Set nickname on {vm.serverName(app.activeServer)}
          </span>
        </button>
      {/if}
      <div class="sm-sep"></div>
      <button class="sm-item" onclick={app.openSettings}>
        User Settings
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><circle cx="12" cy="12" r="3" /><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" /></svg>
      </button>
      <button class="sm-item danger" onclick={logout}>
        Log out
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4" /><path d="m16 17 5-5-5-5" /><path d="M21 12H9" /></svg>
      </button>
    </div>
  {/if}

  <button class="sidebar-user" class:open={ui.userMenu} title="User menu" onclick={() => (ui.userMenu = !ui.userMenu)}>
    <span class="avatar status-avatar">
      {initials(store.session.account)}
      <span class="dot {store.session.myStatus} corner"></span>
    </span>
    <span class="who">
      <span class="name">{store.session.account}</span>
      <span class="key">{myCustom || store.session.myStatus}</span>
    </span>
    <svg class="user-gear" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="m6 9 6 6 6-6" /></svg>
  </button>

  {#if statusModal}
    <div class="status-modal-overlay" role="dialog" aria-modal="true" aria-label="Set custom status" transition:fade|global={{ duration: 160 }}>
      <button class="status-modal-backdrop" aria-label="Cancel" onclick={() => (statusModal = false)}></button>
      <div class="status-modal">
        <div class="status-modal-title">Set a custom status</div>
        <input
          class="text-input"
          use:focusInput
          bind:value={statusDraft}
          maxlength="128"
          placeholder="What's happening?"
          onkeydown={(e) => e.key === "Enter" && saveStatus()}
        />
        <div class="status-modal-actions">
          {#if myCustom}<button class="linkish" onclick={clearStatus}>Clear</button>{/if}
          <span class="status-modal-spacer"></span>
          <button class="linkish" onclick={() => (statusModal = false)}>Cancel</button>
          <button class="ok-btn" onclick={saveStatus}>Save</button>
        </div>
      </div>
    </div>
  {/if}
</div>
