<script lang="ts">
  import { fade } from "svelte/transition";
  import { getApp } from "$lib/context";
  const app = getApp();
  let { onclose }: { onclose: () => void } = $props();

  // A short preview of the root when a thread has no name yet.
  function preview(root: string): string {
    const m = app.activeChannel?.messages.find((msg) => msg.msgid === root);
    if (m && m.body) return m.body.length > 60 ? m.body.slice(0, 60) + "…" : m.body;
    return "Thread";
  }
</script>

<div class="modal-wrap" transition:fade|global={{ duration: 190 }}>
  <button class="modal-backdrop" aria-label="Close" onclick={onclose}></button>
  <div class="modal" role="dialog" aria-modal="true">
    <div class="modal-head">
      <h2>Threads — {app.chanShort(app.active)}</h2>
      <button class="linkish" aria-label="Close" onclick={onclose}>✕</button>
    </div>
    <div class="modal-list">
      {#each app.threadsList as t (t.root)}
        <button class="thread-card" onclick={() => app.openThreadByRoot(t)}>
          <div class="thread-icon">
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" /></svg>
          </div>
          <div class="thread-info">
            <div class="thread-name">{t.name ?? preview(t.root)}</div>
            <div class="thread-sub">{t.replies} {t.replies === 1 ? "reply" : "replies"}</div>
          </div>
        </button>
      {:else}
        <div class="empty-hint">No threads in this channel yet.</div>
      {/each}
    </div>
  </div>
</div>

<style>
  .thread-card {
    display: flex;
    align-items: center;
    gap: 12px;
    width: 100%;
    padding: 10px 12px;
    border: none;
    background: transparent;
    border-radius: var(--radius-md);
    cursor: pointer;
    text-align: left;
    color: var(--text-primary);
  }
  .thread-card:hover {
    background: var(--bg-panel-raised);
  }
  .thread-icon {
    display: flex;
    color: var(--text-muted);
  }
  .thread-info {
    min-width: 0;
    flex: 1;
  }
  .thread-name {
    font-weight: 600;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .thread-sub {
    font-size: 12px;
    color: var(--text-muted);
  }
</style>
