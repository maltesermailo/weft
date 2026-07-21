<script lang="ts">
  import { onMount } from "svelte";
  import { getApp } from "$lib/context";
  import { openLightbox } from "$lib/lightbox.svelte";

  const app = getApp();
  let { uri }: { uri: string } = $props();

  const url = $derived(app.mediaUrl(uri));
  const name = $derived(uri.split("/").pop()?.slice(0, 16) ?? "file");
  // §13 attachments carry only a content-addressed URI (no mime yet), so probe
  // the Content-Type with a 1-byte ranged fetch to pick the right renderer.
  let kind = $state<"loading" | "image" | "video" | "audio" | "file">("loading");

  onMount(async () => {
    try {
      const r = await fetch(url, { headers: { Range: "bytes=0-0" } });
      const ct = r.headers.get("content-type") ?? "";
      kind = ct.startsWith("image/")
        ? "image"
        : ct.startsWith("video/")
          ? "video"
          : ct.startsWith("audio/")
            ? "audio"
            : "file";
    } catch {
      kind = "file";
    }
  });
</script>

{#if kind === "image"}
  <button class="att-image" onclick={() => openLightbox(url, name)} aria-label="Open image">
    <img src={url} alt="attachment" loading="lazy" />
  </button>
{:else if kind === "video"}
  <!-- svelte-ignore a11y_media_has_caption -->
  <video class="att-video" src={url} controls preload="metadata"></video>
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
    max-width: min(420px, 100%);
    max-height: 320px;
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
