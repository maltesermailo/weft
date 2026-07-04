//! The storage ports. `async_trait` (boxed futures) so backends live
//! behind `Arc<dyn …>` — weft-core stays non-generic over storage.

use async_trait::async_trait;
use weft_proto::{Account, ChannelName, MsgId, RetentionPolicy, Ulid};

use crate::types::{EventRecord, Page, Scope, Verification};
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
/// it at boot and the store is the source of truth from then on. This is
/// the substrate CHANNEL CREATE (M4) will write through.
#[async_trait]
pub trait ChannelStore: Send + Sync {
    /// Insert or update a channel's policy.
    async fn upsert_channel(
        &self,
        name: &ChannelName,
        policy: RetentionPolicy,
    ) -> Result<(), StoreError>;

    async fn list_channels(&self) -> Result<Vec<(ChannelName, RetentionPolicy)>, StoreError>;
}
