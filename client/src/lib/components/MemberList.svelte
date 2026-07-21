<script lang="ts">
  import { getApp, type Member } from "$lib/context";
  import Avatar from "$lib/components/Avatar.svelte";
  const app = getApp();
  const members = $derived(app.activeChannel?.members ?? []);
  const statusOf = (name: string) =>
    name === app.account ? app.myStatus : (app.presence[name] ?? "offline");
  const isOnline = (name: string) => {
    const s = statusOf(name);
    return s !== "offline" && s !== "invisible";
  };
  // Discord-style: online members first, offline greyed at the bottom.
  const online = $derived(members.filter((m) => isOnline(m.name)));
  const offline = $derived(members.filter((m) => !isOnline(m.name)));
</script>

{#snippet row(m: Member)}
  <div class="member-row" class:member-offline={!isOnline(m.name)} role="listitem" oncontextmenu={(e) => app.memberCtx(e, m.name)}>
    <button class="member-id" onclick={(e) => app.openProfile(m.name, e)}>
      <div class="avatar"><Avatar account={m.name} /><span class="dot {statusOf(m.name)} corner"></span></div>
      <span class="mname">{app.displayName(m.name)}</span>
      {#if app.badgeFor(m.name, app.active)?.owner}<span class="cap-badge owner">owner</span>
      {:else if app.badgeFor(m.name, app.active)?.mod}<span class="cap-badge mod">mod</span>{/if}
      {#if m.origin === "federated"}<span class="cap-badge bridged">br</span>{/if}
    </button>
    {#if m.name !== app.account}
      <div class="member-actions">
        <button class="mod-btn" title="Message {m.name}" aria-label="Message {m.name}" onclick={() => app.openDm(m.name)}>
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" /></svg>
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
{/snippet}

{#if online.length}
  <div class="member-group-label">Online — {online.length}</div>
  {#each online as m (m.name)}{@render row(m)}{/each}
{/if}
{#if offline.length}
  <div class="member-group-label">Offline — {offline.length}</div>
  {#each offline as m (m.name)}{@render row(m)}{/each}
{/if}
