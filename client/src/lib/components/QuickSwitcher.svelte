<script lang="ts">
  import { autofocus } from "$lib/actions";

  type Result = { name: string; label: string; sigil: string; unread: boolean };

  let {
    query = $bindable(),
    results,
    onselect,
    onclose,
  }: {
    query: string;
    results: Result[];
    onselect: (name: string) => void;
    onclose: () => void;
  } = $props();
</script>

<div class="modal-wrap switcher-wrap">
  <button class="modal-backdrop" aria-label="Close" onclick={onclose}></button>
  <div class="modal switcher" role="dialog" aria-modal="true">
    <input
      class="switcher-input"
      bind:value={query}
      placeholder="Jump to a channel or DM…"
      use:autofocus
      onkeydown={(e) => {
        if (e.key === "Enter" && results[0]) onselect(results[0].name);
      }}
    />
    <div class="switcher-list">
      {#each results as c (c.name)}
        <button class="switcher-item" onclick={() => onselect(c.name)}>
          <span class="si-sigil">{c.sigil}</span>
          <span>{c.label}</span>
          {#if c.unread}<span class="unread-dot"></span>{/if}
        </button>
      {:else}
        <div class="empty-hint">No matches.</div>
      {/each}
    </div>
  </div>
</div>
