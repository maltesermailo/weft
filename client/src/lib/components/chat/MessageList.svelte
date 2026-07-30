<script lang="ts">
  import { tick } from "svelte";
  import { get } from "svelte/store";
  import { createVirtualizer } from "@tanstack/svelte-virtual";
  import { getApp } from "$lib/context";
  import { spoilerReveal } from "$lib/actions";
  import type { Msg } from "$lib/types";
  import MessageItem from "./MessageItem.svelte";

  const app = getApp();
  // One list per channel; the channel route remounts it on navigation.
  // @tanstack/virtual is headless: WE own the scroll element and windowing math,
  // absolutely position each row inside a spacer of the total size, and drive the
  // bottom-anchor / stick / load-older natively.
  let { channel, active }: { channel: string; active: boolean } = $props();

  const ch = $derived(app.channelRecord(channel));
  // Chronological (oldest → newest). We anchor to the bottom by pinning the
  // scroll element to its own scrollHeight and re-asserting it as rows measure.
  const messages = $derived(ch?.messages.filter((m) => !m.thread) ?? []);
  const loadingThis = $derived(app.loadingHistory === channel);

  let scrollEl = $state<HTMLElement>();
  let showLoader = $state(false);
  let positioned = false; // one-time open positioning done?
  let requestedLoad = false; // asked for the first page yet?
  let atBottom = true; // is the viewport pinned to the newest message?
  // While an older page loads we hold the scroll fixed relative to the bottom
  // (older content prepends above, so distance-from-bottom is invariant).
  let loadingOlder = false;
  let anchorDistBottom = 0;
  let lastCount = 0;

  const getKey = (i: number) => messages[i]?.key ?? i;

  const virtualizer = createVirtualizer<HTMLElement, HTMLElement>({
    // Real count is applied by the setOptions effect below once `messages` is
    // read reactively; start at 0 to avoid an eager non-reactive read here.
    count: 0,
    getScrollElement: () => scrollEl ?? null,
    estimateSize: () => 64,
    getItemKey: getKey,
    overscan: 10,
  });
  // Read the (stable) virtualizer instance WITHOUT subscribing. Using
  // `$virtualizer` inside an effect would subscribe it to the store, and since
  // setOptions / measurement fire the store's onChange, the effect would loop
  // forever. `$virtualizer` is used ONLY in the template (reactive windowing);
  // effects/actions go through `v()`.
  const v = () => get(virtualizer);

  // measureElement ref: report each row's real height (dynamic rows — text,
  // media, dividers). Caching is keyed by message key, so measurements survive
  // an older-page prepend even as indices shift.
  function measure(node: HTMLElement) {
    v().measureElement(node);
    return {
      destroy() {
        v().measureElement(null);
      },
    };
  }

  const raf = () => new Promise((r) => requestAnimationFrame(() => r(null)));

  // Re-assert scrollTop=scrollHeight across a few frames: rows measure in over
  // several frames after render, so the true bottom keeps moving until sizes
  // settle. Bounded loop — no standing reactive coupling to the virtualizer.
  async function pinBottom(frames = 8) {
    let prev = -1;
    for (let i = 0; i < frames; i++) {
      if (!scrollEl) return;
      scrollEl.scrollTop = scrollEl.scrollHeight;
      await raf();
      if (!scrollEl) return; // channel unmounted / switched during the frame
      const cur = Math.round(scrollEl.scrollTop);
      if (cur === prev) break;
      prev = cur;
    }
  }

  // Hold distance-from-bottom while an older page prepends + measures in, then
  // release the load-older guard.
  async function restoreOlder(frames = 8) {
    let prev = -1;
    for (let i = 0; i < frames; i++) {
      if (!scrollEl) break;
      scrollEl.scrollTop = Math.max(0, scrollEl.scrollHeight - anchorDistBottom);
      await raf();
      if (!scrollEl) break; // channel unmounted / switched during the frame
      const cur = Math.round(scrollEl.scrollTop);
      if (cur === prev) break;
      prev = cur;
    }
    loadingOlder = false;
  }

  // First open: fetch this channel's first page once (under a skeleton) if we
  // don't have it. Kept lists mount only when first opened, so this fires once.
  $effect(() => {
    if (positioned || requestedLoad || !ch || ch.historyLoaded) return;
    requestedLoad = true;
    showLoader = true;
    app.loadHistory(channel, true);
  });

  // Keep the virtualizer's count in sync and (re)attach it to the scroll element
  // once it mounts — setOptions runs `_willUpdate`, which observes the now-present
  // element. Depends ONLY on messages.length + scrollEl (never the store), so it
  // can't loop against the onChange it triggers.
  $effect(() => {
    const n = messages.length;
    void scrollEl; // dependency: re-attach when the element binds
    v().setOptions({ count: n, getItemKey: getKey, getScrollElement: () => scrollEl ?? null });
  });

  // Position once the data is ready — either already present on mount, or the
  // first page just landed. An empty (but loaded) channel still resolves here so
  // the skeleton drops; a non-empty one waits for the scroll element to mount.
  $effect(() => {
    if (positioned || !ch?.historyLoaded) return;
    if (messages.length && !scrollEl) return;
    positioned = true;
    // Skeleton stays only if we're still fetching the first page (set above);
    // an already-cached channel positions silently, so switching back is instant.
    void positionOpen();
  });

  function unreadIndex() {
    const boundary = active ? app.newBoundary : null;
    return boundary === null ? -1 : messages.findIndex((m) => !m.system && !m.own && m.ts > boundary);
  }

  async function positionOpen() {
    // Immediate estimate-based pin so the very first paint is already near the
    // bottom (no top-flash on switch), then refine as real heights measure in.
    if (scrollEl && messages.length && unreadIndex() <= 0) {
      scrollEl.scrollTop = scrollEl.scrollHeight;
    }

    await tick();
    await raf(); // let the first ResizeObserver measurement pass land

    if (scrollEl && messages.length) {
      const idx = unreadIndex();

      if (idx > 0) {
        // Unread divider inside the loaded page → rest there.
        const off = v().getOffsetForIndex(idx, "start");
        if (off) scrollEl.scrollTop = off[0];
        atBottom = false;
      } else {
        // Rest at the newest — re-assert until the measured bottom settles
        // (fixes the half-render / black-top-gap open).
        atBottom = true;
        await pinBottom();
      }
      lastCount = messages.length;
    }

    showLoader = false;
  }

  function onScroll() {
    if (!scrollEl || !positioned) return;
    const { scrollTop, scrollHeight, clientHeight } = scrollEl;
    atBottom = scrollTop + clientHeight >= scrollHeight - 40;
    // Near the top → page older. We hold distance-from-bottom while it lands.
    if (scrollTop < 200 && ch?.hasMore && !loadingOlder) {
      loadingOlder = true;
      anchorDistBottom = scrollHeight - scrollTop;
      app.loadHistory(channel, false);
    }
  }

  // React to the message set changing: a live append sticks to the bottom (if we
  // were pinned there); an older-page prepend restores the anchor. Depends only
  // on messages.length — no virtualizer subscription, so no feedback loop.
  $effect(() => {
    const n = messages.length;
    if (!positioned || !scrollEl) {
      lastCount = n;
      return;
    }
    if (n > lastCount) {
      if (loadingOlder) {
        void restoreOlder();
      } else if (atBottom) {
        void pinBottom(4);
      }
    }
    lastCount = n;
  });
</script>

<!--
  Bottom-anchored, virtualized list. Rows render oldest→newest; each row carries
  its own leading day-divider and (before the first unread) the "New messages"
  divider, plus the top-of-history indicator on the first row.
-->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="msg-list-wrap" use:spoilerReveal>
  {#if ch && messages.length}
    <div bind:this={scrollEl} class="message-scroll" onscroll={onScroll}>
      <div class="vspacer" style="height: {$virtualizer.getTotalSize()}px;">
        {#each $virtualizer.getVirtualItems() as row (row.key)}
          {@const m = messages[row.index]}
          {#if m}
            {@const prev = messages[row.index - 1]}
            <div class="vrow" data-index={row.index} use:measure style="transform: translateY({row.start}px);">
              {#if row.index === 0}
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
            </div>
          {/if}
        {/each}
      </div>
    </div>
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

<style>
  /* The spacer holds the full virtual height; rows are positioned within it. */
  .vspacer {
    position: relative;
    width: 100%;
  }

  .vrow {
    position: absolute;
    top: 0;
    left: 0;
    width: 100%;
  }
</style>
