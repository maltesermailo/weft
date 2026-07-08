//! # weft-core — WEFT domain logic (L2)
//!
//! Sessions, channel actors, and the registry. This crate never touches a
//! socket: transports hand it lines through the [`ControlStream`] trait, so
//! the whole layer is testable with an in-memory mock (see `tests/`).
//!
//! Concurrency model (architecture doc §3): task-per-connection
//! ([`run_session`]) + actor-per-channel; the actor's inbox order is the
//! channel's total order and the only place msgids are minted.

#![forbid(unsafe_code)]

mod accounts;
mod bridge;
mod channel;
mod context;
mod directory;
mod maintenance;
mod registry;
mod session;
mod stream;

pub use accounts::Accounts;
pub use channel::{ChannelEvent, ChannelHandle, JoinAck};
pub use context::{AutoBridgeRequest, FederationConfig, ServerCtx, ServerInfo, PROTOCOL_VERSION};
pub use maintenance::{apply_due_recoveries, spawn_maintenance, MaintenanceConfig};
pub use registry::Registry;
pub use session::{run_bridge_client, run_bridge_requester, run_session, SessionId};
pub use stream::ControlStream;
pub use weft_store::{ChannelStore, MemoryStore, NamespaceStore, Scope};

// Consumers construct a ServerCtx with a signing key and verify attestations
// against it — re-exported so they share one weft-crypto version.
pub use weft_crypto::{Attestation, Keypair, Manifest, PublicKey, SignedManifest};
