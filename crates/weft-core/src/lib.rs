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

mod channel;
mod context;
mod registry;
mod session;
mod stream;

pub use channel::{ChannelEvent, ChannelHandle, JoinAck};
pub use context::{ServerCtx, ServerInfo, PROTOCOL_VERSION};
pub use registry::Registry;
pub use session::{run_session, SessionId};
pub use stream::ControlStream;
