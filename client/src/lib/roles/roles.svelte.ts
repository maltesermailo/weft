// The §6.5/§6.6 roles domain, split out of the session model: role definitions
// by scope, the fetch subsystem, the RolesTab editor, lazy hydration + name
// colors, and role assignment. Depends on the store/namespaces/notifications —
// never on the session model (session imports `roleScopeOf`/`rolesAt`/
// `memberRoles` from here, one direction).
import { store } from "$lib/store/store.svelte";
import * as weft from "$lib/transport/weft";
import { Role } from "$lib/roles/role.svelte";
import type { Membership } from "$lib/membership/membership.svelte";
import { confirmSuccess, toast, expectSuccess } from "$lib/notifications/toasts.svelte";
import { EVERYONE_ROLE } from "$lib/constants";
import { view } from "$lib/navigation/view.svelte";
import { ui } from "$lib/ui/ui.svelte";
import type { HandlerMap } from "$lib/sync/handler-map";

/// Explicit role definitions by scope. Namespace roles live on `Server.roles`
/// (single source); the `*` (operator) and `#chan` (override) scopes stay here.
/// `rolesAt` is the unified read. Written by the reducer's ROLE batch.
export const rolesByScope = $state<Record<string, Role[]>>({});

/// Explicit role membership keyed `account|scope`, from ROLE-MEMBER (a role is
/// worn because it was assigned, never inferred from caps). Written by the reducer.
export const memberRoles = $state<Record<string, string[]>>({});

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

/// This domain's wire-event handlers (§6.5 roles). The `role` / `grant-info`
/// rows buffer here; the reducer flushes each BATCH into `rolesByScope` /
/// `store.grants` (it owns the batch cursor).
export const rolesHandlers: HandlerMap = {
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
};
