// The client domain model — see docs/architecture/client-model-refactor.md.

/// The wire shape of a role definition (ROLE event), used to build a {@link Role}.
export interface RoleInit {
  id: string;
  name: string;
  color: string;
  caps: string[];
  hoist: boolean;
  pingable: boolean;
  position: number;
}

/**
 * A namespace / channel-scoped role definition (§6.5): name + color + caps +
 * hoist + pingable + position. The reactive replacement for the old `RoleDefC`
 * record — same fields, plus a `grants()` helper — so a `Membership` can hold
 * `Role` refs (Phase 3) instead of bare id strings.
 *
 * `caps` stays a `string[]` (not a Set) — the editors read/splice it directly.
 */
export class Role {
  /// Stable role ULID id (v0.13) — what mutations address; `name` is a label.
  readonly id: string;
  name = $state("");
  color = $state("");
  caps = $state<string[]>([]);
  hoist = $state(false);
  /// Whether members may @-mention this role to ping its holders (§9.3).
  pingable = $state(false);
  position = $state(0);

  constructor(init: RoleInit) {
    this.id = init.id;
    this.name = init.name;
    this.color = init.color;
    this.caps = init.caps;
    this.hoist = init.hoist;
    this.pingable = init.pingable;
    this.position = init.position;
  }

  /// Does this role carry a capability?
  grants(cap: string): boolean {
    return this.caps.includes(cap);
  }
}
