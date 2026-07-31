// The client domain model — see docs/architecture/client-model-refactor.md.
import { SvelteMap } from "svelte/reactivity";
import { store } from "./store.svelte";
import * as weft from "$lib/weft";
import { Role } from "./role.svelte";
import type { Membership } from "./membership.svelte";
import { confirmSuccess, toast, expectSuccess } from "$lib/toasts.svelte";
import { EVERYONE_ROLE } from "$lib/constants";
import { view } from "$lib/view.svelte";
import { ui } from "$lib/ui.svelte";
import type { HandlerMap } from "$lib/sync/handler-map";

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

// ---- §6.5 role / grant fetch subsystem ----
// Roles arrive in `r…`-id BATCHes, grants in `gr…` BATCHes; a per-request scope
// queue records which scope each answer belongs to. The buffers accumulate the
// streamed rows until the reducer flushes them (mutated in place — never
// reassigned — so they stay `const` exports).
export const roleBuf: Role[] = [];
export const roleFetchQueue: string[] = [];
export const grantBuf: { subject: string; caps: string[] }[] = [];
export const grantFetchQueue: string[] = [];

export function fetchRoles(scope: string): void {
  if (!scope) return;
  roleFetchQueue.push(scope);
  weft.roles(scope).catch(() => roleFetchQueue.pop());
}
export function fetchGrants(scope: string): void {
  if (!scope) return;
  grantFetchQueue.push(scope);
  weft.grantsAt(scope).catch(() => grantFetchQueue.pop());
}
export function createRoleAt(
  scope: string,
  name: string,
  color: string,
  caps: string,
  hoist = false,
  pingable = false,
  position = 0,
): Promise<unknown> {
  roleFetchQueue.push(scope);
  return weft.roleCreate(scope, color, caps, hoist, pingable, position, name);
}
export function deleteRoleAt(scope: string, roleId: string): Promise<unknown> {
  roleFetchQueue.push(scope);
  return weft.roleDelete(scope, roleId);
}

// ---- §6.6 role editor (RolesTab) ----
// Roles live at the active namespace's scope (or the network `*` off-server).
export const nsRoleScope = (): string => (view.activeServer ? `ns:${view.activeServer}` : "*");

// The "new role" form draft. A single reactive object so the tab binds fields
// directly (`roleDraft.name`, …) instead of five separate ctx entries.
export const roleDraft = $state<{
  name: string;
  color: string;
  caps: string[];
  hoist: boolean;
  pingable: boolean;
}>({ name: "", color: "#5865f2", caps: [], hoist: false, pingable: false });

export const toggleNewRoleCap = (c: string): void => {
  roleDraft.caps = roleDraft.caps.includes(c) ? roleDraft.caps.filter((x) => x !== c) : [...roleDraft.caps, c];
};

export function createRole(): void {
  // A role may hold zero permissions (granted later); only a name is required.
  if (!roleDraft.name.trim()) return;

  // Append at the bottom of the ordered list.
  const position = rolesAt(nsRoleScope()).length;

  createRoleAt(nsRoleScope(), roleDraft.name.trim(), roleDraft.color, roleDraft.caps.join(","), roleDraft.hoist, roleDraft.pingable, position)
    .then(() => {
      roleDraft.name = "";
      roleDraft.caps = [];
      roleDraft.hoist = false;
      roleDraft.pingable = false;
    })
    .catch((e) => toast(String(e), "error"));
}

// The implicit @everyone role's current caps at the active server (or []).
export const everyoneCaps = (): string[] => rolesAt(nsRoleScope()).find((r) => r.name === EVERYONE_ROLE)?.caps ?? [];

