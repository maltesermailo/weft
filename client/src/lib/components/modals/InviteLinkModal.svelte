<script lang="ts">
  import { fade } from "svelte/transition";
  import * as weft from "$lib/weft";
  let { link, id, onclose }: { link: string; id: string | null; onclose: () => void } = $props();

  function revoke() {
    if (id) weft.inviteRevoke(id).catch(() => {});
    onclose();
  }
</script>

<div class="modal-wrap" transition:fade|global={{ duration: 190 }}>
  <button class="modal-backdrop" aria-label="Close" onclick={onclose}></button>
  <div class="modal" role="dialog" aria-modal="true">
    <div class="modal-head">
      <h2>Invite link</h2>
      <button class="linkish" aria-label="Close" onclick={onclose}>✕</button>
    </div>
    <p class="modal-sub">Share this to let someone join:</p>
    <div class="modal-join">
      <input readonly value={link} />
      <button onclick={() => navigator.clipboard?.writeText(link)}>Copy</button>
    </div>
    {#if id}
      <div class="modal-actions">
        <button class="danger-btn" onclick={revoke}>Revoke invite</button>
      </div>
    {/if}
  </div>
</div>
