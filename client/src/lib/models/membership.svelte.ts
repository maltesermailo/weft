// The client domain model — see docs/architecture/client-model-refactor.md.
import type { Account } from "./account.svelte";
import type { Server } from "./server.svelte";
import type { Role } from "./role.svelte";

/**
 * A namespace membership — the Server↔Account join (§6.2 NS INFO MEMBERS
 * roster). This is the object the old flat model was missing: an account's
 * identity *within one server* (its join time and ns-scoped role assignments),
 * distinct from the lightweight channel-presence `Member` (`{name, origin}`).
 *
 * `account` is the shared interned {@link Account}, so a roster row, a message
 * author, and a DM peer for the same person are one object. `roleIds` are the
 * ns-scoped role ids from NS-MEMBER-INFO; they resolve to `Role` objects once
 * roles become first-class in Phase 3 (`get roles(): Role[]`).
 */
export class Membership {
  readonly server: Server;
  readonly account: Account;
  /// The member's home network (may differ from ours for a federated member).
  network = $state("");
  /// Unix-ms join time (`0` = unknown / pre-v0.12 backfill).
  joinedMs = $state(0);
  /// Assigned ns-scoped role ids (v0.13 NS-MEMBER-INFO).
  roleIds = $state<string[]>([]);

  constructor(server: Server, account: Account) {
    this.server = server;
    this.account = account;
  }

  /// The assigned role definitions, resolved against the server's roles (§6.5).
  /// Skips ids whose definition hasn't been fetched yet.
  get roles(): Role[] {
    return this.roleIds.map((id) => this.server.role(id)).filter((r): r is Role => !!r);
  }
}
