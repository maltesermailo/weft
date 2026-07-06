<script lang="ts">
  import type { CtxItem } from "$lib/types";

  let {
    menu,
    onclose,
  }: {
    menu: { x: number; y: number; items: CtxItem[] } | null;
    onclose: () => void;
  } = $props();
</script>

{#if menu}
  <button class="ctx-backdrop" aria-label="Close menu" onclick={onclose}></button>
  <div class="ctx-menu" style="left:{menu.x}px; top:{menu.y}px">
    {#each menu.items as it (it.label)}
      <button class="ctx-item" class:danger={it.danger} onclick={() => { it.run(); onclose(); }}>{it.label}</button>
    {/each}
  </div>
{/if}
