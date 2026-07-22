<script lang="ts">
  // Discord-style screen-share picker (desktop): a grid of live thumbnails for
  // every screen and window, enumerated natively (an embedded webview can't do
  // it from JS). Picking a source starts native capture → LiveKit.
  import { fade } from "svelte/transition";
  import { invoke } from "@tauri-apps/api/core";
  import { voiceUI } from "$lib/voiceui.svelte";
  import { voice, startNativeVoiceScreenshare, stopNativeVoiceScreenshare } from "$lib/voice.svelte";

  type Source = {
    id: string;
    kind: "screen" | "window";
    title: string;
    app: string;
    thumb?: string;
    done?: boolean; // thumbnail capture finished (empty thumb ⇒ no preview available)
  };

  let sources = $state<Source[]>([]);
  let loading = $state(true);
  let error = $state("");
  let tab = $state<"window" | "screen">("window");

  // Capture quality — remembered across sessions.
  const QKEY = "weft:screenshare-quality";
  let fps = $state(15);
  let maxWidth = $state(1280); // 720p=1280 · 1080p=1920 · 1440p=2560 · source=3840
  try {
    const q = JSON.parse(localStorage.getItem(QKEY) ?? "{}");
    if (q.fps) fps = q.fps;
    if (q.maxWidth) maxWidth = q.maxWidth;
  } catch {
    /* defaults */
  }

  const shown = $derived(sources.filter((s) => s.kind === tab));

  async function load() {
    loading = true;
    error = "";
    try {
      // Metadata only — returns instantly so the grid shows right away.
      const list = await invoke<Source[]>("list_capture_sources");
      sources = list.map((s) => ({ ...s, thumb: "" }));
      // Default to whichever tab actually has entries.
      if (!sources.some((s) => s.kind === "window") && sources.some((s) => s.kind === "screen")) {
        tab = "screen";
      }
      loading = false;
      void loadThumbs();
    } catch (e) {
      error = String(e);
      loading = false;
    }
  }

  // Capture each source's thumbnail lazily (bounded concurrency) so a slow or
  // permission-blocked capture only affects its own tile, not the whole list.
  async function loadThumbs() {
    const ids = sources.map((s) => s.id);
    let next = 0;
    const worker = async () => {
      while (next < ids.length) {
        const id = ids[next++];
        let thumb = "";
        try {
          thumb = await invoke<string>("capture_source_thumb", { id });
        } catch {
          /* capture failed — mark done with no preview */
        }
        const idx = sources.findIndex((x) => x.id === id);
        if (idx >= 0) sources[idx] = { ...sources[idx], thumb, done: true };
      }
    };
    await Promise.all([worker(), worker(), worker()]);
  }

  function pick(s: Source) {
    try {
      localStorage.setItem(QKEY, JSON.stringify({ fps, maxWidth }));
    } catch {
      /* non-fatal */
    }
    void startNativeVoiceScreenshare(s.id, { fps, maxWidth });
    voiceUI.screenPicker = false;
  }
  function cancel() {
    voiceUI.screenPicker = false;
  }
  function stopSharing() {
    void stopNativeVoiceScreenshare();
    voiceUI.screenPicker = false;
  }

  $effect(() => {
    void load();
  });
</script>

