<script lang="ts">
  import { fade } from "svelte/transition";
  import { lightbox, closeLightbox } from "$lib/lightbox.svelte";

  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape") closeLightbox();
  }
</script>

<svelte:window onkeydown={onKey} />

{#if lightbox.url}
  <div class="lightbox" role="dialog" aria-modal="true" aria-label="Image viewer" transition:fade={{ duration: 140 }}>
    <button class="lightbox-backdrop" aria-label="Close image" onclick={closeLightbox}></button>
    <img src={lightbox.url} alt={lightbox.alt} />
    <div class="lightbox-bar">
      <a href={lightbox.url} target="_blank" rel="noreferrer">Open original ↗</a>
      <button aria-label="Close" onclick={closeLightbox}>✕ Close</button>
    </div>
  </div>
{/if}

<style>
  .lightbox {
    position: fixed;
    inset: 0;
    z-index: 200;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 12px;
    padding: 40px;
  }
  .lightbox-backdrop {
    position: absolute;
    inset: 0;
    border: none;
    background: rgba(0, 0, 0, 0.82);
    cursor: zoom-out;
  }
  .lightbox img {
    position: relative;
    max-width: 92vw;
    max-height: 82vh;
    border-radius: 8px;
    box-shadow: 0 20px 60px rgba(0, 0, 0, 0.6);
    object-fit: contain;
  }
  .lightbox-bar {
    position: relative;
    display: flex;
    gap: 10px;
  }
  .lightbox-bar a,
  .lightbox-bar button {
    padding: 6px 12px;
    border-radius: 6px;
    border: 1px solid rgba(255, 255, 255, 0.2);
    background: rgba(0, 0, 0, 0.4);
    color: #fff;
    font: inherit;
    font-size: 13px;
    text-decoration: none;
    cursor: pointer;
  }
  .lightbox-bar a:hover,
  .lightbox-bar button:hover {
    background: rgba(255, 255, 255, 0.14);
  }
</style>
