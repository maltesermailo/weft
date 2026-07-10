//! Browser (WASM) binding for `weft-client-core`: a WebSocket-driven connection
//! loop + a JS event callback, exposing an `invoke(cmd, args)` surface that
//! mirrors the Tauri command set so the SvelteKit frontend switches backends by
//! swapping which `invoke` it calls. Built with wasm-pack; wasm32-only.
//!
//! Device- and namespace-key commands are stubbed here — they need browser key
//! storage (IndexedDB), which is the plan's P4; the password/register auth path
//! works today.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{CloseEvent, MessageEvent, WebSocket};
use weft_client_core as core;
use weft_client_core::{ClientEvent, EventSink, Mode, Phase};

/// Deliver a parsed event to the JS callback (serde → `JsValue`).
#[derive(Clone)]
struct JsSink(js_sys::Function);

impl EventSink for JsSink {
    fn emit(&self, event: ClientEvent) {
        if let Ok(js) = serde_wasm_bindgen::to_value(&event) {
            let _ = self.0.call1(&JsValue::NULL, &js);
        }
    }
}

/// §3.4 keepalive cadence. The server closes silent sessions at ~30s, so PING
/// well under that (matches the desktop client's 10s).
const KEEPALIVE_MS: i32 = 10_000;

/// Live connection state, shared across the WebSocket callbacks.
struct Conn {
    ws: WebSocket,
    phase: Phase,
    net_name: String,
    account: String,
    password: String,
    mode: Mode,
    in_batch: bool,
    buffered: Vec<String>,
    sink: JsSink,
    /// `setInterval` handle for the keepalive PING (0 = none); cleared on close.
    keepalive: i32,
}

/// Stop a keepalive interval (no-op for 0 / no window).
fn clear_keepalive(handle: i32) {
    if handle != 0 {
        if let Some(w) = web_sys::window() {
            w.clear_interval_with_handle(handle);
        }
    }
}

impl Conn {
    /// Run one inbound line through the shared FSM; send any reply, then flush
    /// buffered pre-READY commands the moment we reach READY.
    fn feed(&mut self, raw: &str) {
        let mut close = false;
        if let Some(out) = core::on_line(
            &self.sink,
            &self.account,
            &self.password,
            self.mode,
            None, // device-key auth is P4 (browser key storage)
            &mut self.net_name,
            &mut self.phase,
            &mut self.in_batch,
            &mut close,
            raw,
        ) {
            let _ = self.ws.send_with_str(&out);
        }
        if close {
            let _ = self.ws.close();
            return;
        }
        if self.phase == Phase::Ready && !self.buffered.is_empty() {
            for cmd in std::mem::take(&mut self.buffered) {
                let _ = self.ws.send_with_str(&cmd);
            }
        }
    }

    fn command(&mut self, line: String) {
        if self.phase == Phase::Ready {
            let _ = self.ws.send_with_str(&line);
        } else {
            self.buffered.push(line);
        }
    }
}

#[wasm_bindgen]
pub struct WeftClient {
    conn: Rc<RefCell<Option<Conn>>>,
    sink: JsSink,
    /// Closures must outlive the `WebSocket`; leaked into here for the session.
    _keep: Rc<RefCell<Vec<JsValue>>>,
}

