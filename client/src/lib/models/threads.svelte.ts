// The client domain model — see docs/architecture/client-model-refactor.md.
import { SvelteMap } from "svelte/reactivity";
import type { Msg, ThreadInfo } from "$lib/types";

/**
 * §9.4 threads: the open thread **side panel** (root + replies + composer) and
 * the thread **list** modal. Both stream server results (thread replies /
 * `THREADS`) that the reducer routes here; live replies to the open thread are
 * appended by the message handler. Replaces the `thread*` `$state` cluster in
 * `+page.svelte`.
 */
export class Threads {
  // ---- side panel ----
  root = $state<Msg | null>(null);
  messages = $state<Msg[]>([]);
  composer = $state("");

  // ---- list modal ----
  listOpen = $state(false);
  list = $state<ThreadInfo[]>([]);

  /// Root msgid → thread display name (§9.4).
  readonly names = new SvelteMap<string, string>();

  // ---- reducer streaming machinery (not reactive) ----
  buf: Msg[] = []; // replies batch → messages
  loadingRoot: string | null = null;
  listBuf: ThreadInfo[] = [];
  loadingList = false;

  /// A thread's display name (root msgid → name), if named.
  nameFor(msgid?: string): string | undefined {
    return msgid ? this.names.get(msgid) : undefined;
  }
}
