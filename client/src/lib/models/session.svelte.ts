// The client domain model — see docs/architecture/client-model-refactor.md.
import { SvelteMap } from "svelte/reactivity";
import { store } from "./store.svelte";
import * as weft from "$lib/weft";
import type { Role } from "./role.svelte";

/// A server-resolved capability set at a scope (§10.4): `owner` (implicit
/// all-caps), `mod` (mute/ban/kick), and the raw cap list.
export interface Badge {
  owner: boolean;
  mod: boolean;
  list: string[];
}

/**
 * The current user's session: identity + the server-resolved capability cache,
 * with the permission gates as methods. Caps are keyed "account|scope" and
 * arrive from `caps` events (the server resolves roles→caps; the client does
 * not). The gates walk the **scope** hierarchy over this cache — the caller
 * picks which scopes to check (channel → ns → operator `*`) — never roles.
 *
 * Replaces the `capsFor` record + the free gate functions in `+page.svelte`.
 */
/// The connection lifecycle state: pre-login, attempting, or live.
export type Status = "connect" | "connecting" | "online";

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

// ---- caps/roles read layer (was the layout's caps machinery) ----
// The server-resolved cap cache lives on `store.session.caps`; here are the
// on-demand fetch trigger, the role definitions by scope, and the pure readers /
// gates that don't depend on the active view. The fetch/batch machinery (queues,
// `fetchRoles`, `ensureRoles`, …) travels with the reducer. Module singletons,
// mutated in place so they stay reactive across a bare import.

/// Explicit role definitions by scope. Namespace roles live on `Server.roles`
/// (single source); the `*` (operator) and `#chan` (override) scopes stay here.
/// `rolesAt` is the unified read. Written by the reducer's ROLE batch.
export const rolesByScope = $state<Record<string, Role[]>>({});

/// Explicit role membership keyed `account|scope`, from ROLE-MEMBER (a role is
/// worn because it was assigned, never inferred from caps). Written by the reducer.
export const memberRoles = $state<Record<string, string[]>>({});

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

/// The role/authority scope for a channel: its namespace if it has one, else `*`.
export const roleScopeOf = (channel: string): string => {
  const ns = channel.match(/^#([^/]+)\//)?.[1] ?? "";
  return ns ? `ns:${ns}` : "*";
};

/// The role definitions at a scope (ns → `Server.roles`, else the by-scope map).
export const rolesAt = (scope: string): Role[] =>
  scope.startsWith("ns:") ? (store.servers.get(scope.slice(3))?.roles ?? []) : (rolesByScope[scope] ?? []);

/// Resolve a role id to its definition at a scope (v0.13: id is the identity).
export const roleById = (scope: string, id: string): Role | undefined =>
  rolesAt(scope).find((r) => r.id === id);

/// Is this account the owner/operator at the scope (implicit all-caps)?
export const isOwnerAt = (account: string, scope: string): boolean => store.session.ownerAt(account, scope);

/// Network staff (operator): holds `*`-scope authority. Fetched lazily; surfaced
/// as a "Staff" badge — never as server ownership.
export const isStaff = (account: string): boolean => {
  const a = account.replace(/^@/, "");
  ensureCapsAt(a, "*");
  return store.session.ownerAt(a, "*");
};

/// The role definitions an account is assigned at a scope (matched by id).
export function rolesOf(account: string, scope: string): Role[] {
  const ids = new Set(memberRoles[`${account}|${scope}`] ?? []);
  return rolesAt(scope).filter((r) => ids.has(r.id));
}

/// §11.11 federated authors whose roles we've already fetched (`who|scope`).
export const fedRolesFetched = new Set<string>();
/// Fetch an account's explicit role membership at a scope (fills `memberRoles`).
export function fetchMemberRoles(account: string, scope: string): void {
  weft.rolesOfAccount(scope, account).catch(() => {});
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
