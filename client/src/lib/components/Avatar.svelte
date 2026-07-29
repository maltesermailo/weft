<script lang="ts">
  // §10.3 an account's avatar: the profile picture if set, else initials. Drop
  // it inside an existing `.avatar`-style wrapper (which keeps the status dot).
  import { getApp } from "$lib/context";
  const app = getApp();
  let { account }: { account: string } = $props();
  const acc = $derived(app.accountOf(account));
</script>

{#if acc.avatarUrl}
  <img class="avatar-img" src={acc.avatarUrl} alt="" />
{:else}{acc.initials}{/if}
