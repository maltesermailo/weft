<script lang="ts">
  import { untrack } from "svelte";
  import { fade, fly } from "svelte/transition";
  import { getApp } from "$lib/context";
  import * as weft from "$lib/weft";
  const app = getApp();
  let { onclose }: { onclose: () => void } = $props();

  // §10.3 profile editor — an in-progress *draft* the user commits or reverts
  // from the floating bar. `saved*` mirror the live account state, so a
  // successful save (which flows back as a PROFILE / PRESENCE event) clears the
  // dirty state on its own — no manual "saved" bookkeeping.
  const savedDisplay = $derived(
    app.displayName(app.account) === app.account ? "" : app.displayName(app.account),
  );
  const savedAbout = $derived(app.bioOf(app.account));
  const savedStatus = $derived(app.myStatus);
  const savedAvatarUrl = $derived(app.avatarUrl(app.account));

  // Seed the draft from the current values (initial snapshot — intentionally not
  // reactive; the draft then diverges until saved/reverted).
  let dName = $state(untrack(() => savedDisplay));
  let dAbout = $state(untrack(() => savedAbout));
  let dStatus = $state(untrack(() => savedStatus));
  // A freshly uploaded (not-yet-committed) avatar, or an explicit removal.
  let pendingAvatar = $state<{ hash: string; url: string } | null>(null);
  let removeAvatar = $state(false);
  let uploading = $state(false);
  let fileInput = $state<HTMLInputElement>();

  const previewAvatarUrl = $derived(
    pendingAvatar ? pendingAvatar.url : removeAvatar ? null : savedAvatarUrl,
  );
  const previewName = $derived(dName.trim() || app.account);

  const STATUSES = [
    { key: "online", label: "Online" },
    { key: "away", label: "Away" },
    { key: "dnd", label: "Do Not Disturb" },
    { key: "invisible", label: "Invisible" },
  ];

  const isDirty = $derived(
    dName.trim() !== savedDisplay ||
      dAbout.trim() !== savedAbout ||
      dStatus !== savedStatus ||
      !!pendingAvatar ||
      removeAvatar,
  );

  // §10.5 verification drafts.
  let emailDraft = $state("");
  let codeDraft = $state("");
  let birthdayDraft = $state("");
  let emailSent = $state(false);

  function sendCode() {
    const addr = emailDraft.trim();
    if (!addr) return;
    weft
      .verifyEmail(addr)
      .then(() => (emailSent = true))
      .catch((e) => app.toast(String(e), "error"));
  }
  function confirmEmail() {
    const code = codeDraft.trim();
    if (!code) return;
    weft
      .verifyConfirm("email", code)
      .then(() => {
        codeDraft = "";
        emailSent = false;
      })
      .catch((e) => app.toast(String(e), "error"));
  }
  function saveBirthday() {
    const date = birthdayDraft.trim();
    if (!date) return;
    weft.verifyBirthday(date).catch((e) => app.toast(String(e), "error"));
  }

  async function onAvatarPicked(e: Event) {
    const input = e.currentTarget as HTMLInputElement;
    const file = input.files?.[0];
    if (!file) return;
    uploading = true;
    try {
      // Upload the bytes now (content-addressed); the profile only points at the
      // new blob once the draft is saved.
      const res = await weft.upload(file);
      pendingAvatar = { hash: weft.mediaHash(res.media), url: weft.mediaUrl(res.media) };
      removeAvatar = false;
    } catch (err) {
      app.toast(String(err), "error");
    } finally {
      uploading = false;
      input.value = "";
    }
  }
  function revertProfile() {
    dName = savedDisplay;
    dAbout = savedAbout;
    dStatus = savedStatus;
    pendingAvatar = null;
    removeAvatar = false;
  }
  async function saveProfile() {
    // Send only what changed. Presence rides its own verb; the rest is one
    // partial PROFILE SET (an empty string clears a field).
    const opts: { display?: string; avatar?: string; about?: string } = {};
    if (dName.trim() !== savedDisplay) opts.display = dName.trim();
    if (dAbout.trim() !== savedAbout) opts.about = dAbout.trim();
    if (pendingAvatar) opts.avatar = pendingAvatar.hash;
    else if (removeAvatar) opts.avatar = "";
    try {
      if (Object.keys(opts).length) await weft.profileSet(opts);
      if (dStatus !== savedStatus) app.setStatus(dStatus);
      pendingAvatar = null;
      removeAvatar = false;
    } catch (e) {
      app.toast(String(e), "error");
    }
  }
