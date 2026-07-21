<script lang="ts">
  import { getApp } from "$lib/context";
  import Avatar from "$lib/components/Avatar.svelte";
  import Attachment from "./Attachment.svelte";
  const app = getApp();

  function onKey(e: KeyboardEvent) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      app.sendThread();
    }
  }
</script>

{#if app.threadRoot}
  <aside class="thread-panel">
    <div class="thread-head">
      <div class="thread-title">
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" /></svg>
        Thread
      </div>
      <button class="linkish" aria-label="Close thread" onclick={app.closeThread}>✕</button>
    </div>
    <div class="thread-scroll">
      {#each app.threadMessages as m, i (m.key)}
        <div class="thread-msg" class:root={i === 0}>
          <div class="avatar sm"><Avatar account={m.net ? `${m.author}@${m.net}` : m.author} /></div>
          <div class="thread-body">
            <div class="thread-meta"><b>{app.displayName(m.author)}</b> <span class="time">{m.time}</span></div>
            <div class="msg-line">{#if m.md}{@html app.renderMd(m.body)}{:else}{m.body}{/if}</div>
            {#if m.attachments?.length}
              <div class="attachments">{#each m.attachments as uri (uri)}<Attachment {uri} />{/each}</div>
            {/if}
          </div>
        </div>
        {#if i === 0}<div class="thread-sep"><span>{app.threadCount(m.msgid)} {app.threadCount(m.msgid) === 1 ? "reply" : "replies"}</span></div>{/if}
      {/each}
    </div>
    <div class="thread-composer">
      <textarea rows="1" placeholder="Reply to thread…" bind:value={app.threadComposer} onkeydown={onKey}></textarea>
      <button class="icon-btn" title="Send" aria-label="Send reply" onclick={app.sendThread}>
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M22 2 11 13" /><path d="M22 2 15 22l-4-9-9-4 20-7z" /></svg>
      </button>
    </div>
  </aside>
{/if}

<style>
  .thread-panel {
    position: fixed;
    top: 0;
    right: 0;
    z-index: 40;
    width: min(400px, 92vw);
    height: 100vh;
    display: flex;
    flex-direction: column;
    background: var(--bg-panel);
    border-left: 1px solid var(--border-hair-strong);
    box-shadow: -8px 0 24px rgba(0, 0, 0, 0.25);
  }
  .thread-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 14px 16px;
    border-bottom: 1px solid var(--border-hair);
  }
  .thread-title {
    display: flex;
    align-items: center;
    gap: 8px;
    font-weight: 600;
  }
  .thread-scroll {
    flex: 1;
    overflow-y: auto;
    padding: 12px 14px;
  }
  .thread-msg {
    display: flex;
    gap: 10px;
    padding: 6px 0;
  }
  .thread-msg.root {
    padding-bottom: 10px;
  }
  .thread-body {
    min-width: 0;
    flex: 1;
  }
  .thread-meta {
    font-size: 12px;
    color: var(--text-muted);
    margin-bottom: 2px;
  }
  .thread-meta .time {
    margin-left: 6px;
  }
  .thread-sep {
    display: flex;
    align-items: center;
    gap: 10px;
    margin: 6px 0 10px;
    color: var(--text-faint);
    font-size: 11px;
  }
  .thread-sep::before,
  .thread-sep::after {
    content: "";
    flex: 1;
    height: 1px;
    background: var(--border-hair);
  }
  .thread-composer {
    display: flex;
    align-items: flex-end;
    gap: 8px;
    padding: 12px 14px 16px;
    border-top: 1px solid var(--border-hair);
  }
  .thread-composer textarea {
    flex: 1;
    resize: none;
    max-height: 120px;
    padding: 9px 10px;
    border-radius: var(--radius-md);
    border: 1px solid var(--border-hair-strong);
    background: var(--bg-panel-raised);
    color: var(--text-primary);
    font: inherit;
    font-size: 14px;
  }
</style>
