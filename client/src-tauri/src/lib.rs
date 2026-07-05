//! Tauri glue: managed connection state + the commands the Svelte frontend
//! invokes. All WEFT protocol logic lives in [`weft`].

mod keys;
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

/// Join every visible channel in a namespace (§6.2 `NS JOIN`).
#[tauri::command]
fn ns_join(conn: State<'_, Conn>, name: String) -> Result<(), String> {
    conn.send(weft::build_ns_join(&name)?)
}

/// Create a namespace (§6.2). Generates the root keypair locally (secret stays
/// on this device), submits only the public key via `@root=`.
#[tauri::command]
fn ns_create(
    app: AppHandle,
    conn: State<'_, Conn>,
    network: String,
    name: String,
    visibility: String,
) -> Result<(), String> {
    let root_key = keys::generate_ns_key(&app, &network, &name)?;
    conn.send(weft::build_ns_create(&name, &visibility, &root_key)?)
}

#[tauri::command]
fn ns_meta(conn: State<'_, Conn>, name: String, key: String, value: String) -> Result<(), String> {
    conn.send(weft::build_ns_meta(&name, &key, &value)?)
}

#[tauri::command]
fn ns_visibility(conn: State<'_, Conn>, name: String, visibility: String) -> Result<(), String> {
    conn.send(weft::build_ns_visibility(&name, &visibility)?)
}

#[tauri::command]
fn ns_delegate(
    conn: State<'_, Conn>,
    name: String,
    subject: String,
    caps: String,
) -> Result<(), String> {
    conn.send(weft::build_ns_delegate(&name, &subject, &caps)?)
}

#[tauri::command]
fn ns_delete(conn: State<'_, Conn>, name: String) -> Result<(), String> {
    conn.send(weft::build_ns_delete(&name)?)
}

#[tauri::command]
fn ns_recovery_set(
    conn: State<'_, Conn>,
    name: String,
    m: u32,
    keys: String,
) -> Result<(), String> {
    conn.send(weft::build_ns_recovery_set(&name, m, &keys)?)
}

/// §2.4 root-signed succession — loads the stored root key and signs the
/// transfer locally; the secret never leaves this process.
#[tauri::command]
fn ns_transfer(
    app: AppHandle,
    conn: State<'_, Conn>,
    network: String,
    name: String,
    new_owner: String,
) -> Result<(), String> {
    let root = keys::load_ns_key(&app, &network, &name)?;
    let sig = weft_crypto::signature_to_b64(&weft_crypto::sign_transfer(
        &root, &name, &new_owner,
    ));
    conn.send(weft::build_ns_transfer(&name, &new_owner, &sig)?)
}

/// §2.4 root veto of a pending recovery — root-signed locally.
#[tauri::command]
fn ns_recovery_cancel(
    app: AppHandle,
    conn: State<'_, Conn>,
    network: String,
    name: String,
) -> Result<(), String> {
    let root = keys::load_ns_key(&app, &network, &name)?;
    let sig = weft_crypto::signature_to_b64(&weft_crypto::sign_cancel(&root, &name));
    conn.send(weft::build_ns_recovery_cancel(&name, &sig)?)
}

/// Request a page of history for `target`, older than `before` if given (§6.4).
#[tauri::command]
fn history(conn: State<'_, Conn>, target: String, before: Option<String>) -> Result<(), String> {
    conn.send(weft::build_history(&target, before)?)
}

#[tauri::command]
fn edit(conn: State<'_, Conn>, msgid: String, body: String) -> Result<(), String> {
    conn.send(weft::build_edit(&msgid, &body)?)
}

#[tauri::command]
fn delete(conn: State<'_, Conn>, msgid: String) -> Result<(), String> {
    conn.send(weft::build_delete(&msgid)?)
}

#[tauri::command]
fn react(conn: State<'_, Conn>, msgid: String, emoji: String, add: bool) -> Result<(), String> {
    conn.send(weft::build_react(&msgid, &emoji, add)?)
}

