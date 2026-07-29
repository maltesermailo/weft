<script lang="ts">
  import { tick } from "svelte";
  import { VList } from "virtua/svelte";
  import { getApp } from "$lib/context";
  import { spoilerReveal } from "$lib/actions";
  import type { Msg } from "$lib/types";
  import MessageItem from "./MessageItem.svelte";

  const app = getApp();
  // This list owns ONE channel and stays mounted while it's kept-alive, so
  // switching back is instant. virtua windows the rows (constant DOM regardless
  // of history length); we drive the bottom-anchor / stick / load-older here.
  let { channel, active }: { channel: string; active: boolean } = $props();

  const ch = $derived(app.channelRecord(channel));
  // Chronological (oldest → newest). virtua is a top-anchored virtualizer, so we
  // scroll it to the end on open and stick to the bottom as messages arrive.
  const messages = $derived(ch?.messages.filter((m) => !m.thread) ?? []);
  const loadingThis = $derived(app.loadingHistory === channel);

  let vlist = $state<VList<Msg>>();
  let showLoader = $state(false);
  let positioned = false; // one-time open positioning done?
  let requestedLoad = false; // asked for the first page yet?
  let atBottom = true; // is the viewport pinned to the newest message?
  // While an older page is loading, `shift` keeps the scroll position as the
  // prepended messages measure in (virtua's reverse-infinite-scroll mode).
  let loadingOlder = $state(false);
  let lastCount = 0;

  // First open: fetch this channel's first page once (under a skeleton) if we
  // don't have it. Kept lists mount only when first opened, so this fires once.
  $effect(() => {
    if (positioned || requestedLoad || !ch || ch.historyLoaded) return;
    requestedLoad = true;
    showLoader = true;
    app.loadHistory(channel, true);
  });

  // Position once the data is ready — either already present on mount, or the
  // first page just landed. An empty (but loaded) channel still resolves here so
  // the skeleton drops; a non-empty one waits for the <VList> to mount first.
  $effect(() => {
    if (positioned || !ch?.historyLoaded) return;
    if (messages.length && !vlist) return; // wait for the list element to mount
    positioned = true;
    void positionOpen();
  });

  async function positionOpen() {
    await tick();
    if (vlist && messages.length) {
      // Jump to the unread divider when it's genuinely inside the loaded page and
      // this is the active view; otherwise rest at the newest (bottom).
      const boundary = active ? app.newBoundary : null;
      const idx = boundary === null ? -1 : messages.findIndex((m) => !m.system && !m.own && m.ts > boundary);
      if (idx > 0) {
        vlist.scrollToIndex(idx, { align: "start" });
        atBottom = false;
      } else {
        vlist.scrollToIndex(messages.length - 1, { align: "end" });
        atBottom = true;
      }
      lastCount = messages.length;
      await tick();
    }
    showLoader = false;
  }

  function onScroll(offset: number) {
    if (!vlist || !positioned) return;
    const max = vlist.getScrollSize() - vlist.getViewportSize();
    atBottom = offset >= max - 40;
    // Near the top → page older. `shift` (below) keeps the position as they land.
    if (offset < 200 && ch?.hasMore && !loadingOlder) {
      loadingOlder = true;
      app.loadHistory(channel, false);
    }
  }

  // React to the message set changing: a live append sticks to bottom (if we
  // were pinned there); an older-page prepend just clears the loading flag
  // (`shift` already held the position).
  $effect(() => {
    const n = messages.length;
    if (!positioned || !vlist) {
      lastCount = n;
      return;
    }
    if (n > lastCount) {
      if (loadingOlder) {
        loadingOlder = false; // prepend landed
      } else if (atBottom) {
        const last = n - 1;
        queueMicrotask(() => vlist?.scrollToIndex(last, { align: "end" }));
      }
    }
    lastCount = n;
  });
</script>

<!--
  Bottom-anchored, virtualized list. Rows render oldest→newest; each row carries
  its own leading day-divider and (before the first unread) the "New messages"
  divider, plus the top-of-history indicator on the first row. Kept-alive but
  inactive → hidden, DOM + virtua state retained for an instant return.
-->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="msg-list-wrap" class:list-hidden={!active} use:spoilerReveal>
  {#if ch && messages.length}
    <VList
      bind:this={vlist}
      class="message-scroll"
      data={messages}
      getKey={(m) => m.key}
      shift={loadingOlder}
      onscroll={onScroll}
    >
      {#snippet children(m: Msg, i: number)}
        {@const prev = messages[i - 1]}
        {#if i === 0}
          {#if loadingThis}
            <div class="day-sep">loading history…</div>
          {:else if ch?.truncated}
            <div class="day-sep">older messages have expired</div>
          {:else if ch && ch.historyLoaded && !ch.hasMore}
            <div class="day-sep">beginning of {app.titleOf(ch.name)}</div>
          {/if}
        {/if}
        {#if !prev || app.dayKey(prev.ts) !== app.dayKey(m.ts)}
          <div class="day-sep date"><span>{app.dayLabel(m.ts)}</span></div>
        {/if}
        {#if active && m.key === app.newDividerKey}
          <div class="new-sep" id="new-divider"><span>New messages</span></div>
        {/if}
        <MessageItem {m} />
      {/snippet}
    </VList>
  {:else if ch}
    <div class="message-scroll"><div class="empty-hint">No messages yet — say something.</div></div>
  {:else}
    <div class="message-scroll"><div class="empty-hint">Join a channel to start talking.</div></div>
  {/if}

  {#if showLoader}
    <div class="channel-loader" aria-busy="true" aria-label="Loading messages">
      {#each Array.from({ length: 7 }) as _, i (i)}
        <div class="skel-row" style="animation-delay: {i * 60}ms">
          <div class="skel-avatar"></div>
          <div class="skel-lines">
            <div class="skel-line" style="width: {30 + ((i * 17) % 40)}%"></div>
            <div class="skel-line" style="width: {50 + ((i * 23) % 45)}%"></div>
          </div>
        </div>
      {/each}
    </div>
  {/if}
</div>
