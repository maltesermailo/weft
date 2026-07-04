//! weft-tui — terminal test client for weftd.
//!
//! ```text
//! weft-tui [host:port] [account] [#channel]
//! ```
//! Connects over QUIC (trusting any cert — M1 dev servers are self-signed),
//! performs HELLO/AUTH automatically, then behaves like a minimal IRC
//! client with a raw-wire toggle (Ctrl+R) and a `/raw` escape hatch.

mod app;
mod net;
mod ui;

use std::time::Duration;

use anyhow::Context;
use app::{App, AppEvent};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!("usage: weft-tui [host:port] [account] [#channel] [password]");
        eprintln!("defaults: 127.0.0.1:4433, guest<pid>, no auto-join, dev password");
        eprintln!("(existing account with another password: pass it, or /login in-app)");
        return Ok(());
    }
    let target = args.first().map(String::as_str).unwrap_or("127.0.0.1:4433");
    let account = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| format!("guest{}", std::process::id()));
    account
        .parse::<weft_proto::Account>()
        .context("invalid account name (lowercase a-z 0-9 - _ .)")?;
    let autojoin = args.get(2).cloned();
    let password = args.get(3).cloned().filter(|p| !p.is_empty());

    let (host, _) = target
        .rsplit_once(':')
        .context("target must be host:port")?;
    let addr = tokio::net::lookup_host(target)
        .await
        .with_context(|| format!("resolving {target}"))?
        .next()
        .with_context(|| format!("no address for {target}"))?;

    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    tokio::spawn(net::task(addr, host.to_string(), out_rx, ev_tx.clone()));
    spawn_input_thread(ev_tx);

    let mut terminal = ratatui::init();
    // ratatui::init doesn't grab the mouse; we want wheel scrolling.
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
    let result = run(
        &mut terminal,
        App::new(account, password, autojoin, out_tx),
        ev_rx,
    )
    .await;
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    ratatui::restore();
    // Give the net task a beat to flush the trailing QUIT.
    tokio::time::sleep(Duration::from_millis(150)).await;
    result
}

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    mut app: App,
    mut events: mpsc::UnboundedReceiver<AppEvent>,
) -> anyhow::Result<()> {
    terminal.draw(|frame| ui::render(frame, &mut app))?;
    while let Some(event) = events.recv().await {
        app.on_event(event);
        // Coalesce bursts (message floods, key repeats) into one redraw.
        while !app.quit {
            match events.try_recv() {
                Ok(event) => app.on_event(event),
                Err(_) => break,
            }
        }
        terminal.draw(|frame| ui::render(frame, &mut app))?;
        if app.quit {
            break;
        }
    }
    Ok(())
}

/// Terminal input is blocking; a dedicated thread feeds it into the async
/// loop. The thread dies with the process after `ratatui::restore()`.
fn spawn_input_thread(tx: mpsc::UnboundedSender<AppEvent>) {
    std::thread::spawn(move || loop {
        match crossterm::event::read() {
            Ok(event) => {
                if tx.send(AppEvent::Term(event)).is_err() {
                    return;
                }
            }
            Err(_) => return,
        }
    });
}
