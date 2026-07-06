<script lang="ts">
  import { getApp } from "$lib/context";
  const app = getApp();
  const members = $derived(app.activeChannel?.members ?? []);
</script>

<div class="member-group-label">Members — {members.length}</div>
{#each members as m (m.name)}
  <div class="member-row" role="listitem" oncontextmenu={(e) => app.memberCtx(e, m.name)}>
    <button class="member-id" onclick={(e) => app.openProfile(m.name, e)}>
      <div class="avatar">{app.initials(m.name)}<span class="dot {m.name !== app.account ? (app.presence[m.name] ?? 'offline') : app.myStatus} corner"></span></div>
      <span class="mname">{m.name}</span>
      {#if app.badgeFor(m.name, app.active)?.owner}<span class="cap-badge owner">owner</span>
      {:else if app.badgeFor(m.name, app.active)?.mod}<span class="cap-badge mod">mod</span>{/if}
      {#if m.origin === "federated"}<span class="cap-badge bridged">br</span>{/if}
    </button>
    {#if m.name !== app.account}
      <div class="member-actions">
        <button class="mod-btn" title="Message {m.name}" aria-label="Message {m.name}" onclick={() => app.openDm(m.name)}>
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" /></svg>
        </button>
        <button class="mod-btn" title="Roles for {m.name}" aria-label="Roles for {m.name}" onclick={() => app.openRoles(m.name)}>
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6z" /><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" /></svg>
        </button>
        <button class="mod-btn" title="Mute {m.name}" aria-label="Mute {m.name}" onclick={() => app.moderate("mute", m.name)}>
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M11 5 6 9H2v6h4l5 4V5z" /><line x1="23" y1="9" x2="17" y2="15" /><line x1="17" y1="9" x2="23" y2="15" /></svg>
        </button>
        <button class="mod-btn danger" title="Ban {m.name}" aria-label="Ban {m.name}" onclick={() => app.moderate("ban", m.name)}>
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><circle cx="12" cy="12" r="10" /><line x1="4.9" y1="4.9" x2="19.1" y2="19.1" /></svg>
        </button>
      </div>
    {/if}
  </div>
{/each}
