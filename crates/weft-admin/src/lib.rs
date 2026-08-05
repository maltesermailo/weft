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
    AccountStore, AuditStore, CapabilityStore, ChannelStore, EmojiStore, EventStore, InviteStore,
    MediaBlocklistStore, MembershipStore, ModerationStore, NamespaceStore, NetblockStore,
    PeerStore, ReportStore, RoleStore,
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

    /// plugin-spec §22: the plugin action catalog as JSON, so the panel knows
    /// which `admin`-surface pages to offer. Empty when nothing is registered.
    async fn plugin_catalog(&self) -> String;

    /// plugin-spec §22: run a plugin action **and wait** for its answer.
    ///
    /// The panel is HTTP request/response and holds no session, so unlike every
    /// other surface it cannot receive a pushed view — weftd bridges the shapes
    /// and returns `(view_id, payload)`. `view_id` drives later steps of the same
    /// flow. `None` when the plugin is gone or does not answer in time: an
    /// operator gets a plain failure rather than a request that hangs.
    /// `params_b64` is the wire encoding (base64 CBOR), not JSON — see
    /// [`encode_plugin_params`], which is the only thing that should produce it.
    async fn plugin_invoke(
        &self,
        plugin: &str,
        action: &str,
        ctx_ref: Option<String>,
        params_b64: Option<String>,
    ) -> Option<(String, String)>;

    /// A later step of a panel-owned flow: a submit (`button` = `None`) or a
    /// control click. Same wait-for-the-answer contract as [`Self::plugin_invoke`];
    /// `None` also covers a view the panel does not own.
    async fn plugin_step(
        &self,
        view_id: &str,
        button: Option<String>,
        values_b64: Option<String>,
    ) -> Option<String>;

    /// The panel dismissed a view — tell the plugin so it can drop the flow.
    /// Fire-and-forget: there is nothing to wait for.
    async fn plugin_close(&self, view_id: &str);

    /// Framework §7a.0f: tell the provider that governs `namespace` to stop (or
    /// resume) bridging it. **The bridge stores and enforces this** — weftd only
    /// carries the operator's decision across, once. Not a weaker guarantee: the
    /// bridge is run by the same operator as the server.
    ///
    /// Returns false if no live provider governs that namespace, so the panel can
    /// say "the bridge is not connected" rather than implying it took effect.
    async fn set_bridging(&self, namespace: &weft_proto::NamespaceId, banned: bool) -> bool;

    /// WC7 forced logout: cut every live session of `account`, returning how
    /// many were closed. Suspend only blocks *new* logins — this ends the
    /// already-connected ones. Each session runs its ordinary cleanup, so
    /// co-members observe a normal disconnect (presence goes offline, voice
    /// rooms broadcast a leave); persistent membership is retained, exactly as
    /// when the client's own network drops.
    async fn disconnect_account(&self, account: &weft_proto::Account) -> usize;

    /// Stop a channel's live actor (e.g. during a namespace delete) so it can't
    /// keep accepting posts after its store rows are gone. No-op if not live.
    async fn remove_channel(&self, channel: &weft_proto::ChannelName);
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
    pub(crate) roles: Arc<dyn RoleStore>,
    pub(crate) emoji: Arc<dyn EmojiStore>,
    pub(crate) invites: Arc<dyn InviteStore>,
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
    /// §13 media blobs, so the panel can render attachment images from its own
    /// operator-authed route (the public `/media` endpoint uses a query-string
    /// bearer that an `<img>` tag can't carry). `None` = images 501 / link only.
    pub(crate) blobs: Option<Arc<dyn weft_store::BlobStore>>,
    /// §6.7 the network's support account — an operator can "seize to support" a
    /// namespace, transferring ownership to this (login-disabled) account for
    /// moderation. `None` = the feature is unconfigured.
    pub(crate) support_account: Option<String>,
    /// The foreign-URI schemes still configured in `[[plugin.remote]]`. A
    /// provider counts as **disabled** once its scheme is gone from there —
    /// durable and operator-set, unlike a transient disconnect. Gates deleting a
    /// namespace that provider governs.
    ///
    /// `None` = we cannot know (the panel is running standalone, without the
    /// server's config), and then *no* provider-managed namespace may be
    /// deleted — refusing on ignorance beats destroying a live bridge's space.
    pub(crate) configured_schemes: Option<Vec<String>>,
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
            + RoleStore
            + EmojiStore
            + InviteStore
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
            roles: store.clone(),
            emoji: store.clone(),
            invites: store.clone(),
            audit: store,
            auth: Arc::new(auth),
            network,
            delete_grace_ms: DEFAULT_DELETE_GRACE_MS,
            configured_schemes: None,
            dm_policy: weft_proto::RetentionPolicy::Ephemeral,
            live_connections: None,
            live: None,
            blobs: None,
            support_account: None,
        }
    }

    /// §6.7 configure the network's support account for "seize to support".
    /// The schemes still pinned in `[[plugin.remote]]` (embedded only) — see
    /// [`AdminState::configured_schemes`].
    pub fn with_configured_schemes(mut self, schemes: Vec<String>) -> Self {
        self.configured_schemes = Some(schemes);
        self
    }

    pub fn with_support_account(mut self, account: Option<String>) -> Self {
        self.support_account = account;
        self
    }

    /// Embedded mode: attach the media blob store so the panel renders attachment
    /// images from its own operator-authed route.
    pub fn with_blobs(mut self, blobs: Arc<dyn weft_store::BlobStore>) -> Self {
        self.blobs = Some(blobs);
        self
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