#[wasm_bindgen]
impl WeftClient {
    #[wasm_bindgen(constructor)]
    pub fn new(on_event: js_sys::Function) -> WeftClient {
        WeftClient {
            conn: Rc::new(RefCell::new(None)),
            sink: JsSink(on_event),
            _keep: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// Structured entry point mirroring the Tauri `invoke` surface:
    /// `(cmd, args-object)` → build a wire line + enqueue, or a lifecycle action.
    pub fn invoke(&self, cmd: String, args: JsValue) -> Result<JsValue, JsValue> {
        let parsed: serde_json::Value =
            serde_wasm_bindgen::from_value(args).unwrap_or(serde_json::Value::Null);
        self.dispatch(&cmd, &parsed)
            .map_err(|e| JsValue::from_str(&e))
    }
}

impl WeftClient {
    fn dispatch(&self, cmd: &str, args: &serde_json::Value) -> Result<JsValue, String> {
        // Typed extractors over the JS args object: required string, optional
        // string, bool flag (default false), unsigned int (default 0).
        let arg = |k: &str| {
            args.get(k)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        let opt = |k: &str| args.get(k).and_then(|v| v.as_str()).map(str::to_string);
        let flag = |k: &str| args.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
        let num = |k: &str| args.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
        use core::*;

        let line = match cmd {
            // ---- lifecycle ----
            "connect" => {
                return self
                    .connect(&arg("host"), arg("account"), arg("password"), &arg("mode"))
                    .map(|_| JsValue::UNDEFINED);
            }
            "disconnect" => {
                let handle = self
                    .conn
                    .borrow()
                    .as_ref()
                    .map(|c| c.keepalive)
                    .unwrap_or(0);
                clear_keepalive(handle);
                *self.conn.borrow_mut() = None;
                return Ok(JsValue::UNDEFINED);
            }
            "client_config" => {
                let cfg = serde_json::json!({
                    "allow_insecure": false, "default_host": "",
                    "config_path": serde_json::Value::Null,
                });
                return serde_wasm_bindgen::to_value(&cfg).map_err(|e| e.to_string());
            }
            // ---- key-dependent — need browser key storage (P4) ----
            "enroll_device" | "has_device_key" | "ns_create" | "ns_transfer"
            | "ns_recovery_cancel" | "recovery_pubkey" | "recovery_start" | "recovery_cosign" => {
                return Err("device/namespace keys are not available in the web build yet".into());
            }
            // ---- relay ----
            "send_raw" => arg("line"),
            "join" => build_join(&arg("channel"))?,
            "part" => build_part(&arg("channel"))?,
            "send_message" => {
                let attachments = args
                    .get("attachments")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                build_msg(&arg("target"), &arg("body"), opt("replyTo"), attachments)?
            }
            "edit" => build_edit(&arg("msgid"), &arg("body"))?,
            "delete" => build_delete(&arg("msgid"))?,
            "react" => build_react(&arg("msgid"), &arg("emoji"), flag("add"))?,
            "history" => build_history(&arg("target"), opt("before"))?,
            "typing" => build_typing(&arg("channel"), flag("active"))?,
            "presence" => build_presence(&arg("status"))?,
            "mark" => build_mark(&arg("channel"), &arg("msgid"))?,
            "members" => build_members(&arg("channel"))?,
            "pin" => build_pin(&arg("msgid"), flag("pinned"))?,
            "pins" => build_pins(&arg("channel"))?,
            // ---- caps / roles ----
            "caps" => build_caps(&arg("account"), &arg("scope"))?,
            "grant" => build_grant(&arg("subject"), &arg("scope"), &arg("caps"))?,
            "revoke" => build_revoke(&arg("subject"), &arg("scope"), &arg("caps"))?,
            "roles" => build_roles(&arg("scope"))?,
            "role_create" => {
                build_role_create(&arg("scope"), &arg("color"), &arg("caps"), &arg("name"))?
            }
            "role_delete" => build_role_delete(&arg("scope"), &arg("name"))?,
            "role_assign" => build_role_assign(&arg("scope"), &arg("account"), &arg("name"))?,
            "role_unassign" => build_role_unassign(&arg("scope"), &arg("account"), &arg("name"))?,
            "roles_of" => build_roles_of(&arg("scope"), &arg("account"))?,
            // ---- channels ----
            "channel_create" => build_channel_create(&arg("channel"), opt("policy").as_deref())?,
            "channel_policy" => {
                build_channel_policy(&arg("channel"), &arg("policy"), flag("purge"))?
            }
            "channel_rename" => build_channel_rename(&arg("old"), &arg("new"))?,
            "channel_delete" => build_channel_delete(&arg("channel"))?,
            "channel_meta" => build_channel_meta(&arg("channel"), &arg("key"), &arg("value"))?,
            "channels" => build_channels(&arg("namespace"))?,
            "discover" => build_discover(opt("cursor"))?,
            // ---- namespaces ----
            "ns_join" => build_ns_join(&arg("name"))?,
            "ns_meta" => build_ns_meta(&arg("name"), &arg("key"), &arg("value"))?,
            "ns_visibility" => build_ns_visibility(&arg("name"), &arg("visibility"))?,
            "ns_delegate" => build_ns_delegate(&arg("name"), &arg("subject"), &arg("caps"))?,
            "ns_delete" => build_ns_delete(&arg("name"))?,
            "ns_recovery_set" => {
                build_ns_recovery_set(&arg("name"), num("m") as u32, &arg("keys"))?
            }
            "ns_recover" => build_ns_recover(&arg("name"), &arg("rotation"))?,
            "federate" => build_federate(&arg("target"))?,
            // ---- invites ----
            "invite_mint" => build_invite_mint(&arg("scope"))?,
            "invite_redeem" => build_invite_redeem(&arg("token"))?,
            "invite_revoke" => build_invite_revoke(&arg("inviteId"))?,
            // ---- moderation / reports ----
            "moderate" => build_moderation(
                &arg("verb"),
                &arg("scope"),
                &arg("account"),
                opt("reason").as_deref(),
            )?,
            "report" => build_report(&arg("msgid"), &arg("category"), &arg("scope"), opt("note"))?,
            "reports_list" => build_reports_list(&arg("scope"), opt("status"))?,
            "reports_resolve" => {
                build_reports_resolve(&arg("reportId"), &arg("action"), opt("note"))?
            }
            // ---- federation (operator) ----
            "netblock_add" => build_netblock_add(&arg("network"), opt("reason").as_deref())?,
            "netblock_remove" => build_netblock_remove(&arg("network"))?,
            "netblock_list" => build_netblock_list()?,
            "bridge_propose" => build_bridge_propose(
                &arg("scope"),
                &arg("peer"),
                &arg("history"),
                &arg("media"),
                flag("typing"),
            )?,
            "bridge_accept" => build_bridge_accept(&arg("peer"), num("version"))?,
            "bridge_sever" => build_bridge_sever(&arg("peer"))?,
            other => return Err(format!("unknown command {other}")),
        };
        match self.conn.borrow_mut().as_mut() {
            Some(c) => {
                c.command(line);
                Ok(JsValue::UNDEFINED)
            }
            None => Err("not connected".into()),
        }
    }

    fn connect(
        &self,
        host: &str,
        account: String,
        password: String,
        mode: &str,
    ) -> Result<(), String> {
        let mode = Mode::parse(mode)?;
        if mode == Mode::Key {
            return Err("device-key login is not available in the web build yet".into());
        }
        let url = ws_url(host)?;
        let ws = WebSocket::new(&url).map_err(|_| format!("cannot open WebSocket to {url}"))?;
        let password = core::password_or_default(&password);
        *self.conn.borrow_mut() = Some(Conn {
            ws: ws.clone(),
            phase: Phase::HelloSent,
            net_name: String::new(),
            account,
            password,
            mode,
            in_batch: false,
            buffered: Vec::new(),
            sink: self.sink.clone(),
            keepalive: 0,
        });

        // onopen → HELLO (start the §3.3 handshake)
        {
            let conn = self.conn.clone();
            let onopen = Closure::<dyn FnMut()>::new(move || {
                if let Some(c) = conn.borrow().as_ref() {
                    let _ = c.ws.send_with_str("HELLO weft/1");
                }
            });
            ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));
            self._keep.borrow_mut().push(onopen.into_js_value());
        }
        // onmessage → feed one control line (weftd WS carries one line per frame)
        {
            let conn = self.conn.clone();
            let onmessage = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
                if let Some(txt) = e.data().as_string() {
                    let line = txt.trim_end_matches(['\r', '\n']);
                    if !line.is_empty() {
                        if let Some(c) = conn.borrow_mut().as_mut() {
                            c.feed(line);
                        }
                    }
                }
            });
            ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
            self._keep.borrow_mut().push(onmessage.into_js_value());
        }
        // onclose → Closed (stop the keepalive timer first)
        {
            let conn = self.conn.clone();
            let sink = self.sink.clone();
            let onclose = Closure::<dyn FnMut(CloseEvent)>::new(move |_e: CloseEvent| {
                let handle = conn.borrow().as_ref().map(|c| c.keepalive).unwrap_or(0);
                clear_keepalive(handle);
                *conn.borrow_mut() = None;
                sink.emit(ClientEvent::Closed {
                    reason: "connection closed".into(),
                });
            });
            ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));
            self._keep.borrow_mut().push(onclose.into_js_value());
        }
        // §3.4 keepalive: PING on a cadence under the server's idle timeout so a
        // quiet session isn't dropped every 30s. The interval self-guards on the
        // socket being OPEN; onclose/disconnect clear it.
        {
            let conn = self.conn.clone();
            let ping = Closure::<dyn FnMut()>::new(move || {
                if let Some(c) = conn.borrow().as_ref() {
                    if c.ws.ready_state() == WebSocket::OPEN {
                        let _ = c.ws.send_with_str("PING keepalive");
                    }
                }
            });
            let handle = web_sys::window()
                .and_then(|w| {
                    w.set_interval_with_callback_and_timeout_and_arguments_0(
                        ping.as_ref().unchecked_ref(),
                        KEEPALIVE_MS,
                    )
                    .ok()
                })
                .unwrap_or(0);
            if let Some(c) = self.conn.borrow_mut().as_mut() {
                c.keepalive = handle;
            }
            self._keep.borrow_mut().push(ping.into_js_value());
        }
        Ok(())
    }
}

/// Resolve the session's WebSocket URL.
///
/// The web client is served *by* the weft network it talks to (P3 embed), so by
/// default the URL is derived **same-origin** from `window.location`: the origin
/// is the network (never `127.0.0.1`), and the scheme tracks the page — `wss`
/// under HTTPS, `ws` under HTTP (so no mixed-content block, no TLS-vs-plaintext
/// mismatch). An explicit `ws(s)://…` `host` is honored verbatim as a
/// cross-origin override.
fn ws_url(host: &str) -> Result<String, String> {
    let host = host.trim();
    if host.starts_with("ws://") || host.starts_with("wss://") {
        return Ok(host.to_string());
    }
    let location = web_sys::window()
        .ok_or("no window object (not a browser?)")?
        .location();
    let scheme = match location.protocol().as_deref() {
        Ok("https:") => "wss",
        _ => "ws",
    };
    let authority = location
        .host()
        .map_err(|_| "cannot read window.location.host".to_string())?;
    Ok(format!("{scheme}://{authority}/ws"))
}
