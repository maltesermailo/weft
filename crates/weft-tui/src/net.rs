//! Connection task: owns the QUIC control stream, pumps inbound lines to
//! the app, drains the outbound queue, and keeps the session alive with a
//! periodic PING (§3.4 — the server closes silent connections).

use std::net::SocketAddr;
use std::time::Duration;

use tokio::sync::mpsc;
use weft_transport::QuicControlStream;

use crate::app::AppEvent;

#[derive(Debug)]
pub enum NetEvent {
    Connected,
    Line(String),
    Closed(String),
}

const KEEPALIVE: Duration = Duration::from_secs(10);

pub async fn task(
    addr: SocketAddr,
    server_name: String,
    mut outbound: mpsc::UnboundedReceiver<String>,
    events: mpsc::UnboundedSender<AppEvent>,
) {
    let send = |ev: NetEvent| events.send(AppEvent::Net(ev)).is_ok();
    let mut stream = match connect(addr, &server_name).await {
        Ok(stream) => stream,
        Err(e) => {
            send(NetEvent::Closed(format!("connect failed: {e}")));
            return;
        }
    };
    send(NetEvent::Connected);

    let mut keepalive = tokio::time::interval(KEEPALIVE);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    keepalive.tick().await; // first tick fires immediately — skip it

    loop {
        tokio::select! {
            line = stream.recv_line() => match line {
                Ok(Some(line)) => {
                    if !send(NetEvent::Line(line)) {
                        return; // UI gone
                    }
                }
                Ok(None) => {
                    send(NetEvent::Closed("server closed the connection".to_string()));
                    return;
                }
                Err(e) => {
                    send(NetEvent::Closed(format!("connection lost: {e}")));
                    return;
                }
            },
            line = outbound.recv() => match line {
                Some(line) => {
                    if let Err(e) = stream.send_line(&line).await {
                        send(NetEvent::Closed(format!("send failed: {e}")));
                        return;
                    }
                }
                None => { // UI gone: finish so a trailing QUIT is delivered
                    let _ = stream.finish().await;
                    return;
                }
            },
            _ = keepalive.tick() => {
                if stream.send_line("PING keepalive").await.is_err() {
                    send(NetEvent::Closed("connection lost".to_string()));
                    return;
                }
            }
        }
    }
}

/// Endpoint + connection must outlive the stream; hold them in statics-free
/// style by leaking nothing: quinn keeps a connection alive while streams
/// exist, but we keep handles anyway for explicit lifetime.
async fn connect(addr: SocketAddr, server_name: &str) -> anyhow::Result<QuicControlStream> {
    let endpoint = weft_transport::insecure::client_endpoint(weft_transport::ALPN)?;
    let connection = endpoint.connect(addr, server_name)?.await?;
    let stream = QuicControlStream::open(&connection).await?;
    // Keep the connection driving in the background for the process
    // lifetime of this task's stream.
    tokio::spawn(async move {
        let _endpoint = endpoint;
        connection.closed().await;
    });
    Ok(stream)
}
