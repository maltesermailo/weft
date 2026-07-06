<script lang="ts">
  import { fade } from "svelte/transition";
  import { getApp } from "$lib/context";
  import * as weft from "$lib/weft";
  const app = getApp();
  let { onclose }: { onclose: () => void } = $props();

  let discoverName = $state("");
  let redeemInput = $state("");
  let createName = $state("");
  let createVis = $state("public");

  function joinNamespace(name: string) {
    weft.nsJoin(name).catch(() => {});
    weft.channels(name).catch(() => {}); // fetch its category layout
    onclose();
  }
  function joinNamespaceInput() {
    const n = discoverName.trim().replace(/^@?/, "");
    discoverName = "";
    if (n) joinNamespace(n);
  }
  function doRedeem() {
    const t = redeemInput.trim();
    redeemInput = "";
    if (t) weft.inviteRedeem(t).catch(() => {});
    onclose();
  }
  async function createNamespace() {
    const name = createName.trim().replace(/^@/, "");
    if (!name) return;
    createName = "";
    try {
      // Root keypair is generated + stored on-device; then a default channel.
      await weft.nsCreate(app.network, name, createVis);
      await weft.channelCreate(`#${name}/general`);
      await weft.join(`#${name}/general`);
      onclose();
    } catch (e) {
      app.toast(String(e), "error");
    }
  }
</script>

<div class="modal-wrap" transition:fade|global={{ duration: 190 }}>
  <button class="modal-backdrop" aria-label="Close" onclick={onclose}></button>
  <div class="modal" role="dialog" aria-modal="true">
    <div class="modal-head">
      <h2>Discover namespaces</h2>
      <button class="linkish" aria-label="Close" onclick={onclose}>✕</button>
    </div>
    <div class="modal-join">
      <input bind:value={discoverName} placeholder="join a namespace by name…" onkeydown={(e) => e.key === "Enter" && joinNamespaceInput()} />
      <button onclick={joinNamespaceInput}>Join</button>
    </div>
    <div class="modal-join">
      <input bind:value={redeemInput} placeholder="redeem an invite link…" onkeydown={(e) => e.key === "Enter" && doRedeem()} />
      <button onclick={doRedeem}>Redeem</button>
    </div>
    <div class="modal-join">
      <input bind:value={createName} placeholder="create a namespace…" onkeydown={(e) => e.key === "Enter" && createNamespace()} />
      <select bind:value={createVis} aria-label="Visibility">
        <option value="public">public</option>
        <option value="unlisted">unlisted</option>
        <option value="private">private</option>
      </select>
      <button onclick={createNamespace}>Create</button>
    </div>
    <div class="modal-list">
      {#each Object.values(app.discovered) as ns (ns.name)}
        <div class="ns-card">
          <div class="ns-info">
            <div class="ns-name">{ns.title || ns.name}</div>
            <div class="ns-desc">
              {ns.description || `@${ns.name}`} · {ns.visibility}{ns.owner ? ` · ${ns.owner}` : ""}
            </div>
          </div>
          <button onclick={() => joinNamespace(ns.name)}>Join</button>
        </div>
      {:else}
        <div class="empty-hint">No public namespaces found.</div>
      {/each}
      {#if app.discoverCursor}
        <button class="linkish load-more" onclick={() => weft.discover(app.discoverCursor ?? undefined)}>Load more…</button>
      {/if}
    </div>
  </div>
</div>
