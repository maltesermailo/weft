// The client session/identity + capability model — see
// docs/architecture/client-model-refactor.md. The §6.5/§6.6 roles layer split
// out to `$lib/roles/roles.svelte` (session imports `roleScopeOf`/`rolesAt`/
// `memberRoles` from there for its gates + mention check).
import { SvelteMap } from "svelte/reactivity";
import { store } from "$lib/store/store.svelte";
import * as weft from "$lib/transport/weft";
import { confirmSuccess } from "$lib/notifications/toasts.svelte";
import { view } from "$lib/navigation/view.svelte";
import { roleScopeOf, rolesAt, memberRoles } from "$lib/roles/roles.svelte";
import type { HandlerMap } from "$lib/sync/handler-map";

/// A server-resolved capability set at a scope (§10.4): `owner` (implicit
/// all-caps), `mod` (mute/ban/kick), and the raw cap list.
export interface Badge {
  owner: boolean;
  mod: boolean;
  list: string[];
}

/// The connection lifecycle state: pre-login, attempting, or live.
export type Status = "connect" | "connecting" | "online";

/**
 * The current user's session: identity + the server-resolved capability cache,
 * with the permission gates as methods. Caps are keyed "account|scope" and
 * arrive from `caps` events (the server resolves roles→caps; the client does
 * not). The gates walk the **scope** hierarchy over this cache — the caller
 * picks which scopes to check (channel → ns → operator `*`) — never roles.
 */
export class Session {
  /// The logged-in account handle (bare), set on `connected`.
  account = $state("");
  /// The home network name, set on `connected`.
  network = $state("");
  /// Connection lifecycle (drives the ConnectScreen gate).
  status = $state<Status>("connect");
  /// My own presence (§7 PRESENCE): online / away / dnd / …
  myStatus = $state("online");
  /// §10.5 my own verification claims, keyed by kind (email/birthday/…).
  verifications = $state<Record<string, { subject: string; state: string }>>({});
  verificationsLoaded = $state(false);
  /// Server-resolved caps, keyed "account|scope" (§10.4).
  readonly caps = new SvelteMap<string, Badge>();

  /// Caps for any account at a scope, or undefined if not fetched (badges).
  capsAt(account: string, scope: string): Badge | undefined {
    return this.caps.get(`${account}|${scope}`);
  }
  /// Does an account own/operate the scope (implicit all-caps)?
  ownerAt(account: string, scope: string): boolean {
    return this.caps.get(`${account}|${scope}`)?.owner ?? false;
  }

  /// Do *I* hold a capability at a scope? Owner implies all.
  can(cap: string, scope: string): boolean {
    const c = this.caps.get(`${this.account}|${scope}`);
    return !!c && (c.owner || c.list.includes(cap));
  }
  /// Do *I* hold moderation power (mute/ban/kick, or owner) at a scope?
  moderates(scope: string): boolean {
    const c = this.caps.get(`${this.account}|${scope}`);
    return !!c && (c.owner || c.mod);
  }
  /// Do *I* hold any `grant:*` delegation cap at a scope?
  canGrant(scope: string): boolean {
    const c = this.caps.get(`${this.account}|${scope}`);
    return !!c && (c.owner || c.list.some((x) => x.startsWith("grant:")));
  }
  /// Am I a network operator (owner at the `*` scope)?
  get isOperator(): boolean {
    return this.ownerAt(this.account, "*");
  }
}

// ---- caps read layer (was the layout's caps machinery) ----
// The server-resolved cap cache lives on `store.session.caps`; here is the
// on-demand fetch trigger and the pure readers / gates that don't depend on the
// active view. Role *definitions* + assignment live in `$lib/roles/roles.svelte`.

// In-flight `caps` fetches, so `ensureCapsAt` fires each (account, scope) once.
const capsInflight = new Set<string>();

/// Fetch an account's caps at a scope once (server resolves roles→caps).
export function ensureCapsAt(account: string, scope: string): void {
  if (!scope || !account) return;
  const key = `${account}|${scope}`;
  if (store.session.caps.has(key) || capsInflight.has(key)) return;
  capsInflight.add(key);
  weft.caps(account, scope).catch(() => capsInflight.delete(key));
}
/// Clear the in-flight marker once a `caps` response lands (reducer calls this).
export function capsResolved(account: string, scope: string): void {
  capsInflight.delete(`${account}|${scope}`);
}
/// Fetch caps only for a real channel scope (`#…`).
export const ensureCaps = (account: string, channel: string): void => {
  if (channel.startsWith("#")) ensureCapsAt(account, channel);
};
/// The resolved cap badge for an account at a channel (or undefined).
export const badgeFor = (account: string, channel: string) => store.session.capsAt(account, channel);

