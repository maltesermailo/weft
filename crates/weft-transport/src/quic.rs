//! QUIC control plane (§3.1): ALPN `weft/1`, the first bidi stream is
//! stream 0 = newline-delimited UTF-8 control lines. Uni streams
//! (data plane) come with media in M6.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use quinn::crypto::rustls::QuicServerConfig;
use quinn::{Connection, IdleTimeout, RecvStream, SendStream, TransportConfig};
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::Join;
use tokio_util::codec::{Framed, LinesCodec};
use weft_proto::MAX_LINE_BYTES;

use crate::TransportError;

/// §3.1: the only ALPN this server offers; mismatching clients fail the
/// TLS handshake before a single line is read.
pub const ALPN: &[u8] = b"weft/1";

/// Transport-level idle limit. quinn's default (30 s) matched the §3.4 PING
/// cadence too tightly and silently killed quiet-but-healthy connections.
/// Must stay comfortably above the PING interval AND above the session
/// layer's line-based liveness window (READY: 30 s), which is the intended
/// arbiter of aliveness.
const MAX_IDLE: Duration = Duration::from_secs(120);

/// Shared transport tuning. `keep_alive`: §3.4 lets QUIC keepalives
/// substitute for *sending* PINGs — clients want one; the server does not
/// (liveness is the client's burden).
pub(crate) fn transport_config(keep_alive: Option<Duration>) -> TransportConfig {
    let mut transport = TransportConfig::default();
    transport.max_idle_timeout(Some(
        IdleTimeout::try_from(MAX_IDLE).expect("well below the VarInt bound"),
    ));
    transport.keep_alive_interval(keep_alive);
    transport
}

/// Build the QUIC server config from a fixed identity (ALPN pinned). A thin
/// wrapper over [`server_config_resolving`] with a non-swapping resolver.
pub fn server_config(
    cert_chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<quinn::ServerConfig, TransportError> {
    server_config_resolving(ReloadableCert::new(certified_key(cert_chain, key)?))
}

/// Assemble a rustls [`CertifiedKey`](rustls::sign::CertifiedKey) from a PEM
/// chain + private key — the unit a [`ReloadableCert`] swaps.
pub fn certified_key(
    cert_chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<Arc<rustls::sign::CertifiedKey>, TransportError> {
    let signing_key = rustls::crypto::ring::sign::any_supported_type(&key)
        .map_err(|_| TransportError::NoTls13)?;
    Ok(Arc::new(rustls::sign::CertifiedKey::new(
        cert_chain,
        signing_key,
    )))
}

/// A hot-swappable server certificate. rustls calls [`resolve`] on every
/// ClientHello, so `store`-ing a new key rotates the cert for all *new*
/// connections without dropping live ones — the basis for both cert-file
/// hot-reload and built-in ACME renewal.
///
/// [`resolve`]: rustls::server::ResolvesServerCert::resolve
#[derive(Debug)]
pub struct ReloadableCert {
    current: arc_swap::ArcSwap<rustls::sign::CertifiedKey>,
}

impl ReloadableCert {
    pub fn new(key: Arc<rustls::sign::CertifiedKey>) -> Arc<Self> {
        Arc::new(Self {
            current: arc_swap::ArcSwap::from(key),
        })
    }

    /// Swap in a freshly-issued/renewed certificate.
    pub fn store(&self, key: Arc<rustls::sign::CertifiedKey>) {
        self.current.store(key);
    }
}

impl rustls::server::ResolvesServerCert for ReloadableCert {
    fn resolve(
        &self,
        _hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        Some(self.current.load_full())
    }
}

/// Build the QUIC server config from a hot-swappable cert resolver (file
/// hot-reload or ACME). Cert rotations via [`ReloadableCert::store`] take
/// effect for subsequent connections; this config is built once.
pub fn server_config_resolving(
    resolver: Arc<dyn rustls::server::ResolvesServerCert>,
) -> Result<quinn::ServerConfig, TransportError> {
    let mut tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let quic_tls = QuicServerConfig::try_from(tls).map_err(|_| TransportError::NoTls13)?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(quic_tls));
    config.transport_config(Arc::new(transport_config(None)));
    Ok(config)
}

/// Bind a listening endpoint.
pub fn server_endpoint(
    config: quinn::ServerConfig,
    addr: SocketAddr,
) -> io::Result<quinn::Endpoint> {
    quinn::Endpoint::server(config, addr)
}

/// A **verified** QUIC client endpoint: the server's certificate is checked
/// against the bundled Mozilla root store. This is the default for real
/// clients; `insecure::client_endpoint` (feature `insecure-client`) is the
/// dev-only escape hatch for self-signed servers.
pub fn client_endpoint(alpn: &[u8]) -> io::Result<quinn::Endpoint> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![alpn.to_vec()];
    let quic_tls =
        quinn::crypto::rustls::QuicClientConfig::try_from(tls).map_err(io::Error::other)?;
    let mut endpoint = quinn::Endpoint::client(([0, 0, 0, 0], 0).into())?;
    let mut config = quinn::ClientConfig::new(Arc::new(quic_tls));
    // §3.4: QUIC keepalive substitutes for client PINGs.
    config.transport_config(Arc::new(transport_config(Some(Duration::from_secs(15)))));
    endpoint.set_default_client_config(config);
    Ok(endpoint)
}

/// The control stream of one QUIC connection: line-framed with the §4
/// 8 KiB cap enforced at the framing layer (a peer can't buffer-bloat us
/// with a newline-less flood).
pub struct QuicControlStream {
    framed: Framed<Join<RecvStream, SendStream>, LinesCodec>,
}

impl QuicControlStream {
    /// Server side: wait for the client to open the control stream.
    pub async fn accept(connection: &Connection) -> Result<Self, TransportError> {
        let (send, recv) = connection.accept_bi().await?;
        Ok(Self::new(recv, send))
    }

    /// Client side: open the control stream (also used by conformance tests).
    pub async fn open(connection: &Connection) -> Result<Self, TransportError> {
        let (send, recv) = connection.open_bi().await?;
        Ok(Self::new(recv, send))
    }

    fn new(recv: RecvStream, send: SendStream) -> Self {
        Self {
            framed: Framed::new(
                tokio::io::join(recv, send),
                LinesCodec::new_with_max_length(MAX_LINE_BYTES),
            ),
        }
    }

    pub async fn recv_line(&mut self) -> io::Result<Option<String>> {
        self.framed
            .next()
            .await
            .transpose()
            .map_err(io::Error::other)
    }

    pub async fn send_line(&mut self, line: &str) -> io::Result<()> {
        self.framed.send(line).await.map_err(io::Error::other)
    }

    /// Flush and FIN the send side. Without this, dropping the stream
    /// resets it and un-acked lines are lost.
    pub async fn finish(&mut self) -> io::Result<()> {
        SinkExt::<&str>::flush(&mut self.framed)
            .await
            .map_err(io::Error::other)?;
        let _ = self.framed.get_mut().writer_mut().finish(); // Err = already finished
        Ok(())
    }
}
