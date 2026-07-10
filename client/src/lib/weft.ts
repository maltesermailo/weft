// Thin typed wrapper over the client backend + its event stream. Two backends
// live behind one `invoke`/`onWeft`/`notify` surface, picked at runtime:
//   • Desktop (Tauri): `invoke` → #[tauri::command]s, events over the `weft`
//     channel — the native `weft-client-core` binding.
//   • Browser (WASM): `invoke` → `WeftClient.invoke`, events via a JS callback —
//     the `weft-client-wasm` binding, same core compiled to WebAssembly.
// UI code never sees the difference; only the three primitives below branch.
import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
} from "@tauri-apps/plugin-notification";

/// Tauri v2 injects `__TAURI_INTERNALS__`; its absence ⇒ a plain browser ⇒ WASM.
const IS_TAURI =
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

/// True in the browser (WASM) build, false in the Tauri desktop app. Used by the
/// UI to prefill the network with the page origin (the web client is served by
/// the network it talks to — P3 embed).
export const isWeb = !IS_TAURI;

// ---- WASM backend (lazy: the module + wasm only load in a real browser) ----
type WasmClient = { invoke(cmd: string, args: unknown): Promise<unknown> };
let wasmClient: WasmClient | null = null;
let wasmInit: Promise<WasmClient> | null = null;
const webListeners = new Set<(e: WeftEvent) => void>();

function ensureWasm(): Promise<WasmClient> {
  if (!wasmInit) {
    wasmInit = (async () => {
      // Served by weftd/vite from `static/wasm`; a non-literal specifier keeps
      // the generated pkg off both the desktop bundle and svelte-check.
      const url = "/wasm/weft_client_wasm.js";
      const mod: any = await import(/* @vite-ignore */ url);
      await mod.default();
      wasmClient = new mod.WeftClient((ev: WeftEvent) => {
        for (const cb of webListeners) cb(ev);
      }) as WasmClient;
      return wasmClient;
    })();
  }
  return wasmInit;
}

/// The one backend-agnostic command entry point every wrapper below calls.
function invoke(cmd: string, args: Record<string, unknown> = {}): Promise<any> {
  if (IS_TAURI) return tauriInvoke(cmd, args);
  return ensureWasm().then((c) => c.invoke(cmd, args));
}

export type Mode = "login" | "register" | "key";

export type WeftEvent =
  | { kind: "connected"; network: string; account: string }
  | { kind: "auth-failed"; reason: string }
  | {
      kind: "message";
      target: string;
      sender: string;
      network: string;
      msgid: string;
      body: string;
      own: boolean;
      history: boolean;
      edited: boolean;
      reply_to: string | null;
      md: boolean;
    }
  | { kind: "typing"; channel: string; user: string; state: string }
  | { kind: "presence"; user: string; status: string }
  | { kind: "marked"; channel: string; msgid: string }
  | { kind: "pinned"; channel: string; msgid: string; by: string | null }
  | { kind: "unpinned"; channel: string; msgid: string }
  | { kind: "caps"; account: string; scope: string; caps: string }
  | { kind: "role"; scope: string; color: string; caps: string; name: string }
  | { kind: "role-member"; scope: string; account: string; roles: string }
  | { kind: "chanmeta"; channel: string; key: string; value: string }
  | {
      kind: "ns-meta";
      name: string;
      visibility: string;
      owner: string | null;
      title: string | null;
      description: string | null;
      recovery_set: boolean;
      recovery_eta: number | null;
      recovery_rung: number | null;
      categories: string[];
      federation: boolean;
    }
  | { kind: "channel-layout"; channel: string; category: string | null; position: number }
  | { kind: "channel-renamed"; old: string; new: string }
  | {
      kind: "manifest";
      peer: string;
      version: number;
      state: string;
      channels: string[];
      history: string;
      media: string;
      typing: boolean;
    }
  | { kind: "netblocked"; network: string; reason: string | null }
  | { kind: "more"; cursor: string }
  | { kind: "token"; subject: string; scope: string }
  | { kind: "invited"; scope: string; invite_id: string; link: string | null; max_uses: number | null }
  | { kind: "reported"; report_id: string }
  | {
      kind: "report-filed";
      report_id: string;
      msgid: string;
      category: string;
      state: string;
      scope: string;
      reporter: string | null;
    }
  | { kind: "report-resolved"; report_id: string; action: string; note: string | null }
  | { kind: "batch-start"; id: string }
  | { kind: "batch-end"; id: string; truncated: boolean }
  | {
      kind: "member";
      channel: string;
      user: string;
      network: string;
      action: string;
      count: number | null;
    }
  | { kind: "policy"; channel: string; policy: string }
  | { kind: "edited"; target: string; sender: string; edit_of: string; body: string }
  | { kind: "deleted"; target: string; msgid: string }
  | { kind: "reaction"; target: string; msgid: string; emoji: string; op: string; by: string }
  | {
      kind: "reactions";
      target: string;
      msgid: string;
      emoji: string;
      count: number;
      by: string[];
    }
  | {
      kind: "moderated";
      scope: string;
      account: string;
      action: string;
      by: string | null;
      reason: string | null;
    }
  | { kind: "error"; code: string; text: string }
  | { kind: "closed"; reason: string }
  | { kind: "raw"; line: string };

