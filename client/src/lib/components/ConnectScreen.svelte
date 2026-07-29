<script lang="ts">
  import type { ConnectForm } from "$lib/models/connect.svelte";

  // `form` is a mutable reference — the inputs bind to its fields directly, so
  // no per-field bindable props. `+page` reads the same object.
  let {
    form,
    status,
    canChangeServer,
    onconnect,
    onkeylogin,
    onchooseserver,
    onchangeserver,
  }: {
    form: ConnectForm;
    status: string;
    // Web pins the network to the page origin — no "Change" there.
    canChangeServer: boolean;
    onconnect: () => void;
    onkeylogin: () => void;
    onchooseserver: () => void;
    onchangeserver: () => void;
  } = $props();
</script>

<div class="connect-screen">
  {#if form.serverStep === "server"}
    <!-- Step 1: choose the homeserver. -->
    <form class="connect-card" onsubmit={(e) => { e.preventDefault(); onchooseserver(); }}>
      <h1>WEFT</h1>
      <p class="sub">connect to a homeserver</p>

      <label for="host">Homeserver</label>
      <input id="host" bind:value={form.host} placeholder="127.0.0.1:4433" autocomplete="off" />

      <button type="submit" disabled={!form.host.trim()}>Continue</button>
      {#if form.insecure}
        <div class="insecure-note">⚠ Insecure mode — the server's TLS certificate is <b>not verified</b> (set in <code>client.toml</code>). Use only for servers you control.</div>
      {/if}
    </form>
  {:else}
    <!-- Step 2: log in or register against the chosen homeserver. -->
    <form class="connect-card" onsubmit={(e) => { e.preventDefault(); onconnect(); }}>
      <h1>WEFT</h1>
      <p class="sub">{form.mode === "login" ? "log in to a network" : "register a new account"}</p>

      <div class="server-row">
        <span class="server-name" title={form.host}>{form.probing ? "checking server…" : form.host}</span>
        {#if canChangeServer}
          <button type="button" class="change-server" onclick={onchangeserver}>Change</button>
        {/if}
      </div>

      <div style="display:flex;gap:8px;margin-bottom:4px">
        <button type="button" class="channel-item" style="justify-content:center;{form.mode === 'login' ? 'color:var(--text-primary);background:var(--bg-panel-raised)' : ''}" onclick={() => (form.mode = "login")}>Log in</button>
        <button type="button" class="channel-item" style="justify-content:center;{form.mode === 'register' ? 'color:var(--text-primary);background:var(--bg-panel-raised)' : ''}" onclick={() => (form.mode = "register")}>Register</button>
      </div>

      <label for="acct">{form.mode === "login" ? "Account or email" : "Account"}</label>
      <input id="acct" bind:value={form.account} placeholder={form.mode === "login" ? "ada or ada@example.com" : "ada"} autocomplete="off" />

      {#if form.mode === "register" && form.emailRequired}
        <label for="email">Email</label>
        <input id="email" type="email" bind:value={form.email} placeholder="ada@example.com" autocomplete="off" />
      {/if}

      <label for="pw">Password</label>
      <input id="pw" type="password" bind:value={form.password} placeholder={form.mode === "register" ? "min 12 characters" : "your password"} autocomplete="off" />

      <button type="submit" disabled={status === "connecting" || !form.account.trim()}>
        {status === "connecting" ? "connecting…" : form.mode === "register" ? "Create account" : "Log in"}
      </button>
      {#if form.deviceKeyAvailable && form.mode !== "register"}
        <button type="button" class="key-login" onclick={onkeylogin}>🔑 Log in with device key</button>
      {/if}
      {#if form.authError}<div class="err">{form.authError}</div>{/if}
      {#if form.insecure}
        <div class="insecure-note">⚠ Insecure mode — the server's TLS certificate is <b>not verified</b> (set in <code>client.toml</code>). Use only for servers you control.</div>
      {/if}
    </form>
  {/if}
</div>

<style>
  .server-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    margin-bottom: 4px;
    padding: 6px 10px;
    border-radius: 6px;
    background: var(--bg-panel-raised);
    font-size: 0.85rem;
  }
  .server-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--text-secondary);
  }
  .change-server {
    flex: none;
    width: auto;
    margin: 0;
    padding: 3px 10px;
    font-size: 0.8rem;
  }
</style>
