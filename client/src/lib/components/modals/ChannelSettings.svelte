<script lang="ts">
  import { fade } from "svelte/transition";
  import { untrack } from "svelte";
  import { getApp } from "$lib/context";
  import * as weft from "$lib/weft";
  import { CHAN_CAPS, RETENTION_OPTIONS } from "$lib/constants";
  import CapChecklist from "$lib/components/CapChecklist.svelte";
  const app = getApp();
  let { channel, onclose }: { channel: string; onclose: () => void } = $props();

  let tab = $state<"overview" | "permissions" | "danger">("overview");

  const rec = $derived(app.channels[channel]);
  const ns = $derived(app.nsOf(channel)); // "" for a top-level channel
  // Re-seed the editable fields when the channel identity changes (including a
  // rename, which swaps the prop) — but untrack the record reads so unrelated
  // channel events don't clobber an in-progress edit.
  let slug = $state("");
  let topic = $state("");
  $effect(() => {
    channel;
    untrack(() => {
      slug = app.chanShort(channel);
      topic = app.channels[channel]?.topic ?? "";
    });
  });

  function doRename() {
    const s = slug.trim().replace(/^#/, "").replace(/\s+/g, "-").toLowerCase();
    if (!s) return;
    const target = ns ? `#${ns}/${s}` : `#${s}`;
    if (target === channel) return;
    app.expectSuccess(`rename:${target}`, `Renamed to #${s}`);
    weft.channelRename(channel, target).catch((e) => app.toast(String(e), "error"));
  }
  function saveTopic() {
    weft.channelMeta(channel, "topic", topic).catch((e) => app.toast(String(e), "error"));
  }
  function setRetention(policy: string) {
    app.expectSuccess(`policy:${channel}`, "Retention updated");
    weft.channelPolicy(channel, policy).catch((e) => app.toast(String(e), "error"));
  }
  function deleteChannel() {
    weft
      .channelDelete(channel)
      .then(() => onclose())
      .catch((e) => app.toast(String(e), "error"));
  }
</script>

<div class="settings-overlay" role="dialog" aria-modal="true" transition:fade|global={{ duration: 150 }}>
  <nav class="so-nav">
    <div class="so-nav-inner">
      <div class="so-heading">#{app.chanShort(channel)}</div>
      <button class="so-navitem" class:active={tab === "overview"} onclick={() => (tab = "overview")}>Overview</button>
      <button class="so-navitem" class:active={tab === "permissions"} onclick={() => (tab = "permissions")}>Permissions</button>
      <div class="so-heading">Danger</div>
      <button class="so-navitem danger" class:active={tab === "danger"} onclick={() => (tab = "danger")}>Delete channel</button>
    </div>
  </nav>
  <main class="so-main">
    <button class="so-close" aria-label="Close settings" onclick={onclose}>✕<span>ESC</span></button>
    <div class="so-content">
      {#if tab === "overview"}
        <h1>Overview</h1>
        <p class="so-sub">The channel's address — members are moved automatically on rename.</p>
        <div class="field-label">Channel name</div>
        <div class="modal-join">
          <span class="chan-prefix">{ns ? `#${ns}/` : "#"}</span>
          <input class="text-input" bind:value={slug} onkeydown={(e) => e.key === "Enter" && doRename()} />
          <button disabled={!slug.trim()} onclick={doRename}>Rename</button>
        </div>

        <div class="section-sep"></div>
        <div class="field-label">Topic</div>
        <div class="modal-join">
          <input class="text-input" bind:value={topic} placeholder="what's this channel about" onkeydown={(e) => e.key === "Enter" && saveTopic()} />
          <button onclick={saveTopic}>Save</button>
        </div>

        <div class="section-sep"></div>
        <div class="field-label">Retention</div>
        <p class="so-sub">How long messages are kept. Switching to/from <code>e2ee</code> needs an empty channel or a purge.</p>
        <div class="cap-chips">
          {#each RETENTION_OPTIONS as o (o.value)}
            <button type="button" class="cap-chip" class:on={rec?.retention === o.key} onclick={() => setRetention(o.value)}>{o.label}</button>
          {/each}
        </div>

        <div class="section-sep"></div>
        <div class="field-label">Announcement mode</div>
        <div class="set-row">
          <span>Everyone reads (<code>view</code>), only members with <code>send</code> may post</span>
          <button class="chip-btn" class:on={rec?.restricted} onclick={app.toggleRestricted}>{rec?.restricted ? "On" : "Off"}</button>
        </div>
      {:else if tab === "permissions"}
        <h1>Permissions</h1>
        <div class="field-label">Role permissions</div>
        <p class="so-sub">Give a namespace role extra capabilities in this channel. Assigning the role applies them to the member.</p>
        {#each app.rolesByScope[app.chanNsScope()] ?? [] as role (role.name)}
          <div class="role-perm-block">
            <span class="role-pill" style="--role:{role.color}"><span class="role-dot"></span>{role.name}</span>
            <CapChecklist caps={CHAN_CAPS} selected={app.chanRoleCaps(role.name)} onToggle={(c) => app.toggleChanRoleCap(role, c)} />
          </div>
        {:else}
          <div class="empty-hint">No namespace roles yet — create some in Server Settings → Roles.</div>
        {/each}
        <p class="so-sub">Capabilities are granted only through roles — assign a member a role to give them these permissions.</p>
      {:else if tab === "danger"}
        <h1>Delete channel</h1>
        <p class="so-sub">Removes the channel and its history. This cannot be undone.</p>
        <div class="modal-actions"><button class="danger-btn" onclick={deleteChannel}>Delete #{app.chanShort(channel)}</button></div>
      {/if}
    </div>
  </main>
</div>
