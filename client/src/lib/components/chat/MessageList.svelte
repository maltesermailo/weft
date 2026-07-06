<script lang="ts">
  import { getApp } from "$lib/context";
  import MessageItem from "./MessageItem.svelte";

  const app = getApp();
  let {
    scrollEl = $bindable(),
    onscroll,
  }: {
    scrollEl: HTMLDivElement | null;
    onscroll: (e: Event) => void;
  } = $props();
</script>

<div class="message-scroll" bind:this={scrollEl} {onscroll}>
  {#key app.active}
    <div class="view-fade">
      {#if app.activeChannel}
        {#if app.loadingHistory === app.active}
          <div class="day-sep">loading history…</div>
        {:else if app.activeChannel.truncated}
          <div class="day-sep">older messages have expired</div>
        {:else if app.activeChannel.historyLoaded && !app.activeChannel.hasMore}
          <div class="day-sep">beginning of {app.activeChannel.name}</div>
        {/if}
        {#each app.activeChannel.messages as m (m.key)}
          <MessageItem {m} />
        {/each}
      {:else}
        <div class="empty-hint">Join a channel to start talking.</div>
      {/if}
    </div>
  {/key}
</div>
