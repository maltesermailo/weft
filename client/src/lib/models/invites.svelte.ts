// The client domain model — see docs/architecture/client-model-refactor.md.

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
