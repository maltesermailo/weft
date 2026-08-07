// Derived view + rail state: the active target/channel, the namespace rail
// tiles, the sidebar channel grouping, per-server unread/mention rollups, and
// display labels. Pure `$derived` over the URL (`view`), the channel collection,
// and the store — exposed as a getter object (Svelte 5 can't export a `$derived`
// binding). Lives in its own module (not `view.svelte`, which `channel.svelte`
// imports — that would cycle) so components read it directly, no AppCtx bridge.
import { view } from "$lib/navigation/view.svelte";
import { store } from "$lib/store/store.svelte";
import { channelStore, nsOf, type Channel } from "$lib/channels/channel.svelte";
import { peerOf, profileStore } from "$lib/profile/profile.svelte";


const _activeChannel = $derived(view.active ? channelStore.channels[view.active] : undefined);

// The rail = every namespace I'm a **member** of. Membership is namespace-level,
// so `Server.joined` is the whole answer, and weftd states it outright: SYNC
// replays an `NS-MEMBER … join` per membership (precisely so a channel-less server
// shows), and live join/part keep it current.
//
// It used to also derive a tile from any `#<ns>/…` channel it happened to hold.
// That inverted the unit of membership — a channel implied a namespace — so
// leaving could not stick: `CHANNELS` answers a non-member for a public
// namespace, so one re-fetch re-created a channel and the tile came back.
const _serverNamespaces = $derived(
  [...store.servers.values()]
    .filter((s) => s.joined)
    .map((s) => s.id)
    .sort(),
);

const _dmList = $derived(Object.values(channelStore.channels).filter((c) => c.name.startsWith("@") || c.name.startsWith("&")));

// A legacy-shaped view of the active Server's metadata (snake_case field names
// the modals/banners already read). Undefined until NS-META has landed.
const _activeNsMeta = $derived.by(() => {
  const s = view.activeServer ? store.servers.get(view.activeServer) : undefined;
  if (!s || !s.metaLoaded) return undefined;
  return {
    id: s.id,
    name: s.name,
    title: s.title,
    description: s.description,
    owner: s.owner,
    visibility: s.visibility,
    federation: s.federation,
    welcome: s.welcome,
    recovery_eta: s.recoveryEta,
    recovery_rung: s.recoveryRung,
    categories: s.categories,
  };
});

// Discord-style grouping for the active server: uncategorized channels sit bare
// at the top (category "", no header), then each category (position-ordered).
const _channelGroups = $derived.by(() => {
  const bare: Channel[] = [];
  const groups = new Map<string, Channel[]>();

  // Empty categories the admin created show up too. The list is model-owned
  // (client-core) — seeded from the layout cache on connect, then NS-META.
  for (const cat of store.servers.get(view.activeServer)?.categories ?? [])
    groups.set(cat, []);

  for (const c of Object.values(channelStore.channels)) {
    if (!c.name.startsWith("#") || nsOf(c.name) !== view.activeServer) continue;
    const cat = c.category;
    if (!cat) {
      bare.push(c);
      continue;
    }
    if (!groups.has(cat)) groups.set(cat, []);
    groups.get(cat)!.push(c);
  }

  const byPos = (a: Channel, b: Channel) => (a.position ?? 0) - (b.position ?? 0) || a.name.localeCompare(b.name);
  bare.sort(byPos);
  for (const list of groups.values()) list.sort(byPos);

  const out = bare.length ? [{ category: "", list: bare }] : [];
  for (const [category, list] of groups.entries()) out.push({ category, list });
  return out;
});

// Server-tile unread/mention rollups, folded over the server's own channels
// (excluding the active one).
const serverChannels = (ns: string) => Object.values(channelStore.channels).filter((c) => nsOf(c.name) === ns && c.name !== view.active);

export const vm = {
  get activeChannel() {
    return _activeChannel;
  },
  get activeIsDm() {
    return view.active.startsWith("@");
  },
  get activeIsGroup() {
    return view.active.startsWith("&");
  },
  get serverNamespaces() {
    return _serverNamespaces;
  },
  get dmList() {
    return _dmList;
  },
  get activeNsMeta() {
    return _activeNsMeta;
  },
  get channelGroups() {
    return _channelGroups;
  },
  // v0.13: a namespace's rail tile / header key is its **id**; its display name
  // is the vanity from NS-META (fall back to the id until we've seen it).
  serverName: (nsId: string): string => store.servers.get(nsId)?.displayName ?? nsId,
  serverUnread: (ns: string): boolean => serverChannels(ns).some((c) => c.unread),
  serverMention: (ns: string): boolean => serverChannels(ns).some((c) => c.mention),
  serverMentionCount: (ns: string): number => serverChannels(ns).reduce((sum, c) => sum + c.mentionCount, 0),
  // Am I a member of this namespace (by id)? Discover hides servers I'm in.
  isNsMember: (nsId: string): boolean => store.servers.get(nsId)?.joined ?? false,
  // User-facing label for any target — `#vanity` (channel), peer name (DM), or
  // group label (group DM).
  titleOf: (name: string): string => {
    if (name.startsWith("#")) return `#${channelStore.short(name)}`;
    if (name.startsWith("&")) return store.social.groupLabel(name);
    if (name.startsWith("@")) return profileStore.displayName(peerOf(name));
    return name;
  },
  // §6.2 NS INFO MEMBERS: the moderator roster for a namespace (once fetched).
  nsMembers: (ns: string) => store.servers.get(ns)?.members ?? [],
  get nsMembersLoading() {
    return view.activeServer ? (store.servers.get(view.activeServer)?.membersLoading ?? false) : false;
  },
  fetchNsMembers: (ns: string) => store.server(ns).fetchMembers(),
};
