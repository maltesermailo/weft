// Pure URL <-> view-state mapping for the client's path-based routes.
//
// A view is a single sigil-tagged `active` key plus `activeServer` and
// `homeView`. Routes (segments are percent-encoded; sigils are reconstructed
// from the matched route, so they never appear in the URL):
//
//   /                       home / friends           active=""      server=""  home
//   /c/<server>             server selected, no chan  active=""      server=ns  !home
//   /c/<server>/<channel>   channel in a namespace    active=#ns/ch  server=ns  !home
//   /c/~/<channel>          top-level (network) chan   active=#ch     server=""  !home
//   /dm/<peer>              direct message            active=@peer   server=""  home
//   /g/<group>              group DM                  active=&id     server=""  home

/** Sentinel server segment for a top-level (network) channel with no namespace. */
const NET = "~";

export interface View {
  active: string;
  activeServer: string;
  homeView: boolean;
}

export const HOME: View = { active: "", activeServer: "", homeView: true };

/** The URL path for a view (an `active` key plus the optionally-selected server). */
export function pathFor(active: string, activeServer = ""): string {
  if (active.startsWith("@")) return `/dm/${encodeURIComponent(active.slice(1))}`;
  if (active.startsWith("&")) return `/g/${encodeURIComponent(active.slice(1))}`;
  if (active.startsWith("#")) {
    const body = active.slice(1);
    const slash = body.indexOf("/");
    if (slash < 0) return `/c/${NET}/${encodeURIComponent(body)}`;
    const ns = body.slice(0, slash);
    const chan = body.slice(slash + 1);
    return `/c/${encodeURIComponent(ns)}/${encodeURIComponent(chan)}`;
  }

  if (activeServer) return `/c/${encodeURIComponent(activeServer)}`;
  return "/";
}

/**
 * The inverse of `pathFor`: the view for a matched route id + its params.
 * `params` values are SvelteKit's already-decoded segments (`page.params`).
 */
export function viewFrom(routeId: string | null | undefined, params: Record<string, string | undefined>): View {
  switch (routeId) {
    case "/dm/[peer]":
      return { active: "@" + (params.peer ?? ""), activeServer: "", homeView: true };
    case "/g/[group]":
      return { active: "&" + (params.group ?? ""), activeServer: "", homeView: true };
    case "/c/[server]":
      return { active: "", activeServer: params.server ?? "", homeView: false };
    case "/c/[server]/[channel]": {
      const server = params.server ?? "";
      const chan = params.channel ?? "";
      if (server === NET) return { active: "#" + chan, activeServer: "", homeView: false };
      return { active: "#" + server + "/" + chan, activeServer: server, homeView: false };
    }
    default:
      return { ...HOME };
  }
}
