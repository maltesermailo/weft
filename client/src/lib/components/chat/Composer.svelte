<script lang="ts">
  import { getApp } from "$lib/context";
  const app = getApp();
</script>

<div class="composer-wrap">
  {#if app.mentionQuery !== null && app.mentionMatches.length}
    <div class="mention-pop">
      {#each app.mentionMatches as name, i (name)}
        <button class="mention-opt" class:first={i === 0} onclick={() => app.pickMention(name)}>
          <span class="mention-sigil">@</span>{name}
        </button>
      {/each}
    </div>
  {/if}
  {#if app.replyTo}
    <div class="reply-bar">
      <span>replying to <b>{app.replyTo.author}</b></span>
      <button class="linkish" onclick={() => (app.replyTo = null)} aria-label="Cancel reply">✕</button>
    </div>
  {/if}
  <div class="composer">
    <textarea
      rows="1"
      placeholder={app.active ? `Message ${app.active}…` : "Join a channel first"}
      disabled={!app.active}
      bind:value={app.composer}
      onkeydown={app.composerKey}
      oninput={app.onComposerInput}
    ></textarea>
    <button class="icon-btn" title="Send" aria-label="Send message" onclick={app.doSend}>
      <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M22 2 11 13" /><path d="M22 2 15 22l-4-9-9-4 20-7z" /></svg>
    </button>
  </div>
  <div class="composer-hint">
    {#if app.typingLabel}
      <span class="typing">{app.typingLabel}</span>
    {:else}
      <span><span class="k">Enter</span> send</span>
      <span><span class="k">Shift+Enter</span> newline</span>
    {/if}
  </div>
</div>
