<script lang="ts">
  import { getApp } from "$lib/context";
  const app = getApp();

  // Drag-and-drop file upload: highlight the composer while a file hovers.
  let dragActive = $state(false);
  function onDragOver(e: DragEvent) {
    if (!app.active || !e.dataTransfer?.types.includes("Files")) return;
    e.preventDefault();
    dragActive = true;
  }
  function onDrop(e: DragEvent) {
    dragActive = false;
    app.dropFiles(e);
  }
</script>

<div
  class="composer-wrap"
  class:drag-active={dragActive}
  ondragover={onDragOver}
  ondragleave={() => (dragActive = false)}
  ondrop={onDrop}
  role="group"
>
  {#if dragActive}
    <div class="drop-hint">Drop files to attach</div>
  {/if}
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
  {#if app.pendingAttachments.length}
    <div class="attach-tray">
      {#each app.pendingAttachments as a, i (a.uri)}
        <div class="attach-chip" title={a.name}>
          {#if a.mime.startsWith("image/")}
            <img src={app.mediaUrl(a.thumb ?? a.uri)} alt={a.name} />
          {:else}
            <span class="attach-icon">📎</span>
          {/if}
          <span class="attach-name">{a.name}</span>
          <button class="attach-x" aria-label="Remove attachment" onclick={() => app.removeAttachment(i)}>✕</button>
        </div>
      {/each}
    </div>
  {/if}
  <div class="composer">
    <button class="icon-btn" title="Attach a file" aria-label="Attach a file" disabled={!app.active} onclick={app.attachFile}>
      <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="m21.44 11.05-9.19 9.19a6 6 0 0 1-8.49-8.49l9.19-9.19a4 4 0 0 1 5.66 5.66l-9.2 9.19a2 2 0 0 1-2.83-2.83l8.49-8.48" /></svg>
    </button>
    <textarea
      rows="1"
      placeholder={app.active ? `Message ${app.active}…` : "Join a channel first"}
      disabled={!app.active}
      bind:value={app.composer}
      onkeydown={app.composerKey}
      oninput={app.onComposerInput}
      onpaste={app.pasteFiles}
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
