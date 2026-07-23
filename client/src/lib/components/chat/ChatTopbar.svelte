<script lang="ts">
  import { getApp } from "$lib/context";
  const app = getApp();
</script>

<div class="chat-topbar">
  {#if app.activeChannel && app.activeIsGroup}
    <div class="chan-title">
      <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2" /><circle cx="9" cy="7" r="4" /><path d="M23 21v-2a4 4 0 0 0-3-3.87" /><path d="M16 3.13a4 4 0 0 1 0 7.75" /></svg>
      <span>{app.groupLabel(app.active)}</span>
    </div>
    <div class="topic">group DM</div>
  {:else if app.activeChannel && app.activeIsDm}
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
    {#if app.activeIsDm}
      <button class="icon-btn" title="Start call" aria-label="Start call" disabled={!!app.activeCall} onclick={() => app.callUser(app.peerOf(app.active))}>
        <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M22 16.92v3a2 2 0 0 1-2.18 2 19.79 19.79 0 0 1-8.63-3.07 19.5 19.5 0 0 1-6-6 19.79 19.79 0 0 1-3.07-8.67A2 2 0 0 1 4.11 2h3a2 2 0 0 1 2 1.72 12.84 12.84 0 0 0 .7 2.81 2 2 0 0 1-.45 2.11L8.09 9.91a16 16 0 0 0 6 6l1.27-1.27a2 2 0 0 1 2.11-.45 12.84 12.84 0 0 0 2.81.7A2 2 0 0 1 22 16.92z" /></svg>
      </button>
    {/if}
    {#if app.activeIsGroup}
      {@const inCall = app.activeGroupCall === app.active}
      {@const roster = app.groupCallRoster[app.active] ?? []}
      <button
        class="icon-btn"
        class:in-call={inCall}
        title={inCall ? "Leave call" : "Start / join call"}
        aria-label={inCall ? "Leave call" : "Start or join call"}
        onclick={() => (inCall ? app.leaveGroupCall(app.active) : app.startGroupCall(app.active))}
      >
        {#if inCall}
          <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M10.68 13.31a16 16 0 0 0 3.41 2.6l1.27-1.27a2 2 0 0 1 2.11-.45 12.84 12.84 0 0 0 2.81.7 2 2 0 0 1 1.72 2v3a2 2 0 0 1-2.18 2 19.79 19.79 0 0 1-8.63-3.07 19.42 19.42 0 0 1-3.33-2.67m-2.67-3.34a19.79 19.79 0 0 1-3.07-8.63A2 2 0 0 1 4.11 2h3a2 2 0 0 1 2 1.72 12.84 12.84 0 0 0 .7 2.81 2 2 0 0 1-.45 2.11L8.09 9.91" /><line x1="23" y1="1" x2="1" y2="23" /></svg>
        {:else}
          <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M22 16.92v3a2 2 0 0 1-2.18 2 19.79 19.79 0 0 1-8.63-3.07 19.5 19.5 0 0 1-6-6 19.79 19.79 0 0 1-3.07-8.67A2 2 0 0 1 4.11 2h3a2 2 0 0 1 2 1.72 12.84 12.84 0 0 0 .7 2.81 2 2 0 0 1-.45 2.11L8.09 9.91a16 16 0 0 0 6 6l1.27-1.27a2 2 0 0 1 2.11-.45 12.84 12.84 0 0 0 2.81.7A2 2 0 0 1 22 16.92z" /></svg>
        {/if}
      </button>
      {#if roster.length}
        <span class="call-count" title="{roster.length} in call">{roster.length}</span>
      {/if}
      <button class="icon-btn" title="Leave group" aria-label="Leave group" onclick={() => app.leaveGroup(app.active)}>
        <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4" /><path d="m16 17 5-5-5-5" /><path d="M21 12H9" /></svg>
      </button>
    {/if}
    {#if app.activeChannel && !app.activeIsDm && !app.activeIsGroup}
      <button class="icon-btn" title="Search messages" aria-label="Search messages" onclick={app.openSearch}>
        <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><circle cx="11" cy="11" r="7" /><path d="m21 21-4.3-4.3" /></svg>
      </button>
      <button class="icon-btn" title="Pinned messages" aria-label="Pinned messages" onclick={app.openPins}>
        <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M12 17v5" /><path d="M9 10.76a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24V16a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1v-.76a2 2 0 0 0-1.11-1.79l-1.78-.9A2 2 0 0 1 15 10.76V7a1 1 0 0 1 1-1 2 2 0 0 0 0-4H8a2 2 0 0 0 0 4 1 1 0 0 1 1 1z" /></svg>
      </button>
      <button class="icon-btn" title="Threads" aria-label="Threads" onclick={app.openThreads}>
        <svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" /><path d="M8 9h8" /><path d="M8 13h5" /></svg>
      </button>
      <button class="icon-btn" title="Invite" aria-label="Invite" onclick={app.openInvites}>
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

<style>
  /* Active (in-call) tint for the group-call button. */
  .icon-btn.in-call {
    color: #3ba55d;
  }
  /* Small badge showing how many members are in the group call. */
  .call-count {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    min-width: 18px;
    height: 18px;
    padding: 0 5px;
    margin-left: -4px;
    border-radius: 9px;
    background: #3ba55d;
    color: #fff;
    font-size: 11px;
    font-weight: 700;
  }
</style>
