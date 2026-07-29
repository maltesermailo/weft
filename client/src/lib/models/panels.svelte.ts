// The client domain model — see docs/architecture/client-model-refactor.md.
import type { Msg } from "$lib/types";

/**
 * §6.4 message-search panel. Results are server-streamed (`weft.search` →
 * MESSAGE events), so the reducer accumulates them in `buf` while
 * `loadingChannel` is set and flushes into `results` on the batch terminator;
 * everything else (open/query/scope/loading + the reveal) is owned here and by
 * `SearchModal`.
 */
export class SearchPanel {
  open = $state(false);
  query = $state("");
  scope = $state(""); // the channel searched (for the header)
  results = $state<Msg[]>([]);
  loading = $state(false);
  // Reducer streaming machinery (not reactive — only the reducer touches it).
  buf: Msg[] = [];
  loadingChannel: string | null = null;

  /// Open the panel on a channel, cleared for a fresh query.
  begin(channel: string): void {
    this.query = "";
    this.results = [];
    this.scope = channel;
    this.open = true;
  }
}

/**
 * §6.4 pinned-messages panel. Also server-streamed (`weft.pins` → MESSAGE
 * events); same buffer/flush shape as {@link SearchPanel}.
 */
export class PinsPanel {
  open = $state(false);
  list = $state<Msg[]>([]);
  // Reducer streaming machinery.
  buf: Msg[] = [];
  loadingChannel: string | null = null;
}
