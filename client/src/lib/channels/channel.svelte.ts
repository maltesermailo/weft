// The channels domain model — see docs/architecture/client-model-refactor.md.
// File order (project convention): definitions → classes → operations → events.
import { goto } from "$app/navigation";
import type { Msg, Member } from "$lib/types";
import type { Server } from "$lib/namespaces/server.svelte";
import { store } from "$lib/store/store.svelte";
import * as weft from "$lib/transport/weft";
import * as nav from "$lib/navigation/nav";
import { view } from "$lib/navigation/view.svelte";
import { toast } from "$lib/notifications/toasts.svelte";

// ---- definitions ----

/// Per-namespace layout cache (localStorage): the category list + each channel's
/// category/position, so a reload paints the sidebar instantly (server-authoritative).
type NsLayout = { cats: string[]; chans: Record<string, { category?: string; position?: number }> };

// ---- classes ----

/**
 * A channel, DM, or group conversation. The reactive replacement for the old
 * `Channel` record plus the four parallel per-name maps (`unreadMap`,
 * `mentionMap`, `unreadCount`, `mentionCount`) and the `typers` map — read state
 * now lives on the object it describes.
 *
 * Instances are stored in `channelStore.channels`. Svelte 5 does not proxy class
 * instances, so their `$state` fields stay individually reactive even nested
 * inside that `$state` record.
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

/**
 * The collection of open conversations + the operations over it (was the module
 * `channels` record plus a dozen free helpers). A single reactive instance
 * (`channelStore`); its `$state` fields stay reactive across bare imports.
 */
export class ChannelStore {
  /// Open conversations, keyed by canonical wire name. Mutated in place.
  channels = $state<Record<string, Channel>>({});
  /// Per-namespace layout cache, mirrored to localStorage.
  layoutCache = $state<Record<string, NsLayout>>({});
  /// Pending channel-creates (§6.3): follow-up actions stashed by `<ns>|<slug>`
  /// until the server echoes the canonical name + vanity, then reconciled.
  pendingChanCreate: Record<string, { cat: string; announce: boolean; voice: boolean }> = {};

  /// The record for a name, or undefined (each MessageList reads its own).
  get(name: string): Channel | undefined {
    return this.channels[name];
  }

  /// Intern a channel, seeding its Server edge + cached layout on first creation.
  ensure(name: string): Channel {
    let ch = this.channels[name];
    if (!ch) {
      ch = new Channel(name);
      const ns = nsOf(name);
      if (ns) ch.server = store.server(ns); // the Channel → Server graph edge
      const cached = ns ? this.layoutCache[ns]?.chans[name] : undefined;
      if (cached) {
        ch.category = cached.category;
        ch.position = cached.position ?? 0;
      }
      this.channels[name] = ch;
    }
    return ch;
  }

  /// Clear the unread counters for a channel by name.
  markRead(name: string): void {
    this.channels[name]?.markRead();
  }

  /// Forget every conversation (logout) — mutates in place, never reassigns.
  reset(): void {
    for (const k of Object.keys(this.channels)) delete this.channels[k];
  }

