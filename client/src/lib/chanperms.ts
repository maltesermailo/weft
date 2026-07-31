// §6.5 per-channel permissions editor (ChannelSettings modal): channel-scoped
// role/@everyone overrides + individual-member grants, plus the §6.7 `restricted`
// and §6.3 `view-gated` channel toggles. The target channel is `ui.chanPerms`;
// role/grant data lives on the session role cache + `store.grants`.
import { store } from "$lib/models/store.svelte";
import { ui } from "$lib/ui.svelte";
import * as weft from "$lib/weft";
import { toast } from "$lib/toasts.svelte";
import { channels, nsOf } from "$lib/models/channel.svelte";
import { rolesAt, createRoleAt, deleteRoleAt, fetchRoles, fetchGrants } from "$lib/models/session.svelte";

// The namespace scope of the channel being edited (its role picker's source).
export function chanNsScope(): string {
  const ns = nsOf(ui.chanPerms ?? "");
  return ns ? `ns:${ns}` : "*";
}

// A channel-scoped role/@everyone override's caps (channel roles are named after
// ns roles; `everyone` is the per-channel baseline).
export const chanRoleCaps = (name: string): string[] => rolesAt(ui.chanPerms ?? "").find((r) => r.name === name)?.caps ?? [];

// Apply a channel role / @everyone target's full cap set (the editor commits a
// draft, not per-toggle): a non-empty set upserts the channel role, an empty set
// deletes it. The ROLES refetch inside createRoleAt/deleteRoleAt reconciles the view.
export function setChanRoleCaps(name: string, color: string, caps: string[]): void {
  if (!ui.chanPerms) return;
  (caps.length ? createRoleAt(ui.chanPerms, name, color, caps.join(",")) : deleteRoleAt(ui.chanPerms, name)).catch((e) =>
    toast(String(e), "error"),
  );
}

// Individual-member overrides at the channel scope (direct GRANTs).
export const chanMemberGrants = (): { subject: string; caps: string[] }[] => store.grants.get(ui.chanPerms ?? "") ?? [];
export const chanMemberCaps = (account: string): string[] => chanMemberGrants().find((g) => g.subject === account)?.caps ?? [];

// Apply a member override's full cap set. record_grant replaces, so we GRANT the
// new set (or REVOKE the old one when it empties). Optimistic locally.
export function setChanMemberCaps(account: string, caps: string[]): void {
  if (!ui.chanPerms) return;
  const scope = ui.chanPerms;
  const prev = chanMemberCaps(account);

  // Re-set the whole entry (SvelteMap values aren't deeply reactive).
  const list = store.grants.get(scope) ?? [];
  const idx = list.findIndex((g) => g.subject === account);
  if (caps.length) {
    if (idx >= 0) store.grants.set(scope, list.map((g, i) => (i === idx ? { ...g, caps } : g)));
    else store.grants.set(scope, [...list, { subject: account, caps }]);
  } else if (idx >= 0) {
    store.grants.set(scope, list.filter((g) => g.subject !== account));
  }

  (caps.length ? weft.grant(account, scope, caps.join(",")) : weft.revoke(account, scope, prev.join(","))).catch((e) => {
    toast(String(e), "error");
    fetchGrants(scope);
  });
}

// Remove a whole channel override target. A role override deletes the
// channel-scoped role; a member override revokes all their channel caps.
export function removeChanRole(name: string): void {
  if (!ui.chanPerms) return;
  deleteRoleAt(ui.chanPerms, name).catch((e) => toast(String(e), "error"));
}
export function removeChanMember(account: string): void {
  if (!ui.chanPerms) return;
  const scope = ui.chanPerms;
  const cur = chanMemberCaps(account);
  store.grants.set(scope, (store.grants.get(scope) ?? []).filter((g) => g.subject !== account));
  if (cur.length) weft.revoke(account, scope, cur.join(",")).catch((e) => toast(String(e), "error"));
}

export function openChanPerms(channel: string): void {
  ui.chanPerms = channel;
  fetchRoles(chanNsScope()); // the namespace's roles (the role picker source)
  fetchRoles(channel); // this channel's role + @everyone overrides
  fetchGrants(channel); // this channel's individual-member overrides
}

export function toggleRestricted(): void {
  const ch = ui.chanPerms ? channels[ui.chanPerms] : undefined;
  if (!ch || !ui.chanPerms) return;

  const next = !ch.restricted;
  weft
    .channelMeta(ui.chanPerms, "posting", next ? "restricted" : "open")
    .then(() => (ch.restricted = next))
    .catch((e) => toast(String(e), "error"));
}

// §6.3 view-gate: when on, the channel is hidden from anyone without the `view`
// cap (invariant 1 anti-enumeration). Grant `view` per target in the editor to
// let specific roles/members in.
export function toggleViewGated(): void {
  const ch = ui.chanPerms ? channels[ui.chanPerms] : undefined;
  if (!ch || !ui.chanPerms) return;

  const next = !ch.viewGated;
  weft
    .channelMeta(ui.chanPerms, "view-gated", next ? "true" : "false")
    .then(() => (ch.viewGated = next))
    .catch((e) => toast(String(e), "error"));
}
