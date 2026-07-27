<script lang="ts">
  // The shared "unsaved changes" bar (the profile editor's pattern): a floating
  // Revert / Save Changes dock shown while a draft differs from what's stored.
  // Fixed to the viewport bottom so it works from any editor without needing a
  // positioned ancestor.
  import { fly } from "svelte/transition";
  let {
    message = "You have unsaved changes",
    saveLabel = "Save Changes",
    saveDisabled = false,
    onrevert,
    onsave,
  }: {
    message?: string;
    saveLabel?: string;
    saveDisabled?: boolean;
    onrevert: () => void;
    onsave: () => void;
  } = $props();
</script>

<div class="savebar" transition:fly|global={{ y: 70, duration: 220 }}>
  <div class="savebar-inner">
    <span class="savebar-msg"><span class="savebar-dot"></span>{message}</span>
    <div class="savebar-actions">
      <button class="savebar-revert" onclick={onrevert}>
        <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8" /><path d="M3 3v5h5" /></svg>
        Revert
      </button>
      <button class="savebar-save" disabled={saveDisabled} onclick={onsave}>
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="3" stroke-linecap="round" stroke-linejoin="round"><polyline points="20 6 9 17 4 12" /></svg>
        {saveLabel}
      </button>
    </div>
  </div>
</div>

<style>
  .savebar {
    position: fixed;
    left: 0;
    right: 0;
    bottom: 0;
    z-index: 200;
    padding: 0 24px 20px;
    pointer-events: none;
  }
  .savebar-inner {
    pointer-events: auto;
    max-width: 640px;
    margin: 0 auto;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 16px;
    padding: 12px 16px 12px 18px;
    background: var(--bg-elevated, #111214);
    border: 1px solid var(--border-hair-strong);
    border-radius: 14px;
    box-shadow: 0 18px 50px rgba(0, 0, 0, 0.5);
  }
  .savebar-msg {
    display: flex;
    align-items: center;
    gap: 10px;
    font-size: 14px;
    font-weight: 500;
    color: var(--text-secondary);
    min-width: 0;
  }
  .savebar-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: #faa61a;
    flex-shrink: 0;
  }
  .savebar-actions {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-shrink: 0;
  }
  .savebar-revert,
  .savebar-save {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    border: none;
    border-radius: 10px;
    font: inherit;
    font-size: 13px;
    cursor: pointer;
  }
  .savebar-revert {
    padding: 8px 14px;
    background: transparent;
    color: var(--text-muted);
    font-weight: 500;
  }
  .savebar-revert:hover {
    background: var(--bg-hover);
    color: var(--text-primary);
  }
  .savebar-save {
    padding: 8px 18px;
    background: #3ba55d;
    color: #fff;
    font-weight: 600;
  }
  .savebar-save:hover:not(:disabled) {
    background: #2f8a4c;
  }
  .savebar-save:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
</style>
