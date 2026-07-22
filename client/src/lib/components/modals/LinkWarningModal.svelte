<script lang="ts">
  import { fade } from "svelte/transition";
  import { linkPrompt, closeLink, openConfirmed } from "$lib/linkguard.svelte";

  // Host of the real destination (falls back to the raw URL if it won't parse).
  const host = $derived.by(() => {
    try {
      return new URL(linkPrompt.url).host;
    } catch {
      return linkPrompt.url;
    }
  });

  // Masked-link phishing heuristic: the visible text names a domain that isn't
  // where the link actually goes (e.g. [paypal.com](http://evil.example)).
  const mismatch = $derived.by(() => {
    const m = (linkPrompt.text ?? "").match(/\b([a-z0-9-]+\.)+[a-z]{2,}\b/i);
    if (!m) return false;
    try {
      const claimed = m[0].toLowerCase().replace(/^www\./, "");
      const real = new URL(linkPrompt.url).host.toLowerCase().replace(/^www\./, "");
      return claimed !== real && !real.endsWith("." + claimed);
    } catch {
      return false;
    }
  });

  function onkeydown(e: KeyboardEvent) {
    if (e.key === "Escape") closeLink();
  }
</script>

<svelte:window {onkeydown} />

{#if linkPrompt.open}
  <div class="modal-wrap" transition:fade|global={{ duration: 160 }}>
    <button class="modal-backdrop" aria-label="Cancel" onclick={closeLink}></button>
    <div class="modal link-warn" role="dialog" aria-modal="true">
      <div class="modal-head">
        <h2>Open external link?</h2>
        <button class="linkish" aria-label="Cancel" onclick={closeLink}>✕</button>
      </div>
      <p class="modal-sub">This will open in your browser:</p>
      <div class="link-warn-url" title={linkPrompt.url}>{linkPrompt.url}</div>
      {#if linkPrompt.text && linkPrompt.text.trim() !== linkPrompt.url}
        <p class="link-warn-host">Destination: <b>{host}</b></p>
      {/if}
      {#if mismatch}
        <p class="link-warn-danger">
          ⚠️ The link text names a different site than its real destination. Only continue if you trust it.
        </p>
      {/if}
      <div class="modal-actions">
        <button class="linkish" onclick={closeLink}>Cancel</button>
        <button class="link-warn-open" class:danger={mismatch} onclick={openConfirmed}>Open link</button>
      </div>
    </div>
  </div>
{/if}
