//! Storage row types. Event-sourced (§9.3): edits, deletes, and reactions
//! are rows referencing the original message's msgid — never mutations.

use weft_proto::{
    Account, ChannelName, ContentState, MsgId, MsgMeta, NamespaceName, NetworkName, ReportStatus,
    ResolveAction, RetentionPolicy, Ulid, UserRef,
};

/// Where events live: a channel, or a same-network DM pair (§9.5).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scope {
    Channel(ChannelName),
    /// Participants in sorted order — `Scope::dm` normalizes, so
    /// (ada, bob) and (bob, ada) are the same conversation.
    Dm(Account, Account),
}

impl Scope {
    pub fn dm(a: Account, b: Account) -> Self {
        if a <= b {
            Scope::Dm(a, b)
        } else {
            Scope::Dm(b, a)
        }
    }

    /// Stable string key: the channel name, or `dm:<a>:<b>`. Used as the
    /// database key and safe because channel names always start with `#`.
    pub fn as_key(&self) -> String {
        match self {
            Scope::Channel(channel) => channel.to_string(),
            Scope::Dm(a, b) => format!("dm:{a}:{b}"),
        }
    }

    /// Inverse of [`Scope::as_key`], for backends rehydrating rows.
    pub fn from_key(key: &str) -> Option<Self> {
        if key.starts_with('#') {
            return key.parse().ok().map(Scope::Channel);
        }
        let (a, b) = key.strip_prefix("dm:")?.split_once(':')?;
        Some(Scope::dm(a.parse().ok()?, b.parse().ok()?))
    }
}

/// What happened. `Message` rows are roots; everything else is a child of
/// its `root` msgid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    Message { body: String, meta: MsgMeta },
    Edit { body: String },
    Delete,
    React { emoji: String, add: bool },
}

/// One stored event. Timestamps live inside the msgid's ULID (§9.6 —
/// server-stamped, single source of truth).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRecord {
    pub scope: Scope,
    /// This event's own id (every event gets one, §9.3).
    pub msgid: MsgId,
    /// The original message this event belongs to; equals `msgid` for
    /// `Message` rows.
    pub root: MsgId,
    pub sender: UserRef,
    pub kind: EventKind,
}

impl EventRecord {
    pub fn at_ms(&self) -> u64 {
        self.msgid.timestamp_ms()
    }

    pub fn is_root(&self) -> bool {
        matches!(self.kind, EventKind::Message { .. })
    }
}

/// A HISTORY window (§6.4): exclusive cursors, newest-anchored — the last
/// `limit` roots strictly between `after` and `before`.
#[derive(Debug, Clone, Copy)]
pub struct Page {
    pub before: Option<Ulid>,
    pub after: Option<Ulid>,
    pub limit: usize,
}

/// A verification claim on an account — the *infrastructure* for
/// email/age/phone verification. `kind` is an open namespace ("email",
/// "age", ...); `subject` is what is being verified (an address, a birth
/// year assertion, ...). A claim starts unverified; a verifier (SMTP flow,
/// ID provider, operator panel — all later work) confirms it. The wire
/// protocol for *proving* a claim is a spec decision (§18) and
/// deliberately not implemented here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verification {
    pub kind: String,
    pub subject: String,
    /// Unix seconds when confirmed; `None` = still pending.
    pub verified_at: Option<u64>,
}

/// A channel's stored settings (§6.3). `view_gated` needs the anti-
/// enumeration branch in the session layer (invariant 1); `topic` rides
/// `CHANMETA`. `category` + `position` are the Discord-style layout within
/// a namespace (spec extension — see Appendix A): channels sort by
/// (category, position, name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelRecord {
    pub policy: RetentionPolicy,
    pub topic: Option<String>,
    pub view_gated: bool,
    /// `restricted` posting mode (§6.7): when set, `MSG` requires the `send`
    /// capability (grant/revoke governs who may post). Default `false` = open.
    pub restricted: bool,
    /// Category name (a free label) grouping channels in a namespace.
    pub category: Option<String>,
    /// Sort order within the (namespace, category); default 0.
    pub position: i64,
}

/// A recorded capability grant (§6.5, §10.4). The server keeps these so an
/// authed same-network account's caps are checkable without a token round-
/// trip; the signed token returned to the client is for delegation and
/// federation. `subject` is an account name or b64 pubkey; `scope` is a
/// raw scope string (`#chan|ns:<name>|*`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantRecord {
    pub subject: String,
    pub scope: String,
    pub caps: Vec<String>,
    /// The scope revocation epoch at issue; a later epoch bump invalidates
    /// this grant (§10.4).
    pub epoch: u64,
    /// Unix seconds; `None` = no expiry (operator/root grants).
    pub expiry: Option<u64>,
}

/// A minted invite (§6.5): an unbound authorization redeemable up to
/// `uses_left` times until `expiry`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteRecord {
    pub id: String,
    pub scope: String,
    pub caps: Vec<String>,
    /// `None` = unlimited uses.
    pub uses_left: Option<u32>,
    /// Unix seconds; `None` = no expiry.
    pub expiry: Option<u64>,
}

