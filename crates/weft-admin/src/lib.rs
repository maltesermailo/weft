//! weft-admin — the operator web admin panel (L3).
//!
//! A JSON API + embedded SPA over the store (reports, accounts, messages,
//! moderation, grants, stats) plus moderation actions. It never speaks the WEFT
//! wire protocol — it reads/writes the store directly. weftd mounts [`router`]
//! on its HTTP listener (`[admin] enabled`), sharing the in-process stores +
//! live registry. See `docs/web-admin-panel-plan.md` for the (future) sharded
//! deployment story.
//!
//! Auth is operator-only (see [`auth`]); the panel is the one surface where
//! retention-held report context is visible (invariant 11), and it must keep
//! reporter identity hidden (invariant 12).

pub mod auth;
mod dto;
mod handlers;

use std::sync::Arc;

use axum::Router;
use weft_store::{
    AccountStore, AuditStore, CapabilityStore, ChannelStore, EventStore, MediaBlocklistStore,
    MembershipStore, ModerationStore, NamespaceStore, NetblockStore, PeerStore, ReportStore,
};

pub use auth::AuthConfig;

/// Live-server actions the admin API can only take when it shares the weftd
/// process (embedded): they touch the channel actors (ULID single-writer +
/// broadcast). Standalone leaves this unset, and those endpoints answer 501.
/// weftd provides the adapter over its channel registry.
#[async_trait::async_trait]
pub trait Live: Send + Sync {
    /// Force a channel to drop an account — a kick, or a channel-scope ban's
    /// force-part. No-op if the channel isn't live. The actor broadcasts the
    /// resulting `MEMBER part`; the ejected client cleans up on seeing it.
    async fn eject(&self, channel: &weft_proto::ChannelName, account: &weft_proto::Account);

    /// Delete a message as an operator (delete-any): the owning channel's actor
    /// mints the tombstone (attributed to `by`) and broadcasts `DELETED`.
    /// Returns false if the message or its channel can't be found live.
    async fn delete_message(&self, msgid: &weft_proto::MsgId, by: &weft_proto::Account) -> bool;
}

/// The stores the admin API touches, as trait objects — one process's backend
/// fanned into roles (like `ServerCtx`), so `AdminState` is a plain value.
#[derive(Clone)]
pub struct AdminState {
    pub(crate) accounts: Arc<dyn AccountStore>,
    pub(crate) reports: Arc<dyn ReportStore>,
    pub(crate) events: Arc<dyn EventStore>,
    pub(crate) channels: Arc<dyn ChannelStore>,
    pub(crate) moderation: Arc<dyn ModerationStore>,
    pub(crate) caps: Arc<dyn CapabilityStore>,
    pub(crate) namespaces: Arc<dyn NamespaceStore>,
    pub(crate) memberships: Arc<dyn MembershipStore>,
    pub(crate) netblocks: Arc<dyn NetblockStore>,
    pub(crate) peers: Arc<dyn PeerStore>,
    pub(crate) media_blocks: Arc<dyn MediaBlocklistStore>,
    pub(crate) audit: Arc<dyn AuditStore>,
    pub(crate) auth: Arc<AuthConfig>,
    pub(crate) network: String,
    /// WC3 soft-delete grace window (ms). An account delete is *scheduled*
    /// `delete_grace_ms` in the future (recoverable until the maintenance pass
    /// finalizes it). Default 7 days.
    pub(crate) delete_grace_ms: u64,
    /// The network's uniform DM retention policy (§9.5). WC4 DM-thread browse
    /// gates on it: an `e2ee` policy is "unavailable by policy" (invariant 8),
    /// never materialized. Default `Ephemeral` (non-e2ee).
    pub(crate) dm_policy: weft_proto::RetentionPolicy,
    /// Live connection count, when the API shares the weftd process (embedded);
    /// `None` standalone (a separate process can't see it).
    pub(crate) live_connections: Option<Arc<std::sync::atomic::AtomicUsize>>,
    /// Live-server actions (kick/eject via the channel actors) — embedded only.
    pub(crate) live: Option<Arc<dyn Live>>,
}

/// Default WC3 soft-delete grace window: 7 days.
pub const DEFAULT_DELETE_GRACE_MS: u64 = 7 * 24 * 60 * 60 * 1000;

impl AdminState {
    /// Build from a single concrete backend (`MemoryStore`/`PgStore`). The store
    /// implements every trait, so we clone it into each role object.
    pub fn from_store<S>(store: Arc<S>, auth: AuthConfig, network: String) -> Self
    where
        S: AccountStore
            + ReportStore
            + EventStore
            + ChannelStore
            + ModerationStore
            + CapabilityStore
            + NamespaceStore
            + MembershipStore
            + NetblockStore
            + PeerStore
            + MediaBlocklistStore
            + AuditStore
            + 'static,
    {
        Self {
            accounts: store.clone(),
            reports: store.clone(),
            events: store.clone(),
            channels: store.clone(),
            moderation: store.clone(),
            caps: store.clone(),
            namespaces: store.clone(),
            memberships: store.clone(),
            netblocks: store.clone(),
            peers: store.clone(),
            media_blocks: store.clone(),
            audit: store,
            auth: Arc::new(auth),
            network,
            delete_grace_ms: DEFAULT_DELETE_GRACE_MS,
            dm_policy: weft_proto::RetentionPolicy::Ephemeral,
            live_connections: None,
            live: None,
        }
    }

    /// Override the WC3 soft-delete grace window (default 7 days).
    pub fn with_delete_grace_ms(mut self, ms: u64) -> Self {
        self.delete_grace_ms = ms;
        self
    }

    /// Set the network DM retention policy (WC4 DM-thread browse e2ee gate).
    pub fn with_dm_policy(mut self, policy: weft_proto::RetentionPolicy) -> Self {
        self.dm_policy = policy;
        self
    }

    /// Embedded mode: attach the weftd live-connection counter for `/stats`.
    pub fn with_live_connections(mut self, counter: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        self.live_connections = Some(counter);
        self
    }

    /// Embedded mode: attach live-server actions (kick/eject).
    pub fn with_live(mut self, live: Arc<dyn Live>) -> Self {
        self.live = Some(live);
        self
    }
}

/// The admin surface, all under `/admin`: the SPA at `/admin`, public
/// `login`/`logout` at `/admin/api/*`, everything else operator-gated. weftd
/// merges this into its HTTP router.
pub fn router(state: AdminState) -> Router {
    let protected = handlers::routes().route_layer(axum::middleware::from_fn_with_state(
        state.clone(),
        auth::require_admin,
    ));
    let inner = Router::new()
        .route("/", axum::routing::get(spa))
        .route("/api/v1/login", axum::routing::post(auth::login))
        .route("/api/v1/logout", axum::routing::post(auth::logout))
        .merge(protected)
        .with_state(state);
    Router::new().nest("/admin", inner)
}

/// The single-page app, embedded at build time.
async fn spa() -> axum::response::Html<&'static str> {
    axum::response::Html(include_str!("../ui/index.html"))
}
