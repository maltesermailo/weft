<!--
  The application modal / overlay stack. Every entry is driven by module-level
  state (`ui`, `store`, `confirmDialog`, the create drafts, the group picker), so
  this is a pure projection of that state — no props beyond the two ctx reads
  (`activeServer` / `channelGroups`) the Create-Channel modal needs.
-->
<script lang="ts">
  import { vm } from "$lib/navigation/viewmodel.svelte";
  import { getApp } from "$lib/ui/context";
  import { ui } from "$lib/ui/ui.svelte";
  import { store } from "$lib/store/store.svelte";
  import { confirmDialog, resolveConfirm } from "$lib/ui/confirm.svelte";
  import { chanDraft, catDraft, createChannel, createCategory } from "$lib/channels/channelcreate.svelte";
  

  import DiscoverModal from "$lib/components/modals/DiscoverModal.svelte";
  import ReportModal from "$lib/components/modals/ReportModal.svelte";
  import ReportsQueueModal from "$lib/components/modals/ReportsQueueModal.svelte";
  import InviteCreateModal from "$lib/components/modals/InviteCreateModal.svelte";
  import InvitesModal from "$lib/components/modals/InvitesModal.svelte";
  import NewGroupModal from "$lib/components/modals/NewGroupModal.svelte";
  import PinsModal from "$lib/components/modals/PinsModal.svelte";
  import ThreadsModal from "$lib/components/modals/ThreadsModal.svelte";
  import SearchModal from "$lib/components/modals/SearchModal.svelte";
  import CreateChannelModal from "$lib/components/modals/CreateChannelModal.svelte";
  import CreateCategoryModal from "$lib/components/modals/CreateCategoryModal.svelte";
  import ChannelSettings from "$lib/components/modals/ChannelSettings.svelte";
  import ProfileCard from "$lib/components/modals/ProfileCard.svelte";
  import NicknameModal from "$lib/components/modals/NicknameModal.svelte";
  import ConfirmModal from "$lib/components/modals/ConfirmModal.svelte";
  import PluginModal from "$lib/components/plugins/PluginModal.svelte";
  import { plugins } from "$lib/plugins/plugins.svelte";
  import ProfileModal from "$lib/components/modals/ProfileModal.svelte";
  import UserSettingsModal from "$lib/components/modals/UserSettingsModal.svelte";
  import FederationPanel from "$lib/components/modals/FederationPanel.svelte";
  import ServerSettingsModal from "$lib/components/modals/ServerSettingsModal.svelte";
  import ServerProfileModal from "$lib/components/modals/ServerProfileModal.svelte";
  import NotificationSettingsModal from "$lib/components/modals/NotificationSettingsModal.svelte";
  import CallOverlay from "$lib/components/CallOverlay.svelte";

  const app = getApp();
</script>

<!-- A plugin's modal view (plugin-spec.md §11.2). Drawn like any other dialog;
     what is inside it is whatever the plugin declared. -->
{#if plugins.activeModal}
  <PluginModal open={plugins.activeModal} />
{/if}

{#if ui.discoverOpen}
  <DiscoverModal onclose={() => (ui.discoverOpen = false)} />
{/if}

{#if store.reports.target}
  <ReportModal target={store.reports.target} onclose={() => (store.reports.target = null)} />
{/if}

{#if store.reports.open}
  <ReportsQueueModal onclose={() => (store.reports.open = false)} />
{/if}

{#if store.invites.createOpen}
  <InviteCreateModal onclose={() => { store.invites.createOpen = false; store.invites.link = null; store.invites.id = null; }} />
{/if}

{#if store.invites.listOpen}
  <InvitesModal onclose={() => (store.invites.listOpen = false)} />
{/if}

{#if store.social.groupPicker.open}
  <NewGroupModal
    seed={store.social.groupPicker.seed}
    pos={store.social.groupPicker.pos}
    onclose={() => (store.social.groupPicker.open = false)}
    oncreate={(m) => store.social.createGroupWith(m)}
  />
{/if}

{#if store.pins.open}
  <PinsModal onclose={() => (store.pins.open = false)} />
{/if}

{#if store.threads.listOpen}
  <ThreadsModal onclose={() => (store.threads.listOpen = false)} />
{/if}

{#if store.search.open}
  <SearchModal onclose={() => (store.search.open = false)} />
{/if}

{#if chanDraft.open}
  <CreateChannelModal
    bind:name={chanDraft.name}
    bind:category={chanDraft.category}
    bind:announce={chanDraft.announce}
    bind:retention={chanDraft.retention}
    bind:voice={chanDraft.voice}
    activeServer={app.activeServer}
    serverName={app.activeServer ? vm.serverName(app.activeServer) : ""}
    categories={vm.channelGroups.map((g) => g.category)}
    onclose={() => (chanDraft.open = false)}
    oncreate={createChannel}
  />
{/if}

{#if catDraft.open}
  <CreateCategoryModal bind:name={catDraft.name} onclose={() => (catDraft.open = false)} oncreate={createCategory} />
{/if}

{#if ui.chanPerms}
  <ChannelSettings channel={ui.chanPerms} onclose={() => (ui.chanPerms = null)} />
{/if}

{#if ui.profileTarget}
  <ProfileCard target={ui.profileTarget} pos={ui.profilePos} onclose={() => (ui.profileTarget = null)} />
{/if}

{#if ui.nickTarget}
  <NicknameModal target={ui.nickTarget} onclose={() => (ui.nickTarget = null)} />
{/if}

{#if confirmDialog.current}
  <ConfirmModal message={confirmDialog.current.message} confirmLabel={confirmDialog.current.label} onresult={resolveConfirm} />
{/if}

{#if ui.profileModalTarget}
  <ProfileModal target={ui.profileModalTarget} onclose={() => (ui.profileModalTarget = null)} />
{/if}

{#if ui.settingsOpen}
  <UserSettingsModal onclose={() => (ui.settingsOpen = false)} />
{/if}

{#if ui.federationOpen}
  <FederationPanel onclose={() => (ui.federationOpen = false)} />
{/if}

{#if ui.nsSettingsOpen}
  <ServerSettingsModal onclose={() => (ui.nsSettingsOpen = false)} />
{/if}

{#if ui.serverProfileOpen}
  <ServerProfileModal onclose={() => (ui.serverProfileOpen = false)} />
{/if}

{#if ui.notifSettingsOpen}
  <NotificationSettingsModal onclose={() => (ui.notifSettingsOpen = false)} />
{/if}

<CallOverlay />
