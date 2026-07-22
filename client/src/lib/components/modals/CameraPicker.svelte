<script lang="ts">
  // Camera picker: choose which video input to use, with a live preview, then
  // start the camera on the LiveKit path. Also acts as "switch camera" while the
  // camera is already on.
  import { fade } from "svelte/transition";
  import { voiceUI } from "$lib/voiceui.svelte";
  import {
    voice,
    IS_DESKTOP,
    startCamera,
    stopCamera,
    startNativeVoiceCamera,
    stopNativeVoiceCamera,
    listNativeCameras,
  } from "$lib/voice.svelte";

  type Cam = { deviceId: string; label: string };
  let devices = $state<Cam[]>([]);
  let selected = $state<string>("");
  let error = $state<string>("");
  let previewEl = $state<HTMLVideoElement | null>(null);
  let previewStream: MediaStream | null = null;

  async function loadDevices() {
    if (IS_DESKTOP) {
      // Native cameras (nokhwa) — capture happens in Rust, so no webview preview.
      const cams = await listNativeCameras();
      devices = cams.map((c) => ({ deviceId: c.id, label: c.name }));
      if (!devices.length) error = "No cameras found.";
      else selected = devices[0].deviceId;
      return;
    }
    try {
      // A short-lived capture grants permission so device labels are populated.
      const probe = await navigator.mediaDevices.getUserMedia({ video: true });
      probe.getTracks().forEach((t) => t.stop());
    } catch {
      error = "Camera permission is needed to choose a device.";
      return;
    }
    const list = await navigator.mediaDevices.enumerateDevices();
    devices = list
      .filter((d) => d.kind === "videoinput")
      .map((d) => ({ deviceId: d.deviceId, label: d.label || "Camera" }));
    if (devices.length) selected = devices[0].deviceId;
  }

  async function preview(deviceId: string) {
    stopPreview();
    try {
      previewStream = await navigator.mediaDevices.getUserMedia({
        video: deviceId ? { deviceId: { exact: deviceId } } : true,
      });
      if (previewEl) previewEl.srcObject = previewStream;
    } catch {
      /* preview is best-effort */
    }
  }
  function stopPreview() {
    previewStream?.getTracks().forEach((t) => t.stop());
    previewStream = null;
  }

  function choose(id: string) {
    // Setting `selected` drives the preview effect below — no direct call, or the
    // device would be opened twice.
    selected = id;
  }

  function start() {
    stopPreview();
    if (IS_DESKTOP) void startNativeVoiceCamera(selected || undefined);
    else void startCamera(selected || undefined);
    voiceUI.cameraPicker = false;
  }
  function cancel() {
    stopPreview();
    voiceUI.cameraPicker = false;
  }
  function turnOff() {
    stopPreview();
    if (IS_DESKTOP) void stopNativeVoiceCamera();
    else void stopCamera();
    voiceUI.cameraPicker = false;
  }

  // Load on mount; the effect re-previews when the selection changes (web only —
  // desktop capture is native, no in-webview preview).
  $effect(() => {
    void loadDevices();
    return stopPreview;
  });
  $effect(() => {
    if (!IS_DESKTOP && selected && previewEl) preview(selected);
  });
</script>

<div class="modal-wrap" transition:fade|global={{ duration: 160 }}>
  <button class="modal-backdrop" aria-label="Cancel" onclick={cancel}></button>
  <div class="modal camera-picker" role="dialog" aria-modal="true">
    <div class="modal-head">
      <h2>Choose a camera</h2>
      <button class="linkish" aria-label="Cancel" onclick={cancel}>✕</button>
    </div>

    {#if !IS_DESKTOP}
      <div class="cam-preview">
        <!-- svelte-ignore a11y_media_has_caption -->
        <video bind:this={previewEl} autoplay playsinline muted></video>
      </div>
    {/if}

    {#if error}
      <p class="picker-error">{error}</p>
    {:else if !devices.length}
      <p class="modal-sub">No cameras found.</p>
    {:else}
      <ul class="device-list">
        {#each devices as d (d.deviceId)}
          <li>
            <button class="device-opt" class:sel={selected === d.deviceId} onclick={() => choose(d.deviceId)}>
              <span class="device-dot"></span>
              <span class="device-name">{d.label || "Camera"}</span>
            </button>
          </li>
        {/each}
      </ul>
    {/if}

    <div class="modal-actions">
      {#if voice.cameraOn}
        <button class="cam-off" onclick={turnOff}>Turn off camera</button>
      {/if}
      <button class="linkish" onclick={cancel}>Cancel</button>
      <button class="picker-go" disabled={!devices.length} onclick={start}>
        {voice.cameraOn ? "Switch camera" : "Start camera"}
      </button>
    </div>
  </div>
</div>

<style>
  .camera-picker {
    max-width: 460px;
  }
  .cam-preview {
    aspect-ratio: 16 / 9;
    background: #000;
    border-radius: 8px;
    overflow: hidden;
    margin-bottom: 10px;
  }
  .cam-preview video {
    width: 100%;
    height: 100%;
    object-fit: cover;
    transform: scaleX(-1);
  }
  .device-list {
    list-style: none;
    margin: 0 0 8px;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
    max-height: 180px;
    overflow-y: auto;
  }
  .device-opt {
    width: 100%;
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 8px 10px;
    border: 1px solid var(--border-hair-strong);
    border-radius: 8px;
    background: var(--bg-panel-raised);
    color: var(--text-primary);
    cursor: pointer;
    text-align: left;
  }
  .device-opt:hover {
    background: var(--bg-hover);
  }
  .device-opt.sel {
    border-color: var(--accent, #5865f2);
  }
  .device-dot {
    width: 10px;
    height: 10px;
    flex: none;
    border-radius: 50%;
    border: 2px solid var(--border-hair-strong);
  }
  .device-opt.sel .device-dot {
    border-color: var(--accent, #5865f2);
    background: var(--accent, #5865f2);
  }
  .device-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .picker-error {
    color: #e0645c;
    font-size: 0.85rem;
  }
  .picker-go {
    background: #3ba55d;
    border: none;
    border-radius: 8px;
    color: #fff;
    font-weight: 600;
    padding: 8px 16px;
    cursor: pointer;
  }
  .picker-go:disabled {
    opacity: 0.6;
    cursor: default;
  }
  .cam-off {
    margin-right: auto;
    background: #b3413b;
    border: none;
    border-radius: 8px;
    color: #fff;
    font-weight: 600;
    padding: 8px 16px;
    cursor: pointer;
  }
  .cam-off:hover {
    background: #c94a43;
  }
</style>
