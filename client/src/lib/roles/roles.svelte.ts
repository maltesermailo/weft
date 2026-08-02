// The §6.5/§6.6 roles domain (split out of the session model): role definitions
// by scope, the fetch subsystem, the RolesTab editor, lazy hydration + name
// colors, and role assignment. Depends on store/namespaces/notifications — never
// on the session model (session imports `roleScopeOf`/`roleStore` from here).
// File order: definitions → classes → operations → events.
import { store } from "$lib/store/store.svelte";
import * as weft from "$lib/transport/weft";
import * as md from "$lib/rendering/markdown";
import { Role } from "$lib/roles/role.svelte";
import type { Membership } from "$lib/membership/membership.svelte";
import { confirmSuccess, toast, expectSuccess } from "$lib/notifications/toasts.svelte";
import { EVERYONE_ROLE } from "$lib/constants";
import { view } from "$lib/navigation/view.svelte";
import { ui } from "$lib/ui/ui.svelte";
import type { HandlerMap } from "$lib/sync/handler-map";

// ---- classes ----

/**
 * The roles domain state + operations. A single reactive instance (`roleStore`).
 * Role *definitions* live here for the `*`/`#chan` scopes; namespace roles live
 * on `Server.roles` (single source) and are read through `rolesAt`.
 */
export class RoleStore {
  /// Explicit role definitions by scope (`*` / `#chan`; ns roles are on Server).
  /// Written by the reducer's ROLE batch; read via `rolesAt`.
  rolesByScope = $state<Record<string, Role[]>>({});
  /// Explicit role membership keyed `account|scope`, from ROLE-MEMBER (a role is
  /// worn because it was assigned, never inferred from caps). Written by the reducer.
  memberRoles = $state<Record<string, string[]>>({});
  /// §11.11 federated authors whose roles we've already fetched (`who|scope`).
  fedRolesFetched = new Set<string>();

  // §6.5 grants arrive in `gr…`-id BATCHes; a per-request scope queue records which
  // scope each answer belongs to, and the buffer accumulates rows until the reducer
  // flushes them. (Role definitions are model-owned now — client-core buffers the
  // `r…` ROLE batch and emits `role-list`/`member-roles` diffs; no TS buffer/cursor.)
  grantBuf: { subject: string; caps: string[] }[] = [];
  grantFetchQueue: string[] = [];

  // The "new role" form draft (RolesTab binds `roleStore.roleDraft.name`, …).
  roleDraft = $state<{
    name: string;
    color: string;
    caps: string[];
    hoist: boolean;
    pingable: boolean;
  }>({ name: "", color: "#5865f2", caps: [], hoist: false, pingable: false });

  // Lazy-hydration dedup sets (per (account,scope) / per scope).
  private memberRolesFetched = new Set<string>();
  private rolesFetched = new Set<string>();

  /// The role definitions at a scope (ns → `Server.roles`, else the by-scope map).
  rolesAt(scope: string): Role[] {
    return scope.startsWith("ns:") ? (store.servers.get(scope.slice(3))?.roles ?? []) : (this.rolesByScope[scope] ?? []);
  }
  /// Resolve a role id to its definition at a scope (v0.13: id is the identity).
  roleById(scope: string, id: string): Role | undefined {
    return this.rolesAt(scope).find((r) => r.id === id);
  }
  /// The role definitions an account is assigned at a scope (matched by id).
  rolesOf(account: string, scope: string): Role[] {
    const ids = new Set(this.memberRoles[`${account}|${scope}`] ?? []);
    return this.rolesAt(scope).filter((r) => ids.has(r.id));
  }
  /// Fetch an account's explicit role membership at a scope (fills `memberRoles`).
  fetchMemberRoles(account: string, scope: string): void {
    weft.rolesOfAccount(scope, account).catch(() => {});
  }

  fetchRoles(scope: string): void {
    if (!scope) return;
    // The `r…` batch response is buffered + flushed by the client-core model,
    // which routes each ROLE to its own scope — no client-side scope cursor.
    weft.roles(scope).catch(() => {});
  }
  fetchGrants(scope: string): void {
    if (!scope) return;
    this.grantFetchQueue.push(scope);
    weft.grantsAt(scope).catch(() => this.grantFetchQueue.pop());
  }
  createRoleAt(
    scope: string,
    name: string,
    color: string,
    caps: string,
    hoist = false,
    pingable = false,
    position = 0,
  ): Promise<unknown> {
    return weft.roleCreate(scope, color, caps, hoist, pingable, position, name);
  }
  deleteRoleAt(scope: string, roleId: string): Promise<unknown> {
    return weft.roleDelete(scope, roleId);
  }

