// The client domain model — see docs/architecture/client-model-refactor.md.
import type { HandlerMap } from "$lib/sync/handler-map";
import { store } from "./store.svelte";
import { ui } from "$lib/ui.svelte";
import * as weft from "$lib/weft";

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
