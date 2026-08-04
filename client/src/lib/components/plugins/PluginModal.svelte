<script lang="ts">
  // A plugin's modal view (plugin-spec.md §11.2). The blocks are whatever the
  // plugin sent; the buttons drive the flow's next step.
  //
  // Dismissing sends `PLUGIN CLOSE`, which is terminal server-side — so a user
  // walking away frees the flow rather than leaving it parked for the session.
  import { fade } from "svelte/transition";
  import PluginBlock from "./PluginBlock.svelte";
  import { plugins, type OpenView } from "$lib/plugins/plugins.svelte";

  let { open }: { open: OpenView } = $props();

  // A view with no submit control of its own still needs a way out, and a form
  // is useless without one — so supply the footer unless the plugin drew it.
  const hasOwnSubmit = $derived((open.view.blocks ?? []).some((b) => b.type === "submit"));
</script>

<svelte:window onkeydown={(e) => e.key === "Escape" && plugins.close(open.id)} />

<div class="modal-wrap" transition:fade|global={{ duration: 150 }} style="z-index: 300">
  <button class="modal-backdrop" aria-label="Close" onclick={() => plugins.close(open.id)}></button>
  <div class="modal" role="dialog" aria-modal="true" style="width: min(520px, 100%)">
    <div class="modal-head">
      <h2>{open.view.title ?? "Plugin"}</h2>
    </div>

    <div class="sdui-body" class:busy={open.busy}>
      {#each open.view.blocks ?? [] as block, i (i)}
        <PluginBlock
          {block}
          bind:values={open.values}
          disabled={open.busy}
          onpress={(b) => plugins.press(open, b.id)}
          onsubmit={() => plugins.submit(open)}
        />
      {/each}
    </div>

    {#if !hasOwnSubmit}
      <div class="modal-actions">
        <button class="linkish" onclick={() => plugins.close(open.id)}>Cancel</button>
        <button class="ok-btn" disabled={open.busy} onclick={() => plugins.submit(open)}>
          {open.view.submit_label ?? "Submit"}
        </button>
      </div>
    {/if}
  </div>
</div>

<style>
  .sdui-body {
    max-height: min(60vh, 520px);
    overflow-y: auto;
  }
  /* Waiting on the plugin: keep the view legible but plainly not interactive. */
  .busy {
    opacity: 0.6;
    pointer-events: none;
  }
</style>
