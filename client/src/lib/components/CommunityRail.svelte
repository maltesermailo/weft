<script lang="ts">
  import { vm } from "$lib/navigation/viewmodel.svelte";
  import { openDiscover } from "$lib/navigation/navigation";
  import { serverCtx } from "$lib/ui/ctxmenu.svelte";
  import { toast } from "$lib/notifications/toasts.svelte";
  import { serverMuted } from "$lib/notifications/notif";
  import { initials } from "$lib/profile/profile.svelte";
  import { getApp } from "$lib/ui/context";
  import { store } from "$lib/store/store.svelte";
  const app = getApp();

  /// §9 liveness: a provider-managed namespace is only usable while its bridge
  /// is connected — weftd refuses joins and every write into it, and serves no
  /// history. `providerOnline` is null for a native namespace (nothing governs
  /// it, so it is never offline).
  const offline = (ns: string) => store.servers.get(ns)?.providerOnline === false;

  // Caps are server-resolved per scope and fetched on demand, so a namespace you
  // have never opened has none — and a context menu built from them would show
  // nothing. Warm every tile's scope here (deduped by `capsInflight`), so the
  // first right-click already knows what you may do there.
  $effect(() => {
    for (const ns of vm.serverNamespaces) {
      store.session.ensureCapsAt(store.session.account, `ns:${ns}`);
    }
  });
</script>

<nav class="warp-rail" aria-label="Networks">
  <button class="rail-home" class:active={app.homeView} title="Direct messages" aria-label="Direct messages" onclick={app.goHome}>
    <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><path d="M21 11.5a8.38 8.38 0 0 1-.9 3.8 8.5 8.5 0 0 1-7.6 4.7 8.38 8.38 0 0 1-3.8-.9L3 21l1.9-5.7a8.38 8.38 0 0 1-.9-3.8 8.5 8.5 0 0 1 4.7-7.6 8.38 8.38 0 0 1 3.8-.9h.5a8.48 8.48 0 0 1 8 8v.5z" /></svg>
  </button>
  <div class="rail-divider"></div>
  <div class="rail-communities">
    {#each vm.serverNamespaces as ns (ns)}
      <div
        class="comm-tile"
        class:active={!app.homeView && app.activeServer === ns}
        class:muted={serverMuted(ns)}
        class:offline={offline(ns)}
        title={offline(ns) ? `${vm.serverName(ns)} — bridge offline` : vm.serverName(ns)}
      >
        <!-- Not `disabled`: a disabled button fires no `contextmenu`, and the
             locked tile is exactly where the menu matters (it is how you leave).
             So the click is gated instead, and says why. -->
        <button
          onclick={() =>
            offline(ns)
              ? toast(`${vm.serverName(ns)} is unavailable — its bridge is disconnected`, "info")
              : app.selectServer(ns)}
          oncontextmenu={(e) => serverCtx(e, ns)}
          aria-disabled={offline(ns)}
          title={offline(ns) ? `${vm.serverName(ns)} — its bridge is disconnected, so nothing in it can load` : vm.serverName(ns)}
        >{initials(vm.serverName(ns))}</button>
        <!-- The offline mark outranks unread: a count you cannot open is noise,
             and the reason it will not open is the useful thing to show. -->
        {#if offline(ns)}<span class="tile-badge offline" aria-label="bridge offline">!</span>
        {:else if vm.serverMentionCount(ns)}<span class="tile-badge mention">{vm.serverMentionCount(ns)}</span>
        {:else if vm.serverUnread(ns) && !serverMuted(ns)}<span class="tile-badge"></span>{/if}
      </div>
    {/each}
  </div>
  <button class="rail-add" title="Discover namespaces" aria-label="Discover namespaces" onclick={openDiscover}>
    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M12 5v14M5 12h14" /></svg>
  </button>
</nav>
