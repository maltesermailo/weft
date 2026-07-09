//! Portable WEFT client codec (native + wasm): reply-line parsing into
//! structured `ClientEvent`s, the §6.1/§3.3 auth FSM, and command-line
//! builders. No transport, runtime, or UI toolkit — bindings own the loop,
//! the stream, and the `EventSink`.

use serde::Serialize;
use weft_crypto::{sign_challenge, signature_to_b64, Keypair};
use weft_proto::{Command, Event, MsgId, Reply, Request, Target};

/// How a binding delivers a parsed event to its UI — Tauri `emit`, a JS
/// callback in wasm, a channel in tests.
const DEFAULT_PASSWORD: &str = "weft-client-dev-pw";

pub trait EventSink {
    fn emit(&self, event: ClientEvent);
}

/// Which credential flow the connect screen requested (§6.1).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// AUTH PASSWORD against an existing account; a failure is surfaced, never
    /// silently turned into a registration.
    Login,
    /// REGISTER a new account (which doubles as authentication).
    Register,
    /// AUTH KEY/PROOF with an enrolled device key — passwordless.
    Key,
}

impl Mode {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "login" => Ok(Mode::Login),
            "register" => Ok(Mode::Register),
            "key" => Ok(Mode::Key),
            other => Err(format!("unknown mode {other:?}")),
        }
    }
}

