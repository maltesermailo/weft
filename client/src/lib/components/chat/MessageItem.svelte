<script lang="ts">
  import { getApp } from "$lib/context";
  import { autofocus } from "$lib/actions";
  import EmojiPicker from "./EmojiPicker.svelte";
  import Attachment from "./Attachment.svelte";
  import Avatar from "$lib/components/Avatar.svelte";
  import LinkPreview from "./LinkPreview.svelte";
  import type { Msg } from "$lib/types";

  const app = getApp();
  let { m }: { m: Msg } = $props();

  // The first http(s) link in the body drives a single unfurl preview card
  // (Discord-style: one preview per message). System lines are skipped.
  const firstLink = $derived.by(() => {
    if (m.system) return null;
    const match = m.body.match(/https?:\/\/[^\s<>"']+/);
    return match ? match[0].replace(/[.,;:!?)\]]+$/, "") : null;
  });
</script>

{#if m.system}
  <div class="msg-group"><div style="width:34px;flex-shrink:0"></div><div class="msg-body"><div class="msg-line system">{m.body}</div></div></div>
{:else}
  <div class="msg-group" class:mention-hit={!m.own && app.mentionsMe(m.body)} class:pending={m.pending} id="msg-{m.key}" role="article" oncontextmenu={(e) => app.msgCtx(e, m)}>
    <!-- §10.3 avatar: local by bare handle, federated by `author@network`. -->
    <div class="avatar"><Avatar account={m.net ? `${m.author}@${m.net}` : m.author} /></div>
    <div class="msg-body">
      {#if m.replyTo}
        {@const rep = app.activeChannel?.messages.find((x) => x.msgid === m.replyTo)}
        <button class="reply-quote" onclick={() => app.jumpTo(m.replyTo)}>
          <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M9 17 4 12l5-5" /><path d="M20 18v-2a4 4 0 0 0-4-4H4" /></svg>
          {#if rep}<span class="rq-author">{rep.author}{#if rep.net}<span class="net-suffix">@{rep.net}</span>{/if}</span><span class="rq-body">{rep.body.slice(0, 90)}</span>{:else}<span class="rq-body">an earlier message</span>{/if}
        </button>
      {/if}
      <div class="msg-meta">
        {#if m.net}
          <!-- Foreign sender: fully qualified, and no local profile to open. -->
          <span class="author foreign" title="from {m.net}">{app.displayName(`${m.author}@${m.net}`)}<span class="net-suffix">@{m.net}</span></span>
          <!-- §11.11 recognition: a federated user's role(s) held on this network. -->
          {#each app.rolesOf(`${m.author}@${m.net}`, app.roleScopeOf(app.active)) as r (r.name)}
            <span class="role-pill" style="--role:{r.color}"><span class="role-dot"></span>{r.name}</span>
          {/each}
        {:else}
          <button class="author author-btn" onclick={(e) => app.openProfile(m.author, e)}>{app.displayName(m.author)}</button>
        {/if}
        {#if !m.net && app.badgeFor(m.author, app.active)?.owner}<span class="cap-badge owner">owner</span>
        {:else if !m.net && app.badgeFor(m.author, app.active)?.mod}<span class="cap-badge mod">mod</span>{/if}
        {#if m.own}<span class="cap-badge owner">you</span>{/if}
        <span class="time">{m.time}</span>
      </div>
      {#if app.editingKey === m.key}
        <textarea class="edit-box" rows="1" bind:value={app.editDraft} onkeydown={(e) => app.editKey(e, m)} use:autofocus></textarea>
        <div class="edit-hint">escape to <button class="linkish" onclick={app.cancelEdit}>cancel</button> · enter to <button class="linkish" onclick={() => app.saveEdit(m)}>save</button></div>
      {:else}
        <div class="msg-line">{#if m.md}{@html app.renderMd(m.body)}{:else}{m.body}{/if}{#if m.edited}<span class="edited-tag" title="edited">(edited)</span>{/if}</div>
      {/if}
      {#if m.attachments?.length}
        <div class="attachments">
          {#each m.attachments as uri (uri)}
            <Attachment {uri} />
          {/each}
        </div>
      {/if}
      {#if firstLink && app.editingKey !== m.key}
        <LinkPreview url={firstLink} />
      {/if}
      {#if m.reactions && Object.keys(m.reactions).length}
        <div class="reactions">
          {#each Object.entries(m.reactions) as [emoji, r] (emoji)}
            {@const custom = emoji.match(/^:([a-zA-Z0-9_]+):$/)}
            {@const emUrl = custom && app.emojiUrlFor(custom[1])}
            <button class="reaction" class:mine={r.mine} onclick={() => app.toggleReaction(m, emoji)}>
              {#if emUrl}<img class="custom-emoji" src={emUrl} alt={emoji} />{:else}<span>{emoji}</span>{/if}<span class="count">{r.count}</span>
            </button>
          {/each}
        </div>
      {/if}
      {#if app.active.startsWith("#") && app.threadCount(m.msgid) > 0}
        {@const n = app.threadCount(m.msgid)}
        {@const tn = app.threadNameFor(m.msgid)}
        <button class="thread-indicator" onclick={() => app.openThread(m)}>
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" /></svg>
          {#if tn}<span class="thread-indicator-name">{tn}</span>{/if}
          {n} {n === 1 ? "reply" : "replies"}
        </button>
      {/if}
    </div>
    {#if m.msgid && app.editingKey !== m.key}
      <div class="msg-actions">
        <button class="msg-act" title="React" aria-label="React" onclick={() => (app.pickerKey = app.pickerKey === m.key ? null : m.key)}>
          <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><circle cx="12" cy="12" r="9" /><path d="M8 14s1.5 2 4 2 4-2 4-2" /><path d="M9 9h.01M15 9h.01" /></svg>
        </button>
        <button class="msg-act" title="Reply" aria-label="Reply" onclick={() => (app.replyTo = m)}>
          <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M9 17 4 12l5-5" /><path d="M20 18v-2a4 4 0 0 0-4-4H4" /></svg>
        </button>
        {#if app.active.startsWith("#")}
          <button class="msg-act" title="Reply in thread" aria-label="Reply in thread" onclick={() => app.openThread(m)}>
            <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" /><path d="M8 9h8M8 13h5" /></svg>
          </button>
        {/if}
        {#if app.active.startsWith("#")}
          <button class="msg-act" class:on={app.activeChannel?.pinnedIds?.includes(m.msgid ?? "")} title={app.activeChannel?.pinnedIds?.includes(m.msgid ?? "") ? "Unpin" : "Pin"} aria-label="Pin" onclick={() => app.togglePin(m)}>
            <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M12 17v5" /><path d="M9 10.76a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24V16a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1v-.76a2 2 0 0 0-1.11-1.79l-1.78-.9A2 2 0 0 1 15 10.76V7a1 1 0 0 1 1-1 2 2 0 0 0 0-4H8a2 2 0 0 0 0 4 1 1 0 0 1 1 1z" /></svg>
          </button>
        {/if}
        {#if m.own}
          <button class="msg-act" title="Edit" aria-label="Edit" onclick={() => app.startEdit(m)}>
            <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M12 20h9" /><path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4Z" /></svg>
          </button>
          <button class="msg-act danger" title="Delete" aria-label="Delete" onclick={() => app.doDelete(m)}>
            <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M3 6h18M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2m2 0v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6" /></svg>
          </button>
        {:else}
          <button class="msg-act" title="Report" aria-label="Report" onclick={() => app.openReport(m)}>
            <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M4 15s1-1 4-1 5 2 8 2 4-1 4-1V3s-1 1-4 1-5-2-8-2-4 1-4 1z" /><line x1="4" y1="22" x2="4" y2="15" /></svg>
          </button>
        {/if}
      </div>
      {#if app.pickerKey === m.key}
        <div class="reaction-picker-pop">
          <EmojiPicker onpick={(v) => app.toggleReaction(m, v)} />
        </div>
      {/if}
    {/if}
  </div>
{/if}
