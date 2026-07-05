//! The WEFT connection bridge: one QUIC control connection to a weftd,
//! driving the §3.3 handshake (HELLO → AUTH, auto-registering an unknown
//! account like weft-tui) and relaying between the frontend and the server.
//!
//! Inbound server lines are parsed and re-emitted to the webview as
//! structured `weft` events; outbound command lines from the frontend are
//! buffered until the session is READY, then written to the stream.

use std::net::SocketAddr;

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;
use weft_proto::{Command, Event, MsgId, Reply, Request, Target};
use weft_transport::QuicControlStream;

/// Default credential when the user leaves the password blank (≥12 bytes per
/// §6.1) — a dev convenience, mirroring weft-tui.
const DEFAULT_PASSWORD: &str = "weft-client-dev-pw";

/// Which credential flow the connect screen requested (§6.1).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// AUTH PASSWORD against an existing account; a failure is surfaced, never
    /// silently turned into a registration.
    Login,
    /// REGISTER a new account (which doubles as authentication).
    Register,
}

impl Mode {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "login" => Ok(Mode::Login),
            "register" => Ok(Mode::Register),
            other => Err(format!("unknown mode {other:?}")),
        }
    }
}

/// Structured events pushed to the webview under the `weft` channel.
#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum WeftEvent {
    Connected {
        network: String,
        account: String,
    },
    /// Login/registration failed — the connect screen stays up with `reason`.
    AuthFailed {
        reason: String,
    },
    Message {
        target: String,
        sender: String,
        network: String,
        msgid: String,
        body: String,
        own: bool,
        /// True when this arrived inside a `HISTORY` batch (older messages to
        /// prepend), false for live traffic to append.
        history: bool,
        /// Batch form: the message already carries collapsed edits.
        edited: bool,
        /// `reply-to=` — the msgid this replies to (§9.3), if any.
        reply_to: Option<String>,
        /// `fmt=md` — render the body as markdown (§9.4).
        md: bool,
    },
    /// `TYPING <#chan> start|stop` from another member (§7).
    Typing {
        channel: String,
        user: String,
        state: String,
    },
    /// `PRESENCE <user> <status>` from a shared-channel member (§7).
    Presence {
        user: String,
        status: String,
    },
    /// `BATCH START` — a `HISTORY` page begins (§7).
    BatchStart {
        id: String,
    },
    /// `BATCH END` — page done; `truncated` marks a retention gap (§6.4).
    BatchEnd {
        id: String,
        truncated: bool,
    },
    Member {
        channel: String,
        user: String,
        network: String,
        action: String,
        count: Option<u64>,
    },
    Policy {
        channel: String,
        policy: String,
    },
    Edited {
        target: String,
        sender: String,
        /// The original message this edit replaces (§7 `edit-of=`).
        edit_of: String,
        body: String,
    },
    Deleted {
        target: String,
        msgid: String,
    },
    /// §7 a live reaction add/remove.
    Reaction {
        target: String,
        msgid: String,
        emoji: String,
        op: String,
        by: String,
    },
    /// §12.1 a compacted reaction summary (from history batches).
    Reactions {
        target: String,
        msgid: String,
        emoji: String,
        count: u64,
        by: Vec<String>,
    },
    /// §6.7 a moderation action (mute/ban/kick) landed.
    Moderated {
        scope: String,
        account: String,
        action: String,
        by: Option<String>,
        reason: Option<String>,
    },
    Error {
        code: String,
        text: String,
    },
    Closed {
        reason: String,
    },
    /// Anything not specially modelled — surfaced for debugging.
    Raw {
        line: String,
    },
}

fn emit(app: &AppHandle, event: WeftEvent) {
    let _ = app.emit("weft", event);
}

