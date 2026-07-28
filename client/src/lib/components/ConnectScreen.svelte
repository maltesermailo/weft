<script lang="ts">
  import type { Mode } from "$lib/weft";

  let {
    mode = $bindable(),
    host = $bindable(),
    formAccount = $bindable(),
    formPassword = $bindable(),
    formEmail = $bindable(),
    status,
    authError,
    deviceKeyAvailable,
    insecure,
    serverStep,
    canChangeServer,
    emailRequired,
    probing,
    onconnect,
    onkeylogin,
    onchooseserver,
    onchangeserver,
  }: {
    mode: Mode;
    host: string;
    formAccount: string;
    formPassword: string;
    formEmail: string;
    status: string;
    authError: string;
    deviceKeyAvailable: boolean;
    insecure: boolean;
    // Which sub-screen: pick the homeserver, or log in / register.
    serverStep: "server" | "auth";
    // Web pins the network to the page origin — no "Change" there.
    canChangeServer: boolean;
    // This homeserver requires an email at REGISTER (from its WELCOME, §3.6).
    emailRequired: boolean;
    // A probe of the homeserver is in flight.
    probing: boolean;
    onconnect: () => void;
    onkeylogin: () => void;
    onchooseserver: () => void;
    onchangeserver: () => void;
  } = $props();
</script>

<div class="connect-screen">
  {#if serverStep === "server"}
    <!-- Step 1: choose the homeserver. -->
    <form class="connect-card" onsubmit={(e) => { e.preventDefault(); onchooseserver(); }}>
      <h1>WEFT</h1>
      <p class="sub">connect to a homeserver</p>

      <label for="host">Homeserver</label>
      <input id="host" bind:value={host} placeholder="127.0.0.1:4433" autocomplete="off" />

      <button type="submit" disabled={!host.trim()}>Continue</button>
      {#if insecure}
        <div class="insecure-note">⚠ Insecure mode — the server's TLS certificate is <b>not verified</b> (set in <code>client.toml</code>). Use only for servers you control.</div>
      {/if}
    </form>
  {:else}
    <!-- Step 2: log in or register against the chosen homeserver. -->
    <form class="connect-card" onsubmit={(e) => { e.preventDefault(); onconnect(); }}>
      <h1>WEFT</h1>
      <p class="sub">{mode === "login" ? "log in to a network" : "register a new account"}</p>

      <div class="server-row">
        <span class="server-name" title={host}>{probing ? "checking server…" : host}</span>
        {#if canChangeServer}
          <button type="button" class="change-server" onclick={onchangeserver}>Change</button>
        {/if}
      </div>

      <div style="display:flex;gap:8px;margin-bottom:4px">
        <button type="button" class="channel-item" style="justify-content:center;{mode === 'login' ? 'color:var(--text-primary);background:var(--bg-panel-raised)' : ''}" onclick={() => (mode = "login")}>Log in</button>
        <button type="button" class="channel-item" style="justify-content:center;{mode === 'register' ? 'color:var(--text-primary);background:var(--bg-panel-raised)' : ''}" onclick={() => (mode = "register")}>Register</button>
      </div>

      <label for="acct">{mode === "login" ? "Account or email" : "Account"}</label>
      <input id="acct" bind:value={formAccount} placeholder={mode === "login" ? "ada or ada@example.com" : "ada"} autocomplete="off" />

      {#if mode === "register" && emailRequired}
        <label for="email">Email</label>
        <input id="email" type="email" bind:value={formEmail} placeholder="ada@example.com" autocomplete="off" />
      {/if}

      <label for="pw">Password</label>
      <input id="pw" type="password" bind:value={formPassword} placeholder={mode === "register" ? "min 12 characters" : "your password"} autocomplete="off" />

      <button type="submit" disabled={status === "connecting" || !formAccount.trim()}>
        {status === "connecting" ? "connecting…" : mode === "register" ? "Create account" : "Log in"}
      </button>
      {#if deviceKeyAvailable && mode !== "register"}
        <button type="button" class="key-login" onclick={onkeylogin}>🔑 Log in with device key</button>
      {/if}
      {#if authError}<div class="err">{authError}</div>{/if}
      {#if insecure}
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
