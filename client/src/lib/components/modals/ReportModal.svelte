<script lang="ts">
  import { fade } from "svelte/transition";
  import { getApp } from "$lib/context";
  import * as weft from "$lib/weft";
  import { REPORT_CATEGORIES } from "$lib/constants";
  import type { Msg } from "$lib/types";

  const app = getApp();
  let { target, onclose }: { target: Msg; onclose: () => void } = $props();

  let category = $state("spam");
  let scope = $state(app.nsOf(app.active) || app.activeServer ? "ns" : "net");
  let note = $state("");

  function submit() {
    if (target.msgid) weft.report(target.msgid, category, scope, note || undefined).catch(() => {});
    onclose();
  }
</script>

<div class="modal-wrap" transition:fade|global={{ duration: 190 }}>
  <button class="modal-backdrop" aria-label="Close" onclick={onclose}></button>
  <div class="modal" role="dialog" aria-modal="true">
    <div class="modal-head">
      <h2>Report message</h2>
      <button class="linkish" aria-label="Close" onclick={onclose}>✕</button>
    </div>
    <p class="modal-sub">from <b>{target.author}</b> — “{target.body.slice(0, 80)}”</p>
    <label class="fld">Category
      <select bind:value={category}>
        {#each REPORT_CATEGORIES as c (c)}<option value={c}>{c}</option>{/each}
      </select>
    </label>
    <label class="fld">Route to
      <select bind:value={scope}>
        <option value="ns">namespace moderators</option>
        <option value="net">network operators</option>
      </select>
    </label>
    <label class="fld">Note (optional)
      <input bind:value={note} placeholder="context for the moderators…" />
    </label>
    <div class="modal-actions">
      <button class="linkish" onclick={onclose}>Cancel</button>
      <button class="danger-btn" onclick={submit}>Submit report</button>
    </div>
  </div>
</div>
