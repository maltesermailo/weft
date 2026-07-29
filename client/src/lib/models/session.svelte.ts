// The client domain model — see docs/architecture/client-model-refactor.md.
import { SvelteMap } from "svelte/reactivity";

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
export class Session {
  /// The logged-in account handle (bare), set on `connected`.
  account = $state("");
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
