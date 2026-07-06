<script lang="ts">
  import { fade } from "svelte/transition";
  let {
    name = $bindable(),
    onclose,
    oncreate,
  }: {
    name: string;
    onclose: () => void;
    oncreate: () => void;
  } = $props();
</script>

<div class="modal-wrap" transition:fade|global={{ duration: 190 }}>
  <button class="modal-backdrop" aria-label="Close" onclick={onclose}></button>
  <div class="modal" role="dialog" aria-modal="true">
    <div class="modal-head">
      <h2>Create category</h2>
      <button class="linkish" aria-label="Close" onclick={onclose}>✕</button>
    </div>
    <p class="modal-sub">A category groups channels. Drag channels into it once created.</p>
    <label class="fld">Category name
      <input bind:value={name} placeholder="Text channels" onkeydown={(e) => e.key === "Enter" && oncreate()} />
    </label>
    <div class="modal-actions"><button class="ok-btn" disabled={!name.trim()} onclick={oncreate}>Create</button></div>
  </div>
</div>
