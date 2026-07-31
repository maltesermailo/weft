<script lang="ts">
  import { vm } from "$lib/navigation/viewmodel.svelte";
  import { getApp } from "$lib/ui/context";
  import EmptyHome from "$lib/components/EmptyHome.svelte";
  import VoiceStage from "$lib/components/chat/VoiceStage.svelte";
  import ChatTopbar from "$lib/components/chat/ChatTopbar.svelte";
  import MessageList from "$lib/components/chat/MessageList.svelte";
  import Composer from "$lib/components/chat/Composer.svelte";

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
