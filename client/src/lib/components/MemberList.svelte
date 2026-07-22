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

  // Roles live at the namespace scope; fetch each member's roles once so we can
  // group by hoisted role (Discord-style).
  const roleScope = $derived(app.nsRoleScope());
  $effect(() => {
    for (const m of members) app.ensureMemberRoles(m.name);
  });

  // Hoisted roles, already in position order (top = highest).
  const hoisted = $derived((app.rolesByScope[roleScope] ?? []).filter((r) => r.hoist));

  // A member's primary hoisted role = the highest (first in order) hoisted role
  // they hold, or undefined.
  function primaryHoist(name: string): string | undefined {
    const held = new Set(app.rolesOf(name, roleScope).map((r) => r.name));
    return hoisted.find((r) => held.has(r.name))?.name;
  }

  // Discord grouping: each hoisted role's ONLINE members, then everyone else
  // online under "Online", then all offline under "Offline".
  type Group = { key: string; label: string; color?: string; members: Member[] };
  const groups = $derived.by<Group[]>(() => {
    const online = members.filter((m) => isOnline(m.name));
    const offline = members.filter((m) => !isOnline(m.name));
    const out: Group[] = [];
    const claimed = new Set<string>();
    for (const role of hoisted) {
      const inRole = online.filter((m) => !claimed.has(m.name) && primaryHoist(m.name) === role.name);
      inRole.forEach((m) => claimed.add(m.name));
      if (inRole.length) {
        out.push({ key: `role:${role.name}`, label: role.name, color: role.color, members: inRole });
      }
    }
    const restOnline = online.filter((m) => !claimed.has(m.name));
    if (restOnline.length) out.push({ key: "online", label: "Online", members: restOnline });
    if (offline.length) out.push({ key: "offline", label: "Offline", members: offline });
    return out;
  });
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

{#each groups as g (g.key)}
  <div class="member-group-label" style={g.color ? `color:${g.color}` : ""}>{g.label} — {g.members.length}</div>
  {#each g.members as m (m.name)}{@render row(m)}{/each}
{/each}
