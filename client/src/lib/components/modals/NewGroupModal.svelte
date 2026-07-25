<script lang="ts">
  import { fade } from "svelte/transition";
  import { getApp } from "$lib/context";
  import Avatar from "$lib/components/Avatar.svelte";

  let {
    seed,
    onclose,
    oncreate,
  }: {
    // The DM peer this group grows out of (full `account@network`) — always in.
    seed: string;
    onclose: () => void;
    oncreate: (members: string[]) => void;
  } = $props();

  const app = getApp();

  // Friends you can add — everyone except the peer who's already included.
  const candidates = $derived(app.friendList.filter((u) => u !== seed));
  let picked = $state<Set<string>>(new Set());
  function toggle(u: string) {
    const next = new Set(picked);
    if (next.has(u)) next.delete(u);
    else next.add(u);
    picked = next;
  }
  const avatarAccount = (u: string) => app.friendLocalAccount(u) ?? app.peerOf(u);
</script>

<div class="modal-wrap" transition:fade|global={{ duration: 190 }}>
  <button class="modal-backdrop" aria-label="Close" onclick={onclose}></button>
  <div class="modal" role="dialog" aria-modal="true">
    <div class="modal-head">
      <h2>New group</h2>
      <button class="linkish" aria-label="Close" onclick={onclose}>✕</button>
    </div>
    <p class="modal-sub">
      Start a group DM with <strong>{app.friendLabel(seed)}</strong> and the friends you pick.
    </p>

    <div class="picker-list" role="listbox" aria-label="Friends to add">
      {#each candidates as u (u)}
        <button
          class="picker-row"
          class:on={picked.has(u)}
          role="option"
          aria-selected={picked.has(u)}
          onclick={() => toggle(u)}
        >
          <span class="avatar sm"><Avatar account={avatarAccount(u)} /></span>
          <span class="picker-name">{app.friendLabel(u)}</span>
          <span class="picker-check" aria-hidden="true">{picked.has(u) ? "✓" : ""}</span>
        </button>
      {:else}
        <div class="picker-empty">No other friends to add yet — add friends first.</div>
      {/each}
    </div>

    <div class="modal-actions">
      <button class="ok-btn" disabled={!picked.size} onclick={() => oncreate([seed, ...picked])}>
        {picked.size ? `Create group (${picked.size + 1})` : "Create group"}
      </button>
    </div>
  </div>
</div>

<style>
  .picker-list {
    max-height: 320px;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 2px;
    margin: 4px 0 8px;
  }
  .picker-row {
    display: flex;
    align-items: center;
    gap: 10px;
    width: 100%;
    padding: 7px 8px;
    border: none;
    border-radius: 7px;
    background: transparent;
    color: var(--text-primary);
    cursor: pointer;
    text-align: left;
  }
  .picker-row:hover {
    background: var(--bg-hover, rgba(255, 255, 255, 0.05));
  }
  .picker-row.on {
    background: var(--accent-soft, rgba(88, 101, 242, 0.16));
  }
  .picker-name {
    flex: 1;
    font-size: 14px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .picker-check {
    width: 18px;
    text-align: center;
    color: var(--accent, #5865f2);
    font-weight: 700;
  }
  .picker-empty {
    padding: 16px;
    text-align: center;
    color: var(--text-muted);
    font-size: 13px;
  }
</style>
