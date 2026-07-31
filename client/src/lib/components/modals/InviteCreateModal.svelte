<script lang="ts">
  import { vm } from "$lib/viewmodel.svelte";
  import { revokeInvite, generateInvite, sendInviteDM } from "$lib/models/invites.svelte";
  import { roster, friendLocalAccount } from "$lib/models/social.svelte";
  import { store } from "$lib/models/store.svelte";
  import { friendLabel, initials } from "$lib/profile.svelte";
  import { fade } from "svelte/transition";
  import { getApp } from "$lib/context";
  import Avatar from "$lib/components/Avatar.svelte";
  const app = getApp();
  let { onclose }: { onclose: () => void } = $props();

  type Tab = "link" | "friends";
  let tab = $state<Tab>("link");

  // Expiration + max-uses choices. `null` = unlimited (never / no limit) — the
  // defaults, so a freshly-created invite is unlimited in both time and uses.
  const EXPIRY_OPTS: { label: string; secs: number | null }[] = [
    { label: "30 min", secs: 1800 },
    { label: "1 hour", secs: 3600 },
    { label: "6 hours", secs: 21600 },
    { label: "1 day", secs: 86400 },
    { label: "7 days", secs: 604800 },
    { label: "Never", secs: null },
  ];
  const USES_OPTS: { label: string; n: number | null }[] = [
    { label: "1", n: 1 },
    { label: "5", n: 5 },
    { label: "10", n: 10 },
    { label: "25", n: 25 },
    { label: "50", n: 50 },
    { label: "No limit", n: null },
  ];
  let expiry = $state<number | null>(null); // seconds; null = never
  let maxUses = $state<number | null>(null); // null = unlimited

  let copied = $state(false);
  function copy() {
    const link = store.invites.link;
    if (!link) return;
    navigator.clipboard?.writeText(link).then(
      () => {
        copied = true;
        setTimeout(() => (copied = false), 1800);
        app.toast("Invite link copied", "info");
      },
      () => {},
    );
  }

  function generate() {
    generateInvite(maxUses, expiry);
  }
  function revoke() {
    if (store.invites.id) revokeInvite(store.invites.id);
  }

  // The scope this invite grants access to, in a friendly form.
  const scopeLabel = $derived(
    store.invites.createScope.startsWith("ns:")
      ? store.invites.createScope.slice(3)
      : store.invites.createScope || store.session.network,
  );
  const expiryLabel = $derived(EXPIRY_OPTS.find((o) => o.secs === expiry)?.label ?? "Never");
  const usesLabel = $derived(
    maxUses === null ? "No use limit" : `${maxUses} use${maxUses === 1 ? "" : "s"} max`,
  );

  // ---- Friends tab ----
  let search = $state("");
  let selected = $state<Set<string>>(new Set());
  // Presence is keyed by the friend's local account; federated friends have no
  // local account and can't be DM'd, so the send list is local friends only.
  const statusOf = (ref: string) => {
    const acct = friendLocalAccount(ref);
    return acct ? (store.accountOf(acct).presence ?? "offline") : "offline";
  };
  const isOnline = (ref: string) => statusOf(ref) !== "offline" && statusOf(ref) !== "invisible";
  const friends = $derived(
    roster.friends.filter((r) => {
      if (!friendLocalAccount(r)) return false;
      const q = search.trim().toLowerCase();
      return !q || friendLabel(r).toLowerCase().includes(q) || r.toLowerCase().includes(q);
    }),
  );
  const onlineFriends = $derived(friends.filter(isOnline));
  const offlineFriends = $derived(friends.filter((r) => !isOnline(r)));
  function toggle(ref: string) {
    const next = new Set(selected);
    next.has(ref) ? next.delete(ref) : next.add(ref);
    selected = next;
  }
  function send() {
    const link = store.invites.link;
    if (!link || !selected.size) return;
    for (const ref of selected) sendInviteDM(ref, link);
    app.toast(`Invite sent to ${selected.size} friend${selected.size === 1 ? "" : "s"}`, "info");
    selected = new Set();
  }