  /// Short channel label: the vanity if known, else the raw local segment.
  short(name: string): string {
    return this.channels[name]?.vanity || name.replace(/^#[^/]+\//, "").replace(/^#/, "");
  }

  // ---- §6.3 category list (server state, on the namespace) ----
  nsCategories(): string[] {
    return store.servers.get(view.activeServer)?.categories ?? [];
  }
  setCategories(list: string[]): void {
    if (view.activeServer) weft.nsMeta(view.activeServer, "categories", list.join(",")).catch((e) => toast(String(e), "error"));
  }

  // ---- Discord-style drag/drop reorder (ChannelList) ----
  // Move a channel into `targetCat` at `anchorName` (before/after), then renumber
  // that category so positions stay stable + ordered.
  moveChannel(dragName: string, targetCat: string, anchorName?: string, after = false): void {
    const dragged = this.channels[dragName];
    if (!dragged) return;

    dragged.category = targetCat || undefined; // "" = uncategorized (bare top-level); optimistic
    weft.channelMeta(dragName, "category", targetCat).catch((e) => toast(String(e), "error"));

    const list = Object.values(this.channels)
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
  moveCategory(dragCat: string, targetCat: string): void {
    if (dragCat === targetCat || dragCat === "") return;

    const cats = [...this.nsCategories()];
    const from = cats.indexOf(dragCat);
    if (from < 0) return;
    cats.splice(from, 1);

    let to = cats.indexOf(targetCat);
    if (to < 0) to = cats.length; // dropped on the implicit group → move to the end
    cats.splice(to, 0, dragCat);

    const s = store.servers.get(view.activeServer);
    if (s) s.categories = cats; // optimistic; the NS-META echo confirms
    this.setCategories(cats);
  }

  // ---- layout cache persistence ----
  private saveLayout(): void {
    try {
      localStorage.setItem("weft:layout", JSON.stringify(this.layoutCache));
    } catch {
      /* ignore */
    }
  }
  /// Restore the cached layout from localStorage (boot). Mutates in place.
  loadLayout(): void {
    for (const k of Object.keys(this.layoutCache)) delete this.layoutCache[k];
    try {
      Object.assign(this.layoutCache, JSON.parse(localStorage.getItem("weft:layout") ?? "{}"));
    } catch {
      /* ignore */
    }
  }
  cacheNsCats(ns: string, cats: string[]): void {
    (this.layoutCache[ns] ??= { cats: [], chans: {} }).cats = cats;
    this.saveLayout();
  }
  cacheChanLayout(chanName: string, category: string | undefined, position: number): void {
    const ns = nsOf(chanName);
    if (!ns) return;
    (this.layoutCache[ns] ??= { cats: [], chans: {} }).chans[chanName] = { category, position };
    this.saveLayout();
  }

  /// Finish a channel-create once the server confirms the canonical name + vanity:
  /// subscribe (text), apply the stashed category/announce flags, and navigate.
  reconcileCreate(canonical: string, vanity: string): void {
    const ns = nsOf(canonical);
    if (!ns || !vanity) return;
    const key = `${ns}|${vanity}`;
    const pending = this.pendingChanCreate[key];
    if (!pending) return;
    delete this.pendingChanCreate[key];

    const ch = this.ensure(canonical);
    ch.voice = pending.voice;
    // Subscribe (text) so live messages arrive; voice rooms are entered via VOICE.
    if (!pending.voice) weft.join(canonical).catch(() => {});
    if (pending.cat) weft.channelMeta(canonical, "category", pending.cat).catch((e) => toast(String(e), "error"));
    // Announcement channel: view-open, post-restricted to `send` holders (§6.7).
    if (!pending.voice && pending.announce)
      weft.channelMeta(canonical, "posting", "restricted").catch((e) => toast(String(e), "error"));
    // Jump to the new channel (a voice room is *entered* by clicking it).
    if (!pending.voice) this.markRead(canonical);
    goto(nav.pathFor(canonical));
  }

  // ---- open-DM persistence (the server doesn't yet track a DM list — §18) ----
  // Persisted per account so a conversation (and its history on click) survives a
  // reconnect / relaunch.
  private dmStoreKey(): string {
    return `weft:dms:${store.session.account}@${store.session.network}`;
  }
  persistDms(): void {
    try {
      const keys = Object.keys(this.channels).filter((k) => k.startsWith("@"));
      localStorage.setItem(this.dmStoreKey(), JSON.stringify(keys));
    } catch {
      /* storage unavailable */
    }
  }
  restoreDms(): void {
    try {
      const keys: string[] = JSON.parse(localStorage.getItem(this.dmStoreKey()) ?? "[]");
      for (const k of keys) if (k.startsWith("@")) this.ensure(k);
    } catch {
      /* storage unavailable */
    }
  }
}

/// The app's channel collection + operations. A module singleton.
export const channelStore = new ChannelStore();

// ---- operations ----

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

// ---- events ----
// The channels domain's wire-event handlers live in `sync/channel-handlers.ts`
// (member / ns-member / chanmeta / channel-layout / channel-renamed), registered
// by the reducer alongside the other domains' handler maps.
