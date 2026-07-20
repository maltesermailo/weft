<script lang="ts">
  import { getApp } from "$lib/context";
  import Avatar from "$lib/components/Avatar.svelte";
  const app = getApp();
</script>

<div class="channel-scroll">
  {#each app.dmList as ch (ch.name)}
    <button class="channel-item dm" class:active={ch.name === app.active} onclick={() => app.open(ch.name)}>
      <span class="avatar sm"><Avatar account={app.peerOf(ch.name)} /></span>
      <span>{app.displayName(ch.name)}</span>
      <span class={app.dotClass(app.peerOf(ch.name))}></span>
    </button>
  {/each}
  {#if !app.dmList.length}
    <div class="empty-hint">No conversations yet.<br />Message someone below.</div>
  {/if}
</div>
