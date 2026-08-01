// The client domain model — see docs/architecture/client-model-refactor.md.
import { SvelteMap } from "svelte/reactivity";
import * as media from "$lib/media/media";
import { Membership } from "$lib/membership/membership.svelte";
import type { Role } from "$lib/roles/role.svelte";
import { store, type NotifLevel } from "$lib/store/store.svelte";
import * as weft from "$lib/transport/weft";
import { toast } from "$lib/notifications/toasts.svelte";
import * as md from "$lib/rendering/markdown";
import { view } from "$lib/navigation/view.svelte";
import type { HandlerMap } from "$lib/sync/handler-map";

// ---- §6.2 namespace-admin editor (ServerSettingsModal) ----

/**
 * The Server Settings form draft + the ns-admin actions over the active
 * namespace. One reactive instance (`nsAdmin`) so the modal binds fields
 * directly (`nsAdmin.title`, …); `openNsSettings` seeds it from the live NS-META.
 * `newOwner` (§2.4 transfer) + the recovery-quorum/record fields ride along.
 */
export class NsAdmin {
  title = $state("");
  desc = $state("");
  vis = $state("public");
  newOwner = $state("");
  recM = $state(2);
  recKeys = $state("");
  myRecoveryKey = $state("");
  recoveryDoc = $state("");

  // Persist the overview edits (title/description/visibility) for the active ns.
  saveNsMeta(): void {
    const ns = view.activeServer;
    if (this.title.trim()) weft.nsMeta(ns, "title", this.title.trim()).catch(() => {});
    if (this.desc.trim()) weft.nsMeta(ns, "description", this.desc.trim()).catch(() => {});
    weft.nsVisibility(ns, this.vis).catch(() => {});
  }

  // §11.10 open/close this namespace to on-demand federation (needs public).
  nsSetFederation(open: boolean): void {
    weft.nsMeta(view.activeServer, "federation", open ? "open" : "closed").catch((e) => toast(String(e), "error"));
  }

  // §6.2 set (or clear, "") the channel that greets new members.
  nsSetWelcome(channel: string): void {
    if (!view.activeServer) return;
    weft.nsMeta(view.activeServer, "welcome", channel).catch((e) => toast(String(e), "error"));
  }

  // §2.4 recovery ladder: reveal my quorum pubkey / start / co-sign / submit a
  // recovery record. The record is shared out-of-band and co-signed to quorum.
  showRecoveryKey(): void {
    weft
      .recoveryPubkey(store.session.network, view.activeServer)
      .then((k) => (this.myRecoveryKey = k))
      .catch((e) => toast(String(e), "error"));
  }
  startRecovery(): void {
    weft
      .recoveryStart(store.session.network, view.activeServer, store.session.account)
      .then((doc) => {
        this.recoveryDoc = doc;
        toast("Recovery started — share this record with your quorum to co-sign");
      })
      .catch((e) => toast(String(e), "error"));
  }
  cosignRecovery(): void {
    if (!this.recoveryDoc.trim()) return;
    weft
      .recoveryCosign(store.session.network, view.activeServer, this.recoveryDoc.trim())
      .then((doc) => (this.recoveryDoc = doc))
      .catch((e) => toast(String(e), "error"));
  }
  submitRecovery(): void {
    if (this.recoveryDoc.trim()) weft.nsRecover(view.activeServer, this.recoveryDoc.trim()).catch((e) => toast(String(e), "error"));
  }

  // §9.4 custom emoji (namespace-scoped) admin actions.
  addEmoji(name: string, media: string): void {
    if (!view.activeServer) return;
    weft.emojiAdd(view.activeServer, name, media).catch((e) => toast(String(e), "error"));
  }
  removeEmoji(name: string): void {
    if (!view.activeServer) return;
    weft.emojiRemove(view.activeServer, name).catch((e) => toast(String(e), "error"));
  }
}

export const nsAdmin = new NsAdmin();

// ---- §9.4 custom emoji reads (namespace-scoped) ----
// The active namespace's custom emoji as an array (for pickers).
export const activeEmoji = (): { name: string; media: string }[] =>
  [...(view.activeServer ? (store.servers.get(view.activeServer)?.emoji ?? []) : [])].map(([name, media]) => ({ name, media }));

// Resolve a `:name:` shortcode to a fetchable image URL in the active namespace,
// or null if it isn't a custom emoji here.
export const emojiUrlFor = (name: string): string | null => {
  const ref = view.activeServer ? store.servers.get(view.activeServer)?.emoji.get(name) : undefined;
  return ref ? media.mediaUrl(ref) : null;
};

// The namespace whose §6.2 roster is currently streaming (ns-member-info events
// don't carry the ns, so the reducer routes rows to this target).
let rosterTarget: string | null = null;
export const rosterFetchTarget = (): string | null => rosterTarget;

/// One streamed `ns-member-info` roster row, buffered until the BATCH flush.
export interface RosterRow {
  user: string;
  network: string;
  joinedMs: number;
  roles: string[];
}

/// The NS-META fields this model consumes (a structural subset of the wire
/// event — kept here so the model stays decoupled from `$lib/weft`).
export interface NsMetaFields {
  name?: string | null;
  title?: string | null;
  description?: string | null;
  owner?: string | null;
  visibility?: string;
  federation?: boolean;
  welcome?: string | null;
  recovery_eta?: number | null;
  recovery_rung?: number | null;
  categories?: string[];
}

