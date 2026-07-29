<script lang="ts">
  // §6.5 roles tab, redesigned as a two-pane editor (design/server-settings.html):
  // a searchable, drag-orderable role list on the left; a tabbed editor
  // (Display / Permissions) for the selected role on the right. Order is
  // top = highest, mirroring the member-list grouping. The implicit @everyone
  // baseline and the "create a role" form are selectable rows in the same list.
  import { getApp } from "$lib/context";
  import type { Role } from "$lib/context";
  import { CAP_GROUPS, CAP_META, ROLE_COLORS, EVERYONE_ROLE } from "$lib/constants";
  import Avatar from "$lib/components/Avatar.svelte";
  import SaveBar from "$lib/components/SaveBar.svelte";

  const app = getApp();
  const scope = $derived(app.nsRoleScope());
  // The implicit @everyone role is edited as its own selection, not dragged,
  // renamed, colored or deleted like a normal role.
  const roles = $derived(
    app.rolesAt(app.nsRoleScope()).filter((r) => r.name !== EVERYONE_ROLE),
  );

  const sameCaps = (a: string[], b: string[]) =>
    a.length === b.length && [...a].sort().join() === [...b].sort().join();

  // ---- selection: a role name, EVERYONE_ROLE, "__new__", or null ----
  let selected = $state<string | null>(null);
  let tab = $state<"display" | "permissions" | "members">("display");
  let search = $state("");

  // Members holding the selected role. Sourced from the union of this
  // namespace's visible channel rosters (the fullest roster the client has),
  // then filtered by assignment. `rolesOf` needs the per-member data loaded.
  const nsMembers = $derived.by(() => {
    const seen = new Set<string>();
    const out: string[] = [];
    for (const g of app.channelGroups)
      for (const c of g.list)
        for (const m of c.members)
          if (!seen.has(m.name)) {
            seen.add(m.name);
            out.push(m.name);
          }
    return out.sort((a, b) => a.localeCompare(b));
  });
  $effect(() => {
    for (const name of nsMembers) app.ensureMemberRoles(name);
  });
  let memberSearch = $state("");

  const filtered = $derived(
    roles.filter((r) => r.name.toLowerCase().includes(search.toLowerCase())),
  );

  // ---- edit draft for a normal role ----
  let draft = $state({ name: "", color: "", caps: [] as string[], hoist: false, pingable: false });
  const editing = $derived(
    selected && selected !== EVERYONE_ROLE && selected !== "__new__"
      ? (roles.find((r) => r.id === selected) ?? null)
      : null,
  );

  // Members holding the selected role (filtered by the members-tab search).
  const roleMembers = $derived(
    editing
      ? nsMembers.filter(
          (n) =>
            app.rolesOf(n, scope).some((r) => r.id === editing!.id) &&
            (app.displayName(n).toLowerCase().includes(memberSearch.toLowerCase()) ||
              n.toLowerCase().includes(memberSearch.toLowerCase())),
        )
      : [],
  );

  function pick(id: string) {
    selected = id;
    tab = "display";
    const r = roles.find((x) => x.id === id);
    if (r) draft = { name: r.name, color: r.color, caps: [...r.caps], hoist: r.hoist, pingable: r.pingable };
  }
  function pickEveryone() {
    selected = EVERYONE_ROLE;
    tab = "permissions";
    everyoneDraft = [...app.everyoneCaps()];
  }
  function pickNew() {
    selected = "__new__";
    tab = "display";
  }

  const toggleDraftCap = (c: string) =>
    (draft.caps = draft.caps.includes(c) ? draft.caps.filter((x) => x !== c) : [...draft.caps, c]);

  const dirty = $derived(
    !!editing &&
      (draft.name.trim() !== editing.name ||
        draft.color !== editing.color ||
        draft.hoist !== editing.hoist ||
        draft.pingable !== editing.pingable ||
        !sameCaps(draft.caps, editing.caps)),
  );

  function save() {
    if (!editing) return;
    app.saveRole(editing, {
      name: draft.name,
      color: draft.color,
      caps: draft.caps,
      hoist: draft.hoist,
      pingable: draft.pingable,
    });
    selected = null; // the refreshed ROLES batch is the confirmation
  }
  function remove(id: string) {
    if (selected === id) selected = null;
    app.deleteRole(id);
  }
  function create() {
    app.createRole();
    selected = null;
  }

  // ---- @everyone baseline editor ----
  let everyoneDraft = $state<string[]>([]);
  const everyoneDirty = $derived(!sameCaps(everyoneDraft, app.everyoneCaps()));
  const toggleEveryoneCap = (c: string) =>
    (everyoneDraft = everyoneDraft.includes(c)
      ? everyoneDraft.filter((x) => x !== c)
      : [...everyoneDraft, c]);

  // ---- shared Revert/Save bar (the profile-editor pattern) ----
  // Shown while the current selection has unsaved changes; only one selection
  // is editable at a time so a single bar covers both role + @everyone.
  const showSaveBar = $derived((selected === EVERYONE_ROLE && everyoneDirty) || (!!editing && dirty));
  // A role may hold zero permissions (a cosmetic/hoist role, granted caps
  // later) — only the name is required.
  const saveDisabled = $derived(selected !== EVERYONE_ROLE && !draft.name.trim());
  function revertSelection() {
    if (selected === EVERYONE_ROLE) everyoneDraft = [...app.everyoneCaps()];
    else if (editing) pick(editing.id);
  }
  function saveSelection() {
    if (selected === EVERYONE_ROLE) app.setEveryoneCaps(everyoneDraft);
    else if (editing) save();
  }

  // ---- drag-and-drop reordering ----
  let dragFrom = $state<number | null>(null);
  let dragOver = $state<number | null>(null);
  const resetDrag = () => {
    dragFrom = null;
    dragOver = null;
  };
  function onDragStart(e: DragEvent, i: number) {
    dragFrom = i;
    e.dataTransfer?.setData("text/plain", filtered[i].id);
    if (e.dataTransfer) e.dataTransfer.effectAllowed = "move";
  }
  function onDragOver(e: DragEvent, i: number) {
    if (dragFrom === null) return;
    e.preventDefault();
    if (e.dataTransfer) e.dataTransfer.dropEffect = "move";
    dragOver = i;
  }
  function onDrop(e: DragEvent, to: number) {
    e.preventDefault();
    const from = dragFrom;
    resetDrag();
    if (from === null || from === to) return;
    const list = filtered.map((r) => r.id);
    const [moved] = list.splice(from, 1);
    list.splice(to, 0, moved);
    app.reorderRoles(list);
  }
  function onRowKey(e: KeyboardEvent, r: Role) {
    if (!e.altKey || (e.key !== "ArrowUp" && e.key !== "ArrowDown")) return;
    e.preventDefault();
    app.moveRole(r.id, e.key === "ArrowUp" ? -1 : 1);
  }

  // Currently-selected role's live color, for the editor header preview.
  const headColor = $derived(editing ? draft.color : "#99aab5");
