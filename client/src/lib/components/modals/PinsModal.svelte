<script lang="ts">
  import { renderMd } from "$lib/rendering/mdrender.svelte";
  import { displayName } from "$lib/profile/profile.svelte";
  import { fade } from "svelte/transition";
  import { getApp } from "$lib/ui/context";
  import * as weft from "$lib/transport/weft";
  import { store } from "$lib/store/store.svelte";
  import Avatar from "$lib/components/Avatar.svelte";
  const app = getApp();
  let { onclose }: { onclose: () => void } = $props();
  const pins = store.pins;
</script>

<div class="modal-wrap" transition:fade|global={{ duration: 190 }}>
  <button class="modal-backdrop" aria-label="Close" onclick={onclose}></button>
  <div class="modal" role="dialog" aria-modal="true">
    <div class="modal-head">
      <h2>Pinned — {app.chanShort(app.active)}</h2>
      <button class="linkish" aria-label="Close" onclick={onclose}>✕</button>
    </div>
    <div class="modal-list">
      {#each pins.list as m (m.key)}
        <div class="pin-card">
          <div class="avatar sm"><Avatar account={m.author} /></div>
          <div class="pin-body">
            <div class="pin-meta"><b>{displayName(m.author)}</b> <span class="time">{m.time}</span></div>
            <div class="msg-line">{#if m.md}{@html renderMd(m.body)}{:else}{m.body}{/if}</div>
          </div>
          <button class="linkish" title="Unpin" onclick={() => m.msgid && weft.pin(m.msgid, false).catch(() => {})}>unpin</button>
        </div>
      {:else}
        <div class="empty-hint">No pinned messages.</div>
      {/each}
    </div>
  </div>
</div>
