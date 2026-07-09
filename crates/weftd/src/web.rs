//! P3 web embed: serve the browser client and a **same-origin** `/ws` from the
//! existing `http`/`https` listener, so a browser loads the SPA from
//! `https://host/` and speaks WEFT back to `wss://host/ws` — one origin, one
//! port, TLS terminated by weftd.
//!
//! The `/ws` upgrade bridges straight into the ordinary [`weft_core::run_session`]
//! path (same as the standalone `[listen] ws` socket); it needs no extra
//! features. The SPA itself is embedded with `rust-embed` and only compiled in
//! under `--features web-ui`.

use std::io;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tracing::info;
use weft_core::{ControlStream, ServerCtx};

/// Mount the web routes onto the shared HTTP app: the `/ws` upgrade always, plus
/// the embedded SPA fallback when built with `--features web-ui`.
pub(crate) fn mount(app: Router, ctx: Arc<ServerCtx>) -> Router {
    // Build the `/ws` route with its own state, then merge — keeps the outer
    // app's state as `()` (wellknown/admin routes don't take `ServerCtx`).
    let ws = Router::new().route("/ws", get(ws_upgrade)).with_state(ctx);
    let app = app.merge(ws);
    info!("same-origin /ws mounted");
    #[cfg(feature = "web-ui")]
    let app = app.fallback(spa::serve);
    app
}

/// `GET /ws` → upgrade → run one WEFT session over the socket.
async fn ws_upgrade(State(ctx): State<Arc<ServerCtx>>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| async move {
        weft_core::run_session(AxumWsLines::new(socket), ctx).await;
    })
}

/// Adapter: an axum `WebSocket` as a core [`ControlStream`]. One text frame =
/// one control line, matching `weft_transport::WsControlStream` and the browser
/// client's per-line `send`.
///
/// The socket is `split` into sink/stream halves so the adapter is `Send + Sync`
/// (each half is a `BiLock`, which is `Sync` for a `Send` inner) — `run_session`
/// needs `Sync` (some `&self` handlers await), and the raw `WebSocket` is not.
struct AxumWsLines {
    tx: SplitSink<WebSocket, Message>,
    rx: SplitStream<WebSocket>,
}

impl AxumWsLines {
    fn new(socket: WebSocket) -> Self {
        let (tx, rx) = socket.split();
        Self { tx, rx }
    }
}

impl ControlStream for AxumWsLines {
    async fn recv_line(&mut self) -> io::Result<Option<String>> {
        loop {
            match self.rx.next().await {
                None => return Ok(None),
                Some(Ok(Message::Text(line))) => return Ok(Some(line)),
                Some(Ok(Message::Close(_))) => return Ok(None),
                // Binary = data plane (M6); ping/pong axum answers itself.
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err(io::Error::other(e)),
            }
        }
    }

    async fn send_line(&mut self, line: &str) -> io::Result<()> {
        self.tx
            .send(Message::Text(line.to_string()))
            .await
            .map_err(io::Error::other)
    }

    async fn close(&mut self) -> io::Result<()> {
        let _ = self.tx.close().await;
        Ok(())
    }
}

#[cfg(feature = "web-ui")]
mod spa {
    use axum::http::{header, StatusCode, Uri};
    use axum::response::{IntoResponse, Response};
    use rust_embed::RustEmbed;

    /// The built SvelteKit SPA (`pnpm build:web` → `client/build`, adapter-static
    /// with an `index.html` SPA fallback). Embedded at compile time.
    #[derive(RustEmbed)]
    #[folder = "../../client/build"]
    struct Assets;

    /// Fallback handler (specific routes — `/ws`, `/.well-known`, `/admin` — win
    /// first): serve the requested asset, else hand back `index.html` so the
    /// client router owns unknown paths.
    pub(super) async fn serve(uri: Uri) -> Response {
        let path = uri.path().trim_start_matches('/');
        let path = if path.is_empty() { "index.html" } else { path };
        if let Some(file) = Assets::get(path) {
            return ([(header::CONTENT_TYPE, mime_for(path))], file.data.into_owned())
                .into_response();
        }
        match Assets::get("index.html") {
            Some(index) => {
                ([(header::CONTENT_TYPE, "text/html")], index.data.into_owned()).into_response()
            }
            None => (StatusCode::NOT_FOUND, "web UI not built").into_response(),
        }
    }

    /// Content type by extension. `.wasm` MUST be `application/wasm` for the
    /// browser's streaming instantiation of the client core.
    fn mime_for(path: &str) -> &'static str {
        match path.rsplit('.').next() {
            Some("html") => "text/html",
            Some("js") => "text/javascript",
            Some("wasm") => "application/wasm",
            Some("css") => "text/css",
            Some("json") => "application/json",
            Some("svg") => "image/svg+xml",
            Some("png") => "image/png",
            Some("webp") => "image/webp",
            Some("ico") => "image/x-icon",
            Some("woff2") => "font/woff2",
            Some("woff") => "font/woff",
            Some("wav") => "audio/wav",
            Some("txt") => "text/plain",
            _ => "application/octet-stream",
        }
    }
}

/// Log line explaining the SPA state at boot (kept out of `mount` so the
/// `#[cfg]` split stays readable).
pub(crate) fn log_spa_state() {
    #[cfg(feature = "web-ui")]
    info!("web client (SPA) served at / (built with web-ui)");
    #[cfg(not(feature = "web-ui"))]
    tracing::warn!("[listen] web = true but built without --features web-ui: /ws only, no SPA");
}