/// Resolve `host:port` to a socket address plus the server name for QUIC SNI.
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
    mut outbound: mpsc::UnboundedReceiver<String>,
) {
    let mut stream = match connect(addr, &server_name).await {
        Ok(stream) => stream,
        Err(e) => return emit(&app, WeftEvent::Closed { reason: e }),
    };

    let mut phase = Phase::HelloSent;
    // Whether we're inside a HISTORY BATCH (messages then are older history).
    let mut in_batch = false;
    // Frontend commands that arrive before READY wait here.
    let mut buffered: Vec<String> = Vec::new();

    if send(&mut stream, &app, "HELLO weft/1").await.is_err() {
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
                if send(&mut stream, &app, "PING keepalive").await.is_err() {
                    return;
                }
            }
            line = stream.recv_line() => match line {
                Ok(Some(raw)) => {
                    let mut close = false;
                    if let Some(out) = on_line(&app, &account, &password, mode, &mut phase, &mut in_batch, &mut close, &raw) {
                        if send(&mut stream, &app, &out).await.is_err() { return; }
                    }
                    if close {
                        // Auth failed — tear down; the connect screen retries.
                        let _ = stream.finish().await;
                        return;
                    }
                    // Flush buffered commands the moment we reach READY.
                    if phase == Phase::Ready && !buffered.is_empty() {
                        for cmd in std::mem::take(&mut buffered) {
                            if send(&mut stream, &app, &cmd).await.is_err() { return; }
                        }
                    }
                }
                Ok(None) => return emit(&app, WeftEvent::Closed { reason: "server closed the connection".into() }),
                Err(e) => return emit(&app, WeftEvent::Closed { reason: format!("connection lost: {e}") }),
            },
            cmd = outbound.recv() => match cmd {
                Some(cmd) if phase == Phase::Ready => {
                    if send(&mut stream, &app, &cmd).await.is_err() { return; }
                }
                Some(cmd) => buffered.push(cmd), // not yet authed
                None => { let _ = stream.finish().await; return; } // app gone
            },
        }
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Phase {
    HelloSent,
    AuthSent,
    Ready,
}

