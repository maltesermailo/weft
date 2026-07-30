// Navigation actions shared by the sidebar/rail (via AppCtx) and the reducer
// (auto-select on join, go-home on namespace delete). Pure routing over the
// channel collection — no component state, so both can import it directly.
import { goto } from "$app/navigation";
import * as nav from "$lib/nav";
import { channels, nsOf } from "$lib/models/channel.svelte";
import { view } from "$lib/view.svelte";


/// Open a namespace: land on its first channel (by position), else its empty view.
export function selectServer(ns: string): void {
  const a = view.active;
  if (a.startsWith("#") && nsOf(a) === ns) return; // already in this server
  const first = Object.values(channels)
    .filter((c) => c.name.startsWith("#") && nsOf(c.name) === ns)
    .sort((x, y) => (x.position ?? 0) - (y.position ?? 0) || x.name.localeCompare(y.name))[0];
  goto(nav.pathFor(first?.name ?? "", ns));
}

/// The DM/home tile: land on the most recently active conversation, else Friends.
export function goHome(): void {
  const convos = Object.values(channels).filter((c) => c.name.startsWith("@") || c.name.startsWith("&"));
  if (!convos.length) {
    goto("/");
    return;
  }
  const recent = convos.reduce((a, b) => ((b.messages.at(-1)?.ts ?? 0) >= (a.messages.at(-1)?.ts ?? 0) ? b : a));
  goto(nav.pathFor(recent.name));
}
