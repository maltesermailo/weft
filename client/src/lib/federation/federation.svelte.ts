// The client domain model — see docs/architecture/client-model-refactor.md.
import { SvelteMap } from "svelte/reactivity";
import * as weft from "$lib/transport/weft";
import type { WeftEvent } from "$lib/transport/weft";
import type { HandlerMap } from "$lib/sync/handler-map";
import { store } from "$lib/store/store.svelte";
import { toast } from "$lib/notifications/toasts.svelte";

/// A peering manifest as surfaced to the operator (§11.6 MANIFEST event).
export interface ManifestInfo {
  peer: string;
  version: number;
  state: string;
  channels: string[];
  history: string;
  media: string;
  typing: boolean;
}

/**
 * Operator-facing federation state (§11): the network block-list and the live
 * peering manifests, populated from NETBLOCKED / MANIFEST events. Replaces the
 * parallel `netblocks` / `manifests` records that lived in `+page.svelte`.
 */
export class Federation {
  /// Blocked networks → reason (or null). (§11.6 NETBLOCK)
  readonly netblocks = new SvelteMap<string, string | null>();
  /// Live peering manifests, keyed by peer network.
  readonly manifests = new SvelteMap<string, ManifestInfo>();

  /// §11 MANIFEST: a bridge's channel set/state; `severed`/`removed` drops it.
  applyManifest(e: Extract<WeftEvent, { kind: "manifest" }>): void {
    if (e.state === "severed" || e.state === "removed") this.manifests.delete(e.peer);
    else
      this.manifests.set(e.peer, {
        peer: e.peer,
        version: e.version,
        state: e.state,
        channels: e.channels,
        history: e.history,
        media: e.media,
        typing: e.typing,
      });
  }
  /// §11.6 NETBLOCKED: record a blocked network + reason.
  applyNetblock(e: Extract<WeftEvent, { kind: "netblocked" }>): void {
    this.netblocks.set(e.network, e.reason);
  }
}

/// This domain's wire-event handlers, merged into the reducer's registry.
export const federationHandlers: HandlerMap = {
  manifest: (e) => store.federation.applyManifest(e),
  netblocked: (e) => store.federation.applyNetblock(e),
};

// §11 operator federation actions — thin RPC wrappers with uniform error
// toasts, kept beside the state they read/mutate (`store.federation`).
export function refreshNetblocks(): void {
  store.federation.netblocks.clear();
  weft.netblockList().catch((e) => toast(String(e), "error"));
}
export function netblockAdd(network: string, reason?: string): void {
  weft
    .netblockAdd(network, reason)
    .then(() => setTimeout(refreshNetblocks, 200))
    .catch((e) => toast(String(e), "error"));
}
export function netblockRemove(network: string): void {
  store.federation.netblocks.delete(network);
  weft.netblockRemove(network).catch((e) => toast(String(e), "error"));
}
export function bridgePropose(scope: string, peer: string, history: string, media: string, typing: boolean): void {
  weft.bridgePropose(scope, peer, history, media, typing).catch((e) => toast(String(e), "error"));
}
export function bridgeAccept(peer: string, version: number): void {
  weft.bridgeAccept(peer, version).catch((e) => toast(String(e), "error"));
}
export function bridgeSever(peer: string): void {
  weft.bridgeSever(peer).catch((e) => toast(String(e), "error"));
}
