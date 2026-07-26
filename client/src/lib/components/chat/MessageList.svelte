<script lang="ts">
  import { getApp } from "$lib/context";
  import { spoilerReveal } from "$lib/actions";
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

<!--
  Bottom-anchored list: `.message-scroll` is `flex-direction: column-reverse`, so
  we render messages NEWEST-FIRST in the DOM and the browser lays them out with
  the newest at the bottom — the resting position, no scrolling needed. Because
  the DOM is reversed, each message's day separator and the "New messages"
  divider render *after* it (which is visually *above* it), and the top-of-list
  indicators render after the whole loop (visually at the top).
-->
<div class="message-scroll" bind:this={scrollEl} {onscroll} use:spoilerReveal>
  {#key app.active}
    {#if app.activeChannel}
      {#each app.visibleMessagesReversed as m, i (m.key)}
        {@const older = app.visibleMessagesReversed[i + 1]}
        <MessageItem {m} />
        {#if m.key === app.newDividerKey}
          <div class="new-sep" id="new-divider"><span>New messages</span></div>
        {/if}
        {#if !older || app.dayKey(older.ts) !== app.dayKey(m.ts)}
          <div class="day-sep date"><span>{app.dayLabel(m.ts)}</span></div>
        {/if}
      {/each}
      {#if app.loadingHistory === app.active}
        <div class="day-sep">loading history…</div>
      {:else if app.activeChannel.truncated}
        <div class="day-sep">older messages have expired</div>
      {:else if app.activeChannel.historyLoaded && !app.activeChannel.hasMore}
        <div class="day-sep">beginning of {app.activeChannel.name}</div>
      {/if}
    {:else}
      <div class="empty-hint">Join a channel to start talking.</div>
    {/if}
  {/key}
</div>
