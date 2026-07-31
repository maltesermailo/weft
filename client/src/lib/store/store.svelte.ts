// The client domain model — see docs/architecture/client-model-refactor.md.
import { SvelteMap } from "svelte/reactivity";
import { Account } from "$lib/profile/account.svelte";
import { Server } from "$lib/namespaces/server.svelte";
import { Federation } from "$lib/federation/federation.svelte";
import { Social } from "$lib/social/social.svelte";
import { Session } from "$lib/session/session.svelte";
import { SearchPanel, PinsPanel } from "$lib/messages/panels.svelte";
import { Threads } from "$lib/messages/threads.svelte";
import { Invites } from "$lib/invites/invites.svelte";
import { Reports } from "$lib/moderation/reports.svelte";

/// A scope's notification level (§ notification prefs). Per-namespace (`ns:<id>`)
/// or `net` for top-level channels; the default keeps only DMs/@mentions pinging.
export type NotifLevel = "all" | "mentions" | "nothing";
const NOTIF_KEY = "weft:notif-prefs";

/// A capability grant at a scope (§6.5): a subject and their direct cap list —
/// the channel/ns member overrides shown in the permission editor.
export interface GrantRow {
  subject: string;
  caps: string[];
}
/// A moderation deny-list entry at a scope (§6.7): a mute or ban on an account.
export interface DenyRow {
  account: string;
  kind: string;
  by?: string | null;
  reason?: string | null;
}

/**
 * Root client store: the identity maps and (from Phase 5) the inbound-event
 * reducer. The app reads domain objects from here instead of the parallel
 * string-keyed records that used to live in `+page.svelte`.
 *
 * Phase 0 lands the {@link Account} identity map; `Server`/`Channel`/`Member`
 * and `apply(event)` follow in later phases.
 */
export class AppStore {
  /** The identity map — interns one {@link Account} per handle. */
  readonly accounts = new SvelteMap<string, Account>();

  /** Namespaces (Discord-style servers), interned by id. */
  readonly servers = new SvelteMap<string, Server>();

  /** Notification level per scope (`ns:<id>` | `net`), client-side + persisted. */
  readonly notifPrefs = new SvelteMap<string, NotifLevel>();

  /** §6.5 capability grants per scope (channel/ns member overrides). */
  readonly grants = new SvelteMap<string, GrantRow[]>();
  /** §6.7 moderation deny-list (mutes + bans) per scope. */
  readonly deny = new SvelteMap<string, DenyRow[]>();

  /** Operator-facing federation state (§11): netblocks + peering manifests. */
  readonly federation = new Federation();

  /** The social layer: friends, group DMs, and calls. */
  readonly social = new Social();

  /** The current user: identity + the resolved capability cache + gate methods. */
  readonly session = new Session();

  /** §6.4 message-search panel state (server-streamed results). */
  readonly search = new SearchPanel();
  /** §6.4 pinned-messages panel state (server-streamed). */
  readonly pins = new PinsPanel();
  /** §9.4 threads: open side panel + list modal. */
  readonly threads = new Threads();
  /** §6.5 invites: list menu + create screen. */
  readonly invites = new Invites();
  /** §6.7 moderation reports: queue modal + report-filing target. */
  readonly reports = new Reports();

  constructor() {
    this.loadNotif();
  }

  private loadNotif(): void {
    if (typeof localStorage === "undefined") return;
    try {
      const obj = JSON.parse(localStorage.getItem(NOTIF_KEY) ?? "{}") as Record<string, NotifLevel>;
      for (const [scope, level] of Object.entries(obj)) this.notifPrefs.set(scope, level);
    } catch {
      /* corrupt / unavailable — start empty */
    }
  }

  /** Effective notification level for a scope (default "mentions"). */
  notifAt(scope: string): NotifLevel {
    return this.notifPrefs.get(scope) ?? "mentions";
  }

  /** Whether a scope is fully silenced. */
  mutedAt(scope: string): boolean {
    return this.notifAt(scope) === "nothing";
  }

  /** Set + persist the notification level for a scope. */
  setNotif(scope: string, level: NotifLevel): void {
    this.notifPrefs.set(scope, level);
    if (typeof localStorage === "undefined") return;
    try {
      localStorage.setItem(NOTIF_KEY, JSON.stringify(Object.fromEntries(this.notifPrefs)));
    } catch {
      /* private mode — in-memory only */
    }
  }

  /**
   * Get-or-create the shared {@link Account} for a handle. A leading `@` (as in
   * a DM channel key) is stripped so `@bob` and `bob` are the same identity.
   */
  accountOf(handle: string): Account {
    const key = handle.replace(/^@/, "");
    let a = this.accounts.get(key);
    if (!a) {
      a = new Account(key);
      this.accounts.set(key, a);
    }
    return a;
  }

  /** Get-or-create the {@link Server} for a namespace id. */
  server(id: string): Server {
    let s = this.servers.get(id);
    if (!s) {
      s = new Server(id);
      this.servers.set(id, s);
    }
    return s;
  }

  /**
   * Reconnect housekeeping: forget presence — everyone re-announces on the new
   * session — while keeping cached profiles (display name, avatar, bio), which
   * survive a reconnect to the same server.
   */
  resetPresence(): void {
    for (const a of this.accounts.values()) a.presence = undefined;
  }
}

/**
 * The single client store instance. Exported as a module singleton so domain
 * models (`Channel`, `Server`, …) can navigate to shared state — e.g.
 * `Channel.isMuted` reading `store.notifAt(...)` — without threading a reference
 * through every constructor. `+page.svelte` uses this same instance.
 */
export const store = new AppStore();