  // ---- §6.6 role editor (RolesTab) ----
  // Roles live at the active namespace's scope (or the network `*` off-server).
  nsRoleScope(): string {
    return view.activeServer ? `ns:${view.activeServer}` : "*";
  }

  toggleNewRoleCap(c: string): void {
    this.roleDraft.caps = this.roleDraft.caps.includes(c)
      ? this.roleDraft.caps.filter((x) => x !== c)
      : [...this.roleDraft.caps, c];
  }

  createRole(): void {
    // A role may hold zero permissions (granted later); only a name is required.
    if (!this.roleDraft.name.trim()) return;

    // Append at the bottom of the ordered list.
    const position = this.rolesAt(this.nsRoleScope()).length;

    this.createRoleAt(this.nsRoleScope(), this.roleDraft.name.trim(), this.roleDraft.color, this.roleDraft.caps.join(","), this.roleDraft.hoist, this.roleDraft.pingable, position)
      .then(() => {
        this.roleDraft.name = "";
        this.roleDraft.caps = [];
        this.roleDraft.hoist = false;
        this.roleDraft.pingable = false;
      })
      .catch((e) => toast(String(e), "error"));
  }

  // The implicit @everyone role's current caps at the active server (or []).
  everyoneCaps(): string[] {
    return this.rolesAt(this.nsRoleScope()).find((r) => r.name === EVERYONE_ROLE)?.caps ?? [];
  }

  // Set the @everyone baseline. Non-empty → upsert the reserved role; empty →
  // delete it (the server rejects an empty cap list, and "no role" = no baseline).
  setEveryoneCaps(caps: string[]): void {
    const scope = this.nsRoleScope();
    if (caps.length) {
      this.createRoleAt(scope, EVERYONE_ROLE, "#99aab5", caps.join(","), false, false, 0).catch((e) => toast(String(e), "error"));
      return;
    }

    const everyone = this.rolesAt(scope).find((r) => r.name === EVERYONE_ROLE);
    if (everyone) this.deleteRoleAt(scope, everyone.id).catch((e) => toast(String(e), "error"));
  }

  // Move a role up/down in the ordered list, then persist the new order (§6.5).
  moveRole(roleId: string, dir: -1 | 1): void {
    const scope = this.nsRoleScope();
    const list = [...this.rolesAt(scope)];
    const i = list.findIndex((r) => r.id === roleId);
    const j = i + dir;
    if (i < 0 || j < 0 || j >= list.length) return;

    [list[i], list[j]] = [list[j], list[i]];
    weft.rolesReorder(scope, list.map((r) => r.id)).catch((e) => toast(String(e), "error"));
  }

  // Persist an arbitrary order (drag-and-drop) — positions follow the id list.
  reorderRoles(ids: string[]): void {
    const scope = this.nsRoleScope();
    weft.rolesReorder(scope, ids).catch((e) => toast(String(e), "error"));
  }

  // Apply a role edit — one ROLE UPDATE by id replaces every field + name (§6.5).
  saveRole(
    role: Role,
    patch: { name: string; color: string; caps: string[]; hoist: boolean; pingable: boolean },
  ): void {
    const scope = this.nsRoleScope();
    const name = patch.name.trim() || role.name;

    weft
      .roleUpdate(scope, role.id, patch.color, patch.caps.join(","), patch.hoist, patch.pingable, role.position, name)
      .catch((e) => toast(String(e), "error"));
  }

  deleteRole(roleId: string): void {
    this.deleteRoleAt(this.nsRoleScope(), roleId).catch((e) => toast(String(e), "error"));
  }

  // ---- lazy role hydration (member list + name colors) ----
  // Eagerly fetch a member's namespace roles once (deduped per (account, scope)).
  ensureMemberRoles(account: string): void {
    const scope = this.nsRoleScope();
    const key = `${account}|${scope}`;
    if (this.memberRolesFetched.has(key)) return;

    this.memberRolesFetched.add(key);
    this.fetchMemberRoles(account, scope);
  }
  // Eagerly fetch a scope's role *definitions* once (deduped per scope).
  ensureRoles(scope: string): void {
    if (!scope || this.rolesFetched.has(scope)) return;

    this.rolesFetched.add(scope);
    this.fetchRoles(scope);
  }

