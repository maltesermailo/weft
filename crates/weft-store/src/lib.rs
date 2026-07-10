//! # weft-store — WEFT storage (L1)
//!
//! The persistence boundary: [`EventStore`] / [`AccountStore`] traits, the
//! in-memory backend (tests + `ephemeral`-leaning deployments), and the
//! §12.1 **materialization** — the pure function that turns event-sourced
//! rows into the compacted wire form (final bodies + `edited=` counts,
//! per-emoji `REACTIONS` summaries, tombstones — never `EDITED` chains).
//!
//! Layering (CLAUDE.md): depends on weft-proto only. Crypto stays out —
//! password hashes cross this boundary as opaque PHC strings, device keys
//! as raw bytes. The PostgreSQL backend lands in M3b behind these traits.

#![forbid(unsafe_code)]

mod blob;
mod compact;
mod materialize;
mod memory;
#[cfg(feature = "postgres")]
mod postgres;
mod traits;
mod types;

pub use blob::{
    blob_hash, blob_store_contract, BlobHash, BlobMeta, BlobRecord, BlobStore, MemBlobStore,
};
pub use compact::compaction_plan;
pub use materialize::{materialize, HistoryItem, ReactionSummary, MAX_REACTION_ACTORS};
pub use memory::MemoryStore;
#[cfg(feature = "postgres")]
pub use postgres::PgStore;
pub use traits::{
    AccountStore, CapabilityStore, ChannelStore, EventStore, InviteStore, MediaStore,
    MembershipStore, ModerationStore, NamespaceStore, NetblockStore, PeerStore, PinStore,
    ReportStore, RoleStore, HOLD_RADIUS,
};
pub use types::{
    ChannelRecord, EventKind, EventRecord, GrantRecord, InviteRecord, ModKind, ModRecord,
    NamespaceRecord, NetblockRecord, Page, PeerRecord, PendingRecovery, RedeemOutcome,
    ReportRecord, ReportResolution, RoleDef, RootHistoryEntry, Scope, Verification,
};

use thiserror::Error;

/// Storage failures. The session layer maps these to `ERR INTERNAL`
/// (§8: leaks nothing).
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("storage backend: {0}")]
    Backend(String),
}
