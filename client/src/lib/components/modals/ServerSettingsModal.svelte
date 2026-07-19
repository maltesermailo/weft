<script lang="ts">
  import { fade } from "svelte/transition";
  import { getApp } from "$lib/context";
  import * as weft from "$lib/weft";
  import { CAPS, ROLE_COLORS } from "$lib/constants";
  const app = getApp();
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
</script>

<div class="settings-overlay" role="dialog" aria-modal="true" transition:fade|global={{ duration: 150 }}>
  <nav class="so-nav">
    <div class="so-nav-inner">
      <div class="so-heading">{app.activeServer}</div>
      <button class="so-navitem" class:active={app.nsTab === "overview"} onclick={() => (app.nsTab = "overview")}>Overview</button>
      <button class="so-navitem" class:active={app.nsTab === "roles"} onclick={() => (app.nsTab = "roles")}>Roles</button>
      <button class="so-navitem" class:active={app.nsTab === "members"} onclick={() => (app.nsTab = "members")}>Members &amp; roles</button>
      <button class="so-navitem" class:active={app.nsTab === "bans"} onclick={() => { app.nsTab = "bans"; app.refreshBans(); }}>Bans &amp; mutes</button>
      <button class="so-navitem" class:active={app.nsTab === "federation"} onclick={() => (app.nsTab = "federation")}>Federation</button>
      <div class="so-heading">Security</div>
      <button class="so-navitem" class:active={app.nsTab === "recovery"} onclick={() => (app.nsTab = "recovery")}>Recovery</button>
      <button class="so-navitem danger" class:active={app.nsTab === "danger"} onclick={() => (app.nsTab = "danger")}>Danger zone</button>
    </div>
  </nav>
  <main class="so-main">
    <button class="so-close" aria-label="Close settings" onclick={onclose}>✕<span>ESC</span></button>
    <div class="so-content">
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
        <div class="field-label">Name</div>
        <input class="text-input" bind:value={app.nsTitle} placeholder="display name" />
        <div class="section-sep"></div>
        <div class="field-label">Description</div>
        <input class="text-input" bind:value={app.nsDesc} placeholder="what's this namespace about" />
        <div class="section-sep"></div>
        <div class="field-label">Visibility</div>
        <select class="text-input" bind:value={app.nsVis}>
          <option value="public">public</option>
          <option value="unlisted">unlisted</option>
          <option value="private">private</option>
        </select>
        <div class="modal-actions"><button class="ok-btn" onclick={app.saveNsMeta}>Save changes</button></div>
      {:else if app.nsTab === "roles"}
        <h1>Roles</h1>
        <p class="so-sub">Named capability bundles. Assigning a role grants its tokens — enforcement stays token-based.</p>
        <div class="role-list">
          {#each app.rolesByScope[app.nsRoleScope()] ?? [] as r (r.name)}
            <div class="role-row">
              <span class="role-swatch" style="background:{r.color}"></span>
              <div class="role-meta">
                <div class="role-title">{r.name}</div>
                <div class="role-caps">{r.caps.join(" · ")}</div>
              </div>
              <button class="mini-danger" onclick={() => app.deleteRole(r.name)}>Delete</button>
            </div>
          {:else}
            <div class="empty-hint">No roles yet — create one below.</div>
          {/each}
        </div>
        <div class="section-sep"></div>
        <div class="field-label">Create a role</div>
        <input class="text-input" bind:value={app.newRoleName} placeholder="Role name (e.g. Moderator)" />
        <div class="color-row">
          {#each ROLE_COLORS as c (c)}
            <button class="color-dot" class:on={app.newRoleColor === c} style="background:{c}" aria-label="color {c}" onclick={() => (app.newRoleColor = c)}></button>
          {/each}
        </div>
        <div class="cap-chips">
          {#each CAPS as c (c)}
            <button type="button" class="cap-chip" class:on={app.newRoleCaps.includes(c)} onclick={() => app.toggleNewRoleCap(c)}>{c}</button>
          {/each}
        </div>
        <div class="modal-actions"><button class="ok-btn" disabled={!app.newRoleName.trim() || !app.newRoleCaps.length} onclick={app.createRole}>Create role</button></div>
      {:else if app.nsTab === "members"}
        <h1>Members &amp; roles</h1>
        <p class="so-sub">Assign a role (grants its token bundle) or delegate individual capabilities.</p>
        <div class="field-label">Account</div>
        <input class="text-input" bind:value={app.nsDelegSubject} placeholder="account or account@network (federated)" />
        <div class="section-sep"></div>
        <div class="field-label">Assign a role</div>
        <div class="role-pick">
          {#each app.rolesByScope[app.nsRoleScope()] ?? [] as r (r.name)}
            <button class="role-pill clickable" style="--role:{r.color}" onclick={() => app.assignRole(r.name)}><span class="role-dot"></span>{r.name}</button>
          {:else}
            <div class="empty-hint">No roles defined — create some in the Roles tab.</div>
          {/each}
        </div>
        <div class="section-sep"></div>
        <div class="field-label">Or delegate individual capabilities</div>
        <div class="cap-chips">
          {#each CAPS as c (c)}
            <button type="button" class="cap-chip" class:on={app.nsDelegCaps.includes(c)} onclick={() => app.toggleDelegCap(c)}>{c}</button>
          {/each}
        </div>
        <div class="modal-actions"><button class="ok-btn" onclick={app.doDelegate}>Grant capabilities</button></div>
      {:else if app.nsTab === "bans"}
        <h1>Bans &amp; mutes</h1>
        <p class="so-sub">Accounts denied at <code>ns:{app.activeServer}</code> (§6.7). A <b>ban</b> blocks join + posting; a <b>mute</b> blocks posting. Lifting one takes effect immediately.</p>
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
        <p class="so-sub">Bridge <b>{app.activeServer}</b>'s channels to a peer network (§11). You control this as the namespace owner — bridges are scoped to <code>ns:{app.activeServer}</code>, non-transitive, and every change notifies members.</p>

        <div class="field-label">Auto-federation (§11.10)</div>
        <p class="so-sub">When open, another network can reach this namespace on demand — a user there references <code>{app.network}/{app.activeServer}</code> and their server auto-establishes the bridge. Requires <b>public</b> visibility.</p>
        <label class="fed-check" style="margin-bottom:14px">
          <input
            type="checkbox"
            checked={app.activeNsMeta?.federation ?? false}
            disabled={(app.activeNsMeta?.visibility ?? "") !== "public"}
            onchange={(e) => app.nsSetFederation(e.currentTarget.checked)}
          />
          Open <b>{app.activeServer}</b> to auto-federation
        </label>
        {#if (app.activeNsMeta?.visibility ?? "") !== "public"}
          <p class="so-sub" style="color:var(--amber)">Make this namespace public (Overview → Visibility) to enable auto-federation.</p>
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
          <select bind:value={brHistory}>
            <option value="from-epoch">history: from-epoch</option>
            <option value="full">history: full</option>
          </select>
          <select bind:value={brMedia}>
            <option value="none">media: none</option>
            <option value="mirror">media: mirror</option>
          </select>
          <label class="fed-check"><input type="checkbox" bind:checked={brTyping} /> typing</label>
          <button class="ok-btn" onclick={proposeBridge}>Propose</button>
        </div>
        <p class="so-sub" style="margin-top:14px">Outbound bridge transmission needs the M5d dialer; inbound peering, accept, and sever work today. Network-wide defederation (blocking a peer network entirely) is a network-operator action.</p>
      {:else if app.nsTab === "recovery"}
        <h1>Recovery quorum</h1>
        <p class="so-sub">§2.4 M-of-N root recovery. Share your recovery key, or co-sign and submit a rotation.</p>
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
</div>
