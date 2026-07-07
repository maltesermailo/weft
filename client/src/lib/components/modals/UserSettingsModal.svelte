<script lang="ts">
  import { fade } from "svelte/transition";
  import { getApp } from "$lib/context";
  const app = getApp();
  let { onclose }: { onclose: () => void } = $props();
</script>

<div class="settings-overlay" role="dialog" aria-modal="true" transition:fade|global={{ duration: 150 }}>
  <nav class="so-nav">
    <div class="so-nav-inner">
      <div class="so-heading">{app.account}</div>
      <button class="so-navitem" class:active={app.userTab === "account"} onclick={() => (app.userTab = "account")}>Account</button>
      <button class="so-navitem" class:active={app.userTab === "appearance"} onclick={() => (app.userTab = "appearance")}>Appearance</button>
      <button class="so-navitem" class:active={app.userTab === "connection"} onclick={() => (app.userTab = "connection")}>Device &amp; connection</button>
      <div class="so-heading">Session</div>
      <button class="so-navitem danger" onclick={app.logout}>Log out</button>
    </div>
  </nav>
  <main class="so-main">
    <button class="so-close" aria-label="Close settings" onclick={onclose}>✕<span>ESC</span></button>
    <div class="so-content">
      {#if app.userTab === "account"}
        <h1>Account</h1>
        <p class="so-sub">Your identity on this network.</p>
        <div class="set-row"><span>Identity</span><b>{app.account}@{app.network}</b></div>
        <div class="section-sep"></div>
        <div class="field-label">Status</div>
        <div class="status-inline">
          {#each ["online", "away", "dnd", "invisible"] as s (s)}
            <button class="chip-btn" class:on={app.myStatus === s} onclick={() => app.setStatus(s)}><span class="dot {s}"></span>{s}</button>
          {/each}
        </div>
        {#if app.isOperator}
          <div class="section-sep"></div>
          <div class="field-label">Network defense</div>
          <p class="so-sub">Block abusive peer networks and manage network-wide bridges (§11). Per-namespace federation lives in each namespace's Server Settings.</p>
          <button class="set-btn" onclick={app.openFederation}>Open network federation</button>
        {/if}
      {:else if app.userTab === "appearance"}
        <h1>Appearance</h1>
        <p class="so-sub">Theme for this device.</p>
        <div class="field-label">Theme</div>
        <div class="status-inline">
          <button class="chip-btn" class:on={app.theme === "dark"} onclick={() => app.theme !== "dark" && app.toggleTheme()}>Dark</button>
          <button class="chip-btn" class:on={app.theme === "light"} onclick={() => app.theme !== "light" && app.toggleTheme()}>Light</button>
        </div>
      {:else if app.userTab === "connection"}
        <h1>Device &amp; connection</h1>
        <p class="so-sub">This device's link to the network.</p>
        <div class="set-row"><span>Server</span><b>{app.host}{app.reconnecting ? " · reconnecting…" : ""}</b></div>
        <div class="section-sep"></div>
        <div class="set-row">
          <span>Passwordless login on this device</span>
          <button class="set-btn" onclick={app.enrollThisDevice}>Enroll device key</button>
        </div>
      {/if}
    </div>
  </main>
</div>
