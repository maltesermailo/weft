<script lang="ts">
  import { onMount } from "svelte";
  import { getApp } from "$lib/context";
  import { mediaHash, mediaDims } from "$lib/weft";
  import { openLightbox } from "$lib/lightbox.svelte";

  const app = getApp();
  let { uri }: { uri: string } = $props();

  const url = $derived(app.mediaUrl(uri));
  const name = $derived(mediaHash(uri).slice(0, 16) || "file");
  // §13 intrinsic image size the sender stamped on the reference. Fitted into
  // the display caps and given as the <img>'s width/height, so the browser
  // reserves the exact box before the bytes load — zero layout shift, no visible
  // "build". Absent on older messages / non-images → sizes to content as before.
  const MAX_W = 420;
  const MAX_H = 320;
  const box = $derived.by(() => {
    const d = mediaDims(uri);
    if (!d) return null;
    const scale = Math.min(MAX_W / d.w, MAX_H / d.h, 1);
    return { w: Math.round(d.w * scale), h: Math.round(d.h * scale) };
  });
  // §13 attachments carry only a content-addressed URI (no mime). Render as an
  // image *immediately* (the overwhelmingly common case) so it starts loading
  // right away and exists for the open-time image wait — no blocking probe
  // round-trip that would defer the <img> and let it pop in later. The probe
  // still runs, but only to *downgrade* to video/audio/file; and the tag's own
  // `onerror` chain (image → video → file) recovers if the guess was wrong.
  let kind = $state<"image" | "video" | "audio" | "file">("image");

  onMount(async () => {
    try {
      const r = await fetch(url, { headers: { Range: "bytes=0-0" } });
      if (!r.ok) return; // keep the optimistic image; onerror recovers if needed
      const ct = r.headers.get("content-type") ?? "";
      if (ct.startsWith("video/")) kind = "video";
      else if (ct.startsWith("audio/")) kind = "audio";
      else if (ct && !ct.startsWith("image/")) kind = "file";
      // image/* or an unreadable type → stay an image.
    } catch {
      // Probe blocked/failed — keep the optimistic image.
    }
  });
</script>

{#if kind === "image"}
  <button class="att-image" onclick={() => openLightbox(url, name)} aria-label="Open image">
    <img
      src={url}
      alt="attachment"
      loading="lazy"
      width={box?.w}
      height={box?.h}
      onerror={() => (kind = "video")}
    />
  </button>
{:else if kind === "video"}
  <!-- svelte-ignore a11y_media_has_caption -->
  <video class="att-video" src={url} controls preload="metadata" onerror={() => (kind = "file")}></video>
{:else if kind === "audio"}
  <audio class="att-audio" src={url} controls preload="metadata"></audio>
{:else if kind === "file"}
  <a class="att-file" href={url} target="_blank" rel="noreferrer" download>
    <span class="att-file-icon">📎</span><span class="att-file-name">{name}</span>
  </a>
{/if}

<style>
  .att-image {
    display: block;
    padding: 0;
    border: none;
    background: none;
    cursor: zoom-in;
  }
  .att-image img {
    /* The width/height attributes carry the fitted box, so the browser reserves
       the exact space before load (no shift). `max-width:100%`/`height:auto`
       keep it responsive on a narrow window; the fallback caps size when a
       sender didn't stamp dimensions (older messages). */
    max-width: min(420px, 100%);
    max-height: 320px;
    height: auto;
    border-radius: 8px;
    display: block;
    margin-top: 4px;
  }
  .att-audio {
    margin-top: 6px;
    max-width: min(420px, 100%);
    height: 36px;
  }
  .att-video {
    max-width: min(480px, 100%);
    max-height: 360px;
    border-radius: 8px;
    margin-top: 4px;
  }
  .att-file {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    margin-top: 4px;
    padding: 6px 10px;
    border-radius: 8px;
    background: var(--surface-2, rgba(127, 127, 127, 0.12));
    color: inherit;
    text-decoration: none;
    font-size: 0.85rem;
  }
  .att-file:hover {
    background: var(--surface-3, rgba(127, 127, 127, 0.2));
  }
</style>
