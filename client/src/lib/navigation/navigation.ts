// Navigation actions shared by the sidebar/rail (via AppCtx) and the reducer
// (auto-select on join, go-home on namespace delete). Pure routing over the
// channel collection — no component state, so both can import it directly.
import { goto } from "$app/navigation";
import * as nav from "$lib/navigation/nav";
import { channelStore, nsOf } from "$lib/channels/channel.svelte";
import { view } from "$lib/navigation/view.svelte";
import { ui } from "$lib/ui/ui.svelte";
import { store } from "$lib/store/store.svelte";
import * as weft from "$lib/transport/weft";
import { toast } from "$lib/notifications/toasts.svelte";
import { joinVoice, voice } from "$lib/voice/voice.svelte";
import { nsMetaFetched } from "$lib/connection/connection.svelte";
import { peerOf } from "$lib/profile/profile.svelte";


/// The DM channel key (`@peer`) for a (possibly `@`-prefixed) handle.
export const dmKeyFor = (name: string): string => "@" + peerOf(name);

/// Open (or create) a 1:1 DM and route to it. Persisted so it survives reconnect.
export function openDm(peer: string): void {
  const key = "@" + peer.replace(/^@/, "");
  channelStore.ensure(key);
  channelStore.persistDms();
  goto(nav.pathFor(key));
}

/// Close (hide) an open DM — a local-only view action; nothing is deleted
/// server-side. Switch away if it was the open conversation.
export function closeDm(name: string): void {
  const key = dmKeyFor(name);
  delete channelStore.channels[key];
  channelStore.persistDms();
  if (view.active === key) goHome();
}

/// DM a friend (local friends only — DMs are per-network).
export function messageFriend(user: string): void {
  const acct = store.social.friendLocalAccount(user);
  if (acct) openDm(acct);
}

/// Open a target (channel/DM/group) + mark it read.
export function open(name: string): void {
  channelStore.markRead(name);
  goto(nav.pathFor(name));
}

/// Open a group DM by id (ensuring the local channel exists first).
export function openGroup(id: string): void {
  channelStore.ensure(id);
  goto(nav.pathFor(id));
}

/// Show the Friends home screen (home view, no DM selected).
export function openFriends(): void {
  goto("/");
}

/// Select a server tile and open its header dropdown (rail right-click).
export function openServerMenu(ns: string): void {
  selectServer(ns);
  ui.serverMenu = true;
}

/// §6.2 leave a namespace: drop membership, navigate home, and forget its
/// channels locally so the rail updates without a reload.
///
/// Order matters. `goto` is async, so dropping the channel records *first* leaves
/// a tick in which the URL still names a channel whose record is gone — and the
/// view effects fire one last `HISTORY`/roster fetch for it. weftd has already
/// unsubscribed us by then, so it answers `CAP-REQUIRED`, and `catchUpChannel`
/// would re-create the very record we just deleted. So: flip membership (the rail
/// reads that, and updates immediately), leave the view, and only then tear down.
export async function nsLeave(): Promise<void> {
  const ns = view.activeServer;
  if (!ns) return;

  ui.serverMenu = false;
  weft.nsLeave(ns).catch((e) => toast(String(e), "error"));

  const server = store.servers.get(ns);
  if (server) server.joined = false;

  await goHome();

  for (const name of Object.keys(channelStore.channels)) {
    if (name.startsWith("#") && nsOf(name) === ns) delete channelStore.channels[name];
  }
  store.servers.delete(ns);
}

/// Open a voice channel's stage (switch the main view) and join the call if we're
/// not already in it. Voice channels have no message timeline, so no channelStore.markRead.
export function openVoice(name: string): void {
  if (voice.channel !== name) joinVoice(name);
  goto(nav.pathFor(name));
}

/// Open the Discover modal: clear the transient browse list (loaded non-member
/// servers) but keep the ones I'm in + interned namespaces, then fetch page one.
export function openDiscover(): void {
  ui.discoverOpen = true;
  for (const [id, s] of store.servers) if (s.metaLoaded && !s.joined) store.servers.delete(id);
  nsMetaFetched.clear();
  ui.discoverCursor = null;
  weft.discover().catch(() => {});
}


/// Open a namespace: land on its first channel (by position), else its empty view.
export function selectServer(ns: string): void {
  const a = view.active;
  if (a.startsWith("#") && nsOf(a) === ns) return; // already in this server
  const first = Object.values(channelStore.channels)
    .filter((c) => c.name.startsWith("#") && nsOf(c.name) === ns)
    .sort((x, y) => (x.position ?? 0) - (y.position ?? 0) || x.name.localeCompare(y.name))[0];
  goto(nav.pathFor(first?.name ?? "", ns));
}

/// The DM/home tile: land on the most recently active conversation, else Friends.
///
/// Returns the navigation promise so a caller that must not race the view — see
/// `nsLeave` — can wait for the URL to actually change.
export function goHome(): Promise<void> {
  const convos = Object.values(channelStore.channels).filter((c) => c.name.startsWith("@") || c.name.startsWith("&"));
  if (!convos.length) {
    return goto("/");
  }
  const recent = convos.reduce((a, b) => ((b.messages.at(-1)?.ts ?? 0) >= (a.messages.at(-1)?.ts ?? 0) ? b : a));
  return goto(nav.pathFor(recent.name));
}
