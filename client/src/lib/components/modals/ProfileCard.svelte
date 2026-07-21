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
  const scope = $derived(app.roleScopeOf(app.active));
  const myRoles = $derived(app.rolesOf(target, scope));
  const allRoles = $derived(app.rolesByScope[scope] ?? []);
  const isSelf = $derived(target === app.account);
  const iAmOwner = $derived(app.isOwnerAt(app.account, scope));
  const targetIsOwner = $derived(app.isOwnerAt(target, scope));
  // Roles are the only capability source, so assigning one is a privileged act:
  // offer it for other accounts (the server enforces the caller's authority),
  // and for yourself only when you own the scope — there wearing a role is
  // purely cosmetic, since the owner already holds every capability.
  const canAssignRoles = $derived(allRoles.length > 0 && (!isSelf || iAmOwner));

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

      {#if canAssignRoles}
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
        <div class="role-hint">
          {#if targetIsOwner && isSelf}Owner — you already hold every permission; roles here are just cosmetic.{:else}Click to assign · click again to remove{/if}
        </div>
      {:else if myRoles.length}
        <div class="profile-divider"></div>
        <div class="profile-section-label">Roles</div>
        <div class="role-pills">
          {#each myRoles as r (r.name)}
            <span class="role-pill" style="--role: {r.color}"><span class="role-dot"></span>{r.name}</span>
          {/each}
        </div>
      {:else if targetIsOwner}
        <div class="profile-divider"></div>
        <div class="profile-section-label">Roles</div>
        <div class="role-hint">Owner — holds all permissions.</div>
      {/if}

      <div class="profile-divider"></div>
      <div class="profile-actions">
        {#if target !== app.account}
          <button class="pf-primary" onclick={() => { app.openDm(target); onclose(); }}>Message</button>
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
