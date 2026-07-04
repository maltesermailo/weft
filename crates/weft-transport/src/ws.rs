//! WebSocket fallback (§3.2): one text frame = one control line, so the
//! WS layer already provides framing. Binary frames (data plane with
//! 4-byte virtual stream IDs) come with media in M6 and are ignored here.

use std::io;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use weft_proto::MAX_LINE_BYTES;

use crate::TransportError;

/// One WebSocket control connection, generic over the underlying byte
/// stream (plain TCP in M1 dev; TLS termination is the deployment's job
/// until the HTTPS surface lands in M2).
pub struct WsControlStream<S> {
    inner: WebSocketStream<S>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> WsControlStream<S> {
    /// Server side of the WS handshake, with frame sizes capped near the
    /// §4 line limit.
    pub async fn accept(stream: S) -> Result<Self, TransportError> {
        let config = WebSocketConfig {
            max_message_size: Some(MAX_LINE_BYTES * 2),
            max_frame_size: Some(MAX_LINE_BYTES * 2),
            ..WebSocketConfig::default()
        };
        let inner = tokio_tungstenite::accept_async_with_config(stream, Some(config))
            .await
            .map_err(|e| TransportError::WebSocket(Box::new(e)))?;
        Ok(Self { inner })
    }

    pub async fn recv_line(&mut self) -> io::Result<Option<String>> {
        loop {
            match self.inner.next().await {
                None => return Ok(None),
                Some(Ok(Message::Text(line))) => return Ok(Some(line)),
                Some(Ok(Message::Close(_))) => return Ok(None),
                // Binary = data plane (M6); ping/pong is answered by
                // tungstenite itself. Neither carries control lines.
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err(io::Error::other(e)),
            }
        }
    }

    pub async fn send_line(&mut self, line: &str) -> io::Result<()> {
        self.inner
            .send(Message::Text(line.to_string()))
            .await
            .map_err(io::Error::other)
    }

    /// Flush pending frames and send the WS close handshake.
    pub async fn close(&mut self) -> io::Result<()> {
        self.inner.close(None).await.map_err(io::Error::other)
    }
}
