<script lang="ts">
  import { getApp } from "$lib/context";
  import { highlightComposer } from "$lib/mdhighlight";
  import EmojiPicker from "./EmojiPicker.svelte";
  const app = getApp();

  let emojiOpen = $state(false);
  function insertEmoji(value: string) {
    app.composer = app.composer + value;
    emojiOpen = false;
  }

  // Live Markdown preview (GitHub-style Write/Preview toggle — full render).
  let previewOn = $state(false);

  // Live inline syntax highlighting: an overlay behind a transparent textarea
  // colours valid markdown as you type. `highlightComposer` preserves the text
  // character-for-character (colour only, never weight/size), so the caret stays
  // aligned with the overlay.
  let overlay = $state<HTMLDivElement | null>(null);
  const highlighted = $derived(highlightComposer(app.composer));

  // While an IME composition is active the textarea's own (transparent) preedit
  // text would be invisible, so temporarily show the textarea and hide the
  // overlay until the composition commits.
  let composing = $state(false);

  // Auto-grow the textarea to fit its content, capped at a share of the viewport
  // height (vh) so a long draft scrolls internally instead of eating the screen —
  // and the cap scales uniformly across resolutions rather than a fixed px.
  const MAX_VH = 40;
  let ta = $state<HTMLTextAreaElement | null>(null);
  function autosize() {
    const el = ta;
    if (!el) return;
    const maxH = (window.innerHeight * MAX_VH) / 100;
    el.style.height = "auto";
    el.style.height = Math.min(el.scrollHeight, maxH) + "px";
    el.style.overflowY = el.scrollHeight > maxH ? "auto" : "hidden";
  }
  // Keep the highlight overlay scrolled in lockstep with the textarea.
  function syncScroll() {
    if (overlay && ta) {
      overlay.scrollTop = ta.scrollTop;
      overlay.scrollLeft = ta.scrollLeft;
    }
  }
  // React to every composer change — typing, emoji insert, mention pick, and the
  // reset to "" after send all flow through `app.composer`.
  $effect(() => {
    app.composer;
    autosize();
    syncScroll();
  });

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
  {:else if app.emojiQuery !== null && app.emojiSuggestions.length}
    <div class="mention-pop">
      {#each app.emojiSuggestions as em, i (em.name)}
        <button class="mention-opt" class:first={i === 0} onclick={() => app.pickEmojiSuggestion(em.name)}>
          <img class="custom-emoji" src={em.url ?? ''} alt="" /> :{em.name}:
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
  {#if previewOn && app.composer.trim()}
    <div class="composer-preview">
      <div class="composer-preview-label">Preview</div>
      <div class="msg-line">{@html app.renderMd(app.composer)}</div>
    </div>
  {/if}
  <div class="composer">
    <button class="icon-btn" title="Attach a file" aria-label="Attach a file" disabled={!app.active} onclick={app.attachFile}>
      <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="m21.44 11.05-9.19 9.19a6 6 0 0 1-8.49-8.49l9.19-9.19a4 4 0 0 1 5.66 5.66l-9.2 9.19a2 2 0 0 1-2.83-2.83l8.49-8.48" /></svg>
    </button>
    <div class="composer-input" class:composing>
      <div class="composer-highlight" bind:this={overlay} aria-hidden="true">{@html highlighted}<br /></div>
      <textarea
        class="composer-ta"
        bind:this={ta}
        rows="1"
        placeholder={app.active ? `Message ${app.active}…` : "Join a channel first"}
        disabled={!app.active}
        bind:value={app.composer}
        onkeydown={app.composerKey}
        oninput={app.onComposerInput}
        onscroll={syncScroll}
        onpaste={app.pasteFiles}
        oncompositionstart={() => (composing = true)}
        oncompositionend={() => (composing = false)}
      ></textarea>
    </div>
    <button
      class="icon-btn"
      class:active={previewOn}
      title="Toggle Markdown preview"
      aria-label="Toggle Markdown preview"
      aria-pressed={previewOn}
      disabled={!app.active}
      onclick={() => (previewOn = !previewOn)}
    >
      <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7-10-7-10-7z" /><circle cx="12" cy="12" r="3" /></svg>
    </button>
    <div class="composer-emoji">
      {#if emojiOpen}
        <button class="ctx-backdrop" aria-label="Close" onclick={() => (emojiOpen = false)}></button>
        <div class="composer-emoji-pop"><EmojiPicker onpick={insertEmoji} /></div>
      {/if}
      <button class="icon-btn" title="Emoji" aria-label="Emoji" disabled={!app.active} onclick={() => (emojiOpen = !emojiOpen)}>
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><circle cx="12" cy="12" r="9" /><path d="M8 14s1.5 2 4 2 4-2 4-2" /><path d="M9 9h.01M15 9h.01" /></svg>
      </button>
    </div>
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
