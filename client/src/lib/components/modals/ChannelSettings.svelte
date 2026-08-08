<script lang="ts">
  import { vm } from "$lib/navigation/viewmodel.svelte";
  import { chanNsScope, chanRoleCaps, setChanRoleCaps, chanMemberGrants, chanMemberCaps, setChanMemberCaps, removeChanRole, removeChanMember, toggleRestricted, toggleViewGated } from "$lib/roles/chanperms";
  import { roleStore } from "$lib/roles/roles.svelte";
  import { channelStore, nsOf } from "$lib/channels/channel.svelte";
  import { profileStore } from "$lib/profile/profile.svelte";
  import { fade } from "svelte/transition";
  import { untrack } from "svelte";
  import { getApp } from "$lib/ui/context";
  import * as weft from "$lib/transport/weft";
  import { CHAN_CAPS, CAP_META, EVERYONE_ROLE, RETENTION_OPTIONS } from "$lib/constants";
  import Avatar from "$lib/components/Avatar.svelte";
  import SaveBar from "$lib/components/SaveBar.svelte";
  import PluginBlock from "$lib/components/plugins/PluginBlock.svelte";
  import { plugins } from "$lib/plugins/plugins.svelte";
  const app = getApp();
  let { channel, onclose }: { channel: string; onclose: () => void } = $props();

  /// A plugin page is `plugin:<id>:<action>`, namespaced so it can never
  /// shadow a native tab.
  type Tab = "overview" | "permissions" | "danger" | `plugin:${string}:${string}`;
  let tab = $state<Tab>("overview");

  // §13.1 the `channel-settings` surface: per-channel configuration a plugin
  // supplies (the bridged Matrix room's name/topic, say) — a page here rather
  // than a button in the channel list.
  const pluginPages = $derived(
    plugins
      .actionsFor("channel-settings")
      // A realm adapter's channel page belongs to its own realm's channels. Without
      // this, Matrix's "Bridged room settings" appeared on every channel and opened
      // a flow the adapter refuses ("this channel is not bridged"), leaving the
      // panel on `Loading…` forever.
      .filter(({ plugin }) => plugins.governs(plugin, nsOf(channel)))
      .filter(({ action }) => action.context === "channel" || action.context === "none"),
  );
  /// The panel backing the open plugin page, once its flow has answered. A
  /// plugin that answers with a modal instead is drawn by `AppModals`.
  const pluginView = $derived(tab.startsWith("plugin:") ? [...plugins.views.values()].find((v) => v.isPanel) : undefined);

  /// Open a plugin page: close whatever panel was showing (so its plugin stops
  /// pushing into a screen nobody is watching), then invoke with this channel.
  function openPluginPage(plugin: string, action: string) {
    for (const v of plugins.views.values()) {
      if (v.isPanel) plugins.close(v.id);
    }

    tab = `plugin:${plugin}:${action}`;
    plugins.invoke(plugin, action, channel);
  }

  // ---- §6.5 permission editor (per-target: @everyone / role / member) ----
  type Target =
    | { kind: "everyone" }
    | { kind: "role"; name: string; color: string }
    | { kind: "member"; account: string };
  let selected = $state<Target>({ kind: "everyone" });
  let addOpen = $state(false);
  let memberQuery = $state("");
  // Targets the admin just added but hasn't granted a cap to yet — kept locally
  // so an empty override still shows in the list until its first cap lands.
  let pendingRoles = $state<string[]>([]);
  let pendingMembers = $state<string[]>([]);

  // The namespace's roles = the override picker's role source (@everyone aside).
  const nsRoles = $derived(
    roleStore.rolesAt(chanNsScope()).filter((r) => r.name !== EVERYONE_ROLE),
  );
  // Role overrides live as channel-scoped roles; merge with just-added ones.
  const roleTargets = $derived.by(() => {
    const present = roleStore.rolesAt(channel)
      .filter((r) => r.name !== EVERYONE_ROLE)
      .map((r) => r.name);
    return [...new Set([...present, ...pendingRoles])].map(
      (n) => nsRoles.find((r) => r.name === n) ?? { name: n, color: "#99aab5" },
    );
  });
  const memberTargets = $derived.by(() => {
    const present = chanMemberGrants().map((g) => g.subject);
    return [...new Set([...present, ...pendingMembers])];
  });
  const addableRoles = $derived(nsRoles.filter((r) => !roleTargets.some((t) => t.name === r.name)));
  // The member picker: every server member we know of, minus those already a
  // target, filtered by the search box.
  const memberChoices = $derived.by(() => {
    const seen = new Set<string>();
    const out: string[] = [];
    for (const g of vm.channelGroups)
      for (const c of g.list)
        for (const m of c.members)
          if (!seen.has(m.name)) {
            seen.add(m.name);
            out.push(m.name);
          }
    const q = memberQuery.toLowerCase();
    return out
      .filter(
        (n) =>
          !memberTargets.includes(n) &&
          (profileStore.displayName(n).toLowerCase().includes(q) || n.toLowerCase().includes(q)),
      )
      .sort((a, b) => a.localeCompare(b));
  });

  // Draft-and-commit editing (the profile-editor pattern): toggles mutate a
  // local draft, and a Revert/Save bar commits it — never per-click.
  const sameCaps = (a: string[], b: string[]) =>
    a.length === b.length && [...a].sort().join() === [...b].sort().join();
  const persistedCaps = () => {
    if (selected.kind === "everyone") return chanRoleCaps(EVERYONE_ROLE);
    if (selected.kind === "role") return chanRoleCaps(selected.name);
    return chanMemberCaps(selected.account);
  };
  let draft = $state<string[]>([]);
  // Re-seed the draft whenever the selected target changes (untracked reads so
  // an unrelated ROLES/GRANTS refresh doesn't clobber an in-progress edit).
  $effect(() => {
    selected;
    untrack(() => (draft = [...persistedCaps()]));
  });
  const permDirty = $derived(!sameCaps(draft, persistedCaps()));
  function toggleCap(cap: string) {
    draft = draft.includes(cap) ? draft.filter((c) => c !== cap) : [...draft, cap];
  }
  function revertPerms() {
    draft = [...persistedCaps()];
  }
  function savePerms() {
    if (selected.kind === "everyone") setChanRoleCaps(EVERYONE_ROLE, "#99aab5", draft);
    else if (selected.kind === "role") setChanRoleCaps(selected.name, selected.color, draft);
    else setChanMemberCaps(selected.account, draft);
  }
  function addRole(r: { name: string; color: string }) {
    if (!pendingRoles.includes(r.name)) pendingRoles = [...pendingRoles, r.name];
    selected = { kind: "role", name: r.name, color: r.color };
    addOpen = false;
  }
  function addMember(account: string) {
    const a = account.trim();
    if (!a) return;
    if (!pendingMembers.includes(a)) pendingMembers = [...pendingMembers, a];
    selected = { kind: "member", account: a };
    addOpen = false;
    memberQuery = "";
  }
  function removeRoleTarget(name: string) {
    pendingRoles = pendingRoles.filter((n) => n !== name);
    removeChanRole(name);
    if (selected.kind === "role" && selected.name === name) selected = { kind: "everyone" };
  }
  function removeMemberTarget(account: string) {
    pendingMembers = pendingMembers.filter((n) => n !== account);
    removeChanMember(account);
    if (selected.kind === "member" && selected.account === account) selected = { kind: "everyone" };
  }
  const roleColor = (name: string) =>
    roleStore.rolesAt(chanNsScope()).find((r) => r.name === name)?.color ?? "#99aab5";

  const rec = $derived(channelStore.channels[channel]);
  const ns = $derived(app.nsOf(channel)); // "" for a top-level channel
  // Re-seed the editable fields when the channel identity changes (including a
  // rename, which swaps the prop) — but untrack the record reads so unrelated
  // channel events don't clobber an in-progress edit.
  let slug = $state("");
  let topic = $state("");
  $effect(() => {
    channel;
    untrack(() => {
      slug = app.chanShort(channel);
      topic = channelStore.channels[channel]?.topic ?? "";
    });
  });

  function doRename() {
    const s = slug.trim().replace(/^#/, "").replace(/\s+/g, "-").toLowerCase();
    if (!s) return;
    const target = ns ? `#${ns}/${s}` : `#${s}`;
    if (target === channel) return;
    app.expectSuccess(`rename:${target}`, `Renamed to #${s}`);
    weft.channelRename(channel, target).catch((e) => app.toast(String(e), "error"));
  }
  function saveTopic() {
    weft.channelMeta(channel, "topic", topic).catch((e) => app.toast(String(e), "error"));
  }
  function setRetention(policy: string) {
    app.expectSuccess(`policy:${channel}`, "Retention updated");
    weft.channelPolicy(channel, policy).catch((e) => app.toast(String(e), "error"));
  }
  async function deleteChannel() {
    if (!(await app.confirm(`Delete #${app.chanShort(channel)}? This can't be undone.`, "Delete")))
      return;
    weft
      .channelDelete(channel)
      .then(() => onclose())
      .catch((e) => app.toast(String(e), "error"));
  }
</script>

<svelte:window onclick={() => (addOpen = false)} />

<div class="settings-overlay" role="dialog" aria-modal="true" transition:fade|global={{ duration: 150 }}>
  <nav class="so-nav">
    <div class="so-nav-inner">
      <div class="so-heading">#{app.chanShort(channel)}</div>
      <button class="so-navitem" class:active={tab === "overview"} onclick={() => (tab = "overview")}>Overview</button>
      <button class="so-navitem" class:active={tab === "permissions"} onclick={() => (tab = "permissions")}>Permissions</button>
      {#each pluginPages as { plugin, action } (plugin + action.id)}
        <button
          class="so-navitem"
          class:active={tab === `plugin:${plugin}:${action.id}`}
          title={action.description ?? ""}
          onclick={() => openPluginPage(plugin, action.id)}
        >
          {action.label}
        </button>
      {/each}
      <div class="so-heading">Danger</div>
      <button class="so-navitem danger" class:active={tab === "danger"} onclick={() => (tab = "danger")}>Delete channel</button>
    </div>
  </nav>
  <main class="so-main">
    <div class="so-content" class:wide={tab === "permissions"}>
      {#if tab === "overview"}
        <h1>Overview</h1>
        <p class="so-sub">The channel's address — members are moved automatically on rename.</p>
        <div class="field-label">Channel name</div>
        <div class="modal-join">
          <span class="chan-prefix">{ns ? `#${ns}/` : "#"}</span>
          <input class="text-input" bind:value={slug} onkeydown={(e) => e.key === "Enter" && doRename()} />
          <button disabled={!slug.trim()} onclick={doRename}>Rename</button>
        </div>

        <div class="section-sep"></div>
        <div class="field-label">Topic</div>
        <div class="modal-join">
          <input class="text-input" bind:value={topic} placeholder="what's this channel about" onkeydown={(e) => e.key === "Enter" && saveTopic()} />
          <button onclick={saveTopic}>Save</button>
        </div>

        <div class="section-sep"></div>
        <div class="field-label">Retention</div>
        <p class="so-sub">How long messages are kept. Switching to/from <code>e2ee</code> needs an empty channel or a purge.</p>
        <div class="cap-chips">
          {#each RETENTION_OPTIONS as o (o.value)}
            <button type="button" class="cap-chip" class:on={rec?.retention === o.key} onclick={() => setRetention(o.value)}>{o.label}</button>
          {/each}
        </div>

        <div class="section-sep"></div>
        <div class="field-label">Announcement mode</div>
        <div class="set-row">
          <span>Everyone reads (<code>view</code>), only members with <code>send</code> may post</span>
          <button class="chip-btn" class:on={rec?.restricted} onclick={toggleRestricted}>{rec?.restricted ? "On" : "Off"}</button>
        </div>

        <div class="section-sep"></div>
        <div class="field-label">Private channel</div>
        <div class="set-row">
          <span>Hide this channel from anyone without the <code>view</code> capability. Grant <code>view</code> to roles or members in <b>Permissions</b> to let them in.</span>
          <button class="chip-btn" class:on={rec?.viewGated} onclick={toggleViewGated}>{rec?.viewGated ? "On" : "Off"}</button>
        </div>
      {:else if tab === "permissions"}
        <h1>Permissions</h1>
        <p class="so-sub">Pick who a permission set applies to — the <b>@everyone</b> baseline, a role, or an individual member — then toggle their capabilities in this channel. Roles apply to everyone who holds them; a member override is a direct grant.{#if rec?.viewGated} This channel is <b>private</b> — only targets with <b>View channel</b> can see it.{/if}</p>

        <div class="cp-wrap">
          <!-- ─── target list ─── -->
          <aside class="cp-side">
            <button
              class="cp-row"
              class:active={selected.kind === "everyone"}
              onclick={() => (selected = { kind: "everyone" })}
            >
              <span class="cp-dot" style="background:#99aab5"></span>
              <span class="cp-name">@everyone</span>
              <span class="cp-tag">Baseline</span>
            </button>

            <div class="cp-label">Roles</div>
            {#each roleTargets as r (r.name)}
              <div class="cp-row-wrap">
                <button
                  class="cp-row"
                  class:active={selected.kind === "role" && selected.name === r.name}
                  onclick={() => (selected = { kind: "role", name: r.name, color: r.color })}
                >
                  <span class="cp-dot" style="background:{r.color}"></span>
                  <span class="cp-name">{r.name}</span>
                </button>
                <button class="cp-x" title="Remove role override" aria-label={`Remove ${r.name} override`} onclick={() => removeRoleTarget(r.name)}>✕</button>
              </div>
            {:else}
              <div class="cp-empty">No role overrides.</div>
            {/each}

            <div class="cp-label">Members</div>
            {#each memberTargets as m (m)}
              <div class="cp-row-wrap">
                <button
                  class="cp-row"
                  class:active={selected.kind === "member" && selected.account === m}
                  onclick={() => (selected = { kind: "member", account: m })}
                >
                  <span class="cp-avatar"><Avatar account={m} /></span>
                  <span class="cp-name">{profileStore.displayName(m)}</span>
                </button>
                <button class="cp-x" title="Remove member override" aria-label={`Remove ${m} override`} onclick={() => removeMemberTarget(m)}>✕</button>
              </div>
            {:else}
              <div class="cp-empty">No member overrides.</div>
            {/each}

            <!-- svelte-ignore a11y_no_static_element_interactions -->
            <!-- svelte-ignore a11y_click_events_have_key_events -->
            <div class="cp-add-wrap" onclick={(e) => e.stopPropagation()}>
              <button class="cp-add" onclick={() => (addOpen = !addOpen)}>+ Add role or member</button>
              {#if addOpen}
                <!-- svelte-ignore a11y_no_static_element_interactions -->
                <!-- svelte-ignore a11y_click_events_have_key_events -->
                <div class="cp-picker" onclick={(e) => e.stopPropagation()}>
                  <div class="cp-picker-label">Roles</div>
                  {#each addableRoles as r (r.name)}
                    <button class="cp-pick" onclick={() => addRole(r)}>
                      <span class="cp-dot" style="background:{r.color}"></span>{r.name}
                    </button>
                  {:else}
                    <div class="cp-pick-empty">All roles added.</div>
                  {/each}
                  <div class="cp-picker-label">Members</div>
                  <input class="cp-search" bind:value={memberQuery} placeholder="Search or type a name…" />
                  {#each memberChoices.slice(0, 8) as m (m)}
                    <button class="cp-pick" onclick={() => addMember(m)}>
                      <span class="cp-avatar sm"><Avatar account={m} /></span>{profileStore.displayName(m)}
                    </button>
                  {/each}
                  {#if memberQuery.trim() && !memberChoices.includes(memberQuery.trim())}
                    <button class="cp-pick" onclick={() => addMember(memberQuery)}>Add “{memberQuery.trim()}”</button>
                  {/if}
                </div>
              {/if}
            </div>
          </aside>

          <!-- ─── capability editor ─── -->
          <section class="cp-editor">
            <div class="cp-head">
              {#if selected.kind === "everyone"}
                <span class="cp-dot lg" style="background:#99aab5"></span>
                <span class="cp-head-name">@everyone</span>
                <span class="cp-tag">Baseline</span>
              {:else if selected.kind === "role"}
                <span class="cp-dot lg" style="background:{roleColor(selected.name)}"></span>
                <span class="cp-head-name">{selected.name}</span>
                <span class="cp-tag">Role</span>
              {:else}
                <span class="cp-avatar lg"><Avatar account={selected.account} /></span>
                <span class="cp-head-name">{profileStore.displayName(selected.account)}</span>
                <span class="cp-tag">Member</span>
              {/if}
            </div>
            <p class="cp-sub">
              {#if selected.kind === "everyone"}
                Every member of this channel holds these implicitly.
              {:else if selected.kind === "role"}
                Granted to everyone who holds <b>{selected.name}</b>, in this channel only.
              {:else}
                A direct grant to <b>{profileStore.displayName(selected.account)}</b>, in this channel only.
              {/if}
            </p>

            <div class="cp-perms">
              {#each CHAN_CAPS as cap (cap)}
                <div class="cp-perm">
                  <div class="cp-perm-text">
                    <div class="cp-perm-label">{CAP_META[cap]?.label ?? cap}</div>
                    <div class="cp-perm-sub">{CAP_META[cap]?.desc ?? ""}</div>
                  </div>
                  <button
                    class="cp-toggle"
                    class:on={draft.includes(cap)}
                    role="switch"
                    aria-checked={draft.includes(cap)}
                    aria-label={CAP_META[cap]?.label ?? cap}
                    onclick={() => toggleCap(cap)}
                  ><span class="cp-knob"></span></button>
                </div>
              {/each}
            </div>
          </section>
        </div>
      {:else if tab.startsWith("plugin:")}
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
      {:else if tab === "danger"}
        <h1>Delete channel</h1>
        <p class="so-sub">Removes the channel and its history. This cannot be undone.</p>
        <div class="modal-actions"><button class="danger-btn" onclick={deleteChannel}>Delete #{app.chanShort(channel)}</button></div>
      {/if}
    </div>
  </main>
  <div class="so-exit">
    <button class="so-close" aria-label="Close settings" onclick={onclose}>✕</button>
    <span class="so-close-label">ESC</span>
  </div>
</div>

{#if tab === "permissions" && permDirty}
  <SaveBar onrevert={revertPerms} onsave={savePerms} />
{/if}
