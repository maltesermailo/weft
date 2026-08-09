// §10.3 identity / profile: thin views over the Account map plus the per-server
// nickname cache. Kept out of the layout so the reducer (nick events) and every
// component (avatars, names) share one source without the AppCtx bridge.
// File order: definitions → classes → operations → events.
import { SvelteMap } from "svelte/reactivity";
import { page } from "$app/state";
import * as nav from "$lib/navigation/nav";
import { store } from "$lib/store/store.svelte";
import { ui } from "$lib/ui/ui.svelte";
import { view } from "$lib/navigation/view.svelte";
import { toast } from "$lib/notifications/toasts.svelte";

import { roleScopeOf, roleStore } from "$lib/roles/roles.svelte";
import type { HandlerMap } from "$lib/sync/handler-map";
import * as weft from "$lib/transport/weft";

// ---- definitions ----

const POP_W = 340;
const POP_H = 360;

// ---- classes ----

/**
 * The §10.3 profile domain: the per-namespace nickname cache + the identity
 * reads/actions over it. A single reactive instance (`profileStore`). Pure
 * account/string helpers (peerOf/initials/avatarUrl/…) stay free operations.
 */
export class ProfileStore {
  /// Per-namespace server nicknames, keyed `scope|account` (SvelteMap → delete is
  /// reactive). Written by the `nick` wire-event handler.
  nicks = new SvelteMap<string, string>();
  /// Namespaces whose nicknames we've already pulled (fetch-once dedup).
  nicksFetched = new Set<string>();

  private activeServer(): string {
    return nav.viewFrom(page.route?.id, page.params).activeServer;
  }
  // Normalize a (possibly `account@network`) handle to the bare *local* account.
  private localTarget(handle: string): string {
    const at = handle.lastIndexOf("@");
    return at > 0 && handle.slice(at + 1) === store.session.network ? handle.slice(0, at) : handle;
  }

  /// An account's display name — the active server's nickname if set, else the
  /// global display name (§10.3: the canonical handle is always shown separately).
  displayName(acct: string): string {
    const ns = this.activeServer();
    const nick = ns ? this.nicks.get(nickKey(`ns:${ns}`, peerOf(acct))) : undefined;
    return nick || store.peekAccount(acct).displayName;
  }
  /// An account's server nickname at the active server, or "" (for editors).
  nickOf(acct: string): string {
    const ns = this.activeServer();
    return (ns ? this.nicks.get(nickKey(`ns:${ns}`, peerOf(acct))) : "") ?? "";
  }
  /// A user-facing label for a friend/group member ref (display name if local).
  friendLabel(user: string): string {
    const [acct, net] = user.split("@");
    return net === store.session.network ? this.displayName(acct) : user;
  }

  /// Fetch an account's profile once (deduped via `Account.requested`).
  queryProfile(acct: string): void {
    const a = peerOf(acct);
    if (!a) return;
    const rec = store.accountOf(a);
    if (!rec.requested) {
      rec.requested = true; // don't re-query
      weft.profilesQuery([a]).catch(() => {});
    }
  }

  // Set (or clear, with "") a per-namespace nickname (§10.3).
  setNick(scope: string, account: string, value: string): void {
    weft.nick(scope, account, value).catch((e) => toast(String(e), "error"));
  }
  // Set (or clear, with "") my own custom status (§10.3).
  setCustomStatus(text: string): void {
    weft.profileSet({ status: text }).catch((e) => toast(String(e), "error"));
  }

  // Open the anchored ProfileCard popover next to the clicked row (Discord-style;
  // centered fallback when there's no event), then hydrate the target's profile,
  // caps and roles for display.
  openProfile(handle: string, e?: MouseEvent): void {
    const target = this.localTarget(handle);
    ui.profileTarget = target;

    if (e?.currentTarget instanceof HTMLElement) {
      const r = e.currentTarget.getBoundingClientRect();
      let left = r.left - POP_W - 12; // prefer to the left of the row
      if (left < 8) left = r.right + 12; // flip right if no room
      left = Math.max(8, Math.min(left, window.innerWidth - POP_W - 8));
      const top = Math.max(8, Math.min(r.top - 8, window.innerHeight - POP_H - 8));
      ui.profilePos = { left, top };
    } else {
      ui.profilePos = null;
    }

    const scope = roleScopeOf(view.active);
    this.queryProfile(target); // §10.3 nick / avatar / bio / custom status
    store.session.ensureCaps(target, view.active); // channel-scope owner/mod badges
    store.session.ensureCapsAt(target, scope); // for the owner check
    roleStore.fetchRoles(scope); // role definitions (names + colors)
    roleStore.fetchMemberRoles(target, scope); // this member's assigned roles
  }

  // The *full* profile modal (centered dialog with bio/status/mutuals/actions).
  openFullProfile(handle: string): void {
    const target = this.localTarget(handle);
    ui.profileModalTarget = target;
    ui.profileTarget = null; // close the popover if it was open
    this.queryProfile(target);
    store.session.ensureCaps(target, view.active);
  }

  // §10.3 quick "Set nickname" dialog (per-namespace → targets the active server).
  openNickDialog(handle: string): void {
    ui.nickTarget = this.localTarget(handle);
  }
}

/// The profile domain singleton.
export const profileStore = new ProfileStore();

// ---- operations ----
// Pure account/string helpers (no nick state) — kept free.

export const nickKey = (scope: string, account: string): string => `${scope}|${account}`;
/// The bare peer of a `@peer` DM key (or any handle).
export const peerOf = (key: string): string => key.replace(/^@/, "");
/// Two-letter fallback avatar initials.
export const initials = (s: string): string => s.replace(/[^a-z0-9]/gi, "").slice(0, 2).toUpperCase() || "··";
/// The presence dot CSS class for an account.
export const dotClass = (acct: string): string => store.peekAccount(acct).dotClass;
/// A fetchable avatar URL for an account, or null → render initials.
export const avatarUrl = (acct: string): string | null => store.peekAccount(acct).avatarUrl;
/// An account's free-text bio (§10.3), or "" if unset.
export const bioOf = (acct: string): string => store.peekAccount(acct).about;
/// An account's custom status (§10.3), or "" if unset.
export const statusOf = (acct: string): string => store.peekAccount(acct).status;

// ---- events ----

/// §10.3 identity wire-event handlers: display profiles + per-namespace nicks.
export const profileHandlers: HandlerMap = {
  profile: (e) => {
    // Local users key by bare handle; federated by `account@network`.
    const key = e.network === store.session.network ? e.account : `${e.account}@${e.network}`;
    const acc = store.accountOf(key);
    acc.display = e.display ?? undefined;
    acc.avatar = e.avatar ?? undefined;
    acc.about = e.about ?? "";
    acc.status = e.status ?? "";
    acc.requested = true;
  },
  nick: (e) => {
    const acct = e.network === store.session.network ? e.account : `${e.account}@${e.network}`;
    const key = nickKey(e.scope, acct);
    if (e.nick) profileStore.nicks.set(key, e.nick);
    else profileStore.nicks.delete(key);
  },
};
