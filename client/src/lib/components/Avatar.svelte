<script lang="ts">
  // §10.3 an account's avatar: the profile picture if set, else initials. Drop
  // it inside an existing `.avatar`-style wrapper (which keeps the status dot).
  import { getApp } from "$lib/context";
  const app = getApp();
  let { account }: { account: string } = $props();
  const url = $derived(app.avatarUrl(account));
</script>

{#if url}
  <img class="avatar-img" src={url} alt="" />
{:else}{app.initials(account)}{/if}
