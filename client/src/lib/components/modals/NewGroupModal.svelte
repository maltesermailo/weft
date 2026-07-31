<script lang="ts">
  import { roster, friendLocalAccount } from "$lib/models/social.svelte";
  import { friendLabel, peerOf } from "$lib/profile.svelte";
  import { fade } from "svelte/transition";
  import { getApp } from "$lib/context";
  import Avatar from "$lib/components/Avatar.svelte";

  let {
    seed,
    pos = null,
    onclose,
    oncreate,
  }: {
    // The DM peer this group grows out of (full `account@network`) — always in.
    // Empty from the Friends view: no pre-included peer, pick everyone yourself.
    seed: string;
    // Anchor point (under the button that opened it); null = centered fallback.
    pos?: { left: number; top: number } | null;
    onclose: () => void;
    oncreate: (members: string[]) => void;
  } = $props();

  const app = getApp();

  const hasSeed = $derived(!!seed);
  // Friends you can add — everyone except the peer who's already included.
  const candidates = $derived(roster.friends.filter((u) => u !== seed));
  let picked = $state<Set<string>>(new Set());
  function toggle(u: string) {
    const next = new Set(picked);
    if (next.has(u)) next.delete(u);
    else next.add(u);
    picked = next;
  }
  const avatarAccount = (u: string) => friendLocalAccount(u) ?? peerOf(u);
  // A seeded group needs ≥1 more; a seedless one needs ≥2 friends to be a group.
  const minPick = $derived(hasSeed ? 1 : 2);
  const total = $derived(picked.size + (hasSeed ? 1 : 0));
</script>

<!-- Transparent catcher: closes on outside click without dimming the screen. -->
<button class="ng-catcher" aria-label="Close" onclick={onclose}></button>
<div
  class="ng-pop"
  class:centered={!pos}
  role="dialog"
  aria-modal="false"
  transition:fade|global={{ duration: 110 }}
  style={pos ? `left:${pos.left}px; top:${pos.top}px` : ""}
>
  <div class="ng-head">
    <span class="ng-title">New group DM</span>
    <button class="ng-x" aria-label="Close" onclick={onclose}>✕</button>
  </div>
  <p class="ng-sub">
    {#if hasSeed}
      Add friends to a group with <strong>{friendLabel(seed)}</strong>.
    {:else}
      Pick friends to start a group DM.
    {/if}
  </p>

  <div class="ng-list" role="listbox" aria-label="Friends to add">
    {#each candidates as u (u)}
      <button
        class="ng-row"
        class:on={picked.has(u)}
        role="option"
        aria-selected={picked.has(u)}
        onclick={() => toggle(u)}
      >
        <span class="avatar sm"><Avatar account={avatarAccount(u)} /></span>
        <span class="ng-name">{friendLabel(u)}</span>
        <span class="ng-check" class:on={picked.has(u)} aria-hidden="true">
          {#if picked.has(u)}<svg width="11" height="11" viewBox="0 0 12 12" fill="none"><polyline points="2 6 5 9 10 3" stroke="#fff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" /></svg>{/if}
        </span>
      </button>
    {:else}
      <div class="ng-empty">No friends to add yet — add friends first.</div>
    {/each}
  </div>

  <button
    class="ng-create"
    disabled={picked.size < minPick}
    onclick={() => oncreate(hasSeed ? [seed, ...picked] : [...picked])}
  >
    {picked.size >= minPick ? `Create group (${total})` : "Create group"}
  </button>
</div>

<style>
  .ng-catcher {
    position: fixed;
    inset: 0;
    z-index: 500;
    border: none;
    background: transparent;
    cursor: default;
  }
  .ng-pop {
    position: fixed;
    z-index: 501;
    width: 300px;
    max-width: calc(100vw - 16px);
    max-height: min(400px, calc(100vh - 24px));
    display: flex;
    flex-direction: column;
    padding: 12px;
    background: var(--bg-elevated, #1b1e27);
    border: 1px solid var(--border-hair-strong);
    border-radius: 10px;
    box-shadow: 0 12px 40px rgba(0, 0, 0, 0.5);
  }
  .ng-pop.centered {
    left: 50%;
    top: 50%;
    transform: translate(-50%, -50%);
  }
  .ng-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 2px;
  }
  .ng-title {
    font-size: 14px;
    font-weight: 700;
    color: var(--text-primary);
  }
  .ng-x {
    border: none;
    background: none;
    color: var(--text-faint);
    font-size: 13px;
    cursor: pointer;
    padding: 2px 4px;
    border-radius: 5px;
  }
  .ng-x:hover {
    color: var(--text-primary);
    background: var(--bg-hover);
  }
  .ng-sub {
    font-size: 12px;
    color: var(--text-muted);
    margin-bottom: 8px;
    line-height: 1.35;
  }
  .ng-list {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 1px;
    margin-bottom: 10px;
  }
  .ng-row {
    display: flex;
    align-items: center;
    gap: 9px;
    width: 100%;
    padding: 6px 7px;
    border: none;
    border-radius: 7px;
    background: transparent;
    color: var(--text-primary);
    cursor: pointer;
    text-align: left;
  }
  .ng-row:hover {
    background: var(--bg-hover, rgba(255, 255, 255, 0.05));
  }
  .ng-row.on {
    background: color-mix(in srgb, var(--accent) 14%, transparent);
  }
  .ng-name {
    flex: 1;
    font-size: 13px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .ng-check {
    width: 18px;
    height: 18px;
    flex-shrink: 0;
    border: 2px solid var(--border-hair-strong);
    border-radius: 5px;
    display: grid;
    place-items: center;
  }
  .ng-check.on {
    background: var(--accent, #5865f2);
    border-color: var(--accent, #5865f2);
  }
  .ng-empty {
    padding: 16px 8px;
    text-align: center;
    color: var(--text-muted);
    font-size: 12px;
  }
  .ng-create {
    flex-shrink: 0;
    width: 100%;
    padding: 9px;
    border: none;
    border-radius: 7px;
    background: var(--accent, #5865f2);
    color: #fff;
    font: inherit;
    font-size: 13px;
    font-weight: 600;
    cursor: pointer;
  }
  .ng-create:hover:not(:disabled) {
    background: color-mix(in srgb, var(--accent) 85%, #000);
  }
  .ng-create:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }
</style>
