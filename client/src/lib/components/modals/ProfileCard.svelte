<script lang="ts">
  import { fade } from "svelte/transition";
  import { getApp } from "$lib/context";
  const app = getApp();
  let {
    target,
    pos,
    onclose,
  }: {
    target: string;
    pos: { left: number; top: number } | null;
    onclose: () => void;
  } = $props();

  const b = $derived(app.badgeFor(target, app.active));
  const pr = $derived(app.presence[target] ?? "offline");
  const myRoles = $derived(app.rolesOf(target, app.roleScopeOf(app.active)));

  // §6.7 moderation controls: scope (channel/namespace/network) + optional reason.
  let modScope = $state(app.scopesFor()[0]);
  let modReason = $state("");
</script>

<div class="modal-wrap" class:anchored={pos} transition:fade|global={{ duration: 190 }}>
  <button class="modal-backdrop" aria-label="Close" onclick={onclose}></button>
  <div
    class="profile-pop"
    role="dialog"
    aria-modal="true"
    style={pos ? `position:fixed; left:${pos.left}px; top:${pos.top}px` : ""}>
    <div class="profile-banner" style="--pf-accent: {myRoles[0]?.color ?? 'var(--accent, #5865f2)'}"></div>
    <div class="profile-avwrap">
      <div class="avatar xl" style="--pf-ring: {myRoles[0]?.color ?? 'var(--accent, #5865f2)'}">
        {app.initials(target)}<span class="dot {pr} corner"></span>
      </div>
    </div>
    <div class="profile-body">
      <div class="profile-name-lg">
        {target}
        {#if b?.owner}<span class="cap-badge owner">owner</span>
        {:else if b?.mod}<span class="cap-badge mod">mod</span>{/if}
      </div>
      <div class="profile-handle">{target}@{app.network} · <span class="pres-{pr}">{pr}</span></div>

      {#if app.isOwnerAt(target, app.roleScopeOf(app.active))}
        <div class="profile-divider"></div>
        <div class="profile-section-label">Roles</div>
        <div class="role-hint">Owner — holds all permissions.</div>
      {:else if target !== app.account && (app.rolesByScope[app.roleScopeOf(app.active)] ?? []).length}
        {@const allRoles = app.rolesByScope[app.roleScopeOf(app.active)] ?? []}
        <div class="profile-divider"></div>
        <div class="profile-section-label">Roles</div>
        <div class="role-pills">
          {#each allRoles as r (r.name)}
            {@const held = myRoles.some((h) => h.name === r.name)}
            <button
              class="role-pill clickable"
              class:held
              style="--role: {r.color}"
              title={held ? `Remove ${r.name}` : `Assign ${r.name}`}
              onclick={() => (held ? app.unassignRoleFrom(target, r) : app.assignRoleTo(target, r))}>
              <span class="role-dot"></span>{r.name}{#if held}<span class="pill-x">×</span>{/if}
            </button>
          {/each}
        </div>
        <div class="role-hint">Click to assign · click again to remove</div>
      {:else if myRoles.length}
        <div class="profile-divider"></div>
        <div class="profile-section-label">Roles</div>
        <div class="role-pills">
          {#each myRoles as r (r.name)}
            <span class="role-pill" style="--role: {r.color}"><span class="role-dot"></span>{r.name}</span>
          {/each}
        </div>
      {/if}

      <div class="profile-divider"></div>
      <div class="profile-actions">
        {#if target !== app.account}
          <button class="pf-primary" onclick={() => { app.openDm(target); onclose(); }}>Message</button>
          <div class="pf-row">
            <button class="pf-secondary" onclick={() => { app.openRoles(target); onclose(); }}>Manage roles</button>
          </div>
          <div class="pf-mod">
            <div class="profile-section-label">Moderation</div>
            <div class="pf-mod-inputs">
              <select bind:value={modScope} aria-label="Scope">
                {#each app.scopesFor() as s (s)}<option value={s}>{s}</option>{/each}
              </select>
              <input bind:value={modReason} placeholder="reason (optional)" />
            </div>
            <div class="pf-mod-actions">
              <button class="pf-secondary" onclick={() => app.moderate("mute", target, modScope, modReason)}>Mute</button>
              <button class="pf-secondary" onclick={() => app.moderate("unmute", target, modScope)}>Unmute</button>
              <button class="pf-secondary" onclick={() => app.moderate("kick", target, app.active, modReason)}>Kick</button>
              <button class="pf-secondary danger" onclick={() => app.moderate("ban", target, modScope, modReason)}>Ban</button>
              <button class="pf-secondary" onclick={() => app.moderate("unban", target, modScope)}>Unban</button>
            </div>
          </div>
        {/if}
        <button class="pf-secondary" onclick={() => navigator.clipboard?.writeText(`${target}@${app.network}`)}>Copy ID</button>
      </div>
    </div>
  </div>
</div>
