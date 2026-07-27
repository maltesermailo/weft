<script lang="ts">
  import { fade } from "svelte/transition";
  import { getApp } from "$lib/context";
  import * as weft from "$lib/weft";
  import RolesTab from "$lib/components/modals/RolesTab.svelte";
  import InviteList from "$lib/components/InviteList.svelte";
  import Avatar from "$lib/components/Avatar.svelte";
  const app = getApp();

  // ---- Per-capability tab visibility (§6.5) ----
  // A moderator sees only the tabs they can act on; owner / ns-admin sees all.
  // Each maps to the concrete WEFT capability that governs that surface.
  const isAdmin = $derived(app.isNsOwner(app.account) || app.serverCap("ns-admin"));
  const tabPerm = $derived({
    overview: isAdmin,
    roles: isAdmin || app.serverCanGrant(),
    members: isAdmin || app.serverCanGrant() || ["ban", "mute", "kick", "reports"].some((c) => app.serverCap(c)),
    emoji: isAdmin,
    invites: isAdmin || app.serverCap("invite"),
    federation: isAdmin,
    bans: isAdmin || app.serverCap("ban") || app.serverCap("mute"),
    recovery: app.isNsOwner(app.account),
    danger: app.isNsOwner(app.account),
  } as Record<string, boolean>);
  const visibleTabs = $derived(
    (["overview", "roles", "members", "emoji", "invites", "federation", "bans", "recovery", "danger"] as const).filter(
      (t) => tabPerm[t],
    ),
  );
  // Keep the active tab on something the user can actually see — a mod opening on
  // the default (admin-only) tab lands on their first real one instead.
  $effect(() => {
    if (visibleTabs.length && !visibleTabs.includes(app.nsTab)) app.nsTab = visibleTabs[0];
  });

  // ---- Members directory (NS INFO MEMBERS) ----
  let memberSearch = $state("");
  const roster = $derived(app.nsMembersByNs[app.activeServer] ?? []);
  const shownMembers = $derived(
    roster.filter(
      (m) =>
        app.displayName(m.account).toLowerCase().includes(memberSearch.toLowerCase()) ||
        m.account.toLowerCase().includes(memberSearch.toLowerCase()),
    ),
  );
  // A namespace-scoped role's color, for the member's role pills.
  function roleColor(name: string): string {
    return (app.rolesByScope[`ns:${app.activeServer}`] ?? []).find((r) => r.name === name)?.color ?? "#99aab5";
  }
  // Join date: "0" means the server had no recorded join time (pre-v0.12 backfill).
  function fmtJoined(ms: number): string {
    if (!ms) return "—";
    return new Date(ms).toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
  }
  // All namespace-scoped roles, for the in-line "add role" picker.
  const nsRoles = $derived(app.rolesByScope[app.nsRoleScope()] ?? []);
  // The namespace owner (implicit all-caps holder), surfaced with a crown.
  const ownerAccount = $derived(app.activeNsMeta?.owner ?? null);
  // Which member's add-role popover is open (keyed by `account@network`).
  let addRoleFor = $state<string | null>(null);
  // Roles a member doesn't already hold — the options for their add-role menu.
  function unheldRoles(held: string[]) {
    return nsRoles.filter((r) => !held.includes(r.name));
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
    app.bridgePropose(`ns:${app.activeServer}`, p, brHistory, brMedia, brTyping);
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
        const up = await weft.upload(file);
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
    app.addEmoji(name, pendingEmoji);
    emojiName = "";
    pendingEmoji = "";
  }
  function cancelEmoji() {
    emojiName = "";
    pendingEmoji = "";
  }

  // Live counts for the Overview stat strip (real data — no placeholders).
  const nsChannelCount = $derived(app.channelGroups.reduce((n, g) => n + g.list.length, 0));
  const nsRoleCount = $derived((app.rolesByScope[app.nsRoleScope()] ?? []).length);
  // §6.2 welcome-channel picker: this namespace's text channels + the current
  // setting (from the ns-meta the server last pushed).
  const nsTextChannels = $derived(app.channelGroups.flatMap((g) => g.list).filter((c) => !c.voice));
  const currentWelcome = $derived(app.activeNsMeta?.welcome ?? "");

  // Custom-emoji capacity gauge.
  const EMOJI_SLOTS = 50;
  let emojiSearch = $state("");
  const shownEmoji = $derived(
    app.activeEmoji.filter((e) => e.name.toLowerCase().includes(emojiSearch.toLowerCase())),
  );
