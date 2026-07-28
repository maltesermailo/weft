<script lang="ts">
  import { tick } from "svelte";
  import { getApp } from "$lib/context";
  import { spoilerReveal } from "$lib/actions";
  import MessageItem from "./MessageItem.svelte";

  const app = getApp();
  // This list owns ONE channel and stays mounted while it's kept-alive, so
  // switching back is instant. Everything scroll-related lives HERE, per
  // instance — no shared scroll element or cross-component reactive coupling
  // (that's what broke the previous keep-alive attempt).
  let { channel, active }: { channel: string; active: boolean } = $props();

  let el = $state<HTMLDivElement | null>(null);
  const ch = $derived(app.channelRecord(channel));
  const messages = $derived((ch?.messages.filter((m) => !m.thread) ?? []).slice().reverse());
  const loadingThis = $derived(app.loadingHistory === channel);

  // ---- local scroll / open lifecycle ----
  let showLoader = $state(false);
  let positioning = $state(false); // gate load-older while we position the view
  let stickBottom = $state(true);
  let positioned = false; // has this list done its one-time open positioning?
  let requestedLoad = false; // have we asked for the first page yet?
  let positionStarted = false;

  // First open: fetch this channel's first page once (under a skeleton) if we
  // don't have it. Kept lists only mount when first opened, so this fires once.
  $effect(() => {
    if (positioned || requestedLoad || !ch || ch.historyLoaded) return;
    requestedLoad = true;
    showLoader = true;
    positioning = true;
    app.loadHistory(channel, true);
  });

  // Position once the data is ready — either already present on mount, or the
  // fetch just landed (historyLoaded flips true).
  $effect(() => {
    if (positioned || !el || !ch?.historyLoaded) return;
    void positionSelf();
  });

  async function positionSelf() {
    if (positionStarted) return;
    positionStarted = true;
    // Keep the skeleton up through image decode; a cached, text-only channel
    // has none, so it reveals instantly (no flash).
    if (ch?.messages.some((m) => !!m.attachments?.length)) showLoader = true;
    positioning = true;
    await tick();
    if (!el) return finishPosition();

    const apply = () => {
      if (!el) return;
      // First-open jump to the unread divider only when it's genuinely inside
      // the loaded page (read content above it) and this is the active view;
      // otherwise the newest (scrollTop 0 in a column-reverse list).
      const boundary = active ? app.newBoundary : null;
      if (boundary !== null && ch) {
        const idx = ch.messages.findIndex((m) => !m.system && !m.own && m.ts > boundary);
        if (idx > 0) {
          const divider = el.querySelector<HTMLElement>(".new-sep");
          if (divider) {
            stickBottom = false;
            divider.scrollIntoView({ block: "start" });
            return;
          }
        }
      }
      stickBottom = true;
      el.scrollTop = 0;
    };

    apply();
    requestAnimationFrame(() => {
      apply();
      requestAnimationFrame(async () => {
        positioning = false;
        apply();
        await awaitViewportImages(el, 1200);
        finishPosition();
      });
    });
  }
  function finishPosition() {
    showLoader = false;
    positioning = false;
    positioned = true;
  }

  // Resolve once every not-yet-loaded image in view has finished (or a timeout),
  // so the skeleton drops on a settled picture. Below-fold lazy images ignored.
  function awaitViewportImages(node: HTMLElement | null, timeoutMs: number): Promise<void> {
    if (!node) return Promise.resolve();
    const box = node.getBoundingClientRect();
    const imgs = Array.from(node.querySelectorAll("img")).filter((img) => {
      if (img.complete) return false;
      const r = img.getBoundingClientRect();
      return r.bottom > box.top && r.top < box.bottom;
    });
    if (!imgs.length) return Promise.resolve();
    return new Promise((resolve) => {
      let remaining = imgs.length;
      let settled = false;
      const finish = () => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        resolve();
      };
      const timer = setTimeout(finish, timeoutMs);
      const one = () => {
        if (--remaining <= 0) finish();
      };
      for (const img of imgs) {
        img.addEventListener("load", one, { once: true });
        img.addEventListener("error", one, { once: true });
      }
    });
  }

  function onScroll() {
    if (!el) return;
    updateScrollbar();
    if (positioning) return;
    // column-reverse: scrollTop is 0 at the bottom (newest); |scrollTop| grows as
    // you scroll up (sign differs across engines).
    const up = Math.abs(el.scrollTop);
    stickBottom = up < 60;
    const maxUp = el.scrollHeight - el.clientHeight;
    if (maxUp - up < 80 && ch?.hasMore) app.loadHistory(channel, false);
  }

  // Keep the newest in view when a message arrives, but only while pinned bottom.
  $effect(() => {
    messages.length;
    if (el && stickBottom && !positioning) {
      queueMicrotask(() => {
        if (el) el.scrollTop = 0;
      });
    }
  });

  // ---- custom overlay scrollbar (native one is inverted under column-reverse) ----
  let sbThumbTop = $state(0);
  let sbThumbHeight = $state(0);
  let sbVisible = $state(false);
  function updateScrollbar() {
    if (!el) return;
    const { scrollHeight, clientHeight } = el;
    const maxUp = scrollHeight - clientHeight;
    if (maxUp <= 1) {
      sbVisible = false;
      return;
    }
    const up = Math.min(Math.abs(el.scrollTop), maxUp);
    const thumbFrac = clientHeight / scrollHeight;
    const scrollFrac = up / maxUp; // 0 = newest (bottom), 1 = oldest (top)
    sbThumbHeight = thumbFrac * 100;
    sbThumbTop = (1 - scrollFrac) * (1 - thumbFrac) * 100;
    sbVisible = true;
  }
  let sbDrag = $state<{ y: number; topFrac: number; thumbFrac: number; maxUp: number; trackPx: number } | null>(null);
  function sbDown(e: PointerEvent) {
    if (!el) return;
    const track = (e.currentTarget as HTMLElement).parentElement!;
    sbDrag = {
      y: e.clientY,
      topFrac: sbThumbTop / 100,
      thumbFrac: el.clientHeight / el.scrollHeight,
      maxUp: el.scrollHeight - el.clientHeight,
      trackPx: track.clientHeight,
    };
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    e.preventDefault();
  }
  function sbMove(e: PointerEvent) {
    if (!sbDrag || !el) return;
    const maxTop = 1 - sbDrag.thumbFrac;
    let topFrac = sbDrag.topFrac + (e.clientY - sbDrag.y) / sbDrag.trackPx;
    topFrac = Math.max(0, Math.min(maxTop, topFrac));
    const scrollFrac = maxTop > 0 ? 1 - topFrac / maxTop : 0;
    el.scrollTop = -(scrollFrac * sbDrag.maxUp);
  }
  function sbUp() {
    sbDrag = null;
  }

  // Recompute the thumb when the message set / positioning / size changes.
  $effect(() => {
    messages.length;
    positioning;
    requestAnimationFrame(updateScrollbar);
  });
  $effect(() => {
    const onResize = () => updateScrollbar();
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  });
</script>

