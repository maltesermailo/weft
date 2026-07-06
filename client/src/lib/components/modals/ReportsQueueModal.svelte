<script lang="ts">
  import { fade } from "svelte/transition";
  import { getApp } from "$lib/context";
  import * as weft from "$lib/weft";
  const app = getApp();
  let { onclose }: { onclose: () => void } = $props();
</script>

<div class="modal-wrap" transition:fade|global={{ duration: 190 }}>
  <button class="modal-backdrop" aria-label="Close" onclick={onclose}></button>
  <div class="modal" role="dialog" aria-modal="true">
    <div class="modal-head">
      <h2>Reports — {app.activeServer ? `ns:${app.activeServer}` : "network"}</h2>
      <button class="linkish" aria-label="Close" onclick={onclose}>✕</button>
    </div>
    <div class="modal-list">
      {#each Object.values(app.reportQueue) as r (r.report_id)}
        <div class="ns-card report-card">
          <div class="ns-info">
            <div class="ns-name">{r.category} <span class="rep-state {r.state}">{r.state}</span></div>
            <div class="ns-desc">{r.report_id} · {r.msgid.slice(0, 16)}…{r.reporter ? ` · by ${r.reporter}` : ""}</div>
          </div>
          <select onchange={(e) => weft.reportsResolve(r.report_id, e.currentTarget.value).catch(() => {})}>
            <option value="">resolve…</option>
            {#each app.resolveActions as a (a)}<option value={a}>{a}</option>{/each}
          </select>
        </div>
      {:else}
        <div class="empty-hint">No open reports.</div>
      {/each}
    </div>
  </div>
</div>