#[tauri::command]
fn send_message(
    conn: State<'_, Conn>,
    target: String,
    body: String,
    reply_to: Option<String>,
) -> Result<(), String> {
    conn.send(weft::build_msg(&target, &body, reply_to)?)
}

#[tauri::command]
fn typing(conn: State<'_, Conn>, channel: String, active: bool) -> Result<(), String> {
    conn.send(weft::build_typing(&channel, active)?)
}

#[tauri::command]
fn presence(conn: State<'_, Conn>, status: String) -> Result<(), String> {
    conn.send(weft::build_presence(&status)?)
}

#[tauri::command]
fn mark(conn: State<'_, Conn>, channel: String, msgid: String) -> Result<(), String> {
    conn.send(weft::build_mark(&channel, &msgid)?)
}

#[tauri::command]
fn grant(conn: State<'_, Conn>, subject: String, scope: String, caps: String) -> Result<(), String> {
    conn.send(weft::build_grant(&subject, &scope, &caps)?)
}

#[tauri::command]
fn revoke(
    conn: State<'_, Conn>,
    subject: String,
    scope: String,
    caps: String,
) -> Result<(), String> {
    conn.send(weft::build_revoke(&subject, &scope, &caps)?)
}

#[tauri::command]
fn invite_mint(conn: State<'_, Conn>, scope: String) -> Result<(), String> {
    conn.send(weft::build_invite_mint(&scope)?)
}

#[tauri::command]
fn invite_redeem(conn: State<'_, Conn>, token: String) -> Result<(), String> {
    conn.send(weft::build_invite_redeem(&token)?)
}

#[tauri::command]
fn report(
    conn: State<'_, Conn>,
    msgid: String,
    category: String,
    scope: String,
    note: Option<String>,
) -> Result<(), String> {
    conn.send(weft::build_report(&msgid, &category, &scope, note)?)
}

#[tauri::command]
fn reports_list(conn: State<'_, Conn>, scope: String, status: Option<String>) -> Result<(), String> {
    conn.send(weft::build_reports_list(&scope, status)?)
}

#[tauri::command]
fn reports_resolve(
    conn: State<'_, Conn>,
    report_id: String,
    action: String,
    note: Option<String>,
) -> Result<(), String> {
    conn.send(weft::build_reports_resolve(&report_id, &action, note)?)
}

#[tauri::command]
fn members(conn: State<'_, Conn>, channel: String) -> Result<(), String> {
    conn.send(weft::build_members(&channel)?)
}

#[tauri::command]
fn part(conn: State<'_, Conn>, channel: String) -> Result<(), String> {
    conn.send(weft::build_part(&channel)?)
}

#[tauri::command]
fn channel_create(conn: State<'_, Conn>, channel: String) -> Result<(), String> {
    conn.send(weft::build_channel_create(&channel)?)
}

#[tauri::command]
fn channel_delete(conn: State<'_, Conn>, channel: String) -> Result<(), String> {
    conn.send(weft::build_channel_delete(&channel)?)
}

#[tauri::command]
fn channel_meta(
    conn: State<'_, Conn>,
    channel: String,
    key: String,
    value: String,
) -> Result<(), String> {
    conn.send(weft::build_channel_meta(&channel, &key, &value)?)
}

#[tauri::command]
fn discover(conn: State<'_, Conn>, cursor: Option<String>) -> Result<(), String> {
    conn.send(weft::build_discover(cursor)?)
}

#[tauri::command]
fn channels(conn: State<'_, Conn>, namespace: String) -> Result<(), String> {
    conn.send(weft::build_channels(&namespace)?)
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
            ns_join,
            ns_create,
            ns_meta,
            ns_visibility,
            ns_delegate,
            ns_delete,
            ns_recovery_set,
            ns_transfer,
            ns_recovery_cancel,
            history,
            edit,
            delete,
            react,
            typing,
            presence,
            mark,
            grant,
            revoke,
            invite_mint,
            invite_redeem,
            report,
            reports_list,
            reports_resolve,
            members,
            part,
            channel_create,
            channel_delete,
            channel_meta,
            discover,
            channels,
            send_message,
            send_raw
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