/// A user-owned namespace (§2.1, §2.2). `owner` is the account that
/// created it (holds all caps at `ns:<name>` same-network); `root_key` is
/// the client-generated root pubkey the owner holds — the crypto anchor
/// for TRANSFER/recovery/federation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceRecord {
    pub name: NamespaceName,
    pub owner: Account,
    /// Base64 Ed25519 root pubkey (§2.1).
    pub root_key: String,
    /// `public | unlisted | private` (§2.2), stored as the wire string.
    pub visibility: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub icon: Option<String>,
    /// §2.4 recovery quorum: `(m, [b64 pubkey, …])` when designated.
    pub recovery_set: Option<(u32, Vec<String>)>,
    /// A recovery in its delay window (§2.4).
    pub pending_recovery: Option<PendingRecovery>,
}

/// A recovery in flight: rotates to `new_root_key` + `new_owner` at `eta_ms`
/// unless the current root cancels first (§2.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRecovery {
    pub new_root_key: String,
    pub new_owner: String,
    /// Unix ms when the rotation applies.
    pub eta_ms: u64,
    /// 2 = social quorum, 3 = operator last resort.
    pub rung: u8,
}

/// One entry of a namespace's `root-history` (§2.4) — an append-only audit
/// of every root rotation. `operator_initiated` marks rung-3 forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootHistoryEntry {
    pub root_key: String,
    pub owner: String,
    pub at_ms: u64,
    pub operator_initiated: bool,
}

/// A filed report (§6.7). One row per `REPORT`; resolvable once. Holds on
/// the reported content (`held_roots`) are placed at filing when the state
/// is `Verified` and released a grace window after resolution (invariant 11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportRecord {
    /// ULID string — sorts by filing time; also the cursor.
    pub id: String,
    /// The reported message (a root msgid).
    pub msgid: MsgId,
    /// Where the message lives — for hold placement and handler display.
    pub scope: Scope,
    pub category: String,
    /// Honest content state (§6.7).
    pub state: ContentState,
    pub reporter: Account,
    pub note: Option<String>,
    /// Scope strings a handler lists this report under (`ns:<name>`, `*`).
    /// `csam`/`illegal` carry both the ns scope and `*` (operator).
    pub queue_scopes: Vec<String>,
    pub status: ReportStatus,
    pub filed_at_ms: u64,
    /// Roots under retention hold for this report — populated by the store
    /// at filing (empty unless `Verified`). Released after resolution+grace.
    pub held_roots: Vec<Ulid>,
    pub resolution: Option<ReportResolution>,
    /// Set once the grace window passes and holds are released (idempotence).
    pub holds_released: bool,
}

/// How a report was closed (§6.7). `hold_release_at` schedules the §12.1
/// grace so held content survives resolution by the grace window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportResolution {
    pub action: ResolveAction,
    pub note: Option<String>,
    pub resolved_by: Account,
    pub at_ms: u64,
    /// Unix ms after which the retention holds may be released.
    pub hold_release_at: u64,
}

/// A bridge peering with a remote network (§11.1). Stores the current signed
/// manifest and the last *mutually-acked* one so forwarding can be gated on
/// their intersection (invariant 3): a channel is forwardable to `peer` iff it
/// is present in **both** the acked snapshot (so the peer agreed to it) and the
/// current snapshot (so a `BRIDGE REMOVE` stops it at once). `manifest` and
/// `acked_manifest` are base64 [`weft_crypto::SignedManifest`] blobs — opaque
/// to the store, decoded by weft-core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRecord {
    pub peer: NetworkName,
    /// The original `BRIDGE PROPOSE` scope (`#chan`|`ns:<name>`|`*`).
    pub scope: String,
    /// Current signed manifest (b64) — may be ahead of what the peer acked.
    pub manifest: String,
    pub version: u64,
    /// Last mutually-acked signed manifest (b64); `None` until the first
    /// `BRIDGE ACCEPT`. Forwarding reads its channel snapshot.
    pub acked_manifest: Option<String>,
    /// Torn down by `BRIDGE SEVER` or a NETBLOCK — kept for audit, never
    /// forwarded from.
    pub severed: bool,
    pub created_ms: u64,
    pub updated_ms: u64,
}

/// A moderation deny (§6.7): a mute or ban on `account` at `scope`
/// (`#chan|ns:<name>|*`). A mute denies `send`; a ban also denies `JOIN`.
/// Enforced against the channel's covering scopes (channel, its namespace,
/// `*`), so a `*`-scope record is a network-wide (global-moderator) action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModRecord {
    pub scope: String,
    pub account: Account,
    pub kind: ModKind,
    /// The moderator who set it.
    pub actor: String,
    pub reason: Option<String>,
    pub at_ms: u64,
}

/// The two persistent moderation states (kick is transient — no record).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModKind {
    Mute,
    Ban,
}

impl ModKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ModKind::Mute => "mute",
            ModKind::Ban => "ban",
        }
    }
}

/// An operator blocklist entry (§11.6): `{network, private reason, added,
/// actor}`. **Name-keyed** — the block is on the network *name*, so key
/// rotation never evades it (invariant 7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetblockRecord {
    pub network: NetworkName,
    /// Operator-private reason; surfaced per `blocklist_visibility` config.
    pub reason: Option<String>,
    pub added_ms: u64,
    /// The account (operator or `netblock`-cap holder) who added it.
    pub actor: String,
}

/// Result of an atomic redeem attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedeemOutcome {
    /// Redeemed; carries the scope + caps to grant the redeemer.
    Redeemed(InviteRecord),
    /// Counter exhausted.
    Exhausted,
    /// No such invite, revoked, or expired — one indistinct outcome so the
    /// session answers NO-SUCH-TARGET uniformly (§2.2).
    Gone,
}
