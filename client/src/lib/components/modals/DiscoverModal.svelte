<script lang="ts">
  // "Where do you want to go?" — the namespace entry point, built as a four-step
  // card (choose → join / invite / create) per design/namespace.html.
  //
  // The design's three options cover the app's four actions by folding federation
  // into search: a query containing "/" is read as `network/namespace` and routes
  // through FEDERATE (§11.10) instead of a local join.
  import { fade } from "svelte/transition";
  import { getApp } from "$lib/context";
  import * as weft from "$lib/weft";
  const app = getApp();
  let { onclose }: { onclose: () => void } = $props();

  type Step = "choose" | "join" | "invite" | "create";
  let step = $state<Step>("choose");

  let query = $state("");
  let redeemInput = $state("");
  let redeemError = $state("");
  let createName = $state("");
  let createVis = $state("public");

  const NAME_MAX = 32;

  // A namespace has no avatar of its own, so derive initials from the name —
  // the same "auto-generated from name" rule the create step previews.
  function initials(name: string): string {
    const words = name.trim().split(/[\s_-]+/).filter(Boolean);
    if (!words.length) return "";
    if (words.length > 1) return words.slice(0, 2).map((w) => w[0]).join("").toUpperCase();
    return words[0].slice(0, 2).toUpperCase();
  }

  // ---- join ----
  const listed = $derived(Object.values(app.discovered));
  const matches = $derived(
    query.trim()
      ? listed.filter((ns) => {
          const q = query.trim().toLowerCase();
          return ns.name.toLowerCase().includes(q) || (ns.title ?? "").toLowerCase().includes(q);
        })
      : listed,
  );
  // `network/namespace` in the search box means "somewhere else" — offer to bridge.
  const foreign = $derived.by(() => {
    const t = query.trim().replace(/^weft:\/\//, "");
    const m = t.match(/^([^/\s]+)\/([^/\s]+)$/);
    return m && m[1] !== app.network ? t : null;
  });
  // A namespace can be unlisted but still joinable by exact name (§2.2).
  const directName = $derived.by(() => {
    const n = query.trim().replace(/^@/, "");
    if (!n || foreign || n.includes("/") || matches.some((ns) => ns.name === n)) return null;
    return n;
  });

  function joinNamespace(name: string) {
    weft.nsJoin(name).catch(() => {});
    weft.channels(name).catch(() => {}); // fetch its category layout
    onclose();
  }
  function connectForeign() {
    if (!foreign) return;
    app.federate(foreign);
    onclose();
  }

  // ---- invite ----
  function doRedeem() {
    const t = redeemInput.trim();
    if (!t) return;
    // A foreign invite link (weft://<net>/<ns>/i/<id>) routes into federation —
    // your server auto-bridges to the namespace it points at (§11.10).
    const m = t.match(/^weft:\/\/([^/]+)\/(?:([^/]+)\/)?i\/.+$/);
    if (m && m[1] !== app.network) {
      if (!m[2]) {
        redeemError = "This invite is for another network but names no namespace.";
        return;
      }
      app.federate(`${m[1]}/${m[2]}`);
    } else {
      weft.inviteRedeem(t).catch((e) => app.toast(String(e), "error"));
    }
    redeemInput = "";
    redeemError = "";
    onclose();
  }

  // ---- create ----
  async function createNamespace() {
    const name = createName.trim().replace(/^@/, "");
    if (!name) return;
    createName = "";
    try {
      // Root keypair is generated + stored on-device; then a default channel.
      await weft.nsCreate(app.network, name, createVis);
      await weft.channelCreate(`#${name}/general`);
      await weft.join(`#${name}/general`);
      onclose();
    } catch (e) {
      app.toast(String(e), "error");
    }
  }

  const VIS = [
    { id: "public", label: "🌐 Public", desc: "Anyone can search & join" },
    { id: "unlisted", label: "🔗 Unlisted", desc: "Joinable by exact name" },
    { id: "private", label: "🔒 Private", desc: "Invite only" },
  ];
</script>

<div class="ns-wrap" transition:fade|global={{ duration: 190 }}>
  <button class="ns-backdrop" aria-label="Close" onclick={onclose}></button>
  <div class="ns-portal" role="dialog" aria-modal="true">
    <button class="ns-x" aria-label="Close" onclick={onclose}>✕</button>

    {#if step === "choose"}
      <div class="ns-logo" aria-hidden="true">
        <svg width="22" height="22" viewBox="0 0 18 18" fill="none">
          <path d="M9 2l1.5 4.5L15 8l-4.5 1.5L9 14l-1.5-4.5L3 8l4.5-1.5L9 2z" fill="white" />
        </svg>
      </div>
      <h1>Where do you want to go?</h1>
      <p class="ns-subtitle">
        Join a community, accept an invite, or build your own space from the ground up.
      </p>
      <div class="ns-options">
        <button class="opt join" onclick={() => (step = "join")}>
          <span class="opt-icon">
            <svg width="24" height="24" viewBox="0 0 32 32" fill="none" stroke="currentColor" stroke-width="2">
              <circle cx="16" cy="16" r="12" /><ellipse cx="16" cy="16" rx="5" ry="12" />
              <path d="M4 16h24" stroke-linecap="round" />
              <path d="M6 10h20M6 22h20" stroke-width="1.5" stroke-linecap="round" />
            </svg>
          </span>
          <span class="opt-text">
            <span class="opt-label">Join a Namespace</span>
            <span class="opt-desc">Browse public namespaces, or reach one on another network.</span>
          </span>
          <svg class="chevron" width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
            <path d="M6 3l5 5-5 5" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" />
          </svg>
        </button>

        <button class="opt invite" onclick={() => (step = "invite")}>
          <span class="opt-icon">
            <svg width="24" height="24" viewBox="0 0 32 32" fill="none" stroke="currentColor" stroke-width="2">
              <path d="M13.5 18.5l5-5" stroke-linecap="round" />
              <path d="M10 21a5 5 0 010-7.07l2.12-2.12a5 5 0 017.07 7.07L17.07 21" stroke-linecap="round" stroke-linejoin="round" />
              <path d="M22 11a5 5 0 010 7.07l-2.12 2.12a5 5 0 01-7.07-7.07L14.93 11" stroke-linecap="round" stroke-linejoin="round" />
            </svg>
          </span>
          <span class="opt-text">
            <span class="opt-label">Redeem Invite Link</span>
            <span class="opt-desc">Have an invite link? Paste it here to jump into a private namespace.</span>
          </span>
          <svg class="chevron" width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
            <path d="M6 3l5 5-5 5" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" />
          </svg>
        </button>

        <button class="opt create" onclick={() => (step = "create")}>
          <span class="opt-icon">
            <svg width="24" height="24" viewBox="0 0 32 32" fill="none" stroke="currentColor" stroke-width="2">
              <circle cx="16" cy="16" r="12" /><path d="M16 10v12M10 16h12" stroke-linecap="round" />
            </svg>
          </span>
          <span class="opt-text">
            <span class="opt-label">Create a Namespace</span>
            <span class="opt-desc">Start your own community — set the name and access rules.</span>
          </span>
          <svg class="chevron" width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true">
            <path d="M6 3l5 5-5 5" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" />
          </svg>
        </button>
      </div>

    {:else if step === "join"}
      <button class="back" onclick={() => (step = "choose")}>
        <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true"><path d="M10 3L5 8l5 5" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" /></svg>
        Back
      </button>
      <div class="step-head">
        <span class="step-icon join" aria-hidden="true">
          <svg width="20" height="20" viewBox="0 0 32 32" fill="none" stroke="currentColor" stroke-width="2">
            <circle cx="16" cy="16" r="12" /><ellipse cx="16" cy="16" rx="5" ry="12" />
            <path d="M4 16h24" stroke-linecap="round" />
          </svg>
        </span>
        <h2>Join a Namespace</h2>
      </div>
      <p class="step-desc">
        Search public namespaces and hit Join. To reach another network, type
        <span class="mono accent-join">network/namespace</span>.
      </p>

      <div class="input-wrap join-focus">
        <svg width="14" height="14" viewBox="0 0 16 16" fill="none" class="input-icon" aria-hidden="true">
          <circle cx="7" cy="7" r="4.5" stroke="currentColor" stroke-width="1.6" />
          <path d="M10.5 10.5l3 3" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" />
        </svg>
        <!-- svelte-ignore a11y_autofocus -->
        <input
          autofocus
          bind:value={query}
          placeholder="Search by name…"
          onkeydown={(e) => {
            if (e.key !== "Enter") return;
            if (foreign) connectForeign();
            else if (directName) joinNamespace(directName);
          }}
        />
      </div>

      <div class="ns-list">
        {#if foreign}
          <div class="ns-item foreign">
            <div class="ns-badge">↗</div>
            <div class="ns-info">
              <div class="ns-name mono">{foreign}</div>
              <div class="ns-sub">On another network — your server will bridge to it.</div>
            </div>
            <button class="go-btn" onclick={connectForeign}>Connect</button>
          </div>
        {/if}

        {#if directName}
          <div class="ns-item">
            <div class="ns-badge">{initials(directName)}</div>
            <div class="ns-info">
              <div class="ns-name">@{directName}</div>
              <div class="ns-sub">Not listed — join by exact name.</div>
            </div>
            <button class="go-btn" onclick={() => joinNamespace(directName)}>Join</button>
          </div>
        {/if}

        {#each matches as ns (ns.name)}
          <div class="ns-item">
            <div class="ns-badge">{initials(ns.title || ns.name)}</div>
            <div class="ns-info">
              <div class="ns-name">{ns.title || ns.name}</div>
              <div class="ns-sub">
                {ns.description || `@${ns.name}`} · {ns.visibility}{ns.owner ? ` · ${ns.owner}` : ""}
              </div>
            </div>
            <button class="go-btn" onclick={() => joinNamespace(ns.name)}>Join</button>
          </div>
        {:else}
          {#if !foreign && !directName}
            <div class="ns-empty">
              {query.trim() ? "No namespaces found." : "No public namespaces yet."}
            </div>
          {/if}
        {/each}

        {#if app.discoverCursor && !query.trim()}
          <button class="load-more" onclick={() => weft.discover(app.discoverCursor ?? undefined)}>
            Load more…
          </button>
        {/if}
      </div>

    {:else if step === "invite"}
      <button class="back" onclick={() => (step = "choose")}>
        <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true"><path d="M10 3L5 8l5 5" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" /></svg>
        Back
      </button>
      <div class="step-head">
        <span class="step-icon invite" aria-hidden="true">
          <svg width="20" height="20" viewBox="0 0 32 32" fill="none" stroke="currentColor" stroke-width="2">
            <path d="M13.5 18.5l5-5" stroke-linecap="round" />
            <path d="M10 21a5 5 0 010-7.07l2.12-2.12a5 5 0 017.07 7.07L17.07 21" stroke-linecap="round" stroke-linejoin="round" />
            <path d="M22 11a5 5 0 010 7.07l-2.12 2.12a5 5 0 01-7.07-7.07L14.93 11" stroke-linecap="round" stroke-linejoin="round" />
          </svg>
        </span>
        <h2>Redeem Invite Link</h2>
      </div>
      <p class="step-desc">
        Paste an invite link below — it looks like
        <span class="mono accent-invite">weft://network/ns/i/AbCdEf</span> — and click Redeem to join.
      </p>

      <div class="input-wrap invite-focus" class:bad={redeemError}>
        <!-- svelte-ignore a11y_autofocus -->
        <input
          autofocus
          class="mono"
          bind:value={redeemInput}
          placeholder="weft://network/ns/i/AbCdEf"
          oninput={() => (redeemError = "")}
          onkeydown={(e) => e.key === "Enter" && doRedeem()}
        />
      </div>
      {#if redeemError}<p class="err-msg">{redeemError}</p>{/if}

      <button class="cta invite" disabled={!redeemInput.trim()} onclick={doRedeem}>Redeem Invite</button>

    {:else}
      <button class="back" onclick={() => (step = "choose")}>
        <svg width="14" height="14" viewBox="0 0 16 16" fill="none" aria-hidden="true"><path d="M10 3L5 8l5 5" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" /></svg>
        Back
      </button>
      <div class="step-head">
        <span class="step-icon create" aria-hidden="true">
          <svg width="20" height="20" viewBox="0 0 32 32" fill="none" stroke="currentColor" stroke-width="2">
            <circle cx="16" cy="16" r="12" /><path d="M16 10v12M10 16h12" stroke-linecap="round" />
          </svg>
        </span>
        <h2>Create a Namespace</h2>
      </div>
      <p class="step-desc">
        Give your namespace a name and choose who can find it. You can change everything later.
      </p>

      <div class="icon-row">
        <div class="icon-preview" class:filled={createName.trim()}>
          {#if createName.trim()}
            {initials(createName)}
          {:else}
            <svg width="22" height="22" viewBox="0 0 32 32" fill="none" stroke="currentColor" stroke-width="2">
              <circle cx="16" cy="16" r="12" /><path d="M16 10v12M10 16h12" stroke-linecap="round" />
            </svg>
          {/if}
        </div>
        <div>
          <div class="icon-hint">Namespace Icon</div>
          <div class="icon-sub">Auto-generated from the name.<br />Upload a custom icon later.</div>
        </div>
      </div>

      <label class="field-lbl" for="ns-new-name">Namespace Name</label>
      <div class="input-wrap create-focus">
        <!-- svelte-ignore a11y_autofocus -->
        <input
          id="ns-new-name"
          autofocus
          bind:value={createName}
          maxlength={NAME_MAX}
          placeholder="my-community"
          onkeydown={(e) => e.key === "Enter" && createNamespace()}
        />
        <span class="char-count">{createName.length}/{NAME_MAX}</span>
      </div>

      <div class="field-lbl">Access Type</div>
      <div class="access-grid">
        {#each VIS as v (v.id)}
          <button
            class="access-btn"
            class:sel={createVis === v.id}
            aria-pressed={createVis === v.id}
            onclick={() => (createVis = v.id)}
          >
            <span class="access-name">{v.label}</span>
            <span class="access-desc">{v.desc}</span>
          </button>
        {/each}
      </div>

      <button class="cta create" disabled={!createName.trim()} onclick={createNamespace}>
        Create Namespace
      </button>
    {/if}
  </div>
</div>

<style>
  /* Per-option accents from the design — semantic, so they stay literal while
     surfaces and text follow the app's theme variables. */
  .ns-portal {
    --join: #5865f2;
    --invite: #00b0f4;
    --create: #23a55a;
  }

  .ns-wrap {
    position: fixed;
    inset: 0;
    z-index: 60;
    display: grid;
    place-items: center;
    padding: 48px 16px;
  }
  .ns-backdrop {
    position: absolute;
    inset: 0;
    border: none;
    padding: 0;
    cursor: default;
    background: rgba(0, 0, 0, 0.6);
    /* The design's overhead glow, layered on the scrim. */
    background-image: radial-gradient(ellipse 60% 40% at 50% 20%, rgba(88, 101, 242, 0.12) 0%, transparent 70%);
  }
  .ns-portal {
    position: relative;
    width: 100%;
    max-width: 380px;
    max-height: min(86vh, 780px);
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    padding: 32px 28px;
    border-radius: 20px;
    background: var(--bg-panel-raised);
    border: 1.5px solid var(--border-hair-strong);
    box-shadow: 0 32px 80px rgba(0, 0, 0, 0.45);
  }
  .ns-x {
    position: absolute;
    top: 12px;
    right: 14px;
    border: none;
    background: none;
    color: var(--text-faint);
    font-size: 13px;
    cursor: pointer;
    padding: 4px;
    line-height: 1;
  }
  .ns-x:hover {
    color: var(--text-primary);
  }

  /* ---- choose ---- */
  .ns-logo {
    width: 56px;
    height: 56px;
    border-radius: 16px;
    background: linear-gradient(135deg, #5865f2 0%, #7c3aed 100%);
    display: grid;
    place-items: center;
    margin: 0 auto 20px;
  }
  h1 {
    font-size: 22px;
    font-weight: 700;
    letter-spacing: -0.3px;
    text-align: center;
    color: var(--text-primary);
    margin: 0 0 6px;
  }
  .ns-subtitle {
    font-size: 13px;
    color: var(--text-muted);
    text-align: center;
    line-height: 1.6;
    margin: 0 0 28px;
  }
  .ns-options {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .opt {
    display: flex;
    align-items: center;
    gap: 14px;
    width: 100%;
    padding: 14px 18px;
    border-radius: 14px;
    border: 1.5px solid var(--border-hair-strong);
    background: var(--bg-panel);
    color: inherit;
    font: inherit;
    text-align: left;
    cursor: pointer;
    transition: background 0.15s, border-color 0.15s, transform 0.15s;
  }
  .opt:hover {
    transform: translateX(3px);
  }
  .opt.join:hover {
    border-color: var(--join);
    background: color-mix(in srgb, var(--join) 12%, transparent);
  }
  .opt.invite:hover {
    border-color: var(--invite);
    background: color-mix(in srgb, var(--invite) 12%, transparent);
  }
  .opt.create:hover {
    border-color: var(--create);
    background: color-mix(in srgb, var(--create) 12%, transparent);
  }
  .opt-icon {
    width: 44px;
    height: 44px;
    flex: none;
    border-radius: 12px;
    display: grid;
    place-items: center;
    background: var(--bg-hover);
    color: var(--text-muted);
    transition: background 0.15s, color 0.15s;
  }
  .opt.join:hover .opt-icon,
  .opt.join:hover .opt-label,
  .opt.join:hover .chevron {
    color: var(--join);
  }
  .opt.invite:hover .opt-icon,
  .opt.invite:hover .opt-label,
  .opt.invite:hover .chevron {
    color: var(--invite);
  }
  .opt.create:hover .opt-icon,
  .opt.create:hover .opt-label,
  .opt.create:hover .chevron {
    color: var(--create);
  }
  .opt-text {
    flex: 1;
    min-width: 0;
  }
  .opt-label {
    display: block;
    font-size: 14px;
    font-weight: 600;
    color: var(--text-primary);
    margin-bottom: 2px;
    transition: color 0.15s;
  }
  .opt-desc {
    display: block;
    font-size: 12px;
    color: var(--text-muted);
    line-height: 1.5;
  }
  .chevron {
    flex: none;
    color: var(--text-faint);
    transition: color 0.15s;
  }

  /* ---- sub-steps ---- */
  .back {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    align-self: flex-start;
    padding: 0;
    margin-bottom: 22px;
    border: none;
    background: none;
    font: inherit;
    font-size: 12px;
    color: var(--text-muted);
    cursor: pointer;
  }
  .back:hover {
    color: var(--text-primary);
  }
  .step-head {
    display: flex;
    align-items: center;
    gap: 12px;
    margin-bottom: 6px;
  }
  .step-icon {
    width: 36px;
    height: 36px;
    flex: none;
    border-radius: 10px;
    display: grid;
    place-items: center;
  }
  .step-icon.join {
    color: var(--join);
    background: color-mix(in srgb, var(--join) 15%, transparent);
  }
  .step-icon.invite {
    color: var(--invite);
    background: color-mix(in srgb, var(--invite) 12%, transparent);
  }
  .step-icon.create {
    color: var(--create);
    background: color-mix(in srgb, var(--create) 12%, transparent);
  }
  h2 {
    font-size: 18px;
    font-weight: 700;
    color: var(--text-primary);
    margin: 0;
  }
  .step-desc {
    font-size: 12px;
    color: var(--text-muted);
    line-height: 1.6;
    margin: 0 0 20px;
  }
  .mono {
    font-family: var(--font-mono);
  }
  .accent-join {
    color: var(--join);
  }
  .accent-invite {
    color: var(--invite);
  }

  .input-wrap {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 11px 14px;
    border-radius: 12px;
    background: var(--bg-panel);
    border: 1.5px solid var(--border-hair-strong);
    transition: border-color 0.15s;
  }
  .input-wrap.join-focus:focus-within {
    border-color: var(--join);
  }
  .input-wrap.invite-focus:focus-within {
    border-color: var(--invite);
  }
  .input-wrap.create-focus:focus-within {
    border-color: var(--create);
  }
  .input-wrap.bad {
    border-color: #f23f43;
  }
  .input-wrap input {
    flex: 1;
    min-width: 0;
    border: none;
    outline: none;
    background: none;
    font: inherit;
    font-size: 14px;
    color: var(--text-primary);
  }
  .input-icon {
    flex: none;
    color: var(--text-muted);
  }
  .char-count {
    flex: none;
    font-size: 12px;
    color: var(--text-faint);
  }
  .err-msg {
    margin: 10px 0 0;
    font-size: 12px;
    color: #f23f43;
  }

  .cta {
    width: 100%;
    margin-top: 16px;
    padding: 13px;
    border: none;
    border-radius: 12px;
    font: inherit;
    font-size: 14px;
    font-weight: 600;
    color: #fff;
    cursor: pointer;
  }
  .cta.invite {
    background: var(--invite);
  }
  .cta.create {
    background: var(--create);
  }
  .cta:hover:not(:disabled) {
    filter: brightness(1.1);
  }
  .cta:disabled {
    background: var(--bg-panel);
    color: var(--text-faint);
    cursor: not-allowed;
  }

  /* ---- join list ---- */
  .ns-list {
    display: flex;
    flex-direction: column;
    gap: 8px;
    margin-top: 14px;
  }
  .ns-item {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 10px 14px;
    border-radius: 12px;
    background: var(--bg-panel);
    border: 1.5px solid var(--border-hair);
    transition: border-color 0.12s;
  }
  .ns-item:hover {
    border-color: color-mix(in srgb, var(--join) 40%, transparent);
  }
  .ns-item.foreign {
    border-color: color-mix(in srgb, var(--invite) 35%, transparent);
  }
  .ns-badge {
    width: 36px;
    height: 36px;
    flex: none;
    border-radius: 10px;
    display: grid;
    place-items: center;
    background: var(--bg-hover);
    color: var(--text-secondary);
    font-size: 13px;
    font-weight: 700;
  }
  .ns-item.foreign .ns-badge {
    color: var(--invite);
  }
  .ns-info {
    flex: 1;
    min-width: 0;
  }
  .ns-name {
    font-size: 13px;
    font-weight: 600;
    color: var(--text-primary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .ns-sub {
    font-size: 11px;
    color: var(--text-muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .go-btn {
    flex: none;
    padding: 6px 12px;
    border: none;
    border-radius: 8px;
    font: inherit;
    font-size: 12px;
    font-weight: 600;
    color: var(--join);
    background: color-mix(in srgb, var(--join) 15%, transparent);
    cursor: pointer;
    transition: background 0.12s, color 0.12s;
  }
  .go-btn:hover {
    background: var(--join);
    color: #fff;
  }
  .ns-item.foreign .go-btn {
    color: var(--invite);
    background: color-mix(in srgb, var(--invite) 15%, transparent);
  }
  .ns-item.foreign .go-btn:hover {
    background: var(--invite);
    color: #fff;
  }
  .ns-empty {
    padding: 24px 0;
    text-align: center;
    font-size: 13px;
    color: var(--text-faint);
  }
  .load-more {
    border: none;
    background: none;
    font: inherit;
    font-size: 12px;
    color: var(--text-muted);
    cursor: pointer;
    padding: 6px;
  }
  .load-more:hover {
    color: var(--text-primary);
  }

  /* ---- create ---- */
  .icon-row {
    display: flex;
    align-items: center;
    gap: 14px;
    margin-bottom: 20px;
  }
  .icon-preview {
    width: 60px;
    height: 60px;
    flex: none;
    border-radius: 16px;
    display: grid;
    place-items: center;
    background: var(--bg-hover);
    color: var(--text-faint);
    font-size: 20px;
    font-weight: 700;
    transition: background 0.2s, color 0.2s;
  }
  .icon-preview.filled {
    background: linear-gradient(135deg, #23a55a 0%, #1a7d44 100%);
    color: #fff;
  }
  .icon-hint {
    font-size: 13px;
    font-weight: 600;
    color: var(--text-primary);
    margin-bottom: 2px;
  }
  .icon-sub {
    font-size: 12px;
    color: var(--text-muted);
    line-height: 1.4;
  }
  .field-lbl {
    display: block;
    margin-bottom: 6px;
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--text-muted);
  }
  .access-grid {
    display: grid;
    grid-template-columns: repeat(3, 1fr);
    gap: 8px;
  }
  .access-btn {
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 12px 10px;
    border-radius: 12px;
    border: 1.5px solid var(--border-hair-strong);
    background: var(--bg-panel);
    font: inherit;
    text-align: left;
    cursor: pointer;
    transition: background 0.12s, border-color 0.12s;
  }
  .access-btn.sel {
    border-color: var(--create);
    background: color-mix(in srgb, var(--create) 12%, transparent);
  }
  .access-name {
    font-size: 12px;
    font-weight: 600;
    color: var(--text-muted);
    transition: color 0.12s;
  }
  .access-btn.sel .access-name {
    color: var(--create);
  }
  .access-desc {
    font-size: 11px;
    color: var(--text-muted);
    line-height: 1.35;
  }

  /* The create step's fields need breathing room between groups. */
  .input-wrap.create-focus {
    margin-bottom: 18px;
  }
</style>
