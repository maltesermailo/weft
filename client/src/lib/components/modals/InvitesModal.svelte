<script lang="ts">
  import { fade } from "svelte/transition";
  import { getApp } from "$lib/context";
  import InviteList from "$lib/components/InviteList.svelte";
  const app = getApp();
  let { onclose }: { onclose: () => void } = $props();
</script>

<div class="modal-wrap" transition:fade|global={{ duration: 190 }}>
  <button class="modal-backdrop" aria-label="Close" onclick={onclose}></button>
  <div class="modal" role="dialog" aria-modal="true">
    <div class="modal-head">
      <h2>Invites — {app.invitesScope}</h2>
      <button class="linkish" aria-label="Close" onclick={onclose}>✕</button>
    </div>
    <div class="modal-list">
      <InviteList />
    </div>
  </div>
</div>
