//! Shared server context handed to every session.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use weft_crypto::{Attestation, Capability, Grant, Keypair, PublicKey, Subject, TokenScope};
use weft_proto::{Account, ChannelName, NamespaceName, NetworkName, RetentionPolicy};
use weft_store::{
    AccountStore, CapabilityStore, ChannelStore, EventStore, InviteStore, MembershipStore,
    ModerationStore, NamespaceStore, NetblockStore, PeerStore, PinStore, ReportStore, RoleStore,
    StoreError,
};

use crate::accounts::Accounts;
use crate::directory::Directory;
use crate::registry::Registry;

/// The only protocol version this server speaks (§3.6).
pub const PROTOCOL_VERSION: &str = "weft/1";

/// Who is acting in a session: a **local** account, or a **federated** user
/// (`account@network`) whose home network vouches for her over a bridge (§10.4,
/// homeserver authority — F proves its network key, then speaks for its users).
/// Enforcement keys by the subject; local-only authority (operator / namespace
/// owner) never applies to a foreign actor.
#[derive(Debug, Clone)]
pub enum Actor {
    Local(Account),
    /// `account@network`.
    Foreign(String),
}

impl Actor {
    /// The local account, if this actor is local (a foreign actor → `None`) —
    /// for owner/operator comparisons a federated user can never satisfy.
    pub fn local(&self) -> Option<&Account> {
        match self {
            Actor::Local(account) => Some(account),
            Actor::Foreign(_) => None,
        }
    }
}

impl std::fmt::Display for Actor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Actor::Local(account) => write!(f, "{account}"),
            Actor::Foreign(user) => write!(f, "{user}"),
        }
    }
}

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

/// §11 federation config: which peer networks this server will bridge with and
/// how it treats incoming proposals.
///
/// Two trust modes for opening a bridge session (§11.2):
/// - **Pinned** (default, closed): only peers in `peer_keys` may bridge, and
///   only with the exact key pinned there — key rotation is a config change,
///   not a bypass. The secure default.
/// - **Accept-any** (`accept_any = true`, open federation): any non-blocked
///   network may open a bridge by asserting a key and proving control of it
///   (challenge/proof). The session is then bound to *that* key — a peer can
///   only speak for the identity it holds a key for, but nothing external
///   confirms the key really is that network's (no pin / well-known check), so
///   this is trust-on-first-use. `NETBLOCK` is the escape hatch.
///
/// A pinned key always wins over accept-any for that network.
#[derive(Debug, Clone, Default)]
pub struct FederationConfig {
    /// peer network → its pinned Ed25519 signing key.
    pub peer_keys: HashMap<NetworkName, PublicKey>,
    /// Accept a bridge from *any* non-blocked network (open federation),
    /// trusting the key it proves control of. Pinned peers still take their
    /// pinned key.
    pub accept_any: bool,
    /// Auto-accept incoming `BRIDGE PROPOSE` (reference convenience). When
    /// false, an operator must `BRIDGE ACCEPT` explicitly.
    pub auto_accept: bool,
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
    /// Report queue + retention holds (§6.7).
    pub(crate) reports: Arc<dyn ReportStore>,
    /// Bridge peerings + signed manifests (§11.1).
    pub(crate) peers: Arc<dyn PeerStore>,
    /// Operator network blocklist (§11.6).
    pub(crate) netblocks: Arc<dyn NetblockStore>,
    /// Mute/ban deny-list (§6.7).
    pub(crate) moderation: Arc<dyn ModerationStore>,
    /// Pinned messages, per channel (§6.4).
    pub(crate) pins: Arc<dyn PinStore>,
    /// Persistent channel membership for auto-rejoin (§6.3).
    pub(crate) memberships: Arc<dyn MembershipStore>,
    /// Role definitions — named capability-token bundles per scope (§6.5).
    pub(crate) roles: Arc<dyn RoleStore>,
    /// §6.1 live presence, in-memory only (never stored, never bridged).
    /// account → last non-invisible status; served with MEMBERS for correct
    /// roster dots.
    pub(crate) presence:
        std::sync::Mutex<std::collections::HashMap<Account, weft_proto::PresenceStatus>>,
    /// §11 federation config: pinned peer keys + auto-accept.
    pub(crate) federation: FederationConfig,
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
    /// Live session count — inc/dec per connection in `run_session`. Read by the
    /// admin panel's `/stats`; never affects protocol behavior.
    pub connections: Arc<std::sync::atomic::AtomicUsize>,
    /// Graceful-shutdown signal. Cancelled once on shutdown; sessions finish
    /// their current command and close, accept loops stop, maintenance exits.
    pub shutdown: tokio_util::sync::CancellationToken,
    /// §11.10 auto-federation trigger port: `FEDERATE` (weft-core) can't dial
    /// (no transport), so it hands requests to weftd (L3), which owns the
    /// dialer. `None` = the network's `auto_bridge` policy is off.
    auto_bridge_tx: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<AutoBridgeRequest>>,
    /// §11.10 per-account cooldown on `FEDERATE` — a light dial-storm guard even
    /// under the open trigger policy (§6).
    federate_cooldown: std::sync::Mutex<HashMap<Account, std::time::Instant>>,
}

