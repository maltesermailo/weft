<script lang="ts">
  import { getApp } from "$lib/context";
  const app = getApp();
</script>

<nav class="warp-rail" aria-label="Networks">
  <button class="rail-home" class:active={app.homeView} title="Direct messages" aria-label="Direct messages" onclick={app.goHome}>
    <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><path d="M21 11.5a8.38 8.38 0 0 1-.9 3.8 8.5 8.5 0 0 1-7.6 4.7 8.38 8.38 0 0 1-3.8-.9L3 21l1.9-5.7a8.38 8.38 0 0 1-.9-3.8 8.5 8.5 0 0 1 4.7-7.6 8.38 8.38 0 0 1 3.8-.9h.5a8.48 8.48 0 0 1 8 8v.5z" /></svg>
  </button>
  <div class="rail-divider"></div>
  <div class="rail-communities">
    <div class="comm-tile" class:active={!app.homeView && app.activeServer === ""} class:muted={app.serverMuted("")} title={app.network}>
      <button onclick={() => app.selectServer("")}>{app.initials(app.network)}</button>
      <span class="trust-mark signed" title="Connected network"></span>
    </div>
    {#each app.serverNamespaces as ns (ns)}
      <div class="comm-tile" class:active={!app.homeView && app.activeServer === ns} class:muted={app.serverMuted(ns)} title={ns}>
        <button onclick={() => app.selectServer(ns)}>{app.initials(ns)}</button>
        {#if app.serverMentionCount(ns)}<span class="tile-badge mention">{app.serverMentionCount(ns)}</span>
        {:else if app.serverUnread(ns) && !app.serverMuted(ns)}<span class="tile-badge"></span>{/if}
      </div>
    {/each}
  </div>
  <button class="rail-add" title="Discover namespaces" aria-label="Discover namespaces" onclick={app.openDiscover}>
    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M12 5v14M5 12h14" /></svg>
  </button>
</nav>
