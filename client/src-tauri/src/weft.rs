//! Native (Tauri + QUIC) binding for `weft-client-core`: the connection loop,
//! the QUIC transport, DNS resolution, and a Tauri `EventSink`. The wire codec,
//! the auth FSM, and the command builders live in the portable core (re-exported
//! below so `lib.rs`'s `weft::build_*` / `weft::Mode` references are unchanged).

use std::net::SocketAddr;

use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;
use weft_crypto::Keypair;
use weft_transport::QuicControlStream;

pub use weft_client_core::*;

/// A Tauri `EventSink`: pushes each `ClientEvent` to the webview on the `weft`
/// channel.
struct TauriSink(AppHandle);

impl EventSink for TauriSink {
    fn emit(&self, event: ClientEvent) {
        let _ = self.0.emit("weft", event);
    }
}

pub async fn resolve(host: &str) -> Result<(SocketAddr, String), String> {
    let (name, _) = host.rsplit_once(':').ok_or("target must be host:port")?;
    let addr = tokio::net::lookup_host(host)
        .await
        .map_err(|e| format!("resolving {host}: {e}"))?
        .next()
        .ok_or_else(|| format!("no address for {host}"))?;
    Ok((addr, name.to_string()))
}

/// Drive one connection to completion. Emits `Connected` once authed, then
/// relays until the stream closes or the app drops the outbound sender.
#[allow(clippy::too_many_arguments)]
pub async fn run_connection(
    app: AppHandle,
    addr: SocketAddr,
    server_name: String,
    account: String,
    password: String,
    mode: Mode,
    device: Option<Keypair>,
    allow_insecure: bool,
    mut outbound: mpsc::UnboundedReceiver<String>,
) {
    let sink = TauriSink(app);
    let mut stream = match connect(addr, &server_name, allow_insecure).await {
        Ok(stream) => stream,
        Err(e) => return sink.emit(ClientEvent::Closed { reason: e }),
    };

    let mut phase = Phase::HelloSent;
    // Whether we're inside a HISTORY BATCH (messages then are older history).
    let mut in_batch = false;
    // The network name, captured from the first WELCOME — needed to sign the
    // device-key challenge (`nonce ‖ network`, §6.1).
    let mut net_name = String::new();
    // Frontend commands that arrive before READY wait here.
    let mut buffered: Vec<String> = Vec::new();

    if send(&mut stream, &sink, "HELLO weft/1").await.is_err() {
        return;
    }

    // §3.4 keepalive: the server closes silent sessions (~30 s) and QUIC has
    // its own idle timeout, so PING on a cadence well under both.
    let mut keepalive = tokio::time::interval(std::time::Duration::from_secs(10));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    keepalive.tick().await; // the first tick fires immediately — skip it

    loop {
        tokio::select! {
            _ = keepalive.tick() => {
                if send(&mut stream, &sink, "PING keepalive").await.is_err() {
                    return;
                }
            }
            line = stream.recv_line() => match line {
                Ok(Some(raw)) => {
                    let mut close = false;
                    if let Some(out) = on_line(&sink, &account, &password, mode, device.as_ref(), &mut net_name, &mut phase, &mut in_batch, &mut close, &raw) {
                        if send(&mut stream, &sink, &out).await.is_err() { return; }
                    }
                    if close {
                        // Auth failed — tear down; the connect screen retries.
                        let _ = stream.finish().await;
                        return;
                    }
                    // Flush buffered commands the moment we reach READY.
                    if phase == Phase::Ready && !buffered.is_empty() {
                        for cmd in std::mem::take(&mut buffered) {
                            if send(&mut stream, &sink, &cmd).await.is_err() { return; }
                        }
                    }
                }
                Ok(None) => return sink.emit(ClientEvent::Closed { reason: "server closed the connection".into() }),
                Err(e) => return sink.emit(ClientEvent::Closed { reason: format!("connection lost: {e}") }),
            },
            cmd = outbound.recv() => match cmd {
                Some(cmd) if phase == Phase::Ready => {
                    if send(&mut stream, &sink, &cmd).await.is_err() { return; }
                }
                Some(cmd) => buffered.push(cmd), // not yet authed
                None => { let _ = stream.finish().await; return; } // app gone
            },
        }
    }
}

async fn send(stream: &mut QuicControlStream, sink: &TauriSink, line: &str) -> Result<(), ()> {
    match stream.send_line(line).await {
        Ok(()) => Ok(()),
        Err(e) => {
            sink.emit(ClientEvent::Closed {
                    reason: format!("send failed: {e}"),
                });
            Err(())
        }
    }
}


async fn connect(
    addr: SocketAddr,
    server_name: &str,
    allow_insecure: bool,
) -> Result<QuicControlStream, String> {
    // Verified by default (bundled Mozilla roots); `allow_insecure` opts into the
    // cert-blind endpoint for dev / self-signed servers (client.toml).
    let endpoint = if allow_insecure {
        weft_transport::insecure::client_endpoint(weft_transport::ALPN)
    } else {
        weft_transport::client_endpoint(weft_transport::ALPN)
    }
    .map_err(|e| format!("endpoint: {e}"))?;
    let connection = endpoint
        .connect(addr, server_name)
        .map_err(|e| format!("connect: {e}"))?
        .await
        .map_err(|e| format!("handshake: {e}"))?;
    let stream = QuicControlStream::open(&connection)
        .await
        .map_err(|e| format!("control stream: {e}"))?;
    // Keep the connection (and endpoint) alive for the stream's lifetime.
    tokio::spawn(async move {
        let _endpoint = endpoint;
        connection.closed().await;
    });
    Ok(stream)
}

