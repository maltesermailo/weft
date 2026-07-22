<script lang="ts">
  // Discord-style screen-share popover, shown while a share is live: stop it,
  // switch source, or change quality. Quality changes re-apply immediately by
  // restarting capture on the same source (the Rust side stops the previous
  // share first, so this is a switch, not a second publish).
  import { fade } from "svelte/transition";
  import { voiceUI } from "$lib/voiceui.svelte";
  import { voice, startNativeVoiceScreenshare, stopNativeVoiceScreenshare } from "$lib/voice.svelte";

  const QKEY = "weft:screenshare-quality";
  const RES = [
    { w: 1280, label: "720p" },
    { w: 1920, label: "1080p" },
    { w: 2560, label: "1440p" },
    { w: 3840, label: "Source" },
  ];
  const FPS = [15, 30, 60];

  const close = () => (voiceUI.screenMenu = null);

  function stop() {
    void stopNativeVoiceScreenshare();
    close();
  }
  function changeSource() {
    close();
    voiceUI.screenPicker = true;
  }
  // Re-publish the *same* source at the new quality, and remember the choice so
  // the picker opens with it next time.
  function setQuality(fps: number, maxWidth: number) {
    if (fps === voice.screenFps && maxWidth === voice.screenMaxWidth) return;
    try {
      localStorage.setItem(QKEY, JSON.stringify({ fps, maxWidth }));
    } catch {
      /* non-fatal */
    }
    if (voice.screenSource) void startNativeVoiceScreenshare(voice.screenSource, { fps, maxWidth });
    else {
      voice.screenFps = fps;
      voice.screenMaxWidth = maxWidth;
    }
  }

  // The label of whatever we're sharing — the id is `screen:<n>` / `window:<n>`.
  const kind = $derived(voice.screenSource?.startsWith("window:") ? "window" : "screen");
</script>

<svelte:window on:keydown={(e) => e.key === "Escape" && close()} />

<!-- Click-away layer: any click outside the menu dismisses it. -->
<button class="ssm-scrim" aria-label="Close menu" onclick={close}></button>

<div
  class="ssm"
  role="dialog"
  aria-label="Screen share options"
  transition:fade|global={{ duration: 110 }}
  style="left:{voiceUI.screenMenu?.left ?? 0}px; bottom:{voiceUI.screenMenu?.bottom ?? 0}px"
>
  <div class="ssm-head">
    <span class="ssm-live" aria-hidden="true"></span>
    Sharing your {kind}
  </div>

  <button class="ssm-item" onclick={changeSource}>
    <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true">
      <rect x="2" y="3" width="20" height="14" rx="2" /><line x1="8" y1="21" x2="16" y2="21" /><line x1="12" y1="17" x2="12" y2="21" />
    </svg>
    Change source
  </button>

  <div class="ssm-sep"></div>

  <div class="ssm-label">Resolution</div>
  <div class="ssm-chips">
    {#each RES as r (r.w)}
      <button
        class="ssm-chip"
        class:on={voice.screenMaxWidth === r.w}
        aria-pressed={voice.screenMaxWidth === r.w}
        onclick={() => setQuality(voice.screenFps, r.w)}
      >
        {r.label}
      </button>
    {/each}
  </div>

  <div class="ssm-label">Frame rate</div>
  <div class="ssm-chips">
    {#each FPS as f (f)}
      <button
        class="ssm-chip"
        class:on={voice.screenFps === f}
        aria-pressed={voice.screenFps === f}
        onclick={() => setQuality(f, voice.screenMaxWidth)}
      >
        {f} fps
      </button>
    {/each}
  </div>

  <div class="ssm-sep"></div>

  <button class="ssm-item danger" onclick={stop}>
    <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true">
      <rect x="2" y="3" width="20" height="14" rx="2" /><line x1="3" y1="3" x2="21" y2="21" />
    </svg>
    Stop sharing
  </button>
</div>

<style>
  .ssm-scrim {
    position: fixed;
    inset: 0;
    z-index: 70;
    border: none;
    padding: 0;
    background: none;
    cursor: default;
  }
  .ssm {
    position: fixed;
    z-index: 71;
    transform: translateX(-50%); /* `left` is the anchor button's centre */
    width: 240px;
    padding: 8px;
    border-radius: 10px;
    background: var(--bg-panel-raised);
    border: 1px solid var(--border-hair-strong);
    box-shadow: 0 12px 32px rgba(0, 0, 0, 0.45);
  }
  .ssm-head {
    display: flex;
    align-items: center;
    gap: 7px;
    padding: 5px 8px 8px;
    font-size: 12px;
    font-weight: 600;
    color: var(--text-secondary);
  }
  .ssm-live {
    width: 7px;
    height: 7px;
    flex: none;
    border-radius: 50%;
    background: #3ba55d;
  }
  .ssm-item {
    display: flex;
    align-items: center;
    gap: 9px;
    width: 100%;
    padding: 7px 8px;
    border: none;
    border-radius: 6px;
    background: none;
    color: var(--text-secondary);
    font: inherit;
    font-size: 13px;
    text-align: left;
    cursor: pointer;
  }
  .ssm-item:hover {
    background: var(--bg-hover);
    color: var(--text-primary);
  }
  .ssm-item.danger {
    color: #e0645c;
  }
  .ssm-item.danger:hover {
    background: #b3413b;
    color: #fff;
  }
  .ssm-sep {
    height: 1px;
    margin: 6px 4px;
    background: var(--border-hair);
  }
  .ssm-label {
    padding: 2px 8px 5px;
    font-size: 10.5px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--text-faint);
  }
  .ssm-chips {
    display: flex;
    flex-wrap: wrap;
    gap: 4px;
    padding: 0 4px 8px;
  }
  .ssm-chip {
    flex: 1;
    min-width: 48px;
    padding: 5px 6px;
    border: 1px solid var(--border-hair-strong);
    border-radius: 6px;
    background: var(--bg-panel);
    color: var(--text-muted);
    font: inherit;
    font-size: 11.5px;
    cursor: pointer;
  }
  .ssm-chip:hover {
    color: var(--text-primary);
    background: var(--bg-hover);
  }
  .ssm-chip.on {
    border-color: var(--accent, #5865f2);
    color: #fff;
    background: var(--accent, #5865f2);
  }
</style>
