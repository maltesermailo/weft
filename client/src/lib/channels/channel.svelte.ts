// The client domain model — see docs/architecture/client-model-refactor.md.
import { goto } from "$app/navigation";
import { page } from "$app/state";
import type { Msg, Member } from "$lib/types";
import type { Server } from "$lib/namespaces/server.svelte";
import { store } from "$lib/store/store.svelte";
import * as weft from "$lib/transport/weft";
import * as nav from "$lib/navigation/nav";
import { view } from "$lib/navigation/view.svelte";
import { toast } from "$lib/notifications/toasts.svelte";

/**
 * A channel, DM, or group conversation. The reactive replacement for the old
 * `Channel` record plus the four parallel per-name maps (`unreadMap`,
 * `mentionMap`, `unreadCount`, `mentionCount`) and the `typers` map — read state
 * now lives on the object it describes.
 *
 * Instances are stored in `+page.svelte`'s `channels` record. Svelte 5 does not
 * proxy class instances, so their `$state` fields stay individually reactive
 * even nested inside that `$state` record.
 *
 * Not yet folded in (later phases): `messages[].author` / `typers` become
 * `Account` refs with the Message model; the namespace back-reference and mute
 * level move onto `Server` (mute is a per-namespace setting, not per-channel).
 */
export class Channel {
  /// Canonical wire name `#<ns-id>/<chan-id>` (v0.13), `@dm`, or `&group`.
  /// Reassigned on CHANNEL rename (`channel-renamed`), which re-keys the record.
  name = $state("");
  /// The namespace this channel belongs to (null for a DM / group / top-level).
  /// Set at creation from `store.server(nsId)` — the upward graph edge.
  server = $state<Server | null>(null);
  /// Human display name (CHANNEL-LAYOUT `vanity=`); the wire name is opaque ids.
  vanity = $state<string | undefined>(undefined);
  retention = $state("retained");
  messages = $state<Msg[]>([]);
  members = $state<Member[]>([]);

  // ---- read state (was unreadMap / mentionMap / unreadCount / mentionCount) ----
  unread = $state(false);
  mention = $state(false);
  unreadCount = $state(0);
  mentionCount = $state(0);
  /// Accounts currently typing here (was the `typers` map). Handles for now.
  typers = $state<string[]>([]);

  // ---- history backfill ----
  historyLoaded = $state(false);
  hasMore = $state(false); // older pages available upstream
  truncated = $state(false); // a retention gap at the top (§6.4)

  // ---- management + layout ----
  topic = $state<string | undefined>(undefined);
  restricted = $state(false); // §6.7 posting requires the `send` cap
  viewGated = $state(false); // §6.3 visibility requires the `view` cap
  lastRead = $state<string | undefined>(undefined); // newest msgid marked read
  category = $state<string | undefined>(undefined); // CHANNEL-LAYOUT grouping
  position = $state(0);
  voice = $state(false); // §16 a voice-only channel (kind=voice)
  rosterLoaded = $state(false); // MEMBERS snapshot fetched
  pinnedIds = $state<string[] | undefined>(undefined); // pinned msgids (§6.4)

  constructor(name: string, retention = "retained") {
    this.name = name;
    this.retention = retention;
  }

