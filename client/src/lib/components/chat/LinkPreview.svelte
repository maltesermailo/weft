<script lang="ts">
  import * as weft from "$lib/weft";
  import type { LinkPreview } from "$lib/weft";

  let { url }: { url: string } = $props();

  let preview = $state<LinkPreview | null>(null);

  // Re-fetch when the URL changes; the weft.unfurl cache dedupes repeats.
  $effect(() => {
    const u = url;
    preview = null;
    weft.unfurl(u).then((p) => {
      // Guard against a stale resolve after the prop changed.
      if (u === url) preview = p;
    });
  });

  const imageSrc = $derived(preview?.image ? weft.unfurlImageUrl(preview.image) : null);
</script>

{#if preview && (preview.title || preview.description || imageSrc)}
  <a class="link-preview" href={preview.url} target="_blank" rel="noopener noreferrer">
    {#if imageSrc}
      <img class="lp-image" src={imageSrc} alt="" loading="lazy" />
    {/if}
    <div class="lp-text">
      {#if preview.siteName}<div class="lp-site">{preview.siteName}</div>{/if}
      {#if preview.title}<div class="lp-title">{preview.title}</div>{/if}
      {#if preview.description}<div class="lp-desc">{preview.description}</div>{/if}
    </div>
  </a>
{/if}

<style>
  .link-preview {
    display: flex;
    gap: 12px;
    max-width: 460px;
    margin-top: 6px;
    padding: 10px 12px;
    border: 1px solid var(--border-hair-strong);
    border-left: 3px solid var(--accent, #5865f2);
    border-radius: var(--radius-md);
    background: var(--bg-panel-raised);
    text-decoration: none;
    color: inherit;
  }
  .link-preview:hover {
    background: var(--bg-hover);
  }
  .lp-image {
    width: 80px;
    height: 80px;
    flex: none;
    object-fit: cover;
    border-radius: var(--radius-sm);
    background: var(--bg-panel);
  }
  .lp-text {
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 3px;
  }
  .lp-site {
    font-size: 11px;
    color: var(--text-muted);
    text-transform: uppercase;
    letter-spacing: 0.03em;
  }
  .lp-title {
    font-weight: 600;
    color: var(--accent, #5865f2);
    overflow: hidden;
    text-overflow: ellipsis;
    display: -webkit-box;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    -webkit-box-orient: vertical;
  }
  .lp-desc {
    font-size: 13px;
    color: var(--text-secondary, var(--text-muted));
    overflow: hidden;
    display: -webkit-box;
    -webkit-line-clamp: 3;
    line-clamp: 3;
    -webkit-box-orient: vertical;
  }
</style>
