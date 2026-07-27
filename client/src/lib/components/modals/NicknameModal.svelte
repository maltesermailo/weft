<script lang="ts">
  // §10.3 quick per-namespace nickname editor, opened from a user's context
  // menu. Own nick needs `nick`; another member's needs `manage-nicks` — the
  // server enforces, a missing cap just ERRs.
  import { untrack } from "svelte";
  import { fade } from "svelte/transition";
  import { getApp } from "$lib/context";
  const app = getApp();
  let { target, onclose }: { target: string; onclose: () => void } = $props();

  const isSelf = $derived(target === app.account);
  const scope = $derived(`ns:${app.activeServer}`);
  // Prefill once with the current nickname (the modal is opened fresh per target).
  let value = $state(untrack(() => app.nickOf(target)));

  function focusInput(node: HTMLInputElement) {
    node.focus();
    node.select();
  }
  function save() {
    app.setNick(scope, target, value.trim());
    onclose();
  }
</script>

<div class="modal-wrap" transition:fade|global={{ duration: 190 }}>
  <button class="modal-backdrop" aria-label="Close" onclick={onclose}></button>
  <div class="modal" role="dialog" aria-modal="true" style="width: min(400px, 100%)">
    <div class="modal-head">
      <h2>{isSelf ? "Set your nickname" : `Set nickname for ${app.displayName(target)}`}</h2>
      <button class="linkish" aria-label="Close" onclick={onclose}>✕</button>
    </div>
    <p class="modal-sub">
      Your display name on <b>{app.activeServer}</b>{isSelf ? "" : ` for ${target}`} — leave empty to clear it.
    </p>
    <label class="fld">Nickname
      <input
        use:focusInput
        bind:value
        maxlength="128"
        placeholder={app.displayName(target)}
        onkeydown={(e) => e.key === "Enter" && save()}
      />
    </label>
    <div class="modal-actions">
      <button class="linkish" onclick={onclose}>Cancel</button>
      <button class="ok-btn" onclick={save}>Save</button>
    </div>
  </div>
</div>
