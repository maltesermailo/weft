<script lang="ts">
  import { fade } from "svelte/transition";
  import { getApp } from "$lib/context";
  import { autofocus } from "$lib/actions";
  import Avatar from "$lib/components/Avatar.svelte";
  const app = getApp();
  let { onclose }: { onclose: () => void } = $props();

  let query = $state(app.searchQuery);
  function submit() {
    if (query.trim()) app.runSearch(query);
  }
</script>

<div class="modal-wrap" transition:fade|global={{ duration: 190 }}>
  <button class="modal-backdrop" aria-label="Close" onclick={onclose}></button>
  <div class="modal" role="dialog" aria-modal="true">
    <div class="modal-head">
      <h2>Search — {app.chanShort(app.searchScope || app.active)}</h2>
      <button class="linkish" aria-label="Close" onclick={onclose}>✕</button>
    </div>
    <div class="search-input">
      <input
        bind:value={query}
        placeholder="Search this channel…"
        onkeydown={(e) => e.key === "Enter" && submit()}
        use:autofocus
      />
      <button class="ok-btn" disabled={!query.trim()} onclick={submit}>Search</button>
    </div>
    <div class="modal-list">
      {#if app.searching}
        <div class="empty-hint">Searching…</div>
      {:else if app.searchResults.length}
        <div class="search-count">{app.searchResults.length} result{app.searchResults.length === 1 ? "" : "s"}</div>
        {#each app.searchResults as m (m.key)}
          <button class="search-card" onclick={() => app.jumpToResult(m)}>
            <div class="avatar sm"><Avatar account={m.net ? `${m.author}@${m.net}` : m.author} /></div>
            <div class="search-body">
              <div class="search-meta"><b>{app.displayName(m.author)}</b> <span class="time">{m.time}</span></div>
              <div class="msg-line">{#if m.md}{@html app.renderMd(m.body)}{:else}{m.body}{/if}</div>
            </div>
          </button>
        {/each}
      {:else if app.searchQuery}
        <div class="empty-hint">No messages match “{app.searchQuery}”.</div>
      {:else}
        <div class="empty-hint">Type a query to search this channel's messages.</div>
      {/if}
    </div>
  </div>
</div>

<style>
  .search-input {
    display: flex;
    gap: 8px;
    margin-bottom: 10px;
  }
  .search-input input {
    flex: 1;
    padding: 8px 10px;
    border-radius: 6px;
    border: 1px solid var(--border-hair-strong);
    background: var(--bg-panel);
    color: var(--text-primary);
    font: inherit;
  }
  .search-count {
    font-size: 12px;
    color: var(--text-muted);
    margin-bottom: 6px;
  }
  .search-card {
    display: flex;
    gap: 10px;
    width: 100%;
    padding: 8px;
    border: none;
    border-radius: 8px;
    background: none;
    color: var(--text-primary);
    cursor: pointer;
    text-align: left;
  }
  .search-card:hover {
    background: var(--bg-hover);
  }
  .search-body {
    min-width: 0;
    flex: 1;
  }
  .search-meta {
    font-size: 12px;
    color: var(--text-muted);
    margin-bottom: 2px;
  }
  .search-meta .time {
    margin-left: 6px;
  }
</style>
