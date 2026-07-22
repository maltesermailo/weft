<script lang="ts">
  // Persistent voice status panel — sits just above the user footer while
  // connected (Discord-style), independent of which channel you're viewing. It
  // drives the controller in `voice.svelte.ts` directly. Clicking the status
  // opens the voice channel's stage.
  import {
    voice,
    leaveVoice,
    toggleMute,
    toggleDeafen,
    stopCamera,
    startScreenShare,
    stopScreenShare,
    IS_DESKTOP,
  } from "$lib/voice.svelte";
  import { voiceUI } from "$lib/voiceui.svelte";
  import { getApp } from "$lib/context";

  const app = getApp();
  function openStage() {
    if (voice.channel) app.openVoice(voice.channel);
  }
  // Camera opens the in-app device picker. Screen share opens the Discord-style
  // native picker on desktop, or the OS getDisplayMedia picker on the web.
  const camClick = () => (voice.cameraOn ? stopCamera() : (voiceUI.cameraPicker = true));
  const screenClick = () =>
    voice.sharingScreen
      ? stopScreenShare()
      : IS_DESKTOP
        ? (voiceUI.screenPicker = true)
        : startScreenShare();
</script>

{#if voice.channel}
  <div class="voice-panel">
    <button class="voice-status" onclick={openStage} title="Open voice channel">
      <span class="voice-live" class:connecting={voice.connecting} aria-hidden="true"></span>
      <span class="voice-status-text">
        <span class="voice-state">{voice.connecting ? "Connecting…" : "Voice Connected"}</span>
        <span class="voice-chan">{voice.channel}</span>
      </span>
    </button>

    <div class="voice-controls">
      <button
        class="vp-btn"
        class:on={voice.muted}
        title={voice.muted ? "Unmute microphone" : "Mute microphone"}
        aria-label="Toggle mute"
        onclick={toggleMute}
      >
        {#if voice.muted}
          <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="1.8"><line x1="3" y1="3" x2="21" y2="21" /><path d="M9 9v3a3 3 0 0 0 5 2.1M15 12V6a3 3 0 0 0-6 0" /><path d="M17 12a5 5 0 0 1-1 3M5 11a7 7 0 0 0 4 6" /><line x1="12" y1="19" x2="12" y2="22" /></svg>
        {:else}
          <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="1.8"><rect x="9" y="2" width="6" height="12" rx="3" /><path d="M5 10a7 7 0 0 0 14 0" /><line x1="12" y1="19" x2="12" y2="22" /></svg>
        {/if}
      </button>

      <button
        class="vp-btn"
        class:on={voice.deafened}
        title={voice.deafened ? "Undeafen" : "Deafen (hear nothing)"}
        aria-label="Toggle deafen"
        onclick={toggleDeafen}
      >
        <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M4 14v-3a8 8 0 0 1 16 0v3" /><rect x="2" y="13" width="4" height="7" rx="1.5" /><rect x="18" y="13" width="4" height="7" rx="1.5" />{#if voice.deafened}<line x1="3" y1="3" x2="21" y2="21" />{/if}</svg>
      </button>

      <button
        class="vp-btn"
        class:on={voice.cameraOn}
        title={voice.cameraOn ? "Stop camera" : "Start camera"}
        aria-label="Toggle camera"
        onclick={camClick}
      >
        <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="1.8"><rect x="2" y="6" width="14" height="12" rx="2" /><path d="M22 8l-6 4 6 4V8z" />{#if !voice.cameraOn}<line x1="3" y1="3" x2="21" y2="21" />{/if}</svg>
      </button>

      <button
        class="vp-btn"
        class:on={voice.sharingScreen}
        title={voice.sharingScreen ? "Stop sharing screen" : "Share your screen"}
        aria-label="Toggle screen share"
        onclick={screenClick}
      >
        <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="1.8"><rect x="2" y="3" width="20" height="14" rx="2" /><line x1="8" y1="21" x2="16" y2="21" /><line x1="12" y1="17" x2="12" y2="21" /></svg>
      </button>

      <button class="vp-btn leave" title="Disconnect" aria-label="Disconnect" onclick={leaveVoice}>
        <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M2 9.5C5 7 9 6 12 6s7 1 10 3.5c.6.5.8 1.3.4 2l-1.3 2c-.4.6-1.2.8-1.9.5l-2.6-1a1.5 1.5 0 0 1-.9-1.4v-1.3C13.9 11 10.1 11 8.3 11.8v1.3c0 .6-.4 1.2-.9 1.4l-2.6 1c-.7.3-1.5.1-1.9-.5l-1.3-2c-.4-.7-.2-1.5.4-2z" /></svg>
      </button>
    </div>

    {#if voice.error}<div class="voice-error">{voice.error}</div>{/if}
  </div>
{/if}

<style>
  .voice-panel {
    display: flex;
    flex-direction: column;
    gap: 6px;
    margin: 0 8px 6px;
    padding: 7px 8px;
    border-radius: 8px;
    background: var(--bg-panel-raised);
    border: 1px solid var(--border-hair-strong);
  }
  .voice-status {
    display: flex;
    align-items: center;
    gap: 8px;
    background: none;
    border: none;
    padding: 2px;
    cursor: pointer;
    text-align: left;
    color: inherit;
  }
  .voice-status:hover .voice-chan {
    text-decoration: underline;
  }
  .voice-live {
    width: 9px;
    height: 9px;
    flex: none;
    border-radius: 50%;
    background: #43b581;
    box-shadow: 0 0 0 0 rgba(67, 181, 129, 0.7);
    animation: voice-pulse 2s infinite;
  }
  .voice-live.connecting {
    background: #e0a53c;
  }
  @keyframes voice-pulse {
    0% { box-shadow: 0 0 0 0 rgba(67, 181, 129, 0.5); }
    70% { box-shadow: 0 0 0 6px rgba(67, 181, 129, 0); }
    100% { box-shadow: 0 0 0 0 rgba(67, 181, 129, 0); }
  }
  .voice-status-text {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .voice-state {
    font-size: 0.74rem;
    font-weight: 700;
    color: #43b581;
  }
  .voice-chan {
    font-size: 0.78rem;
    color: var(--text-secondary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .voice-controls {
    display: flex;
    gap: 4px;
  }
  .vp-btn {
    flex: 1;
    display: grid;
    place-items: center;
    padding: 6px 0;
    border: none;
    border-radius: 6px;
    cursor: pointer;
    color: var(--text-secondary);
    background: var(--bg-hover);
  }
  .vp-btn:hover {
    color: var(--text-primary);
    background: var(--border-hair-strong);
  }
  /* "on" = an active toggle. Mute/deafen light red; camera/screen light green. */
  .vp-btn.on {
    background: #3ba55d;
    color: #fff;
  }
  .vp-btn.on[aria-label="Toggle mute"],
  .vp-btn.on[aria-label="Toggle deafen"] {
    background: #b3413b;
  }
  .vp-btn.leave {
    color: #e0645c;
  }
  .vp-btn.leave:hover {
    background: #b3413b;
    color: #fff;
  }
  .voice-error {
    font-size: 0.72rem;
    color: #e0645c;
  }
</style>