export function connect(host: string, account: string, password: string, mode: Mode) {
  return invoke("connect", { host, account, password, mode });
}

export type ClientConfig = { allow_insecure: boolean; default_host: string | null; config_path: string | null };

/// The active client.toml settings (TLS mode + prefill host + file path).
export function clientConfig(): Promise<ClientConfig> {
  return invoke("client_config");
}

/// Tear down the current connection (logout / switch account).
export function disconnect() {
  return invoke("disconnect");
}

/// Enroll a device key for passwordless login next time (while authed).
export function enrollDevice(host: string, account: string) {
  return invoke("enroll_device", { host, account });
}

/// Is a device key enrolled locally for this host + account?
export function hasDeviceKey(host: string, account: string): Promise<boolean> {
  return invoke("has_device_key", { host, account });
}

/// Fire a desktop notification (requests permission on first use). Web falls
/// back to the browser Notification API.
export async function notify(title: string, body: string) {
  if (IS_TAURI) {
    let ok = await isPermissionGranted();
    if (!ok) ok = (await requestPermission()) === "granted";
    if (ok) sendNotification({ title, body });
    return;
  }
  if (typeof Notification === "undefined") return;
  if (Notification.permission === "granted") {
    new Notification(title, { body });
  } else if (Notification.permission !== "denied") {
    if ((await Notification.requestPermission()) === "granted") {
      new Notification(title, { body });
    }
  }
}

export function join(channel: string) {
  return invoke("join", { channel });
}

/// Auto-join every visible channel in a namespace (§6.2 NS JOIN).
export function nsJoin(name: string) {
  return invoke("ns_join", { name });
}

/// Create a namespace — the root keypair is generated + stored on-device;
/// only the public key is sent (§6.2).
export function nsCreate(network: string, name: string, visibility: string) {
  return invoke("ns_create", { network, name, visibility });
}

// ---- auto-federation (§11.10) ----
/** Request an on-demand bridge to a foreign namespace (`network/namespace`). */
export function federate(target: string) {
  return invoke("federate", { target });
}

// ---- namespace admin (§6.2 / §2.4) ----
export function nsMeta(name: string, key: string, value: string) {
  return invoke("ns_meta", { name, key, value });
}
export function nsVisibility(name: string, visibility: string) {
  return invoke("ns_visibility", { name, visibility });
}
export function nsDelegate(name: string, subject: string, caps: string) {
  return invoke("ns_delegate", { name, subject, caps });
}
export function nsDelete(name: string) {
  return invoke("ns_delete", { name });
}
export function nsRecoverySet(name: string, m: number, keys: string) {
  return invoke("ns_recovery_set", { name, m, keys });
}
/// Root-signed (loads the stored key in the backend).
export function nsTransfer(network: string, name: string, newOwner: string) {
  return invoke("ns_transfer", { network, name, newOwner });
}
export function nsRecoveryCancel(network: string, name: string) {
  return invoke("ns_recovery_cancel", { network, name });
}
/// §2.4 recovery quorum flow.
export function recoveryPubkey(network: string, name: string): Promise<string> {
  return invoke("recovery_pubkey", { network, name });
}
export function recoveryStart(network: string, name: string, newOwner: string): Promise<string> {
  return invoke("recovery_start", { network, name, newOwner });
}
export function recoveryCosign(network: string, name: string, rotation: string): Promise<string> {
  return invoke("recovery_cosign", { network, name, rotation });
}
export function nsRecover(name: string, rotation: string) {
  return invoke("ns_recover", { name, rotation });
}

/// Request a page of history for `target`, older than `before` if given.
export function history(target: string, before?: string) {
  return invoke("history", { target, before: before ?? null });
}

export function edit(msgid: string, body: string) {
  return invoke("edit", { msgid, body });
}

export function del(msgid: string) {
  return invoke("delete", { msgid });
}

export function react(msgid: string, emoji: string) {
  return invoke("react", { msgid, emoji, add: true });
}

export function unreact(msgid: string, emoji: string) {
  return invoke("react", { msgid, emoji, add: false });
}

export function sendMessage(target: string, body: string, replyTo?: string) {
  return invoke("send_message", { target, body, replyTo: replyTo ?? null });
}

export function typing(channel: string, active: boolean) {
  return invoke("typing", { channel, active });
}

