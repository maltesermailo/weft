//! # weft-matrix — the WEFT↔Matrix bridge daemon
//!
//! Adapter #1 of the Foreign-Realm Bridging Framework: an appservice to a
//! companion homeserver on one side, a `weft-appservice` provider session on
//! the other. `docs/architecture/matrix.md` is the design;
//! `docs/protocol/bridge-session-protocol.md` the WEFT-side wire contract.
//!
//! MVP scope (plan slice 10, inbound-consume first): provisioning (resolve +
//! join + enumerate a space), structure assertion, bidirectional message sync,
//! membership statements. Moderation/power-level mapping is slice 11; media,
//! typing and DMs are deferred with the rest of v2.

#![forbid(unsafe_code)]

pub mod actions;
pub mod asapi;
pub mod bridge;
pub mod config;
pub mod hs;
pub mod ident;
pub mod levels;
pub mod media;
pub mod store;