/// Process one inbound line: advance the handshake, emit structured events,
/// and return an outbound line to send in response (if any).
#[allow(clippy::too_many_arguments)]
fn on_line(
    app: &AppHandle,
    account: &str,
    password: &str,
    mode: Mode,
    phase: &mut Phase,
    in_batch: &mut bool,
    close: &mut bool,
    raw: &str,
) -> Option<String> {
    let reply = match Reply::parse(raw) {
        Ok(reply) => reply,
        Err(_) => {
            emit(
                app,
                WeftEvent::Raw {
                    line: raw.to_string(),
                },
            );
            return None;
        }
    };
    // §3.3 handshake progression — the auth verb depends on the chosen mode.
    match (*phase, &reply.event) {
        (Phase::HelloSent, Event::Welcome { .. }) => {
            *phase = Phase::AuthSent;
            return Some(match mode {
                Mode::Login => format!("AUTH PASSWORD {account} :{password}"),
                Mode::Register => format!("REGISTER {account} :{password}"),
            });
        }
        (Phase::AuthSent, Event::Welcome { network, .. }) => {
            *phase = Phase::Ready;
            emit(
                app,
                WeftEvent::Connected {
                    network: network.to_string(),
                    account: account.to_string(),
                },
            );
            return None;
        }
        // Login/registration rejected — surface a friendly reason and close.
        (Phase::AuthSent, Event::Err(e)) => {
            let reason = match (mode, e.code) {
                (Mode::Login, weft_proto::ErrCode::AuthFailed) => {
                    "authentication failed — check the account name and password".to_string()
                }
                (Mode::Register, weft_proto::ErrCode::Conflict) => {
                    "that account name is already taken".to_string()
                }
                _ => e.text.clone(),
            };
            emit(app, WeftEvent::AuthFailed { reason });
            *close = true;
            return None;
        }
        _ => {}
    }
    // Steady-state events → structured pushes.
    match reply.event {
        // §7 HISTORY framing — toggle the batch flag so the messages between
        // are tagged as older history for the frontend to prepend.
        Event::BatchStart { id } => {
            *in_batch = true;
            emit(app, WeftEvent::BatchStart { id });
        }
        Event::BatchEnd { id, truncated, .. } => {
            *in_batch = false;
            emit(app, WeftEvent::BatchEnd { id, truncated });
        }
        Event::Message(m) => emit(
            app,
            WeftEvent::Message {
                target: m.target.to_string(),
                sender: m.sender.account.to_string(),
                network: m.sender.network.to_string(),
                msgid: m.msgid.to_string(),
                own: m.sender.account.as_str() == account,
                history: *in_batch,
                edited: m.edited.is_some(),
                reply_to: m.meta.reply_to.as_ref().map(|r| r.to_string()),
                md: m.meta.fmt.as_deref() == Some("md"),
                body: m.body,
            },
        ),
        Event::Member {
            channel,
            user,
            action,
            count,
            ..
        } => emit(
            app,
            WeftEvent::Member {
                channel: channel.to_string(),
                user: user.account.to_string(),
                network: user.network.to_string(),
                action: action.to_string(),
                count,
            },
        ),
        Event::Policy { channel, policy } => emit(
            app,
            WeftEvent::Policy {
                channel: channel.to_string(),
                policy: policy.to_string(),
            },
        ),
        Event::Typing {
            channel,
            user,
            state,
        } => emit(
            app,
            WeftEvent::Typing {
                channel: channel.to_string(),
                user: user.account.to_string(),
                state: state.to_string(),
            },
        ),
        Event::Presence { user, status } => emit(
            app,
            WeftEvent::Presence {
                user: user.account.to_string(),
                status: status.to_string(),
            },
        ),
        Event::Edited {
            target,
            user,
            edit_of,
            body,
            ..
        } => emit(
            app,
            WeftEvent::Edited {
                target: target.to_string(),
                sender: user.account.to_string(),
                edit_of: edit_of.to_string(),
                body,
            },
        ),
        Event::Deleted { target, msgid, .. } => emit(
            app,
            WeftEvent::Deleted {
                target: target.to_string(),
                msgid: msgid.to_string(),
            },
        ),
        Event::Reaction {
            target,
            msgid,
            emoji,
            op,
            by,
        } => emit(
            app,
            WeftEvent::Reaction {
                target: target.to_string(),
                msgid: msgid.to_string(),
                emoji,
                op: op.to_string(),
                by: by.account.to_string(),
            },
        ),
        Event::Reactions {
            target,
            msgid,
            emoji,
            count,
            by,
        } => emit(
            app,
            WeftEvent::Reactions {
                target: target.to_string(),
                msgid: msgid.to_string(),
                emoji,
                count,
                by: by.iter().map(|u| u.account.to_string()).collect(),
            },
        ),
        Event::Moderated {
            scope,
            account,
            action,
            by,
            reason,
        } => emit(
            app,
            WeftEvent::Moderated {
                scope,
                account: account.to_string(),
                action: action.to_string(),
                by: by.map(|a| a.to_string()),
                reason,
            },
        ),
        Event::Err(e) => emit(
            app,
            WeftEvent::Error {
                code: e.code.to_string(),
                text: e.text,
            },
        ),
        // Keepalive answers are internal — never shown.
        Event::Pong { .. } => {}
        // Batches, reactions, presence, etc. — surfaced raw for now.
        _ => emit(
            app,
            WeftEvent::Raw {
                line: raw.to_string(),
            },
        ),
    }
    None
}

async fn send(stream: &mut QuicControlStream, app: &AppHandle, line: &str) -> Result<(), ()> {
    match stream.send_line(line).await {
        Ok(()) => Ok(()),
        Err(e) => {
            emit(
                app,
                WeftEvent::Closed {
                    reason: format!("send failed: {e}"),
                },
            );
            Err(())
        }
    }
}

