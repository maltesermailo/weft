<script lang="ts">
  import { autofocus } from "$lib/ui/actions";

  type Result = { name: string; label: string; sigil: string; unread: boolean };
  type Command = { plugin: string; id: string; label: string };

  let {
    query = $bindable(),
    results,
    commands = [],
    onselect,
    oncommand,
    onclose,
  }: {
    query: string;
    results: Result[];
    /// §13.1 `global`-surface plugin actions — app-wide commands, so the palette
    /// is where they belong.
    commands?: Command[];
    onselect: (name: string) => void;
    oncommand?: (plugin: string, action: string) => void;
    onclose: () => void;
  } = $props();

  // Commands are filtered here rather than by the parent: the palette owns what
  // "matches the query" means, and channels are already filtered the same way.
  const matching = $derived(
    commands.filter((c) => c.label.toLowerCase().includes(query.trim().toLowerCase())),
  );
</script>

<div class="modal-wrap switcher-wrap">
  <button class="modal-backdrop" aria-label="Close" onclick={onclose}></button>
  <div class="modal switcher" role="dialog" aria-modal="true">
    <input
      class="switcher-input"
      bind:value={query}
      placeholder="Jump to a channel, DM, or run a command…"
      use:autofocus
      onkeydown={(e) => {
        if (e.key !== "Enter") return;
        // A channel wins the bare Enter — jumping is the common case, and a
        // command firing when someone meant to navigate is the worse mistake.
        if (results[0]) onselect(results[0].name);
        else if (matching[0]) oncommand?.(matching[0].plugin, matching[0].id);
      }}
    />
    <div class="switcher-list">
      {#each results as c (c.name)}
        <button class="switcher-item" onclick={() => onselect(c.name)}>
          <span class="si-sigil">{c.sigil}</span>
          <span>{c.label}</span>
          {#if c.unread}<span class="unread-dot"></span>{/if}
        </button>
      {/each}
      {#if matching.length}
        <div class="switcher-heading">Commands</div>
        {#each matching as c (c.plugin + c.id)}
          <button class="switcher-item" onclick={() => oncommand?.(c.plugin, c.id)}>
            <span class="si-sigil">/</span>
            <span>{c.label}</span>
          </button>
        {/each}
      {/if}
      {#if results.length === 0 && matching.length === 0}
        <div class="empty-hint">No matches.</div>
      {/if}
    </div>
  </div>
</div>

<style>
  .switcher-heading {
    font-size: 0.72rem;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    opacity: 0.55;
    padding: 0.5rem 0.65rem 0.25rem;
  }
</style>
