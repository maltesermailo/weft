//! QUIC control plane (§3.1): ALPN `weft/1`, the first bidi stream is
//! stream 0 = newline-delimited UTF-8 control lines. Uni streams
//! (data plane) come with media in M6.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use quinn::crypto::rustls::QuicServerConfig;
use quinn::{Connection, RecvStream, SendStream};
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::Join;
use tokio_util::codec::{Framed, LinesCodec};
use weft_proto::MAX_LINE_BYTES;

use crate::TransportError;

/// §3.1: the only ALPN this server offers; mismatching clients fail the
/// TLS handshake before a single line is read.
pub const ALPN: &[u8] = b"weft/1";

/// Build the QUIC server config: TLS with the given identity, ALPN pinned.
pub fn server_config(
    cert_chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<quinn::ServerConfig, TransportError> {
    let mut tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let quic_tls = QuicServerConfig::try_from(tls).map_err(|_| TransportError::NoTls13)?;
    Ok(quinn::ServerConfig::with_crypto(Arc::new(quic_tls)))
}

/// Bind a listening endpoint.
pub fn server_endpoint(
    config: quinn::ServerConfig,
    addr: SocketAddr,
) -> io::Result<quinn::Endpoint> {
    quinn::Endpoint::server(config, addr)
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
