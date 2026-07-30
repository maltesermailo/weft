// Reconnect lifecycle state + the backoff loop. Split out so the reducer's
// `closed` handler can drive reconnection directly (no component seam). The
// initial connect / logout flows (in the layout) mutate `conn` and call
// `attemptReconnect` too. `doConnect` / `logout` themselves stay in the layout
// for now (they touch the login form + UI overlays).
import * as weft from "$lib/weft";
import { cf } from "$lib/models/connect.svelte";
import { store } from "$lib/models/store.svelte";
import { ui } from "$lib/ui.svelte";

/// localStorage keys shared by the connect flow + the reducer.
export const HOMESERVER_KEY = "weft:homeserver";
export const SAVED_KEY = "weft:last-connect";

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
