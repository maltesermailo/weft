//! Tauri glue: managed connection state + the commands the Svelte frontend
//! invokes. All WEFT protocol logic lives in [`weft`].

mod config;
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
    let parsed_mode = weft::Mode::parse(&mode)?;
    let (addr, server_name) = weft::resolve(&host).await?;
    let password = weft::password_or_default(&password);

    // Device-key login loads the stored keypair up front (secret never leaves
    // the backend); a missing key fails clearly.
    let device = if parsed_mode == weft::Mode::Key {
        Some(keys::load_device(&app, &host, &account).ok_or_else(|| {
            "no device key enrolled on this device — log in with a password and enroll first"
                .to_string()
        })?)
    } else {
        None
    };

    let allow_insecure = config::load(&app).allow_insecure;

    let (tx, rx) = mpsc::unbounded_channel();
    *conn.tx.lock().unwrap() = Some(tx);
    tauri::async_runtime::spawn(weft::run_connection(
        app,
        addr,
        server_name,
        account,
        password,
        parsed_mode,
        device,
        allow_insecure,
        rx,
    ));
    Ok(())
}

/// The active client config (TLS mode + prefill host) + where the file lives,
/// so the UI can show whether it's in secure or insecure mode.
#[tauri::command]
fn client_config(app: AppHandle) -> serde_json::Value {
    let cfg = config::load(&app);
    serde_json::json!({
        "allow_insecure": cfg.allow_insecure,
        "default_host": cfg.default_host,
        "config_path": config::path(&app).map(|p| p.display().to_string()),
    })
}

/// Generate + enroll a device key for `(host, account)` while authed, so the
/// next launch can log in passwordless. Sends `AUTH ENROLL` over the connection.
#[tauri::command]
fn enroll_device(
    app: AppHandle,
    conn: State<'_, Conn>,
    host: String,
    account: String,
) -> Result<(), String> {
    let pubkey = keys::enroll_device(&app, &host, &account)?;
    conn.send(weft::build_auth_enroll(&pubkey)?)
}

/// Is a device key enrolled locally for `(host, account)`?
#[tauri::command]
fn has_device_key(app: AppHandle, host: String, account: String) -> bool {
    keys::has_device(&app, &host, &account)
}

