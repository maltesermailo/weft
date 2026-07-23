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
type WasmClient = {
  invoke(cmd: string, args: unknown): Promise<unknown>;
  /** §6/§13 fold one backfill-stream line back through the inbound FSM. */
  feed_line(line: string): void;
};
let wasmClient: WasmClient | null = null;
let wasmInit: Promise<WasmClient> | null = null;
const webListeners = new Set<(e: WeftEvent) => void>();

/// §6/§13 a large HISTORY page arrives as a `backfill` event carrying a token:
/// pull the serialized batch off `/backfill` and replay each line through the
/// FSM, so it folds exactly like an inline BATCH. A failed pull is harmless —
/// the client re-issues the HISTORY (resume = new token). Never forwarded to the
/// app; only the resulting batch events are.
async function pullBackfill(token: string): Promise<void> {
  const client = wasmClient;
  if (!client) return;
  try {
    const res = await fetch(`${mediaOrigin()}/backfill?t=${encodeURIComponent(token)}`);
    if (!res.ok) return;
    const text = await res.text();
    for (const line of text.split("\n")) {
      const trimmed = line.trim();
      if (trimmed) client.feed_line(trimmed);
    }
  } catch {
    /* transient — the next HISTORY re-request mints a fresh token */
  }
}

function ensureWasm(): Promise<WasmClient> {
  if (!wasmInit) {
    wasmInit = (async () => {
      // Served by weftd/vite from `static/wasm`; a non-literal specifier keeps
      // the generated pkg off both the desktop bundle and svelte-check.
      const url = "/wasm/weft_client_wasm.js";
      const mod: any = await import(/* @vite-ignore */ url);
      await mod.default();
      wasmClient = new mod.WeftClient((ev: WeftEvent) => {
        // Intercept backfill tokens: pull + replay, don't surface to the app.
        if (ev.kind === "backfill") {
          void pullBackfill(ev.token);
          return;
        }
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
  | { kind: "media-token"; token: string }
  | { kind: "backfill"; token: string }
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
      thread: string | null;
      md: boolean;
      attachments: string[];
      system: string | null;
    }
  | { kind: "typing"; channel: string; user: string; state: string }
  | { kind: "presence"; user: string; status: string }
  | { kind: "marked"; channel: string; msgid: string }
  | { kind: "unread-counts"; channel: string; unread: number; mentions: number }
  | { kind: "emoji"; namespace: string; name: string; media: string }
  | { kind: "emoji-removed"; namespace: string; name: string }
  | { kind: "pinned"; channel: string; msgid: string; by: string | null }
  | { kind: "unpinned"; channel: string; msgid: string }
  | { kind: "caps"; account: string; scope: string; caps: string }
  | { kind: "role"; scope: string; color: string; caps: string; hoist: boolean; position: number; name: string }
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
  | {
      kind: "channel-layout";
      channel: string;
      category: string | null;
      position: number;
      channel_kind: string;
    }
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
      voice: boolean;
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
  | {
      kind: "profile";
      account: string;
      network: string;
      display: string | null;
      avatar: string | null;
    }
  | { kind: "verified"; claim_kind: string; subject: string; state: string }
  | {
      kind: "voice-offer";
      channel: string;
      mode: string;
      token: string;
      room: string | null;
      endpoint: string | null;
    }
  | {
      kind: "voice-state";
      channel: string;
      user: string;
      action: string;
      muted: boolean;
      deaf: boolean;
      speaking: boolean;
    }
  | { kind: "voice-desc"; channel: string; sdp: string }
  | { kind: "voice-cand"; channel: string; candidate: string }
  | { kind: "error"; code: string; text: string }
  | { kind: "closed"; reason: string }
  | { kind: "raw"; line: string };

export async function connect(host: string, account: string, password: string, mode: Mode) {
  // The desktop page is served from the app bundle, not by the network, so the
  // §13 media endpoints need an explicit origin before any upload or fetch.
  // Doing it here means every call site gets it, connect and reconnect alike.
  if (IS_TAURI) {
    const cfg = await clientConfig().catch(() => null);
    setMediaBase(host, cfg?.media_base ?? null);
  }
  return invoke("connect", { host, account, password, mode });
}

export type ClientConfig = {
  allow_insecure: boolean;
  default_host: string | null;
  /// Override the HTTP origin serving `/media`; unset ⇒ derive `https://<host>`.
  media_base: string | null;
  config_path: string | null;
};

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

// §10.3 display profiles. `profileSet` omits a key to leave that field
// unchanged, sends "" to clear it, or a value to set it (avatar = a blob hash).
export function profileSet(opts: { display?: string; avatar?: string }) {
  return invoke("profile_set", opts);
}
export function profilesQuery(accounts: string[]) {
  return invoke("profiles_query", { accounts });
}

// §10.5 account verification.
export function verifyEmail(address: string) {
  return invoke("verify_email", { address });
}
export function verifyBirthday(date: string) {
  return invoke("verify_birthday", { date });
}
export function verifyConfirm(kind: string, code: string) {
  return invoke("verify_confirm", { kind, code });
}
export function verifyList() {
  return invoke("verify_list", {});
}

// §16 WEFT-RT voice signaling. The SDP/ICE payloads ride these control
// commands; the media path is a separate browser WebRTC connection (see
// `voice.svelte.ts`).
export function voiceJoin(channel: string) {
  return invoke("voice_join", { channel });
}
export function voiceLeave(channel: string) {
  return invoke("voice_leave", { channel });
}
export function voiceDesc(channel: string, sdp: string) {
  return invoke("voice_desc", { channel, sdp });
}
export function voiceCand(channel: string, candidate: string) {
  return invoke("voice_cand", { channel, candidate });
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

/// Request a page of history for `target`, older than `before` if given, or a
/// single thread's messages when `thread` (the root msgid) is set (§9.4).
export function history(target: string, before?: string, thread?: string) {
  return invoke("history", { target, before: before ?? null, thread: thread ?? null });
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

export function sendMessage(
  target: string,
  body: string,
  replyTo?: string,
  attachments?: string[],
  thread?: string,
) {
  return invoke("send_message", {
    target,
    body,
    replyTo: replyTo ?? null,
    attachments: attachments ?? [],
    thread: thread ?? null,
  });
}

// ---- §13 media ----

/** The per-session fetch bearer (from the `media-token` event); set on connect. */
let mediaBearer = "";
export function setMediaBearer(token: string) {
  mediaBearer = token;
}

export type UploadResult = {
  media: string; // weft-media://origin/hash
  thumb: string | null;
  width: number | null;
  height: number | null;
};

/**
 * Base HTTP origin for the §13 media endpoints.
 *
 * On the **web** the page is already served by the network, so same-origin is
 * right. On the **desktop** the page is served from the Tauri bundle, so
 * same-origin points at the app, not the server — the base has to be set
 * explicitly (see {@link setMediaBase}) or media silently 404s.
 */
let mediaBase = "";
function mediaOrigin(): string {
  if (IS_TAURI) return mediaBase;
  if (typeof window !== "undefined" && window.location) return window.location.origin;
  return "";
}

/**
 * Point the desktop client at the HTTP origin serving `/media`.
 *
 * `configured` wins when set (dev / reverse proxy on a nonstandard port);
 * otherwise it is derived as `https://<host>` with the QUIC port dropped —
 * weftd's HTTP listener is a different port, and in a real deployment the
 * network's DNS name fronts it on 443.
 */
export function setMediaBase(host: string, configured?: string | null) {
  if (configured) {
    mediaBase = configured.replace(/\/+$/, "");
    return;
  }
  const hostname = host.trim().replace(/^\w+:\/\//, "").split("/")[0].replace(/:\d+$/, "");
  mediaBase = hostname ? `https://${hostname}` : "";
}

/**
 * Upload a file to the network and return its content-addressed reference: a
 * single authed POST to `/media`, the session bearer authorizing it. Identical
 * on web and desktop — they differ only in where {@link mediaOrigin} points.
 */
export async function upload(file: File | Blob): Promise<UploadResult> {
  if (!mediaBearer) throw new Error("no media session");
  if (IS_TAURI && !mediaOrigin()) {
    throw new Error(
      "no media server configured — set `media_base` in client.toml if it isn't at https://<host>",
    );
  }
  const res = await fetch(`${mediaOrigin()}/media?t=${encodeURIComponent(mediaBearer)}`, {
    method: "POST",
    headers: { "Content-Type": (file as File).type || "application/octet-stream" },
    body: file,
  });
  if (!res.ok) throw new Error(`upload failed (${res.status})`);
  const j = await res.json();
  return { media: j.media, thumb: j.thumb ?? null, width: j.width ?? null, height: j.height ?? null };
}

/** Resolve a `weft-media://origin/hash` reference to a fetchable URL. */
export function mediaUrl(ref: string): string {
  return avatarUrl(mediaHash(ref));
}

/** The BLAKE3 hash portion of a `weft-media://origin/hash` reference. */
export function mediaHash(ref: string): string {
  const rest = ref.replace(/^weft-media:\/\//, "");
  return rest.slice(rest.indexOf("/") + 1);
}

/** §10.3 a fetchable URL for an avatar (or any) blob hash, home-network only. */
export function avatarUrl(hash: string): string {
  return `${mediaOrigin()}/media/${hash}?t=${encodeURIComponent(mediaBearer)}`;
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

/// §6.4 message search in a channel; results arrive as a BATCH of messages.
export function search(channel: string, query: string) {
  return invoke("search", { channel, query });
}

// ---- §9.4 custom emoji ----
export function emojiAdd(namespace: string, name: string, media: string) {
  return invoke("emoji_add", { namespace, name, media });
}
export function emojiRemove(namespace: string, name: string) {
  return invoke("emoji_remove", { namespace, name });
}
export function emojiList(namespace: string) {
  return invoke("emoji_list", { namespace });
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
export function roleCreate(
  scope: string,
  color: string,
  caps: string,
  hoist: boolean,
  position: number,
  name: string,
) {
  return invoke("role_create", { scope, color, caps, hoist, position, name });
}
/// Reorder roles: positions are set from the order of `names` (§6.5).
export function rolesReorder(scope: string, names: string[]) {
  return invoke("roles_reorder", { scope, order: names.join(",") });
}
export function roleDelete(scope: string, name: string) {
  return invoke("role_delete", { scope, name });
}
/// Rename a role in place — its members and granted caps come with it (§6.5).
export function roleRename(scope: string, old: string, name: string) {
  return invoke("role_rename", { scope, old, new: name });
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

/// Revoke every invite for a namespace at once (§6.5).
export function inviteRevokeAll(scope: string) {
  return invoke("invite_revoke_all", { scope });
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

/// List the moderation deny-list (mutes + bans) at a scope (§6.7). Answered as
/// a batch of `moderated` events (each a current mute/ban).
export function modList(scope: string) {
  return invoke("mod_list", { scope });
}

export function reportsResolve(reportId: string, action: string, note?: string) {
  return invoke("reports_resolve", { reportId, action, note: note ?? null });
}

export function part(channel: string) {
  return invoke("part", { channel });
}

export function channelCreate(channel: string, policy?: string, kind?: string) {
  return invoke("channel_create", { channel, policy: policy ?? null, kind: kind ?? null });
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
