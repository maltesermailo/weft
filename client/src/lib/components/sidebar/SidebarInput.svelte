<script lang="ts">
  let {
    value = $bindable(),
    placeholder,
    onenter,
  }: {
    value: string;
    placeholder: string;
    onenter: () => void;
  } = $props();
</script>

<div class="sidebar-join">
  <input bind:value {placeholder} onkeydown={(e) => e.key === "Enter" && onenter()} />
</div>