</script>

<div class="rl-wrap">
  <!-- ─── Role list ─── -->
  <aside class="rl-side">
    <div class="rl-side-top">
      <div class="rl-search">
        <span aria-hidden="true">⌕</span>
        <input bind:value={search} placeholder="Search roles" />
      </div>
      <button class="rl-create-btn" onclick={pickNew}>+ Create Role</button>
    </div>
    <div class="rl-side-label">Roles — {roles.length}</div>
    <div class="rl-list">
      {#each filtered as r, i (r.id)}
        <div
          class="rl-row"
          class:active={selected === r.id}
          class:dragging={dragFrom === i}
          class:drop-before={dragOver === i && dragFrom !== null && dragFrom > i}
          class:drop-after={dragOver === i && dragFrom !== null && dragFrom < i}
          draggable="true"
          role="button"
          tabindex="0"
          ondragstart={(e) => onDragStart(e, i)}
          ondragover={(e) => onDragOver(e, i)}
          ondrop={(e) => onDrop(e, i)}
          ondragend={resetDrag}
          onclick={() => pick(r.id)}
          onkeydown={(e) => {
            if (e.key === "Enter" || e.key === " ") { e.preventDefault(); pick(r.id); }
            else onRowKey(e, r);
          }}
          title="Edit {r.name} — Alt+↑/↓ to reorder"
        >
          <span class="rl-grip" aria-hidden="true">⠿</span>
          <span class="rl-dot" style="background:{r.color}"></span>
          <span class="rl-name">{r.name}</span>
          {#if r.hoist}<span class="rl-flag" title="Shown separately in the member list">★</span>{/if}
          {#if r.pingable}<span class="rl-flag" title="Members can @-mention this role">@</span>{/if}
        </div>
      {:else}
        <div class="empty-hint" style="padding:10px">No roles yet — create one.</div>
      {/each}

      <div class="rl-divider"></div>
      <div
        class="rl-row"
        class:active={selected === EVERYONE_ROLE}
        role="button"
        tabindex="0"
        onclick={pickEveryone}
        onkeydown={(e) => (e.key === "Enter" || e.key === " ") && (e.preventDefault(), pickEveryone())}
        title="Edit the @everyone baseline"
      >
        <span class="rl-grip rl-grip-empty" aria-hidden="true"></span>
        <span class="rl-dot" style="background:#99aab5"></span>
        <span class="rl-name">@everyone</span>
      </div>
    </div>
  </aside>

  <!-- ─── Editor ─── -->
  <section class="rl-editor">
    {#if selected === "__new__"}
      <div class="rl-head">
        <div class="rl-head-title">
          <span class="rl-dot lg" style="background:{app.newRoleColor}"></span>
          <span>{app.newRoleName.trim() || "New role"}</span>
        </div>
      </div>
      <div class="rl-body">
        <div class="rl-field">
          <div class="field-label">Role name</div>
          <input class="text-input" bind:value={app.newRoleName} placeholder="e.g. Moderator" />
        </div>
        {@render colorField(app.newRoleColor, (c) => (app.newRoleColor = c), app.newRoleName)}
        {@render displayOptions(app.newRoleHoist, (v) => (app.newRoleHoist = v), app.newRolePingable, (v) => (app.newRolePingable = v), app.newRoleName)}
        {@render permGroups(app.newRoleCaps, app.toggleNewRoleCap)}
        <div class="rl-actions">
          <button class="ok-btn" disabled={!app.newRoleName.trim()} onclick={create}>Create role</button>
          <button class="linkish" onclick={() => (selected = null)}>Cancel</button>
        </div>
      </div>
    {:else if selected === EVERYONE_ROLE}
      <div class="rl-head">
        <div class="rl-head-title">
          <span class="rl-dot lg" style="background:#99aab5"></span>
          <span>@everyone</span>
          <span class="rl-badge">Baseline</span>
        </div>
      </div>
      <div class="rl-tabs">
        <button class="active">Permissions</button>
      </div>
      <div class="rl-body">
        <p class="so-sub">Every member holds these implicitly. Grant a permission here and it applies to the whole namespace.</p>
        {@render permGroups(everyoneDraft, toggleEveryoneCap)}
      </div>
    {:else if editing}
      <div class="rl-head">
        <div class="rl-head-title">
          <span class="rl-dot lg" style="background:{headColor}"></span>
          <span>{draft.name.trim() || editing.name}</span>
          {#if draft.caps.includes("ns-admin")}<span class="rl-badge admin">Administrator</span>{/if}
        </div>
        <button class="rl-delete" onclick={() => remove(editing.id)}>🗑 Delete</button>
      </div>
      <div class="rl-tabs">
        <button class:active={tab === "display"} onclick={() => (tab = "display")}>Display</button>
        <button class:active={tab === "permissions"} onclick={() => (tab = "permissions")}>Permissions</button>
        <button class:active={tab === "members"} onclick={() => (tab = "members")}>Members</button>
      </div>
      <div class="rl-body">
        {#if tab === "display"}
          <div class="rl-field">
            <div class="field-label">Role name</div>
            <input
              class="text-input"
              bind:value={draft.name}
              placeholder="Role name"
              onkeydown={(e) => e.key === "Enter" && dirty && draft.name.trim() && draft.caps.length && save()}
            />
          </div>
          {@render colorField(draft.color, (c) => (draft.color = c), draft.name)}
          {@render displayOptions(draft.hoist, (v) => (draft.hoist = v), draft.pingable, (v) => (draft.pingable = v), draft.name)}
          {#if draft.name.trim() && draft.name.trim() !== editing.name}
            <p class="rename-note">Renaming keeps every member and granted permission — the role is renamed in place.</p>
          {/if}
        {:else if tab === "permissions"}
          {@render permGroups(draft.caps, toggleDraftCap)}
        {:else}
          <div class="rl-member-search">
            <span aria-hidden="true">⌕</span>
            <input bind:value={memberSearch} placeholder="Search members" />
          </div>
          <div class="rl-member-count">{roleMembers.length} {roleMembers.length === 1 ? "member" : "members"}</div>
          {#each roleMembers as name (name)}
            <div class="rl-member">
              <span class="rl-member-avatar"><Avatar account={name} /></span>
              <div class="rl-member-meta">
                <div class="rl-member-name">{app.displayName(name)}</div>
                <div class="rl-member-handle">{name}</div>
              </div>
              <button
                class="rl-member-x"
                aria-label="Remove {name} from {editing.name}"
                title="Remove from {editing.name}"
                onclick={() => app.unassignRoleFrom(name, editing)}
              >✕</button>
            </div>
          {:else}
            <div class="rl-member-empty">
              {memberSearch ? "No members match your search." : `No members hold ${editing.name} yet — assign it from the Members & roles tab.`}
            </div>
          {/each}
        {/if}
      </div>
    {:else}
      <div class="rl-empty">
        <div class="rl-empty-icon">🎭</div>
        <h2>Roles</h2>
        <p>Named capability bundles — assigning a role grants its tokens, so enforcement stays token-based. Pick a role to edit it, or create a new one.</p>
      </div>
    {/if}
  </section>
</div>

{#if showSaveBar}
  <SaveBar {saveDisabled} onrevert={revertSelection} onsave={saveSelection} />
{/if}

<!-- ─── Reusable editor fragments ─── -->
{#snippet colorField(color: string, setColor: (c: string) => void, name: string)}
  <div class="rl-field">
    <div class="field-label">Role color</div>
    <div class="rl-color-head">
      <span class="rl-color-chip" style="background:{color}"></span>
      <span class="rl-color-preview" style="color:{color}">@{name.trim() || "role"}</span>
    </div>
    <div class="rl-color-grid">
      {#each ROLE_COLORS as c (c)}
        <button
          class="rl-color-dot"
          class:on={color === c}
          style="background:{c}"
          aria-label="color {c}"
          onclick={() => setColor(c)}
        ></button>
      {/each}
    </div>
  </div>
{/snippet}

{#snippet displayOptions(hoist: boolean, setHoist: (v: boolean) => void, pingable: boolean, setPingable: (v: boolean) => void, name: string)}
  <div class="rl-field">
    <div class="field-label">Display options</div>
    <div class="rl-opt-card">
      <div class="rl-opt">
        <div class="rl-opt-text">
          <div class="rl-opt-label">Display role members separately</div>
          <div class="rl-opt-sub">This role is shown as its own group in the member list.</div>
        </div>
        {@render toggle(hoist, () => setHoist(!hoist), "Display role members separately")}
      </div>
      <div class="rl-opt">
        <div class="rl-opt-text">
          <div class="rl-opt-label">Allow anyone to @mention this role</div>
          <div class="rl-opt-sub">Members can use @{name.trim() || "role"} to ping everyone in this role.</div>
        </div>
        {@render toggle(pingable, () => setPingable(!pingable), "Allow anyone to @mention this role")}
      </div>
    </div>
  </div>
{/snippet}

{#snippet permGroups(caps: string[], onToggle: (c: string) => void)}
  {#if caps.includes("ns-admin")}
    <div class="rl-admin-note">
      <span aria-hidden="true">⚡</span>
      <div>
        <div class="rl-admin-title">Administrator permission active</div>
        <div class="rl-admin-sub">This role has full control over the namespace regardless of the toggles below.</div>
      </div>
    </div>
  {/if}
  {#each CAP_GROUPS as group (group.label)}
    <div class="rl-perm-group">
      <div class="rl-perm-grouplabel">{group.label}</div>
      {#each group.caps as cap (cap)}
        <div class="rl-perm">
          <div class="rl-opt-text">
            <div class="rl-opt-label">{CAP_META[cap]?.label ?? cap}</div>
            <div class="rl-opt-sub">{CAP_META[cap]?.desc ?? ""}</div>
          </div>
          {@render toggle(caps.includes(cap), () => onToggle(cap), CAP_META[cap]?.label ?? cap)}
        </div>
      {/each}
    </div>
  {/each}
{/snippet}

{#snippet toggle(on: boolean, onclick: () => void, label: string)}
  <button class="rl-toggle" class:on role="switch" aria-checked={on} aria-label={label} {onclick}>
    <span class="rl-toggle-knob"></span>
  </button>
{/snippet}

<style>
  .rl-wrap {
    display: flex;
    gap: 0;
    min-height: 480px;
    border: 1px solid var(--border-hair);
    border-radius: 8px;
    overflow: hidden;
    background: var(--bg-panel);
  }

  /* ── list ── */
  .rl-side {
    width: 240px;
    flex-shrink: 0;
    display: flex;
    flex-direction: column;
    border-right: 1px solid var(--border-hair);
    background: var(--bg-panel);
  }
  .rl-side-top {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 12px;
    border-bottom: 1px solid var(--border-hair);
  }
  .rl-search {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px 12px;
    border-radius: 4px;
    background: var(--bg-void);
    color: var(--text-muted);
  }
  .rl-search input {
    flex: 1;
    min-width: 0;
    border: none;
    background: none;
    color: var(--text-primary);
    font: inherit;
    font-size: 14px;
    outline: none;
  }
  .rl-create-btn {
    padding: 8px 0;
    border: none;
    border-radius: 4px;
    background: var(--accent, #5865f2);
    color: #fff;
    font: inherit;
    font-size: 14px;
    font-weight: 700;
    cursor: pointer;
  }
  .rl-create-btn:hover {
    filter: brightness(1.08);
  }
  .rl-side-label {
    padding: 10px 12px 4px;
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--text-muted);
  }
  .rl-list {
    flex: 1;
    overflow-y: auto;
    padding: 4px 8px 12px;
  }
  .rl-row {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 8px 10px;
    border-radius: 4px;
    cursor: pointer;
    user-select: none;
    border-top: 2px solid transparent;
    border-bottom: 2px solid transparent;
  }
  .rl-row:hover {
    background: var(--bg-hover);
  }
  .rl-row.active {
    background: var(--bg-hover);
  }
  .rl-row.dragging {
    opacity: 0.45;
  }
  .rl-row.drop-before {
    border-top-color: var(--accent, #5865f2);
  }
  .rl-row.drop-after {
    border-bottom-color: var(--accent, #5865f2);
  }
  .rl-grip {
    color: var(--text-faint);
    font-size: 12px;
    line-height: 1;
    cursor: grab;
  }
  .rl-grip-empty {
    width: 7px;
  }
  .rl-dot {
    width: 12px;
    height: 12px;
    flex-shrink: 0;
    border-radius: 50%;
  }
  .rl-dot.lg {
    width: 20px;
    height: 20px;
  }
  .rl-name {
    flex: 1;
    min-width: 0;
    font-size: 14px;
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .rl-flag {
    flex-shrink: 0;
    font-size: 12px;
    color: var(--accent, #5865f2);
  }
  .rl-divider {
    height: 1px;
    margin: 8px 4px;
    background: var(--border-hair);
  }

  /* ── editor ── */
  .rl-editor {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    background: var(--bg-void);
  }
  .rl-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 20px 24px 16px;
    flex-shrink: 0;
  }
  .rl-head-title {
    display: flex;
    align-items: center;
    gap: 12px;
    font-size: 20px;
    font-weight: 700;
    color: var(--text-primary);
    min-width: 0;
  }
  .rl-head-title > span:nth-child(2) {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .rl-badge {
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    padding: 3px 10px;
    border-radius: 99px;
    background: var(--bg-hover);
    color: var(--text-secondary);
  }
  .rl-badge.admin {
    background: color-mix(in srgb, var(--accent, #5865f2) 22%, transparent);
    color: var(--accent, #7aa2f7);
  }
  .rl-delete {
    display: flex;
    align-items: center;
    gap: 6px;
    flex-shrink: 0;
    padding: 7px 16px;
    border: 1px solid color-mix(in srgb, var(--danger, #d9685f) 45%, transparent);
    border-radius: 4px;
    background: color-mix(in srgb, var(--danger, #d9685f) 10%, transparent);
    color: var(--danger, #d9685f);
    font: inherit;
    font-size: 13px;
    font-weight: 600;
    cursor: pointer;
  }
  .rl-delete:hover {
    background: color-mix(in srgb, var(--danger, #d9685f) 18%, transparent);
  }
  .rl-tabs {
    display: flex;
    gap: 0;
    padding: 0 24px;
    border-bottom: 1px solid var(--border-hair);
    flex-shrink: 0;
  }
  .rl-tabs button {
    padding: 12px 20px;
    border: none;
    border-bottom: 2px solid transparent;
    margin-bottom: -1px;
    background: none;
    color: var(--text-muted);
    font: inherit;
    font-size: 14px;
    font-weight: 600;
    cursor: pointer;
  }
  .rl-tabs button:hover {
    color: var(--text-secondary);
  }
  .rl-tabs button.active {
    color: var(--text-primary);
    border-bottom-color: var(--accent, #5865f2);
  }
  .rl-body {
    flex: 1;
    overflow-y: auto;
    padding: 22px 24px 28px;
    max-width: 560px;
  }
  .rl-field {
    margin-bottom: 26px;
  }

  /* color */
  .rl-color-head {
    display: flex;
    align-items: center;
    gap: 16px;
    margin-bottom: 14px;
  }
  .rl-color-chip {
    width: 44px;
    height: 44px;
    border-radius: 8px;
    border: 2px solid rgba(255, 255, 255, 0.12);
    flex-shrink: 0;
  }
  .rl-color-preview {
    font-size: 15px;
    font-weight: 600;
  }
  .rl-color-grid {
    display: flex;
    flex-wrap: wrap;
    gap: 10px;
  }
  .rl-color-dot {
    width: 32px;
    height: 32px;
    border-radius: 6px;
    border: 2px solid transparent;
    cursor: pointer;
    padding: 0;
  }
  .rl-color-dot.on {
    border-color: #fff;
    outline: 2px solid var(--accent, #5865f2);
    outline-offset: 2px;
  }

  /* display options / permission rows */
  .rl-opt-card {
    border-radius: 8px;
    overflow: hidden;
    background: var(--bg-panel);
    border: 1px solid var(--border-hair);
  }
  .rl-opt {
    display: flex;
    align-items: flex-start;
    gap: 20px;
    padding: 16px 18px;
  }
  .rl-opt + .rl-opt {
    border-top: 1px solid var(--border-hair);
  }
  .rl-opt-text {
    flex: 1;
    min-width: 0;
  }
  .rl-opt-label {
    font-size: 14px;
    font-weight: 600;
    color: var(--text-primary);
    margin-bottom: 3px;
  }
  .rl-opt-sub {
    font-size: 13px;
    color: var(--text-muted);
    line-height: 1.4;
  }
  .rl-perm-group {
    margin-bottom: 30px;
  }
  .rl-perm-grouplabel {
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--text-muted);
    padding-bottom: 10px;
    margin-bottom: 4px;
    border-bottom: 1px solid var(--border-hair);
  }
  .rl-perm {
    display: flex;
    align-items: flex-start;
    gap: 16px;
    padding: 14px 4px;
    border-bottom: 1px solid color-mix(in srgb, var(--border-hair) 60%, transparent);
  }
  .rl-perm:last-child {
    border-bottom: none;
  }

  .rl-admin-note {
    display: flex;
    align-items: flex-start;
    gap: 14px;
    padding: 16px 18px;
    margin-bottom: 26px;
    border-radius: 8px;
    background: color-mix(in srgb, var(--thread-amber, #e2a13d) 12%, transparent);
    border: 1px solid color-mix(in srgb, var(--thread-amber, #e2a13d) 30%, transparent);
  }
  .rl-admin-note > span {
    font-size: 18px;
    color: var(--thread-amber, #e2a13d);
  }
  .rl-admin-title {
    font-size: 14px;
    font-weight: 700;
    color: var(--thread-amber, #e2a13d);
    margin-bottom: 3px;
  }
  .rl-admin-sub {
    font-size: 13px;
    color: var(--text-muted);
  }

  /* toggle switch */
  .rl-toggle {
    position: relative;
    flex-shrink: 0;
    width: 44px;
    height: 24px;
    border: none;
    border-radius: 12px;
    background: var(--border-hair-strong);
    cursor: pointer;
    padding: 0;
    transition: background 0.15s;
  }
  .rl-toggle.on {
    background: var(--signal-teal, #23a55a);
  }
  .rl-toggle-knob {
    position: absolute;
    top: 2px;
    left: 2px;
    width: 20px;
    height: 20px;
    border-radius: 50%;
    background: #fff;
    box-shadow: 0 1px 3px rgba(0, 0, 0, 0.4);
    transition: left 0.15s;
  }
  .rl-toggle.on .rl-toggle-knob {
    left: 22px;
  }

  .rl-actions {
    display: flex;
    align-items: center;
    gap: 12px;
    margin-top: 8px;
  }
  .rename-note {
    margin: 0 0 16px;
    font-size: 12px;
    color: var(--text-muted);
  }

  /* members tab */
  .rl-member-search {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 10px 14px;
    margin-bottom: 16px;
    border-radius: 4px;
    background: var(--bg-panel);
    border: 1px solid var(--border-hair);
    color: var(--text-muted);
  }
  .rl-member-search input {
    flex: 1;
    min-width: 0;
    border: none;
    background: none;
    color: var(--text-primary);
    font: inherit;
    font-size: 14px;
    outline: none;
  }
  .rl-member-count {
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--text-muted);
    margin-bottom: 10px;
  }
  .rl-member {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 8px 10px;
    border-radius: 6px;
  }
  .rl-member:hover {
    background: var(--bg-panel);
  }
  .rl-member-avatar {
    display: flex;
    align-items: center;
    justify-content: center;
    width: 34px;
    height: 34px;
    flex-shrink: 0;
    border-radius: 50%;
    overflow: hidden;
    background: var(--accent, #5865f2);
    color: #fff;
    font-size: 13px;
    font-weight: 600;
    text-transform: uppercase;
  }
  .rl-member-avatar :global(.avatar-img) {
    width: 100%;
    height: 100%;
    object-fit: cover;
  }
  .rl-member-meta {
    flex: 1;
    min-width: 0;
  }
  .rl-member-name {
    font-size: 14px;
    font-weight: 600;
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .rl-member-handle {
    font-size: 12px;
    color: var(--text-muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .rl-member-x {
    flex-shrink: 0;
    border: none;
    background: none;
    color: var(--text-muted);
    font-size: 15px;
    cursor: pointer;
    opacity: 0.5;
    padding: 4px 6px;
    border-radius: 4px;
  }
  .rl-member:hover .rl-member-x {
    opacity: 0.9;
  }
  .rl-member-x:hover {
    color: var(--danger, #d9685f);
    background: color-mix(in srgb, var(--danger, #d9685f) 12%, transparent);
  }
  .rl-member-empty {
    padding: 32px 4px;
    text-align: center;
    font-size: 14px;
    color: var(--text-muted);
  }

  .rl-empty {
    flex: 1;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    text-align: center;
    gap: 8px;
    padding: 48px;
  }
  .rl-empty-icon {
    font-size: 34px;
    opacity: 0.5;
    margin-bottom: 6px;
  }
  .rl-empty h2 {
    margin: 0;
    font-size: 20px;
    font-weight: 700;
    color: var(--text-primary);
  }
  .rl-empty p {
    margin: 0;
    max-width: 380px;
    font-size: 14px;
    line-height: 1.6;
    color: var(--text-muted);
  }

  @media (max-width: 720px) {
    .rl-wrap {
      flex-direction: column;
    }
    .rl-side {
      width: auto;
      border-right: none;
      border-bottom: 1px solid var(--border-hair);
    }
  }
</style>
