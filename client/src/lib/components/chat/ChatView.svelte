<script lang="ts">
  import { vm } from "$lib/viewmodel.svelte";
  import { getApp } from "$lib/context";
  import EmptyHome from "$lib/components/EmptyHome.svelte";
  import VoiceStage from "./VoiceStage.svelte";
  import ChatTopbar from "./ChatTopbar.svelte";
  import MessageList from "./MessageList.svelte";
  import Composer from "./Composer.svelte";

  const app = getApp();
  // Shared by the channel / DM / group routes. `active` is URL-derived, so the
  // channel record may not exist yet on a deep link (still syncing) — show a
  // neutral placeholder until it lands rather than feeding undefined downstream.
</script>

{#if vm.activeChannel?.voice}
  <VoiceStage />
{:else if vm.activeChannel}
  <ChatTopbar />

  <div class="msg-area">
    <!-- Keyed on the channel so switching remounts the virtualized list fresh
         (re-anchoring to the newest message); history + roster stay cached in
         the channel record, so the remount is cheap. -->
    {#key app.active}
      <MessageList channel={app.active} active />
    {/key}
  </div>
  <Composer />
{:else}
  <EmptyHome />
{/if}
