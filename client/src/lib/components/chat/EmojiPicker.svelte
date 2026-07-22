<script lang="ts">
  // §9.4 unified emoji picker (modeled on design/emoji.html): a search box, a
  // category rail, and a scrolling grid whose FIRST section is this server's
  // custom emoji, followed by the unicode categories — plus a hover-preview bar.
  // `onpick` receives a unicode char or a `:name:` shortcode.
  import { getApp } from "$lib/context";
  import { EMOJI } from "$lib/emoji";
  import { charName } from "$lib/shortcodes";
  const app = getApp();
  let { onpick }: { onpick: (value: string) => void } = $props();

  type Item = { value: string; char?: string; name?: string; url?: string | null };
  type Section = {
    id: string;
    label: string;
    icon: string;
    iconUrl?: string | null;
    items: Item[];
  };

  const sections = $derived.by<Section[]>(() => {
    const out: Section[] = [];
    // Per-server custom emoji first.
    if (app.activeEmoji.length) {
      const items: Item[] = app.activeEmoji.map((em) => ({
        value: `:${em.name}:`,
        name: em.name,
        url: app.emojiUrlFor(em.name),
      }));
      out.push({
        id: "server",
        label: app.activeServer || "Server",
        icon: (app.activeServer || "S").slice(0, 1).toUpperCase(),
        iconUrl: items[0]?.url ?? null,
        items,
      });
    }
    // Then the curated unicode set, one section per category.
    for (const [cat, list] of Object.entries(EMOJI)) {
      out.push({
        id: cat,
        label: cat,
        icon: list[0] ?? "🙂",
        items: list.map((e) => ({ value: e, char: e, name: charName(e) })),
      });
    }
    return out;
  });

  let query = $state("");
  // Search by shortcode name across every section (custom + unicode).
  const visible = $derived.by<Section[]>(() => {
    const q = query.toLowerCase().replace(/:/g, "").trim();
    if (!q) return sections;
    return sections
      .map((s) => ({ ...s, items: s.items.filter((it) => (it.name ?? "").toLowerCase().includes(q)) }))
      .filter((s) => s.items.length);
  });

  let preview = $state<Item | null>(null);
  let gridWrap = $state<HTMLDivElement | null>(null);
  function jump(id: string) {
    gridWrap
      ?.querySelector(`[data-section="${id}"]`)
      ?.scrollIntoView({ block: "start", behavior: "smooth" });
  }
</script>

