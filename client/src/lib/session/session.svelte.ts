// The client session/identity + capability model — see
// docs/architecture/client-model-refactor.md. The §6.5/§6.6 roles layer lives in
// `$lib/roles/roles.svelte` (session imports `roleScopeOf`/`roleStore` for its
// gates + mention check). File order: definitions → classes → operations → events.
import { SvelteMap } from "svelte/reactivity";
import { store } from "$lib/store/store.svelte";
import * as weft from "$lib/transport/weft";
import { confirmSuccess } from "$lib/notifications/toasts.svelte";
import { view } from "$lib/navigation/view.svelte";
import { roleScopeOf, roleStore } from "$lib/roles/roles.svelte";
import type { HandlerMap } from "$lib/sync/handler-map";

// ---- definitions ----

/// A server-resolved capability set at a scope (§10.4): `owner` (implicit
/// all-caps), `mod` (mute/ban/kick), and the raw cap list.
export interface Badge {
  owner: boolean;
  mod: boolean;
  list: string[];
}

/// The connection lifecycle state: pre-login, attempting, or live.
export type Status = "connect" | "connecting" | "online";

// ---- classes ----

/**
 * The current user's session: identity + the server-resolved capability cache,
 * with the on-demand fetch trigger and the permission gates as methods. Caps are
 * keyed "account|scope" and arrive from `caps` events (the server resolves
 * roles→caps; the client does not). The gates walk the **scope** hierarchy over
 * this cache — the caller picks which scopes to check (channel → ns → operator
 * `*`) — never roles.
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
  /// In-flight `caps` fetches, so `ensureCapsAt` fires each (account, scope) once.
  private capsInflight = new Set<string>();

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

  // ---- caps fetch layer (the on-demand trigger over the cache above) ----

  /// Fetch an account's caps at a scope once (server resolves roles→caps).
  ensureCapsAt(account: string, scope: string): void {
    if (!scope || !account) return;
    const key = `${account}|${scope}`;
    if (this.caps.has(key) || this.capsInflight.has(key)) return;
    this.capsInflight.add(key);
    weft.caps(account, scope).catch(() => this.capsInflight.delete(key));
  }
  /// Clear the in-flight marker once a `caps` response lands (reducer calls this).
  capsResolved(account: string, scope: string): void {
    this.capsInflight.delete(`${account}|${scope}`);
  }
  /// Fetch caps only for a real channel scope (`#…`).
  ensureCaps(account: string, channel: string): void {
    if (channel.startsWith("#")) this.ensureCapsAt(account, channel);
  }
  /// The resolved cap badge for an account at a channel (or undefined).
  badgeFor(account: string, channel: string): Badge | undefined {
    return this.capsAt(account, channel);
  }

  /// Is this account the owner/operator at the scope (implicit all-caps)?
  isOwnerAt(account: string, scope: string): boolean {
    return this.ownerAt(account, scope);
  }

  /// Network staff (operator): holds `*`-scope authority. Fetched lazily; surfaced
  /// as a "Staff" badge — never as server ownership.
  isStaff(account: string): boolean {
    const a = account.replace(/^@/, "");
    this.ensureCapsAt(a, "*");
    return this.ownerAt(a, "*");
  }

  // ---- §10.4 permission gates ----
  // operator (`*`) status is deliberately NOT consulted for namespaced channels —
  // mirrors the server (context.rs): a network operator's god-mode is web-admin
  // authority, never day-to-day power on someone else's server. At the network
  // level (top-level channels) the scope *is* `*`, so operator power applies there.

  // The real owner of the active namespace — the record's owner, NOT anyone who
  // merely holds ns-admin caps.
  isNsOwner(account: string): boolean {
    return !!view.activeServer && account.replace(/^@/, "") === (store.servers.get(view.activeServer)?.owner ?? "");
  }

  // Can I delete any message in the active channel (moderation delete-any)?
  canModDelete(): boolean {
    const me = this.account;
    if (!view.active.startsWith("#")) return false;

    const nsScope = roleScopeOf(view.active);
    this.ensureCapsAt(me, view.active);
    this.ensureCapsAt(me, nsScope);
    return this.can("delete-any", view.active) || this.can("delete-any", nsScope);
  }

  // Do I hold moderation power (mute/ban/kick, or owner) in a channel's server?
  canModerate(channel: string): boolean {
    const me = this.account;
    if (!channel.startsWith("#")) return false;

    const nsScope = roleScopeOf(channel);
    this.ensureCapsAt(me, channel);
    this.ensureCapsAt(me, nsScope);
    return this.moderates(channel) || this.moderates(nsScope);
  }

  // Do I hold a specific capability at the active server's scope (`ns:<server>`,
  // or `*` at network level)? The per-permission gate for server-menu actions.
  serverCap(cap: string): boolean {
    const scope = view.activeServer ? `ns:${view.activeServer}` : "*";
    this.ensureCapsAt(this.account, scope);
    return this.can(cap, scope);
  }

  // Do I hold any `grant:*` delegation cap at the server scope? Gates the Roles tab.
  serverCanGrant(): boolean {
    const scope = view.activeServer ? `ns:${view.activeServer}` : "*";
    this.ensureCapsAt(this.account, scope);
    return this.canGrant(scope);
  }

  // Server Settings is reachable with any moderation/administration capability.
  canOpenServerSettings(): boolean {
    return (
      this.isNsOwner(this.account) ||
      this.serverCanGrant() ||
      ["ns-admin", "ban", "mute", "kick", "reports", "chan-create", "policy", "manage-nicks"].some((c) => this.serverCap(c))
    );
  }

  /// Does a body mention me, @everyone/@here, or a pingable role I hold at `ns`?
  mentionsMe(body: string, ns: string): boolean {
    const me = this.account;
    if (!me) return false;
    if (new RegExp(`@${me}\\b`, "i").test(body) || /@(everyone|here)\b/i.test(body)) return true;
    const scope = ns ? `ns:${ns}` : "*";
    const mineIds = new Set(roleStore.memberRoles[`${me}|${scope}`] ?? []);
    return roleStore.rolesAt(scope).some(
      (r) =>
        r.pingable &&
        mineIds.has(r.id) &&
        new RegExp(`@${r.name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}\\b`, "i").test(body),
    );
  }
}

// ---- events ----

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
    store.session.capsResolved(e.account, e.scope);
    confirmSuccess(`caps:${e.account}|${e.scope}`);
  },
  verified: (e) => {
    // §10.5 one of our own verification claims (email/birthday).
    store.session.verifications[e.claim_kind] = { subject: e.subject, state: e.state };
  },
};