  // The color to tint an account's name with — their highest assigned role at
  // the active namespace (Discord-style), excluding the implicit @everyone.
  nameColor(account: string): string {
    const scope = roleScopeOf(view.active);
    if (!scope.startsWith("ns:")) return "";

    this.ensureMemberRoles(account);
    this.ensureRoles(scope);

    const top = this.rolesOf(account, scope).find((r) => r.name !== EVERYONE_ROLE);
    return top?.color ?? "";
  }

  // ---- §6.6 role assignment (profile card + members roster) ----
  // Assign / unassign a role at the active channel's role scope (ProfileCard).
  assignRoleTo(acct: string, role: Role): void {
    const scope = roleScopeOf(view.active);
    expectSuccess(`roles:${acct}|${scope}`, `Roles updated for ${acct}`);
    weft
      .roleAssign(scope, acct, role.id)
      .then(() => this.fetchMemberRoles(acct, scope)) // ROLES-OF queues after ASSIGN → fresh list
      .catch((e) => toast(String(e), "error"));
  }
  unassignRoleFrom(acct: string, role: Role): void {
    const scope = roleScopeOf(view.active);
    expectSuccess(`roles:${acct}|${scope}`, `Roles updated for ${acct}`);
    weft
      .roleUnassign(scope, acct, role.id)
      .then(() => this.fetchMemberRoles(acct, scope))
      .catch((e) => toast(String(e), "error"));
  }

  // In-line role editing for the members directory (optimistic → reconcile).
  private reconcileRoster(ns: string): void {
    setTimeout(() => {
      if (ui.nsSettingsOpen && ui.nsTab === "members") store.server(ns).fetchMembers();
    }, 500);
  }
  private memberRow(ns: string, account: string): Membership | undefined {
    return store.servers.get(ns)?.member(account);
  }

  assignNsRole(account: string, roleId: string): void {
    const scope = this.nsRoleScope();
    const ns = view.activeServer;
    const m = this.memberRow(ns, account);
    if (m && !m.roleIds.includes(roleId)) m.roleIds = [...m.roleIds, roleId];

    expectSuccess(`roles:${account}|${scope}`, `Roles updated for ${account}`);
    weft
      .roleAssign(scope, account, roleId)
      .then(() => {
        this.reconcileRoster(ns);
        this.fetchMemberRoles(account, scope);
      })
      .catch((e) => {
        toast(String(e), "error");
        store.server(ns).fetchMembers();
      });
  }
  unassignNsRole(account: string, roleId: string): void {
    const scope = this.nsRoleScope();
    const ns = view.activeServer;
    const m = this.memberRow(ns, account);
    if (m) m.roleIds = m.roleIds.filter((r) => r !== roleId);

    expectSuccess(`roles:${account}|${scope}`, `Roles updated for ${account}`);
    weft
      .roleUnassign(scope, account, roleId)
      .then(() => {
        this.reconcileRoster(ns);
        this.fetchMemberRoles(account, scope); // regroup the member-list sidebar too
      })
      .catch((e) => {
        toast(String(e), "error");
        store.server(ns).fetchMembers();
      });
  }
}

/// The roles domain singleton.
export const roleStore = new RoleStore();

// ---- operations ----

/// The role/authority scope for a channel: its namespace if it has one, else `*`.
/// Pure — kept free (no store state).
export const roleScopeOf = (channel: string): string => {
  const ns = channel.match(/^#([^/]+)\//)?.[1] ?? "";
  return ns ? `ns:${ns}` : "*";
};

// ---- events ----

/// This domain's wire-event handlers (§6.5 roles). Role definitions + membership
/// are model-owned now (client-core buffers the `r…` ROLE batch and emits these
/// diffs on its end); `grant-info` still buffers on `roleStore` (the reducer
/// flushes the `gr…` grant BATCH into `store.grants`).
export const rolesHandlers: HandlerMap = {
  // Model diff: a scope's full role list. Route by scope (ns → Server, else
  // by-scope), rebuild `Role` instances, and drop the md cache (role names/colors
  // feed mention rendering).
  "role-list": (e) => {
    const roles = e.roles.map((r) => new Role(r));
    if (e.scope.startsWith("ns:")) store.server(e.scope.slice(3)).roles = roles;
    else roleStore.rolesByScope[e.scope] = roles;
    md.clearMdCache();
  },
  "member-roles": (e) => {
    roleStore.memberRoles[`${e.account}|${e.scope}`] = e.roles;
    confirmSuccess(`roles:${e.account}|${e.scope}`);
  },
  "grant-info": (e) => {
    roleStore.grantBuf.push({ subject: e.subject, caps: e.caps ? e.caps.split(",") : [] });
  },
};
