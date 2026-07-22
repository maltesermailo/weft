<script lang="ts">
  // §6.5 roles tab: a drag-orderable list of the namespace's roles, an inline
  // editor for the selected one (rename / color / hoist / permissions), and the
  // create form. Order is top = highest, mirroring the member-list grouping.
  import { getApp } from "$lib/context";
  import type { RoleDefC } from "$lib/types";
  import { CAPS, ROLE_COLORS } from "$lib/constants";
  import CapChecklist from "$lib/components/CapChecklist.svelte";

  const app = getApp();
  const roles = $derived(app.rolesByScope[app.nsRoleScope()] ?? []);

  // ---- selection + edit draft ----
  let selected = $state<string | null>(null);
  let draft = $state({ name: "", color: "", caps: [] as string[], hoist: false });
  const editing = $derived(roles.find((r) => r.name === selected) ?? null);

  function select(r: RoleDefC) {
    if (selected === r.name) {
      selected = null; // clicking the open role collapses it
      return;
    }
    selected = r.name;
    draft = { name: r.name, color: r.color, caps: [...r.caps], hoist: r.hoist };
  }
  const toggleDraftCap = (c: string) =>
    (draft.caps = draft.caps.includes(c) ? draft.caps.filter((x) => x !== c) : [...draft.caps, c]);

  const sameCaps = (a: string[], b: string[]) =>
    a.length === b.length && [...a].sort().join() === [...b].sort().join();
  const dirty = $derived(
    !!editing &&
      (draft.name.trim() !== editing.name ||
        draft.color !== editing.color ||
        draft.hoist !== editing.hoist ||
        !sameCaps(draft.caps, editing.caps)),
  );

  function save() {
    if (!editing) return;
    app.saveRole(editing, {
      name: draft.name,
      color: draft.color,
      caps: draft.caps,
      hoist: draft.hoist,
    });
    selected = null; // the refreshed ROLES batch is the confirmation
  }
  function remove(name: string) {
    if (selected === name) selected = null;
    app.deleteRole(name);
  }

  // ---- drag-and-drop reordering ----
  // `dragFrom` is the grabbed index; `dragOver` marks where it would land, so the
  // list can draw an insertion line before committing.
  let dragFrom = $state<number | null>(null);
  let dragOver = $state<number | null>(null);
  const resetDrag = () => {
    dragFrom = null;
    dragOver = null;
  };

  function onDragStart(e: DragEvent, i: number) {
    dragFrom = i;
    // Firefox only starts a drag once some data is set.
    e.dataTransfer?.setData("text/plain", roles[i].name);
    if (e.dataTransfer) e.dataTransfer.effectAllowed = "move";
  }
  function onDragOver(e: DragEvent, i: number) {
    if (dragFrom === null) return;
    e.preventDefault(); // required, or the drop never fires
    if (e.dataTransfer) e.dataTransfer.dropEffect = "move";
    dragOver = i;
  }
  function onDrop(e: DragEvent, to: number) {
    e.preventDefault();
    const from = dragFrom;
    resetDrag();
    if (from === null || from === to) return;
    const list = roles.map((r) => r.name);
    const [moved] = list.splice(from, 1);
    list.splice(to, 0, moved);
    app.reorderRoles(list);
  }

  // Keyboard equivalent of a drag, so ordering isn't mouse-only.
  function onRowKey(e: KeyboardEvent, r: RoleDefC) {
    if (!e.altKey || (e.key !== "ArrowUp" && e.key !== "ArrowDown")) return;
    e.preventDefault();
    app.moveRole(r.name, e.key === "ArrowUp" ? -1 : 1);
  }
</script>

<h1>Roles</h1>
<p class="so-sub">
  Named capability bundles — assigning a role grants its tokens, so enforcement stays token-based.
  Drag to reorder (top = highest); the order drives member-list grouping. Click a role to edit its
  name, color and permissions.
</p>

