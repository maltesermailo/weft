<script lang="ts">
  import { plugins } from "$lib/plugins/plugins.svelte";
  import { store } from "$lib/store/store.svelte";
  import { vm } from "$lib/navigation/viewmodel.svelte";
  import { nsLeave } from "$lib/navigation/navigation";
  import { ui } from "$lib/ui/ui.svelte";
  
  
  import { getApp } from "$lib/ui/context";
  const app = getApp();

  // §13.1 the `server-menu` surface. A namespace-context action makes sense
  // here; a context-less one is app-wide and belongs on `global` instead.
  const serverMenuActions = $derived(
    plugins.actionsFor("server-menu").filter(({ action }) => action.context === "namespace" || action.context === "none"),
  );
</script>

<div class="sidebar-header">
  {#if app.homeView}
    <p class="comm-name">Direct Messages</p>
  {:else}
    <button class="comm-name-btn" class:open={ui.serverMenu} onclick={() => (ui.serverMenu = !ui.serverMenu)}>
      <span class="comm-head">
        <!-- `displayName` (title → origin → vanity → id), not a local re-derivation:
             re-deriving it here skipped the §7a.2 origin fallback, so a bridged
             namespace whose title hadn't loaded fell through to the *network* name
             and the header claimed to be the whole server. -->
        <span class="comm-name">{app.activeServer ? vm.serverName(app.activeServer) : store.session.network}</span>
        <span class="comm-origin">
          <span class="origin-dot"></span>
          <span>{app.activeServer ? `namespace · ${store.session.network}` : `${store.session.network} · connected`}</span>
        </span>
      </span>
      <svg class="hdr-chev" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="m6 9 6 6 6-6" /></svg>
    </button>
    {#if ui.serverMenu}
      <button class="ctx-backdrop" aria-label="Close menu" onclick={() => (ui.serverMenu = false)}></button>
      <div class="server-menu">
        {#if store.session.serverCap("invite")}
          <button class="sm-item" onclick={() => { store.invites.mintInvite(); ui.serverMenu = false; }}>
            Create Invite
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2" /><circle cx="9" cy="7" r="4" /><path d="M19 8v6M22 11h-6" /></svg>
          </button>
        {/if}
        <button class="sm-item" onclick={app.openNotifSettings}>
          Notification Settings
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M18 8a6 6 0 0 0-12 0c0 7-3 9-3 9h18s-3-2-3-9" /><path d="M13.7 21a2 2 0 0 1-3.4 0" /></svg>
        </button>
        {#if app.activeServer && store.session.serverCap("ns-admin")}
          <button class="sm-item" onclick={app.openServerProfile}>
            Edit Server Profile
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M20 21v-2a4 4 0 0 0-4-4H8a4 4 0 0 0-4 4v2" /><circle cx="12" cy="7" r="4" /></svg>
          </button>
        {/if}
        {#if app.activeServer && store.session.canOpenServerSettings()}
          <button class="sm-item" onclick={() => { app.openNsSettings(); ui.serverMenu = false; }}>
            Server Settings
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><circle cx="12" cy="12" r="3" /><path d="M19.4 15a1.7 1.7 0 0 0 .3 1.9l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.9-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1-1.6 1.7 1.7 0 0 0-1.9.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.9 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.6-1 1.7 1.7 0 0 0-.3-1.9l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.9.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.9-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.9V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z" /></svg>
          </button>
        {/if}
        {#if store.session.serverCap("chan-create")}
          <div class="sm-sep"></div>
          <button class="sm-item" onclick={() => app.openCreateChannel()}>
            Create Channel
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><circle cx="12" cy="12" r="9" /><path d="M12 8v8M8 12h8" /></svg>
          </button>
          <button class="sm-item" onclick={() => app.newCat()}>
            Create Category
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M3 7h7l2 2h9v10a1 1 0 0 1-1 1H3Z" /><path d="M12 13v4M10 15h4" /></svg>
          </button>
        {/if}
        <!-- §13.1 plugin-declared server-menu entries, below the built-ins so a
             plugin adds to the menu rather than displacing what is there. The
             namespace rides along as `ctxRef` — it is what the action acts on. -->
        {#if serverMenuActions.length}
          <div class="sm-sep"></div>
          {#each serverMenuActions as { plugin, action } (plugin + action.id)}
            <button
              class="sm-item"
              onclick={() => {
                plugins.invoke(plugin, action.id, app.activeServer);
                ui.serverMenu = false;
              }}
            >
              {action.label}
            </button>
          {/each}
        {/if}
        <div class="sm-sep"></div>
        <button class="sm-item" onclick={() => { navigator.clipboard?.writeText(app.activeServer || store.session.network); ui.serverMenu = false; }}>Copy Server ID</button>
        {#if app.activeServer && !store.session.isNsOwner(store.session.account)}
          <div class="sm-sep"></div>
          <button class="sm-item danger" onclick={nsLeave}>
            Leave Server
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4" /><polyline points="16 17 21 12 16 7" /><line x1="21" y1="12" x2="9" y2="12" /></svg>
          </button>
        {/if}
      </div>
    {/if}
  {/if}
</div>
