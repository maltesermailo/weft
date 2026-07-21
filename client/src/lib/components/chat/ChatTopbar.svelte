<script lang="ts">
  import { getApp } from "$lib/context";
  const app = getApp();
</script>

<div class="chat-topbar">
  {#if app.activeChannel && app.activeIsDm}
    <div class="chan-title">
      <span class={app.dotClass(app.peerOf(app.active))}></span>
      <span>{app.peerOf(app.active)}</span>
    </div>
    <div class="topic">{app.presence[app.peerOf(app.active)] ?? "offline"}</div>
  {:else if app.activeChannel}
    {@const meta = app.retentionMeta[app.activeChannel.retention]}
    <div class="chan-title">
      <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M4 9h16M4 15h16M10 3 8 21M16 3l-2 18" /></svg>
      <span>{app.chanShort(app.activeChannel.name)}</span>
    </div>
    <div class="topic">{app.activeChannel.topic ?? ""}</div>
    <div class="status-chip">
      <span style="display:flex;color:var(--{meta.cls})"><svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7">{@html meta.icon}</svg></span>{meta.label}
    </div>
  {:else}
    <div class="chan-title"><span>no channel</span></div>
    <div class="topic"></div>
  {/if}
  <div class="topbar-actions">
    {#if app.activeChannel && !app.activeIsDm}
      <button class="icon-btn" title="Search messages" aria-label="Search messages" onclick={app.openSearch}>
        <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><circle cx="11" cy="11" r="7" /><path d="m21 21-4.3-4.3" /></svg>
      </button>
      <button class="icon-btn" title="Pinned messages" aria-label="Pinned messages" onclick={app.openPins}>
        <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M12 17v5" /><path d="M9 10.76a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24V16a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1v-.76a2 2 0 0 0-1.11-1.79l-1.78-.9A2 2 0 0 1 15 10.76V7a1 1 0 0 1 1-1 2 2 0 0 0 0-4H8a2 2 0 0 0 0 4 1 1 0 0 1 1 1z" /></svg>
      </button>
      <button class="icon-btn" title="Invite" aria-label="Invite" onclick={app.mintInvite}>
        <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2" /><circle cx="9" cy="7" r="4" /><line x1="19" y1="8" x2="19" y2="14" /><line x1="22" y1="11" x2="16" y2="11" /></svg>
      </button>
      <button class="icon-btn" title="Reports queue" aria-label="Reports queue" onclick={app.openReports}>
        <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M4 15s1-1 4-1 5 2 8 2 4-1 4-1V3s-1 1-4 1-5-2-8-2-4 1-4 1z" /><line x1="4" y1="22" x2="4" y2="15" /></svg>
      </button>
      <button class="icon-btn" title="Leave channel" aria-label="Leave channel" onclick={app.partActive}>
        <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4" /><path d="m16 17 5-5-5-5" /><path d="M21 12H9" /></svg>
      </button>
    {/if}
    <button class="icon-btn" title="Toggle member list" aria-label="Toggle member list" onclick={() => (app.membersVisible = !app.membersVisible)}>
      <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2" /><circle cx="9" cy="7" r="4" /><path d="M23 21v-2a4 4 0 0 0-3-3.87" /><path d="M16 3.13a4 4 0 0 1 0 7.75" /></svg>
    </button>
  </div>
</div>
