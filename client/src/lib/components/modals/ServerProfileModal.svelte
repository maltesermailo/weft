<script lang="ts">
  import { store } from "$lib/store/store.svelte";
  import { vm } from "$lib/navigation/viewmodel.svelte";
  import { displayName, nickOf, setNick } from "$lib/profile/profile.svelte";
  import { untrack } from "svelte";
  import { fade } from "svelte/transition";
  import { getApp } from "$lib/ui/context";
  const app = getApp();
  let { onclose }: { onclose: () => void } = $props();

  const scope = `ns:${app.activeServer}`;
  const serverName = $derived(vm.activeNsMeta?.title || vm.activeNsMeta?.name || app.activeServer);
  const current = $derived(nickOf(store.session.account));
  let draft = $state(untrack(() => nickOf(store.session.account)));
  const dirty = $derived(draft.trim() !== current);
  // The global display name we fall back to when there's no nickname.
  const globalName = $derived(displayName(store.session.account));

  function save() {
    setNick(scope, store.session.account, draft.trim());
    onclose();
  }
  function clearNick() {
    setNick(scope, store.session.account, "");
    onclose();
  }
</script>

<div class="modal-wrap" transition:fade|global={{ duration: 190 }}>
  <button class="modal-backdrop" aria-label="Close" onclick={onclose}></button>
  <div class="modal" role="dialog" aria-modal="true">
    <div class="modal-head">
      <h2>Edit Server Profile</h2>
      <button class="linkish" aria-label="Close" onclick={onclose}>✕</button>
    </div>
    <p class="modal-sub">
      Your display name in <strong>{serverName}</strong> — shown only here. Leave it blank to use your
      global name.
    </p>

    <div class="field-label">Server nickname</div>
    <input
      class="sp-input"
      bind:value={draft}
      maxlength="128"
      placeholder={globalName}
      onkeydown={(e) => e.key === "Enter" && dirty && save()}
    />

    <div class="modal-actions">
      {#if current}
        <button class="linkish sp-clear" onclick={clearNick}>Reset to global name</button>
      {/if}
      <button class="ok-btn" disabled={!dirty} onclick={save}>Save</button>
    </div>
  </div>
</div>

<style>
  .sp-input {
    width: 100%;
    padding: 10px 14px;
    border-radius: 10px;
    border: 2px solid transparent;
    background: var(--bg-void, rgba(0, 0, 0, 0.18));
    color: var(--text-primary, inherit);
    font: inherit;
    font-size: 14px;
    outline: none;
    transition: border-color 0.15s;
  }
  .sp-input:focus {
    border-color: var(--accent, #5865f2);
  }
  .modal-actions {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-top: 16px;
  }
  .sp-clear {
    margin-right: auto;
  }
</style>