  get isDm(): boolean {
    return this.name.startsWith("@");
  }
  get isGroup(): boolean {
    return this.name.startsWith("&");
  }
  /// The namespace id in `#<ns>/<chan>`, or "" for a top-level / DM / group channel.
  get nsId(): string {
    return this.name.match(/^#([^/]+)\//)?.[1] ?? "";
  }

  /// Are notifications silenced here? Mute is a per-namespace pref, so this walks
  /// to the owning `Server`; DMs / top-level channels use the `net` scope.
  get isMuted(): boolean {
    return this.server ? this.server.isMuted : store.mutedAt("net");
  }

  /// Channel opened / caught up — clear every unread counter.
  markRead(): void {
    this.unread = false;
    this.mention = false;
    this.unreadCount = 0;
    this.mentionCount = 0;
  }

  /// Per-user typing-expiry timers (§4 TYPING); a fallback in case a `stop` is lost.
  private typingTimers = new Map<string, ReturnType<typeof setTimeout>>();
  /// Mark a user as typing (or stopped) here, with a 6s fallback expiry.
  setTyping(user: string, active: boolean): void {
    clearTimeout(this.typingTimers.get(user));
    if (active) {
      if (!this.typers.includes(user)) this.typers = [...this.typers, user];
      this.typingTimers.set(
        user,
        setTimeout(() => this.setTyping(user, false), 6000),
      );
    } else {
      this.typers = this.typers.filter((u) => u !== user);
      this.typingTimers.delete(user);
    }
  }

  /// Tally one freshly-arrived message; a mention also bumps the mention counters.
  bump(mentioned: boolean): void {
    this.unread = true;
    this.unreadCount += 1;
    if (mentioned) {
      this.mention = true;
      this.mentionCount += 1;
    }
  }
}

// ---- the channel collection (was the layout's `channels` record + helpers) ----
// The app's open conversations, keyed by canonical wire name. A module singleton
// mutated in place (never reassigned) so it can be a `const` export imported bare
// and stay reactive across modules. Cleared via `resetChannels()` on logout.
export const channels = $state<Record<string, Channel>>({});

/// The namespace id in `#<ns>/<chan>`, else "" (top-level / DM / group).
export const nsOf = (name: string): string => name.match(/^#([^/]+)\//)?.[1] ?? "";

/// The covering scopes for the current target, most specific first: the channel
/// itself (if one is open), its namespace, then the network `*`. Used to pick a
/// default scope for invites and to resolve per-scope grants.
export function scopesFor(): string[] {
  const s: string[] = [];
  if (view.active.startsWith("#")) s.push(view.active);

  const ns = nsOf(view.active) || view.activeServer;
  if (ns) s.push(`ns:${ns}`);

  s.push("*");
  return s;
}

// ---- §6.3 category list (server state, on the namespace) ----
export const nsCategories = (): string[] => store.servers.get(view.activeServer)?.categories ?? [];
export function setCategories(list: string[]): void {
  if (view.activeServer) weft.nsMeta(view.activeServer, "categories", list.join(",")).catch((e) => toast(String(e), "error"));
}

// ---- Discord-style drag/drop reorder (ChannelList) ----
// Move a channel into `targetCat` at `anchorName` (before/after), then renumber
// that category so positions stay stable + ordered.
export function moveChannel(dragName: string, targetCat: string, anchorName?: string, after = false): void {
  const dragged = channels[dragName];
  if (!dragged) return;

  dragged.category = targetCat || undefined; // "" = uncategorized (bare top-level); optimistic
  weft.channelMeta(dragName, "category", targetCat).catch((e) => toast(String(e), "error"));

  const list = Object.values(channels)
    .filter(
      (c) => c.name.startsWith("#") && nsOf(c.name) === view.activeServer && (c.category || "") === targetCat && c.name !== dragName,
    )
    .sort((a, b) => (a.position ?? 0) - (b.position ?? 0) || a.name.localeCompare(b.name));

  let at = anchorName ? list.findIndex((c) => c.name === anchorName) : -1;
  if (at < 0) at = list.length;
  else if (after) at += 1;
  list.splice(at, 0, dragged);

  list.forEach((c, i) => {
    if (c.position !== i) {
      c.position = i;
      weft.channelMeta(c.name, "position", String(i)).catch(() => {});
    }
  });
}

// Reorder a named category (the bare top-level group "" stays put — only named
// categories are persisted in the §6.3 NS categories list).
export function moveCategory(dragCat: string, targetCat: string): void {
  if (dragCat === targetCat || dragCat === "") return;

  const cats = [...nsCategories()];
  const from = cats.indexOf(dragCat);
  if (from < 0) return;
  cats.splice(from, 1);

  let to = cats.indexOf(targetCat);
  if (to < 0) to = cats.length; // dropped on the implicit group → move to the end
  cats.splice(to, 0, dragCat);

  const s = store.servers.get(view.activeServer);
  if (s) s.categories = cats; // optimistic; the NS-META echo confirms
  setCategories(cats);
}

/// Short channel label: the vanity if known, else the raw local segment.
export const chanShort = (name: string): string =>
  channels[name]?.vanity || name.replace(/^#[^/]+\//, "").replace(/^#/, "");

/// The record for a name, or undefined (each MessageList reads its own).
export const channelRecord = (name: string): Channel | undefined => channels[name];

/// Intern a channel, seeding its Server edge + cached layout on first creation.
export function ensureChannel(name: string): Channel {
  let ch = channels[name];
  if (!ch) {
    ch = new Channel(name);
    const ns = nsOf(name);
    if (ns) ch.server = store.server(ns); // the Channel → Server graph edge
    const cached = ns ? layoutCache[ns]?.chans[name] : undefined;
    if (cached) {
      ch.category = cached.category;
      ch.position = cached.position ?? 0;
    }
    channels[name] = ch;
  }
  return ch;
}

/// Clear the unread counters for a channel by name.
export function markRead(name: string): void {
  channels[name]?.markRead();
}

/// Forget every conversation (logout) — mutates in place, never reassigns.
export function resetChannels(): void {
  for (const k of Object.keys(channels)) delete channels[k];
}

// ---- layout cache (server-authoritative, cached in localStorage for instant
// reload): per namespace, the category list + each channel's category/position.
type NsLayout = { cats: string[]; chans: Record<string, { category?: string; position?: number }> };
export const layoutCache = $state<Record<string, NsLayout>>({});

function saveLayoutCache(): void {
  try {
    localStorage.setItem("weft:layout", JSON.stringify(layoutCache));
  } catch {
    /* ignore */
  }
}
/// Restore the cached layout from localStorage (boot). Mutates in place.
export function loadLayoutCache(): void {
  for (const k of Object.keys(layoutCache)) delete layoutCache[k];
  try {
    Object.assign(layoutCache, JSON.parse(localStorage.getItem("weft:layout") ?? "{}"));
  } catch {
    /* ignore */
  }
}
export function cacheNsCats(ns: string, cats: string[]): void {
  (layoutCache[ns] ??= { cats: [], chans: {} }).cats = cats;
  saveLayoutCache();
}
export function cacheChanLayout(chanName: string, category: string | undefined, position: number): void {
  const ns = nsOf(chanName);
  if (!ns) return;
  (layoutCache[ns] ??= { cats: [], chans: {} }).chans[chanName] = { category, position };
  saveLayoutCache();
}

// ---- pending channel-creates (§6.3): follow-up actions stashed by `<ns>|<slug>`
// until the server echoes the canonical name + vanity, then reconciled. ----
export const pendingChanCreate: Record<string, { cat: string; announce: boolean; voice: boolean }> = {};

/// Finish a channel-create once the server confirms the canonical name + vanity:
/// subscribe (text), apply the stashed category/announce flags, and navigate.
export function reconcileChannelCreate(canonical: string, vanity: string): void {
  const ns = nsOf(canonical);
  if (!ns || !vanity) return;
  const key = `${ns}|${vanity}`;
  const pending = pendingChanCreate[key];
  if (!pending) return;
  delete pendingChanCreate[key];

  const ch = ensureChannel(canonical);
  ch.voice = pending.voice;
  // Subscribe (text) so live messages arrive; voice rooms are entered via VOICE.
  if (!pending.voice) weft.join(canonical).catch(() => {});
  if (pending.cat) weft.channelMeta(canonical, "category", pending.cat).catch((e) => toast(String(e), "error"));
  // Announcement channel: view-open, post-restricted to `send` holders (§6.7).
  if (!pending.voice && pending.announce)
    weft.channelMeta(canonical, "posting", "restricted").catch((e) => toast(String(e), "error"));
  // Jump to the new channel (a voice room is *entered* by clicking it).
  if (!pending.voice) markRead(canonical);
  goto(nav.pathFor(canonical));
}

// ---- open-DM persistence (the server doesn't yet track a DM list — §18) ----
// Persisted per account so a conversation (and its history on click) survives a
// reconnect / relaunch.
const dmStoreKey = () => `weft:dms:${store.session.account}@${store.session.network}`;
export function persistDms(): void {
  try {
    const keys = Object.keys(channels).filter((k) => k.startsWith("@"));
    localStorage.setItem(dmStoreKey(), JSON.stringify(keys));
  } catch {
    /* storage unavailable */
  }
}
export function restoreDms(): void {
  try {
    const keys: string[] = JSON.parse(localStorage.getItem(dmStoreKey()) ?? "[]");
    for (const k of keys) if (k.startsWith("@")) ensureChannel(k);
  } catch {
    /* storage unavailable */
  }
}