<ul class="role-list">
  {#each roles as r, i (r.name)}
    <li
      class="role-item"
      class:dragging={dragFrom === i}
      class:drop-before={dragOver === i && dragFrom !== null && dragFrom > i}
      class:drop-after={dragOver === i && dragFrom !== null && dragFrom < i}
      draggable="true"
      ondragstart={(e) => onDragStart(e, i)}
      ondragover={(e) => onDragOver(e, i)}
      ondrop={(e) => onDrop(e, i)}
      ondragend={resetDrag}
    >
      <div class="role-row">
        <span class="role-grip" title="Drag to reorder" aria-hidden="true">⠿</span>
        <button
          class="role-open"
          class:on={selected === r.name}
          aria-expanded={selected === r.name}
          onclick={() => select(r)}
          onkeydown={(e) => onRowKey(e, r)}
          title="Edit {r.name} — Alt+↑/↓ to reorder"
        >
          <span class="role-swatch" style="background:{r.color}"></span>
          <span class="role-meta">
            <span class="role-title">{r.name}</span>
            <span class="role-caps">{r.caps.join(" · ") || "no permissions"}</span>
          </span>
          {#if r.hoist}<span class="role-flag" title="Shown separately in the member list">★</span>{/if}
        </button>
      </div>

      {#if selected === r.name}
        <div class="role-editor">
          <label class="field-label" for="role-name-{i}">Role name</label>
          <input
            id="role-name-{i}"
            class="text-input"
            bind:value={draft.name}
            placeholder="Role name"
            onkeydown={(e) => e.key === "Enter" && dirty && save()}
          />

          <div class="field-label">Color</div>
          <div class="color-row">
            {#each ROLE_COLORS as c (c)}
              <button
                class="color-dot"
                class:on={draft.color === c}
                style="background:{c}"
                aria-label="color {c}"
                onclick={() => (draft.color = c)}
              ></button>
            {/each}
          </div>

          <label class="hoist-row">
            <input type="checkbox" bind:checked={draft.hoist} />
            Display this role's members separately in the sidebar
          </label>

          <div class="field-label">Permissions</div>
          <CapChecklist caps={CAPS} selected={draft.caps} onToggle={toggleDraftCap} />

          <div class="editor-actions">
            <button class="mini-danger" onclick={() => remove(r.name)}>Delete role</button>
            <button class="linkish" onclick={() => (selected = null)}>Cancel</button>
            <button class="ok-btn" disabled={!dirty || !draft.name.trim() || !draft.caps.length} onclick={save}>
              Save changes
            </button>
          </div>
          {#if draft.name.trim() && draft.name.trim() !== r.name}
            <p class="rename-note">
              Renaming keeps every member and granted permission — the role is renamed in place.
            </p>
          {/if}
        </div>
      {/if}
    </li>
  {:else}
    <div class="empty-hint">No roles yet — create one below.</div>
  {/each}
</ul>

<div class="section-sep"></div>
<div class="field-label">Create a role</div>
<input class="text-input" bind:value={app.newRoleName} placeholder="Role name (e.g. Moderator)" />
<div class="color-row">
  {#each ROLE_COLORS as c (c)}
    <button
      class="color-dot"
      class:on={app.newRoleColor === c}
      style="background:{c}"
      aria-label="color {c}"
      onclick={() => (app.newRoleColor = c)}
    ></button>
  {/each}
</div>
<label class="hoist-row">
  <input type="checkbox" bind:checked={app.newRoleHoist} />
  Display this role's members separately in the sidebar
</label>
<div class="field-label">Permissions</div>
<CapChecklist caps={CAPS} selected={app.newRoleCaps} onToggle={app.toggleNewRoleCap} />
<div class="modal-actions">
  <button class="ok-btn" disabled={!app.newRoleName.trim() || !app.newRoleCaps.length} onclick={app.createRole}>
    Create role
  </button>
</div>

<style>
  .role-list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .role-item {
    border-radius: var(--radius-md);
    /* The insertion line lives on the item's edge so the gap doesn't jump. */
    border-top: 2px solid transparent;
    border-bottom: 2px solid transparent;
  }
  .role-item.dragging {
    opacity: 0.45;
  }
  .role-item.drop-before {
    border-top-color: var(--accent, #5865f2);
  }
  .role-item.drop-after {
    border-bottom-color: var(--accent, #5865f2);
  }
  .role-row {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .role-grip {
    padding: 0 2px;
    color: var(--text-faint);
    cursor: grab;
    user-select: none;
    font-size: 14px;
    line-height: 1;
  }
  .role-row:hover .role-grip {
    color: var(--text-muted);
  }
  .role-open {
    flex: 1;
    display: flex;
    align-items: center;
    gap: 10px;
    min-width: 0;
    padding: 8px 10px;
    border: 1px solid var(--border-hair-strong);
    border-radius: var(--radius-md);
    background: var(--bg-panel);
    color: inherit;
    font: inherit;
    text-align: left;
    cursor: pointer;
  }
  .role-open:hover {
    background: var(--bg-hover);
  }
  .role-open.on {
    border-color: var(--accent, #5865f2);
  }
  .role-swatch {
    width: 12px;
    height: 12px;
    flex: none;
    border-radius: 50%;
  }
  .role-meta {
    display: flex;
    flex-direction: column;
    gap: 1px;
    min-width: 0;
    flex: 1;
  }
  .role-title {
    font-size: 13px;
    font-weight: 600;
    color: var(--text-primary);
  }
  .role-caps {
    font-size: 11.5px;
    color: var(--text-muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .role-flag {
    flex: none;
    font-size: 12px;
    color: var(--accent, #5865f2);
  }
  .role-editor {
    margin: 6px 0 10px 22px;
    padding: 12px 14px;
    border: 1px solid var(--border-hair-strong);
    border-radius: var(--radius-md);
    background: var(--bg-panel-raised);
  }
  .editor-actions {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 14px;
  }
  /* Delete sits apart from the confirm pair. */
  .editor-actions .mini-danger {
    margin-right: auto;
  }
  .rename-note {
    margin: 8px 0 0;
    font-size: 11.5px;
    color: var(--text-muted);
  }
</style>
