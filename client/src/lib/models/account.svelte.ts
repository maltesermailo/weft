// The client domain model — see docs/architecture/client-model-refactor.md.
import * as weft from "$lib/weft";

/**
 * A global account identity (§10.3): profile + presence, independent of any one
 * server. Interned by {@link AppStore.accountOf} so every reference to a handle
 * — a message author, a server member, a friend, a DM peer — resolves to the
 * *same* instance; a profile or presence update lands once and every surface
 * reacts.
 *
 * Per-server facts (nickname, roles, effective caps) are deliberately NOT here —
 * they belong to a future `Member` (the Server↔Account join, Phase 2).
 */
export class Account {
  /** Canonical handle: bare `name` for a local account, `name@network` if federated. */
  readonly handle: string;

  /** Live presence, or `undefined` until we've heard any — rendered "offline". */
  presence = $state<string | undefined>(undefined);

  display = $state<string | undefined>(undefined);
  avatar = $state<string | undefined>(undefined);
  about = $state("");
  status = $state("");

  /** A `PROFILES` fetch has been issued — the dedup guard for on-demand queries. */
  requested = $state(false);

  constructor(handle: string) {
    this.handle = handle;
  }

  /** The bare account part, without any `@network` suffix. */
  get name(): string {
    return this.handle.split("@")[0];
  }

  /** Two-letter monogram fallback shown when there's no avatar. */
  get initials(): string {
    return this.name.replace(/[^a-z0-9]/gi, "").slice(0, 2).toUpperCase() || "··";
  }

  /** Presence dot class (`dot online` / `dot offline` / …), defaulting to offline. */
  get dotClass(): string {
    return `dot ${this.presence ?? "offline"}`;
  }

  /** Global display name — server nicknames layer on top at the `Member` level. */
  get displayName(): string {
    return this.display || this.name;
  }

  /** §10.3 a fetchable avatar URL, or null → render {@link initials}. */
  get avatarUrl(): string | null {
    return this.avatar ? weft.avatarUrl(this.avatar) : null;
  }
}
