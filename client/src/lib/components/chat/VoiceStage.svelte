<script lang="ts">
  // Discord-style voice stage: shown in the main pane when the active channel is
  // a voice channel. Renders a tile per participant (camera video or avatar) plus
  // a large tile per active screen share, and a control bar.
  import { getApp } from "$lib/context";
  import {
    voice,
    voiceRosters,
    joinVoice,
    leaveVoice,
    toggleMute,
    toggleDeafen,
    stopCamera,
    startScreenShare,
    stopScreenShare,
    attachVideo,
    detachVideo,
    IS_DESKTOP,
    type VoiceParticipant,
  } from "$lib/voice.svelte";
  import { voiceUI } from "$lib/voiceui.svelte";
  import Avatar from "$lib/components/Avatar.svelte";

  const app = getApp();
  // Camera opens the in-app device picker. Screen share opens the Discord-style
  // native picker on desktop, or the OS getDisplayMedia picker on the web.
  const camClick = () => (voice.cameraOn ? stopCamera() : (voiceUI.cameraPicker = true));
  const screenClick = () =>
    voice.sharingScreen
      ? stopScreenShare()
      : IS_DESKTOP
        ? (voiceUI.screenPicker = true)
        : startScreenShare();

  const channel = $derived(app.active);
  const joined = $derived(voice.channel === channel);

  // Roster to show: the live LiveKit roster when we're in the room, otherwise the
  // presence preview (server `voice-state`) for a channel we're only peeking at.
  const tiles = $derived.by<VoiceParticipant[]>(() =>
    joined ? Object.values(voice.participants) : Object.values(voiceRosters[channel] ?? {}),
  );
  const shares = $derived(tiles.filter((p) => p.sharingScreen));

  type Bind = { user: string; source: "camera" | "screen"; tick: number };

  // Svelte action: (re)attach a participant's LiveKit video track to this
  // <video>. `tick` (voice.mediaTick) is part of the param so a track that
  // arrives after the element mounts triggers `update` → a fresh attach.
  function bindVideo(el: HTMLVideoElement, arg: Bind) {
    let cur = arg;
    attachVideo(el, cur.user, cur.source);
    return {
      update(next: Bind) {
        if (next.user !== cur.user || next.source !== cur.source || next.tick !== cur.tick) {
          detachVideo(el, cur.user, cur.source);
          cur = next;
          attachVideo(el, cur.user, cur.source);
        }
      },
      destroy() {
        detachVideo(el, cur.user, cur.source);
      },
    };
  }
</script>