// Set the @everyone baseline. Non-empty → upsert the reserved role; empty →
// delete it (the server rejects an empty cap list, and "no role" = no
// baseline). It's never assigned or hoisted.
export function setEveryoneCaps(caps: string[]): void {
  const scope = nsRoleScope();
  // Non-empty upserts the @everyone role by name (ROLE CREATE matches it by
  // name); empty deletes it — deletion addresses the role by its id (v0.13).
  if (caps.length) {
    createRoleAt(scope, EVERYONE_ROLE, "#99aab5", caps.join(","), false, false, 0).catch((e) => toast(String(e), "error"));
    return;
  }

  const everyone = rolesAt(scope).find((r) => r.name === EVERYONE_ROLE);
  if (everyone) deleteRoleAt(scope, everyone.id).catch((e) => toast(String(e), "error"));
}

// Move a role up/down in the ordered list, then persist the new order (§6.5).
// Addressed by the role id (names aren't unique, v0.13).
export function moveRole(roleId: string, dir: -1 | 1): void {
  const scope = nsRoleScope();
  const list = [...rolesAt(scope)];
  const i = list.findIndex((r) => r.id === roleId);
  const j = i + dir;
  if (i < 0 || j < 0 || j >= list.length) return;

  [list[i], list[j]] = [list[j], list[i]];
  roleFetchQueue.push(scope);
  weft.rolesReorder(scope, list.map((r) => r.id)).catch((e) => toast(String(e), "error"));
}

// Persist an arbitrary order (drag-and-drop) — positions follow the id list.
export function reorderRoles(ids: string[]): void {
  const scope = nsRoleScope();
  roleFetchQueue.push(scope);
  weft.rolesReorder(scope, ids).catch((e) => toast(String(e), "error"));
}

// Apply a role edit. v0.13: a single ROLE UPDATE addressed by the role's id
// replaces every field and carries a name change (keeping members + issued
// caps) — no separate RENAME + upsert (§6.5).
export function saveRole(
  role: Role,
  patch: { name: string; color: string; caps: string[]; hoist: boolean; pingable: boolean },
): void {
  const scope = nsRoleScope();
  const name = patch.name.trim() || role.name;

  // Zero permissions is valid (a cosmetic/hoist role, or perms granted later).
  roleFetchQueue.push(scope);
  weft
    .roleUpdate(scope, role.id, patch.color, patch.caps.join(","), patch.hoist, patch.pingable, role.position, name)
    .catch((e) => toast(String(e), "error"));
}

export function deleteRole(roleId: string): void {
  deleteRoleAt(nsRoleScope(), roleId).catch((e) => toast(String(e), "error"));
}

// ---- lazy role hydration (member list + name colors) ----
// Eagerly fetch a member's namespace roles once, so the member list can group
// by hoisted role without opening each profile. Deduped per (account, scope).
const memberRolesFetched = new Set<string>();
export function ensureMemberRoles(account: string): void {
  const scope = nsRoleScope();
  const key = `${account}|${scope}`;
  if (memberRolesFetched.has(key)) return;

  memberRolesFetched.add(key);
  fetchMemberRoles(account, scope);
}
// Eagerly fetch a scope's role *definitions* (names/colors/hoist) once, so the
// member list can group by hoisted role on open — not only after a profile or
// the perms modal happens to fetch them. Deduped per scope.
const rolesFetched = new Set<string>();
export function ensureRoles(scope: string): void {
  if (!scope || rolesFetched.has(scope)) return;

  rolesFetched.add(scope);
  fetchRoles(scope);
}

// The color to tint an account's name with — their highest assigned role at
// the active namespace (Discord-style), excluding the implicit @everyone.
// "" ⇒ no colored role, render in the default text color. Fetches the member's
// roles + the scope's role defs lazily so it resolves on next paint.
export function nameColor(account: string): string {
  const scope = roleScopeOf(view.active);
  if (!scope.startsWith("ns:")) return "";

  ensureMemberRoles(account);
  ensureRoles(scope);

  const top = rolesOf(account, scope).find((r) => r.name !== EVERYONE_ROLE);
  return top?.color ?? "";
}

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

