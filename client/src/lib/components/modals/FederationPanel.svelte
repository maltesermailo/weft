<script lang="ts">
  import { fade } from "svelte/transition";
  import { getApp } from "$lib/context";
  const app = getApp();
  let { onclose }: { onclose: () => void } = $props();

  let nbNetwork = $state("");
  let nbReason = $state("");
  function addBlock() {
    const n = nbNetwork.trim();
    if (!n) return;
    app.netblockAdd(n, nbReason.trim() || undefined);
    nbNetwork = "";
    nbReason = "";
  }

  let brPeer = $state("");
  let brScope = $state("*");
  let brHistory = $state("from-epoch");
  let brMedia = $state("none");
  let brTyping = $state(true);
  function propose() {
    const p = brPeer.trim();
    if (!p) return;
    app.bridgePropose(brScope.trim() || "*", p, brHistory, brMedia, brTyping);
    brPeer = "";
  }
</script>

<div class="modal-wrap" transition:fade|global={{ duration: 190 }}>
  <button class="modal-backdrop" aria-label="Close" onclick={onclose}></button>
  <div class="modal wide" role="dialog" aria-modal="true">
    <div class="modal-head">
      <h2>Federation</h2>
      <button class="linkish" aria-label="Close" onclick={onclose}>✕</button>
    </div>
    <p class="modal-sub">Network peering (§11), operator-only. Netblocks, inbound peering, and accept/sever work today; <b>outbound</b> bridge transmission needs the M5d dialer.</p>

    <div class="section-sep"></div>
    <div class="field-label">Blocked networks</div>
    <div class="modal-list">
      {#each Object.entries(app.netblocks) as [nw, reason] (nw)}
        <div class="ns-card">
          <div class="ns-info">
            <div class="ns-name">{nw}</div>
            <div class="ns-desc">{reason || "blocked"}</div>
          </div>
          <button class="mini-danger" onclick={() => app.netblockRemove(nw)}>Unblock</button>
        </div>
      {:else}
        <div class="empty-hint">No blocked networks.</div>
      {/each}
    </div>
    <div class="modal-join">
      <input bind:value={nbNetwork} placeholder="network to block…" onkeydown={(e) => e.key === "Enter" && addBlock()} />
      <input bind:value={nbReason} placeholder="reason (optional)" />
      <button onclick={addBlock}>Block</button>
    </div>

    <div class="section-sep"></div>
    <div class="field-label">Bridges</div>
    <div class="modal-list">
      {#each Object.values(app.manifests) as m (m.peer)}
        <div class="ns-card">
          <div class="ns-info">
            <div class="ns-name">{m.peer} <span class="rep-state {m.state}">{m.state}</span> · v{m.version}</div>
            <div class="ns-desc">{m.channels.length} channel(s) · history {m.history} · media {m.media}{m.typing ? " · typing" : ""}</div>
          </div>
          <div class="fed-actions">
            <button onclick={() => app.bridgeAccept(m.peer, m.version)}>Accept</button>
            <button class="mini-danger" onclick={() => app.bridgeSever(m.peer)}>Sever</button>
          </div>
        </div>
      {:else}
        <div class="empty-hint">No bridges yet — propose one below, or wait for an inbound peer.</div>
      {/each}
    </div>

    <div class="field-label">Propose a bridge</div>
    <div class="modal-join">
      <input bind:value={brPeer} placeholder="peer network" />
      <input bind:value={brScope} placeholder="scope (e.g. * or ns:gaming)" />
    </div>
    <div class="fed-propose">
      <select bind:value={brHistory}>
        <option value="from-epoch">history: from-epoch</option>
        <option value="full">history: full</option>
      </select>
      <select bind:value={brMedia}>
        <option value="none">media: none</option>
        <option value="mirror">media: mirror</option>
      </select>
      <label class="fed-check"><input type="checkbox" bind:checked={brTyping} /> typing</label>
      <button class="ok-btn" onclick={propose}>Propose</button>
    </div>
  </div>
</div>