<div class="stage">
  <header class="stage-head">
    <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M11 5 6 9H2v6h4l5 4V5z" /><path d="M15.5 8.5a5 5 0 0 1 0 7M19 5a9 9 0 0 1 0 14" /></svg>
    <span class="stage-title">{channel}</span>
    <span class="stage-count">{tiles.length} in voice</span>
  </header>

  <div class="stage-body">
    {#if shares.length}
      <div class="stage-shares" class:multi={shares.length > 1}>
        {#each shares as p (p.user)}
          <div class="share-tile">
            <!-- svelte-ignore a11y_media_has_caption -->
            <video autoplay playsinline use:bindVideo={{ user: p.user, source: "screen", tick: voice.mediaTick }}></video>
            <span class="tile-label">{p.user}{p.self ? " (you)" : ""} · screen</span>
          </div>
        {/each}
      </div>
    {/if}

    <div class="stage-grid" class:with-shares={shares.length > 0}>
      {#each tiles as p (p.user)}
        <div class="cam-tile" class:speaking={p.speaking}>
          {#if p.cameraOn}
            <!-- svelte-ignore a11y_media_has_caption -->
            <video
              class:mirror={p.self}
              autoplay
              playsinline
              muted={p.self}
              use:bindVideo={{ user: p.user, source: "camera", tick: voice.mediaTick }}
            ></video>
          {:else}
            <div class="tile-avatar"><Avatar account={p.user} /></div>
          {/if}
          <span class="tile-label">
            {p.user}{p.self ? " (you)" : ""}
            {#if p.muted}<span class="tile-flag" title="Muted">🔇</span>{/if}
          </span>
        </div>
      {/each}

      {#if !tiles.length}
        <div class="stage-empty">No one's in this voice channel yet.</div>
      {/if}
    </div>
  </div>

  <div class="stage-controls">
    {#if joined}
      <button class="sc-btn" class:on={voice.muted} title={voice.muted ? "Unmute" : "Mute"} aria-label="Mute" onclick={toggleMute}>
        {voice.muted ? "🔇" : "🎙️"}<span>{voice.muted ? "Unmute" : "Mute"}</span>
      </button>
      <button class="sc-btn" class:on={voice.deafened} title="Deafen" aria-label="Deafen" onclick={toggleDeafen}>
        🎧<span>{voice.deafened ? "Undeafen" : "Deafen"}</span>
      </button>
      <button class="sc-btn" class:go={voice.cameraOn} title="Camera" aria-label="Camera" onclick={camClick}>
        📹<span>{voice.cameraOn ? "Stop Video" : "Video"}</span>
      </button>
      <button class="sc-btn" class:go={voice.sharingScreen} title="Share screen" aria-label="Share screen" onclick={screenClick}>
        🖥️<span>{voice.sharingScreen ? "Stop Share" : "Share"}</span>
      </button>
      <button class="sc-btn leave" title="Disconnect" aria-label="Disconnect" onclick={leaveVoice}>
        📴<span>Leave</span>
      </button>
    {:else}
      <button class="sc-join" disabled={voice.connecting} onclick={() => joinVoice(channel)}>
        {voice.connecting && voice.channel === channel ? "Connecting…" : "🔊 Join Voice"}
      </button>
    {/if}
  </div>

  {#if voice.error}<div class="stage-error">{voice.error}</div>{/if}
</div>

<style>
  .stage {
    display: flex;
    flex-direction: column;
    height: 100%;
    min-height: 0;
    background: var(--bg-void);
  }
  .stage-head {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 12px 16px;
    border-bottom: 1px solid var(--border-hair);
    color: var(--text-primary);
  }
  .stage-title {
    font-weight: 700;
  }
  .stage-count {
    margin-left: auto;
    font-size: 0.8rem;
    color: var(--text-muted);
  }
  .stage-body {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 14px;
    padding: 16px;
  }
  .stage-shares {
    display: grid;
    grid-template-columns: 1fr;
    gap: 12px;
  }
  .stage-shares.multi {
    grid-template-columns: repeat(auto-fit, minmax(320px, 1fr));
  }
  .share-tile {
    position: relative;
    aspect-ratio: 16 / 9;
    background: #000;
    border-radius: 10px;
    overflow: hidden;
    border: 1px solid var(--border-hair-strong);
  }
  .share-tile video {
    width: 100%;
    height: 100%;
    object-fit: contain;
    background: #000;
  }
  .stage-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
    gap: 12px;
    align-content: start;
  }
  .stage-grid.with-shares {
    grid-template-columns: repeat(auto-fit, minmax(140px, 1fr));
  }
  .cam-tile {
    position: relative;
    aspect-ratio: 16 / 10;
    background: var(--bg-panel);
    border-radius: 10px;
    overflow: hidden;
    display: grid;
    place-items: center;
    border: 2px solid transparent;
  }
  .cam-tile.speaking {
    border-color: #43b581;
  }
  .cam-tile video {
    width: 100%;
    height: 100%;
    object-fit: cover;
    background: #000;
  }
  .cam-tile video.mirror {
    transform: scaleX(-1);
  }
  .tile-avatar {
    width: 72px;
    height: 72px;
    border-radius: 50%;
    overflow: hidden;
    display: grid;
    place-items: center;
  }
  .tile-label {
    position: absolute;
    left: 8px;
    bottom: 8px;
    display: inline-flex;
    align-items: center;
    gap: 5px;
    max-width: calc(100% - 16px);
    padding: 2px 8px;
    border-radius: 6px;
    background: rgba(0, 0, 0, 0.6);
    color: #fff;
    font-size: 0.76rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .tile-flag {
    font-size: 0.72rem;
  }
  .stage-empty {
    grid-column: 1 / -1;
    padding: 40px 0;
    text-align: center;
    color: var(--text-muted);
  }
  .stage-controls {
    display: flex;
    justify-content: center;
    gap: 10px;
    padding: 12px;
    border-top: 1px solid var(--border-hair);
    background: var(--bg-panel);
  }
  .sc-btn {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 3px;
    min-width: 64px;
    padding: 8px 10px;
    border: none;
    border-radius: 10px;
    cursor: pointer;
    font-size: 1.1rem;
    color: var(--text-primary);
    background: var(--bg-hover);
  }
  .sc-btn span {
    font-size: 0.68rem;
    color: var(--text-muted);
  }
  .sc-btn:hover {
    background: var(--border-hair-strong);
  }
  .sc-btn.on {
    background: #b3413b;
    color: #fff;
  }
  .sc-btn.go {
    background: #3ba55d;
    color: #fff;
  }
  .sc-btn.on span,
  .sc-btn.go span {
    color: rgba(255, 255, 255, 0.85);
  }
  .sc-btn.leave:hover {
    background: #b3413b;
    color: #fff;
  }
  .sc-join {
    padding: 12px 28px;
    border: none;
    border-radius: 10px;
    cursor: pointer;
    font-weight: 700;
    color: #fff;
    background: #3ba55d;
  }
  .sc-join:disabled {
    opacity: 0.6;
    cursor: default;
  }
  .stage-error {
    padding: 6px 16px;
    color: #e0645c;
    font-size: 0.78rem;
    text-align: center;
  }
</style>