/// Structured events pushed to the webview under the `weft` channel.
#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ClientEvent {
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
    /// `PINNED <#chan> <msgid>` — a message was pinned (§7).
    Pinned {
        channel: String,
        msgid: String,
        by: Option<String>,
    },
    /// `UNPINNED <#chan> <msgid>` — a message was unpinned (§7).
    Unpinned {
        channel: String,
        msgid: String,
    },
    /// `CAPS <account> <scope> :<caps>` — effective caps (§10.4).
    Caps {
        account: String,
        scope: String,
        caps: String,
    },
    /// `ROLE <scope> <color> <caps> :<name>` — a role definition (§6.5).
    Role {
        scope: String,
        color: String,
        caps: String,
        name: String,
    },
    /// `ROLE-MEMBER <scope> <account> :<roles>` — an account's assigned roles.
    RoleMember {
        scope: String,
        account: String,
        roles: String,
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
        /// Server-authoritative channel categories (§6.3 layout).
        categories: Vec<String>,
        /// §11.10 auto-federation reachable (owner opened it to bridging).
        federation: bool,
    },
    /// `CHANNEL-LAYOUT <#chan> <position>` with optional `category=` (§7).
    ChannelLayout {
        channel: String,
        category: Option<String>,
        position: i64,
    },
    /// `CHANNEL-RENAMED <#old> <#new>` — a channel changed identity (§6.3).
    ChannelRenamed {
        old: String,
        new: String,
    },
    /// `MANIFEST <peer> <version> <state>` — a bridge's channel set/state (§11).
    Manifest {
        peer: String,
        version: u64,
        state: String,
        channels: Vec<String>,
        history: String,
        media: String,
        typing: bool,
    },
    /// `NETBLOCKED <network> [:reason]` — a blocked network (§11.6).
    Netblocked {
        network: String,
        reason: Option<String>,
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
        /// `0` marks a revoked/closed invite (§6.5).
        max_uses: Option<u32>,
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


/// §3.3 client handshake phase. The binding starts a connection in `HelloSent`;
/// `on_line` advances it to `Ready` (or signals close on `AUTH-FAILED`).
#[derive(PartialEq, Eq, Clone, Copy)]
pub enum Phase {
    HelloSent,
    AuthSent,
    Ready,
}

/// Process one inbound line: advance the handshake, emit structured events,
/// and return an outbound line to send in response (if any).
#[allow(clippy::too_many_arguments)]
pub fn on_line<E: EventSink>(
    sink: &E,
    account: &str,
    password: &str,
    mode: Mode,
    device: Option<&Keypair>,
    net_name: &mut String,
    phase: &mut Phase,
    in_batch: &mut bool,
    close: &mut bool,
    raw: &str,
) -> Option<String> {
    let reply = match Reply::parse(raw) {
        Ok(reply) => reply,
        Err(_) => {
            sink.emit(
                ClientEvent::Raw {
                    line: raw.to_string(),
                },
            );
            return None;
        }
    };
    // §3.3 handshake progression — the auth verb depends on the chosen mode.
    match (*phase, &reply.event) {
        (Phase::HelloSent, Event::Welcome { network, .. }) => {
            *phase = Phase::AuthSent;
            *net_name = network.to_string(); // needed to sign the key challenge
            return Some(match mode {
                Mode::Login => format!("AUTH PASSWORD {account} :{password}"),
                Mode::Register => format!("REGISTER {account} :{password}"),
                Mode::Key => match device {
                    Some(kp) => format!("AUTH KEY {account} {}", kp.public().to_b64()),
                    None => {
                        sink.emit(
                            ClientEvent::AuthFailed {
                                reason: "no device key on this device".into(),
                            },
                        );
                        *close = true;
                        return None;
                    }
                },
            });
        }
        // §6.1 device-key challenge → sign `nonce ‖ network` and prove.
        (Phase::AuthSent, Event::Challenge { nonce }) => {
            let (Some(kp), Ok(nonce_bytes)) = (device, weft_crypto::b64::decode(nonce)) else {
                sink.emit(
                    ClientEvent::AuthFailed {
                        reason: "bad device-key challenge".into(),
                    },
                );
                *close = true;
                return None;
            };
            let sig = sign_challenge(kp, &nonce_bytes, net_name);
            return Some(format!("AUTH PROOF {}", signature_to_b64(&sig)));
        }
        (Phase::AuthSent, Event::Welcome { network, .. }) => {
            *phase = Phase::Ready;
            sink.emit(
                ClientEvent::Connected {
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
                (Mode::Key, weft_proto::ErrCode::AuthFailed) => {
                    "device-key login failed — enroll this device first".to_string()
                }
                _ => e.text.clone(),
            };
            sink.emit(ClientEvent::AuthFailed { reason });
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
            sink.emit(ClientEvent::BatchStart { id });
        }
        Event::BatchEnd { id, truncated, .. } => {
            *in_batch = false;
            sink.emit(ClientEvent::BatchEnd { id, truncated });
        }
        Event::Message(m) => sink.emit(
            ClientEvent::Message {
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
        } => sink.emit(
            ClientEvent::Member {
                channel: channel.to_string(),
                user: user.account.to_string(),
                network: user.network.to_string(),
                action: action.to_string(),
                count,
            },
        ),
        Event::Policy { channel, policy } => sink.emit(
            ClientEvent::Policy {
                channel: channel.to_string(),
                policy: policy.to_string(),
            },
        ),
        Event::Typing {
            channel,
            user,
            state,
        } => sink.emit(
            ClientEvent::Typing {
                channel: channel.to_string(),
                user: user.account.to_string(),
                state: state.to_string(),
            },
        ),
        Event::Presence { user, status } => sink.emit(
            ClientEvent::Presence {
                user: user.account.to_string(),
                status: status.to_string(),
            },
        ),
        Event::Marked { channel, msgid } => sink.emit(
            ClientEvent::Marked {
                channel: channel.to_string(),
                msgid: msgid.to_string(),
            },
        ),
        Event::Pinned { channel, msgid, by } => sink.emit(
            ClientEvent::Pinned {
                channel: channel.to_string(),
                msgid: msgid.to_string(),
                by: by.map(|a| a.to_string()),
            },
        ),
        Event::Unpinned { channel, msgid } => sink.emit(
            ClientEvent::Unpinned {
                channel: channel.to_string(),
                msgid: msgid.to_string(),
            },
        ),
        Event::Caps {
            account,
            scope,
            caps,
        } => sink.emit(
            ClientEvent::Caps {
                account: account.to_string(),
                scope,
                caps,
            },
        ),
        Event::Role {
            scope,
            color,
            caps,
            name,
        } => sink.emit(
            ClientEvent::Role {
                scope,
                color,
                caps,
                name,
            },
        ),
        Event::RoleMember {
            scope,
            account,
            roles,
        } => sink.emit(
            ClientEvent::RoleMember {
                scope,
                account: account.to_string(),
                roles,
            },
        ),
        Event::Chanmeta {
            channel,
            key,
            value,
        } => sink.emit(
            ClientEvent::Chanmeta {
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
            categories,
            federation,
            ..
        } => sink.emit(
            ClientEvent::NsMeta {
                name: name.to_string(),
                visibility: visibility.to_string(),
                owner,
                title,
                description,
                recovery_set,
                recovery_eta: recovery_pending.map(|(eta, _)| eta),
                recovery_rung: recovery_pending.map(|(_, rung)| rung),
                categories,
                federation,
            },
        ),
        Event::ChannelLayout {
            channel,
            category,
            position,
        } => sink.emit(
            ClientEvent::ChannelLayout {
                channel: channel.to_string(),
                category,
                position,
            },
        ),
        Event::ChannelRenamed { old, new } => sink.emit(
            ClientEvent::ChannelRenamed {
                old: old.to_string(),
                new: new.to_string(),
            },
        ),
        Event::More { cursor } => sink.emit(ClientEvent::More { cursor }),
        Event::Token { subject, scope, .. } => sink.emit(ClientEvent::Token { subject, scope }),
        Event::Invited {
            scope,
            invite_id,
            link,
            max_uses,
            ..
        } => sink.emit(
            ClientEvent::Invited {
                scope,
                invite_id,
                link,
                max_uses,
            },
        ),
        Event::Reported { report_id } => sink.emit(ClientEvent::Reported { report_id }),
        Event::ReportFiled {
            report_id,
            msgid,
            category,
            state,
            scope,
            reporter,
        } => sink.emit(
            ClientEvent::ReportFiled {
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
        } => sink.emit(
            ClientEvent::ReportResolved {
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
        } => sink.emit(
            ClientEvent::Edited {
                target: target.to_string(),
                sender: user.account.to_string(),
                edit_of: edit_of.to_string(),
                body,
            },
        ),
        Event::Deleted { target, msgid, .. } => sink.emit(
            ClientEvent::Deleted {
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
        } => sink.emit(
            ClientEvent::Reaction {
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
        } => sink.emit(
            ClientEvent::Reactions {
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
        } => sink.emit(
            ClientEvent::Moderated {
                scope,
                account: account.to_string(),
                action: action.to_string(),
                by: by.map(|a| a.to_string()),
                reason,
            },
        ),
        Event::Err(e) => sink.emit(
            ClientEvent::Error {
                code: e.code.to_string(),
                text: e.text,
            },
        ),
        // Federation (§11): bridge manifests + netblock notifications.
        Event::Manifest {
            peer,
            version,
            state,
            channels,
            history,
            media,
            typing,
        } => sink.emit(
            ClientEvent::Manifest {
                peer: peer.to_string(),
                version,
                state: state.to_string(),
                channels: channels.iter().map(|c| c.to_string()).collect(),
                history: history.to_string(),
                media: media.to_string(),
                typing,
            },
        ),
        Event::Netblocked { network, reason } => sink.emit(
            ClientEvent::Netblocked {
                network: network.to_string(),
                reason,
            },
        ),
        // Keepalive answers are internal — never shown.
        Event::Pong { .. } => {}
        // Batches, reactions, presence, etc. — surfaced raw for now.
        _ => sink.emit(
            ClientEvent::Raw {
                line: raw.to_string(),
            },
        ),
    }
    None
}


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

/// `INVITE REVOKE <invite-id>` — close an outstanding invite (§6.5).
pub fn build_invite_revoke(invite_id: &str) -> Result<String, String> {
    Request::new(Command::InviteRevoke {
        invite_id: invite_id.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

// ---- federation (§11): netblocks + bridges (operator surface) ----

/// `NETBLOCK ADD <network> [:reason]` (§11.6). Cap `netblock` at `*`.
pub fn build_netblock_add(network: &str, reason: Option<&str>) -> Result<String, String> {
    let network: weft_proto::NetworkName = network.parse().map_err(|_| "bad network".to_string())?;
    Request::new(Command::NetblockAdd {
        network,
        reason: reason.filter(|r| !r.is_empty()).map(String::from),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `NETBLOCK REMOVE <network>` (§11.6).
pub fn build_netblock_remove(network: &str) -> Result<String, String> {
    let network: weft_proto::NetworkName = network.parse().map_err(|_| "bad network".to_string())?;
    Request::new(Command::NetblockRemove { network })
        .serialize()
        .map_err(|e| e.to_string())
}

/// `NETBLOCK LIST` (§11.6) → a `NETBLOCKED` per blocked network.
pub fn build_netblock_list() -> Result<String, String> {
    Request::new(Command::NetblockList)
        .serialize()
        .map_err(|e| e.to_string())
}

/// `BRIDGE PROPOSE <scope> <peer> …` (§11.1) — sign + store a peering manifest.
/// `history` = from-epoch|full, `media` = mirror|mirror-max:<bytes>|none.
pub fn build_bridge_propose(
    scope: &str,
    peer: &str,
    history: &str,
    media: &str,
    typing: bool,
) -> Result<String, String> {
    let peer: weft_proto::NetworkName = peer.parse().map_err(|_| "bad peer network".to_string())?;
    let history: weft_proto::HistoryMode =
        history.parse().map_err(|_| "bad history mode".to_string())?;
    let media: weft_proto::MediaMode = media.parse().map_err(|_| "bad media mode".to_string())?;
    Request::new(Command::BridgePropose {
        scope: scope.to_string(),
        peer,
        history,
        media,
        typing,
        manifest: None,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `BRIDGE ACCEPT <peer> <version>` (§11.1) — ack a proposed manifest version.
pub fn build_bridge_accept(peer: &str, version: u64) -> Result<String, String> {
    let peer: weft_proto::NetworkName = peer.parse().map_err(|_| "bad peer network".to_string())?;
    Request::new(Command::BridgeAccept { peer, version })
        .serialize()
        .map_err(|e| e.to_string())
}

/// `BRIDGE SEVER <peer>` (§11.1) — tear down a bridge.
pub fn build_bridge_sever(peer: &str) -> Result<String, String> {
    let peer: weft_proto::NetworkName = peer.parse().map_err(|_| "bad peer network".to_string())?;
    Request::new(Command::BridgeSever { peer })
        .serialize()
        .map_err(|e| e.to_string())
}

/// Moderation (§6.7): `MUTE`/`UNMUTE`/`BAN`/`UNBAN` `<scope> <account> [:reason]`
/// or `KICK <#chan> <account> [:reason]`. For `kick`, `scope` is the channel.
pub fn build_moderation(
    verb: &str,
    scope: &str,
    account: &str,
    reason: Option<&str>,
) -> Result<String, String> {
    let acct: weft_proto::Account = account.parse().map_err(|_| "bad account".to_string())?;
    let scope = scope.to_string();
    let reason = reason.filter(|r| !r.is_empty()).map(String::from);
    let cmd = match verb {
        "mute" => Command::Mute { scope, account: acct, reason },
        "unmute" => Command::Unmute { scope, account: acct },
        "ban" => Command::Ban { scope, account: acct, reason },
        "unban" => Command::Unban { scope, account: acct },
        "kick" => Command::Kick {
            channel: scope.parse().map_err(|_| "bad channel".to_string())?,
            account: acct,
            reason,
        },
        _ => return Err(format!("unknown moderation verb: {verb}")),
    };
    Request::new(cmd).serialize().map_err(|e| e.to_string())
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

/// `PIN`/`UNPIN <msgid>` — (un)pin a message (§6.4).
pub fn build_pin(msgid: &str, pinned: bool) -> Result<String, String> {
    let msgid: MsgId = msgid.parse().map_err(|_| "bad msgid".to_string())?;
    let cmd = if pinned {
        Command::Pin { msgid }
    } else {
        Command::Unpin { msgid }
    };
    Request::new(cmd).serialize().map_err(|e| e.to_string())
}

/// `AUTH ENROLL <b64-pubkey>` — add a device key while authed (§6.1).
pub fn build_auth_enroll(pubkey: &str) -> Result<String, String> {
    Request::new(Command::AuthEnroll {
        pubkey: pubkey.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `CAPS <account> <scope>` — query an account's effective caps (§10.4).
pub fn build_caps(account: &str, scope: &str) -> Result<String, String> {
    Request::new(Command::Caps {
        account: account.parse().map_err(|_| "bad account".to_string())?,
        scope: scope.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// §6.5 named roles (capability-token bundles).
pub fn build_roles(scope: &str) -> Result<String, String> {
    Request::new(Command::RolesList {
        scope: scope.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

pub fn build_role_create(
    scope: &str,
    color: &str,
    caps: &str,
    name: &str,
) -> Result<String, String> {
    Request::new(Command::RoleCreate {
        scope: scope.to_string(),
        color: color.to_string(),
        caps: caps.to_string(),
        name: name.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

pub fn build_role_delete(scope: &str, name: &str) -> Result<String, String> {
    Request::new(Command::RoleDelete {
        scope: scope.to_string(),
        name: name.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

pub fn build_role_assign(scope: &str, account: &str, name: &str) -> Result<String, String> {
    Request::new(Command::RoleAssign {
        scope: scope.to_string(),
        account: account.parse().map_err(|_| "bad account".to_string())?,
        name: name.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

pub fn build_role_unassign(scope: &str, account: &str, name: &str) -> Result<String, String> {
    Request::new(Command::RoleUnassign {
        scope: scope.to_string(),
        account: account.parse().map_err(|_| "bad account".to_string())?,
        name: name.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

pub fn build_roles_of(scope: &str, account: &str) -> Result<String, String> {
    Request::new(Command::RolesOf {
        scope: scope.to_string(),
        account: account.parse().map_err(|_| "bad account".to_string())?,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `PINS <#chan>` — list pinned messages (§6.4).
pub fn build_pins(channel: &str) -> Result<String, String> {
    let channel: weft_proto::ChannelName =
        channel.parse().map_err(|_| "bad channel".to_string())?;
    Request::new(Command::Pins { channel })
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

/// `CHANNEL CREATE <#chan> [policy]` — optional retention, else server default (§6.3).
pub fn build_channel_create(channel: &str, policy: Option<&str>) -> Result<String, String> {
    let channel: weft_proto::ChannelName =
        channel.parse().map_err(|_| "bad channel".to_string())?;
    let policy = match policy {
        Some(p) if !p.is_empty() => {
            Some(p.parse::<weft_proto::RetentionPolicy>().map_err(|_| "bad policy".to_string())?)
        }
        _ => None,
    };
    Request::new(Command::ChannelCreate { channel, policy })
        .serialize()
        .map_err(|e| e.to_string())
}

/// `CHANNEL POLICY <#chan> <policy> [purge]` — change an existing channel's
/// retention (§6.3). `purge` is required for some e2ee transitions (invariant 8).
pub fn build_channel_policy(channel: &str, policy: &str, purge: bool) -> Result<String, String> {
    let channel: weft_proto::ChannelName =
        channel.parse().map_err(|_| "bad channel".to_string())?;
    let policy = policy
        .parse::<weft_proto::RetentionPolicy>()
        .map_err(|_| "bad policy".to_string())?;
    Request::new(Command::ChannelPolicy {
        channel,
        policy,
        purge,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `CHANNEL RENAME <#old> <#new>` — change a channel's identity (§6.3).
pub fn build_channel_rename(old: &str, new: &str) -> Result<String, String> {
    let channel: weft_proto::ChannelName = old.parse().map_err(|_| "bad channel".to_string())?;
    let new_name: weft_proto::ChannelName = new.parse().map_err(|_| "bad channel".to_string())?;
    Request::new(Command::ChannelRename { channel, new_name })
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
    let visibility: weft_proto::Visibility = visibility
        .parse()
        .map_err(|_| "bad visibility".to_string())?;
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

/// `FEDERATE <network>/<namespace>` (§11.10) — request an on-demand bridge to a
/// foreign namespace. Accepts `network/namespace` or a `weft://<net>/<ns>` link.
pub fn build_federate(target: &str) -> Result<String, String> {
    let t = target.trim().strip_prefix("weft://").unwrap_or(target.trim());
    let (net, ns) = t.split_once('/').ok_or("expected network/namespace")?;
    Request::new(Command::Federate {
        network: net.parse().map_err(|_| "bad network".to_string())?,
        namespace: ns.parse().map_err(|_| "bad namespace".to_string())?,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `NS VISIBILITY <name> <tier>` (§6.2).
pub fn build_ns_visibility(name: &str, visibility: &str) -> Result<String, String> {
    Request::new(Command::NsVisibility {
        name: name.parse().map_err(|_| "bad namespace".to_string())?,
        visibility: visibility
            .parse()
            .map_err(|_| "bad visibility".to_string())?,
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
    let name: weft_proto::NamespaceName = name.parse().map_err(|_| "bad namespace".to_string())?;
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

/// `NS RECOVER <name> <b64-rotation-record>` — submit a co-signed rotation.
pub fn build_ns_recover(name: &str, rotation: &str) -> Result<String, String> {
    Request::new(Command::NsRecover {
        name: name.parse().map_err(|_| "bad namespace".to_string())?,
        rotation: rotation.to_string(),
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
