//! # weftd — the WEFT reference server (L3)
//!
//! Glue layer: loads config, builds the `weft-core` context, and feeds it
//! connections from the `weft-transport` acceptors. Exposed as a library so
//! the conformance suite can run a real server in-process on ephemeral
//! ports.

#![forbid(unsafe_code)]

mod acceptor;
pub mod admin_cli;
pub mod config;
pub mod dialer;
pub mod livekit;
pub mod mailer;
pub mod media;
pub mod telemetry;
mod tls;
mod web;
mod wellknown;

use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tracing::{info, warn};
use weft_core::{Keypair, MaintenanceConfig, MemoryStore, ServerCtx, ServerInfo};
use weft_store::{AccountStore, CapabilityStore, ChannelStore, EventStore, InviteStore, PgStore};

pub use config::Config;

/// A running server; dropping it does NOT stop the accept loops — call
/// [`Server::shutdown`].
pub struct Server {
    /// Actual QUIC address (resolves port 0).
    pub quic_addr: SocketAddr,
    /// Actual WS address, if the fallback is enabled.
    pub ws_addr: Option<SocketAddr>,
    /// Actual HTTP address (`/.well-known/weft`), if enabled.
    pub http_addr: Option<SocketAddr>,
    /// Actual HTTPS address (well-known + admin, TLS), if enabled.
    pub https_addr: Option<SocketAddr>,
    /// Actual WEFT-IRC gateway address (§17), if enabled.
    pub irc_addr: Option<SocketAddr>,
    endpoint: quinn::Endpoint,
    tasks: Vec<JoinHandle<()>>,
    ctx: Arc<ServerCtx>,
    peer_links: dialer::PeerLinks,
}

/// How long graceful shutdown waits for connections to drain before giving up.
const GRACE_SECS: u64 = 10;

impl Server {
    /// The server context — the stores + registry + network identity. Exposed
    /// for the outbound dialer and integration tests.
    pub fn ctx(&self) -> &Arc<ServerCtx> {
        &self.ctx
    }

    /// The registry of live outbound bridge connections the mirror consumer
    /// (§11.8) pulls over. Exposed so an integration test can drive a bridge by
    /// hand yet still have the in-process mirror consumer see its connection.
    pub fn peer_links(&self) -> dialer::PeerLinks {
        self.peer_links.clone()
    }

    /// Graceful shutdown: signal every session to finish its current command
    /// and close, stop accepting, drain in-flight connections + HTTP requests,
    /// then close the QUIC endpoint. Bounded by `GRACE_SECS`; anything still
    /// running past that is left to the runtime teardown on process exit.
    pub async fn shutdown(self) {
        let Server {
            endpoint,
            tasks,
            ctx,
            ..
        } = self;
        info!("graceful shutdown: draining connections");
        // Signal sessions, accept loops, maintenance, and the HTTP servers.
        ctx.shutdown.cancel();
        let grace = std::time::Duration::from_secs(GRACE_SECS);
        let drained = tokio::time::timeout(grace, async move {
            for task in tasks {
                let _ = task.await;
            }
        })
        .await;
        if drained.is_err() {
            warn!("shutdown grace ({GRACE_SECS}s) elapsed; remaining tasks stop on exit");
        }
        // Peers already received session-level closes; this closes the endpoint.
        endpoint.close(0u32.into(), b"shutdown");
        endpoint.wait_idle().await;
        info!("shutdown complete");
    }
}

/// §16 build the voice backend from config, or `None` when voice is off / can't
/// come up. Dispatches on `[voice] backend`: `native` = the embedded WEFT-RT SFU
/// (feature-gated), `livekit` = a LiveKit token minter (no build feature). A
/// construction failure logs + degrades to no-voice rather than aborting boot.
fn build_voice_backend(
    cfg: &config::Voice,
    network: &weft_proto::NetworkName,
) -> Option<Arc<dyn weft_core::VoiceBackend>> {
    if !cfg.enabled {
        return None;
    }
    match cfg.backend {
        config::VoiceBackendKind::Native => build_voice_sfu(cfg),
        config::VoiceBackendKind::Livekit => build_livekit(cfg, network),
    }
}

