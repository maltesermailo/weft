<script lang="ts">
  // §16 voice controls for one channel: join/leave, local mute, and the live
  // voice-room roster (speaking ring + mute badges). Self-contained — it drives
  // the WebRTC controller in `voice.svelte.ts` directly, not through AppCtx.
  import { voice, joinVoice, leaveVoice, toggleMute, toggleDeafen } from "$lib/voice.svelte";
  import Avatar from "$lib/components/Avatar.svelte";

  let { channel }: { channel: string } = $props();

  // Are we in *this* channel's voice room?
  const here = $derived(voice.channel === channel);
  const roster = $derived(Object.values(voice.participants));
</script>

<div class="voice-bar">
  {#if here}
    <div class="voice-head">
      <span class="voice-live" aria-hidden="true"></span>
      <span class="voice-title">Voice connected</span>
      <div class="voice-actions">
        <button
          class="voice-btn"
          class:active={voice.muted}
          title={voice.muted ? "Unmute microphone" : "Mute microphone"}
          aria-label={voice.muted ? "Unmute microphone" : "Mute microphone"}
          onclick={toggleMute}
        >
          {voice.muted ? "🔇" : "🎙️"}
        </button>
        <button
          class="voice-btn deafen"
          class:active={voice.deafened}
          title={voice.deafened ? "Undeafen" : "Deafen (hear nothing)"}
          aria-label={voice.deafened ? "Undeafen" : "Deafen"}
          onclick={toggleDeafen}
        >
          🎧
        </button>
        <button class="voice-btn leave" title="Leave voice" aria-label="Leave voice" onclick={leaveVoice}>
          Leave
        </button>
      </div>
    </div>
    <ul class="voice-roster">
      {#each roster as p (p.user)}
        <li class="voice-member" class:speaking={p.speaking}>
          <span class="voice-avatar"><Avatar account={p.user} /></span>
          <span class="voice-name">{p.user}{p.self ? " (you)" : ""}</span>
          {#if p.muted}<span class="voice-flag" title="Muted" aria-hidden="true">🔇</span>{/if}
          {#if p.deaf}<span class="voice-flag deaf" title="Deafened" aria-hidden="true">🎧</span>{/if}
        </li>
      {/each}
    </ul>
  {:else}
    <button
      class="voice-join"
      disabled={voice.connecting}
      onclick={() => joinVoice(channel)}
    >
      {voice.connecting && voice.channel === channel ? "Connecting…" : "🔊 Join Voice"}
    </button>
  {/if}
  {#if voice.error}
    <div class="voice-error">{voice.error}</div>
  {/if}
</div>

<style>
  .voice-bar {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 8px;
    border-radius: 8px;
    background: var(--bg-2, rgba(255, 255, 255, 0.03));
  }
  .voice-head {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .voice-live {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: #43b581;
    box-shadow: 0 0 0 0 rgba(67, 181, 129, 0.7);
    animation: voice-pulse 2s infinite;
  }
  @keyframes voice-pulse {
    0% { box-shadow: 0 0 0 0 rgba(67, 181, 129, 0.5); }
    70% { box-shadow: 0 0 0 6px rgba(67, 181, 129, 0); }
    100% { box-shadow: 0 0 0 0 rgba(67, 181, 129, 0); }
  }
  .voice-title {
    font-size: 0.8rem;
    font-weight: 600;
    color: #43b581;
  }
  .voice-actions {
    margin-left: auto;
    display: flex;
    gap: 4px;
  }
  .voice-btn,
  .voice-join {
    cursor: pointer;
    border: none;
    border-radius: 6px;
    padding: 5px 10px;
    font-size: 0.8rem;
    color: var(--text, inherit);
    background: var(--bg-3, rgba(255, 255, 255, 0.06));
  }
  .voice-btn:hover,
  .voice-join:hover {
    background: var(--bg-4, rgba(255, 255, 255, 0.1));
  }
  .voice-btn.active {
    background: #b3413b;
    color: #fff;
  }
  .voice-btn.leave {
    background: #b3413b;
    color: #fff;
  }
  .voice-join {
    width: 100%;
    font-weight: 600;
  }
  .voice-join:disabled {
    opacity: 0.6;
    cursor: default;
  }
  .voice-roster {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .voice-member {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 3px 4px;
    border-radius: 6px;
    font-size: 0.82rem;
  }
  .voice-avatar {
    width: 24px;
    height: 24px;
    border-radius: 50%;
    display: grid;
    place-items: center;
    font-size: 0.62rem;
    font-weight: 700;
    background: var(--bg-4, rgba(255, 255, 255, 0.1));
    outline: 2px solid transparent;
    transition: outline-color 0.1s;
  }
  .voice-member.speaking .voice-avatar {
    outline-color: #43b581;
  }
  .voice-name {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .voice-flag {
    font-size: 0.7rem;
    opacity: 0.7;
  }
  /* Deafened reads as a headphone with a red slash, distinct from the mute icon. */
  .voice-flag.deaf {
    position: relative;
  }
  .voice-flag.deaf::after {
    content: "";
    position: absolute;
    left: -1px;
    right: -1px;
    top: 50%;
    height: 2px;
    background: #e0645c;
    transform: rotate(-20deg);
  }
  .voice-error {
    font-size: 0.72rem;
    color: #e0645c;
  }
</style>
