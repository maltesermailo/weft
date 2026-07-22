//! The storage ports. `async_trait` (boxed futures) so backends live
//! behind `Arc<dyn …>` — weft-core stays non-generic over storage.

use async_trait::async_trait;
use weft_proto::{Account, ChannelName, MsgId, ReportStatus, RetentionPolicy, Ulid};

use weft_proto::NamespaceName;

use weft_proto::NetworkName;

use crate::types::{
    AuditEntry, AuditRecord, ChannelRecord, EventRecord, GrantRecord, InviteRecord,
    MediaBlockRecord, ModKind, ModRecord, NamespaceRecord, NetblockRecord, Page, PeerRecord,
    PendingRecovery, RedeemOutcome, ReportRecord, ReportResolution, RoleDef, RootHistoryEntry,
    Scope, Verification,
};
use crate::StoreError;

#[async_trait]
pub trait EventStore: Send + Sync {
    async fn append(&self, record: EventRecord) -> Result<(), StoreError>;

    /// Root (`Message`) rows in the page window, ascending msgid order.
    async fn roots(&self, scope: &Scope, page: Page) -> Result<Vec<EventRecord>, StoreError>;

    /// All child rows (edits/deletes/reactions) belonging to `roots`.
    async fn children(&self, scope: &Scope, roots: &[Ulid])
        -> Result<Vec<EventRecord>, StoreError>;

    /// Server-computed unread tally for `account` in `scope` since the read
    /// marker `since` (§6.3): `(unread, mentions)`, where `unread` counts root
    /// messages from *other* senders with a ULID strictly after `since`, and
    /// `mentions` is the subset whose body references the account (`@account`)
    /// or `@everyone`/`@here`. Own messages never count as unread.
    async fn unread_counts(
        &self,
        scope: &Scope,
        account: &Account,
        since: Ulid,
    ) -> Result<(u64, u64), StoreError>;

    /// Locate a root by its ULID across scopes — EDIT/DELETE/REACT arrive
    /// with only a msgid (§6.4).
    async fn find_root(&self, ulid: Ulid) -> Result<Option<EventRecord>, StoreError>;