/// Drop the outbound sender — the connection task then finishes and closes the
/// stream. Used for logout / switch-account.
#[tauri::command]
fn disconnect(conn: State<'_, Conn>) {
    *conn.tx.lock().unwrap() = None;
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
fn federate(conn: State<'_, Conn>, target: String) -> Result<(), String> {
    conn.send(weft::build_federate(&target)?)
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
    let sig = weft_crypto::signature_to_b64(&weft_crypto::sign_transfer(&root, &name, &new_owner));
    conn.send(weft::build_ns_transfer(&name, &new_owner, &sig)?)
}

/// §2.4 the quorum member's recovery pubkey — share it with the owner to be
/// included in `NS RECOVERY SET`. Generated + stored on first call.
#[tauri::command]
fn recovery_pubkey(app: AppHandle, network: String, name: String) -> Result<String, String> {
    Ok(keys::recovery_key(&app, &network, &name)?.public().to_b64())
}

/// §2.4 start a recovery: mint a fresh root key (held locally by the initiator,
/// the new owner), build a rotation record, and sign it with our recovery key.
/// Returns the b64 record to pass to the other quorum members for co-signing.
#[tauri::command]
fn recovery_start(
    app: AppHandle,
    network: String,
    name: String,
    new_owner: String,
) -> Result<String, String> {
    let recovery = keys::recovery_key(&app, &network, &name)?;
    let new_root = weft_crypto::Keypair::generate();
    keys::store_ns_key(&app, &network, &name, &new_root.seed_b64())?;
    let record = weft_crypto::RotationRecord {
        namespace: name,
        new_root_key: new_root.public(),
        new_owner,
    };
    let sig = record.sign(&recovery);
    let signed = weft_crypto::SignedRotation {
        record,
        signatures: vec![sig],
    };
    Ok(signed.to_b64())
}

/// §2.4 add our recovery signature to an in-progress rotation record.
#[tauri::command]
fn recovery_cosign(
    app: AppHandle,
    network: String,
    name: String,
    rotation: String,
) -> Result<String, String> {
    let recovery = keys::recovery_key(&app, &network, &name)?;
    let mut signed = weft_crypto::SignedRotation::from_b64(&rotation)
        .map_err(|_| "bad rotation record".to_string())?;
    let sig = signed.record.sign(&recovery);
    signed.signatures.push(sig);
    Ok(signed.to_b64())
}

/// §2.4 submit a co-signed rotation to the server (`NS RECOVER`).
#[tauri::command]
fn ns_recover(conn: State<'_, Conn>, name: String, rotation: String) -> Result<(), String> {
    conn.send(weft::build_ns_recover(&name, &rotation)?)
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
fn grant(
    conn: State<'_, Conn>,
    subject: String,
    scope: String,
    caps: String,
) -> Result<(), String> {
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
fn invite_revoke(conn: State<'_, Conn>, invite_id: String) -> Result<(), String> {
    conn.send(weft::build_invite_revoke(&invite_id)?)
}

#[tauri::command]
fn moderate(
    conn: State<'_, Conn>,
    verb: String,
    scope: String,
    account: String,
    reason: Option<String>,
) -> Result<(), String> {
    conn.send(weft::build_moderation(&verb, &scope, &account, reason.as_deref())?)
}

// ---- federation (operator) ----
#[tauri::command]
fn netblock_add(conn: State<'_, Conn>, network: String, reason: Option<String>) -> Result<(), String> {
    conn.send(weft::build_netblock_add(&network, reason.as_deref())?)
}

#[tauri::command]
fn netblock_remove(conn: State<'_, Conn>, network: String) -> Result<(), String> {
    conn.send(weft::build_netblock_remove(&network)?)
}

#[tauri::command]
fn netblock_list(conn: State<'_, Conn>) -> Result<(), String> {
    conn.send(weft::build_netblock_list()?)
}

#[tauri::command]
fn bridge_propose(
    conn: State<'_, Conn>,
    scope: String,
    peer: String,
    history: String,
    media: String,
    typing: bool,
) -> Result<(), String> {
    conn.send(weft::build_bridge_propose(&scope, &peer, &history, &media, typing)?)
}

#[tauri::command]
fn bridge_accept(conn: State<'_, Conn>, peer: String, version: u64) -> Result<(), String> {
    conn.send(weft::build_bridge_accept(&peer, version)?)
}

#[tauri::command]
fn bridge_sever(conn: State<'_, Conn>, peer: String) -> Result<(), String> {
    conn.send(weft::build_bridge_sever(&peer)?)
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
fn reports_list(
    conn: State<'_, Conn>,
    scope: String,
    status: Option<String>,
) -> Result<(), String> {
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
fn pin(conn: State<'_, Conn>, msgid: String, pinned: bool) -> Result<(), String> {
    conn.send(weft::build_pin(&msgid, pinned)?)
}

#[tauri::command]
fn pins(conn: State<'_, Conn>, channel: String) -> Result<(), String> {
    conn.send(weft::build_pins(&channel)?)
}

#[tauri::command]
fn caps(conn: State<'_, Conn>, account: String, scope: String) -> Result<(), String> {
    conn.send(weft::build_caps(&account, &scope)?)
}

/// §6.6 named roles.
#[tauri::command]
fn roles(conn: State<'_, Conn>, scope: String) -> Result<(), String> {
    conn.send(weft::build_roles(&scope)?)
}

#[tauri::command]
fn role_create(
    conn: State<'_, Conn>,
    scope: String,
    color: String,
    caps: String,
    name: String,
) -> Result<(), String> {
    conn.send(weft::build_role_create(&scope, &color, &caps, &name)?)
}

#[tauri::command]
fn role_delete(conn: State<'_, Conn>, scope: String, name: String) -> Result<(), String> {
    conn.send(weft::build_role_delete(&scope, &name)?)
}

#[tauri::command]
fn role_assign(
    conn: State<'_, Conn>,
    scope: String,
    account: String,
    name: String,
) -> Result<(), String> {
    conn.send(weft::build_role_assign(&scope, &account, &name)?)
}

#[tauri::command]
fn role_unassign(
    conn: State<'_, Conn>,
    scope: String,
    account: String,
    name: String,
) -> Result<(), String> {
    conn.send(weft::build_role_unassign(&scope, &account, &name)?)
}

#[tauri::command]
fn roles_of(conn: State<'_, Conn>, scope: String, account: String) -> Result<(), String> {
    conn.send(weft::build_roles_of(&scope, &account)?)
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
fn channel_create(
    conn: State<'_, Conn>,
    channel: String,
    policy: Option<String>,
) -> Result<(), String> {
    conn.send(weft::build_channel_create(&channel, policy.as_deref())?)
}

#[tauri::command]
fn channel_policy(
    conn: State<'_, Conn>,
    channel: String,
    policy: String,
    purge: bool,
) -> Result<(), String> {
    conn.send(weft::build_channel_policy(&channel, &policy, purge)?)
}

#[tauri::command]
fn channel_rename(conn: State<'_, Conn>, old: String, new: String) -> Result<(), String> {
    conn.send(weft::build_channel_rename(&old, &new)?)
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
        .plugin(tauri_plugin_notification::init())
        .manage(Conn::default())
        .invoke_handler(tauri::generate_handler![
            connect,
            client_config,
            disconnect,
            enroll_device,
            has_device_key,
            join,
            ns_join,
            ns_create,
            ns_meta,
            federate,
            ns_visibility,
            ns_delegate,
            ns_delete,
            ns_recovery_set,
            ns_transfer,
            ns_recovery_cancel,
            recovery_pubkey,
            recovery_start,
            recovery_cosign,
            ns_recover,
            roles,
            role_create,
            role_delete,
            role_assign,
            role_unassign,
            roles_of,
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
            invite_revoke,
            moderate,
            netblock_add,
            netblock_remove,
            netblock_list,
            bridge_propose,
            bridge_accept,
            bridge_sever,
            report,
            reports_list,
            reports_resolve,
            members,
            pin,
            pins,
            caps,
            part,
            channel_create,
            channel_policy,
            channel_rename,
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
