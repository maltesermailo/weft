// The client domain model — see docs/architecture/client-model-refactor.md.
import type { Msg, Member } from "$lib/types";
import type { Server } from "./server.svelte";
import { store } from "./store.svelte";

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

let msgSeq = 0;
/// Stamp a unique, monotonic render key onto a message (session-local).
export const mkMsg = (m: Omit<Msg, "key">): Msg => ({ ...m, key: msgSeq++ });

/// The namespace id in `#<ns>/<chan>`, else "" (top-level / DM / group).
export const nsOf = (name: string): string => name.match(/^#([^/]+)\//)?.[1] ?? "";

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
