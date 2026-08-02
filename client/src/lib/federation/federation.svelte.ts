// The client domain model — see docs/architecture/client-model-refactor.md.
import { SvelteMap } from "svelte/reactivity";
import * as weft from "$lib/transport/weft";
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

  // The netblock + manifest maps are the client-core model's now: it reshapes
  // NETBLOCKED / MANIFEST events into diffs (`netblock-set` / `manifest-set` /
  // `manifest-drop`) applied by `federationHandlers` below. The clear-on-refresh
  // + the optimistic remove stay here as UI operations on these maps.

  // ---- §11 operator federation actions (thin RPC wrappers, uniform toasts) ----
  refreshNetblocks(): void {
    this.netblocks.clear();
    weft.netblockList().catch((e) => toast(String(e), "error"));
  }
  netblockAdd(network: string, reason?: string): void {
    // The ADD echo now carries the reason (→ `netblock-set`) — no LIST refresh.
    weft.netblockAdd(network, reason).catch((e) => toast(String(e), "error"));
  }
  netblockRemove(network: string): void {
    // The server echoes NETBLOCK-REMOVED (→ `netblock-drop`), which removes it — no
    // optimistic delete (which the old re-adding NETBLOCKED echo used to undo).
    weft.netblockRemove(network).catch((e) => toast(String(e), "error"));
  }
  bridgePropose(scope: string, peer: string, history: string, media: string, typing: boolean): void {
    weft.bridgePropose(scope, peer, history, media, typing).catch((e) => toast(String(e), "error"));
  }
  bridgeAccept(peer: string, version: number): void {
    weft.bridgeAccept(peer, version).catch((e) => toast(String(e), "error"));
  }
  bridgeSever(peer: string): void {
    weft.bridgeSever(peer).catch((e) => toast(String(e), "error"));
  }
}

/// Model-mirror handlers (client-core migration): apply the Rust federation diffs
/// onto `store.federation`. NETBLOCKED/NETBLOCK-REMOVED/MANIFEST are reshaped by
/// the model into these; a block sets (with reason), an unblock drops, and a
/// `severed`/`removed` manifest arrives pre-resolved as `manifest-drop`.
export const federationHandlers: HandlerMap = {
  "netblock-set": (e) => store.federation.netblocks.set(e.network, e.reason),
  "netblock-drop": (e) => store.federation.netblocks.delete(e.network),
  "manifest-set": (e) => store.federation.manifests.set(e.manifest.peer, e.manifest),
  "manifest-drop": (e) => store.federation.manifests.delete(e.peer),
};
