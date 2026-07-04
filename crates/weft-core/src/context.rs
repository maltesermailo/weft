//! Shared server context handed to every session.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use weft_crypto::{Attestation, Capability, Grant, Keypair, PublicKey, Subject, TokenScope};
use weft_proto::{Account, ChannelName, NamespaceName, NetworkName, RetentionPolicy};
use weft_store::{
    AccountStore, CapabilityStore, ChannelStore, EventStore, InviteStore, NamespaceStore,
    StoreError,
};

use crate::accounts::Accounts;
use crate::directory::Directory;
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

/// Everything a session needs: identity, accounts, channels, events, and
/// the capability layer (§6.5, §10.4).
pub struct ServerCtx {
    pub info: ServerInfo,
    pub registry: Registry,
    pub accounts: Accounts,
    /// Read path for HISTORY; the write path goes through channel actors.
    pub events: Arc<dyn EventStore>,
    /// §9.5: one network-config retention policy for all DMs.
    pub dm_policy: weft_proto::RetentionPolicy,
    /// Account-scoped routing: DMs + MARK sync.
    pub(crate) directory: Directory,
    /// §6.1: `registration: open` — closed networks answer REGISTER with
    /// FORBIDDEN.
    pub registration_open: bool,
    /// Grants + revocation epochs (§10.4).
    pub(crate) caps: Arc<dyn CapabilityStore>,
    /// Invite lifecycle (§6.5).
    pub(crate) invites: Arc<dyn InviteStore>,
    /// Channel settings (topic, view-gated) beyond the running actors.
    pub(crate) channel_store: Arc<dyn ChannelStore>,
    /// User-owned namespaces (§2.1).
    pub(crate) namespaces: Arc<dyn NamespaceStore>,
    /// §2.2 namespace creation: `open` (any account, up to `ns_quota`) or
    /// gated (needs the `ns-create` cap).
    pub(crate) ns_creation_open: bool,
    pub(crate) ns_quota: u64,
    /// Operator accounts: they hold the network key's authority — every
    /// capability at `*` (§11.3). This is how the first admin exists;
    /// everyone else's caps chain from a GRANT.
    operators: HashSet<Account>,
    /// The network signing key (§10.2): signs device attestations AND the
    /// capability tokens rooted at `*`/`#chan` scope (§11.3).
    identity: Keypair,
    next_session: AtomicU64,
}

impl ServerCtx {
    /// Spawns one actor per seeded channel; the registry mutates at runtime
    /// via CHANNEL CREATE/DELETE (§6.3). One store object backs every port.
    #[allow(clippy::too_many_arguments)]
    pub fn new<S>(
        info: ServerInfo,
        channels: impl IntoIterator<Item = (ChannelName, RetentionPolicy)>,
        identity: Keypair,
        registration_open: bool,
        store: Arc<S>,
        dm_policy: RetentionPolicy,
        operators: impl IntoIterator<Item = Account>,
        ns_creation_open: bool,
        ns_quota: u64,
    ) -> Self
    where
        S: EventStore
            + AccountStore
            + CapabilityStore
            + ChannelStore
            + InviteStore
            + NamespaceStore
            + 'static,
    {
        let events: Arc<dyn EventStore> = store.clone();
        let accounts: Arc<dyn AccountStore> = store.clone();
        let caps: Arc<dyn CapabilityStore> = store.clone();
        let invites: Arc<dyn InviteStore> = store.clone();
        let channel_store: Arc<dyn ChannelStore> = store.clone();
        let namespaces: Arc<dyn NamespaceStore> = store;
        let registry = Registry::spawn(channels, info.network.clone(), Arc::clone(&events));
        let directory = crate::directory::spawn(
            info.network.clone(),
            dm_policy,
            Arc::clone(&events),
            Arc::clone(&accounts),
        );
        Self {
            info,
            registry,
            accounts: Accounts::new(accounts),
            events,
            dm_policy,
            directory,
            registration_open,
            caps,
            invites,
            channel_store,
            namespaces,
            ns_creation_open,
            ns_quota,
            operators: operators.into_iter().collect(),
            identity,
            next_session: AtomicU64::new(1),
        }
    }

    // ---- capability enforcement (§10.4, invariant 4) ----

    /// Does `account` hold `cap` for an object at `scope`? Operators hold
    /// everything at `*`; everyone else's authority comes from grants that
    /// cover the scope, are unexpired, and are at or above the scope's
    /// current revocation epoch.
    pub(crate) async fn account_has_cap(
        &self,
        account: &Account,
        cap: &Capability,
        scope: &TokenScope,
        now: u64,
    ) -> Result<bool, StoreError> {
        if self.operators.contains(account) {
            return Ok(true);
        }
        // A namespace owner holds the ns root key's authority — every cap
        // within their namespace (§2.1), the ns-scoped analog of an
        // operator at `*`.
        if let Some(ns_name) = scope_namespace(scope) {
            if let Some(ns) = self.namespaces.namespace(&ns_name).await? {
                if ns.owner == *account {
                    return Ok(true);
                }
            }
        }
        for grant in self.caps.grants_for(account.as_str()).await? {
            let Some(gscope) = TokenScope::parse(&grant.scope) else {
                continue;
            };
            if !gscope.covers(scope) {
                continue;
            }
            if grant.expiry.is_some_and(|e| now >= e) {
                continue;
            }
            if grant.epoch < self.caps.scope_epoch(&grant.scope).await? {
                continue;
            }
            if grant
                .caps
                .iter()
                .filter_map(|c| c.parse::<Capability>().ok())
                .any(|c| &c == cap)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// May `account` delegate `cap` at `scope`? (Holds `grant:<cap>` or is
    /// an operator.)
    pub(crate) async fn account_can_grant(
        &self,
        account: &Account,
        cap: &Capability,
        scope: &TokenScope,
        now: u64,
    ) -> Result<bool, StoreError> {
        if self.operators.contains(account) {
            return Ok(true);
        }
        let grant_cap = Capability::Grant(Box::new(cap.clone()));
        self.account_has_cap(account, &grant_cap, scope, now).await
    }

    /// Mint a network-key-signed token for a `*`/`#chan`-scoped grant
    /// (§11.3). `ns:` scopes need the namespace root key (M4b).
    pub(crate) fn mint_token(
        &self,
        subject: Subject,
        scope: TokenScope,
        caps: Vec<Capability>,
        epoch: u64,
        expiry: u64,
    ) -> String {
        Grant {
            issuer: self.identity.public(),
            subject,
            scope,
            caps,
            epoch,
            expiry,
            parent: None,
        }
        .sign(&self.identity)
        .to_b64()
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

/// The namespace a scope belongs to, if any: `ns:<n>` → n; `#n/chan` → n
/// (a channel names its namespace in its first segment, §2.1).
fn scope_namespace(scope: &TokenScope) -> Option<NamespaceName> {
    let name = match scope {
        TokenScope::Namespace(n) => n.clone(),
        TokenScope::Channel(c) => c.strip_prefix('#')?.split_once('/')?.0.to_string(),
        TokenScope::Wildcard => return None,
    };
    name.parse().ok()
}
