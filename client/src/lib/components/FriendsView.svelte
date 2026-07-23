<script lang="ts">
  import { getApp } from "$lib/context";
  import Avatar from "$lib/components/Avatar.svelte";
  const app = getApp();

  // Local friends get a presence dot + avatar by handle; federated friends
  // render by their full `account@network` ref.
  const avatarAccount = (user: string) => app.friendLocalAccount(user) ?? user;

  function onAddKey(e: KeyboardEvent) {
    if (e.key === "Enter") {
      e.preventDefault();
      app.addFriend();
    }
  }
</script>

<div class="friends-view">
  <div class="fv-head">
    <div class="fv-title">
      <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2" /><circle cx="9" cy="7" r="4" /><path d="M22 21v-2a4 4 0 0 0-3-3.87" /><path d="M16 3.13a4 4 0 0 1 0 7.75" /></svg>
      Friends
    </div>
  </div>

  <div class="fv-add">
    <input
      placeholder="Add a friend — handle or account@network"
      bind:value={app.addFriendInput}
      onkeydown={onAddKey}
    />
    <button class="btn-primary" disabled={!app.addFriendInput.trim()} onclick={app.addFriend}>
      Send Request
    </button>
  </div>

  <div class="fv-scroll">
    {#if app.incomingRequests.length}
      <div class="fv-section">Incoming Requests — {app.incomingRequests.length}</div>
      {#each app.incomingRequests as user (user)}
        <div class="fv-row">
          <div class="avatar sm"><Avatar account={avatarAccount(user)} /></div>
          <div class="fv-name">{app.friendLabel(user)}<span class="fv-sub">wants to be friends</span></div>
          <div class="fv-actions">
            <button class="icon-pill accept" title="Accept" aria-label="Accept" onclick={() => app.acceptFriend(user)}>
              <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2"><path d="M20 6 9 17l-5-5" /></svg>
            </button>
            <button class="icon-pill decline" title="Decline" aria-label="Decline" onclick={() => app.removeFriend(user)}>
              <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2"><path d="M18 6 6 18M6 6l12 12" /></svg>
            </button>
          </div>
        </div>
      {/each}
    {/if}

    {#if app.outgoingRequests.length}
      <div class="fv-section">Sent Requests — {app.outgoingRequests.length}</div>
      {#each app.outgoingRequests as user (user)}
        <div class="fv-row">
          <div class="avatar sm"><Avatar account={avatarAccount(user)} /></div>
          <div class="fv-name">{app.friendLabel(user)}<span class="fv-sub">request sent</span></div>
          <div class="fv-actions">
            <button class="icon-pill decline" title="Cancel" aria-label="Cancel" onclick={() => app.removeFriend(user)}>
              <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2"><path d="M18 6 6 18M6 6l12 12" /></svg>
            </button>
          </div>
        </div>
      {/each}
    {/if}

    <div class="fv-section">All Friends — {app.friendList.length}</div>
    {#each app.friendList as user (user)}
      <div class="fv-row">
        <div class="avatar sm">
          <Avatar account={avatarAccount(user)} />
          {#if app.friendLocalAccount(user)}<span class={app.dotClass(app.friendLocalAccount(user) ?? "")}></span>{/if}
        </div>
        <div class="fv-name">{app.friendLabel(user)}</div>
        <div class="fv-actions">
          {#if app.friendLocalAccount(user)}
            <button class="icon-pill" title="Message" aria-label="Message" onclick={() => app.messageFriend(user)}>
              <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" /></svg>
            </button>
          {/if}
          <button class="icon-pill decline" title="Remove friend" aria-label="Remove friend" onclick={() => app.removeFriend(user)}>
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9"><path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2" /><circle cx="9" cy="7" r="4" /><path d="M17 8l5 5m0-5-5 5" /></svg>
          </button>
        </div>
      </div>
    {:else}
      {#if !app.incomingRequests.length && !app.outgoingRequests.length}
        <div class="fv-empty">No friends yet — add someone by their handle above. Friends can live on other networks too (<code>name@network</code>).</div>
      {/if}
    {/each}
  </div>
</div>

<style>
  .friends-view {
    display: flex;
    flex-direction: column;
    height: 100%;
    min-height: 0;
  }
  .fv-head {
    padding: 14px 20px;
    border-bottom: 1px solid var(--border-hair);
  }
  .fv-title {
    display: flex;
    align-items: center;
    gap: 10px;
    font-weight: 700;
    font-size: 16px;
  }
  .fv-add {
    display: flex;
    gap: 10px;
    padding: 16px 20px;
    border-bottom: 1px solid var(--border-hair);
  }
  .fv-add input {
    flex: 1;
    padding: 10px 12px;
    border-radius: var(--radius-md);
    border: 1px solid var(--border-hair-strong);
    background: var(--bg-panel-raised);
    color: var(--text-primary);
    font: inherit;
  }
  .fv-scroll {
    flex: 1;
    overflow-y: auto;
    padding: 8px 12px 20px;
  }
  .fv-section {
    padding: 16px 8px 8px;
    font-size: 12px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--text-muted);
  }
  .fv-row {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 8px 8px;
    border-radius: var(--radius-md);
    border-top: 1px solid var(--border-hair);
  }
  .fv-row:hover {
    background: var(--bg-panel-raised);
  }
  .fv-name {
    flex: 1;
    min-width: 0;
    font-weight: 600;
    display: flex;
    flex-direction: column;
  }
  .fv-sub {
    font-size: 12px;
    font-weight: 400;
    color: var(--text-muted);
  }
  .fv-actions {
    display: flex;
    gap: 8px;
  }
  .icon-pill {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 34px;
    height: 34px;
    border-radius: 50%;
    border: none;
    background: var(--bg-panel);
    color: var(--text-secondary, var(--text-muted));
    cursor: pointer;
  }
  .icon-pill:hover {
    color: var(--text-primary);
    background: var(--bg-hover);
  }
  .icon-pill.accept:hover {
    color: #43b581;
  }
  .icon-pill.decline:hover {
    color: #f04747;
  }
  .fv-empty {
    padding: 30px 12px;
    color: var(--text-muted);
    text-align: center;
    line-height: 1.6;
  }
  .fv-empty code {
    font-family: var(--font-mono);
    font-size: 12px;
  }
</style>
