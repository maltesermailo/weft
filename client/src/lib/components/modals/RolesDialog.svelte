<script lang="ts">
  import { fade } from "svelte/transition";
  import { getApp } from "$lib/context";
  import * as weft from "$lib/weft";
  import { CAPS } from "$lib/constants";

  const app = getApp();
  let { target, onclose }: { target: string; onclose: () => void } = $props();

  let scope = $state(app.scopesFor()[0]);
  let caps = $state<string[]>([]);
  const toggle = (c: string) => (caps = caps.includes(c) ? caps.filter((x) => x !== c) : [...caps, c]);

  function grant() {
    app.expectSuccess(`caps:${target}|${scope}`, `Permissions updated for ${target}`);
    weft.grant(target, scope, caps.join(",")).catch((e) => app.toast(String(e), "error"));
  }
  function revoke() {
    app.expectSuccess(`caps:${target}|${scope}`, `Permissions updated for ${target}`);
    weft.revoke(target, scope, caps.join(",")).catch((e) => app.toast(String(e), "error"));
  }
</script>

<div class="modal-wrap" transition:fade|global={{ duration: 190 }}>
  <button class="modal-backdrop" aria-label="Close" onclick={onclose}></button>
  <div class="modal" role="dialog" aria-modal="true">
    <div class="modal-head">
      <h2>Roles — {target}</h2>
      <button class="linkish" aria-label="Close" onclick={onclose}>✕</button>
    </div>
    <label class="fld">Scope
      <select bind:value={scope}>
        {#each app.scopesFor() as s (s)}<option value={s}>{s}</option>{/each}
      </select>
    </label>
    <div class="fld">
      Capabilities
      <div class="cap-chips">
        {#each CAPS as c (c)}
          <button type="button" class="cap-chip" class:on={caps.includes(c)} onclick={() => toggle(c)}>{c}</button>
        {/each}
      </div>
    </div>
    <div class="modal-actions">
      <button class="danger-btn" disabled={!caps.length} onclick={revoke}>Revoke</button>
      <button class="ok-btn" disabled={!caps.length} onclick={grant}>Grant</button>
    </div>
    <p class="modal-sub">Select one or more caps. Grants are additive; revoking bumps the scope's epoch.</p>
  </div>
</div>
