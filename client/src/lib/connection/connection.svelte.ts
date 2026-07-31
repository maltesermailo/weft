// The session lifecycle: connect / login / register / reconnect / logout, plus
// the reconnect backoff loop (driven by the reducer's `closed` handler). Split
// out of the layout so the whole flow lives beside the `conn`/`syncState` it
// mutates; the layout keeps only the reactive device-key + ns-meta effects.
import { goto } from "$app/navigation";
import * as weft from "$lib/transport/weft";
import { cf } from "$lib/session/connect.svelte";
import { store } from "$lib/store/store.svelte";
import { ui } from "$lib/ui/ui.svelte";
import { toast } from "$lib/notifications/toasts.svelte";
import { channelStore } from "$lib/channels/channel.svelte";

/// localStorage keys shared by the connect flow + the reducer.
export const HOMESERVER_KEY = "weft:homeserver";
export const SAVED_KEY = "weft:last-connect";

/// SYNC-protocol flags: `syncing` = an initial/reconnect SYNC is streaming (so
/// event handlers must not auto-navigate); `synced` = this app session has
/// synced at least once (a later reconnect replays the cursor, not a full sync).
export const syncState = { syncing: false, synced: false };

/// The v0.12 SYNC cursor key (per account+device); replayed on reconnect so
/// `SYNC since=` catches up missed messages + offline edits/reactions.
export const syncCursorKey = (): string => `weft:sync:${store.session.account}@${store.session.network}`;
export function loadSyncCursor(): string | undefined {
  try {
    return localStorage.getItem(syncCursorKey()) ?? undefined;
  } catch {
    return undefined;
  }
}

export const conn = $state<{
  /// Credentials of the live session, kept for silent reconnect (null = none).
  lastCreds: { host: string; account: string; password: string } | null;
  /// A user-initiated logout is in flight — suppress the reconnect on `closed`.
  manualLogout: boolean;
  /// Exponential-backoff attempt counter (reset on a successful connect).
  reconnectAttempts: number;
}>({
  lastCreds: null,
  manualLogout: false,
  reconnectAttempts: 0,
});

/// Schedule a reconnect with exponential backoff (login mode — the account
/// already exists). No-op if there are no stored credentials.
export function attemptReconnect(): void {
  if (!conn.lastCreds) {
    store.session.status = "connect";
    return;
  }
  ui.reconnecting = true;
  const delay = Math.min(1500 * 2 ** conn.reconnectAttempts, 15000);
  conn.reconnectAttempts++;
  setTimeout(() => {
    if (!ui.reconnecting) return; // logged out meanwhile
    weft
      .connect(conn.lastCreds!.host, conn.lastCreds!.account, conn.lastCreds!.password, "login")
      .catch(() => attemptReconnect());
  }, delay);
}

/// Namespaces whose NS-META we've already requested this session (rail-tile
/// auto-fetch dedup). Cleared on logout + discover reset; the layout's effect
/// fills it as new tiles appear.
export const nsMetaFetched = new Set<string>();

// ---- §10.3 presence ----
export function setStatus(s: string): void {
  store.session.myStatus = s;
  ui.userMenu = false;
  weft.presence(s).catch(() => {});
}

// ---- §6.1 connect / login / register ----
export function keyLogin(): void {
  cf.mode = "key";
  doConnect();
}
export function enrollThisDevice(): void {
  weft
    .enrollDevice(cf.host.trim(), store.session.account)
    .then(() => toast("Device key enrolled — passwordless login is on for next time"))
    .catch((e) => toast(String(e), "error"));
}

export async function doConnect(): Promise<void> {
  if (!cf.account.trim()) return;
  // §6.1 a register email is required only when the homeserver asks for one.
  if (cf.mode === "register" && cf.emailRequired && !cf.email.trim()) {
    cf.authError = "this server requires an email address to register";
    return;
  }
  cf.authError = "";
  cf.authFailed = false;
  store.session.status = "connecting";
  conn.manualLogout = false;
  conn.reconnectAttempts = 0;
  // Held in memory (never persisted) so a mid-session drop can reconnect.
  conn.lastCreds = { host: cf.host.trim(), account: cf.account.trim(), password: cf.password };
  try {
    await weft.connect(cf.host.trim(), cf.account.trim(), cf.password, cf.mode, cf.mode === "register" ? cf.email.trim() : undefined);
  } catch (err) {
    store.session.status = "connect";
    cf.authError = String(err);
  }
}

/// §3.6 probe the current homeserver for its shape (does REGISTER need an
/// email?). Best-effort: a failure just leaves the email field optional.
export async function probeServer(): Promise<void> {
  const h = cf.host.trim();
  if (!h) return;
  cf.probing = true;
  try {
    const info = await weft.probe(h);
    cf.emailRequired = info.emailRequired;
  } catch {
    cf.emailRequired = false;
  } finally {
    cf.probing = false;
  }
}

/// Confirm the typed homeserver: persist it as the local default, move to the
/// login/register step, and probe it for its register-email requirement.
export function chooseServer(): void {
  const h = cf.host.trim();
  if (!h) return;
  try {
    localStorage.setItem(HOMESERVER_KEY, h);
  } catch {
    /* storage unavailable */
  }
  cf.serverStep = "auth";
  void probeServer();
}

/// "Change" on the login screen → back to the homeserver picker.
export function changeServer(): void {
  cf.authError = "";
  cf.emailRequired = false;
  cf.serverStep = "server";
}

export function logout(): void {
  conn.manualLogout = true;
  ui.reconnecting = false;
  conn.lastCreds = null;
  ui.userMenu = false;
  ui.settingsOpen = false;
  weft.disconnect().catch(() => {});
  channelStore.reset();
  goto("/"); // reset the view so the next login lands home, not on a stale URL
  store.servers.clear();
  nsMetaFetched.clear();
  store.resetPresence();
  store.reports.queue.clear();
  // The in-memory skeleton is gone — the next login must do a full sync, not a
  // cursor delta (which would leave the rail empty).
  syncState.synced = false;
  store.session.status = "connect";
}