// ---- §6.6 role assignment (profile card + members roster) ----
// Assign / unassign a role at the active channel's role scope (ProfileCard).
export function assignRoleTo(acct: string, role: Role): void {
  const scope = roleScopeOf(view.active);
  // Success is confirmed by the resulting ROLE-MEMBER event; a missing-cap
  // failure never confirms and its ERR toasts.
  expectSuccess(`roles:${acct}|${scope}`, `Roles updated for ${acct}`);
  weft
    .roleAssign(scope, acct, role.id)
    .then(() => fetchMemberRoles(acct, scope)) // ROLES-OF queues after ASSIGN → fresh list
    .catch((e) => toast(String(e), "error"));
}
export function unassignRoleFrom(acct: string, role: Role): void {
  const scope = roleScopeOf(view.active);
  expectSuccess(`roles:${acct}|${scope}`, `Roles updated for ${acct}`);
  weft
    .roleUnassign(scope, acct, role.id)
    .then(() => fetchMemberRoles(acct, scope))
    .catch((e) => toast(String(e), "error"));
}

// In-line role editing for the members directory. Both mutate the roster
// optimistically then reconcile against the server truth shortly after — a
// rejected change simply snaps back on the refetch (and its ERR toasts).
function reconcileRoster(ns: string): void {
  setTimeout(() => {
    if (ui.nsSettingsOpen && ui.nsTab === "members") store.server(ns).fetchMembers();
  }, 500);
}
const memberRow = (ns: string, account: string): Membership | undefined => store.servers.get(ns)?.member(account);

// v0.13: addressed by the role id. The roster's `roleIds` is a list of ids, so
// the optimistic update adds/removes the same id.
export function assignNsRole(account: string, roleId: string): void {
  const scope = nsRoleScope();
  const ns = view.activeServer;
  const m = memberRow(ns, account);
  if (m && !m.roleIds.includes(roleId)) m.roleIds = [...m.roleIds, roleId];

  expectSuccess(`roles:${account}|${scope}`, `Roles updated for ${account}`);
  weft
    .roleAssign(scope, account, roleId)
    // Refresh BOTH rosters: the settings members tab AND `memberRoles` (which the
    // member-list sidebar groups by hoisted role), else a hoisted assignment
    // wouldn't regroup the sidebar until the next interaction.
    .then(() => {
      reconcileRoster(ns);
      fetchMemberRoles(account, scope);
    })
    .catch((e) => {
      toast(String(e), "error");
      store.server(ns).fetchMembers();
    });
}
export function unassignNsRole(account: string, roleId: string): void {
  const scope = nsRoleScope();
  const ns = view.activeServer;
  const m = memberRow(ns, account);
  if (m) m.roleIds = m.roleIds.filter((r) => r !== roleId);

  expectSuccess(`roles:${account}|${scope}`, `Roles updated for ${account}`);
  weft
    .roleUnassign(scope, account, roleId)
    .then(() => {
      reconcileRoster(ns);
      fetchMemberRoles(account, scope); // regroup the member-list sidebar too
    })
    .catch((e) => {
      toast(String(e), "error");
      store.server(ns).fetchMembers();
    });
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

/// This domain's wire-event handlers (§10.4 caps + §6.5 roles). The `role` /
/// `grant-info` rows buffer here; the reducer flushes each BATCH into
/// `rolesByScope` / `store.grants` (it owns the batch cursor).
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
  role: (e) => {
    roleBuf.push(
      new Role({
        id: e.role,
        name: e.name,
        color: e.color,
        caps: e.caps ? e.caps.split(",") : [],
        hoist: e.hoist,
        pingable: e.pingable,
        position: e.position,
      }),
    );
  },
  "role-member": (e) => {
    memberRoles[`${e.account}|${e.scope}`] = e.roles ? e.roles.split(",") : [];
    confirmSuccess(`roles:${e.account}|${e.scope}`);
  },
  "grant-info": (e) => {
    grantBuf.push({ subject: e.subject, caps: e.caps ? e.caps.split(",") : [] });
  },
  verified: (e) => {
    // §10.5 one of our own verification claims (email/birthday).
    store.session.verifications[e.claim_kind] = { subject: e.subject, state: e.state };
  },
};