<!--
  Bottom-anchored list: `.message-scroll` is `flex-direction: column-reverse`, so
  we render messages NEWEST-FIRST and the browser lays out the newest at the
  bottom (the resting position). Each message's day separator + the "New
  messages" divider render *after* it (visually above); top indicators after the
  loop (visually at the top). Kept-alive but inactive → hidden, DOM + scroll
  retained for an instant return.
-->
<div class="msg-list-wrap" class:list-hidden={!active}>
  <div class="message-scroll" bind:this={el} onscroll={onScroll} use:spoilerReveal>
    {#if ch}
      {#each messages as m, i (m.key)}
        {@const older = messages[i + 1]}
        <MessageItem {m} />
        {#if active && m.key === app.newDividerKey}
          <div class="new-sep" id="new-divider"><span>New messages</span></div>
        {/if}
        {#if !older || app.dayKey(older.ts) !== app.dayKey(m.ts)}
          <div class="day-sep date"><span>{app.dayLabel(m.ts)}</span></div>
        {/if}
      {/each}
      {#if loadingThis}
        <div class="day-sep">loading history…</div>
      {:else if ch.truncated}
        <div class="day-sep">older messages have expired</div>
      {:else if ch.historyLoaded && !ch.hasMore}
        <div class="day-sep">beginning of {app.titleOf(ch.name)}</div>
      {/if}
    {:else}
      <div class="empty-hint">Join a channel to start talking.</div>
    {/if}
  </div>

  {#if sbVisible}
    <div class="msg-scrollbar">
      <!-- svelte-ignore a11y_no_static_element_interactions -->
      <div
        class="msg-scrollbar-thumb"
        class:dragging={sbDrag}
        style="top: {sbThumbTop}%; height: {sbThumbHeight}%"
        onpointerdown={sbDown}
        onpointermove={sbMove}
        onpointerup={sbUp}
      ></div>
    </div>
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