/// A `FEDERATE` request handed from weft-core to weftd's dialer (§11.10).
#[derive(Debug, Clone)]
pub struct AutoBridgeRequest {
    pub network: NetworkName,
    pub namespace: NamespaceName,
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
        federation: FederationConfig,
    ) -> Self
    where
        S: EventStore
            + AccountStore
            + CapabilityStore
            + ChannelStore
            + InviteStore
            + NamespaceStore
            + ReportStore
            + PeerStore
            + NetblockStore
            + ModerationStore
            + PinStore
            + MembershipStore
            + RoleStore
            + 'static,
    {
        let events: Arc<dyn EventStore> = store.clone();
        let accounts: Arc<dyn AccountStore> = store.clone();
        let caps: Arc<dyn CapabilityStore> = store.clone();
        let invites: Arc<dyn InviteStore> = store.clone();
        let channel_store: Arc<dyn ChannelStore> = store.clone();
        let reports: Arc<dyn ReportStore> = store.clone();
        let peers: Arc<dyn PeerStore> = store.clone();
        let netblocks: Arc<dyn NetblockStore> = store.clone();
        let moderation: Arc<dyn ModerationStore> = store.clone();
        let pins: Arc<dyn PinStore> = store.clone();
        let memberships: Arc<dyn MembershipStore> = store.clone();
        let roles: Arc<dyn RoleStore> = store.clone();
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
            reports,
            ns_creation_open,
            ns_quota,
            peers,
            netblocks,
            moderation,
            pins,
            memberships,
            roles,
            presence: std::sync::Mutex::new(std::collections::HashMap::new()),
            federation,
            operators: operators.into_iter().collect(),
            identity,
            next_session: AtomicU64::new(1),
            connections: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            shutdown: tokio_util::sync::CancellationToken::new(),
            auto_bridge_tx: std::sync::OnceLock::new(),
            federate_cooldown: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// weftd installs the auto-federation dialer sink (enables `FEDERATE`).
    pub fn set_auto_bridge_sink(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<AutoBridgeRequest>,
    ) {
        let _ = self.auto_bridge_tx.set(tx);
    }

    /// §11.10 hand a `FEDERATE` request to the dialer. `false` if auto-bridging
    /// is off (no sink) or the channel is gone.
    pub(crate) fn request_auto_bridge(&self, req: AutoBridgeRequest) -> bool {
        matches!(self.auto_bridge_tx.get(), Some(tx) if tx.send(req).is_ok())
    }

    /// §11.10 per-account cooldown: at most one `FEDERATE` per window.
    pub(crate) fn federate_allowed(&self, account: &Account) -> bool {
        const COOLDOWN: std::time::Duration = std::time::Duration::from_secs(3);
        let now = std::time::Instant::now();
        let mut recent = self.federate_cooldown.lock().expect("cooldown lock");
        match recent.get(account) {
            Some(&last) if now.duration_since(last) < COOLDOWN => false,
            _ => {
                recent.insert(account.clone(), now);
                true
            }
        }
    }

    // ---- §11 federation ----

    /// The pinned signing key for a configured peer network, if any. Bridge
    /// sessions authenticate against this (§11.2).
    pub(crate) fn peer_key(&self, network: &NetworkName) -> Option<&PublicKey> {
        self.federation.peer_keys.get(network)
    }

    pub(crate) fn bridge_auto_accept(&self) -> bool {
        self.federation.auto_accept
    }

    /// Open-federation mode: accept a bridge from any non-blocked network,
    /// trusting the key it proves control of (§11.2, trust-on-first-use).
    pub(crate) fn bridge_accept_any(&self) -> bool {
        self.federation.accept_any
    }

    /// Sign a manifest with this network's key (§11.3). The §11.3 authority
    /// ladder is enforced *locally* before calling this (does the operator
    /// hold `bridge`/ns-owner/`*`?); the wire artifact is uniformly
    /// network-key-signed so the peer can verify it against our well-known.
    pub(crate) fn sign_manifest(&self, manifest: &weft_crypto::Manifest) -> String {
        manifest.sign(&self.identity).to_b64()
    }

    /// Our own network name as the validated type (manifests name their peer).
    pub(crate) fn network(&self) -> &NetworkName {
        &self.info.network
    }

    // ---- capability enforcement (§10.4, invariant 4) ----

    /// The grant-store key for an actor: a local account's immutable **ULID**
    /// (§10.4), or a foreign `account@network` verbatim. `None` = unknown local
    /// account (holds no grants).
    pub(crate) async fn actor_store_key(
        &self,
        actor: &Actor,
    ) -> Result<Option<String>, StoreError> {
        match actor {
            Actor::Local(account) => self.accounts.account_ulid(account).await,
            Actor::Foreign(user) => Ok(Some(user.clone())),
        }
    }

    /// Does `actor` hold `cap` for an object at `scope`? Operators hold
    /// everything at `*`; a namespace owner holds everything in their namespace;
    /// everyone else's authority comes from grants that cover the scope, are
    /// unexpired, and are at or above the scope's current revocation epoch.
    /// Operator/owner authority is **local-only** — a federated actor (§10.4,
    /// homeserver authority) is never a local operator or owner, so her power on
    /// H comes purely from what H granted `account@network`.
    pub(crate) async fn actor_has_cap(
        &self,
        actor: &Actor,
        cap: &Capability,
        scope: &TokenScope,
        now: u64,
    ) -> Result<bool, StoreError> {
        if let Actor::Local(account) = actor {
            if self.operators.contains(account) {
                return Ok(true);
            }
            if let Some(ns_name) = scope_namespace(scope) {
                if let Some(ns) = self.namespaces.namespace(&ns_name).await? {
                    if ns.owner == *account {
                        return Ok(true);
                    }
                }
            }
        }
        let Some(key) = self.actor_store_key(actor).await? else {
            return Ok(false);
        };
        for grant in self.caps.grants_for(&key).await? {
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

    /// Local-account convenience wrapper over [`Self::actor_has_cap`].
    pub(crate) async fn account_has_cap(
        &self,
        account: &Account,
        cap: &Capability,
        scope: &TokenScope,
        now: u64,
    ) -> Result<bool, StoreError> {
        self.actor_has_cap(&Actor::Local(account.clone()), cap, scope, now)
            .await
    }

    /// May `actor` delegate `cap` at `scope`? (Holds `grant:<cap>`, or is a local
    /// operator.)
    pub(crate) async fn actor_can_grant(
        &self,
        actor: &Actor,
        cap: &Capability,
        scope: &TokenScope,
        now: u64,
    ) -> Result<bool, StoreError> {
        if let Actor::Local(account) = actor {
            if self.operators.contains(account) {
                return Ok(true);
            }
        }
        let grant_cap = Capability::Grant(Box::new(cap.clone()));
        self.actor_has_cap(actor, &grant_cap, scope, now).await
    }


    /// Resolve a GRANT/REVOKE subject string to its stable identity: a device
    /// key, a local account's **ULID**, or a foreign `account@network` (§10.4).
    /// Returns the typed token `Subject` and the string the grant store keys by
    /// (they always agree). `None` for an account name with no such account —
    /// you can't grant to an identity that doesn't exist.
    pub(crate) async fn resolve_subject(
        &self,
        s: &str,
    ) -> Result<Option<(Subject, String)>, StoreError> {
        if let Ok(key) = PublicKey::from_b64(s) {
            return Ok(Some((Subject::Key(key), s.to_string())));
        }
        if s.contains('@') {
            return Ok(Some((Subject::Foreign(s.to_string()), s.to_string())));
        }
        let Ok(account) = s.parse::<Account>() else {
            return Ok(None);
        };
        match self.accounts.account_ulid(&account).await? {
            Some(ulid_str) => match weft_proto::Ulid::from_string(&ulid_str) {
                Ok(ulid) => Ok(Some((Subject::Account(ulid), ulid_str))),
                Err(_) => Ok(None),
            },
            None => Ok(None),
        }
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

    /// This network's signing public key — a peer pins this to verify our
    /// manifests + bridge auth (§11.2). Safe to expose; the private half stays in.
    pub fn network_public(&self) -> weft_crypto::PublicKey {
        self.identity.public()
    }

    /// Operator accounts — the net-scope (`*`) report handlers (§6.7).
    pub(crate) fn operator_accounts(&self) -> Vec<Account> {
        self.operators.iter().cloned().collect()
    }

    pub(crate) fn next_session_id(&self) -> u64 {
        self.next_session.fetch_add(1, Ordering::Relaxed)
    }
}

/// The scopes a channel's moderation checks consult, widest-covering last: the
/// channel itself, its namespace (if any), and `*`. A mute/ban at any of these
/// covers the channel, so a `*`-scope record is a network-wide action and an
/// `ns:` one is namespace-wide (§6.7).
pub(crate) fn covering_scopes(channel: &ChannelName) -> Vec<String> {
    let mut scopes = vec![channel.to_string()];
    if let Some(ns) = channel_namespace(channel) {
        scopes.push(format!("ns:{ns}"));
    }
    scopes.push("*".to_string());
    scopes
}

/// The namespace a channel belongs to, if any (`#n/chan` → n). Top-level
/// channels (`#general`) have none — they answer to the operator (§2.1).
pub(crate) fn channel_namespace(channel: &ChannelName) -> Option<NamespaceName> {
    channel
        .as_str()
        .strip_prefix('#')?
        .split_once('/')?
        .0
        .parse()
        .ok()
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
