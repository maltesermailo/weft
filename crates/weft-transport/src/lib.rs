//! # weft-transport — WEFT transports (L2)
//!
//! QUIC (native, §3.1) and WebSocket (fallback, §3.2) framing for the
//! control plane. This crate turns byte streams into newline-delimited
//! lines and back — it never parses a verb (CLAUDE.md layering). The
//! `ControlStream` trait these types are adapted to lives in `weft-core`;
//! `weftd` provides the (trivial) adapters, keeping this crate's only
//! internal dependency `weft-proto`.

#![forbid(unsafe_code)]

mod quic;
mod ws;

pub use quic::{server_config, server_endpoint, QuicControlStream, ALPN};
pub use ws::WsControlStream;

use thiserror::Error;

/// Transport-level setup/handshake failures. I/O on established streams
/// surfaces as `std::io::Error` instead.
#[derive(Debug, Error)]
pub enum TransportError {
    #[error("TLS configuration: {0}")]
    Tls(#[from] rustls::Error),

    #[error("QUIC endpoint requires TLS 1.3 support in the crypto provider")]
    NoTls13,

    #[error("QUIC connection: {0}")]
    Quic(#[from] quinn::ConnectionError),

    #[error("WebSocket handshake: {0}")]
    WebSocket(Box<tokio_tungstenite::tungstenite::Error>), // boxed: 10× the other variants

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
