<script lang="ts">
  import { vm } from "$lib/navigation/viewmodel.svelte";
  import { nsMemberCtx } from "$lib/ui/ctxmenu.svelte";
  
  import { nsAdmin, activeEmoji, emojiUrlFor } from "$lib/namespaces/server.svelte";
  import { denyList, refreshBans, liftMod } from "$lib/moderation/moderation";
  
  
import { roleStore } from "$lib/roles/roles.svelte";
  import { store } from "$lib/store/store.svelte";
  import { initials, profileStore } from "$lib/profile/profile.svelte";
  import { fade } from "svelte/transition";
  import { getApp } from "$lib/ui/context";
  import * as weft from "$lib/transport/weft";
  import * as media from "$lib/media/media";
  import type { NsTab } from "$lib/ui/ui.svelte";
  import RolesTab from "$lib/components/modals/RolesTab.svelte";
  import PluginBlock from "$lib/components/plugins/PluginBlock.svelte";
  import { plugins } from "$lib/plugins/plugins.svelte";
  import InviteList from "$lib/components/InviteList.svelte";
  import Avatar from "$lib/components/Avatar.svelte";
  const app = getApp();

  // ---- Per-capability tab visibility (§6.5) ----
  // A moderator sees only the tabs they can act on; owner / ns-admin sees all.
  // Each maps to the concrete WEFT capability that governs that surface.
  const isAdmin = $derived(store.session.isNsOwner(store.session.account) || store.session.serverCap("ns-admin"));
  // The active namespace's display name (v0.13) — `activeServer` is its id, used
  // for scopes/commands; anywhere a *name* is shown to the user, use this.
  const serverVanity = $derived(vm.activeNsMeta?.name || app.activeServer);
  // §7a.3 the namespace's capability profile. A provider-managed namespace says
  // which native surfaces to hide — a Matrix-bridged one hides `roles`, because
  // its authority is power levels and it supplies its own screen for them.
  //
  // Display gating only: the server already refuses these verbs on a replica, so
  // hiding them stops us offering buttons that would be rejected. It is a hint
  // that can be *stricter* than the server, never looser.
  // §13.1 actions a plugin declared for the settings surface, scoped to what
  // makes sense here: a namespace-context action, or one with no context.
  // The scheme of the active namespace's origin (`matrix://…` → `matrix`), or
  // null when it is native.
  const nsScheme = $derived(
    store.servers.get(app.activeServer)?.origin?.match(/^([a-z][a-z0-9+.-]*):\/\//)?.[1] ?? null,
  );
  // Schemes some *connected* provider serves — the realms this namespace could be
  // projected into. Empty when no adapter is up, which is why the section hides.
  const realmSchemes = $derived([
    ...new Set([...plugins.catalog.values()].flatMap((p) => p.schemes ?? [])),
  ].sort());
  const projectedInto = (scheme: string) =>
    (store.servers.get(app.activeServer)?.bridges ?? []).includes(scheme);

  const pluginPages = $derived(
    plugins.actionsFor("settings").filter(({ plugin, action }) => {
      if (action.context !== "namespace" && action.context !== "none") return false;
      // A realm adapter's settings page belongs on *its* realm's replicas only.
      // Filtering by surface + context alone put Matrix's Power Levels page on
      // every namespace, native ones included — the catalog said "a plugin offers
      // a namespace page" and nothing said which namespaces it speaks for.
      const schemes = plugins.schemesOf(plugin);
      if (schemes.length === 0) return true; // governs no realm ⇒ generic
      return nsScheme !== null && schemes.includes(nsScheme);
    }),
  );
  /// Tab key for a plugin page. Namespaced so it can never collide with a native
  /// tab name — a plugin should not be able to shadow "roles" by picking that id.
  const pluginTab = (plugin: string, action: string): NsTab => `plugin:${plugin}:${action}`;
  /// The panel backing the selected plugin page, once its flow has answered.
  /// A settings page is a panel (§11.3), so that is what we look for; a plugin
  /// that answers with a modal instead gets drawn as one, by `AppModals`.
  const pluginView = $derived(
    app.nsTab.startsWith("plugin:") ? [...plugins.views.values()].find((v) => v.isPanel) : undefined,
  );

  /// Open a plugin page: close whatever panel was showing (so its plugin stops
  /// being told to push into a screen nobody is looking at), then invoke.
  function openPluginPage(plugin: string, action: string) {
    for (const v of plugins.views.values()) {
      if (v.isPanel) plugins.close(v.id);
    }

    app.nsTab = pluginTab(plugin, action);
    plugins.invoke(plugin, action, app.activeServer);
  }

  const profile = $derived(store.servers.get(app.activeServer));
  const hidden = $derived(new Set(profile?.settingsDisabled ?? []));
  const tabPerm = $derived({
    overview: isAdmin,
    roles: isAdmin || store.session.serverCanGrant(),
    members: isAdmin || store.session.serverCanGrant() || ["ban", "mute", "kick", "reports"].some((c) => store.session.serverCap(c)),
    emoji: isAdmin,
    invites: isAdmin || store.session.serverCap("invite"),
    federation: isAdmin,
    bans: isAdmin || store.session.serverCap("ban") || store.session.serverCap("mute"),
    recovery: store.session.isNsOwner(store.session.account),
    danger: store.session.isNsOwner(store.session.account),
  } as Record<string, boolean>);
  const visibleTabs = $derived(
    (["overview", "roles", "members", "emoji", "invites", "federation", "bans", "recovery", "danger"] as const).filter(
      (t) => tabPerm[t] && !hidden.has(t),
    ),
  );
  // Keep the active tab on something the user can actually see — a mod opening on
  // the default (admin-only) tab lands on their first real one instead.
  $effect(() => {
    // A plugin page is legitimately outside `visibleTabs` — it is not a native
    // tab — so only native selections are corrected.
    if (app.nsTab.startsWith("plugin:")) return;
    if (visibleTabs.length && !visibleTabs.includes(app.nsTab as never)) app.nsTab = visibleTabs[0];
  });

  // ---- Members directory (NS INFO MEMBERS) ----
  let memberSearch = $state("");
  const roster = $derived(vm.nsMembers(app.activeServer));
  const shownMembers = $derived(
    roster.filter(
      (m) =>
        profileStore.displayName(m.account.name).toLowerCase().includes(memberSearch.toLowerCase()) ||
        m.account.name.toLowerCase().includes(memberSearch.toLowerCase()),
    ),
  );
  // A member's role pill is keyed by role **id** (v0.13); resolve its color +
  // display name through the scope's definitions.
  function roleColor(id: string): string {
    return roleStore.roleById(`ns:${app.activeServer}`, id)?.color ?? "#99aab5";
  }
  function roleName(id: string): string {
    return roleStore.roleById(`ns:${app.activeServer}`, id)?.name ?? id;
  }
  // Join date: "0" means the server had no recorded join time (pre-v0.12 backfill).
  function fmtJoined(ms: number): string {
    if (!ms) return "—";
    return new Date(ms).toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
  }
  // All namespace-scoped roles, for the in-line "add role" picker.
  const nsRoles = $derived(roleStore.rolesAt(roleStore.nsRoleScope()));
  // The namespace owner (implicit all-caps holder), surfaced with a crown.
  const ownerAccount = $derived(vm.activeNsMeta?.owner ?? null);
  // Which member's add-role popover is open (keyed by `account@network`).
  let addRoleFor = $state<string | null>(null);
  // Roles a member doesn't already hold — the options for their add-role menu.
  function unheldRoles(held: string[]) {
    return nsRoles.filter((r) => !held.includes(r.id));
  }

  // Options for the segmented (button-group) inputs — the fancy replacements
  // for the plain <select> boxes.
  const VIS_OPTIONS: { value: string; label: string; desc: string }[] = [
    { value: "public", label: "Public", desc: "Listed in Discover" },
    { value: "unlisted", label: "Unlisted", desc: "Invite only" },
    { value: "private", label: "Private", desc: "Hidden" },
  ];
  const BR_HISTORY: { value: string; label: string }[] = [
    { value: "from-epoch", label: "From epoch" },
    { value: "full", label: "Full history" },
  ];
  const BR_MEDIA: { value: string; label: string }[] = [
    { value: "none", label: "No media" },
    { value: "mirror", label: "Mirror" },
  ];
  let { onclose }: { onclose: () => void } = $props();

  // Federation: bridges are proposed at this namespace's scope (§11) — the
  // namespace owner/admin decides, not the network operator.
  let brPeer = $state("");
  let brHistory = $state("from-epoch");
  let brMedia = $state("none");
  let brTyping = $state(true);
  function proposeBridge() {
    const p = brPeer.trim();
    if (!p) return;
    store.federation.bridgePropose(`ns:${app.activeServer}`, p, brHistory, brMedia, brTyping);
    brPeer = "";
  }

  // §9.4 custom emoji upload.
  let emojiName = $state("");
  let pendingEmoji = $state(""); // media ref of an uploaded (not-yet-named) image
  function pickEmojiImage() {
    const input = document.createElement("input");
    input.type = "file";
    input.accept = "image/*";
    input.onchange = async () => {
      const file = input.files?.[0];
      if (!file) return;
      try {
        const up = await media.upload(file);
        pendingEmoji = up.media;
      } catch (e) {
        app.toast(String(e), "error");
      }
    };
    input.click();
  }
  function submitEmoji() {
    const name = emojiName.trim().replace(/[^a-zA-Z0-9_]/g, "");
    if (!name || !pendingEmoji) return;
    nsAdmin.addEmoji(name, pendingEmoji);
    emojiName = "";
    pendingEmoji = "";
  }
  function cancelEmoji() {
    emojiName = "";
    pendingEmoji = "";
  }

  // Live counts for the Overview stat strip (real data — no placeholders).
  const nsChannelCount = $derived(vm.channelGroups.reduce((n, g) => n + g.list.length, 0));
  const nsRoleCount = $derived(roleStore.rolesAt(roleStore.nsRoleScope()).length);
  // §6.2 welcome-channel picker: this namespace's text channels + the current
  // setting (from the ns-meta the server last pushed).
  const nsTextChannels = $derived(vm.channelGroups.flatMap((g) => g.list).filter((c) => !c.voice));
  const currentWelcome = $derived(vm.activeNsMeta?.welcome ?? "");

  // Custom-emoji capacity gauge.
  const EMOJI_SLOTS = 50;
  let emojiSearch = $state("");
  const shownEmoji = $derived(
    activeEmoji().filter((e) => e.name.toLowerCase().includes(emojiSearch.toLowerCase())),
  );
</script>

<svelte:window onclick={() => (addRoleFor = null)} />

<div class="settings-overlay" role="dialog" aria-modal="true" transition:fade|global={{ duration: 150 }}>
  <nav class="so-nav">
    <div class="so-nav-inner">
      <div class="so-server-head">
        <span class="so-server-avatar">{initials(vm.activeNsMeta?.name || app.activeServer)}</span>
        <div class="so-server-meta">
          <div class="so-server-name">{vm.activeNsMeta?.name || app.activeServer}</div>
          <div class="so-server-sub">Server Settings</div>
        </div>
      </div>
      {#if ["overview", "roles", "members", "emoji"].some((t) => visibleTabs.includes(t as never))}
        <div class="so-heading">Server Settings</div>
      {/if}
      {#if visibleTabs.includes("overview")}
        <button class="so-navitem" class:active={app.nsTab === "overview"} onclick={() => (app.nsTab = "overview")}>Overview</button>
      {/if}
      {#if visibleTabs.includes("roles")}
        <button class="so-navitem" class:active={app.nsTab === "roles"} onclick={() => (app.nsTab = "roles")}>Roles</button>
      {/if}
      {#if visibleTabs.includes("members")}
        <button class="so-navitem" class:active={app.nsTab === "members"} onclick={() => { app.nsTab = "members"; vm.fetchNsMembers(app.activeServer); refreshBans(); }}>Members</button>
      {/if}
      {#if visibleTabs.includes("emoji")}
        <button class="so-navitem" class:active={app.nsTab === "emoji"} onclick={() => (app.nsTab = "emoji")}>Emoji</button>
      {/if}
      {#if visibleTabs.includes("invites") || visibleTabs.includes("federation")}
        <div class="so-heading">Community</div>
      {/if}
      {#if visibleTabs.includes("invites")}
        <button class="so-navitem" class:active={app.nsTab === "invites"} onclick={() => { app.nsTab = "invites"; store.invites.loadNsInvites(); }}>Invites</button>
      {/if}
      {#if visibleTabs.includes("federation")}
        <button class="so-navitem" class:active={app.nsTab === "federation"} onclick={() => (app.nsTab = "federation")}>Federation</button>
      {/if}
      {#if visibleTabs.includes("bans")}
        <div class="so-heading">Moderation</div>
        <button class="so-navitem" class:active={app.nsTab === "bans"} onclick={() => { app.nsTab = "bans"; refreshBans(); }}>Bans &amp; mutes</button>
      {/if}
      <!-- §13.1 plugin-supplied settings pages. A Matrix-bridged namespace hides
           the native Roles tab (above) and puts Power Levels here instead. -->
      {#if pluginPages.length}
        <div class="so-heading">Plugins</div>
        {#each pluginPages as { plugin, action } (plugin + action.id)}
          <button
            class="so-navitem"
            class:active={app.nsTab === pluginTab(plugin, action.id)}
            onclick={() => openPluginPage(plugin, action.id)}
          >
            {action.label}
          </button>
        {/each}
      {/if}
      {#if visibleTabs.includes("recovery") || visibleTabs.includes("danger")}
        <div class="so-heading">Security</div>
      {/if}
      {#if visibleTabs.includes("recovery")}
        <button class="so-navitem" class:active={app.nsTab === "recovery"} onclick={() => (app.nsTab = "recovery")}>Recovery</button>
      {/if}
      {#if visibleTabs.includes("danger")}
        <button class="so-navitem danger" class:active={app.nsTab === "danger"} onclick={() => (app.nsTab = "danger")}>Danger zone</button>
      {/if}
    </div>
  </nav>
  <main class="so-main">
    <div class="so-content" class:wide={app.nsTab === "roles"}>
      {#if vm.activeNsMeta?.recovery_eta}
        <div class="ns-card recovery-pending">
          <div class="ns-info">
            <div class="ns-name">⚠ Recovery pending (rung {vm.activeNsMeta.recovery_rung})</div>
            <div class="ns-desc">A root rotation is scheduled. As the live owner you can veto it.</div>
          </div>
          <button class="danger-btn" onclick={() => weft.nsRecoveryCancel(store.session.network, app.activeServer).catch((e) => app.toast(String(e), "error"))}>Cancel recovery</button>
        </div>
      {/if}

      {#if app.nsTab === "overview"}
        <h1>Overview</h1>
        <p class="so-sub">How this namespace appears in invites and, if listed, in Discover.</p>

        <div class="ov-card">
          <div class="ov-identity">
            <span class="ov-avatar">{initials(nsAdmin.title.trim() || vm.activeNsMeta?.name || app.activeServer)}</span>
            <div class="ov-identity-meta">
              <div class="ov-identity-name">{nsAdmin.title.trim() || vm.activeNsMeta?.name || app.activeServer}</div>
              <div class="ov-identity-sub">Namespace on {store.session.network}</div>
            </div>
          </div>
          <div class="field-label">Display name</div>
          <input class="text-input" bind:value={nsAdmin.title} placeholder={serverVanity} />
          <div class="ov-gap"></div>
          <div class="field-label">Visibility</div>
          <div class="segmented" role="radiogroup" aria-label="Visibility">
            {#each VIS_OPTIONS as o (o.value)}
              <button
                type="button"
                class="seg"
                class:on={nsAdmin.vis === o.value}
                role="radio"
                aria-checked={nsAdmin.vis === o.value}
                onclick={() => (nsAdmin.vis = o.value)}
              >
                <span class="seg-label">{o.label}</span>
                <span class="seg-desc">{o.desc}</span>
              </button>
            {/each}
          </div>
          <div class="ov-gap"></div>
          <div class="field-label">Description</div>
          <textarea class="text-input ov-desc" rows="3" bind:value={nsAdmin.desc} placeholder="what's this namespace about"></textarea>
          <div class="ov-gap"></div>
          <div class="field-label">Welcome channel</div>
          <p class="so-sub" style="margin:0 0 8px">Post a greeting here whenever someone new joins the server.</p>
          <select class="text-input" value={currentWelcome} onchange={(e) => nsAdmin.nsSetWelcome(e.currentTarget.value)}>
            <option value="">No welcome message</option>
            {#each nsTextChannels as c (c.name)}
              <option value={c.name}>#{app.chanShort(c.name)}</option>
            {/each}
          </select>
        </div>

        <div class="ov-stats">
          <div class="ov-stat">
            <div class="ov-stat-num" style="color:var(--accent)">{nsChannelCount}</div>
            <div class="ov-stat-label">Channels</div>
          </div>
          <div class="ov-stat">
            <div class="ov-stat-num" style="color:var(--signal-teal)">{nsRoleCount}</div>
            <div class="ov-stat-label">Roles</div>
          </div>
          <div class="ov-stat">
            <div class="ov-stat-num" style="color:var(--thread-amber)">{activeEmoji().length}</div>
            <div class="ov-stat-label">Emoji</div>
          </div>
          <div class="ov-stat">
            <div class="ov-stat-num" style="text-transform:capitalize">{nsAdmin.vis}</div>
            <div class="ov-stat-label">Visibility</div>
          </div>
        </div>

        <div class="modal-actions"><button class="ok-btn" onclick={() => nsAdmin.saveNsMeta()}>Save changes</button></div>
      {:else if app.nsTab === "invites"}
        <h1>Invites</h1>
        <p class="so-sub">Every active invite for <b>{serverVanity}</b> — who created it, how many times it's been used, its remaining uses and expiry. Revoke one, or close them all at once.</p>
        <div class="modal-actions">
          <button class="ok-btn" onclick={() => store.invites.createInvite()}>Create invite</button>
          <button class="danger-btn" onclick={app.revokeAllInvites}>Revoke all</button>
        </div>
        <div class="section-sep"></div>
        <InviteList showCreate={false} />
      {:else if app.nsTab === "roles"}
        <RolesTab />
      {:else if app.nsTab === "members"}
        <h1>Members</h1>
        <p class="so-sub">Everyone in <b>{vm.activeNsMeta?.name || app.activeServer}</b>, when they joined, and the roles they hold. Click a role's <b>✕</b> to remove it, <b>+</b> to add one — roles are the only way to grant capabilities. Right-click a member for moderation.</p>

        <div class="mem-search">
          <span aria-hidden="true">⌕</span>
          <input bind:value={memberSearch} placeholder="Search members" />
          <button class="mem-refresh" title="Refresh" aria-label="Refresh roster" onclick={() => { vm.fetchNsMembers(app.activeServer); refreshBans(); }}>↻</button>
        </div>
        <div class="mem-count">{shownMembers.length} {shownMembers.length === 1 ? "member" : "members"}</div>

        <div class="mem-table">
          <div class="mem-thead">
            <span>Member</span>
            <span>Roles</span>
            <span>Joined</span>
          </div>
          {#each shownMembers as m (m.account.name + "@" + m.network)}
            {@const acct = m.account.name}
            {@const handle = acct + "@" + m.network}
            {@const isOwner = ownerAccount === acct}
            <!-- svelte-ignore a11y_no_static_element_interactions -->
            <div class="mem-row" oncontextmenu={(e) => nsMemberCtx(e, acct)}>
              <div class="mem-id">
                <span class="mem-avatar"><Avatar account={acct} /></span>
                <div class="mem-id-meta">
                  <div class="mem-name">
                    {profileStore.displayName(acct)}
                    {#if isOwner}<span class="mem-owner" title="Server owner">👑 Owner</span>{/if}
                  </div>
                  <div class="mem-handle">{acct}{m.network !== store.session.network ? `@${m.network}` : ""}</div>
                </div>
              </div>
              <div class="mem-roles">
                {#each m.roleIds as r (r)}
                  <span class="role-pill editable" style="--role:{roleColor(r)}">
                    <span class="role-dot"></span>{roleName(r)}
                    <button class="role-x" title="Remove role" aria-label={`Remove ${roleName(r)}`} onclick={() => roleStore.unassignNsRole(acct, r)}>✕</button>
                  </span>
                {/each}
                <!-- svelte-ignore a11y_no_static_element_interactions -->
                <!-- svelte-ignore a11y_click_events_have_key_events -->
                <div class="role-add-wrap" onclick={(e) => e.stopPropagation()}>
                  <button
                    class="role-add"
                    title="Add role"
                    aria-label={`Add a role to ${profileStore.displayName(acct)}`}
                    onclick={() => (addRoleFor = addRoleFor === handle ? null : handle)}
                  >+</button>
                  {#if addRoleFor === handle}
                    <!-- svelte-ignore a11y_no_static_element_interactions -->
                    <div class="role-add-menu" role="menu">
                      {#each unheldRoles(m.roleIds) as r (r.name)}
                        <button class="role-add-opt" onclick={() => { roleStore.assignNsRole(acct, r.id); addRoleFor = null; }}>
                          <span class="role-dot" style="--role:{r.color}"></span>{r.name}
                        </button>
                      {:else}
                        <div class="role-add-empty">{nsRoles.length ? "All roles assigned" : "No roles defined yet"}</div>
                      {/each}
                    </div>
                  {/if}
                </div>
              </div>
              <div class="mem-joined">{fmtJoined(m.joinedMs)}</div>
            </div>
          {:else}
            <div class="mem-empty">
              {#if vm.nsMembersLoading}
                Loading roster…
              {:else if memberSearch}
                No members match your search.
              {:else}
                No members to show. This roster needs a moderation capability (ns-admin / ban / kick / mute / reports).
              {/if}
            </div>
          {/each}
        </div>
      {:else if app.nsTab === "emoji"}
        <div class="em-head">
          <div>
            <h1>Emoji</h1>
            <p class="so-sub" style="margin-bottom:0">Upload images members type as <code>:name:</code>. Emoji are per-namespace; adding needs <code>ns-admin</code>.</p>
          </div>
          <button class="ok-btn em-upload" onclick={pickEmojiImage}>↑ Upload emoji</button>
        </div>

        {#if pendingEmoji}
          <div class="em-add">
            <button class="em-add-thumb" title="Change image" onclick={pickEmojiImage}><img class="custom-emoji" src={app.mediaUrl(pendingEmoji)} alt="preview" /></button>
            <div class="em-add-name">
              <span class="emoji-colon">:</span>
              <input class="text-input" bind:value={emojiName} placeholder="name" onkeydown={(e) => e.key === "Enter" && submitEmoji()} />
              <span class="emoji-colon">:</span>
            </div>
            <button class="ok-btn" disabled={!emojiName.trim()} onclick={submitEmoji}>Add</button>
            <button class="linkish" onclick={cancelEmoji}>Cancel</button>
          </div>
        {/if}

        <div class="em-gauge">
          <div class="em-gauge-top">
            <span>Emoji slots</span>
            <span class="em-gauge-count">{activeEmoji().length} / {EMOJI_SLOTS}</span>
          </div>
          <div class="em-gauge-bar">
            <div
              class="em-gauge-fill"
              class:full={activeEmoji().length >= EMOJI_SLOTS * 0.8}
              style="width:{Math.min(100, (activeEmoji().length / EMOJI_SLOTS) * 100)}%"
            ></div>
          </div>
          <div class="em-gauge-sub">{Math.max(0, EMOJI_SLOTS - activeEmoji().length)} slots remaining</div>
        </div>

        <div class="em-search">
          <span aria-hidden="true">⌕</span>
          <input bind:value={emojiSearch} placeholder="Search emoji" />
        </div>

        <div class="em-grid">
          {#each shownEmoji as em (em.name)}
            <div class="em-tile">
              <img class="em-tile-img" src={emojiUrlFor(em.name) ?? ''} alt=":{em.name}:" />
              <code class="em-tile-name">:{em.name}:</code>
              <button class="em-tile-x" aria-label="Remove :{em.name}:" title="Remove" onclick={() => nsAdmin.removeEmoji(em.name)}>🗑</button>
            </div>
          {:else}
            <div class="em-empty">
              <div class="em-empty-icon">😊</div>
              <p>{activeEmoji().length ? "No emoji match your search." : "No custom emoji yet — upload one above."}</p>
            </div>
          {/each}
        </div>
      {:else if app.nsTab.startsWith("plugin:")}
        {#if pluginView}
          {#each pluginView.view.blocks ?? [] as block, i (i)}
            <PluginBlock
              {block}
              bind:values={pluginView.values}
              disabled={pluginView.busy}
              onpress={(b) => plugins.press(pluginView, b.id)}
              onsubmit={() => plugins.submit(pluginView)}
            />
          {/each}
        {:else}
          <p class="so-empty">Loading…</p>
        {/if}
      {:else if app.nsTab === "bans"}
        <h1>Bans &amp; mutes</h1>
        <p class="so-sub">Accounts denied at <code>ns:{app.activeServer}</code>. A <b>ban</b> blocks join + posting; a <b>mute</b> blocks posting. Lifting one takes effect immediately.</p>
        <div class="modal-list">
          {#each denyList() as d (d.kind + d.account)}
            <div class="ns-card">
              <div class="ns-info">
                <div class="ns-name">{d.account} <span class="rep-state {d.kind === "ban" ? "severed" : "added"}">{d.kind}</span></div>
                <div class="ns-desc">{d.reason ? d.reason : "no reason given"}{d.by ? ` · by ${d.by}` : ""}</div>
              </div>
              <div class="fed-actions">
                <button class="mini-danger" onclick={() => liftMod(d.kind, d.account)}>{d.kind === "ban" ? "Unban" : "Unmute"}</button>
              </div>
            </div>
          {:else}
            <div class="empty-hint">No bans or mutes at this server.</div>
          {/each}
        </div>
        <div class="modal-actions"><button class="set-btn" onclick={refreshBans}>Refresh</button></div>
      {:else if app.nsTab === "federation"}
        <h1>Federation</h1>
        <p class="so-sub">Bridge <b>{serverVanity}</b>'s channels to a peer network. You control this as the namespace owner — bridges are scoped to <code>ns:{app.activeServer}</code>, non-transitive, and every change notifies members.</p>

        <div class="field-label">Auto-federation</div>
        <p class="so-sub">When open, another network can reach this namespace on demand — a user there references <code>{store.session.network}/{serverVanity}</code> and their server auto-establishes the bridge. Off by default; enabling it is an explicit opt-in.</p>
        <label class="fed-check" style="margin-bottom:14px">
          <input
            type="checkbox"
            checked={vm.activeNsMeta?.federation ?? false}
            onchange={(e) => nsAdmin.nsSetFederation(e.currentTarget.checked)}
          />
          Open <b>{serverVanity}</b> to auto-federation
        </label>
        {#if (vm.activeNsMeta?.visibility ?? "") === "public"}
          <p class="so-sub">Public — reachable by <b>anyone</b> once open.</p>
        {:else}
          <p class="so-sub">{(vm.activeNsMeta?.visibility ?? "unlisted") === "private" ? "Private" : "Unlisted"} — reachable only to someone who holds an <b>invite</b> to this namespace (mint one in the Invites tab). The invite is the access control.</p>
        {/if}
        <div class="section-sep"></div>

        <!-- matrix.md §17.1 outbound projection. One switch per *connected* realm
             provider, read from the plugin catalog's `schemes` rather than
             hardcoding "matrix": a future adapter shows up here with no client
             change, and a realm nobody is bridging is not offered at all. -->
        {#if realmSchemes.length}
          <div class="field-label">Projection</div>
          <p class="so-sub">
            Mirror <b>{serverVanity}</b> into a foreign realm, so its users can find and join it there.
            This is also what authorizes that realm to attribute its users into this namespace, so it is
            your consent as owner.
          </p>
          {#each realmSchemes as scheme (scheme)}
            <label class="fed-check" style="margin-bottom:6px">
              <input
                type="checkbox"
                checked={(vm.activeNsMeta?.visibility ?? "") === "public" && projectedInto(scheme)}
                disabled={(vm.activeNsMeta?.visibility ?? "") !== "public"}
                onchange={(e) => nsAdmin.nsSetProjection(scheme, e.currentTarget.checked)}
              />
              Project into <b>{scheme}</b>
            </label>
          {/each}
          {#if (vm.activeNsMeta?.visibility ?? "") !== "public"}
            <p class="so-sub">Projection needs a <b>public</b> namespace — an unlisted or private one would be
              exposed by the foreign realm's own directory, leaking exactly what its visibility hides. Change
              visibility in Overview first.</p>
          {/if}
          <div class="section-sep"></div>
        {/if}

        <div class="field-label">Active bridges</div>
        <div class="modal-list">
          {#each Object.values(store.federation.manifests) as m (m.peer)}
            <div class="ns-card">
              <div class="ns-info">
                <div class="ns-name">{m.peer} <span class="rep-state {m.state}">{m.state}</span> · v{m.version}</div>
                <div class="ns-desc">{m.channels.length} channel(s) · history {m.history} · media {m.media}{m.typing ? " · typing" : ""}</div>
              </div>
              <div class="fed-actions">
                <button onclick={() => store.federation.bridgeAccept(m.peer, m.version)}>Accept</button>
                <button class="mini-danger" onclick={() => store.federation.bridgeSever(m.peer)}>Sever</button>
              </div>
            </div>
          {:else}
            <div class="empty-hint">No bridges yet — propose one below, or wait for an inbound peer.</div>
          {/each}
        </div>
        <div class="section-sep"></div>
        <div class="field-label">Propose a bridge</div>
        <p class="so-sub">Snapshot this namespace's channels to <code>&lt;peer&gt;</code> and offer a bridge. Live on mutual accept.</p>
        <input class="text-input" bind:value={brPeer} placeholder="peer network (e.g. weft.example)" onkeydown={(e) => e.key === "Enter" && proposeBridge()} />
        <div class="fed-propose">
          <div class="fed-field">
            <div class="field-label">History</div>
            <div class="segmented compact">
              {#each BR_HISTORY as o (o.value)}
                <button type="button" class="seg" class:on={brHistory === o.value} role="radio" aria-checked={brHistory === o.value} onclick={() => (brHistory = o.value)}>
                  <span class="seg-label">{o.label}</span>
                </button>
              {/each}
            </div>
          </div>
          <div class="fed-field">
            <div class="field-label">Media</div>
            <div class="segmented compact">
              {#each BR_MEDIA as o (o.value)}
                <button type="button" class="seg" class:on={brMedia === o.value} role="radio" aria-checked={brMedia === o.value} onclick={() => (brMedia = o.value)}>
                  <span class="seg-label">{o.label}</span>
                </button>
              {/each}
            </div>
          </div>
          <label class="fed-check"><input type="checkbox" bind:checked={brTyping} /> Relay typing</label>
          <button class="ok-btn" onclick={proposeBridge}>Propose</button>
        </div>
        <p class="so-sub" style="margin-top:14px">Outbound bridge transmission needs the M5d dialer; inbound peering, accept, and sever work today. Network-wide defederation (blocking a peer network entirely) is a network-operator action.</p>
      {:else if app.nsTab === "recovery"}
        <h1>Recovery quorum</h1>
        <p class="so-sub">M-of-N root recovery. Share your recovery key, or co-sign and submit a rotation.</p>
        <div class="field-label">Threshold M</div>
        <input class="text-input" type="number" min="1" bind:value={nsAdmin.recM} />
        <div class="section-sep"></div>
        <div class="field-label">Quorum keys (comma-separated b64 pubkeys)</div>
        <input class="text-input" bind:value={nsAdmin.recKeys} placeholder="key1,key2,key3" />
        <div class="modal-actions"><button class="ok-btn" onclick={() => nsAdmin.recKeys.trim() && weft.nsRecoverySet(app.activeServer, nsAdmin.recM, nsAdmin.recKeys.trim()).catch((e) => app.toast(String(e), "error"))}>Set recovery quorum</button></div>
        <div class="section-sep"></div>
        <div class="set-row">
          <span>My recovery key (share for the quorum)</span>
          <button class="set-btn" onclick={() => nsAdmin.showRecoveryKey()}>Reveal</button>
        </div>
        {#if nsAdmin.myRecoveryKey}
          <div class="modal-join"><input readonly value={nsAdmin.myRecoveryKey} /><button onclick={() => navigator.clipboard?.writeText(nsAdmin.myRecoveryKey)}>Copy</button></div>
        {/if}
        <div class="field-label">Rotation record (co-sign or submit)</div>
        <textarea class="text-input" rows="2" bind:value={nsAdmin.recoveryDoc} placeholder="paste a record to co-sign, or Start one below"></textarea>
        <div class="modal-actions">
          <button class="set-btn" onclick={() => nsAdmin.startRecovery()}>Start (recover to me)</button>
          <button class="set-btn" onclick={() => nsAdmin.cosignRecovery()}>Co-sign</button>
          <button class="ok-btn" onclick={() => nsAdmin.submitRecovery()}>Submit</button>
        </div>
      {:else if app.nsTab === "danger"}
        <h1>Danger zone</h1>
        <p class="so-sub">Irreversible actions. Transfer is root-key-signed on this device.</p>
        <div class="field-label">Transfer ownership to</div>
        <input class="text-input" bind:value={nsAdmin.newOwner} placeholder="account" />
        <div class="modal-actions">
          <button class="danger-btn" onclick={app.doTransfer}>Transfer (root-signed)</button>
        </div>
        <div class="section-sep"></div>
        <div class="modal-actions"><button class="danger-btn" onclick={app.deleteNamespace}>Delete namespace</button></div>
      {/if}
    </div>
  </main>
  <div class="so-exit">
    <button class="so-close" aria-label="Close settings" onclick={onclose}>✕</button>
    <span class="so-close-label">ESC</span>
  </div>
</div>
