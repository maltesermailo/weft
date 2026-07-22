//! weftd entry point: `weftd [config.toml]` — defaults to a localhost dev
//! network with `#general` when no config is given.

use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // `weftd admin …` — operator-management CLI (§10.4); no server, quiet output.
    if args.get(1).map(String::as_str) == Some("admin") {
        return weftd::admin_cli::run(&args[2..]).await;
    }

    weftd::telemetry::init();

    let config = match args.get(1) {
        Some(path) => weftd::config::load(path)?,
        None => weftd::Config::default(),
    };

    let server = weftd::start(config).await?;
    info!(quic = %server.quic_addr, ws = ?server.ws_addr, http = ?server.http_addr, "weftd listening");

    wait_for_shutdown_signal().await;
    info!("shutdown signal received");
    server.shutdown().await;
    Ok(())
}

/// Resolve on SIGINT (Ctrl-C) or, on Unix, SIGTERM (`systemctl stop`, `docker
/// stop`) — so orchestrated stops trigger the same graceful shutdown.
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                // Fall back to Ctrl-C only if we can't register SIGTERM.
                tracing::warn!("cannot listen for SIGTERM ({e}); SIGINT only");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
