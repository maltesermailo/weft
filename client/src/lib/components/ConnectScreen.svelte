<script lang="ts">
  import type { Mode } from "$lib/weft";

  let {
    mode = $bindable(),
    host = $bindable(),
    formAccount = $bindable(),
    formPassword = $bindable(),
    status,
    authError,
    deviceKeyAvailable,
    insecure,
    onconnect,
    onkeylogin,
  }: {
    mode: Mode;
    host: string;
    formAccount: string;
    formPassword: string;
    status: string;
    authError: string;
    deviceKeyAvailable: boolean;
    insecure: boolean;
    onconnect: () => void;
    onkeylogin: () => void;
  } = $props();
</script>

<div class="connect-screen">
  <form class="connect-card" onsubmit={(e) => { e.preventDefault(); onconnect(); }}>
    <h1>WEFT</h1>
    <p class="sub">{mode === "login" ? "log in to a network" : "register a new account"}</p>

    <div style="display:flex;gap:8px;margin-bottom:4px">
      <button type="button" class="channel-item" style="justify-content:center;{mode === 'login' ? 'color:var(--text-primary);background:var(--bg-panel-raised)' : ''}" onclick={() => (mode = "login")}>Log in</button>
      <button type="button" class="channel-item" style="justify-content:center;{mode === 'register' ? 'color:var(--text-primary);background:var(--bg-panel-raised)' : ''}" onclick={() => (mode = "register")}>Register</button>
    </div>

    <label for="host">Network</label>
    <input id="host" bind:value={host} placeholder="127.0.0.1:4433" autocomplete="off" />
    <label for="acct">Account</label>
    <input id="acct" bind:value={formAccount} placeholder="ada" autocomplete="off" />
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
</div>
