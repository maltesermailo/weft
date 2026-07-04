//! Shared server context handed to every session.

use std::sync::atomic::{AtomicU64, Ordering};

use weft_proto::{ChannelName, NetworkName};

use crate::registry::Registry;

/// The only protocol version this server speaks (§3.6).
pub const PROTOCOL_VERSION: &str = "weft/1";

/// Static identity/config of this network, from weftd's config file.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub network: NetworkName,
    /// WELCOME trailing text (§3.6).
    pub motd: Option<String>,
    /// `features=` flags for WELCOME. Empty in M1 — media/voice/e2ee/
    /// backfill/presence all land in later milestones.
    pub features: Vec<String>,
}

/// Everything a session needs: identity, channel registry, ID source.
#[derive(Debug)]
pub struct ServerCtx {
    pub info: ServerInfo,
    pub registry: Registry,
    next_session: AtomicU64,
}

impl ServerCtx {
    /// Spawns one actor per configured channel (M1: the channel set is
    /// static — CHANNEL CREATE is M4 and JOIN never auto-creates, §6.3).
    pub fn new(info: ServerInfo, channels: impl IntoIterator<Item = ChannelName>) -> Self {
        let registry = Registry::spawn(channels, info.network.clone());
        Self {
            info,
            registry,
            next_session: AtomicU64::new(1),
        }
    }

    pub(crate) fn next_session_id(&self) -> u64 {
        self.next_session.fetch_add(1, Ordering::Relaxed)
    }
}