/// §16 M-lk-0: build the LiveKit backend — validate the `[voice.livekit]` config,
/// then wrap the token signer in weft-core's `LiveKitBackend`. Missing config
/// degrades to no-voice (never a half-configured server).
fn build_livekit(
    cfg: &config::Voice,
    network: &weft_proto::NetworkName,
) -> Option<Arc<dyn weft_core::VoiceBackend>> {
    let lk = &cfg.livekit;
    if lk.url.is_empty() || lk.api_key.is_empty() || lk.api_secret.is_empty() {
        warn!("[voice] backend=livekit but [voice.livekit] url/api_key/api_secret is incomplete; voice disabled");
        return None;
    }

    // Clients get `url` (the offer endpoint); weftd's own Room-API calls use
    // `api_url` when set (the internal address), else fall back to `url`.
    let api_url = if lk.api_url.is_empty() {
        &lk.url
    } else {
        &lk.api_url
    };
    let signer = livekit::LiveKitSigner::new(lk.api_key.clone(), lk.api_secret.clone(), api_url);
    let ttl = if lk.token_ttl_secs == 0 {
        600
    } else {
        lk.token_ttl_secs
    };

    info!(url = %lk.url, ttl_secs = ttl, "voice backend: LiveKit (§16, M-lk-0)");
    Some(Arc::new(weft_core::LiveKitBackend::new(
        Arc::new(signer),
        lk.url.clone(),
        network.clone(),
        ttl,
    )))
}

/// §16 build the embedded WEFT-RT SFU (`backend = native`). Feature-gated: a
/// build without `voice` can't carry the webrtc stack, so it degrades with a
/// warning.
#[cfg(feature = "voice")]
fn build_voice_sfu(cfg: &config::Voice) -> Option<Arc<dyn weft_core::VoiceBackend>> {
    match weft_rt::WebrtcSfu::new(weft_rt::SfuConfig {
        udp_port_min: cfg.udp_port_min,
        udp_port_max: cfg.udp_port_max,
        ice_servers: cfg.stun.clone(),
    }) {
        Ok(sfu) => {
            info!(
                udp = %format!("{}-{}", cfg.udp_port_min, cfg.udp_port_max),
                "voice SFU enabled (§16)"
            );
            Some(std::sync::Arc::new(sfu))
        }
        Err(e) => {
            warn!("voice SFU failed to start, continuing without voice: {e}");
            None
        }
    }
}

#[cfg(not(feature = "voice"))]
fn build_voice_sfu(_cfg: &config::Voice) -> Option<Arc<dyn weft_core::VoiceBackend>> {
    warn!("[voice] backend=native but weftd was built without the `voice` feature");
    None
}