/**
 * A namespace — the Discord-style "server" (§2): the aggregate identity for a
 * community. Interned by `AppStore.server(id)`.
 *
 * Phase 2a owns the namespace's metadata, my membership, and its custom emoji.
 * Later phases fold in the member roster (`Member`) and role definitions
 * (`Role`) — so "a Server has Members which have Roles" becomes literal — plus
 * mute (per-namespace `notifPrefs`) and the permission traversal.
 */
export class Server {
  /// Immutable namespace id (v0.13) — addresses are `ns:<id>` and `#<id>/…`.
  readonly id: string;
  /// Vanity display name (NS-META `name`); mutable label, not identity.
  name = $state("");
  title = $state<string | null>(null);
  description = $state<string | null>(null);
  /// Owner handle (an `Account` ref lands with the roster in Phase 2b).
  owner = $state<string | null>(null);
  visibility = $state("public");
  federation = $state(false);
  welcome = $state<string | null>(null);
  recoveryEta = $state<number | null>(null);
  recoveryRung = $state<number | null>(null);
  categories = $state<string[]>([]);
  /// Am I a member? (was `memberNs`) — owning a namespace implies membership.
  joined = $state(false);
  /// Have we received NS-META yet? Distinguishes "known only by id" (a channel's
  /// namespace we haven't fetched) from "loaded" (shows in Discover, has a name).
  metaLoaded = $state(false);
  /// §9.4 custom emoji, `name` → media ref (was `customEmoji[id]`).
  readonly emoji = new SvelteMap<string, string>();
  /// §6.2 the moderator roster (NS INFO MEMBERS), once fetched (was
  /// `nsMembersByNs[id]`). Empty until a mod fetches it; not all servers load it.
  members = $state<Membership[]>([]);
  /// Roster fetch state: `membersLoading` gates the UI spinner; `memberBuf`
  /// accumulates the streamed `ns-member-info` rows until the `ni…` BATCH
  /// terminator flushes them into `members` (transient, non-reactive).
  membersLoading = $state(false);
  memberBuf: RosterRow[] = [];
  /// §6.5 this namespace's role definitions (position-ordered), once fetched.
  /// Populated from the `ns:<id>` ROLE flush; `Membership.roles` resolves here.
  roles = $state<Role[]>([]);

  constructor(id: string) {
    this.id = id;
  }

  /// v0.13 rail-tile / header display: vanity title, else name, else the id.
  get displayName(): string {
    return this.title || this.name || this.id;
  }
  /// The capability scope string for this namespace.
  get scope(): string {
    return `ns:${this.id}`;
  }

  /// My notification level for this namespace (a client-side pref, default
  /// "mentions"). Set in the Notification Settings modal.
  get muteLevel(): NotifLevel {
    return store.notifAt(this.scope);
  }
  /// Whether this namespace is fully silenced (no unread dots / badges).
  get isMuted(): boolean {
    return this.muteLevel === "nothing";
  }

  /// A roster row by bare account handle, or undefined (§6.2 mutation helpers).
  member(handle: string): Membership | undefined {
    return this.members.find((m) => m.account.name === handle);
  }

  /// §6.2 fetch this namespace's moderator roster (NS INFO MEMBERS). Streams back
  /// as `ns-member-info` events into `memberBuf`, flushed by `applyMembers`.
  fetchMembers(): void {
    rosterTarget = this.id;
    this.membersLoading = true;
    this.memberBuf = [];
    weft.nsInfoMembers(this.id).catch((e) => {
      this.membersLoading = false;
      rosterTarget = null;
      toast(String(e), "error");
    });
  }

  /// Build `members` from the streamed rows (the `ni…` BATCH terminator). `me`
  /// is my own network, so same-network handles stay bare.
  applyMembers(me: string): void {
    this.members = this.memberBuf.map((r) => {
      const handle = r.network === me ? r.user : `${r.user}@${r.network}`;
      const m = new Membership(this, store.accountOf(handle));
      m.network = r.network;
      m.joinedMs = r.joinedMs;
      m.roleIds = r.roles;
      return m;
    });
    this.memberBuf = [];
    this.membersLoading = false;
    rosterTarget = null;
  }

  /// A role definition by id (§6.5), or undefined.
  role(id: string): Role | undefined {
    return this.roles.find((r) => r.id === id);
  }

  /// Absorb an NS-META event (marks the namespace loaded).
  applyMeta(e: NsMetaFields): void {
    this.name = e.name ?? "";
    this.title = e.title ?? null;
    this.description = e.description ?? null;
    this.owner = e.owner ?? null;
    this.visibility = e.visibility ?? "public";
    this.federation = e.federation ?? false;
    this.welcome = e.welcome ?? null;
    this.recoveryEta = e.recovery_eta ?? null;
    this.recoveryRung = e.recovery_rung ?? null;
    // `categories` is model-owned now (client-core) — applied via the `cat-list`
    // diff from the same NS-META, not set here.
    this.metaLoaded = true;
  }
}

/// §9.4 custom-emoji wire-event handlers: keep a namespace's emoji map current
/// (and drop the markdown cache since rendered `:name:` may change).
export const serverHandlers: HandlerMap = {
  emoji: (e) => {
    store.server(e.namespace).emoji.set(e.name, e.media);
    md.clearMdCache();
  },
  "emoji-removed": (e) => {
    store.servers.get(e.namespace)?.emoji.delete(e.name);
    md.clearMdCache();
  },
};
