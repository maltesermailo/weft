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
    /// `MARKED <#chan> <msgid>` — read-marker sync across own devices (§9.7).
    Marked {
        channel: String,
        msgid: String,
    },
    /// `CHANMETA <#chan> <key> <value>` — topic / posting / … (§7).
    Chanmeta {
        channel: String,
        key: String,
        value: String,
    },
    /// `NS-META` — a namespace descriptor (DISCOVER result / ns update, §7).
    NsMeta {
        name: String,
        visibility: String,
        owner: Option<String>,
        title: Option<String>,
        description: Option<String>,
        /// §2.4 recovery ladder announcement fields.
        recovery_set: bool,
        recovery_eta: Option<u64>,
        recovery_rung: Option<u8>,
    },
    /// `CHANNEL-LAYOUT <#chan> <position>` with optional `category=` (§7).
    ChannelLayout {
        channel: String,
        category: Option<String>,
        position: i64,
    },
    /// `MORE <cursor>` — DISCOVER pagination continuation (§7).
    More {
        cursor: String,
    },
    /// `TOKEN <subject> <scope>` — a GRANT/REVOKE landed (§7).
    Token {
        subject: String,
        scope: String,
    },
    /// `INVITED <scope> <invite-id> :<link>` — a freshly minted invite (§7).
    Invited {
        scope: String,
        invite_id: String,
        link: Option<String>,
    },
    /// `REPORTED <report-id>` — ack to the reporter (§7).
    Reported {
        report_id: String,
    },
    /// `REPORT-FILED …` — a queue entry for `reports` holders (§7).
    ReportFiled {
        report_id: String,
        msgid: String,
        category: String,
        state: String,
        scope: String,
        reporter: Option<String>,
    },
    /// `REPORT-RESOLVED <report-id> <action>` (§7).
    ReportResolved {
        report_id: String,
        action: String,
        note: Option<String>,
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
        Event::Marked { channel, msgid } => emit(
            app,
            WeftEvent::Marked {
                channel: channel.to_string(),
                msgid: msgid.to_string(),
            },
        ),
        Event::Chanmeta {
            channel,
            key,
            value,
        } => emit(
            app,
            WeftEvent::Chanmeta {
                channel: channel.to_string(),
                key,
                value,
            },
        ),
        Event::NsMeta {
            name,
            visibility,
            owner,
            title,
            description,
            recovery_set,
            recovery_pending,
            ..
        } => emit(
            app,
            WeftEvent::NsMeta {
                name: name.to_string(),
                visibility: visibility.to_string(),
                owner,
                title,
                description,
                recovery_set,
                recovery_eta: recovery_pending.map(|(eta, _)| eta),
                recovery_rung: recovery_pending.map(|(_, rung)| rung),
            },
        ),
        Event::ChannelLayout {
            channel,
            category,
            position,
        } => emit(
            app,
            WeftEvent::ChannelLayout {
                channel: channel.to_string(),
                category,
                position,
            },
        ),
        Event::More { cursor } => emit(app, WeftEvent::More { cursor }),
        Event::Token { subject, scope, .. } => emit(app, WeftEvent::Token { subject, scope }),
        Event::Invited {
            scope,
            invite_id,
            link,
            ..
        } => emit(
            app,
            WeftEvent::Invited {
                scope,
                invite_id,
                link,
            },
        ),
        Event::Reported { report_id } => emit(app, WeftEvent::Reported { report_id }),
        Event::ReportFiled {
            report_id,
            msgid,
            category,
            state,
            scope,
            reporter,
        } => emit(
            app,
            WeftEvent::ReportFiled {
                report_id,
                msgid: msgid.to_string(),
                category,
                state: state.to_string(),
                scope: scope.to_string(),
                reporter,
            },
        ),
        Event::ReportResolved {
            report_id,
            action,
            note,
            ..
        } => emit(
            app,
            WeftEvent::ReportResolved {
                report_id,
                action: action.to_string(),
                note,
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
        Some(r) => Some(
            r.parse::<MsgId>()
                .map_err(|_| "bad reply-to msgid".to_string())?,
        ),
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
    let status: weft_proto::PresenceStatus = status
        .parse()
        .map_err(|_| "bad presence status".to_string())?;
    Request::new(Command::Presence { status })
        .serialize()
        .map_err(|e| e.to_string())
}

/// `GRANT <subject> <scope> <caps> [expiry=]` — delegate capabilities (§6.5).
pub fn build_grant(subject: &str, scope: &str, caps: &str) -> Result<String, String> {
    Request::new(Command::Grant {
        subject: subject.to_string(),
        scope: scope.to_string(),
        caps: caps.to_string(),
        expiry: None,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `REVOKE <subject> <scope> [caps=]` — withdraw capabilities (§6.5).
pub fn build_revoke(subject: &str, scope: &str, caps: &str) -> Result<String, String> {
    Request::new(Command::Revoke {
        subject: subject.to_string(),
        scope: scope.to_string(),
        caps: (!caps.is_empty()).then(|| caps.to_string()),
        epoch: None,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `INVITE MINT <scope>` — shareable invite for a channel/namespace (§6.5).
pub fn build_invite_mint(scope: &str) -> Result<String, String> {
    Request::new(Command::InviteMint {
        scope: scope.to_string(),
        max_uses: None,
        expiry: None,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `INVITE REDEEM <b64>` — redeem an invite token (§6.5).
pub fn build_invite_redeem(token: &str) -> Result<String, String> {
    // Accept a full `weft://<net>/i/<b64>` link or a bare token.
    let token = token.rsplit('/').next().unwrap_or(token).to_string();
    Request::new(Command::InviteRedeem { token })
        .serialize()
        .map_err(|e| e.to_string())
}

/// `REPORT <msgid> <category> [scope] [:note]` — flag a message (§6.7).
pub fn build_report(
    msgid: &str,
    category: &str,
    scope: &str,
    note: Option<String>,
) -> Result<String, String> {
    let msgid: MsgId = msgid.parse().map_err(|_| "bad msgid".to_string())?;
    let scope: weft_proto::ReportScope = scope.parse().map_err(|_| "bad scope".to_string())?;
    Request::new(Command::Report {
        msgid,
        category: category.to_string(),
        scope,
        note: note.filter(|n| !n.is_empty()),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `REPORTS LIST <scope> [status=]` — the handler queue (§6.7).
pub fn build_reports_list(scope: &str, status: Option<String>) -> Result<String, String> {
    let status = match status.filter(|s| !s.is_empty()) {
        Some(s) => Some(s.parse().map_err(|_| "bad status".to_string())?),
        None => None,
    };
    Request::new(Command::ReportsList {
        scope: scope.to_string(),
        status,
        cursor: None,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `REPORTS RESOLVE <report-id> <action> [:note]` (§6.7).
pub fn build_reports_resolve(
    report_id: &str,
    action: &str,
    note: Option<String>,
) -> Result<String, String> {
    let action: weft_proto::ResolveAction = action.parse().map_err(|_| "bad action".to_string())?;
    Request::new(Command::ReportsResolve {
        report_id: report_id.to_string(),
        action,
        note: note.filter(|n| !n.is_empty()),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `MARK <#chan> <msgid>` — read marker, synced across own devices (§6.3).
pub fn build_mark(channel: &str, msgid: &str) -> Result<String, String> {
    let channel: weft_proto::ChannelName =
        channel.parse().map_err(|_| "bad channel".to_string())?;
    let msgid: MsgId = msgid.parse().map_err(|_| "bad msgid".to_string())?;
    Request::new(Command::Mark { channel, msgid })
        .serialize()
        .map_err(|e| e.to_string())
}

/// `MEMBERS <#chan>` — request the roster snapshot (§6.3).
pub fn build_members(channel: &str) -> Result<String, String> {
    let channel: weft_proto::ChannelName =
        channel.parse().map_err(|_| "bad channel".to_string())?;
    Request::new(Command::Members {
        channel,
        cursor: None,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `PART <#chan>` — leave a channel (§6.3).
pub fn build_part(channel: &str) -> Result<String, String> {
    let channel: weft_proto::ChannelName =
        channel.parse().map_err(|_| "bad channel".to_string())?;
    Request::new(Command::Part {
        channel,
        reason: None,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `CHANNEL CREATE <#chan>` — default retention (§6.3).
pub fn build_channel_create(channel: &str) -> Result<String, String> {
    let channel: weft_proto::ChannelName =
        channel.parse().map_err(|_| "bad channel".to_string())?;
    Request::new(Command::ChannelCreate {
        channel,
        policy: None,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `CHANNEL DELETE <#chan> <#chan>` — confirmed by repetition (§6.3).
pub fn build_channel_delete(channel: &str) -> Result<String, String> {
    let channel: weft_proto::ChannelName =
        channel.parse().map_err(|_| "bad channel".to_string())?;
    Request::new(Command::ChannelDelete {
        channel: channel.clone(),
        confirm: channel,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `CHANNEL META <#chan> <key> :<value>` — topic/view-gated/posting/… (§6.3).
pub fn build_channel_meta(channel: &str, key: &str, value: &str) -> Result<String, String> {
    let channel: weft_proto::ChannelName =
        channel.parse().map_err(|_| "bad channel".to_string())?;
    Request::new(Command::ChannelMeta {
        channel,
        key: key.to_string(),
        value: value.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `DISCOVER [cursor]` — public namespace directory (§6.2).
pub fn build_discover(cursor: Option<String>) -> Result<String, String> {
    Request::new(Command::Discover {
        cursor: cursor.filter(|c| !c.is_empty()),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `CHANNELS <ns>` — a namespace's ordered channel layout (§6.2).
pub fn build_channels(namespace: &str) -> Result<String, String> {
    let namespace: weft_proto::NamespaceName =
        namespace.parse().map_err(|_| "bad namespace".to_string())?;
    Request::new(Command::Channels { namespace })
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
        Some(b) => Some(
            b.parse::<MsgId>()
                .map_err(|_| "bad before msgid".to_string())?,
        ),
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

/// `NS CREATE <name> <tier>` with `@root=<b64-pubkey>` (§6.2). The keypair is
/// generated + stored by [`crate::keys`]; only the public key rides the wire.
pub fn build_ns_create(name: &str, visibility: &str, root_key: &str) -> Result<String, String> {
    let name: weft_proto::NamespaceName =
        name.parse().map_err(|_| "bad namespace name".to_string())?;
    let visibility: weft_proto::Visibility =
        visibility.parse().map_err(|_| "bad visibility".to_string())?;
    Request::new(Command::NsCreate {
        name,
        visibility,
        root_key: root_key.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `NS META <name> <key> :<value>` — title/description/icon (§6.2).
pub fn build_ns_meta(name: &str, key: &str, value: &str) -> Result<String, String> {
    Request::new(Command::NsMeta {
        name: name.parse().map_err(|_| "bad namespace".to_string())?,
        key: key.to_string(),
        value: value.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `NS VISIBILITY <name> <tier>` (§6.2).
pub fn build_ns_visibility(name: &str, visibility: &str) -> Result<String, String> {
    Request::new(Command::NsVisibility {
        name: name.parse().map_err(|_| "bad namespace".to_string())?,
        visibility: visibility.parse().map_err(|_| "bad visibility".to_string())?,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `NS DELEGATE <name> <subject> <caps>` — delegate ns caps (§6.2).
pub fn build_ns_delegate(name: &str, subject: &str, caps: &str) -> Result<String, String> {
    Request::new(Command::NsDelegate {
        name: name.parse().map_err(|_| "bad namespace".to_string())?,
        subject: subject.to_string(),
        caps: caps.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `NS DELETE <name> <name>` — confirmed by repetition (§6.2).
pub fn build_ns_delete(name: &str) -> Result<String, String> {
    let name: weft_proto::NamespaceName =
        name.parse().map_err(|_| "bad namespace".to_string())?;
    Request::new(Command::NsDelete {
        name: name.clone(),
        confirm: name,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `NS TRANSFER <name> <account>` with `@sig=` — root-signed succession (§2.4).
/// The signature is produced from the stored root key by the caller.
pub fn build_ns_transfer(name: &str, new_owner: &str, signature: &str) -> Result<String, String> {
    Request::new(Command::NsTransfer {
        name: name.parse().map_err(|_| "bad namespace".to_string())?,
        new_owner: new_owner.parse().map_err(|_| "bad account".to_string())?,
        signature: signature.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `NS RECOVERY SET <name> <m> <keys>` — designate the M-of-N quorum (§2.4).
pub fn build_ns_recovery_set(name: &str, m: u32, keys: &str) -> Result<String, String> {
    Request::new(Command::NsRecoverySet {
        name: name.parse().map_err(|_| "bad namespace".to_string())?,
        m,
        keys: keys.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `NS RECOVERY CANCEL <name>` with `@sig=` — root veto of a pending recovery.
pub fn build_ns_recovery_cancel(name: &str, signature: &str) -> Result<String, String> {
    Request::new(Command::NsRecoveryCancel {
        name: name.parse().map_err(|_| "bad namespace".to_string())?,
        signature: signature.to_string(),
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
