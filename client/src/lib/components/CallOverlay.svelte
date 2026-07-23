<script lang="ts">
  import { fade, fly } from "svelte/transition";
  import { getApp } from "$lib/context";
  import Avatar from "$lib/components/Avatar.svelte";
  const app = getApp();

  // Short handle for a caller/peer userref.
  const label = (u: string) => app.friendLabel(u);
</script>

<!-- Incoming call — ringing modal with accept/decline -->
{#if app.incomingCall}
  <div class="call-ring-wrap" transition:fade|global={{ duration: 160 }}>
    <div class="call-ring" transition:fly|global={{ y: 16, duration: 200 }}>
      <div class="avatar lg ringing"><Avatar account={app.incomingCall.from} /></div>
      <div class="call-name">{label(app.incomingCall.from)}</div>
      <div class="call-sub">Incoming call…</div>
      <div class="call-actions">
        <button class="call-btn decline" title="Decline" aria-label="Decline" onclick={app.declineCall}>
          <svg width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M10.68 13.31a16 16 0 0 0 3.41 2.6l1.27-1.27a2 2 0 0 1 2.11-.45 12.84 12.84 0 0 0 2.81.7 2 2 0 0 1 1.72 2v3a2 2 0 0 1-2.18 2 19.79 19.79 0 0 1-8.63-3.07 19.42 19.42 0 0 1-3.33-2.67m-2.67-3.34a19.79 19.79 0 0 1-3.07-8.63A2 2 0 0 1 4.11 2h3a2 2 0 0 1 2 1.72 12.84 12.84 0 0 0 .7 2.81 2 2 0 0 1-.45 2.11L8.09 9.91" /><line x1="23" y1="1" x2="1" y2="23" /></svg>
        </button>
        <button class="call-btn accept" title="Accept" aria-label="Accept" onclick={app.acceptCall}>
          <svg width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M22 16.92v3a2 2 0 0 1-2.18 2 19.79 19.79 0 0 1-8.63-3.07 19.5 19.5 0 0 1-6-6 19.79 19.79 0 0 1-3.07-8.67A2 2 0 0 1 4.11 2h3a2 2 0 0 1 2 1.72 12.84 12.84 0 0 0 .7 2.81 2 2 0 0 1-.45 2.11L8.09 9.91a16 16 0 0 0 6 6l1.27-1.27a2 2 0 0 1 2.11-.45 12.84 12.84 0 0 0 2.81.7A2 2 0 0 1 22 16.92z" /></svg>
        </button>
      </div>
    </div>
  </div>
{/if}

<!-- Active / outgoing call — a persistent bar -->
{#if app.activeCall}
  <div class="call-bar" transition:fly|global={{ y: 20, duration: 200 }}>
    <span class="avatar sm"><Avatar account={app.activeCall.peer} /></span>
    <div class="call-bar-text">
      <b>{label(app.activeCall.peer)}</b>
      <span class="call-bar-state">
        {app.callConnecting ? "Connecting…" : app.activeCall.state === "ringing" ? "Calling…" : "In call"}
      </span>
    </div>
    <button
      class="call-btn mute small"
      class:active={app.callMuted}
      title={app.callMuted ? "Unmute" : "Mute"}
      aria-label={app.callMuted ? "Unmute" : "Mute"}
      onclick={app.toggleCallMute}
    >
      {#if app.callMuted}
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><line x1="1" y1="1" x2="23" y2="23" /><path d="M9 9v3a3 3 0 0 0 5.12 2.12M15 9.34V4a3 3 0 0 0-5.94-.6" /><path d="M17 16.95A7 7 0 0 1 5 12v-2m14 0v2a7 7 0 0 1-.11 1.23" /><line x1="12" y1="19" x2="12" y2="23" /></svg>
      {:else}
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M12 1a3 3 0 0 0-3 3v8a3 3 0 0 0 6 0V4a3 3 0 0 0-3-3z" /><path d="M19 10v2a7 7 0 0 1-14 0v-2" /><line x1="12" y1="19" x2="12" y2="23" /></svg>
      {/if}
    </button>
    <button class="call-btn decline small" title="Hang up" aria-label="Hang up" onclick={app.endCall}>
      <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M10.68 13.31a16 16 0 0 0 3.41 2.6l1.27-1.27a2 2 0 0 1 2.11-.45 12.84 12.84 0 0 0 2.81.7 2 2 0 0 1 1.72 2v3a2 2 0 0 1-2.18 2 19.79 19.79 0 0 1-8.63-3.07 19.42 19.42 0 0 1-3.33-2.67m-2.67-3.34a19.79 19.79 0 0 1-3.07-8.63A2 2 0 0 1 4.11 2h3a2 2 0 0 1 2 1.72 12.84 12.84 0 0 0 .7 2.81 2 2 0 0 1-.45 2.11L8.09 9.91" /><line x1="23" y1="1" x2="1" y2="23" /></svg>
    </button>
  </div>
{/if}

<style>
  .call-ring-wrap {
    position: fixed;
    inset: 0;
    z-index: 200;
    display: flex;
    align-items: center;
    justify-content: center;
    background: rgba(0, 0, 0, 0.5);
  }
  .call-ring {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 8px;
    padding: 32px 40px;
    border-radius: var(--radius-lg, 14px);
    background: var(--bg-panel);
    border: 1px solid var(--border-hair-strong);
    box-shadow: 0 12px 40px rgba(0, 0, 0, 0.4);
  }
  .avatar.lg {
    width: 88px;
    height: 88px;
    font-size: 32px;
    border-radius: 50%;
  }
  .avatar.lg.ringing {
    animation: pulse 1.4s ease-in-out infinite;
  }
  @keyframes pulse {
    0%, 100% { box-shadow: 0 0 0 0 rgba(88, 101, 242, 0.5); }
    50% { box-shadow: 0 0 0 14px rgba(88, 101, 242, 0); }
  }
  .call-name {
    font-size: 20px;
    font-weight: 700;
  }
  .call-sub {
    color: var(--text-muted);
    font-size: 13px;
  }
  .call-actions {
    display: flex;
    gap: 28px;
    margin-top: 12px;
  }
  .call-btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 56px;
    height: 56px;
    border-radius: 50%;
    border: none;
    cursor: pointer;
    color: #fff;
  }
  .call-btn.small {
    width: 36px;
    height: 36px;
    flex: none;
  }
  .call-btn.accept {
    background: #3ba55d;
  }
  .call-btn.accept:hover {
    background: #34924f;
  }
  .call-btn.decline {
    background: #ed4245;
  }
  .call-btn.decline:hover {
    background: #d83c3e;
  }
  .call-btn.mute {
    background: var(--bg-panel-raised, rgba(255, 255, 255, 0.08));
    color: var(--text-primary);
  }
  .call-btn.mute:hover {
    background: var(--bg-hover, rgba(255, 255, 255, 0.14));
  }
  .call-btn.mute.active {
    background: #ed4245;
    color: #fff;
  }
  .call-bar {
    position: fixed;
    bottom: 16px;
    left: 50%;
    transform: translateX(-50%);
    z-index: 150;
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 8px 12px 8px 8px;
    border-radius: 999px;
    background: var(--bg-panel);
    border: 1px solid var(--border-hair-strong);
    box-shadow: 0 6px 20px rgba(0, 0, 0, 0.3);
  }
  .call-bar-text {
    display: flex;
    flex-direction: column;
    line-height: 1.15;
  }
  .call-bar-state {
    font-size: 11px;
    color: var(--text-muted);
  }
</style>
