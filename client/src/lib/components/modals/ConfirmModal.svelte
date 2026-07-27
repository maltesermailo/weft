<script lang="ts">
  // In-app confirmation dialog — the Tauri webview blocks native window.confirm,
  // so destructive actions route through this promise-backed modal instead.
  import { fade } from "svelte/transition";
  let {
    message,
    confirmLabel = "Confirm",
    danger = true,
    onresult,
  }: {
    message: string;
    confirmLabel?: string;
    danger?: boolean;
    onresult: (ok: boolean) => void;
  } = $props();
</script>

<svelte:window onkeydown={(e) => e.key === "Escape" && onresult(false)} />

<div class="modal-wrap" transition:fade|global={{ duration: 150 }} style="z-index: 300">
  <button class="modal-backdrop" aria-label="Cancel" onclick={() => onresult(false)}></button>
  <div class="modal" role="dialog" aria-modal="true" style="width: min(420px, 100%)">
    <div class="modal-head"><h2>Are you sure?</h2></div>
    <p class="modal-sub">{message}</p>
    <div class="modal-actions">
      <button class="linkish" onclick={() => onresult(false)}>Cancel</button>
      <button class:danger-btn={danger} class:ok-btn={!danger} onclick={() => onresult(true)}>{confirmLabel}</button>
    </div>
  </div>
</div>
