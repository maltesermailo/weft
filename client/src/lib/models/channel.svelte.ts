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
