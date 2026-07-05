//! The storage ports. `async_trait` (boxed futures) so backends live
//! behind `Arc<dyn …>` — weft-core stays non-generic over storage.

use async_trait::async_trait;
use weft_proto::{Account, ChannelName, MsgId, ReportStatus, RetentionPolicy, Ulid};

use weft_proto::NamespaceName;

use weft_proto::NetworkName;

use crate::types::{
    ChannelRecord, EventRecord, GrantRecord, InviteRecord, NamespaceRecord, NetblockRecord, Page,
    PeerRecord, PendingRecovery, RedeemOutcome, ReportRecord, ReportResolution, RootHistoryEntry,
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

    /// Locate a root by its ULID across scopes — EDIT/DELETE/REACT arrive
    /// with only a msgid (§6.4).
    async fn find_root(&self, ulid: Ulid) -> Result<Option<EventRecord>, StoreError>;

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

    /// Idempotent; false iff the account is unknown.
    async fn enroll_device(&self, account: &Account, device: [u8; 32]) -> Result<bool, StoreError>;

    async fn device_enrolled(
        &self,
        account: &Account,
        device: &[u8; 32],
    ) -> Result<bool, StoreError>;

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
    /// Insert or update a channel's policy (leaves topic/view-gated intact).
    async fn upsert_channel(
        &self,
        name: &ChannelName,
        policy: RetentionPolicy,
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

    /// CHANNEL DELETE. False = no such channel.
    async fn delete_channel(&self, name: &ChannelName) -> Result<bool, StoreError>;

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