/// Validate config, load identity + TLS, spawn actors and accept loops.
pub async fn start(config: Config) -> anyhow::Result<Server> {
    let network: weft_proto::NetworkName = config
        .network
        .parse()
        .with_context(|| format!("invalid network name {:?}", config.network))?;
    // Log before any potentially-slow step (store connect, key load) so a hang
    // during boot is diagnosable rather than silent.
    info!(%network, backend = ?config.storage.backend, "weftd starting");
    // Config channels are validated here, then *seeded* into the store —
    // the store is the source of truth the registry loads from.
    let seed_channels = config
        .channels
        .iter()
        .map(|channel| {
            let name = channel
                .name()
                .parse::<weft_proto::ChannelName>()
                .with_context(|| format!("invalid channel {:?}", channel.name()))?;
            let policy = channel
                .policy()
                .parse::<weft_proto::RetentionPolicy>()
                .with_context(|| format!("invalid policy {:?} for {}", channel.policy(), name))?;
            anyhow::ensure!(
                policy != weft_proto::RetentionPolicy::E2ee,
                "e2ee channels land in M6 ({name})"
            );
            let kind = channel
                .kind()
                .parse::<weft_proto::ChannelKind>()
                .map_err(|_| {
                    anyhow::anyhow!("invalid channel kind {:?} for {}", channel.kind(), name)
                })?;
            Ok((name, policy, kind))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let dm_policy: weft_proto::RetentionPolicy = config
        .dm_policy
        .parse()
        .with_context(|| format!("invalid dm_policy {:?}", config.dm_policy))?;
    // §11.3: operator accounts hold the network key's authority (every cap
    // at `*`) — the root of the grant chain.
    let operators = config
        .operators
        .iter()
        .map(|name| {
            name.parse::<weft_proto::Account>()
                .with_context(|| format!("invalid operator {name:?}"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    // §2.2 namespace creation policy.
    let ns_creation_open = config.namespaces.creation == config::NsCreation::Open;
    let ns_quota = config.namespaces.quota;

    // Network signing key (§10.2): persisted so attestations stay valid
    // across restarts; ephemeral only when explicitly configured away.
    let identity = match &config.identity.key_file {
        Some(path) => load_or_generate_key(path)?,
        None => Keypair::generate(),
    };
    // Seed copy for the outbound dialer (identity is consumed by ServerCtx).
    let identity_seed = identity.seed_b64();

    // §16 build the voice backend up front (native SFU or LiveKit) so WELCOME
    // advertises `features=voice` only when it actually came up; it's installed
    // into `ctx` once boot returns it.
    let voice_sfu = build_voice_backend(&config.voice, &network);
    let mut features = vec!["presence".to_string()]; // media/e2ee/backfill: later
    if voice_sfu.is_some() {
        features.push("voice".to_string());
    }
    let info = ServerInfo {
        network: network.clone(),
        motd: config.motd.clone(),
        features,
    };
    // Backend selection. Both paths run the same seed-then-load boot:
    // config channels are upserted, then the registry is built from
    // list_channels() — channels created at runtime (M4) or by an earlier
    // boot survive on Postgres.
    let registration_open = config.registration == config::Registration::Open;
    let maintenance = MaintenanceConfig {
        interval: std::time::Duration::from_secs(config.storage.maintenance_interval_secs),
        compact_after: std::time::Duration::from_secs(config.storage.compact_after_hours * 3600),
    };
    // §11 inbound bridge policy. `[[peers]]` pin peer keys (verified both for
    // inbound auth and for the outbound dialer); `accept_any` opens it wider.
    let mut peer_keys = std::collections::HashMap::new();
    for peer in &config.peers {
        match (
            peer.network.parse::<weft_proto::NetworkName>(),
            weft_core::PublicKey::from_b64(&peer.key),
        ) {
            (Ok(n), Ok(k)) => {
                peer_keys.insert(n, k);
            }
            _ => warn!(peer = %peer.network, "skipping [[peers]] entry with invalid network/key"),
        }
    }
    // Shared with the media data plane so inbound `MIRROR` pulls can verify a
    // requester network's key exactly as the control plane verifies a bridge.
    let mirror_peer_keys: media::PeerKeys = Arc::new(peer_keys.clone());
    let federation = weft_core::FederationConfig {
        peer_keys,
        accept_any: config.federation.accept_any,
        auto_accept: config.federation.auto_accept,
    };
    // §13 blob store: filesystem CAS when `[media] dir` is set, else in-memory
    // (ephemeral — pairs with the memory storage backend + conformance tests).
    let blobs: Arc<dyn weft_store::BlobStore> = match &config.media.dir {
        Some(dir) => Arc::new(
            media::FsBlobStore::open(dir.clone())
                .await
                .with_context(|| format!("opening media dir {}", dir.display()))?,
        ),
        None => {
            warn!(
                "[media] dir is unset — using an IN-MEMORY blob store; uploaded \
                 images/files are lost on restart. Set `[media] dir = \"…\"` \
                 (and a persistent `[storage] backend`) to keep media."
            );
            Arc::new(weft_core::MemBlobStore::default())
        }
    };
    let (ctx, channels, mut tasks, admin_router) = match config.storage.backend {
        config::StorageBackend::Memory => {
            boot(
                info,
                identity,
                registration_open,
                dm_policy,
                maintenance,
                Arc::new(MemoryStore::default()),
                Arc::clone(&blobs),
                &seed_channels,
                operators.clone(),
                ns_creation_open,
                ns_quota,
                federation.clone(),
                config.admin.enabled,
                config.admin.delete_grace_days,
            )
            .await?
        }
        config::StorageBackend::Postgres => {
            let url = config
                .storage
                .url
                .as_deref()
                .context("storage.backend = \"postgres\" requires storage.url")?;
            info!("connecting to PostgreSQL (this blocks until the DB answers)…");
            let store = PgStore::connect(url)
                .await
                .context("connecting to PostgreSQL")?;
            boot(
                info,
                identity,
                registration_open,
                dm_policy,
                maintenance,
                Arc::new(store),
                Arc::clone(&blobs),
                &seed_channels,
                operators.clone(),
                ns_creation_open,
                ns_quota,
                federation.clone(),
                config.admin.enabled,
                config.admin.delete_grace_days,
            )
            .await?
        }
    };
    info!(
        channels = channels.len(),
        backend = ?config.storage.backend,
        registration = ?config.registration,
        dm_policy = %dm_policy,
        "channel registry loaded from store"
    );

    // §16 install the voice SFU backend (enables the VOICE verbs; already
    // advertised in WELCOME features above).
    if let Some(sfu) = voice_sfu {
        ctx.set_voice_backend(sfu);
        // §16 M-lk-3b: federated voice only cascades on LiveKit. Install the
        // (no-op) relay driver so the WEFT-side relay lifecycle runs; the real
        // libwebrtc media driver is a deployment add-on.
        if config.voice.backend == config::VoiceBackendKind::Livekit {
            ctx.set_voice_relay(Arc::new(livekit::LogRelay));
        }
    }

    // §10.5 install the email sender for account verification: real SMTP when
    // configured, else a dev log-mailer (records claims, prints the code).
    if config.smtp.enabled && !config.smtp.host.is_empty() {
        match mailer::SmtpMailer::new(&config.smtp) {
            Ok(smtp) => {
                info!(host = %config.smtp.host, "SMTP mailer enabled (§10.5)");
                ctx.set_mailer(Arc::new(smtp));
            }
            Err(e) => {
                warn!("SMTP mailer misconfigured, falling back to log-only: {e}");
                ctx.set_mailer(Arc::new(mailer::LogMailer));
            }
        }
    } else {
        ctx.set_mailer(Arc::new(mailer::LogMailer));
    }

    // TLS: one hot-swappable resolver, fed from ACME / a PEM file / self-signed.
    let challenges = tls::Challenges::default();
    let (cert_resolver, tls_task) = tls::setup(&config, &network, Arc::clone(&challenges)).await?;
    if let Some(task) = tls_task {
        tasks.push(task);
    }

    // Share the resolver with the HTTPS listener (below) before QUIC consumes it.
    let https_resolver = Arc::clone(&cert_resolver);
    let server_config = weft_transport::server_config_resolving(cert_resolver)?;
    let endpoint = weft_transport::server_endpoint(server_config, config.listen.quic)
        .context("binding QUIC endpoint")?;
    let quic_addr = endpoint.local_addr()?;

    tasks.push(tokio::spawn(acceptor::accept_quic(
        endpoint.clone(),
        Arc::clone(&ctx),
        Arc::clone(&mirror_peer_keys),
    )));

    // §11.8 federation media mirroring: outbound bridge connections register
    // here so the mirror consumer can pull foreign blobs back over them.
    let peer_links = dialer::PeerLinks::new();

    // §11.2 outbound bridges: one maintained dial per `[[peers]]` entry.
    tasks.extend(dialer::spawn_dialers(
        &config.peers,
        identity_seed.clone(),
        network.clone(),
        Arc::clone(&ctx),
        peer_links.clone(),
    ));

    // §11.8 drain the mirror port: pull blobs referenced by ingested foreign
    // messages back over the bridge (self-authenticating signed `MIRROR`).
    let (mirror_tx, mirror_rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.set_mirror_sink(mirror_tx);
    tasks.push(dialer::spawn_mirror_consumer(
        mirror_rx,
        peer_links.clone(),
        identity_seed.clone(),
        network.clone(),
        Arc::clone(&ctx),
    ));

    // §11.7 drain the backfill port: pull a peer's large scrollback over the
    // bridge data plane (`BACKFILL <token>`) and ingest it (invariants 2, 3).
    let (backfill_tx, backfill_rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.set_backfill_sink(backfill_tx);
    tasks.push(dialer::spawn_backfill_consumer(
        backfill_rx,
        peer_links.clone(),
        Arc::clone(&ctx),
    ));

    // §11.10 auto-federation: wire the FEDERATE trigger to the dialer, only when
    // the network has opted into on-demand outbound bridging.
    if config.federation.auto_bridge == config::AutoBridge::Open {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        ctx.set_auto_bridge_sink(tx);
        tasks.push(dialer::spawn_auto_bridge_consumer(
            rx,
            &config.peers,
            identity_seed,
            network.clone(),
            Arc::clone(&ctx),
            peer_links.clone(),
        ));
        info!("auto-federation enabled (auto_bridge = open)");
    }

    let ws_addr = match config.listen.ws {
        None => None,
        Some(addr) => {
            let listener = bind(addr, "WS").await?;
            let ws_addr = listener.local_addr()?;
            tasks.push(tokio::spawn(acceptor::accept_ws(
                listener,
                Arc::clone(&ctx),
            )));
            Some(ws_addr)
        }
    };

    // One app (well-known + ACME challenge + admin), served plaintext on `http`
    // (needed for the ACME HTTP-01 challenge on :80) and/or TLS-terminated on
    // `https` (the secure way to reach the admin — same cert as QUIC).
    if config.listen.web && config.listen.http.is_none() && config.listen.https.is_none() {
        warn!("[listen] web = true needs an http or https listener; web client not served");
    }
    let http_app = (config.listen.http.is_some() || config.listen.https.is_some()).then(|| {
        let mut app = wellknown::router(&ctx, Arc::clone(&challenges));
        if let Some(admin) = admin_router {
            app = app.merge(admin);
            info!("admin panel mounted at /admin (api at /admin/api)");
        }
        if config.listen.web {
            app = web::mount(app, Arc::clone(&ctx));
            web::log_spa_state();
        }
        // §13 media data plane over HTTP: /media upload + /media/<hash> fetch.
        app = app.merge(media::router(Arc::clone(&ctx)));
        app
    });

    let http_addr = match (config.listen.http, &http_app) {
        (Some(addr), Some(app)) => {
            let listener = bind(addr, "HTTP").await?;
            let http_addr = listener.local_addr()?;
            let app = app.clone();
            let shutdown = ctx.shutdown.clone();
            tasks.push(tokio::spawn(async move {
                let result = axum::serve(listener, app)
                    .with_graceful_shutdown(async move { shutdown.cancelled().await })
                    .await;
                if let Err(e) = result {
                    tracing::error!("HTTP server failed: {e}");
                }
            }));
            Some(http_addr)
        }
        _ => None,
    };

    let https_addr = match (config.listen.https, &http_app) {
        (Some(addr), Some(app)) => {
            let tls = weft_transport::https_config(https_resolver);
            let app = app.clone();
            info!(%addr, "HTTPS (admin/well-known) listening");
            // axum-server drains in-flight requests on `graceful_shutdown`.
            let handle = axum_server::Handle::new();
            let watcher = handle.clone();
            let shutdown = ctx.shutdown.clone();
            tokio::spawn(async move {
                shutdown.cancelled().await;
                watcher.graceful_shutdown(Some(std::time::Duration::from_secs(GRACE_SECS)));
            });
            tasks.push(tokio::spawn(async move {
                let config = axum_server::tls_rustls::RustlsConfig::from_config(tls);
                if let Err(e) = axum_server::bind_rustls(addr, config)
                    .handle(handle)
                    .serve(app.into_make_service())
                    .await
                {
                    tracing::error!("HTTPS server failed: {e}");
                }
            }));
            Some(addr)
        }
        _ => None,
    };

    let irc_addr = match config.listen.irc {
        None => None,
        Some(addr) => {
            let listener = bind(addr, "IRC").await?;
            let irc_addr = listener.local_addr()?;
            tasks.push(tokio::spawn(acceptor::accept_irc(
                listener,
                Arc::clone(&ctx),
                network.to_string(),
            )));
            Some(irc_addr)
        }
    };

    Ok(Server {
        quic_addr,
        ws_addr,
        http_addr,
        https_addr,
        irc_addr,
        endpoint,
        tasks,
        ctx,
        peer_links,
    })
}

async fn bind(addr: SocketAddr, what: &str) -> anyhow::Result<TcpListener> {
    TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {what} listener on {addr}"))
}

/// Load the signing-key seed, or mint one on first boot.
fn load_or_generate_key(path: &Path) -> anyhow::Result<Keypair> {
    if path.exists() {
        let seed = fs::read_to_string(path)
            .with_context(|| format!("reading key file {}", path.display()))?;
        Keypair::from_seed_b64(seed.trim())
            .map_err(|e| anyhow::anyhow!("invalid key file {}: {e}", path.display()))
    } else {
        let keypair = Keypair::generate();
        fs::write(path, keypair.seed_b64() + "\n")
            .with_context(|| format!("writing key file {}", path.display()))?;
        info!(path = %path.display(), "generated network signing key");
        Ok(keypair)
    }
}

/// Adapter implementing the admin panel's live-server actions over the channel
/// registry (embedded mode). Kick / channel-scope-ban force-part the target.
struct LiveRegistry {
    ctx: Arc<ServerCtx>,
}

#[async_trait::async_trait]
impl weft_admin::Live for LiveRegistry {
    async fn eject(&self, channel: &weft_proto::ChannelName, account: &weft_proto::Account) {
        if let Some(handle) = self.ctx.registry.get(channel) {
            handle.eject(account.clone()).await;
        }
    }

    async fn delete_message(&self, msgid: &weft_proto::MsgId, by: &weft_proto::Account) -> bool {
        // Resolve which channel owns the message, then tell its actor to
        // tombstone it (the actor is the single ULID writer, §9.1).
        let Ok(Some(root)) = self.ctx.events.find_root(msgid.ulid()).await else {
            return false;
        };
        let weft_store::Scope::Channel(channel) = root.scope else {
            return false; // DMs / non-channel scopes aren't live actors here
        };
        let Some(handle) = self.ctx.registry.get(&channel) else {
            return false;
        };
        handle.admin_delete(msgid.clone(), by.clone()).await;
        true
    }
}

/// Seed config channels into the store, load the full channel set back
/// (store = source of truth), build the context, start maintenance.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
async fn boot<S>(
    info: ServerInfo,
    identity: Keypair,
    registration_open: bool,
    dm_policy: weft_proto::RetentionPolicy,
    maintenance: MaintenanceConfig,
    store: Arc<S>,
    blobs: Arc<dyn weft_store::BlobStore>,
    seed: &[(
        weft_proto::ChannelName,
        weft_proto::RetentionPolicy,
        weft_proto::ChannelKind,
    )],
    operators: Vec<weft_proto::Account>,
    ns_creation_open: bool,
    ns_quota: u64,
    federation: weft_core::FederationConfig,
    admin_enabled: bool,
    admin_delete_grace_days: u64,
) -> anyhow::Result<(
    Arc<ServerCtx>,
    Vec<(weft_proto::ChannelName, weft_proto::RetentionPolicy)>,
    Vec<JoinHandle<()>>,
    Option<axum::Router>,
)>
where
    S: EventStore
        + AccountStore
        + ChannelStore
        + CapabilityStore
        + InviteStore
        + weft_store::NamespaceStore
        + weft_store::ReportStore
        + weft_store::PeerStore
        + weft_store::NetblockStore
        + weft_store::ModerationStore
        + weft_store::PinStore
        + weft_store::EmojiStore
        + weft_store::MembershipStore
        + weft_store::MediaStore
        + weft_store::MediaBlocklistStore
        + weft_store::RoleStore
        + weft_store::ProfileStore
        + weft_store::AuditStore
        + 'static,
{
    for (name, policy, kind) in seed {
        store
            .upsert_channel(name, *policy, *kind)
            .await
            .map_err(|e| anyhow::anyhow!("seeding channel {name}: {e}"))?;
    }
    let channels = store
        .list_channels()
        .await
        .map_err(|e| anyhow::anyhow!("loading channels: {e}"))?;

    // The admin API shares this process's store + registry. Capture the cookie
    // ingredients (server-only network seed, operators, network) before `info`/
    // `identity`/`operators` are consumed by ServerCtx; wire the live actions
    // over the registry once `ctx` exists.
    let admin_ingredients = admin_enabled.then(|| {
        (
            identity.seed_b64().into_bytes(),
            operators.clone(),
            info.network.to_string(),
        )
    });

    let ctx = Arc::new(ServerCtx::new(
        info,
        channels.iter().cloned(),
        identity,
        registration_open,
        Arc::clone(&store),
        Arc::clone(&blobs),
        dm_policy,
        operators,
        ns_creation_open,
        ns_quota,
        // §11 inbound bridge policy; peer *pinning* + the outbound dialer are M5d.
        federation,
    ));
    let admin_router = admin_ingredients.map(|(secret, ops, network)| {
        let auth = weft_admin::auth::config(secret, ops);
        let live: Arc<dyn weft_admin::Live> = Arc::new(LiveRegistry {
            ctx: Arc::clone(&ctx),
        });
        weft_admin::router(
            weft_admin::AdminState::from_store(Arc::clone(&store), auth, network)
                .with_delete_grace_ms(admin_delete_grace_days * 24 * 60 * 60 * 1000)
                .with_dm_policy(dm_policy)
                .with_live(live)
                .with_live_connections(Arc::clone(&ctx.connections)),
        )
    });

    let events: Arc<dyn EventStore> = store.clone();
    let reports: Arc<dyn weft_store::ReportStore> = store.clone();
    let media_refs: Arc<dyn weft_store::MediaStore> = store.clone();
    let profiles: Arc<dyn weft_store::ProfileStore> = store.clone();
    let accounts: Arc<dyn weft_store::AccountStore> = store.clone();
    let namespaces: Arc<dyn weft_store::NamespaceStore> = store;
    let tasks = vec![weft_core::spawn_maintenance(
        events,
        namespaces,
        reports,
        media_refs,
        Arc::clone(&blobs),
        profiles,
        accounts,
        channels.clone(),
        dm_policy,
        maintenance,
        ctx.shutdown.clone(),
    )];
    Ok((ctx, channels, tasks, admin_router))
}
