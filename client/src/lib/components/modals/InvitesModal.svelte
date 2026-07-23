<script lang="ts">
  import { fade } from "svelte/transition";
  import { getApp } from "$lib/context";
  const app = getApp();
  let { onclose }: { onclose: () => void } = $props();

  function copy(text: string) {
    navigator.clipboard?.writeText(text).then(
      () => app.toast("Invite link copied", "info"),
      () => {},
    );
  }
  function expiryLabel(expiry: number | null): string {
    if (!expiry) return "never";
    const secs = expiry - Math.floor(Date.now() / 1000);
    if (secs <= 0) return "expired";
    if (secs < 3600) return `${Math.ceil(secs / 60)}m`;
    if (secs < 86400) return `${Math.ceil(secs / 3600)}h`;
    return `${Math.ceil(secs / 86400)}d`;
  }
</script>

<div class="modal-wrap" transition:fade|global={{ duration: 190 }}>
  <button class="modal-backdrop" aria-label="Close" onclick={onclose}></button>
  <div class="modal" role="dialog" aria-modal="true">
    <div class="modal-head">
      <h2>Invites — {app.invitesScope}</h2>
      <button class="linkish" aria-label="Close" onclick={onclose}>✕</button>
    </div>
    <div class="modal-list">
      {#each app.invitesList as inv (inv.invite_id)}
        <div class="invite-card">
          <div class="invite-main">
            <button class="invite-code" title="Copy invite link" onclick={() => copy(app.inviteLinkFor(inv))}>
              {inv.invite_id}
              <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><rect x="9" y="9" width="13" height="13" rx="2" /><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" /></svg>
            </button>
            <div class="invite-meta">
              <span>by <b>{app.displayName(inv.creator)}</b></span>
              <span>·</span>
              <span>{inv.uses_left === null ? "∞ uses" : `${inv.uses_left} left`}</span>
              <span>·</span>
              <span>expires {expiryLabel(inv.expiry)}</span>
            </div>
          </div>
          <button class="btn-danger" onclick={() => app.revokeInvite(inv.invite_id)}>Revoke</button>
        </div>
      {:else}
        <div class="empty-hint">No active invites for this scope.</div>
      {/each}
    </div>
    <div class="modal-foot">
      <button class="btn-primary" onclick={app.createInvite}>Create Invite</button>
    </div>
  </div>
</div>

<style>
  .invite-card {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 10px 12px;
    border-radius: var(--radius-md);
    border-top: 1px solid var(--border-hair);
  }
  .invite-card:hover {
    background: var(--bg-panel-raised);
  }
  .invite-main {
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 3px;
  }
  .invite-code {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    align-self: flex-start;
    padding: 2px 4px;
    border: none;
    background: transparent;
    color: var(--accent, #5865f2);
    font-family: var(--font-mono);
    font-weight: 600;
    cursor: pointer;
    border-radius: var(--radius-sm);
    max-width: 340px;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .invite-code:hover {
    background: var(--bg-panel);
  }
  .invite-meta {
    display: flex;
    gap: 6px;
    font-size: 12px;
    color: var(--text-muted);
  }
  .modal-foot {
    padding: 12px 16px 16px;
    border-top: 1px solid var(--border-hair);
    display: flex;
    justify-content: flex-end;
  }
  .btn-danger {
    padding: 6px 12px;
    border-radius: var(--radius-md);
    border: 1px solid var(--border-hair-strong);
    background: transparent;
    color: #f04747;
    font: inherit;
    font-weight: 600;
    cursor: pointer;
    flex: none;
  }
  .btn-danger:hover {
    background: rgba(240, 71, 71, 0.12);
  }
</style>
