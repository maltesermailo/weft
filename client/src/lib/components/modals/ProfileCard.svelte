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
  // Roles this account doesn't hold yet — the "+" dropdown's options.
  const unheldRoles = $derived(allRoles.filter((r) => !myRoles.some((h) => h.name === r.name)));
  let roleMenuOpen = $state(false);

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

      {#if myRoles.length || canAssignRoles || targetIsOwner}
        <div class="profile-divider"></div>
        <div class="profile-section-label">Roles</div>
        <div class="role-pills">
          <!-- Discord-style: show only the roles this member holds; the "+" adds. -->
          {#each myRoles as r (r.name)}
            <span class="role-pill" style="--role: {r.color}">
              <span class="role-dot"></span>{r.name}
              {#if canAssignRoles}<button class="pill-x" title="Remove {r.name}" aria-label="Remove {r.name}" onclick={() => app.unassignRoleFrom(target, r)}>×</button>{/if}
            </span>
          {/each}
          {#if canAssignRoles && unheldRoles.length}
            <div class="role-add-wrap">
              <button class="role-add" title="Add role" aria-label="Add role" onclick={() => (roleMenuOpen = !roleMenuOpen)}>+</button>
              {#if roleMenuOpen}
                <button class="role-add-backdrop" aria-label="Close" onclick={() => (roleMenuOpen = false)}></button>
                <div class="role-add-menu">
                  {#each unheldRoles as r (r.name)}
                    <button class="role-add-item" onclick={() => { app.assignRoleTo(target, r); roleMenuOpen = false; }}>
                      <span class="role-dot" style="--role: {r.color}"></span>{r.name}
                    </button>
                  {/each}
                </div>
              {/if}
            </div>
          {/if}
        </div>
        {#if targetIsOwner}
          <div class="role-hint">Owner — holds every permission{#if isSelf}; roles here are cosmetic{/if}.</div>
        {/if}
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
