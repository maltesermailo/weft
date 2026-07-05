//! Tauri glue: managed connection state + the commands the Svelte frontend
//! invokes. All WEFT protocol logic lives in [`weft`].

mod weft;

use std::sync::Mutex;

use tauri::{AppHandle, State};
use tokio::sync::mpsc;

/// The active connection's outbound command channel (None until connected).
#[derive(Default)]
struct Conn {
    tx: Mutex<Option<mpsc::UnboundedSender<String>>>,
}

impl Conn {
    fn send(&self, line: String) -> Result<(), String> {
        self.tx
            .lock()
            .unwrap()
            .as_ref()
            .ok_or_else(|| "not connected".to_string())?
            .send(line)
            .map_err(|_| "connection closed".to_string())
    }
}

/// Open a connection to `host` (`host:port`) as `account`. Replaces any
/// existing connection. Progress arrives as `weft` events (`connected`, …).
#[tauri::command]
async fn connect(
    app: AppHandle,
    conn: State<'_, Conn>,
    host: String,
    account: String,
    password: String,
    mode: String,
) -> Result<(), String> {
    account
        .parse::<weft_proto::Account>()
        .map_err(|_| "invalid account name (a-z 0-9 - _ .)".to_string())?;
    let mode = weft::Mode::parse(&mode)?;
    let (addr, server_name) = weft::resolve(&host).await?;
    let password = weft::password_or_default(&password);

    let (tx, rx) = mpsc::unbounded_channel();
    *conn.tx.lock().unwrap() = Some(tx);
    tauri::async_runtime::spawn(weft::run_connection(
        app,
        addr,
        server_name,
        account,
        password,
        mode,
        rx,
    ));
    Ok(())
}

#[tauri::command]
fn join(conn: State<'_, Conn>, channel: String) -> Result<(), String> {
    conn.send(weft::build_join(&channel)?)
}

#[tauri::command]
fn send_message(conn: State<'_, Conn>, target: String, body: String) -> Result<(), String> {
    conn.send(weft::build_msg(&target, &body)?)
}

/// Escape hatch — send a raw wire line (netcat-debuggable control plane).
#[tauri::command]
fn send_raw(conn: State<'_, Conn>, line: String) -> Result<(), String> {
    conn.send(line)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(Conn::default())
        .invoke_handler(tauri::generate_handler![
            connect,
            join,
            send_message,
            send_raw
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
