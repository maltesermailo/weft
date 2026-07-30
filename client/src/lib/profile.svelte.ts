// §10.3 identity / profile: thin views over the Account map plus the per-server
// nickname cache. Kept out of the layout so the reducer (nick events) and every
// component (avatars, names) share one source without the AppCtx bridge.
import { SvelteMap } from "svelte/reactivity";
import { page } from "$app/state";
import * as nav from "$lib/nav";
import { store } from "$lib/models/store.svelte";
import type { HandlerMap } from "$lib/sync/handler-map";
import * as weft from "$lib/weft";

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
