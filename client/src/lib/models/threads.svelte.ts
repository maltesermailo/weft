// The client domain model — see docs/architecture/client-model-refactor.md.
import { SvelteMap } from "svelte/reactivity";
import type { Msg, ThreadInfo } from "$lib/types";
import type { HandlerMap } from "$lib/sync/handler-map";
import { store } from "./store.svelte";

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

/// §9.4 thread wire-event handlers. `thread` rows buffer into `listBuf` while a
/// threads-list BATCH is loading; `thread-named` reflects a live rename.
export const threadsHandlers: HandlerMap = {
  thread: (e) => {
    if (e.name) store.threads.names.set(e.root, e.name);
    else store.threads.names.delete(e.root);
    if (store.threads.loadingList)
      store.threads.listBuf.push({
        root: e.root,
        name: e.name ?? undefined,
        replies: e.replies,
        last: e.last ?? undefined,
      });
  },
  "thread-named": (e) => {
    if (e.name) store.threads.names.set(e.root, e.name);
    else store.threads.names.delete(e.root);
    const i = store.threads.list.findIndex((t) => t.root === e.root);
    if (i >= 0) store.threads.list[i] = { ...store.threads.list[i], name: e.name ?? undefined };
  },
};
