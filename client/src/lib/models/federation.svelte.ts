// The client domain model — see docs/architecture/client-model-refactor.md.
import { SvelteMap } from "svelte/reactivity";

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
}
