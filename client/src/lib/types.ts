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
  /// The sender's network when foreign (`author@net` disambiguates federated
  /// users); absent for local senders, who render as a bare handle.
  net?: string;
  /// §13 `attach.N=` media references (`weft-media://…` URIs), in order.
  attachments?: string[];
};

export type Channel = {
  name: string;
  retention: string;
  messages: Msg[];
  members: Member[];
  /// History backfill.
  historyLoaded?: boolean;
  hasMore?: boolean; // older pages available upstream
  truncated?: boolean; // a retention gap at the top (§6.4)
  /// Channel management + layout.
  topic?: string;
  restricted?: boolean; // §6.7 posting requires the `send` cap
  lastRead?: string; // newest msgid we've marked read
  category?: string; // CHANNEL-LAYOUT grouping
  position?: number;
  voice?: boolean; // §16 a voice-only channel (kind=voice) — entered via VOICE JOIN
  rosterLoaded?: boolean; // MEMBERS snapshot fetched
  pinnedIds?: string[]; // pinned msgids (§6.4)
};

/// A right-click context-menu entry.
export type CtxItem = { label: string; danger?: boolean; run: () => void };

/// A namespace-scoped role definition (name + color + caps).
export type RoleDefC = { name: string; color: string; caps: string[] };
