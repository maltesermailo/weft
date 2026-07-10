//! Accept loops + the transport→core adapters. This is the only place the
//! layering allows the two L2 crates to meet: weft-transport produces line
//! streams, weft-core consumes them via its `ControlStream` trait.

use std::io;
use std::sync::Arc;

use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tracing::{debug, info};
use weft_core::{ControlStream, ServerCtx};
use weft_transport::{QuicControlStream, WsControlStream};

/// Drain finished session tasks from a `JoinSet` without blocking (keeps it from
/// growing unbounded during normal operation).
fn reap(sessions: &mut JoinSet<()>) {
    while sessions.try_join_next().is_some() {}
}

/// After an accept loop stops (shutdown), wait for its in-flight sessions to
/// finish gracefully. Bounded by the caller's overall shutdown timeout.
async fn drain(mut sessions: JoinSet<()>, what: &str) {
    if sessions.is_empty() {
        return;
    }
    debug!(pending = sessions.len(), "{what}: draining sessions");
    while sessions.join_next().await.is_some() {}
}

/// Adapter: QUIC control stream as a core `ControlStream`. Also used by the
/// outbound dialer to hand an authenticated stream to `run_bridge_client`.
pub(crate) struct QuicLines(pub(crate) QuicControlStream);

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

/// §13 data plane: accept blob-transfer bidi streams on an established
/// connection (beyond the control stream) and hand each to the media handler.
/// One task per connection; aborted when its session ends.
async fn accept_data_plane(connection: quinn::Connection, ctx: Arc<ServerCtx>) {
    let mut transfers = JoinSet::new();
    // Ends when the connection closes / stops opening streams (Err from accept_bi).
    while let Ok((send, recv)) = connection.accept_bi().await {
        let ctx = Arc::clone(&ctx);
        transfers.spawn(crate::media::handle_data_stream(ctx, send, recv));
        while transfers.try_join_next().is_some() {}
    }
}

pub(crate) async fn accept_quic(endpoint: quinn::Endpoint, ctx: Arc<ServerCtx>) {
    let mut sessions = JoinSet::new();
    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break }; // endpoint closed
                let ctx = Arc::clone(&ctx);
                sessions.spawn(async move {
                    let connection = match incoming.await {
                        Ok(connection) => connection,
                        Err(e) => {
                            debug!("QUIC handshake failed: {e}");
                            return;
                        }
                    };
                    info!(peer = %connection.remote_address(), "QUIC connection");
                    // The client opens the control stream FIRST (§3.1 stream 0);
                    // accepting it here means the data-plane loop below only ever
                    // sees the *subsequent* bidi streams (§13 blob transfers).
                    match QuicControlStream::accept(&connection).await {
                        Ok(stream) => {
                            let data_plane =
                                tokio::spawn(accept_data_plane(connection.clone(), Arc::clone(&ctx)));
                            weft_core::run_session(QuicLines(stream), ctx).await;
                            data_plane.abort(); // session over ⇒ stop taking transfers
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
                reap(&mut sessions);
            }
            _ = ctx.shutdown.cancelled() => break,
        }
    }
    drain(sessions, "QUIC").await;
}

/// §17 WEFT-IRC gateway accept loop. `server_name` (this network's name)
/// prefixes server-originated IRC lines. TLS termination, if any, is the
/// operator's (a reverse proxy) — this listener is plaintext.
pub(crate) async fn accept_irc(listener: TcpListener, ctx: Arc<ServerCtx>, server_name: String) {
    let mut sessions = JoinSet::new();
    loop {
        let (tcp, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(e) => {
                    debug!("IRC accept error: {e}");
                    continue;
                }
            },
            _ = ctx.shutdown.cancelled() => break,
        };
        let ctx = Arc::clone(&ctx);
        let server_name = server_name.clone();
        sessions.spawn(async move {
            info!(%peer, "IRC connection");
            let _ = tcp.set_nodelay(true);
            weft_core::run_session(weft_irc::IrcStream::new(tcp, server_name), ctx).await;
        });
        reap(&mut sessions);
    }
    drain(sessions, "IRC").await;
}

pub(crate) async fn accept_ws(listener: TcpListener, ctx: Arc<ServerCtx>) {
    let mut sessions = JoinSet::new();
    loop {
        let (tcp, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(e) => {
                    debug!("WS accept error: {e}");
                    continue;
                }
            },
            _ = ctx.shutdown.cancelled() => break,
        };
        let ctx = Arc::clone(&ctx);
        sessions.spawn(async move {
            info!(%peer, "WebSocket connection");
            match WsControlStream::accept(tcp).await {
                Ok(stream) => weft_core::run_session(WsLines(stream), ctx).await,
                Err(e) => debug!("WS handshake failed: {e}"),
            }
        });
        reap(&mut sessions);
    }
    drain(sessions, "WS").await;
}
