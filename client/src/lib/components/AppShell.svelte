<!--
  The online application shell: chrome + overlays around the routed <main>. Owns
  the quick switcher, the sidebar message/join inputs, and the §6.1 email nudge
  (all template-only concerns); everything else is read from the AppCtx + module
  singletons. The routed page is passed in as `children`; the federation banner's
  transient state stays in the layout (it drives a bridge watcher) and arrives as
  props.
-->
<script lang="ts">
  import { vm } from "$lib/viewmodel.svelte";
  import type { Snippet } from "svelte";
  import { goto } from "$app/navigation";
  import * as nav from "$lib/nav";
  import { getApp } from "$lib/context";
  import { ui } from "$lib/ui.svelte";
  import { store } from "$lib/models/store.svelte";
  import * as weft from "$lib/weft";
  import { cf, emailNudgeKey } from "$lib/models/connect.svelte";
  import { toasts } from "$lib/toasts.svelte";
  import { ctxMenu } from "$lib/ctxmenu.svelte";
  import { voiceUI } from "$lib/voiceui.svelte";
  import { channels, chanShort } from "$lib/models/channel.svelte";
  import { peerOf } from "$lib/profile.svelte";
  import { openDm } from "$lib/navigation";
  import { chanDraft, catDraft } from "$lib/channelcreate.svelte";

  import Toasts from "$lib/components/Toasts.svelte";
  import Lightbox from "$lib/components/chat/Lightbox.svelte";
  import LinkWarningModal from "$lib/components/modals/LinkWarningModal.svelte";
  import CameraPicker from "$lib/components/modals/CameraPicker.svelte";
  import ScreenPicker from "$lib/components/modals/ScreenPicker.svelte";
  import ScreenShareMenu from "$lib/components/modals/ScreenShareMenu.svelte";
  import ThreadPanel from "$lib/components/chat/ThreadPanel.svelte";
  import ContextMenu from "$lib/components/ContextMenu.svelte";
  import QuickSwitcher from "$lib/components/QuickSwitcher.svelte";
  import CommunityRail from "$lib/components/CommunityRail.svelte";
  import MemberList from "$lib/components/MemberList.svelte";
  import VoiceBar from "$lib/components/VoiceBar.svelte";
  import ChannelList from "$lib/components/sidebar/ChannelList.svelte";
  import SidebarHeader from "$lib/components/sidebar/SidebarHeader.svelte";
  import DmList from "$lib/components/sidebar/DmList.svelte";
  import UserFooter from "$lib/components/sidebar/UserFooter.svelte";
  import SidebarInput from "$lib/components/sidebar/SidebarInput.svelte";
  import AppModals from "$lib/components/AppModals.svelte";

  let {
    children,
    federating,
    oncancelfederating,
  }: { children: Snippet; federating: { target: string; ns: string } | null; oncancelfederating: () => void } = $props();

  const app = getApp();

  // ---- quick switcher (Ctrl/Cmd+K) ----
  let switcherOpen = $state(false);
  let switcherQuery = $state("");
  const switcherResults = $derived.by(() => {
    const q = switcherQuery.toLowerCase().replace(/^[#@]/, "");
    return Object.values(channels)
      .filter((c) => c.name.toLowerCase().includes(q))
      .sort((a, b) => a.name.localeCompare(b.name))
      .slice(0, 25);
  });
  function switchTo(name: string) {
    switcherOpen = false;
    goto(nav.pathFor(name));
  }
  function globalKey(e: KeyboardEvent) {
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k") {
      e.preventDefault();
      switcherOpen = true;
      switcherQuery = "";
    } else if (e.key === "Escape") {
      switcherOpen = false;
      store.pins.open = false;
      ui.discoverOpen = false;
      ui.settingsOpen = false;
      ui.nsSettingsOpen = false;
      ui.profileTarget = null;
      ctxMenu.close();
      ui.serverMenu = false;
      ui.userMenu = false;
      chanDraft.open = false;
      catDraft.open = false;
      ui.chanPerms = null;
    }
  }

  // ---- sidebar inputs ----
  let dmInput = $state("");
  let joinInput = $state("");
  function startDm() {
    const p = dmInput.trim().replace(/^@/, "");
    dmInput = "";
    if (p) openDm(p);
  }
  function joinNamespace(name: string) {
    weft.nsJoin(name).catch(() => {});
    weft.channels(name).catch(() => {}); // fetch its category layout
  }
  function doJoin() {
    const raw = joinInput.trim();
    if (!raw) return;
    joinInput = "";
    // `#chan` joins one channel; a bare name (or `ns:name`) joins the whole
    // namespace — the server auto-joins every channel we're allowed to see.
    if (raw.startsWith("#")) weft.join(raw).catch((e) => (cf.authError = String(e)));
    else joinNamespace(raw.replace(/^ns:/, ""));
  }

  // ---- §6.1 "no email on file" nudge ----
  // Gated on `verificationsLoaded` (VERIFY LIST streams with no terminator, so we
  // can't know an account has zero claims until the response has landed) and on
  // the homeserver actually offering email; dismissed once, persisted per account.
  const needsEmailWarning = $derived(
    store.session.verificationsLoaded &&
      ui.serverEmailAvailable &&
      !store.session.verifications.email &&
      !ui.emailBannerDismissed,
  );
  function openVerification() {
    ui.userTab = "verification";
    ui.settingsOpen = true;
    ui.userMenu = false;
  }
  function dismissEmailBanner() {
    ui.emailBannerDismissed = true;
    try {
      localStorage.setItem(emailNudgeKey(), "1");
    } catch {
      /* storage unavailable */
    }
  }
</script>

<svelte:window onkeydown={globalKey} />

{#if ui.reconnecting}
  <div class="reconnect-banner">Connection lost — reconnecting…</div>
{:else if needsEmailWarning}
  <div class="email-banner">
    <span>⚠ No email is on file for this account — you won't be able to reset your password.</span>
    <button class="email-banner-btn" onclick={openVerification}>Add email</button>
    <button class="email-banner-close" aria-label="Dismiss" title="Dismiss" onclick={dismissEmailBanner}>✕</button>
  </div>
{/if}

<Toasts {toasts} />
<Lightbox />
<LinkWarningModal />
{#if voiceUI.cameraPicker}<CameraPicker />{/if}
{#if voiceUI.screenPicker}<ScreenPicker />{/if}
{#if voiceUI.screenMenu}<ScreenShareMenu />{/if}
<ThreadPanel />
{#if federating}
  <div class="federating-banner">
    <span class="fed-spinner"></span>
    Connecting to <b>{federating.target}</b>…
    <button class="linkish" onclick={oncancelfederating}>dismiss</button>
  </div>
{/if}
<ContextMenu menu={ctxMenu.current} onclose={ctxMenu.close} />
{#if switcherOpen}
  <QuickSwitcher
    bind:query={switcherQuery}
    results={switcherResults.map((c) => ({
      name: c.name,
      label: c.name.startsWith("@") ? peerOf(c.name) : chanShort(c.name),
      sigil: c.name.startsWith("@") ? "@" : "#",
      unread: c.unread,
    }))}
    onselect={switchTo}
    onclose={() => (switcherOpen = false)}
  />
{/if}

<div
  class="app"
  class:members-collapsed={!ui.membersVisible || vm.activeChannel?.voice}
  class:with-top-banner={needsEmailWarning && !ui.reconnecting}
>
  <CommunityRail />

  <aside class="sidebar">
    <SidebarHeader />
    {#if app.homeView}
      <DmList />
      <SidebarInput bind:value={dmInput} placeholder="message @user…" onenter={startDm} />
    {:else}
      {#key app.activeServer}
        <ChannelList />
      {/key}
      <SidebarInput bind:value={joinInput} placeholder="join #channel or namespace…" onenter={doJoin} />
    {/if}
    <VoiceBar />
    <UserFooter />
  </aside>

  <main class="main">
    {@render children()}
  </main>

  <aside class="members">
    {#if vm.activeChannel && !vm.activeIsDm && !vm.activeChannel.voice}
      <MemberList />
    {/if}
  </aside>

  <AppModals />
</div>
