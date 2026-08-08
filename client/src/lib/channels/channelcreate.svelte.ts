// §6.3 channel + category creation (the CreateChannelModal / CreateCategoryModal
// drafts + submit). Draft state lives here so the modals bind directly and the
// server-menu openers reach it without the AppCtx bridge. Channel creation is
// server-side only: we send the desired vanity and reconcile on the CHANNEL-LAYOUT
// echo (see `channelStore.reconcileCreate`).
import { ui, scopedNs } from "$lib/ui/ui.svelte";
import { view } from "$lib/navigation/view.svelte";
import * as weft from "$lib/transport/weft";
import { toast } from "$lib/notifications/toasts.svelte";
import { channelStore, nsOf } from "$lib/channels/channel.svelte";

export const chanDraft = $state<{
  open: boolean;
  name: string;
  category: string;
  announce: boolean;
  retention: string; // "" = server default; else a RETENTION_OPTIONS value
  voice: boolean; // §16 voice channel
  /// The namespace to create in, captured when the dialog opens. Empty = whatever
  /// `scopedNs()` resolves to. Captured rather than read at submit time so the
  /// dialog cannot retarget itself if the view changes while it is open.
  ns: string;
}>({ open: false, name: "", category: "", announce: false, retention: "", voice: false, ns: "" });

export const catDraft = $state<{ open: boolean; name: string; ns: string }>({ open: false, name: "", ns: "" });

function resetChan(name = "", category = "", ns = ""): void {
  chanDraft.ns = ns;
  chanDraft.name = name;
  chanDraft.category = category;
  chanDraft.announce = false;
  chanDraft.retention = "";
  chanDraft.voice = false;
  chanDraft.open = true;
}

export function openCreateChannel(prefillName = "", ns?: string): void {
  resetChan(prefillName, "", ns ?? "");
  ui.serverMenu = false;
}
export function openCreateChannelInCat(cat: string): void {
  resetChan("", cat); // "" = uncategorized (bare, top-level)
}

export function createChannel(): void {
  const ns = chanDraft.ns || scopedNs();
  const slug = chanDraft.name.trim().replace(/^#/, "").replace(/\s+/g, "-").toLowerCase();
  if (!slug || !ns) {
    chanDraft.open = false;
    return;
  }

  // v0.13: channels are `#<ns-id>/<chan-id>` — send the desired vanity as the
  // local segment; the server mints the id. We can't JOIN/META by the name we
  // sent (NO-SUCH-TARGET), so stash the follow-ups and apply them when
  // CHANNEL-LAYOUT echoes the canonical name (see `channelStore.reconcileCreate`).
  const full = `#${ns}/${slug}`;
  const key = `${ns}|${slug}`;
  channelStore.pendingChanCreate[key] = { cat: chanDraft.category.trim(), announce: chanDraft.announce, voice: chanDraft.voice };

  weft
    .channelCreate(full, chanDraft.voice ? undefined : chanDraft.retention || undefined, chanDraft.voice ? "voice" : undefined)
    .catch((e) => {
      delete channelStore.pendingChanCreate[key];
      toast(String(e), "error");
    });

  chanDraft.open = false;
}

export function openCreateCategory(ns?: string): void {
  catDraft.name = "";
  catDraft.ns = ns ?? "";
  catDraft.open = true;
  ui.serverMenu = false;
}
export function createCategory(): void {
  const ns = catDraft.ns || scopedNs();
  const n = catDraft.name.trim();
  if (!n || !ns) return;

  if (!channelStore.nsCategories(ns).includes(n)) channelStore.setCategories([...channelStore.nsCategories(ns), n], ns);
  catDraft.name = "";
  catDraft.open = false;
}

// Delete a category: uncategorize its channels (back to the bare top-level),
// then drop the label from the §6.3 NS categories list.
export function deleteCategory(cat: string): void {
  for (const c of Object.values(channelStore.channels)) {
    if (c.name.startsWith("#") && nsOf(c.name) === scopedNs() && (c.category || "") === cat) {
      c.category = undefined;
      weft.channelMeta(c.name, "category", "").catch(() => {});
    }
  }
  channelStore.setCategories(channelStore.nsCategories().filter((x) => x !== cat));
}
