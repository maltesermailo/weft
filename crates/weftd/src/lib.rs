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

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use rustls_pki_types::PrivateKeyDer;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use weft_core::{ServerCtx, ServerInfo};

pub use config::Config;

/// A running server; dropping it does NOT stop the accept loops — call
/// [`Server::shutdown`].
pub struct Server {
    /// Actual QUIC address (resolves port 0).
    pub quic_addr: SocketAddr,
    /// Actual WS address, if the fallback is enabled.
    pub ws_addr: Option<SocketAddr>,
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

/// Validate config, spawn channel actors and both accept loops.
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

    let info = ServerInfo {
        network: network.clone(),
        motd: config.motd.clone(),
        features: Vec::new(), // M1 offers no optional features (§3.6)
    };
    let ctx = Arc::new(ServerCtx::new(info, channels));

    // M1 runs on a fresh self-signed certificate: clients have nothing to
    // pin yet — key publication arrives with /.well-known/weft in M2.
    let cert = rcgen::generate_simple_self_signed(vec![
        network.as_str().to_string(),
        "localhost".to_string(),
    ])
    .context("generating self-signed certificate")?;
    let cert_chain = vec![cert.cert.der().clone()];
    let key = PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into());

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
            let listener = TcpListener::bind(addr)
                .await
                .with_context(|| format!("binding WS listener on {addr}"))?;
            let ws_addr = listener.local_addr()?;
            tasks.push(tokio::spawn(acceptor::accept_ws(
                listener,
                Arc::clone(&ctx),
            )));
            Some(ws_addr)
        }
    };

    Ok(Server {
        quic_addr,
        ws_addr,
        endpoint,
        tasks,
    })
}
