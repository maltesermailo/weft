// The client domain model — see docs/architecture/client-model-refactor.md.
import { SvelteMap } from "svelte/reactivity";
import type { Msg, ThreadInfo } from "$lib/types";
import type { HandlerMap } from "$lib/sync/handler-map";
import { store } from "$lib/store/store.svelte";
import { channels } from "$lib/channels/channel.svelte";
import { mkMsg } from "$lib/messages/messages.svelte";
import { view } from "$lib/navigation/view.svelte";
import * as weft from "$lib/transport/weft";
import { toast } from "$lib/notifications/toasts.svelte";

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

// ---- §9.4 thread actions (ThreadPanel / ThreadsModal / message row) ----
const activeChannel = () => (view.active ? channels[view.active] : undefined);

// How many loaded replies a root has (its thread size), for the indicator.
export const threadCount = (msgid?: string): number => {
  const ch = activeChannel();
  return !msgid || !ch ? 0 : ch.messages.filter((m) => m.thread === msgid).length;
};

// A thread's display name (from THREAD / THREAD-NAMED), for the indicator + title.
export const threadNameFor = (msgid?: string): string | undefined => store.threads.nameFor(msgid);

// ---- side panel ----
export function openThread(root: Msg): void {
  if (!root.msgid) return;
  store.threads.root = root;
  store.threads.messages = [root];
  store.threads.composer = "";
  store.threads.loadingRoot = root.msgid;
  weft.history(view.active, undefined, root.msgid).catch((e) => {
    store.threads.loadingRoot = null;
    toast(String(e), "error");
  });
}
export function closeThread(): void {
  store.threads.root = null;
  store.threads.messages = [];
  store.threads.loadingRoot = null;
  store.threads.buf = [];
}
export function sendThread(): void {
  const text = store.threads.composer.trim();
  const root = store.threads.root?.msgid;
  if (!text || !root || !view.active) return;
  weft
    .sendMessage(view.active, text, undefined, [], root)
    .then(() => (store.threads.composer = ""))
    .catch((e) => toast(String(e), "error"));
}
// Rename (or, with an empty string, clear the name of) the open thread.
export function renameThread(name: string): void {
  const root = store.threads.root?.msgid;
  if (!root || !view.active) return;
  weft.nameThread(view.active, root, name.trim()).catch((e) => toast(String(e), "error"));
}

// ---- list modal ----
export function openThreads(): void {
  if (!view.active.startsWith("#")) return;
  store.threads.listOpen = true;
  store.threads.list = [];
  store.threads.listBuf = [];
  store.threads.loadingList = true;
  weft.listThreads(view.active).catch((e) => {
    store.threads.loadingList = false;
    toast(String(e), "error");
  });
}
export function closeThreads(): void {
  store.threads.listOpen = false;
}
// Open a thread from the list. Reuse the root if it's already in the timeline;
// otherwise seed a placeholder — the thread HISTORY (incl. the root) replaces it.
export function openThreadByRoot(info: ThreadInfo): void {
  store.threads.listOpen = false;
  const loaded = activeChannel()?.messages.find((m) => m.msgid === info.root);
  if (loaded) {
    openThread(loaded);
    return;
  }
  openThread(mkMsg({ author: "", body: "", time: "", ts: 0, own: false, msgid: info.root }));
}
