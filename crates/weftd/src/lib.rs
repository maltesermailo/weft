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
use weft_core::{Keypair, ServerCtx, ServerInfo};

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
    let channels = config
        .channels
        .iter()
        .map(|name| {
            name.parse::<weft_proto::ChannelName>()
                .with_context(|| format!("invalid channel {name:?}"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    // Network signing key (§10.2): persisted so attestations stay valid
    // across restarts; ephemeral only when explicitly configured away.
    let identity = match &config.identity.key_file {
        Some(path) => load_or_generate_key(path)?,
        None => Keypair::generate(),
    };

    let info = ServerInfo {
        network: network.clone(),
        motd: config.motd.clone(),
        features: Vec::new(), // media/voice/e2ee/backfill/presence: later milestones
    };
    let ctx = Arc::new(ServerCtx::new(
        info,
        channels,
        identity,
        config.registration == config::Registration::Open,
    ));

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

    let mut tasks = vec![tokio::spawn(acceptor::accept_quic(
        endpoint.clone(),
        Arc::clone(&ctx),
    ))];

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

    Ok(Server {
        quic_addr,
        ws_addr,
        http_addr,
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
