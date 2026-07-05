//! Accept loops + the transport→core adapters. This is the only place the
//! layering allows the two L2 crates to meet: weft-transport produces line
//! streams, weft-core consumes them via its `ControlStream` trait.

use std::io;
use std::sync::Arc;

use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info};
use weft_core::{ControlStream, ServerCtx};
use weft_transport::{QuicControlStream, WsControlStream};

/// Adapter: QUIC control stream as a core `ControlStream`.
struct QuicLines(QuicControlStream);

impl ControlStream for QuicLines {
    async fn recv_line(&mut self) -> io::Result<Option<String>> {
        self.0.recv_line().await
    }

    async fn send_line(&mut self, line: &str) -> io::Result<()> {
        self.0.send_line(line).await
    }

    async fn close(&mut self) -> io::Result<()> {
        self.0.finish().await
    }
}

/// Adapter: WebSocket control stream as a core `ControlStream`.
struct WsLines(WsControlStream<TcpStream>);

impl ControlStream for WsLines {
    async fn recv_line(&mut self) -> io::Result<Option<String>> {
        self.0.recv_line().await
    }

    async fn send_line(&mut self, line: &str) -> io::Result<()> {
        self.0.send_line(line).await
    }

    async fn close(&mut self) -> io::Result<()> {
        self.0.close().await
    }
}

pub(crate) async fn accept_quic(endpoint: quinn::Endpoint, ctx: Arc<ServerCtx>) {
    while let Some(incoming) = endpoint.accept().await {
        let ctx = Arc::clone(&ctx);
        tokio::spawn(async move {
            let connection = match incoming.await {
                Ok(connection) => connection,
                Err(e) => {
                    debug!("QUIC handshake failed: {e}");
                    return;
                }
            };
            info!(peer = %connection.remote_address(), "QUIC connection");
            // The client opens the control stream (§3.1 stream 0).
            match QuicControlStream::accept(&connection).await {
                Ok(stream) => {
                    weft_core::run_session(QuicLines(stream), ctx).await;
                    // The session finished its stream; give the peer a
                    // moment to receive everything and close first —
                    // `Connection::close` abandons un-acked stream data.
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_secs(3),
                        connection.closed(),
                    )
                    .await;
                    connection.close(0u32.into(), b"session ended");
                }
                Err(e) => debug!("no control stream: {e}"),
            }
        });
    }
}

/// §17 WEFT-IRC gateway accept loop. `server_name` (this network's name)
/// prefixes server-originated IRC lines. TLS termination, if any, is the
/// operator's (a reverse proxy) — this listener is plaintext.
pub(crate) async fn accept_irc(listener: TcpListener, ctx: Arc<ServerCtx>, server_name: String) {
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                debug!("IRC accept error: {e}");
                continue;
            }
        };
        let ctx = Arc::clone(&ctx);
        let server_name = server_name.clone();
        tokio::spawn(async move {
            info!(%peer, "IRC connection");
            let _ = tcp.set_nodelay(true);
            weft_core::run_session(weft_irc::IrcStream::new(tcp, server_name), ctx).await;
        });
    }
}

pub(crate) async fn accept_ws(listener: TcpListener, ctx: Arc<ServerCtx>) {
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                debug!("WS accept error: {e}");
                continue;
            }
        };
        let ctx = Arc::clone(&ctx);
        tokio::spawn(async move {
            info!(%peer, "WebSocket connection");
            match WsControlStream::accept(tcp).await {
                Ok(stream) => weft_core::run_session(WsLines(stream), ctx).await,
                Err(e) => debug!("WS handshake failed: {e}"),
            }
        });
    }
}
