<script lang="ts">
  import { fade } from "svelte/transition";
  import { getApp } from "$lib/context";
  const app = getApp();
  let { onclose }: { onclose: () => void } = $props();
</script>

<div class="modal-wrap" transition:fade|global={{ duration: 190 }}>
  <button class="modal-backdrop" aria-label="Close" onclick={onclose}></button>
  <div class="modal settings-modal" role="dialog" aria-modal="true">
    <div class="modal-head">
      <h2>Settings</h2>
      <button class="icon-btn" aria-label="Close" onclick={onclose}>✕</button>
    </div>
    <div class="settings-sec">
      <h3>Account</h3>
      <div class="set-row"><span>Identity</span><b>{app.account}@{app.network}</b></div>
      <div class="set-row">
        <span>Status</span>
        <div class="status-inline">
          {#each ["online", "away", "dnd", "invisible"] as s (s)}
            <button class="chip-btn" class:on={app.myStatus === s} onclick={() => app.setStatus(s)}><span class="dot {s}"></span>{s}</button>
          {/each}
        </div>
      </div>
    </div>
    <div class="settings-sec">
      <h3>Appearance</h3>
      <div class="set-row">
        <span>Theme</span>
        <div class="status-inline">
          <button class="chip-btn" class:on={app.theme === "dark"} onclick={() => app.theme !== "dark" && app.toggleTheme()}>Dark</button>
          <button class="chip-btn" class:on={app.theme === "light"} onclick={() => app.theme !== "light" && app.toggleTheme()}>Light</button>
        </div>
      </div>
    </div>
    <div class="settings-sec">
      <h3>Device &amp; connection</h3>
      <div class="set-row"><span>Server</span><b>{app.host}{app.reconnecting ? " · reconnecting…" : ""}</b></div>
      <div class="set-row">
        <span>Passwordless login on this device</span>
        <button class="set-btn" onclick={app.enrollThisDevice}>Enroll device key</button>
      </div>
    </div>
    {#if app.isOperator}
      <div class="settings-sec">
        <h3>Network</h3>
        <div class="set-row">
          <span>Federation — bridges &amp; blocked networks (§11)</span>
          <button class="set-btn" onclick={app.openFederation}>Open</button>
        </div>
      </div>
    {/if}
    <div class="settings-sec danger-sec">
      <div class="modal-actions"><button class="danger-btn" onclick={app.logout}>Log out</button></div>
    </div>
  </div>
</div>