    /// Message search within a scope (§6.4): non-system root messages whose
    /// body contains `query` (case-insensitive substring), newest-first, capped
    /// at `limit`. Deleted (tombstoned) roots are excluded.
    async fn search(
        &self,
        scope: &Scope,
        query: &str,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError>;

    /// A thread's root messages (§9.4): the root itself plus every message
    /// tagged `thread=<root>`, oldest-first, capped at `limit`. Children
    /// (edits/reactions) are fetched separately via [`EventStore::children`].
    async fn thread_roots(
        &self,
        scope: &Scope,
        root: &MsgId,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError>;

    /// Root (`Message`) rows authored by `sender` (the `account@network` form),
    /// across every scope, newest-first. The operator admin surface only — the
    /// wire protocol is channel-scoped and never queries by author.
    async fn messages_by_sender(
        &self,
        sender: &str,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError>;

    /// Whether a root already carries a tombstone.
    async fn is_deleted(&self, scope: &Scope, root: Ulid) -> Result<bool, StoreError>;

    /// Drop whole messages (root + children, tombstones included) whose
    /// root is older than `cutoff_ms`, and advance the truncation
    /// watermark. Returns the number of roots purged.
    async fn purge_before(&self, scope: &Scope, cutoff_ms: u64) -> Result<u64, StoreError>;

    /// Millisecond watermark below which data may have been purged —
    /// HISTORY answers `truncated` from this; silence about gaps is
    /// forbidden (§6.4).
    async fn purged_before(&self, scope: &Scope) -> Result<Option<u64>, StoreError>;

    /// Purge across all DM scopes (one uniform network-config policy,
    /// §9.5). Returns roots purged.
    async fn purge_dms_before(&self, cutoff_ms: u64) -> Result<u64, StoreError>;

    /// §12.1 compaction across all scopes: apply [`crate::compaction_plan`]
    /// to every message family whose stale rows have left the audit
    /// window. Returns rows dropped.
    async fn compact_before(&self, cutoff_ms: u64) -> Result<u64, StoreError>;
}

#[async_trait]
pub trait AccountStore: Send + Sync {
    /// False = name taken (§6.1 CONFLICT).
    async fn register(&self, account: &Account, password_phc: &str) -> Result<bool, StoreError>;

    async fn password_phc(&self, account: &Account) -> Result<Option<String>, StoreError>;

    /// The account's immutable **ULID** (minted at register) — the stable
    /// capability-subject key, independent of the mutable handle. `None` if the
    /// account is unknown. Backends backfill a ULID for any pre-existing
    /// account on first read.
    async fn account_ulid(&self, account: &Account) -> Result<Option<String>, StoreError>;

    /// Every registered account, sorted by name. The operator admin surface —
    /// the wire protocol never enumerates accounts network-wide.
    async fn list_accounts(&self) -> Result<Vec<Account>, StoreError>;

    /// Operator hard-delete of an account: drop the account record **and** its
    /// per-account data (memberships, capability grants keyed by its ULID,
    /// moderation records targeting it, marks/devices/verifications). Its posted
    /// **messages are preserved** (attributed) — content moderation is a
    /// separate action (delete message / block hash). Returns false if unknown.
    async fn delete_account(&self, account: &Account) -> Result<bool, StoreError>;

    /// WC3 soft delete: schedule the account to be hard-deleted at `purge_at_ms`
    /// (a grace window during which it's recoverable via [`cancel_deletion`]).
    /// Idempotent — reschedules if already pending. False iff the account is
    /// unknown. The account keeps working until the maintenance pass finalizes.
    ///
    /// [`cancel_deletion`]: AccountStore::cancel_deletion
    async fn schedule_deletion(
        &self,
        account: &Account,
        purge_at_ms: u64,
    ) -> Result<bool, StoreError>;

    /// Cancel a scheduled deletion (restore). False iff not currently scheduled.
    async fn cancel_deletion(&self, account: &Account) -> Result<bool, StoreError>;

    /// The scheduled purge time (ms) if the account is pending deletion, else
    /// `None`. `Ok(None)` also covers an unknown account.
    async fn deletion_scheduled(&self, account: &Account) -> Result<Option<u64>, StoreError>;

    /// Accounts whose scheduled purge time is at/before `now_ms` — the
    /// maintenance finalize list.
    async fn due_deletions(&self, now_ms: u64) -> Result<Vec<Account>, StoreError>;

    /// WC7 moderation: suspend/unsuspend an account. A suspended account can't
    /// authenticate (uniform `AUTH-FAILED`) — its tokens are effectively frozen
    /// because it can't open a session to exercise them. Idempotent; false iff
    /// the account is unknown.
    async fn set_suspended(&self, account: &Account, suspended: bool) -> Result<bool, StoreError>;

    /// Whether the account is currently suspended (`false` also covers unknown).
    async fn is_suspended(&self, account: &Account) -> Result<bool, StoreError>;

    /// Set/clear an account's operator flag (§10.4). An operator holds every
    /// capability at every scope. Idempotent; false iff the account is unknown.
    async fn set_operator(&self, account: &Account, operator: bool) -> Result<bool, StoreError>;

    /// Whether the account holds operator authority (`false` also covers unknown).
    async fn is_operator(&self, account: &Account) -> Result<bool, StoreError>;

    /// Every operator account, name-sorted.
    async fn list_operators(&self) -> Result<Vec<Account>, StoreError>;

    /// Idempotent; false iff the account is unknown.
    async fn enroll_device(&self, account: &Account, device: [u8; 32]) -> Result<bool, StoreError>;

    async fn device_enrolled(
        &self,
        account: &Account,
        device: &[u8; 32],
    ) -> Result<bool, StoreError>;

    /// Every enrolled device pubkey (Ed25519) for the account — the operator
    /// device-list view (WC4). Empty if none / unknown account.
    async fn devices(&self, account: &Account) -> Result<Vec<[u8; 32]>, StoreError>;

    /// Accounts holding an email verification claim in `domain` (the part after
    /// `@`, case-insensitive) — the "find related" spam-wave pivot (WC4, §2).
    /// Both verified and pending claims count. Sorted by name.
    async fn accounts_by_email_domain(&self, domain: &str) -> Result<Vec<Account>, StoreError>;

    /// §6.3 MARK: account-scoped read marker per target; survives
    /// `ephemeral` (markers are account data, not channel data).
    async fn set_mark(
        &self,
        account: &Account,
        target: &str,
        msgid: &MsgId,
    ) -> Result<(), StoreError>;

    /// All markers for an account — the §9.7 reconnect snapshot.
    async fn marks(&self, account: &Account) -> Result<Vec<(String, MsgId)>, StoreError>;

    /// Record (or replace) a verification claim; starts unverified.
    /// One claim per kind per account.
    async fn upsert_verification(
        &self,
        account: &Account,
        kind: &str,
        subject: &str,
    ) -> Result<(), StoreError>;

    /// Confirm a pending claim. False = no such claim.
    async fn confirm_verification(
        &self,
        account: &Account,
        kind: &str,
        verified_at: u64,
    ) -> Result<bool, StoreError>;

    async fn verifications(&self, account: &Account) -> Result<Vec<Verification>, StoreError>;
}

/// The channel set lives in the store — config entries are *seeded* into
/// it at boot and the store is the source of truth from then on. CHANNEL
/// CREATE/DELETE/META (§6.3) write through here.
#[async_trait]
pub trait ChannelStore: Send + Sync {
    /// Insert or update a channel's policy + kind (leaves topic/view-gated
    /// intact). `kind` is set on first insert and not changed by later upserts
    /// (§16 kind is immutable after creation).
    async fn upsert_channel(
        &self,
        name: &ChannelName,
        policy: RetentionPolicy,
        kind: weft_proto::ChannelKind,
    ) -> Result<(), StoreError>;

    async fn list_channels(&self) -> Result<Vec<(ChannelName, RetentionPolicy)>, StoreError>;

    /// Full settings for one channel.
    async fn channel(&self, name: &ChannelName) -> Result<Option<ChannelRecord>, StoreError>;

    /// CHANNEL META topic (§6.3).
    async fn set_channel_topic(&self, name: &ChannelName, topic: &str) -> Result<(), StoreError>;

    /// CHANNEL META view-gated (§6.3) — flips the anti-enumeration branch.
    async fn set_channel_view_gated(
        &self,
        name: &ChannelName,
        gated: bool,
    ) -> Result<(), StoreError>;

    /// CHANNEL META posting (§6.7) — `restricted` requires the `send` cap.
    async fn set_channel_restricted(
        &self,
        name: &ChannelName,
        restricted: bool,
    ) -> Result<(), StoreError>;

    /// CHANNEL DELETE. False = no such channel.
    async fn delete_channel(&self, name: &ChannelName) -> Result<bool, StoreError>;

    /// CHANNEL RENAME — re-key EVERYTHING scoped to the channel name (§6.3):
    /// the channel record, history/events, capability grants + scope epochs,
    /// moderation, pins, memberships, roles + assignments, and retention-hold
    /// scoping. Atomic (single transaction on durable backends). Returns
    /// `Ok(false)` if `old` is absent or `new` already exists.
    async fn rename_channel(
        &self,
        old: &ChannelName,
        new: &ChannelName,
    ) -> Result<bool, StoreError>;

    /// Set a channel's layout within its namespace (category + position) —
    /// the Discord-style ordering (spec extension).
    async fn set_channel_layout(
        &self,
        name: &ChannelName,
        category: Option<&str>,
        position: i64,
    ) -> Result<(), StoreError>;

    /// Channels in a namespace, ordered by (category, position, name) — the
    /// layout the client renders. `namespace` matches the `#ns/…` prefix.
    async fn channels_in_namespace(
        &self,
        namespace: &str,
    ) -> Result<Vec<(ChannelName, ChannelRecord)>, StoreError>;
}

/// Capability grants + per-scope revocation epochs (§6.5, §10.4). The
/// server-side grant table is the enforcement fast path for same-network
/// authed accounts.
#[async_trait]
pub trait CapabilityStore: Send + Sync {
    /// Record a granted capability (GRANT). Replaces any existing grant for
    /// the same (subject, scope) — re-granting re-mints (§10.4).
    async fn record_grant(
        &self,
        subject: &str,
        scope: &str,
        caps: &[String],
        epoch: u64,
        expiry: Option<u64>,
    ) -> Result<(), StoreError>;

    /// All grants held by a subject (account or pubkey).
    async fn grants_for(&self, subject: &str) -> Result<Vec<GrantRecord>, StoreError>;

    /// Every grant recorded *at* a scope, across subjects. Used to find who
    /// holds a role (§6.5) so a channel role-permission can propagate to them.
    async fn grants_at_scope(&self, scope: &str) -> Result<Vec<GrantRecord>, StoreError>;

    /// REVOKE: drop grants for (subject, scope); `caps = None` drops all.
    /// Returns the number removed.
    async fn revoke_grants(
        &self,
        subject: &str,
        scope: &str,
        caps: Option<&[String]>,
    ) -> Result<u64, StoreError>;

    /// The current revocation epoch for a scope (0 if never bumped).
    async fn scope_epoch(&self, scope: &str) -> Result<u64, StoreError>;

    /// Bump and return the new epoch — invalidates every grant/token at the
    /// scope issued before it (§10.4).
    async fn bump_epoch(&self, scope: &str) -> Result<u64, StoreError>;
}

/// Invite lifecycle (§6.5).
#[async_trait]
pub trait InviteStore: Send + Sync {
    async fn create_invite(&self, invite: InviteRecord) -> Result<(), StoreError>;

    async fn invite(&self, id: &str) -> Result<Option<InviteRecord>, StoreError>;

    /// Atomically decrement the counter and check expiry. The single
    /// mutating check keeps concurrent redeems from over-drawing a
    /// limited-use invite.
    async fn redeem_invite(&self, id: &str, now: u64) -> Result<RedeemOutcome, StoreError>;

    /// INVITE REVOKE — closes the counter. False = no such invite.
    async fn revoke_invite(&self, id: &str) -> Result<bool, StoreError>;
}

/// User-owned namespaces (§2.1, §2.2).
#[async_trait]
pub trait NamespaceStore: Send + Sync {
    /// Create a namespace. False = name already taken (§6.2 CONFLICT).
    async fn create_namespace(&self, record: NamespaceRecord) -> Result<bool, StoreError>;

    async fn namespace(&self, name: &NamespaceName) -> Result<Option<NamespaceRecord>, StoreError>;

    /// Namespaces owned by an account (for quota enforcement, §2.2).
    async fn namespaces_owned(&self, owner: &str) -> Result<u64, StoreError>;

    /// The `public` directory, sorted by name, for DISCOVER (§6.2). `after`
    /// is the exclusive cursor (a namespace name); `limit` caps the page.
    async fn list_public(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<NamespaceRecord>, StoreError>;

    /// NS META (title/description/icon) — `key` is one of those.
    async fn set_namespace_meta(
        &self,
        name: &NamespaceName,
        key: &str,
        value: &str,
    ) -> Result<(), StoreError>;

    /// NS VISIBILITY.
    async fn set_namespace_visibility(
        &self,
        name: &NamespaceName,
        visibility: &str,
    ) -> Result<(), StoreError>;

    /// §11.10 NS META federation: toggle auto-federation reachability. No-op if
    /// the namespace is unknown.
    async fn set_namespace_federation(
        &self,
        name: &NamespaceName,
        open: bool,
    ) -> Result<(), StoreError>;

    /// NS DELETE. False = no such namespace.
    async fn delete_namespace(&self, name: &NamespaceName) -> Result<bool, StoreError>;

    // ---- §2.4 recovery ladder ----

    /// Change ownership (NS TRANSFER, rung 1) and/or rotate the root key
    /// (recovery application). Appends a `root-history` entry.
    async fn rotate_root(
        &self,
        name: &NamespaceName,
        new_owner: &str,
        new_root_key: &str,
        operator_initiated: bool,
        at_ms: u64,
    ) -> Result<(), StoreError>;

    /// NS RECOVERY SET — designate the M-of-N quorum.
    async fn set_recovery_set(
        &self,
        name: &NamespaceName,
        m: u32,
        keys: &[String],
    ) -> Result<(), StoreError>;

    /// Begin a recovery's delay window (NS RECOVER).
    async fn set_pending_recovery(
        &self,
        name: &NamespaceName,
        pending: PendingRecovery,
    ) -> Result<(), StoreError>;

    /// Clear a pending recovery (NS RECOVERY CANCEL, or after it applies).
    async fn clear_pending_recovery(&self, name: &NamespaceName) -> Result<(), StoreError>;

    /// Every namespace with a pending recovery whose `eta_ms <= now_ms` —
    /// the ones the scheduler must apply.
    async fn due_recoveries(&self, now_ms: u64) -> Result<Vec<NamespaceRecord>, StoreError>;

    async fn root_history(&self, name: &NamespaceName)
        -> Result<Vec<RootHistoryEntry>, StoreError>;
}

/// Report queue + retention holds (§6.7, §12.1, invariant 11). Holds live
/// alongside events so `EventStore::purge_before` / `compact_before` skip
/// held roots automatically — the two traits share a backend.
#[async_trait]
pub trait ReportStore: Send + Sync {
    /// File a report. When `record.state == Verified`, places retention
    /// holds on the reported root plus its context (±[`HOLD_RADIUS`]) so
    /// purge and compaction skip them until resolution + grace.
    async fn file_report(&self, record: ReportRecord) -> Result<(), StoreError>;

    async fn report(&self, id: &str) -> Result<Option<ReportRecord>, StoreError>;

    /// Reports listable at `scope` (present in `queue_scopes`), optionally
    /// filtered by status, newest first. `after` is an exclusive report-id
    /// cursor; `limit` caps the page.
    async fn list_reports(
        &self,
        scope: &str,
        status: Option<ReportStatus>,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ReportRecord>, StoreError>;

    /// Resolve an open report; schedules hold release after the grace
    /// window. False = no such open report.
    async fn resolve_report(
        &self,
        id: &str,
        resolution: ReportResolution,
    ) -> Result<bool, StoreError>;

    /// ESCALATE (§6.7): add `*` (net) to an ns-scope report's queues, leaving
    /// it open and its holds intact. False = no such open report.
    async fn escalate_report(&self, id: &str) -> Result<bool, StoreError>;

    /// Reports filed by `reporter` at or after `since_ms` — the THROTTLED
    /// rate-limit check (§6.7).
    async fn reports_by_since(&self, reporter: &Account, since_ms: u64) -> Result<u64, StoreError>;

    /// Release holds for reports resolved past their grace window (§12.1).
    /// Returns the number of reports whose holds were released. Idempotent —
    /// the maintenance scheduler calls it each tick.
    async fn release_due_holds(&self, now_ms: u64) -> Result<u64, StoreError>;
}

/// §12.1 recommended context radius: a verified report holds the reported
/// message plus this many roots on each side.
pub const HOLD_RADIUS: usize = 25;

/// Bridge peerings + their signed manifests (§11.1). One record per peer
/// network; the store keeps the manifest blobs opaque (weft-core signs and
/// verifies them). Forwarding is gated on the acked-vs-current channel
/// intersection (invariant 3), computed in core from these blobs.
#[async_trait]
pub trait PeerStore: Send + Sync {
    /// Insert or replace a peering (PROPOSE/ADD/REMOVE bump `manifest`;
    /// ACCEPT sets `acked_manifest`; SEVER sets `severed`).
    async fn upsert_peer(&self, record: PeerRecord) -> Result<(), StoreError>;

    async fn peer(&self, peer: &NetworkName) -> Result<Option<PeerRecord>, StoreError>;

    async fn list_peers(&self) -> Result<Vec<PeerRecord>, StoreError>;

    /// Hard-remove a peering. False = no such peer.
    async fn remove_peer(&self, peer: &NetworkName) -> Result<bool, StoreError>;
}

/// Moderation deny-list (§6.7): mutes + bans keyed by `(scope, account)`.
#[async_trait]
pub trait ModerationStore: Send + Sync {
    /// Set (or refresh) a mute/ban. Replaces any existing record for the same
    /// `(scope, account, kind)`.
    async fn set_moderation(&self, record: ModRecord) -> Result<(), StoreError>;

    /// Clear a mute/ban. False = there was none.
    async fn clear_moderation(
        &self,
        scope: &str,
        account: &Account,
        kind: ModKind,
    ) -> Result<bool, StoreError>;

    /// Is `account` under a record of `kind` at any of `scopes` (the covering
    /// set: channel, its namespace, `*`)?
    async fn is_moderated(
        &self,
        account: &Account,
        scopes: &[String],
        kind: ModKind,
    ) -> Result<bool, StoreError>;

    /// All active records at a scope (for a moderation list).
    async fn list_moderation(&self, scope: &str) -> Result<Vec<ModRecord>, StoreError>;
}

/// Operator network blocklist (§11.6). Name-keyed (invariant 7).
#[async_trait]
pub trait NetblockStore: Send + Sync {
    /// NETBLOCK ADD — idempotent; re-adding refreshes reason/actor/time.
    async fn add_netblock(&self, record: NetblockRecord) -> Result<(), StoreError>;

    /// NETBLOCK REMOVE. False = the network wasn't blocked.
    async fn remove_netblock(&self, network: &NetworkName) -> Result<bool, StoreError>;

    async fn is_netblocked(&self, network: &NetworkName) -> Result<bool, StoreError>;

    async fn list_netblocks(&self) -> Result<Vec<NetblockRecord>, StoreError>;
}

/// §13 media hash blocklist. Content-addressed, so a block is network-wide and
/// re-uploads of the same bytes are dead on arrival.
#[async_trait]
pub trait MediaBlocklistStore: Send + Sync {
    /// MEDIA BLOCK — idempotent; re-blocking refreshes reason/actor/time.
    async fn block_hash(&self, record: MediaBlockRecord) -> Result<(), StoreError>;

    /// MEDIA UNBLOCK. False = the hash wasn't blocked.
    async fn unblock_hash(&self, hash: &str) -> Result<bool, StoreError>;

    async fn is_hash_blocked(&self, hash: &str) -> Result<bool, StoreError>;

    async fn list_blocked_hashes(&self) -> Result<Vec<MediaBlockRecord>, StoreError>;
}

/// WC1 admin audit trail. Append-only + hash-chained (tamper-evident): the
/// panel ships to strangers, so "prove afterward who did what" matters more than
/// for a personal tool. `append_audit` is the single writer of the chain — it
/// computes `seq`/`prev_hash`/`hash` atomically, exactly as the channel actor is
/// the single writer of ULID order.
#[async_trait]
pub trait AuditStore: Send + Sync {
    /// Append one event, linking it to the tail of the chain. Returns the
    /// committed record (with its computed `seq`/`prev_hash`/`hash`).
    async fn append_audit(&self, entry: AuditEntry) -> Result<AuditRecord, StoreError>;

    /// The log, newest-first, optionally filtered by exact operator and/or
    /// action. Bounded by `limit`.
    async fn list_audit(
        &self,
        operator: Option<&str>,
        action: Option<&str>,
        limit: usize,
    ) -> Result<Vec<AuditRecord>, StoreError>;
}

/// §6.4 pinned messages, per channel. Pins are a small set keyed by channel;
/// the message content is fetched from the [`EventStore`] on demand.
#[async_trait]
pub trait PinStore: Send + Sync {
    /// Pin or unpin a message. Idempotent.
    async fn set_pin(
        &self,
        channel: &ChannelName,
        msgid: &MsgId,
        pinned: bool,
    ) -> Result<(), StoreError>;

    /// The pinned msgids for a channel, oldest-first.
    async fn pins(&self, channel: &ChannelName) -> Result<Vec<MsgId>, StoreError>;
}

/// §9.4 custom (per-namespace) emoji: a shortcode `name` → a `weft-media://…`
/// reference. The image bytes live in the blob store; this only maps names.
#[async_trait]
pub trait EmojiStore: Send + Sync {
    /// Add or replace a namespace emoji (idempotent by `(namespace, name)`).
    async fn set_emoji(
        &self,
        namespace: &NamespaceName,
        name: &str,
        media: &str,
    ) -> Result<(), StoreError>;

    /// Remove a namespace emoji. Returns false iff it didn't exist.
    async fn remove_emoji(
        &self,
        namespace: &NamespaceName,
        name: &str,
    ) -> Result<bool, StoreError>;

    /// All emoji for a namespace as `(name, media)`, name-sorted.
    async fn list_emoji(
        &self,
        namespace: &NamespaceName,
    ) -> Result<Vec<(String, String)>, StoreError>;

    /// Every media reference used by a custom emoji across all namespaces — so
    /// the orphan-blob GC keeps their images (an emoji references its blob here,
    /// not via message media-refs, §13).
    async fn emoji_media(&self) -> Result<Vec<String>, StoreError>;
}

/// §6.3 persistent channel membership. Unlike the live channel-actor roster
/// (session-scoped), this survives reconnects: a client is auto-rejoined to
/// these channels on auth, so its tiles reappear (the Discord model).
#[async_trait]
pub trait MembershipStore: Send + Sync {
    /// Record that `account` is a member of `channel`. Idempotent.
    async fn set_membership(
        &self,
        account: &Account,
        channel: &ChannelName,
    ) -> Result<(), StoreError>;

    /// Drop a membership (PART / kick / ban). Idempotent.
    async fn clear_membership(
        &self,
        account: &Account,
        channel: &ChannelName,
    ) -> Result<(), StoreError>;

    /// Every channel `account` is a member of, for auto-rejoin.
    async fn memberships(&self, account: &Account) -> Result<Vec<ChannelName>, StoreError>;

    /// Every account that is a persistent member of `channel` — the roster
    /// (§6.3), including members currently offline (Discord-style member list).
    async fn members(&self, channel: &ChannelName) -> Result<Vec<Account>, StoreError>;
}

/// §6.5 role definitions + assignments. Definitions are named, colored
/// capability-token bundles per scope; assignments are **explicit** (an account
/// holds a role because it was assigned, recorded here — not because its caps
/// happen to match). Enforcement stays token-based (assigning grants the caps);
/// this store is the source of truth for *membership* (who wears which role).
#[async_trait]
pub trait RoleStore: Send + Sync {
    /// Define or replace a role at a scope. Idempotent on `(scope, name)`.
    async fn set_role(
        &self,
        scope: &str,
        name: &str,
        color: &str,
        caps: &[String],
    ) -> Result<(), StoreError>;

    /// Remove a role definition (and every assignment of it). Idempotent.
    async fn delete_role(&self, scope: &str, name: &str) -> Result<(), StoreError>;

    /// All role definitions at a scope.
    async fn roles(&self, scope: &str) -> Result<Vec<RoleDef>, StoreError>;

    /// Record that `subject` (a local name or foreign `account@network`, §10.4)
    /// holds role `name` at `scope`. Idempotent.
    async fn assign_role(&self, scope: &str, name: &str, subject: &str) -> Result<(), StoreError>;

    /// Drop an assignment. Idempotent.
    async fn unassign_role(&self, scope: &str, name: &str, subject: &str)
        -> Result<(), StoreError>;

    /// The role names `subject` holds at `scope` (explicit membership).
    async fn roles_of(&self, scope: &str, subject: &str) -> Result<Vec<String>, StoreError>;

    /// The subjects holding role `name` at `scope` — for propagation (§6.5).
    async fn role_members(&self, scope: &str, name: &str) -> Result<Vec<String>, StoreError>;
}

/// §13 media reference index + orphan tracking (M-media-1). Blob *bytes* live in
/// a [`BlobStore`](crate::BlobStore); this maps which messages (in which scopes)
/// reference which blob hashes, so fetches can be membership-gated and
/// unreferenced blobs GC'd (refcount → message retention). Hashes are the wire
/// hex form.
#[async_trait]
pub trait MediaStore: Send + Sync {
    /// Record a freshly-uploaded blob's metadata (the `created_ms` is also the GC
    /// grace anchor: it is not collected until orphaned *past* the grace window,
    /// so an uploaded-but-not-yet-posted blob survives the gap). Idempotent on
    /// `hash`.
    async fn record_blob(&self, record: crate::BlobRecord) -> Result<(), StoreError>;

    /// A stored blob's metadata (mime, size, dimensions, thumbnail), if known.
    async fn blob_meta(&self, hash: &str) -> Result<Option<crate::BlobRecord>, StoreError>;

    /// Record that `msgid` (in `scope`) references these blob hashes.
    async fn add_refs(
        &self,
        scope: &Scope,
        msgid: &MsgId,
        hashes: &[String],
    ) -> Result<(), StoreError>;

    /// Drop every reference held by `msgid` (explicit DELETE). Idempotent.
    async fn drop_refs(&self, msgid: &MsgId) -> Result<(), StoreError>;

    /// Drop references for messages in `scope` minted before `cutoff_ms`
    /// (retention purge — the msgid's ULID carries its timestamp).
    async fn drop_refs_before(&self, scope: &Scope, cutoff_ms: u64) -> Result<(), StoreError>;

    /// The scopes that currently reference `hash` (for membership gating).
    async fn blob_scopes(&self, hash: &str) -> Result<Vec<Scope>, StoreError>;

    /// Known blobs uploaded before `cutoff_ms` with **zero** live references —
    /// the GC candidates.
    async fn orphans(&self, cutoff_ms: u64) -> Result<Vec<String>, StoreError>;

    /// Forget a blob's tracking row after its bytes are deleted. Idempotent.
    async fn forget_blob(&self, hash: &str) -> Result<(), StoreError>;
}

/// §10.3 display profiles (nick + avatar) keyed by account handle. A local
/// account is keyed by its bare name; a federated user by `account@network`.
#[async_trait]
pub trait ProfileStore: Send + Sync {
    /// Insert or replace `account`'s profile (last-writer-wins). Idempotent.
    async fn set_profile(
        &self,
        account: &str,
        profile: crate::ProfileRecord,
    ) -> Result<(), StoreError>;

    /// One account's profile, if set.
    async fn profile(&self, account: &str) -> Result<Option<crate::ProfileRecord>, StoreError>;

    /// The profiles of several accounts at once (for a roster snapshot); absent
    /// accounts are simply omitted.
    async fn profiles(
        &self,
        accounts: &[String],
    ) -> Result<Vec<(String, crate::ProfileRecord)>, StoreError>;

    /// Is `hash` some account's avatar? Avatar blobs are fetchable by any authed
    /// session (§10.3, semi-public) and exempt from orphan GC while referenced.
    async fn avatar_exists(&self, hash: &str) -> Result<bool, StoreError>;
}
