// The current view (what's open), derived from the URL. Exposed as a getter
// object (Svelte 5 can't export a `$derived` binding directly) so any module —
// including the reducer + sync handlers running off the weft event stream —
// reads a reliable, up-to-date value. (Reading `page` from `$app/state` raw in a
// plain function outside a reactive context is stale; a `$derived` stays live.)
import { page } from "$app/state";
import * as nav from "$lib/nav";

const v = $derived(nav.viewFrom(page.route?.id, page.params));

export const view = {
  get active() {
    return v.active;
  },
  get activeServer() {
    return v.activeServer;
  },
  get homeView() {
    return v.homeView;
  },
};
