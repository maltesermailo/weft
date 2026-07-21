<script lang="ts">
  import { fade } from "svelte/transition";
  import { getApp } from "$lib/context";
  import * as weft from "$lib/weft";
  import Avatar from "$lib/components/Avatar.svelte";
  const app = getApp();
  let { onclose }: { onclose: () => void } = $props();

  // §10.3 profile editor. Prefill the display draft with the current name (empty
  // if unset, so we don't accidentally re-set the handle as a display name).
  const current = app.displayName(app.account);
  let displayDraft = $state(current === app.account ? "" : current);
  let uploading = $state(false);
  let fileInput = $state<HTMLInputElement>();

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

  function saveDisplay() {
    weft.profileSet({ display: displayDraft.trim() }).catch((e) => app.toast(String(e), "error"));
  }
  async function onAvatarPicked(e: Event) {
    const input = e.currentTarget as HTMLInputElement;
    const file = input.files?.[0];
    if (!file) return;
    uploading = true;
    try {
      const res = await weft.upload(file);
      await weft.profileSet({ avatar: weft.mediaHash(res.media) });
    } catch (err) {
      app.toast(String(err), "error");
    } finally {
      uploading = false;
      input.value = "";
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
    <button class="so-close" aria-label="Close settings" onclick={onclose}>✕<span>ESC</span></button>
    <div class="so-content">
      {#if app.userTab === "account"}
        <h1>Account</h1>
        <p class="so-sub">Your identity on this network.</p>
        <div class="set-row"><span>Identity</span><b>{app.account}@{app.network}</b></div>
        <div class="section-sep"></div>
        <div class="field-label">Profile</div>
        <p class="so-sub">Your display name and avatar, shown to people who share a server with you.</p>
        <div class="profile-edit">
          <button class="avatar prof-avatar" title="Change avatar" onclick={() => fileInput?.click()}>
            <Avatar account={app.account} />
          </button>
          <div class="profile-fields">
            <input class="prof-input" bind:value={displayDraft} maxlength="128" placeholder="Display name (optional)" onkeydown={(e) => e.key === "Enter" && saveDisplay()} />
            <div class="profile-actions">
              <button class="ok-btn" onclick={saveDisplay}>Save name</button>
              <button class="set-btn" onclick={() => fileInput?.click()}>{uploading ? "Uploading…" : "Upload avatar"}</button>
            </div>
          </div>
        </div>
        <input type="file" accept="image/*" bind:this={fileInput} onchange={onAvatarPicked} hidden />
        <div class="section-sep"></div>
        <div class="field-label">Status</div>
        <div class="status-inline">
          {#each ["online", "away", "dnd", "invisible"] as s (s)}
            <button class="chip-btn" class:on={app.myStatus === s} onclick={() => app.setStatus(s)}><span class="dot {s}"></span>{s}</button>
          {/each}
        </div>
        {#if app.isOperator}
          <div class="section-sep"></div>
          <div class="field-label">Network defense</div>
          <p class="so-sub">Block abusive peer networks and manage network-wide bridges. Per-namespace federation lives in each namespace's Server Settings.</p>
          <button class="set-btn" onclick={app.openFederation}>Open network federation</button>
        {/if}
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
  </main>
</div>

<style>
  .profile-edit {
    display: flex;
    align-items: center;
    gap: 14px;
    margin-top: 8px;
  }
  .prof-avatar {
    width: 64px;
    height: 64px;
    border-radius: 12px;
    padding: 0;
    cursor: pointer;
    font-size: 20px;
  }
  .profile-fields {
    display: flex;
    flex-direction: column;
    gap: 8px;
    flex: 1;
    min-width: 0;
  }
  .prof-input {
    padding: 7px 10px;
    border-radius: 7px;
    border: 1px solid var(--border-hair-strong);
    background: var(--bg-2, rgba(255, 255, 255, 0.03));
    color: var(--text, inherit);
    font-size: 0.9rem;
  }
  .profile-actions {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
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
