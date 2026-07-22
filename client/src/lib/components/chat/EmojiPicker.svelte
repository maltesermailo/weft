<script lang="ts">
  // §9.4 unified emoji picker: the namespace's custom emoji (as images) plus
  // the curated unicode set. `onpick` receives a unicode char or a `:name:`
  // shortcode; the caller decides whether to react or insert into the composer.
  import { getApp } from "$lib/context";
  import { EMOJI, QUICK_EMOJI } from "$lib/emoji";
  const app = getApp();
  let { onpick }: { onpick: (value: string) => void } = $props();
</script>

<div class="emoji-picker">
  {#if app.activeEmoji.length}
    <div class="emoji-cat">Custom</div>
    <div class="emoji-row">
      {#each app.activeEmoji as em (em.name)}
        <button class="emoji-opt" title=":{em.name}:" onclick={() => onpick(`:${em.name}:`)}>
          <img class="custom-emoji" src={app.emojiUrlFor(em.name) ?? ''} alt=":{em.name}:" />
        </button>
      {/each}
    </div>
  {/if}
  <div class="emoji-quick">
    {#each QUICK_EMOJI as emoji (emoji)}
      <button class="emoji-opt" onclick={() => onpick(emoji)}>{emoji}</button>
    {/each}
  </div>
  <div class="emoji-grid">
    {#each Object.entries(EMOJI) as [cat, list] (cat)}
      <div class="emoji-cat">{cat}</div>
      <div class="emoji-row">
        {#each list as emoji (emoji)}
          <button class="emoji-opt" onclick={() => onpick(emoji)}>{emoji}</button>
        {/each}
      </div>
    {/each}
  </div>
</div>
