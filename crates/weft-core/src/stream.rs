//! The transport-facing port. weft-core never touches sockets (CLAUDE.md
//! layering); transports produce lines, sessions consume them through this
//! trait. Implemented in `weftd` for the QUIC and WebSocket streams, and by
//! an in-memory mock in tests — the whole domain layer runs networkless.

use std::future::Future;
use std::io;

/// One control-plane connection: newline-framed UTF-8 lines (spec §3.1
/// stream 0 / §3.2 text frames). Line *content* is opaque here — parsing
/// belongs to `weft-proto`, framing to the transport.
pub trait ControlStream: Send {
    /// Next line, without its terminator. `Ok(None)` = peer closed cleanly.
    fn recv_line(&mut self) -> impl Future<Output = io::Result<Option<String>>> + Send;

    /// Write one line; the transport adds framing/terminator.
    fn send_line(&mut self, line: &str) -> impl Future<Output = io::Result<()>> + Send;

    /// Graceful shutdown: make already-written lines (e.g. a final `ERR`
    /// before close, §3.6) deliverable before the transport tears down.
    /// Default: nothing to do.
    fn close(&mut self) -> impl Future<Output = io::Result<()>> + Send {
        async { Ok(()) }
    }

    /// Whether this is a protocol-gateway front-end (e.g. WEFT-IRC) that
    /// auto-registers accounts and can't supply a contact email. Such sessions
    /// are exempt from the network's `require_email` policy (§6.1); the default
    /// is a native client, which is subject to it.
    fn is_gateway(&self) -> bool {
        false
    }
}
