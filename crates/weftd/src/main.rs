//! weftd entry point: `weftd [config.toml]` — defaults to a localhost dev
//! network with `#general` when no config is given.

use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    weftd::telemetry::init();

    let config = match std::env::args().nth(1) {
        Some(path) => weftd::config::load(path)?,
        None => weftd::Config::default(),
    };

    let server = weftd::start(config).await?;
    info!(quic = %server.quic_addr, ws = ?server.ws_addr, http = ?server.http_addr, "weftd listening");

    tokio::signal::ctrl_c().await?;
    info!("shutting down");
    server.shutdown().await;
    Ok(())
}
