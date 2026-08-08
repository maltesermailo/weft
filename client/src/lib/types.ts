// Shared client-side domain types.

export type Member = { name: string; origin: "local" | "federated" };

export type Msg = {
  /// Stable render key (msgids aren't on system lines, and prepending history
  /// shifts array indices — so keying by index would misrender).
  key: number;
  author: string;
  body: string;
  time: string;
  /// Epoch ms for day grouping / the unread divider (from the ULID, or arrival).
  ts: number;
  own: boolean;
  system?: boolean;
  /// Origin msgid — the target for edit / delete / react / reply. Absent on
  /// system lines.
  msgid?: string;
  /// Shows the "(edited)" marker.
  edited?: boolean;
  /// emoji → aggregate count + whether *I* reacted.
  reactions?: Record<string, { count: number; mine: boolean }>;
  /// Render body as markdown (§9.4 `fmt=md`).
  md?: boolean;
  /// msgid this replies to (§9.3).
  replyTo?: string;
  /// Root msgid this message belongs to, when it's a thread reply (§9.4).
  thread?: string;
  /// Sender is from a federated peer network.
  bridged?: boolean;
  /// Framework §7a: stored here, but the realm this channel mirrors never got it
  /// and will not be retried. weftd's echo only ever acked local storage, so
  /// without this the message read as sent.
  failed?: boolean;
  /// Why, for the tooltip.
  failReason?: string;
  /// The sender's network when foreign (`author@net` disambiguates federated
  /// users); absent for local senders, who render as a bare handle.
  net?: string;
  /// §13 `attach.N=` media references (`weft-media://…` URIs), in order.
  attachments?: string[];
  /// §3.5 the request label. On an optimistic placeholder it is the key the
  /// server echoes back on our own `MESSAGE` — locally, or re-attached by our
  /// server for a home-authoritative channel minted elsewhere (§11.13) — so we
  /// reconcile; cleared once the real copy arrives.
  label?: string;
  /// Optimistic placeholder: shown immediately on send, greyed as "sending",
  /// until the authoritative copy (matched by `label`) replaces it.
  pending?: boolean;
};

/// §9.4 a channel thread as summarized in a `THREADS` list: its root msgid,
/// optional display name, reply count, and last-activity msgid.
export type ThreadInfo = {
  root: string;
  name?: string;
  replies: number;
  last?: string;
};

// `Channel` is now a reactive class — see $lib/channels/channel.svelte.

/// A right-click context-menu entry.
export type CtxItem =
  | { label: string; danger?: boolean; run: () => void; icon?: string }
  | { header: string; mod?: boolean }
  | { divider: true };

/// One @-mention autocomplete suggestion (a row in the composer's selection
/// pop). Members carry an avatar (resolved from `name`) + a resolved display
/// name + their canonical `account@network` identity; `@everyone`/`@here` and
/// pingable roles are the other kinds.
export type MentionOpt = {
  /// The token inserted into the composer (`@name`).
  name: string;
  kind: "special" | "role" | "member";
  /// The label shown — a member's display name, or the word for everyone/here.
  display: string;
  /// A member's canonical `account@network` handle, shown at the right edge.
  identity?: string;
  /// A pingable role's display color.
  color?: string;
};

// The `NS INFO MEMBERS` roster row is now the `Membership` class — see
// $lib/membership/membership.svelte.

// The role definition is now the `Role` class — see $lib/roles/role.svelte.
