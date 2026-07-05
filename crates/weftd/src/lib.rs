//! # weftd — the WEFT reference server (L3)
//!
//! Glue layer: loads config, builds the `weft-core` context, and feeds it
//! connections from the `weft-transport` acceptors. Exposed as a library so
//! the conformance suite can run a real server in-process on ephemeral
//! ports.

#![forbid(unsafe_code)]

mod acceptor;
pub mod config;
pub mod telemetry;
mod wellknown;

use std::fs;
use std::io::BufReader;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tracing::info;
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
    /// Actual WEFT-IRC gateway address (§17), if enabled.
    pub irc_addr: Option<SocketAddr>,
    endpoint: quinn::Endpoint,
    tasks: Vec<JoinHandle<()>>,
}

impl Server {
    pub async fn shutdown(self) {
        self.endpoint.close(0u32.into(), b"shutdown");
        for task in &self.tasks {
            task.abort();
        }
        self.endpoint.wait_idle().await;
    }
}

/// Validate config, load identity + TLS, spawn actors and accept loops.
pub async fn start(config: Config) -> anyhow::Result<Server> {
    let network: weft_proto::NetworkName = config
        .network
        .parse()
        .with_context(|| format!("invalid network name {:?}", config.network))?;
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
            Ok((name, policy))
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

    let info = ServerInfo {
        network: network.clone(),
        motd: config.motd.clone(),
        features: vec!["presence".to_string()], // media/voice/e2ee/backfill: later
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
    // §11 inbound bridge policy. Peer key pinning arrives with the M5d dialer;
    // for now `accept_any` (open federation) is the configurable knob.
    let federation = weft_core::FederationConfig {
        peer_keys: std::collections::HashMap::new(),
        accept_any: config.federation.accept_any,
        auto_accept: config.federation.auto_accept,
    };
    let (ctx, channels, mut tasks) = match config.storage.backend {
        config::StorageBackend::Memory => {
            boot(
                info,
                identity,
                registration_open,
                dm_policy,
                maintenance,
                Arc::new(MemoryStore::default()),
                &seed_channels,
                operators.clone(),
                ns_creation_open,
                ns_quota,
                federation.clone(),
            )
            .await?
        }
        config::StorageBackend::Postgres => {
            let url = config
                .storage
                .url
                .as_deref()
                .context("storage.backend = \"postgres\" requires storage.url")?;
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
                &seed_channels,
                operators.clone(),
                ns_creation_open,
                ns_quota,
                federation.clone(),
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

    // TLS identity: operator-provided PEM ("real certs"), else a fresh
    // self-signed one — fine for dev, unverifiable by design.
    let (cert_chain, key) = match &config.tls {
        Some(tls) => load_tls(tls)?,
        None => self_signed(&network)?,
    };

    let server_config = weft_transport::server_config(cert_chain, key)?;
    let endpoint = weft_transport::server_endpoint(server_config, config.listen.quic)
        .context("binding QUIC endpoint")?;
    let quic_addr = endpoint.local_addr()?;

    tasks.push(tokio::spawn(acceptor::accept_quic(
        endpoint.clone(),
        Arc::clone(&ctx),
    )));

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

    let http_addr = match config.listen.http {
        None => None,
        Some(addr) => {
            let listener = bind(addr, "HTTP").await?;
            let http_addr = listener.local_addr()?;
            tasks.push(tokio::spawn(wellknown::serve(listener, Arc::clone(&ctx))));
            Some(http_addr)
        }
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
        irc_addr,
        endpoint,
        tasks,
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

fn load_tls(
    tls: &config::Tls,
) -> anyhow::Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let mut reader = BufReader::new(
        fs::File::open(&tls.cert)
            .with_context(|| format!("opening certificate {}", tls.cert.display()))?,
    );
    let certs = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .context("parsing certificate PEM")?;
    anyhow::ensure!(
        !certs.is_empty(),
        "no certificates in {}",
        tls.cert.display()
    );

    let mut reader = BufReader::new(
        fs::File::open(&tls.key)
            .with_context(|| format!("opening private key {}", tls.key.display()))?,
    );
    let key = rustls_pemfile::private_key(&mut reader)
        .context("parsing private key PEM")?
        .with_context(|| format!("no private key in {}", tls.key.display()))?;
    Ok((certs, key))
}

fn self_signed(
    network: &weft_proto::NetworkName,
) -> anyhow::Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let cert = rcgen::generate_simple_self_signed(vec![
        network.as_str().to_string(),
        "localhost".to_string(),
    ])
    .context("generating self-signed certificate")?;
    Ok((
        vec![cert.cert.der().clone()],
        PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into()),
    ))
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
    seed: &[(weft_proto::ChannelName, weft_proto::RetentionPolicy)],
    operators: Vec<weft_proto::Account>,
    ns_creation_open: bool,
    ns_quota: u64,
    federation: weft_core::FederationConfig,
) -> anyhow::Result<(
    Arc<ServerCtx>,
    Vec<(weft_proto::ChannelName, weft_proto::RetentionPolicy)>,
    Vec<JoinHandle<()>>,
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
        + 'static,
{
    for (name, policy) in seed {
        store
            .upsert_channel(name, *policy)
            .await
            .map_err(|e| anyhow::anyhow!("seeding channel {name}: {e}"))?;
    }
    let channels = store
        .list_channels()
        .await
        .map_err(|e| anyhow::anyhow!("loading channels: {e}"))?;
    let ctx = Arc::new(ServerCtx::new(
        info,
        channels.iter().cloned(),
        identity,
        registration_open,
        Arc::clone(&store),
        dm_policy,
        operators,
        ns_creation_open,
        ns_quota,
        // §11 inbound bridge policy; peer *pinning* + the outbound dialer are M5d.
        federation,
    ));
    let events: Arc<dyn EventStore> = store.clone();
    let reports: Arc<dyn weft_store::ReportStore> = store.clone();
    let namespaces: Arc<dyn weft_store::NamespaceStore> = store;
    let tasks = vec![weft_core::spawn_maintenance(
        events,
        namespaces,
        reports,
        channels.clone(),
        dm_policy,
        maintenance,
    )];
    Ok((ctx, channels, tasks))
}