async fn connect(addr: SocketAddr, server_name: &str) -> Result<QuicControlStream, String> {
    let endpoint = weft_transport::insecure::client_endpoint(weft_transport::ALPN)
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

/// The default password used when the user leaves it blank.
pub fn password_or_default(password: &str) -> String {
    if password.is_empty() {
        DEFAULT_PASSWORD.to_string()
    } else {
        password.to_string()
    }
}

/// Build a WEFT command line for the frontend's high-level intents, validated
/// through the proto codec so we never emit something our own parser rejects.
pub fn build_msg(target: &str, body: &str, reply_to: Option<String>) -> Result<String, String> {
    let target: Target = target.parse().map_err(|_| "bad target".to_string())?;
    let reply_to = match reply_to.filter(|r| !r.is_empty()) {
        Some(r) => Some(r.parse::<MsgId>().map_err(|_| "bad reply-to msgid".to_string())?),
        None => None,
    };
    let meta = weft_proto::MsgMeta {
        // The client composes in markdown; tag it so peers render it (§9.4).
        fmt: Some("md".to_string()),
        reply_to,
        ..Default::default()
    };
    weft_proto::Request::new(weft_proto::Command::Msg {
        target,
        body: Some(body.to_string()),
        meta,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `PRESENCE <status>` — set own status (§6.1). `invisible` renders offline.
pub fn build_presence(status: &str) -> Result<String, String> {
    let status: weft_proto::PresenceStatus =
        status.parse().map_err(|_| "bad presence status".to_string())?;
    Request::new(Command::Presence { status })
        .serialize()
        .map_err(|e| e.to_string())
}

/// `TYPING <#chan> start|stop` (§6.3).
pub fn build_typing(channel: &str, active: bool) -> Result<String, String> {
    let channel: weft_proto::ChannelName =
        channel.parse().map_err(|_| "bad channel".to_string())?;
    let state = if active {
        weft_proto::TypingState::Start
    } else {
        weft_proto::TypingState::Stop
    };
    Request::new(Command::Typing { channel, state })
        .serialize()
        .map_err(|e| e.to_string())
}

pub fn build_join(channel: &str) -> Result<String, String> {
    let channel: weft_proto::ChannelName =
        channel.parse().map_err(|_| "bad channel".to_string())?;
    weft_proto::Request::new(weft_proto::Command::Join {
        channel,
        invite: None,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `EDIT <msgid> :<body>` — replace an own message's text (§6.4).
pub fn build_edit(msgid: &str, body: &str) -> Result<String, String> {
    let msgid: MsgId = msgid.parse().map_err(|_| "bad msgid".to_string())?;
    Request::new(Command::Edit {
        msgid,
        body: body.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `DELETE <msgid>` — tombstone an own message (§6.4).
pub fn build_delete(msgid: &str) -> Result<String, String> {
    let msgid: MsgId = msgid.parse().map_err(|_| "bad msgid".to_string())?;
    Request::new(Command::Delete { msgid })
        .serialize()
        .map_err(|e| e.to_string())
}

/// `REACT`/`UNREACT <msgid> <emoji>` — toggle a reaction (§6.4, idempotent).
pub fn build_react(msgid: &str, emoji: &str, add: bool) -> Result<String, String> {
    let msgid: MsgId = msgid.parse().map_err(|_| "bad msgid".to_string())?;
    let emoji = emoji.to_string();
    let cmd = if add {
        Command::React { msgid, emoji }
    } else {
        Command::Unreact { msgid, emoji }
    };
    Request::new(cmd).serialize().map_err(|e| e.to_string())
}

/// `HISTORY <target> [before=] limit=50` — a backfill page (§6.4). `before` is
/// the oldest msgid already held, for scroll-up paging.
pub fn build_history(target: &str, before: Option<String>) -> Result<String, String> {
    let target: Target = target.parse().map_err(|_| "bad target".to_string())?;
    let before = match before.filter(|b| !b.is_empty()) {
        Some(b) => Some(b.parse::<MsgId>().map_err(|_| "bad before msgid".to_string())?),
        None => None,
    };
    Request::new(Command::History {
        target,
        before,
        after: None,
        limit: Some(50),
        thread: None,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `NS JOIN <name>` — auto-join every visible channel in the namespace (§6.2).
pub fn build_ns_join(name: &str) -> Result<String, String> {
    let name: weft_proto::NamespaceName =
        name.parse().map_err(|_| "bad namespace name".to_string())?;
    weft_proto::Request::new(weft_proto::Command::NsJoin { name })
        .serialize()
        .map_err(|e| e.to_string())
}
