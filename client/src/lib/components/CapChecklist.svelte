<script lang="ts">
  // A vertical, Discord-style permission list: one row per capability with a
  // name, a one-line description, and a checkbox. Reused wherever capabilities
  // are attached to a role (role creation, per-channel role permissions).
  import { CAP_META } from "$lib/constants";

  let {
    caps,
    selected,
    onToggle,
  }: {
    caps: string[];
    selected: string[];
    onToggle: (cap: string) => void;
  } = $props();
</script>

<ul class="cap-checklist">
  {#each caps as c (c)}
    <li>
      <label class="cap-row">
        <span class="cap-info">
          <span class="cap-name">{CAP_META[c]?.label ?? c}</span>
          {#if CAP_META[c]?.desc}<span class="cap-desc">{CAP_META[c].desc}</span>{/if}
        </span>
        <input type="checkbox" checked={selected.includes(c)} onchange={() => onToggle(c)} />
      </label>
    </li>
  {/each}
</ul>

<style>
  .cap-checklist {
    list-style: none;
    margin: 6px 0 0;
    padding: 0;
    border: 1px solid var(--border-hair-strong);
    border-radius: var(--radius-md);
    overflow: hidden;
    background: var(--bg-panel);
  }
  .cap-checklist > li + li {
    border-top: 1px solid color-mix(in srgb, var(--border-hair-strong) 55%, transparent);
  }
  .cap-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 14px;
    padding: 9px 12px;
    cursor: pointer;
  }
  .cap-row:hover {
    background: var(--bg-hover);
  }
  .cap-info {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }
  .cap-name {
    font-size: 13px;
    font-weight: 500;
    color: var(--text-primary);
  }
  .cap-desc {
    font-size: 11.5px;
    color: var(--text-muted);
  }
  .cap-row input[type="checkbox"] {
    flex-shrink: 0;
    width: 17px;
    height: 17px;
    accent-color: var(--accent, #5865f2);
    cursor: pointer;
  }
</style>