</script>

<div class="ic-wrap" transition:fade|global={{ duration: 190 }}>
  <button class="ic-backdrop" aria-label="Close" onclick={onclose}></button>
  <div class="ic-modal" role="dialog" aria-modal="true">
    <!-- Header -->
    <div class="ic-head">
      <div class="ic-server">
        <div class="ic-icon">{initials(scopeLabel)}</div>
        <div class="ic-meta">
          <div class="ic-name">{vm.activeNsMeta?.title || scopeLabel}</div>
          <div class="ic-scope">on {store.session.network}</div>
        </div>
      </div>
      <div class="ic-title">Invite to server</div>
      <div class="ic-sub">Generate a link — or send it straight to a friend.</div>
      <button class="ic-close" aria-label="Close" onclick={onclose}>✕</button>
    </div>

    <!-- Tabs -->
    <div class="ic-tabs">
      <button class="ic-tab" class:active={tab === "link"} onclick={() => (tab = "link")}>Invite link</button>
      <button class="ic-tab" class:active={tab === "friends"} onclick={() => (tab = "friends")}>Friends</button>
    </div>

    {#if tab === "link"}
      <div class="ic-body">
        <div class="ic-field-label">Your invite link</div>
        <div class="ic-linkbox" class:empty={!store.invites.link}>
          <div class="ic-linkinfo">
            <div class="ic-linkcode">{store.invites.link ?? "Choose options, then generate a link"}</div>
            <div class="ic-linkexpire">Expires: {expiryLabel} · {usesLabel}</div>
          </div>
          <button class="ic-copy" class:copied disabled={!store.invites.link} onclick={copy}>
            {copied ? "Copied" : "Copy"}
          </button>
        </div>

        <div class="ic-field-label">Expire after</div>
        <div class="ic-opts">
          {#each EXPIRY_OPTS as o (o.label)}
            <button class="ic-opt" class:sel={expiry === o.secs} onclick={() => (expiry = o.secs)}>{o.label}</button>
          {/each}
        </div>

        <div class="ic-field-label">Max number of uses</div>
        <div class="ic-opts">
          {#each USES_OPTS as o (o.label)}
            <button class="ic-opt" class:sel={maxUses === o.n} onclick={() => (maxUses = o.n)}>{o.label}</button>
          {/each}
        </div>

        <div class="ic-actions">
          {#if store.invites.id}
            <button class="ic-btn-ghost danger" onclick={revoke}>Revoke</button>
          {/if}
          <button class="ic-btn-primary" onclick={generate}>
            {store.invites.link ? "Generate new link" : "Generate invite link"}
          </button>
        </div>
      </div>
    {:else}
      <div class="ic-body">
        <div class="ic-search">
          <svg width="15" height="15" viewBox="0 0 16 16" fill="none"><circle cx="7" cy="7" r="4.5" stroke="currentColor" stroke-width="1.6" /><path d="M10.5 10.5l3 3" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" /></svg>
          <input placeholder="Search friends…" bind:value={search} />
        </div>

        {#if selected.size}
          <div class="ic-chips">
            {#each [...selected] as ref (ref)}
              <div class="ic-chip">
                {friendLabel(ref)}
                <button class="ic-chip-x" aria-label="Remove" onclick={() => toggle(ref)}>✕</button>
              </div>
            {/each}
          </div>
        {/if}

        <div class="ic-friends">
          {#snippet frow(ref: string)}
            <button class="ic-friend" class:sel={selected.has(ref)} onclick={() => toggle(ref)}>
              <div class="ic-fav"><Avatar account={friendLocalAccount(ref) ?? ref} /><span class="ic-fdot {statusOf(ref)}"></span></div>
              <div class="ic-finfo">
                <div class="ic-fname">{friendLabel(ref)}</div>
                <div class="ic-fsub">{statusOf(ref)}</div>
              </div>
              <span class="ic-check" class:on={selected.has(ref)}>
                {#if selected.has(ref)}<svg width="11" height="11" viewBox="0 0 12 12" fill="none"><polyline points="2 6 5 9 10 3" stroke="#fff" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" /></svg>{/if}
              </span>
            </button>
          {/snippet}
          {#if onlineFriends.length}
            <div class="ic-group">Online — {onlineFriends.length}</div>
            {#each onlineFriends as ref (ref)}{@render frow(ref)}{/each}
          {/if}
          {#if offlineFriends.length}
            <div class="ic-group">Offline — {offlineFriends.length}</div>
            {#each offlineFriends as ref (ref)}{@render frow(ref)}{/each}
          {/if}
          {#if !friends.length}
            <div class="ic-empty">No friends {search ? "match your search" : "yet"}.</div>
          {/if}
        </div>

        {#if !store.invites.link}
          <div class="ic-hint">Generate a link on the <b>Invite link</b> tab first, then send it here.</div>
        {/if}
        <div class="ic-actions">
          <button class="ic-btn-ghost" disabled={!selected.size} onclick={() => (selected = new Set())}>Clear</button>
          <button class="ic-btn-primary" disabled={!selected.size || !store.invites.link} onclick={send}>
            {selected.size ? `Send ${selected.size} invite${selected.size === 1 ? "" : "s"}` : "Send invite"}
          </button>
        </div>
      </div>
    {/if}
  </div>
</div>

<style>
  .ic-wrap {
    position: fixed;
    inset: 0;
    z-index: 600;
    display: grid;
    place-items: center;
  }
  .ic-backdrop {
    position: absolute;
    inset: 0;
    border: none;
    background: rgba(0, 0, 0, 0.55);
    cursor: default;
  }
  .ic-modal {
    position: relative;
    z-index: 1;
    width: 460px;
    max-width: calc(100vw - 32px);
    max-height: calc(100vh - 48px);
    display: flex;
    flex-direction: column;
    background: var(--bg-elevated, #14161e);
    border: 1px solid var(--border-hair-strong);
    border-radius: 14px;
    box-shadow: 0 24px 80px rgba(0, 0, 0, 0.6);
    overflow: hidden;
  }

  /* Header */
  .ic-head {
    position: relative;
    padding: 20px 20px 16px;
    border-bottom: 1px solid var(--border-hair);
    background: linear-gradient(180deg, color-mix(in srgb, var(--accent) 10%, transparent), transparent);
  }
  .ic-server {
    display: flex;
    align-items: center;
    gap: 12px;
    margin-bottom: 12px;
  }
  .ic-icon {
    width: 48px;
    height: 48px;
    border-radius: 12px;
    flex-shrink: 0;
    display: grid;
    place-items: center;
    font-size: 15px;
    font-weight: 800;
    color: #fff;
    background: linear-gradient(135deg, var(--accent, #5865f2), color-mix(in srgb, var(--accent) 55%, #000));
  }
  .ic-name {
    font-size: 15px;
    font-weight: 800;
    color: var(--text-primary);
  }
  .ic-scope {
    font-size: 12px;
    color: var(--text-muted);
    margin-top: 2px;
  }
  .ic-title {
    font-size: 11px;
    font-weight: 800;
    letter-spacing: 0.1em;
    text-transform: uppercase;
    color: var(--accent, #5865f2);
  }
  .ic-sub {
    font-size: 13px;
    color: var(--text-muted);
    margin-top: 3px;
  }
  .ic-close {
    position: absolute;
    top: 14px;
    right: 14px;
    border: none;
    background: none;
    color: var(--text-faint);
    font-size: 14px;
    cursor: pointer;
    padding: 4px;
    border-radius: 6px;
  }
  .ic-close:hover {
    color: var(--text-primary);
    background: var(--bg-hover);
  }

  /* Tabs */
  .ic-tabs {
    display: flex;
    gap: 20px;
    padding: 0 20px;
    border-bottom: 1px solid var(--border-hair);
  }
  .ic-tab {
    padding: 12px 0;
    border: none;
    background: none;
    font: inherit;
    font-size: 12px;
    font-weight: 700;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--text-muted);
    cursor: pointer;
    border-bottom: 2px solid transparent;
    margin-bottom: -1px;
  }
  .ic-tab:hover {
    color: var(--text-primary);
  }
  .ic-tab.active {
    color: var(--accent, #5865f2);
    border-bottom-color: var(--accent, #5865f2);
  }

  /* Body */
  .ic-body {
    padding: 18px 20px 20px;
    display: flex;
    flex-direction: column;
    gap: 14px;
    overflow-y: auto;
  }
  .ic-field-label {
    font-size: 10px;
    font-weight: 800;
    letter-spacing: 0.1em;
    text-transform: uppercase;
    color: var(--text-faint);
    margin-bottom: -6px;
  }

  /* Link box */
  .ic-linkbox {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 11px 12px;
    background: var(--bg-panel);
    border: 1px solid var(--border-hair);
    border-radius: 8px;
  }
  .ic-linkbox.empty .ic-linkcode {
    color: var(--text-faint);
    font-style: italic;
    font-family: inherit;
  }
  .ic-linkinfo {
    flex: 1;
    min-width: 0;
  }
  .ic-linkcode {
    font-size: 13px;
    font-family: var(--font-mono);
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .ic-linkexpire {
    font-size: 10px;
    font-weight: 600;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    color: var(--text-muted);
    margin-top: 3px;
  }
  .ic-copy {
    flex-shrink: 0;
    padding: 7px 14px;
    border: 1px solid var(--border-hair-strong);
    border-radius: 6px;
    background: var(--bg-panel-raised);
    color: var(--text-primary);
    font: inherit;
    font-size: 12px;
    font-weight: 700;
    cursor: pointer;
  }
  .ic-copy:hover:not(:disabled) {
    background: var(--accent, #5865f2);
    border-color: var(--accent, #5865f2);
    color: #fff;
  }
  .ic-copy:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
  .ic-copy.copied {
    color: #3ba55d;
    border-color: #3ba55d;
  }

  /* Option pills */
  .ic-opts {
    display: flex;
    flex-wrap: wrap;
    gap: 7px;
  }
  .ic-opt {
    padding: 6px 13px;
    border: 1px solid var(--border-hair);
    border-radius: 6px;
    background: var(--bg-panel);
    color: var(--text-muted);
    font: inherit;
    font-size: 12px;
    font-weight: 600;
    cursor: pointer;
  }
  .ic-opt:hover {
    color: var(--text-primary);
    border-color: var(--border-hair-strong);
  }
  .ic-opt.sel {
    background: color-mix(in srgb, var(--accent) 16%, transparent);
    border-color: color-mix(in srgb, var(--accent) 45%, transparent);
    color: var(--accent, #5865f2);
  }

  /* Actions */
  .ic-actions {
    display: flex;
    gap: 8px;
    margin-top: 2px;
  }
  .ic-btn-primary {
    flex: 1;
    padding: 10px;
    border: none;
    border-radius: 8px;
    background: var(--accent, #5865f2);
    color: #fff;
    font: inherit;
    font-size: 13px;
    font-weight: 700;
    cursor: pointer;
  }
  .ic-btn-primary:hover:not(:disabled) {
    background: color-mix(in srgb, var(--accent) 85%, #fff);
  }
  .ic-btn-primary:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
  .ic-btn-ghost {
    padding: 10px 16px;
    border: 1px solid var(--border-hair-strong);
    border-radius: 8px;
    background: transparent;
    color: var(--text-secondary);
    font: inherit;
    font-size: 13px;
    font-weight: 600;
    cursor: pointer;
  }
  .ic-btn-ghost:hover:not(:disabled) {
    color: var(--text-primary);
  }
  .ic-btn-ghost:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
  .ic-btn-ghost.danger {
    color: var(--danger, #e06c6c);
    border-color: color-mix(in srgb, var(--danger) 40%, transparent);
  }
  .ic-btn-ghost.danger:hover {
    background: color-mix(in srgb, var(--danger) 12%, transparent);
  }

  /* Search */
  .ic-search {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 9px 12px;
    background: var(--bg-panel);
    border: 1px solid var(--border-hair);
    border-radius: 8px;
    color: var(--text-muted);
  }
  .ic-search input {
    flex: 1;
    border: none;
    background: none;
    outline: none;
    font: inherit;
    font-size: 13px;
    color: var(--text-primary);
  }

  /* Chips */
  .ic-chips {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }
  .ic-chip {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 4px 6px 4px 10px;
    border-radius: 20px;
    font-size: 12px;
    font-weight: 600;
    color: var(--accent, #5865f2);
    background: color-mix(in srgb, var(--accent) 14%, transparent);
    border: 1px solid color-mix(in srgb, var(--accent) 30%, transparent);
  }
  .ic-chip-x {
    border: none;
    background: none;
    color: inherit;
    cursor: pointer;
    font-size: 10px;
    opacity: 0.7;
    padding: 0 2px;
  }
  .ic-chip-x:hover {
    opacity: 1;
  }

  /* Friends list */
  .ic-friends {
    display: flex;
    flex-direction: column;
    gap: 1px;
    max-height: 260px;
    overflow-y: auto;
  }
  .ic-group {
    font-size: 9px;
    font-weight: 800;
    letter-spacing: 0.12em;
    text-transform: uppercase;
    color: var(--text-faint);
    padding: 10px 6px 4px;
  }
  .ic-friend {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 7px 8px;
    border: 1px solid transparent;
    border-radius: 8px;
    background: none;
    cursor: pointer;
    text-align: left;
  }
  .ic-friend:hover {
    background: var(--bg-hover);
  }
  .ic-friend.sel {
    background: color-mix(in srgb, var(--accent) 12%, transparent);
    border-color: color-mix(in srgb, var(--accent) 25%, transparent);
  }
  .ic-fav {
    position: relative;
    width: 34px;
    height: 34px;
    flex-shrink: 0;
  }
  .ic-fdot {
    position: absolute;
    right: -2px;
    bottom: -2px;
    width: 11px;
    height: 11px;
    border-radius: 50%;
    border: 2px solid var(--bg-elevated, #14161e);
    background: #6b7280;
  }
  .ic-fdot.online {
    background: #3ba55d;
  }
  .ic-fdot.idle,
  .ic-fdot.away {
    background: #faa61a;
  }
  .ic-fdot.dnd,
  .ic-fdot.busy {
    background: #ed4245;
  }
  .ic-finfo {
    flex: 1;
    min-width: 0;
  }
  .ic-fname {
    font-size: 13px;
    font-weight: 600;
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .ic-fsub {
    font-size: 11px;
    color: var(--text-muted);
    text-transform: capitalize;
  }
  .ic-check {
    width: 20px;
    height: 20px;
    flex-shrink: 0;
    border: 2px solid var(--border-hair-strong);
    border-radius: 5px;
    display: grid;
    place-items: center;
  }
  .ic-check.on {
    background: var(--accent, #5865f2);
    border-color: var(--accent, #5865f2);
  }
  .ic-empty,
  .ic-hint {
    font-size: 12px;
    color: var(--text-muted);
    text-align: center;
    padding: 10px 4px;
  }
  .ic-hint {
    background: var(--bg-panel);
    border-radius: 8px;
  }
</style>