/// Is this account the owner/operator at the scope (implicit all-caps)?
export const isOwnerAt = (account: string, scope: string): boolean => store.session.ownerAt(account, scope);

/// Network staff (operator): holds `*`-scope authority. Fetched lazily; surfaced
/// as a "Staff" badge — never as server ownership.
export const isStaff = (account: string): boolean => {
  const a = account.replace(/^@/, "");
  ensureCapsAt(a, "*");
  return store.session.ownerAt(a, "*");
};

// ---- §10.4 permission gates ----
// operator (`*`) status is deliberately NOT consulted for namespaced channels —
// mirrors the server (context.rs): a network operator's god-mode is web-admin
// authority, never day-to-day power on someone else's server. At the network
// level (top-level channels) the scope *is* `*`, so operator power applies there.

// The real owner of the active namespace — the record's owner, NOT anyone who
// merely holds ns-admin caps (an operator holds them everywhere, but that's
// web-admin authority, not ownership of this server).
export const isNsOwner = (account: string): boolean =>
  !!view.activeServer && account.replace(/^@/, "") === (store.servers.get(view.activeServer)?.owner ?? "");

// Can I delete any message in the active channel (moderation delete-any)?
export function canModDelete(): boolean {
  const me = store.session.account;
  if (!view.active.startsWith("#")) return false;

  const nsScope = roleScopeOf(view.active);
  ensureCapsAt(me, view.active);
  ensureCapsAt(me, nsScope);
  return store.session.can("delete-any", view.active) || store.session.can("delete-any", nsScope);
}

// Do I hold moderation power (mute/ban/kick, or owner) in a channel's server?
// Namespaced channels never consult `*`; top-level channels honor operator caps
// because their scope *is* `*`. Gates every moderation surface.
export function canModerate(channel: string): boolean {
  const me = store.session.account;
  if (!channel.startsWith("#")) return false;

  const nsScope = roleScopeOf(channel);
  ensureCapsAt(me, channel);
  ensureCapsAt(me, nsScope);
  return store.session.moderates(channel) || store.session.moderates(nsScope);
}

// Do I hold a specific capability at the active server's scope (`ns:<server>`, or
// `*` at network level)? Owner/ns-admin implies every cap; operator (`*`) counts
// only at network level. The per-permission gate for server-menu actions.
export function serverCap(cap: string): boolean {
  const scope = view.activeServer ? `ns:${view.activeServer}` : "*";
  ensureCapsAt(store.session.account, scope);
  return store.session.can(cap, scope);
}

// Do I hold any `grant:*` delegation cap at the server scope? Gates the Roles tab.
export function serverCanGrant(): boolean {
  const scope = view.activeServer ? `ns:${view.activeServer}` : "*";
  ensureCapsAt(store.session.account, scope);
  return store.session.canGrant(scope);
}

// Server Settings is reachable with any moderation/administration capability —
// not plain member caps. Each tab then gates itself.
export function canOpenServerSettings(): boolean {
  return (
    isNsOwner(store.session.account) ||
    serverCanGrant() ||
    ["ns-admin", "ban", "mute", "kick", "reports", "chan-create", "policy", "manage-nicks"].some(serverCap)
  );
}

/// Does a body mention me, @everyone/@here, or a pingable role I hold at `ns`?
export function mentionsMe(body: string, ns: string): boolean {
  const me = store.session.account;
  if (!me) return false;
  if (new RegExp(`@${me}\\b`, "i").test(body) || /@(everyone|here)\b/i.test(body)) return true;
  const scope = ns ? `ns:${ns}` : "*";
  const mineIds = new Set(memberRoles[`${me}|${scope}`] ?? []);
  return rolesAt(scope).some(
    (r) =>
      r.pingable &&
      mineIds.has(r.id) &&
      new RegExp(`@${r.name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}\\b`, "i").test(body),
  );
}

/// This domain's wire-event handlers (§10.4 caps + §10.5 verification). Role
/// events live in `$lib/roles/roles.svelte`.
export const sessionHandlers: HandlerMap = {
  caps: (e) => {
    const set = e.caps ? e.caps.split(",") : [];
    store.session.caps.set(`${e.account}|${e.scope}`, {
      owner: set.includes("ns-admin") || set.includes("netblock"),
      mod: set.includes("mute") || set.includes("ban") || set.includes("kick"),
      list: set,
    });
    capsResolved(e.account, e.scope);
    confirmSuccess(`caps:${e.account}|${e.scope}`);
  },
  verified: (e) => {
    // §10.5 one of our own verification claims (email/birthday).
    store.session.verifications[e.claim_kind] = { subject: e.subject, state: e.state };
  },
};
