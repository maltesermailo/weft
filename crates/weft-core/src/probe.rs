//! The "who owns this domain, and what did they choose to run there?" seam.
//!
//! A **realm is a network** (foreign-bridge framework §7a.0), which is what lets
//! a bridged space behave like federation — but it also means a realm name lands
//! in the same identity space as real WEFT networks: a realm called
//! `hda.example` mints `alice@hda.example`, indistinguishable from that
//! network's own user.
//!
//! The arbiter is the domain owner, not our local bookkeeping: whoever controls
//! `hda.example` chooses whether it runs a WEFT server or something a bridge
//! reaches. A domain that publishes `/.well-known/weft` has chosen WEFT, so no
//! bridge may claim it as a realm.
//!
//! weft-core must not do socket I/O (L2), so it asks an installed probe; the
//! real one lives in weftd (L3) on the same SSRF-guarded fetch auto-federation
//! uses (invariant 13), and a stub drives the core tests.

use async_trait::async_trait;

#[async_trait]
pub trait NetworkProbe: Send + Sync {
    /// Does `host` publish a WEFT `/.well-known/weft` — i.e. did its owner
    /// choose to run a WEFT network there?
    ///
    /// **Only a positive answer is actionable.** Anything else — no well-known,
    /// NXDOMAIN, a connection failure, or a realm that is not a domain at all
    /// (a Discord guild id) — is *inconclusive* and must read as `false`, or a
    /// transient DNS blip would lock out every legitimate bridge and no
    /// non-DNS realm could ever bind.
    async fn is_weft_network(&self, host: &str) -> bool;
}
