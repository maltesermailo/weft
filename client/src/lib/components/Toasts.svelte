<script lang="ts">
  let { toasts }: { toasts: { id: number; text: string; kind: string }[] } = $props();
</script>

<div class="toast-stack">
  {#each toasts as t (t.id)}
    <div class="toast {t.kind}">{t.text}</div>
  {/each}
</div>