export function presence(status: string) {
  return invoke("presence", { status });
}

export function mark(channel: string, msgid: string) {
  return invoke("mark", { channel, msgid });
}

export function members(channel: string) {
  return invoke("members", { channel });
}

export function pin(msgid: string, pinned: boolean) {
  return invoke("pin", { msgid, pinned });
}

export function pins(channel: string) {
  return invoke("pins", { channel });
}

export function caps(account: string, scope: string) {
  return invoke("caps", { account, scope });
}

export function grant(subject: string, scope: string, caps: string) {
  return invoke("grant", { subject, scope, caps });
}

export function revoke(subject: string, scope: string, caps: string) {
  return invoke("revoke", { subject, scope, caps });
}

/// §6.6 named roles (capability-token bundles).
export function roles(scope: string) {
  return invoke("roles", { scope });
}
export function roleCreate(scope: string, color: string, caps: string, name: string) {
  return invoke("role_create", { scope, color, caps, name });
}
export function roleDelete(scope: string, name: string) {
  return invoke("role_delete", { scope, name });
}
export function roleAssign(scope: string, account: string, name: string) {
  return invoke("role_assign", { scope, account, name });
}
export function roleUnassign(scope: string, account: string, name: string) {
  return invoke("role_unassign", { scope, account, name });
}
/// Query an account's explicitly-assigned roles at a scope → a `role-member` event.
export function rolesOfAccount(scope: string, account: string) {
  return invoke("roles_of", { scope, account });
}

export function inviteMint(scope: string) {
  return invoke("invite_mint", { scope });
}

export function inviteRedeem(token: string) {
  return invoke("invite_redeem", { token });
}

/// Close an outstanding invite (§6.5).
export function inviteRevoke(inviteId: string) {
  return invoke("invite_revoke", { inviteId });
}

/// Moderation (§6.7). `verb` = mute|unmute|ban|unban|kick. `scope` is a channel
/// (`#chan`), namespace (`ns:<name>`) or `*`; for `kick` it must be a channel.
export function moderate(verb: string, scope: string, account: string, reason?: string) {
  return invoke("moderate", { verb, scope, account, reason: reason ?? null });
}

// ---- federation (§11): netblocks + bridges (operator) ----
export function netblockAdd(network: string, reason?: string) {
  return invoke("netblock_add", { network, reason: reason ?? null });
}
export function netblockRemove(network: string) {
  return invoke("netblock_remove", { network });
}
export function netblockList() {
  return invoke("netblock_list");
}
/// `history` = from-epoch|full; `media` = mirror|mirror-max:<bytes>|none.
export function bridgePropose(scope: string, peer: string, history: string, media: string, typing: boolean) {
  return invoke("bridge_propose", { scope, peer, history, media, typing });
}
export function bridgeAccept(peer: string, version: number) {
  return invoke("bridge_accept", { peer, version });
}
export function bridgeSever(peer: string) {
  return invoke("bridge_sever", { peer });
}

export function report(msgid: string, category: string, scope: string, note?: string) {
  return invoke("report", { msgid, category, scope, note: note ?? null });
}

export function reportsList(scope: string, status?: string) {
  return invoke("reports_list", { scope, status: status ?? null });
}

export function reportsResolve(reportId: string, action: string, note?: string) {
  return invoke("reports_resolve", { reportId, action, note: note ?? null });
}

export function part(channel: string) {
  return invoke("part", { channel });
}

export function channelCreate(channel: string, policy?: string) {
  return invoke("channel_create", { channel, policy: policy ?? null });
}

/// Change an existing channel's retention (§6.3). `purge` is required for some
/// e2ee transitions (invariant 8).
export function channelPolicy(channel: string, policy: string, purge = false) {
  return invoke("channel_policy", { channel, policy, purge });
}

/// Change a channel's identity (§6.3). Re-keys everything server-side; the
/// server replies with a `channel-renamed` event.
export function channelRename(old: string, next: string) {
  return invoke("channel_rename", { old, new: next });
}

export function channelDelete(channel: string) {
  return invoke("channel_delete", { channel });
}

export function channelMeta(channel: string, key: string, value: string) {
  return invoke("channel_meta", { channel, key, value });
}

export function discover(cursor?: string) {
  return invoke("discover", { cursor: cursor ?? null });
}

export function channels(namespace: string) {
  return invoke("channels", { namespace });
}

export function sendRaw(line: string) {
  return invoke("send_raw", { line });
}

export function onWeft(cb: (e: WeftEvent) => void): Promise<UnlistenFn> {
  if (IS_TAURI) return listen<WeftEvent>("weft", (evt) => cb(evt.payload));
  // Web: register into the fan-out set and make sure the WASM client is live.
  webListeners.add(cb);
  void ensureWasm();
  return Promise.resolve(() => {
    webListeners.delete(cb);
  });
}
