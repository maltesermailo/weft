<script lang="ts">
  import { fade } from "svelte/transition";
  import { getApp } from "$lib/context";
  import { autofocus } from "$lib/actions";
  import * as weft from "$lib/weft";
  import { store } from "$lib/models/store.svelte";
  import Avatar from "$lib/components/Avatar.svelte";
  const app = getApp();
  let { onclose }: { onclose: () => void } = $props();

  // This panel owns its own query + fetch; results stream in via the reducer
  // into `store.search`. It searches the active channel (server-side, §6.4).
  const search = store.search;
  let query = $state(search.query);

  function submit() {
    const q = query.trim();
    if (!q || !app.active.startsWith("#")) return;
    search.query = q;
    search.scope = app.active;
    search.results = [];
    search.buf = [];
    search.loading = true;
    search.loadingChannel = app.active;
    weft.search(app.active, q).catch((e) => {
      search.loadingChannel = null;
      search.loading = false;
      app.toast(String(e), "error");
    });
  }
  function jumpToResult(m: { msgid?: string }) {
    search.open = false;
    app.jumpTo(m.msgid); // best-effort: scrolls if the message is loaded
  }
</script>

<div class="modal-wrap" transition:fade|global={{ duration: 190 }}>
  <button class="modal-backdrop" aria-label="Close" onclick={onclose}></button>
  <div class="modal" role="dialog" aria-modal="true">
    <div class="modal-head">
      <h2>Search — {app.chanShort(search.scope || app.active)}</h2>
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
      {#if search.loading}
        <div class="empty-hint">Searching…</div>
      {:else if search.results.length}
        <div class="search-count">{search.results.length} result{search.results.length === 1 ? "" : "s"}</div>
        {#each search.results as m (m.key)}
          <button class="search-card" onclick={() => jumpToResult(m)}>
            <div class="avatar sm"><Avatar account={m.net ? `${m.author}@${m.net}` : m.author} /></div>
            <div class="search-body">
              <div class="search-meta"><b>{app.displayName(m.author)}</b> <span class="time">{m.time}</span></div>
              <div class="msg-line">{#if m.md}{@html app.renderMd(m.body)}{:else}{m.body}{/if}</div>
            </div>
          </button>
        {/each}
      {:else if search.query}
        <div class="empty-hint">No messages match “{search.query}”.</div>
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