<div class="emoji-panel">
  <div class="ep-search">
    <svg width="15" height="15" viewBox="0 0 24 24" fill="none">
      <circle cx="10.5" cy="10.5" r="6.5" stroke="currentColor" stroke-width="2" />
      <path d="M15.5 15.5 21 21" stroke="currentColor" stroke-width="2" stroke-linecap="round" />
    </svg>
    <input type="text" placeholder="Search emoji" bind:value={query} aria-label="Search emoji" />
  </div>

  <div class="ep-body">
    <div class="ep-rail" aria-label="Emoji categories">
      {#each sections as s (s.id)}
        <button class="ep-rail-item" class:server={s.id === "server"} title={s.label} onclick={() => jump(s.id)}>
          {#if s.iconUrl}<img src={s.iconUrl} alt="" />{:else}{s.icon}{/if}
        </button>
      {/each}
    </div>

    <div class="ep-grid-wrap" bind:this={gridWrap}>
      {#each visible as s (s.id)}
        <div class="ep-section" data-section={s.id}>
          <div class="ep-section-h">
            {#if s.iconUrl}<img class="ep-h-ico" src={s.iconUrl} alt="" />{:else}<span class="ep-h-ico">{s.icon}</span>{/if}
            {s.label}
          </div>
          <div class="ep-grid">
            {#each s.items as it (it.value)}
              <button
                class="ep-cell"
                title={it.name ? `:${it.name}:` : ""}
                onmouseenter={() => (preview = it)}
                onfocus={() => (preview = it)}
                onclick={() => onpick(it.value)}
              >
                {#if it.url}<img class="ep-custom" src={it.url} alt={it.value} />{:else}{it.char}{/if}
              </button>
            {/each}
          </div>
        </div>
      {:else}
        <div class="ep-empty">No emoji match “{query}”.</div>
      {/each}
    </div>
  </div>

  <div class="ep-preview">
    {#if preview}
      {#if preview.url}<img class="ep-pv-img" src={preview.url} alt="" />{:else}<span class="ep-pv-big">{preview.char}</span>{/if}
      <span class="ep-pv-name">{preview.name ? `:${preview.name}:` : "emoji"}</span>
    {:else}
      <span class="ep-pv-name dim">Pick an emoji</span>
    {/if}
  </div>
</div>

<style>
  .emoji-panel {
    width: 340px;
    max-width: 92vw;
    height: 400px;
    display: flex;
    flex-direction: column;
    background: var(--bg-panel);
    border: 1px solid var(--border-hair-strong);
    border-radius: 10px;
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.45);
    overflow: hidden;
  }

  .ep-search {
    display: flex;
    align-items: center;
    gap: 8px;
    margin: 10px;
    padding: 7px 10px;
    border-radius: 8px;
    background: var(--bg-void, var(--bg-panel-raised));
    border: 1px solid var(--accent, #5865f2);
    color: var(--text-muted);
  }
  .ep-search input {
    flex: 1;
    background: none;
    border: none;
    outline: none;
    color: var(--text-primary);
    font: inherit;
    font-size: 13px;
  }

  .ep-body {
    flex: 1;
    display: flex;
    min-height: 0;
  }

  .ep-rail {
    width: 44px;
    flex: none;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 8px;
    padding: 10px 0;
    background: var(--bg-panel-raised);
    overflow-y: auto;
    scrollbar-width: none;
  }
  .ep-rail::-webkit-scrollbar {
    display: none;
  }
  .ep-rail-item {
    width: 30px;
    height: 30px;
    flex: none;
    display: flex;
    align-items: center;
    justify-content: center;
    border: none;
    border-radius: 50%;
    background: var(--bg-hover);
    color: var(--text-primary);
    cursor: pointer;
    font-size: 16px;
    overflow: hidden;
    filter: grayscale(1) opacity(0.7);
  }
  .ep-rail-item:hover {
    filter: none;
    opacity: 1;
  }
  .ep-rail-item.server {
    font-size: 12px;
    font-weight: 700;
    filter: none;
    opacity: 1;
    background: color-mix(in srgb, var(--accent, #5865f2) 30%, transparent);
  }
  .ep-rail-item img {
    width: 20px;
    height: 20px;
    object-fit: contain;
  }

  .ep-grid-wrap {
    flex: 1;
    overflow-y: auto;
    padding: 4px 8px 8px;
  }
  .ep-section {
    padding-top: 6px;
  }
  .ep-section-h {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 6px 4px;
    font-size: 12px;
    font-weight: 700;
    color: var(--text-muted);
    position: sticky;
    top: 0;
    background: var(--bg-panel);
  }
  .ep-h-ico {
    width: 15px;
    height: 15px;
    font-size: 13px;
    object-fit: contain;
  }
  .ep-grid {
    display: grid;
    grid-template-columns: repeat(8, 1fr);
  }
  .ep-cell {
    aspect-ratio: 1;
    display: flex;
    align-items: center;
    justify-content: center;
    border: none;
    border-radius: 6px;
    background: none;
    cursor: pointer;
    font-size: 22px;
    line-height: 1;
  }
  .ep-cell:hover {
    background: var(--bg-hover);
  }
  .ep-custom {
    width: 26px;
    height: 26px;
    object-fit: contain;
  }
  .ep-empty {
    padding: 20px 8px;
    color: var(--text-muted);
    font-size: 13px;
  }

  .ep-preview {
    flex: none;
    height: 44px;
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 0 14px;
    background: var(--bg-panel-raised);
    border-top: 1px solid var(--border-hair);
  }
  .ep-pv-big {
    font-size: 24px;
    line-height: 1;
  }
  .ep-pv-img {
    width: 26px;
    height: 26px;
    object-fit: contain;
  }
  .ep-pv-name {
    font-size: 13px;
    font-weight: 600;
    color: var(--text-primary);
  }
  .ep-pv-name.dim {
    color: var(--text-muted);
    font-weight: 500;
  }
</style>