</script>

<div class="settings-overlay" role="dialog" aria-modal="true" transition:fade|global={{ duration: 150 }}>
  <nav class="so-nav">
    <div class="so-nav-inner">
      <div class="so-heading">{app.account}</div>
      <button class="so-navitem" class:active={app.userTab === "account"} onclick={() => (app.userTab = "account")}>Account</button>
      <button class="so-navitem" class:active={app.userTab === "appearance"} onclick={() => (app.userTab = "appearance")}>Appearance</button>
      <button class="so-navitem" class:active={app.userTab === "verification"} onclick={() => (app.userTab = "verification")}>Verification</button>
      <button class="so-navitem" class:active={app.userTab === "connection"} onclick={() => (app.userTab = "connection")}>Device &amp; connection</button>
      <div class="so-heading">Session</div>
      <button class="so-navitem danger" onclick={app.logout}>Log out</button>
    </div>
  </nav>
  <main class="so-main">
    <div class="so-content">
      {#if app.userTab === "account"}
        <h1>Edit Profile</h1>
        <p class="so-sub">Changes preview live — save or revert them from the bar below.</p>
        <input type="file" accept="image/*" bind:this={fileInput} onchange={onAvatarPicked} hidden />

        <div class="pe-grid">
          <div class="pe-editor">
            <!-- Avatar -->
            <div class="pe-card">
              <div class="field-label">Profile picture</div>
              <div class="pe-avrow">
                <button class="pe-av" title="Change avatar" onclick={() => fileInput?.click()}>
                  {#if previewAvatarUrl}<img src={previewAvatarUrl} alt="" />{:else}{app.initials(app.account)}{/if}
                  <span class="pe-av-overlay">
                    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M23 19a2 2 0 0 1-2 2H3a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h4l2-3h6l2 3h4a2 2 0 0 1 2 2z" /><circle cx="12" cy="13" r="4" /></svg>
                    Change
                  </span>
                </button>
                <div class="pe-av-actions">
                  <button class="pe-link" onclick={() => fileInput?.click()}>{uploading ? "Uploading…" : "Upload new photo"}</button>
                  {#if previewAvatarUrl}
                    <span class="pe-dotsep">·</span>
                    <button class="pe-link danger" onclick={() => { pendingAvatar = null; removeAvatar = true; }}>Remove</button>
                  {/if}
                </div>
              </div>
            </div>

            <!-- Display name -->
            <div class="pe-card">
              <div class="field-label">Display name</div>
              <input class="pe-input" bind:value={dName} maxlength="128" placeholder="Your display name (optional)" />
              <div class="pe-count">{dName.length}/128</div>
            </div>

            <!-- About -->
            <div class="pe-card">
              <div class="field-label">About me</div>
              <textarea class="pe-input pe-textarea" bind:value={dAbout} maxlength="512" rows="4" placeholder="Tell people about yourself…"></textarea>
              <div class="pe-count">{dAbout.length}/512</div>
            </div>

            <!-- Status -->
            <div class="pe-card">
              <div class="field-label">Status</div>
              <div class="pe-status-grid">
                {#each STATUSES as s (s.key)}
                  <button class="pe-status" class:on={dStatus === s.key} onclick={() => (dStatus = s.key)}>
                    <span class="dot {s.key}"></span>{s.label}
                  </button>
                {/each}
              </div>
            </div>

            <!-- Identity + operator -->
            <div class="pe-card">
              <div class="set-row"><span>Identity</span><b>{app.account}@{app.network}</b></div>
              {#if app.isOperator}
                <div class="section-sep"></div>
                <div class="field-label">Network defense</div>
                <p class="so-sub">Block abusive peer networks and manage network-wide bridges. Per-namespace federation lives in each namespace's Server Settings.</p>
                <button class="set-btn" onclick={app.openFederation}>Open network federation</button>
              {/if}
            </div>
          </div>

          <!-- Live preview -->
          <aside class="pe-preview">
            <div class="field-label">Preview</div>
            <div class="pe-preview-card">
              <div class="pe-preview-banner"></div>
              <div class="pe-preview-avwrap">
                <div class="pe-preview-av">
                  {#if previewAvatarUrl}<img src={previewAvatarUrl} alt="" />{:else}{app.initials(app.account)}{/if}
                  <span class="pe-preview-dot dot {dStatus}"></span>
                </div>
              </div>
              <div class="pe-preview-body">
                <div class="pe-preview-name">{previewName}</div>
                <div class="pe-preview-handle">{app.account}@{app.network}</div>
                <div class="pe-preview-section">
                  <div class="pe-preview-slabel">About me</div>
                  <div class="pe-preview-bio" class:empty={!dAbout.trim()}>{dAbout.trim() || "No bio set."}</div>
                </div>
              </div>
            </div>
            {#if isDirty}<div class="pe-unsaved">Unsaved changes</div>{/if}
          </aside>
        </div>
      {:else if app.userTab === "appearance"}
        <h1>Appearance</h1>
        <p class="so-sub">Theme for this device.</p>
        <div class="field-label">Theme</div>
        <div class="status-inline">
          <button class="chip-btn" class:on={app.theme === "dark"} onclick={() => app.theme !== "dark" && app.toggleTheme()}>Dark</button>
          <button class="chip-btn" class:on={app.theme === "light"} onclick={() => app.theme !== "light" && app.toggleTheme()}>Light</button>
        </div>
      {:else if app.userTab === "verification"}
        <h1>Verification</h1>
        <p class="so-sub">Verify your email and age. Only you can see these — they're never shown to other members.</p>

        <div class="field-label">Email</div>
        {#if app.verifications.email}
          <div class="set-row">
            <span>{app.verifications.email.subject}</span>
            <b class="vstate {app.verifications.email.state}">{app.verifications.email.state === "confirmed" ? "✓ Verified" : "Pending"}</b>
          </div>
        {/if}
        {#if app.verifications.email?.state !== "confirmed"}
          <div class="vrow">
            <input class="prof-input" type="email" bind:value={emailDraft} placeholder="you@example.com" onkeydown={(e) => e.key === "Enter" && sendCode()} />
            <button class="ok-btn" onclick={sendCode}>Send code</button>
          </div>
          {#if emailSent || app.verifications.email?.state === "pending"}
            <p class="so-sub">Enter the code we emailed you (expires in 15 minutes).</p>
            <div class="vrow">
              <input class="prof-input" bind:value={codeDraft} maxlength="6" inputmode="numeric" placeholder="123456" onkeydown={(e) => e.key === "Enter" && confirmEmail()} />
              <button class="ok-btn" onclick={confirmEmail}>Confirm</button>
            </div>
          {/if}
        {/if}

        <div class="section-sep"></div>
        <div class="field-label">Birthday</div>
        {#if app.verifications.birthday}
          <div class="set-row"><span>{app.verifications.birthday.subject}</span><b class="vstate confirmed">✓ Set</b></div>
        {/if}
        <p class="so-sub">Self-declared (not independently verified).</p>
        <div class="vrow">
          <input class="prof-input" type="date" bind:value={birthdayDraft} />
          <button class="ok-btn" onclick={saveBirthday}>Save birthday</button>
        </div>
      {:else if app.userTab === "connection"}
        <h1>Device &amp; connection</h1>
        <p class="so-sub">This device's link to the network.</p>
        <div class="set-row"><span>Server</span><b>{app.host}{app.reconnecting ? " · reconnecting…" : ""}</b></div>
        <div class="section-sep"></div>
        <div class="set-row">
          <span>Passwordless login on this device</span>
          <button class="set-btn" onclick={app.enrollThisDevice}>Enroll device key</button>
        </div>
      {/if}
    </div>

    {#if app.userTab === "account" && isDirty}
      <div class="pe-savebar" transition:fly|global={{ y: 70, duration: 220 }}>
        <div class="pe-savebar-inner">
          <span class="pe-savebar-msg"><span class="pe-savebar-dot"></span>You have unsaved changes</span>
          <div class="pe-savebar-actions">
            <button class="pe-revert" onclick={revertProfile}>
              <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8" /><path d="M3 3v5h5" /></svg>
              Revert
            </button>
            <button class="pe-save" onclick={saveProfile}>
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="3" stroke-linecap="round" stroke-linejoin="round"><polyline points="20 6 9 17 4 12" /></svg>
              Save Changes
            </button>
          </div>
        </div>
      </div>
    {/if}
  </main>
  <div class="so-exit">
    <button class="so-close" aria-label="Close settings" onclick={onclose}>✕</button>
    <span class="so-close-label">ESC</span>
  </div>
</div>

<style>
  /* ---- profile editor (draft + live preview + save/revert bar) ---- */
  .pe-grid {
    display: flex;
    gap: 24px;
    align-items: flex-start;
    flex-wrap: wrap;
    margin-top: 12px;
    padding-bottom: 72px; /* clear the floating save bar */
  }
  .pe-editor {
    flex: 1;
    min-width: 280px;
    max-width: 560px;
    display: flex;
    flex-direction: column;
    gap: 12px;
  }
  .pe-card {
    background: var(--bg-panel, rgba(255, 255, 255, 0.03));
    border: 1px solid var(--border-hair);
    border-radius: 14px;
    padding: 16px;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .pe-avrow {
    display: flex;
    align-items: center;
    gap: 16px;
  }
  .pe-av {
    position: relative;
    width: 76px;
    height: 76px;
    flex-shrink: 0;
    border-radius: 50%;
    border: none;
    padding: 0;
    overflow: hidden;
    cursor: pointer;
    display: grid;
    place-items: center;
    font-size: 24px;
    font-weight: 700;
    color: #fff;
    background: var(--accent, #5865f2);
  }
  .pe-av img {
    width: 100%;
    height: 100%;
    object-fit: cover;
  }
  .pe-av-overlay {
    position: absolute;
    inset: 0;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 2px;
    background: rgba(0, 0, 0, 0.6);
    color: #fff;
    font-size: 9px;
    font-weight: 700;
    letter-spacing: 0.05em;
    text-transform: uppercase;
    opacity: 0;
    transition: opacity 0.12s;
  }
  .pe-av:hover .pe-av-overlay {
    opacity: 1;
  }
  .pe-av-actions {
    display: flex;
    align-items: center;
    gap: 6px;
    flex-wrap: wrap;
  }
  .pe-link {
    border: none;
    background: none;
    padding: 0;
    cursor: pointer;
    font: inherit;
    font-size: 13px;
    font-weight: 600;
    color: var(--accent, #5865f2);
  }
  .pe-link:hover {
    text-decoration: underline;
  }
  .pe-link.danger {
    color: var(--danger, #e06c6c);
    font-weight: 500;
  }
  .pe-dotsep {
    color: var(--text-faint);
  }
  .pe-input {
    width: 100%;
    padding: 10px 14px;
    border-radius: 10px;
    border: 2px solid transparent;
    background: var(--bg-void, rgba(0, 0, 0, 0.18));
    color: var(--text-primary, inherit);
    font: inherit;
    font-size: 14px;
    outline: none;
    transition: border-color 0.15s;
  }
  .pe-input:hover {
    border-color: color-mix(in srgb, var(--accent) 40%, transparent);
  }
  .pe-input:focus {
    border-color: var(--accent, #5865f2);
  }
  .pe-textarea {
    resize: vertical;
    min-height: 88px;
    line-height: 1.55;
  }
  .pe-count {
    text-align: right;
    font-size: 11px;
    color: var(--text-faint);
    margin-top: -4px;
  }
  .pe-status-grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 8px;
  }
  .pe-status {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 10px 14px;
    border-radius: 12px;
    border: 2px solid transparent;
    background: var(--bg-void, rgba(0, 0, 0, 0.18));
    color: var(--text-muted);
    font: inherit;
    font-size: 13px;
    font-weight: 500;
    cursor: pointer;
    transition:
      background 0.12s,
      color 0.12s;
  }
  .pe-status:hover {
    background: var(--bg-hover);
    color: var(--text-secondary);
  }
  .pe-status.on {
    background: color-mix(in srgb, var(--accent) 15%, transparent);
    border-color: var(--accent, #5865f2);
    color: var(--text-primary);
  }

  /* live preview card */
  .pe-preview {
    position: sticky;
    top: 8px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .pe-preview-card {
    width: 280px;
    max-width: 100%;
    border-radius: 16px;
    overflow: hidden;
    background: var(--bg-void, #111214);
    border: 1px solid var(--border-hair);
  }
  .pe-preview-banner {
    height: 72px;
    background: linear-gradient(135deg, var(--accent, #5865f2), color-mix(in srgb, var(--accent) 50%, #000));
  }
  .pe-preview-avwrap {
    padding: 0 16px;
  }
  .pe-preview-av {
    position: relative;
    width: 68px;
    height: 68px;
    margin-top: -34px;
    border-radius: 50%;
    border: 5px solid var(--bg-void, #111214);
    background: var(--accent, #5865f2);
    display: grid;
    place-items: center;
    font-size: 22px;
    font-weight: 700;
    color: #fff;
    overflow: visible;
  }
  .pe-preview-av img {
    width: 100%;
    height: 100%;
    border-radius: 50%;
    object-fit: cover;
  }
  .pe-preview-dot {
    position: absolute;
    right: -1px;
    bottom: -1px;
    width: 18px;
    height: 18px;
    border-radius: 50%;
    border: 4px solid var(--bg-void, #111214);
  }
  .pe-preview-body {
    padding: 8px 16px 18px;
  }
  .pe-preview-name {
    font-size: 18px;
    font-weight: 800;
    color: var(--text-primary);
    line-height: 1.2;
    word-break: break-word;
  }
  .pe-preview-handle {
    font-size: 12px;
    font-family: var(--font-mono);
    color: var(--text-muted);
    margin-top: 2px;
    word-break: break-all;
  }
  .pe-preview-section {
    margin-top: 12px;
    padding-top: 12px;
    border-top: 1px solid var(--border-hair);
  }
  .pe-preview-slabel {
    font-size: 10px;
    font-weight: 800;
    letter-spacing: 0.1em;
    text-transform: uppercase;
    color: var(--text-muted);
    margin-bottom: 6px;
  }
  .pe-preview-bio {
    font-size: 13px;
    line-height: 1.55;
    color: var(--text-secondary);
    white-space: pre-wrap;
    word-break: break-word;
  }
  .pe-preview-bio.empty {
    color: var(--text-faint);
    font-style: italic;
  }
  .pe-unsaved {
    text-align: center;
    font-size: 12px;
    color: #faa61a;
  }

  /* floating save / revert bar */
  .pe-savebar {
    position: absolute;
    left: 0;
    right: 0;
    bottom: 0;
    z-index: 5;
    padding: 0 24px 16px;
    pointer-events: none;
  }
  .pe-savebar-inner {
    pointer-events: auto;
    max-width: 640px;
    margin: 0 auto;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 16px;
    padding: 12px 16px 12px 18px;
    background: var(--bg-elevated, #111214);
    border: 1px solid var(--border-hair-strong);
    border-radius: 14px;
    box-shadow: 0 18px 50px rgba(0, 0, 0, 0.5);
  }
  .pe-savebar-msg {
    display: flex;
    align-items: center;
    gap: 10px;
    font-size: 14px;
    font-weight: 500;
    color: var(--text-secondary);
  }
  .pe-savebar-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: #faa61a;
    flex-shrink: 0;
  }
  .pe-savebar-actions {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .pe-revert,
  .pe-save {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    border: none;
    border-radius: 10px;
    font: inherit;
    font-size: 13px;
    cursor: pointer;
  }
  .pe-revert {
    padding: 8px 14px;
    background: transparent;
    color: var(--text-muted);
    font-weight: 500;
  }
  .pe-revert:hover {
    background: var(--bg-hover);
    color: var(--text-primary);
  }
  .pe-save {
    padding: 8px 18px;
    background: #3ba55d;
    color: #fff;
    font-weight: 600;
  }
  .pe-save:hover {
    background: #2f8a4c;
  }

  .vrow {
    display: flex;
    gap: 8px;
    align-items: center;
    margin-top: 6px;
  }
  .vrow .prof-input {
    flex: 1;
    min-width: 0;
  }
  .vstate {
    font-size: 0.82rem;
    font-weight: 600;
  }
  .vstate.confirmed {
    color: #43b581;
  }
  .vstate.pending {
    color: #d9a441;
  }
</style>
