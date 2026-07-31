// The client domain model — see docs/architecture/client-model-refactor.md.
import type { HandlerMap } from "$lib/sync/handler-map";
import { store } from "$lib/store/store.svelte";
import { ui } from "$lib/ui/ui.svelte";
import * as weft from "$lib/transport/weft";
import { view } from "$lib/navigation/view.svelte";
import { toast } from "$lib/notifications/toasts.svelte";
import { scopesFor, ensureChannel, persistDms } from "$lib/channels/channel.svelte";
import { friendLocalAccount } from "$lib/social/social.svelte";

/** One live invite in the Discord-style invites menu (§6.5). */
export interface InviteInfo {
  scope: string;
  invite_id: string;
  creator: string;
  uses_left: number | null;
  used: number;
  expiry: number | null;
}

/**
 * §6.5 invites: the list menu (streamed via `INVITE-INFO` BATCH) + the
 * create screen (the minted link arrives on the `invited` event). Replaces the
 * `invites*` / `invite*` `$state` cluster in `+page.svelte`; the reducer and the
 * `+page` actions (which need scope/friend/weft context) write it, components
 * read it.
 */
export class Invites {
  // ---- list menu ----
  listOpen = $state(false);
  scope = $state("");
  list = $state<InviteInfo[]>([]);
  // streaming machinery (reducer-only, not reactive)
  loading = false;
  buf: InviteInfo[] = [];

  // ---- create screen ----
  createOpen = $state(false);
  createScope = $state("");
  link = $state<string | null>(null); // the minted link, once it lands
  id = $state<string | null>(null); // for INVITE REVOKE of the just-minted one
}

/// §6.5 invite wire-event handlers. `invited` is the mint/revoke echo (updates
/// the create screen + refreshes an open list); `invite-info` buffers list rows.
export const invitesHandlers: HandlerMap = {
  invited: (e) => {
    if (e.max_uses === 0) {
      // A revoke echo (INVITED … max-uses=0) — close it + drop from the menu.
      if (store.invites.id === e.invite_id) {
        store.invites.link = null;
        store.invites.id = null;
      }
      store.invites.list = store.invites.list.filter((i) => i.invite_id !== e.invite_id);
    } else {
      store.invites.link = e.link ?? e.invite_id;
      store.invites.id = e.invite_id;
      // Reflect a freshly-minted invite live wherever the list is shown.
      const listShown = store.invites.listOpen || (ui.nsSettingsOpen && ui.nsTab === "invites");
      if (listShown && e.scope === store.invites.scope) weft.inviteList(store.invites.scope).catch(() => {});
    }
  },
  "invite-info": (e) => {
    if (store.invites.loading) store.invites.buf.push(e);
  },
};

// ---- §6.5 invite actions ----
// Open the create screen, seeding the scope (default = most specific covering
// scope) and clearing any previously-minted link.
export function openInviteCreate(scope?: string): void {
  store.invites.createScope = scope || scopesFor()[0] || "";
  store.invites.link = null;
  store.invites.id = null;
  store.invites.createOpen = true;
}
export function mintInvite(): void {
  openInviteCreate();
}

// Mint with the chosen limits — `null` = unlimited uses / never expires. The
// resulting link arrives on the `invited` event and fills `store.invites.link`.
export function generateInvite(maxUses: number | null, expiry: number | null): void {
  const scope = store.invites.createScope;
  if (!scope) return;
  weft.inviteMint(scope, maxUses ?? undefined, expiry ?? undefined).catch((e) => toast(String(e), "error"));
}

// Share an invite link with a friend by dropping it into their DM. Only
// local-network friends are DM-able (cross-network DMs are out of scope).
export function sendInviteDM(ref: string, link: string): void {
  const acct = friendLocalAccount(ref);
  if (!acct) return;

  const target = "@" + acct;
  ensureChannel(target);
  persistDms();
  weft.sendMessage(target, link).catch((e) => toast(String(e), "error"));
}

// ---- Discord-style invites menu ----
function loadInvites(scope: string): void {
  store.invites.scope = scope;
  store.invites.list = [];
  store.invites.buf = [];
  store.invites.loading = true;
  weft.inviteList(scope).catch((e) => {
    store.invites.loading = false;
    toast(String(e), "error");
  });
}
export function openInvites(): void {
  loadInvites(scopesFor()[0]);
  store.invites.listOpen = true;
}
// The Server-Settings Invites tab lists the whole namespace's invites.
export function loadNsInvites(): void {
  if (view.activeServer) loadInvites(`ns:${view.activeServer}`);
}
export function revokeInvite(id: string): void {
  weft.inviteRevoke(id).catch((e) => toast(String(e), "error"));
  store.invites.list = store.invites.list.filter((i) => i.invite_id !== id); // optimistic
}
export function createInvite(): void {
  openInviteCreate(store.invites.scope || scopesFor()[0]);
}
// Reconstruct the shareable link for an invite (the list doesn't carry it).
export function inviteLinkFor(inv: InviteInfo): string {
  const ns = inv.scope.startsWith("ns:")
    ? inv.scope.slice(3)
    : inv.scope.startsWith("#") && inv.scope.includes("/")
      ? inv.scope.slice(1).split("/")[0]
      : null;
  const network = store.session.network;
  return ns ? `weft://${network}/${ns}/i/${inv.invite_id}` : `weft://${network}/i/${inv.invite_id}`;
}
