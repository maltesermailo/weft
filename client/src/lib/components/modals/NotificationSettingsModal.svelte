<script lang="ts">
  import { fade } from "svelte/transition";
  import { getApp } from "$lib/context";
  const app = getApp();
  let { onclose }: { onclose: () => void } = $props();

  const scope = $derived(app.notifScopeKey());
  const level = $derived(app.notifLevelOf(scope));

  const OPTIONS = [
    {
      value: "all",
      label: "All messages",
      desc: "Notify me for every message in this namespace.",
    },
    {
      value: "mentions",
      label: "Only @mentions",
      desc: "Notify me only for @mentions and direct messages.",
    },
    {
      value: "nothing",
      label: "Nothing",
      desc: "Mute this namespace — no notifications, no unread badges.",
    },
  ];
</script>

<div class="modal-wrap" transition:fade|global={{ duration: 190 }}>
  <button class="modal-backdrop" aria-label="Close" onclick={onclose}></button>
  <div class="modal" role="dialog" aria-modal="true">
    <div class="modal-head">
      <h2>Notifications — {app.notifScopeLabel()}</h2>
      <button class="linkish" aria-label="Close" onclick={onclose}>✕</button>
    </div>
    <p class="modal-sub">
      Choose how this namespace notifies you. This is your personal setting,
      saved on this device.
    </p>
    <div class="notif-options">
      {#each OPTIONS as o (o.value)}
        <button
          class="notif-option"
          class:on={level === o.value}
          onclick={() => app.setNotifLevel(scope, o.value)}
        >
          <span class="notif-radio" aria-hidden="true"></span>
          <span class="notif-text">
            <span class="notif-label">{o.label}</span>
            <span class="notif-desc">{o.desc}</span>
          </span>
        </button>
      {/each}
    </div>
  </div>
</div>

<style>
  .notif-options {
    display: flex;
    flex-direction: column;
    gap: 8px;
    margin-top: 10px;
  }
  .notif-option {
    display: flex;
    align-items: flex-start;
    gap: 12px;
    padding: 12px 14px;
    border-radius: 8px;
    border: 1px solid var(--border-hair-strong);
    background: var(--bg-panel);
    color: var(--text-primary);
    cursor: pointer;
    text-align: left;
    font: inherit;
  }
  .notif-option:hover {
    background: var(--bg-hover);
  }
  .notif-option.on {
    border-color: var(--accent, #5865f2);
    background: color-mix(in srgb, var(--accent, #5865f2) 12%, transparent);
  }
  .notif-radio {
    flex-shrink: 0;
    width: 16px;
    height: 16px;
    margin-top: 2px;
    border-radius: 50%;
    border: 2px solid var(--text-faint);
  }
  .notif-option.on .notif-radio {
    border-color: var(--accent, #5865f2);
    background:
      radial-gradient(circle, var(--accent, #5865f2) 0 4px, transparent 5px);
  }
  .notif-text {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }
  .notif-label {
    font-size: 14px;
    font-weight: 600;
  }
  .notif-desc {
    font-size: 12.5px;
    color: var(--text-muted);
  }
</style>
