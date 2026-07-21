<script lang="ts">
  import { fade } from "svelte/transition";
  import { RETENTION_OPTIONS } from "$lib/constants";

  let {
    name = $bindable(),
    category = $bindable(),
    announce = $bindable(),
    retention = $bindable(),
    voice = $bindable(),
    activeServer,
    categories,
    onclose,
    oncreate,
  }: {
    name: string;
    category: string;
    announce: boolean;
    retention: string;
    voice: boolean;
    activeServer: string;
    categories: string[];
    onclose: () => void;
    oncreate: () => void;
  } = $props();
</script>

<div class="modal-wrap" transition:fade|global={{ duration: 190 }}>
  <button class="modal-backdrop" aria-label="Close" onclick={onclose}></button>
  <div class="modal" role="dialog" aria-modal="true">
    <div class="modal-head">
      <h2>Create channel{activeServer ? ` in ${activeServer}` : ""}</h2>
      <button class="linkish" aria-label="Close" onclick={onclose}>✕</button>
    </div>
    <label class="fld">Channel name
      <input bind:value={name} placeholder="general" onkeydown={(e) => e.key === "Enter" && oncreate()} />
    </label>
    <label class="fld">Category (optional)
      <input list="cat-suggest" bind:value={category} placeholder="Text channels" onkeydown={(e) => e.key === "Enter" && oncreate()} />
    </label>
    <datalist id="cat-suggest">
      {#each categories as c (c)}<option value={c}></option>{/each}
    </datalist>
    <label class="check-row">
      <input type="checkbox" bind:checked={voice} />
      <span>🔊 Voice channel — members connect to talk; no text, and hidden from IRC clients</span>
    </label>
    {#if !voice}
      <label class="fld">Retention
        <select bind:value={retention}>
          <option value="">Server default</option>
          {#each RETENTION_OPTIONS as o (o.value)}<option value={o.value}>{o.label}</option>{/each}
        </select>
      </label>
      <label class="check-row">
        <input type="checkbox" bind:checked={announce} />
        <span>📢 Announcement channel — everyone can read, only members with the <code>send</code> permission can post</span>
      </label>
    {/if}
    <div class="modal-actions"><button class="ok-btn" disabled={!name.trim()} onclick={oncreate}>Create</button></div>
  </div>
</div>
