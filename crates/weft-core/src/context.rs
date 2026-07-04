//! Shared server context handed to every session.

use std::sync::atomic::{AtomicU64, Ordering};

use weft_crypto::{Attestation, Keypair, PublicKey};
use weft_proto::{Account, ChannelName, NetworkName};

use crate::accounts::Accounts;
use crate::registry::Registry;

/// The only protocol version this server speaks (§3.6).
pub const PROTOCOL_VERSION: &str = "weft/1";

/// Attestation lifetime (§10.2: rotation = superseding attestation, so
/// lifetimes stay short-ish; re-auth refreshes well before expiry).
const ATTESTATION_TTL_SECS: u64 = 30 * 24 * 3600;

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

/// Everything a session needs: identity, accounts, channel registry.
pub struct ServerCtx {
    pub info: ServerInfo,
    pub registry: Registry,
    pub accounts: Accounts,
    /// §6.1: `registration: open` — closed networks answer REGISTER with
    /// FORBIDDEN.
    pub registration_open: bool,
    /// The network signing key (§10.2): signs device attestations; its
    /// public half is published at `/.well-known/weft`.
    identity: Keypair,
    next_session: AtomicU64,
}

impl ServerCtx {
    /// Spawns one actor per configured channel (the channel set is static
    /// until CHANNEL CREATE in M4 — JOIN never auto-creates, §6.3).
    pub fn new(
        info: ServerInfo,
        channels: impl IntoIterator<Item = ChannelName>,
        identity: Keypair,
        registration_open: bool,
    ) -> Self {
        let registry = Registry::spawn(channels, info.network.clone());
        Self {
            info,
            registry,
            accounts: Accounts::default(),
            registration_open,
            identity,
            next_session: AtomicU64::new(1),
        }
    }

    /// The public signing key, for `/.well-known/weft` (§10.2).
    pub fn identity_public(&self) -> PublicKey {
        self.identity.public()
    }

    /// Issue a device attestation for a just-verified session (§6.1, §10.2).
    pub(crate) fn mint_attestation(
        &self,
        account: &Account,
        device: PublicKey,
        now: u64,
    ) -> Attestation {
        Attestation::sign(
            &self.identity,
            device,
            account.as_str(),
            self.info.network.as_str(),
            now + ATTESTATION_TTL_SECS,
        )
    }

    pub(crate) fn network_name(&self) -> &str {
        self.info.network.as_str()
    }

    pub(crate) fn next_session_id(&self) -> u64 {
        self.next_session.fetch_add(1, Ordering::Relaxed)
    }
}