<div class="modal-wrap" transition:fade|global={{ duration: 160 }}>
  <button class="modal-backdrop" aria-label="Cancel" onclick={cancel}></button>
  <div class="modal screen-picker" role="dialog" aria-modal="true">
    <div class="sp-tabs">
      <button class="sp-tab" class:on={tab === "window"} onclick={() => (tab = "window")}>Applications</button>
      <button class="sp-tab" class:on={tab === "screen"} onclick={() => (tab = "screen")}>Entire Screen</button>
      <button class="linkish sp-close" aria-label="Cancel" onclick={cancel}>✕</button>
    </div>

    <div class="sp-body">
      {#if loading}
        <div class="sp-msg">Loading sources…</div>
      {:else if error}
        <div class="sp-msg sp-err">
          Couldn't list sources. On macOS, grant Weft the <b>Screen Recording</b> permission
          (System Settings → Privacy &amp; Security), then reopen the app.
          <div class="sp-err-detail">{error}</div>
        </div>
      {:else if !shown.length}
        <div class="sp-msg">Nothing to share here.</div>
      {:else}
        <div class="sp-grid">
          {#each shown as s (s.id)}
            <button class="sp-card" onclick={() => pick(s)}>
              <div class="sp-thumb">
                {#if s.thumb}
                  <img src={s.thumb} alt={s.title} />
                {:else if s.done}
                  <div class="sp-thumb-none">No preview</div>
                {:else}
                  <div class="sp-thumb-empty" aria-hidden="true"></div>
                {/if}
                <span class="sp-share">Share</span>
              </div>
              <div class="sp-label" title={s.title}>{s.title}</div>
            </button>
          {/each}
        </div>
      {/if}
    </div>

    <div class="sp-quality">
      <label class="sp-q">
        Resolution
        <select bind:value={maxWidth}>
          <option value={1280}>720p</option>
          <option value={1920}>1080p</option>
          <option value={2560}>1440p</option>
          <option value={3840}>Source</option>
        </select>
      </label>
      <label class="sp-q">
        Frame rate
        <select bind:value={fps}>
          <option value={15}>15 fps</option>
          <option value={30}>30 fps</option>
          <option value={60}>60 fps</option>
        </select>
      </label>
      <span class="sp-q-hint">
        {#if voice.sharingScreen}Pick a source to switch, or stop sharing.{:else}Higher settings need more CPU — lower the resolution if it stutters.{/if}
      </span>
      {#if voice.sharingScreen}
        <button class="sp-stop" onclick={stopSharing}>Stop sharing</button>
      {/if}
    </div>
  </div>
</div>

<style>
  .screen-picker {
    max-width: 860px;
    width: 92vw;
    padding: 0;
    overflow: hidden;
  }
  .sp-tabs {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 10px 12px;
    border-bottom: 1px solid var(--border-hair);
  }
  .sp-tab {
    padding: 7px 14px;
    border: none;
    border-radius: 8px;
    background: transparent;
    color: var(--text-muted);
    font: inherit;
    font-weight: 600;
    cursor: pointer;
  }
  .sp-tab:hover {
    background: var(--bg-hover);
    color: var(--text-primary);
  }
  .sp-tab.on {
    background: var(--bg-hover);
    color: var(--text-primary);
  }
  .sp-close {
    margin-left: auto;
  }
  .sp-body {
    max-height: 66vh;
    overflow-y: auto;
    padding: 14px;
  }
  .sp-msg {
    padding: 40px 16px;
    text-align: center;
    color: var(--text-muted);
  }
  .sp-err {
    color: var(--text-secondary);
  }
  .sp-err-detail {
    margin-top: 8px;
    font-family: var(--font-mono);
    font-size: 11px;
    color: var(--text-faint);
    word-break: break-word;
  }
  .sp-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(220px, 1fr));
    gap: 14px;
  }
  .sp-card {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 0;
    border: none;
    background: none;
    cursor: pointer;
    text-align: left;
  }
  .sp-thumb {
    position: relative;
    aspect-ratio: 16 / 10;
    border-radius: 8px;
    overflow: hidden;
    background: #0c0d11;
    border: 2px solid var(--border-hair-strong);
    display: grid;
    place-items: center;
  }
  .sp-card:hover .sp-thumb {
    border-color: var(--accent, #5865f2);
  }
  .sp-thumb img {
    width: 100%;
    height: 100%;
    object-fit: contain;
  }
  .sp-thumb-empty {
    width: 100%;
    height: 100%;
    background: var(--bg-panel-raised);
    animation: sp-shimmer 1.4s ease-in-out infinite;
  }
  @keyframes sp-shimmer {
    0%,
    100% {
      opacity: 0.4;
    }
    50% {
      opacity: 0.8;
    }
  }
  .sp-thumb-none {
    color: var(--text-faint);
    font-size: 0.78rem;
  }
  .sp-share {
    position: absolute;
    inset: 0;
    display: grid;
    place-items: center;
    background: rgba(0, 0, 0, 0.45);
    color: #fff;
    font-weight: 700;
    opacity: 0;
    transition: opacity 0.12s;
  }
  .sp-card:hover .sp-share {
    opacity: 1;
  }
  .sp-label {
    font-size: 0.82rem;
    color: var(--text-secondary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .sp-quality {
    display: flex;
    align-items: center;
    gap: 16px;
    padding: 10px 14px;
    border-top: 1px solid var(--border-hair);
  }
  .sp-q {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 0.8rem;
    color: var(--text-muted);
  }
  .sp-q select {
    background: var(--bg-panel-raised);
    color: var(--text-primary);
    border: 1px solid var(--border-hair-strong);
    border-radius: 6px;
    padding: 5px 8px;
    font: inherit;
    cursor: pointer;
  }
  .sp-q-hint {
    margin-left: auto;
    font-size: 0.72rem;
    color: var(--text-faint);
  }
  .sp-stop {
    padding: 7px 14px;
    border: none;
    border-radius: 8px;
    background: #b3413b;
    color: #fff;
    font: inherit;
    font-weight: 600;
    cursor: pointer;
  }
  .sp-stop:hover {
    background: #c94a43;
  }
</style>
