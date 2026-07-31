// §10.3 identity / profile: thin views over the Account map plus the per-server
// nickname cache. Kept out of the layout so the reducer (nick events) and every
// component (avatars, names) share one source without the AppCtx bridge.
import { SvelteMap } from "svelte/reactivity";
import { page } from "$app/state";
import * as nav from "$lib/navigation/nav";
import { store } from "$lib/store/store.svelte";
import { ui } from "$lib/ui/ui.svelte";
import { view } from "$lib/navigation/view.svelte";
import { toast } from "$lib/notifications/toasts.svelte";
import { ensureCaps, ensureCapsAt } from "$lib/session/session.svelte";
import { roleScopeOf, fetchRoles, fetchMemberRoles } from "$lib/roles/roles.svelte";
import type { HandlerMap } from "$lib/sync/handler-map";
import * as weft from "$lib/transport/weft";

/// Per-namespace server nicknames, keyed `scope|account` (SvelteMap → delete is
/// reactive, no re-trigger hack). Written by the reducer's `nick` handler.
export const nicks = new SvelteMap<string, string>();
export const nickKey = (scope: string, account: string): string => `${scope}|${account}`;
/// Namespaces whose nicknames we've already pulled (fetch-once dedup).
export const nicksFetched = new Set<string>();

const activeServer = (): string => nav.viewFrom(page.route?.id, page.params).activeServer;

/// The bare peer of a `@peer` DM key (or any handle).
export const peerOf = (key: string): string => key.replace(/^@/, "");
/// Two-letter fallback avatar initials.
export const initials = (s: string): string => s.replace(/[^a-z0-9]/gi, "").slice(0, 2).toUpperCase() || "··";
/// The presence dot CSS class for an account.
export const dotClass = (acct: string): string => store.accountOf(acct).dotClass;
/// A fetchable avatar URL for an account, or null → render initials.
export const avatarUrl = (acct: string): string | null => store.accountOf(acct).avatarUrl;

/// An account's display name — the active server's nickname if set, else the
/// global display name (§10.3: the canonical handle is always shown separately).
export const displayName = (acct: string): string => {
  const ns = activeServer();
  const nick = ns ? nicks.get(nickKey(`ns:${ns}`, peerOf(acct))) : undefined;
  return nick || store.accountOf(acct).displayName;
};
/// An account's server nickname at the active server, or "" (for editors).
export const nickOf = (acct: string): string => {
  const ns = activeServer();
  return (ns ? nicks.get(nickKey(`ns:${ns}`, peerOf(acct))) : "") ?? "";
};
/// An account's free-text bio (§10.3), or "" if unset.
export const bioOf = (acct: string): string => store.accountOf(acct).about;
/// An account's custom status (§10.3), or "" if unset.
export const statusOf = (acct: string): string => store.accountOf(acct).status;

/// A user-facing label for a friend/group member ref (display name if local).
export function friendLabel(user: string): string {
  const [acct, net] = user.split("@");
  return net === store.session.network ? displayName(acct) : user;
}

/// Fetch an account's profile once (deduped via `Account.requested`).
export function queryProfile(acct: string): void {
  const a = peerOf(acct);
  if (!a) return;
  const rec = store.accountOf(a);
  if (!rec.requested) {
    rec.requested = true; // don't re-query
    weft.profilesQuery([a]).catch(() => {});
  }
}

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
    if (e.nick) nicks.set(key, e.nick);
    else nicks.delete(key);
  },
};

// ---- §10.3 profile actions ----
// Normalize a (possibly `account@network`) handle to the bare *local* account,
// keeping a genuinely federated ref whole.
function localTarget(handle: string): string {
  const at = handle.lastIndexOf("@");
  return at > 0 && handle.slice(at + 1) === store.session.network ? handle.slice(0, at) : handle;
}

// Set (or clear, with "") a per-namespace nickname (§10.3).
export function setNick(scope: string, account: string, value: string): void {
  weft.nick(scope, account, value).catch((e) => toast(String(e), "error"));
}

// Set (or clear, with "") my own custom status (§10.3).
export function setCustomStatus(text: string): void {
  weft.profileSet({ status: text }).catch((e) => toast(String(e), "error"));
}

const POP_W = 340;
const POP_H = 360;

// Open the anchored ProfileCard popover next to the clicked row (Discord-style;
// centered fallback when there's no event), then hydrate the target's profile,
// caps and roles for display.
export function openProfile(handle: string, e?: MouseEvent): void {
  const target = localTarget(handle);
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
  queryProfile(target); // §10.3 nick / avatar / bio / custom status
  ensureCaps(target, view.active); // channel-scope owner/mod badges
  ensureCapsAt(target, scope); // for the owner check
  fetchRoles(scope); // role definitions (names + colors)
  fetchMemberRoles(target, scope); // this member's assigned roles
}

// The *full* profile modal (distinct from the anchored popover): a centered
// dialog with bio, status, mutual servers and quick actions.
export function openFullProfile(handle: string): void {
  const target = localTarget(handle);
  ui.profileModalTarget = target;
  ui.profileTarget = null; // close the popover if it was open
  queryProfile(target);
  ensureCaps(target, view.active);
}

// §10.3 quick "Set nickname" dialog (per-namespace → targets the active server).
export function openNickDialog(handle: string): void {
  ui.nickTarget = localTarget(handle);
}