</script>

<svelte:window onclick={() => (addRoleFor = null)} />

<div class="settings-overlay" role="dialog" aria-modal="true" transition:fade|global={{ duration: 150 }}>
  <nav class="so-nav">
    <div class="so-nav-inner">
      <div class="so-server-head">
        <span class="so-server-avatar">{app.initials(app.activeServer)}</span>
        <div class="so-server-meta">
          <div class="so-server-name">{app.activeServer}</div>
          <div class="so-server-sub">Server Settings</div>
        </div>
      </div>
      {#if tabPerm.overview || tabPerm.roles || tabPerm.members || tabPerm.emoji}
        <div class="so-heading">Server Settings</div>
      {/if}
      {#if tabPerm.overview}
        <button class="so-navitem" class:active={app.nsTab === "overview"} onclick={() => (app.nsTab = "overview")}>Overview</button>
      {/if}
      {#if tabPerm.roles}
        <button class="so-navitem" class:active={app.nsTab === "roles"} onclick={() => (app.nsTab = "roles")}>Roles</button>
      {/if}
      {#if tabPerm.members}
        <button class="so-navitem" class:active={app.nsTab === "members"} onclick={() => { app.nsTab = "members"; app.fetchNsMembers(app.activeServer); app.refreshBans(); }}>Members</button>
      {/if}
      {#if tabPerm.emoji}
        <button class="so-navitem" class:active={app.nsTab === "emoji"} onclick={() => (app.nsTab = "emoji")}>Emoji</button>
      {/if}
      {#if tabPerm.invites || tabPerm.federation}
        <div class="so-heading">Community</div>
      {/if}
      {#if tabPerm.invites}
        <button class="so-navitem" class:active={app.nsTab === "invites"} onclick={() => { app.nsTab = "invites"; app.loadNsInvites(); }}>Invites</button>
      {/if}
      {#if tabPerm.federation}
        <button class="so-navitem" class:active={app.nsTab === "federation"} onclick={() => (app.nsTab = "federation")}>Federation</button>
      {/if}
      {#if tabPerm.bans}
        <div class="so-heading">Moderation</div>
        <button class="so-navitem" class:active={app.nsTab === "bans"} onclick={() => { app.nsTab = "bans"; app.refreshBans(); }}>Bans &amp; mutes</button>
      {/if}
      {#if tabPerm.recovery || tabPerm.danger}
        <div class="so-heading">Security</div>
      {/if}
      {#if tabPerm.recovery}
        <button class="so-navitem" class:active={app.nsTab === "recovery"} onclick={() => (app.nsTab = "recovery")}>Recovery</button>
      {/if}
      {#if tabPerm.danger}
        <button class="so-navitem danger" class:active={app.nsTab === "danger"} onclick={() => (app.nsTab = "danger")}>Danger zone</button>
      {/if}
    </div>
  </nav>
  <main class="so-main">
    <div class="so-content" class:wide={app.nsTab === "roles"}>
      {#if app.activeNsMeta?.recovery_eta}
        <div class="ns-card recovery-pending">
          <div class="ns-info">
            <div class="ns-name">⚠ Recovery pending (rung {app.activeNsMeta.recovery_rung})</div>
            <div class="ns-desc">A root rotation is scheduled. As the live owner you can veto it.</div>
          </div>
          <button class="danger-btn" onclick={() => weft.nsRecoveryCancel(app.network, app.activeServer).catch((e) => app.toast(String(e), "error"))}>Cancel recovery</button>
        </div>
      {/if}

      {#if app.nsTab === "overview"}
        <h1>Overview</h1>
        <p class="so-sub">How this namespace appears in invites and, if listed, in Discover.</p>

        <div class="ov-card">
          <div class="ov-identity">
            <span class="ov-avatar">{app.initials(app.nsTitle.trim() || app.activeServer)}</span>
            <div class="ov-identity-meta">
              <div class="ov-identity-name">{app.nsTitle.trim() || app.activeServer}</div>
              <div class="ov-identity-sub">Namespace on {app.network}</div>
            </div>
          </div>
          <div class="field-label">Display name</div>
          <input class="text-input" bind:value={app.nsTitle} placeholder={app.activeServer} />
          <div class="ov-gap"></div>
          <div class="field-label">Visibility</div>
          <div class="segmented" role="radiogroup" aria-label="Visibility">
            {#each VIS_OPTIONS as o (o.value)}
              <button
                type="button"
                class="seg"
                class:on={app.nsVis === o.value}
                role="radio"
                aria-checked={app.nsVis === o.value}
                onclick={() => (app.nsVis = o.value)}
              >
                <span class="seg-label">{o.label}</span>
                <span class="seg-desc">{o.desc}</span>
              </button>
            {/each}
          </div>
          <div class="ov-gap"></div>
          <div class="field-label">Description</div>
          <textarea class="text-input ov-desc" rows="3" bind:value={app.nsDesc} placeholder="what's this namespace about"></textarea>
          <div class="ov-gap"></div>
          <div class="field-label">Welcome channel</div>
          <p class="so-sub" style="margin:0 0 8px">Post a greeting here whenever someone new joins the server.</p>
          <select class="text-input" value={currentWelcome} onchange={(e) => app.nsSetWelcome(e.currentTarget.value)}>
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
            <div class="ov-stat-num" style="color:var(--thread-amber)">{app.activeEmoji.length}</div>
            <div class="ov-stat-label">Emoji</div>
          </div>
          <div class="ov-stat">
            <div class="ov-stat-num" style="text-transform:capitalize">{app.nsVis}</div>
            <div class="ov-stat-label">Visibility</div>
          </div>
        </div>

        <div class="modal-actions"><button class="ok-btn" onclick={app.saveNsMeta}>Save changes</button></div>
      {:else if app.nsTab === "invites"}
        <h1>Invites</h1>
        <p class="so-sub">Every active invite for <b>{app.activeServer}</b> — who created it, how many times it's been used, its remaining uses and expiry. Revoke one, or close them all at once.</p>
        <div class="modal-actions">
          <button class="ok-btn" onclick={app.createInvite}>Create invite</button>
          <button class="danger-btn" onclick={app.revokeAllInvites}>Revoke all</button>
        </div>
        <div class="section-sep"></div>
        <InviteList showCreate={false} />
      {:else if app.nsTab === "roles"}
        <RolesTab />
      {:else if app.nsTab === "members"}
        <h1>Members</h1>
        <p class="so-sub">Everyone in <b>{app.activeServer}</b>, when they joined, and the roles they hold. Click a role's <b>✕</b> to remove it, <b>+</b> to add one — roles are the only way to grant capabilities. Right-click a member for moderation.</p>

        <div class="mem-search">
          <span aria-hidden="true">⌕</span>
          <input bind:value={memberSearch} placeholder="Search members" />
          <button class="mem-refresh" title="Refresh" aria-label="Refresh roster" onclick={() => { app.fetchNsMembers(app.activeServer); app.refreshBans(); }}>↻</button>
        </div>
        <div class="mem-count">{shownMembers.length} {shownMembers.length === 1 ? "member" : "members"}</div>

        <div class="mem-table">
          <div class="mem-thead">
            <span>Member</span>
            <span>Roles</span>
            <span>Joined</span>
          </div>
          {#each shownMembers as m (m.account + "@" + m.network)}
            {@const handle = m.account + "@" + m.network}
            {@const isOwner = ownerAccount === m.account}
            <!-- svelte-ignore a11y_no_static_element_interactions -->
            <div class="mem-row" oncontextmenu={(e) => app.nsMemberCtx(e, m.account)}>
              <div class="mem-id">
                <span class="mem-avatar"><Avatar account={m.account} /></span>
                <div class="mem-id-meta">
                  <div class="mem-name">
                    {app.displayName(m.account)}
                    {#if isOwner}<span class="mem-owner" title="Server owner">👑 Owner</span>{/if}
                  </div>
                  <div class="mem-handle">{m.account}{m.network !== app.network ? `@${m.network}` : ""}</div>
                </div>
              </div>
              <div class="mem-roles">
                {#each m.roles as r (r)}
                  <span class="role-pill editable" style="--role:{roleColor(r)}">
                    <span class="role-dot"></span>{r}
                    <button class="role-x" title="Remove role" aria-label={`Remove ${r}`} onclick={() => app.unassignNsRole(m.account, r)}>✕</button>
                  </span>
                {/each}
                <!-- svelte-ignore a11y_no_static_element_interactions -->
                <!-- svelte-ignore a11y_click_events_have_key_events -->
                <div class="role-add-wrap" onclick={(e) => e.stopPropagation()}>
                  <button
                    class="role-add"
                    title="Add role"
                    aria-label={`Add a role to ${app.displayName(m.account)}`}
                    onclick={() => (addRoleFor = addRoleFor === handle ? null : handle)}
                  >+</button>
                  {#if addRoleFor === handle}
                    <!-- svelte-ignore a11y_no_static_element_interactions -->
                    <div class="role-add-menu" role="menu">
                      {#each unheldRoles(m.roles) as r (r.name)}
                        <button class="role-add-opt" onclick={() => { app.assignNsRole(m.account, r.name); addRoleFor = null; }}>
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
              {#if app.nsMembersLoading}
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
            <span class="em-gauge-count">{app.activeEmoji.length} / {EMOJI_SLOTS}</span>
          </div>
          <div class="em-gauge-bar">
            <div
              class="em-gauge-fill"
              class:full={app.activeEmoji.length >= EMOJI_SLOTS * 0.8}
              style="width:{Math.min(100, (app.activeEmoji.length / EMOJI_SLOTS) * 100)}%"
            ></div>
          </div>
          <div class="em-gauge-sub">{Math.max(0, EMOJI_SLOTS - app.activeEmoji.length)} slots remaining</div>
        </div>

        <div class="em-search">
          <span aria-hidden="true">⌕</span>
          <input bind:value={emojiSearch} placeholder="Search emoji" />
        </div>

        <div class="em-grid">
          {#each shownEmoji as em (em.name)}
            <div class="em-tile">
              <img class="em-tile-img" src={app.emojiUrlFor(em.name) ?? ''} alt=":{em.name}:" />
              <code class="em-tile-name">:{em.name}:</code>
              <button class="em-tile-x" aria-label="Remove :{em.name}:" title="Remove" onclick={() => app.removeEmoji(em.name)}>🗑</button>
            </div>
          {:else}
            <div class="em-empty">
              <div class="em-empty-icon">😊</div>
              <p>{app.activeEmoji.length ? "No emoji match your search." : "No custom emoji yet — upload one above."}</p>
            </div>
          {/each}
        </div>
      {:else if app.nsTab === "bans"}
        <h1>Bans &amp; mutes</h1>
        <p class="so-sub">Accounts denied at <code>ns:{app.activeServer}</code>. A <b>ban</b> blocks join + posting; a <b>mute</b> blocks posting. Lifting one takes effect immediately.</p>
        <div class="modal-list">
          {#each app.denyList() as d (d.kind + d.account)}
            <div class="ns-card">
              <div class="ns-info">
                <div class="ns-name">{d.account} <span class="rep-state {d.kind === "ban" ? "severed" : "added"}">{d.kind}</span></div>
                <div class="ns-desc">{d.reason ? d.reason : "no reason given"}{d.by ? ` · by ${d.by}` : ""}</div>
              </div>
              <div class="fed-actions">
                <button class="mini-danger" onclick={() => app.liftMod(d.kind, d.account)}>{d.kind === "ban" ? "Unban" : "Unmute"}</button>
              </div>
            </div>
          {:else}
            <div class="empty-hint">No bans or mutes at this server.</div>
          {/each}
        </div>
        <div class="modal-actions"><button class="set-btn" onclick={app.refreshBans}>Refresh</button></div>
      {:else if app.nsTab === "federation"}
        <h1>Federation</h1>
        <p class="so-sub">Bridge <b>{app.activeServer}</b>'s channels to a peer network. You control this as the namespace owner — bridges are scoped to <code>ns:{app.activeServer}</code>, non-transitive, and every change notifies members.</p>

        <div class="field-label">Auto-federation</div>
        <p class="so-sub">When open, another network can reach this namespace on demand — a user there references <code>{app.network}/{app.activeServer}</code> and their server auto-establishes the bridge. Off by default; enabling it is an explicit opt-in.</p>
        <label class="fed-check" style="margin-bottom:14px">
          <input
            type="checkbox"
            checked={app.activeNsMeta?.federation ?? false}
            onchange={(e) => app.nsSetFederation(e.currentTarget.checked)}
          />
          Open <b>{app.activeServer}</b> to auto-federation
        </label>
        {#if (app.activeNsMeta?.visibility ?? "") === "public"}
          <p class="so-sub">Public — reachable by <b>anyone</b> once open.</p>
        {:else}
          <p class="so-sub">{(app.activeNsMeta?.visibility ?? "unlisted") === "private" ? "Private" : "Unlisted"} — reachable only to someone who holds an <b>invite</b> to this namespace (mint one in the Invites tab). The invite is the access control.</p>
        {/if}
        <div class="section-sep"></div>

        <div class="field-label">Active bridges</div>
        <div class="modal-list">
          {#each Object.values(app.manifests) as m (m.peer)}
            <div class="ns-card">
              <div class="ns-info">
                <div class="ns-name">{m.peer} <span class="rep-state {m.state}">{m.state}</span> · v{m.version}</div>
                <div class="ns-desc">{m.channels.length} channel(s) · history {m.history} · media {m.media}{m.typing ? " · typing" : ""}</div>
              </div>
              <div class="fed-actions">
                <button onclick={() => app.bridgeAccept(m.peer, m.version)}>Accept</button>
                <button class="mini-danger" onclick={() => app.bridgeSever(m.peer)}>Sever</button>
              </div>
            </div>
          {:else}
            <div class="empty-hint">No bridges yet — propose one below, or wait for an inbound peer.</div>
          {/each}
        </div>
        <div class="section-sep"></div>
        <div class="field-label">Propose a bridge</div>
        <p class="so-sub">Snapshot this namespace's channels to <code>&lt;peer&gt;</code> and offer a bridge. Live on mutual accept.</p>
        <input class="text-input" bind:value={brPeer} placeholder="peer network (e.g. hda.example)" onkeydown={(e) => e.key === "Enter" && proposeBridge()} />
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
        <input class="text-input" type="number" min="1" bind:value={app.nsRecM} />
        <div class="section-sep"></div>
        <div class="field-label">Quorum keys (comma-separated b64 pubkeys)</div>
        <input class="text-input" bind:value={app.nsRecKeys} placeholder="key1,key2,key3" />
        <div class="modal-actions"><button class="ok-btn" onclick={() => app.nsRecKeys.trim() && weft.nsRecoverySet(app.activeServer, app.nsRecM, app.nsRecKeys.trim()).catch((e) => app.toast(String(e), "error"))}>Set recovery quorum</button></div>
        <div class="section-sep"></div>
        <div class="set-row">
          <span>My recovery key (share for the quorum)</span>
          <button class="set-btn" onclick={app.showRecoveryKey}>Reveal</button>
        </div>
        {#if app.myRecoveryKey}
          <div class="modal-join"><input readonly value={app.myRecoveryKey} /><button onclick={() => navigator.clipboard?.writeText(app.myRecoveryKey)}>Copy</button></div>
        {/if}
        <div class="field-label">Rotation record (co-sign or submit)</div>
        <textarea class="text-input" rows="2" bind:value={app.recoveryDoc} placeholder="paste a record to co-sign, or Start one below"></textarea>
        <div class="modal-actions">
          <button class="set-btn" onclick={app.startRecovery}>Start (recover to me)</button>
          <button class="set-btn" onclick={app.cosignRecovery}>Co-sign</button>
          <button class="ok-btn" onclick={app.submitRecovery}>Submit</button>
        </div>
      {:else if app.nsTab === "danger"}
        <h1>Danger zone</h1>
        <p class="so-sub">Irreversible actions. Transfer is root-key-signed on this device.</p>
        <div class="field-label">Transfer ownership to</div>
        <input class="text-input" bind:value={app.nsNewOwner} placeholder="account" />
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
