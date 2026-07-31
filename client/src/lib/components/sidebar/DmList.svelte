<script lang="ts">
  import { store } from "$lib/store/store.svelte";
  import { vm } from "$lib/navigation/viewmodel.svelte";
  import { open, openFriends, openGroup } from "$lib/navigation/navigation";
  import { userCtx, groupCtx } from "$lib/ui/ctxmenu.svelte";
  import { roster } from "$lib/social/social.svelte";
  import { dotClass, peerOf, profileStore } from "$lib/profile/profile.svelte";
  import { getApp } from "$lib/ui/context";
  import Avatar from "$lib/components/Avatar.svelte";
  const app = getApp();
</script>

<div class="channel-scroll">
  <button class="channel-item friends-nav" class:active={app.homeView && !app.active} onclick={openFriends}>
    <span class="friends-ico">
      <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2" /><circle cx="9" cy="7" r="4" /><path d="M22 21v-2a4 4 0 0 0-3-3.87" /><path d="M16 3.13a4 4 0 0 1 0 7.75" /></svg>
    </span>
    <span class="dm-name">Friends</span>
    {#if roster.incoming.length}<span class="mention-badge">{roster.incoming.length}</span>{/if}
  </button>
  {#each vm.dmList as ch (ch.name)}
    {#if ch.name.startsWith("&")}
      <!-- group DM -->
      <button class="channel-item dm" class:active={ch.name === app.active} class:unread={ch.unread} onclick={() => openGroup(ch.name)} oncontextmenu={(e) => groupCtx(e, ch.name)}>
        <span class="avatar sm group-ico">
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2" /><circle cx="9" cy="7" r="4" /><path d="M23 21v-2a4 4 0 0 0-3-3.87" /><path d="M16 3.13a4 4 0 0 1 0 7.75" /></svg>
        </span>
        <span class="dm-name">{store.social.groupLabel(ch.name)}</span>
        {#if ch.unreadCount}<span class="mention-badge">{ch.unreadCount}</span>{/if}
      </button>
    {:else}
      <button class="channel-item dm" class:active={ch.name === app.active} class:unread={ch.unread} onclick={() => open(ch.name)} oncontextmenu={(e) => userCtx(e, peerOf(ch.name))}>
        <span class="avatar sm"><Avatar account={peerOf(ch.name)} /></span>
        <span class="dm-name">{profileStore.displayName(ch.name)}</span>
        {#if ch.unreadCount}<span class="mention-badge">{ch.unreadCount}</span>{/if}
        <span class={dotClass(peerOf(ch.name))}></span>
      </button>
    {/if}
  {/each}
  {#if !vm.dmList.length}
    <div class="empty-hint">No conversations yet.<br />Message someone below.</div>
  {/if}
</div>

<style>
  .friends-nav {
    margin-bottom: 4px;
  }
  .friends-ico {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 28px;
    color: var(--text-muted);
  }
  .friends-nav.active .friends-ico,
  .friends-nav:hover .friends-ico {
    color: var(--text-primary);
  }
  .group-ico {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    color: var(--text-muted);
    background: var(--bg-panel);
  }
</style>
