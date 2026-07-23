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
    /// §13 per-session media fetch bearer (issued at auth); the UI puts it on
    /// `/media/<hash>?t=…` fetch URLs.
    MediaToken {
        token: String,
    },
    /// §6/§13 a large HISTORY page is being served as a data-plane stream: the
    /// client pulls `/backfill?t=<token>` and folds the returned lines exactly
    /// like an inline `BATCH` (M-media-4). Correlates to the pending HISTORY.
    Backfill {
        token: String,
    },
    Message {
        target: String,
        sender: String,
        network: String,
        msgid: String,
        body: String,
        /// §13 `attach.N=` media references (`weft-media://…` URIs), in order.
        attachments: Vec<String>,
        /// `system=<kind>` — a server-generated system message (`join`/`part`);
        /// the client renders localized text instead of a normal message.
        system: Option<String>,
        own: bool,
        /// True when this arrived inside a `HISTORY` batch (older messages to
        /// prepend), false for live traffic to append.
        history: bool,
        /// Batch form: the message already carries collapsed edits.
        edited: bool,
        /// `reply-to=` — the msgid this replies to (§9.3), if any.
        reply_to: Option<String>,
        /// `thread=` — the root msgid this message belongs to (§9.4), if any.
        thread: Option<String>,
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
    /// `UNREAD-COUNTS <#chan> <unread> <mentions>` — server-computed unread
    /// tally for a channel (§6.3), authoritative over the client's live tally.
    UnreadCounts {
        channel: String,
        unread: u64,
        mentions: u64,
    },
    /// `EMOJI <ns> <name> <media>` — a namespace custom emoji (§9.4).
    Emoji {
        namespace: String,
        name: String,
        media: String,
    },
    /// `EMOJI-REMOVED <ns> <name>` — a namespace emoji was removed (§9.4).
    EmojiRemoved {
        namespace: String,
        name: String,
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
    /// `THREAD <#chan> <root> replies=<n> [last=] [:name]` — one thread in a
    /// `THREADS` list response (§9.4).
    Thread {
        channel: String,
        root: String,
        replies: u32,
        last: Option<String>,
        name: Option<String>,
    },
    /// `THREAD-NAMED <#chan> <root> [:name]` — a thread was (re)named or, with
    /// no name, cleared (§9.4).
    ThreadNamed {
        channel: String,
        root: String,
        name: Option<String>,
    },
    /// `FRIEND <user@net> <state>` — a friendship state (social layer): a
    /// `FRIENDS` list entry or a live change (`friends`/`incoming`/`outgoing`).
    Friend {
        user: String,
        state: String,
    },
    /// `FRIEND-REMOVED <user@net>` — a friendship or pending request ended.
    FriendRemoved {
        user: String,
    },
    /// `GROUP <&id> [name] :<members>` — a group DM's identity, name, members.
    Group {
        id: String,
        name: Option<String>,
        members: Vec<String>,
    },
    /// `GROUP-MEMBER <&id> <user@net> <join|part>` — a membership change.
    GroupMember {
        group: String,
        user: String,
        action: String,
    },
    /// `GROUP-CALL <&id> <user@net> <state>` — a member's presence in the group's
    /// voice call (`active` = in it, `ended` = left).
    GroupCallState {
        group: String,
        user: String,
        state: String,
    },
    /// `CALL-RING <from@net> <room>` — an incoming 1:1 friend call.
    CallRing {
        from: String,
        room: String,
    },
    /// `CALL-STATE <user@net> <state>` — a call's lifecycle update.
    CallState {
        user: String,
        state: String,
    },
    /// `CALL-MEDIA <room> <token> :<endpoint>` — the LiveKit credential for a
    /// friend call, delivered per-participant when the call goes active.
    CallMedia {
        room: String,
        mode: String,
        token: String,
        endpoint: Option<String>,
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
        hoist: bool,
        position: i32,
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
    /// `CHANNEL-LAYOUT <#chan> <position>` with optional `category=`/`kind=` (§7).
    /// `channel_kind` is `text` (default) or `voice` (§16 voice-only room) —
    /// named to avoid clashing with the enum's `kind` serde tag.
    ChannelLayout {
        channel: String,
        category: Option<String>,
        position: i64,
        channel_kind: String,
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
        voice: bool,
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
    /// `INVITE-INFO …` — one live invite in an `INVITE LIST` response (§6.5).
    InviteInfo {
        scope: String,
        invite_id: String,
        creator: String,
        uses_left: Option<u32>,
        expiry: Option<u64>,
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
    /// §10.3 `PROFILE <account>` — a display profile (nick + avatar hash) for an
    /// account, broadcast on change and in reply to `PROFILES`. A `None` field is
    /// unset; the client resolves `avatar` to a `weft-media://` URL, falling back
    /// to initials.
    Profile {
        account: String,
        /// The account's home network (so a federated profile is distinguishable
        /// from a local one with the same handle).
        network: String,
        display: Option<String>,
        avatar: Option<String>,
    },
    /// §10.5 `VERIFIED <kind> <subject>` — one of the caller's own verification
    /// claims (email/birthday), `state` = `pending`|`confirmed`. Owner-only.
    /// (`claim_kind`, not `kind` — the enum's serde tag is already `kind`.)
    Verified {
        claim_kind: String,
        subject: String,
        state: String,
    },
    /// §16 `VOICE OFFER <#chan> <token> [:endpoint]` — the answer to our
    /// `VOICE JOIN`. `mode` picks the media path: `"webrtc"` = negotiate with the
    /// embedded SFU via `VOICE DESC` (token = media token); `"livekit"` = connect
    /// the LiveKit SDK to `endpoint` (the server URL) with `token` (a LiveKit
    /// access JWT) in `room`. `room` is set only for LiveKit.
    VoiceOffer {
        channel: String,
        mode: String,
        token: String,
        room: Option<String>,
        endpoint: Option<String>,
    },
    /// §16 `VOICE STATE <#chan> <user@net> <join|leave|update>` — voice-room
    /// presence for the channel's members (speaking / muted / deafened flags).
    VoiceState {
        channel: String,
        user: String,
        action: String,
        muted: bool,
        deaf: bool,
        speaking: bool,
    },
    /// §16 `VOICE DESC <#chan> :<sdp>` — the SFU's SDP answer to our offer.
    VoiceDesc {
        channel: String,
        sdp: String,
    },
    /// §16 `VOICE CAND <#chan> :<candidate>` — a trickle-ICE candidate from the
    /// SFU (unused by the non-trickle default; handled for completeness).
    VoiceCand {
        channel: String,
        candidate: String,
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
            sink.emit(ClientEvent::Raw {
                line: raw.to_string(),
            });
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
                        sink.emit(ClientEvent::AuthFailed {
                            reason: "no device key on this device".into(),
                        });
                        *close = true;
                        return None;
                    }
                },
            });
        }
        // §6.1 device-key challenge → sign `nonce ‖ network` and prove.
        (Phase::AuthSent, Event::Challenge { nonce }) => {
            let (Some(kp), Ok(nonce_bytes)) = (device, weft_crypto::b64::decode(nonce)) else {
                sink.emit(ClientEvent::AuthFailed {
                    reason: "bad device-key challenge".into(),
                });
                *close = true;
                return None;
            };
            let sig = sign_challenge(kp, &nonce_bytes, net_name);
            return Some(format!("AUTH PROOF {}", signature_to_b64(&sig)));
        }
        (Phase::AuthSent, Event::Welcome { network, .. }) => {
            *phase = Phase::Ready;
            sink.emit(ClientEvent::Connected {
                network: network.to_string(),
                account: account.to_string(),
            });
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
        Event::MediaToken { token } => sink.emit(ClientEvent::MediaToken { token }),
        // §6/§13 a HISTORY over the stream threshold — pull it off the data plane.
        Event::StreamAccept { token } => sink.emit(ClientEvent::Backfill { token }),
        Event::Message(m) => sink.emit(ClientEvent::Message {
            target: m.target.to_string(),
            sender: m.sender.account.to_string(),
            network: m.sender.network.to_string(),
            msgid: m.msgid.to_string(),
            own: m.sender.account.as_str() == account,
            history: *in_batch,
            edited: m.edited.is_some(),
            reply_to: m.meta.reply_to.as_ref().map(|r| r.to_string()),
            thread: m.meta.thread.as_ref().map(|t| t.to_string()),
            md: m.meta.fmt.as_deref() == Some("md"),
            attachments: m.meta.attachments.clone(),
            system: m.meta.system.clone(),
            body: m.body,
        }),
        Event::Member {
            channel,
            user,
            action,
            count,
            ..
        } => sink.emit(ClientEvent::Member {
            channel: channel.to_string(),
            user: user.account.to_string(),
            network: user.network.to_string(),
            action: action.to_string(),
            count,
        }),
        Event::Policy { channel, policy } => sink.emit(ClientEvent::Policy {
            channel: channel.to_string(),
            policy: policy.to_string(),
        }),
        Event::Typing {
            channel,
            user,
            state,
        } => sink.emit(ClientEvent::Typing {
            channel: channel.to_string(),
            user: user.account.to_string(),
            state: state.to_string(),
        }),
        Event::Presence { user, status } => sink.emit(ClientEvent::Presence {
            user: user.account.to_string(),
            status: status.to_string(),
        }),
        Event::Marked { channel, msgid } => sink.emit(ClientEvent::Marked {
            channel: channel.to_string(),
            msgid: msgid.to_string(),
        }),
        Event::UnreadCounts {
            channel,
            unread,
            mentions,
        } => sink.emit(ClientEvent::UnreadCounts {
            channel: channel.to_string(),
            unread,
            mentions,
        }),
        Event::Emoji {
            namespace,
            name,
            media,
        } => sink.emit(ClientEvent::Emoji {
            namespace: namespace.to_string(),
            name,
            media,
        }),
        Event::EmojiRemoved { namespace, name } => sink.emit(ClientEvent::EmojiRemoved {
            namespace: namespace.to_string(),
            name,
        }),
        Event::Pinned { channel, msgid, by } => sink.emit(ClientEvent::Pinned {
            channel: channel.to_string(),
            msgid: msgid.to_string(),
            by: by.map(|a| a.to_string()),
        }),
        Event::Unpinned { channel, msgid } => sink.emit(ClientEvent::Unpinned {
            channel: channel.to_string(),
            msgid: msgid.to_string(),
        }),
        Event::Thread {
            channel,
            root,
            replies,
            last,
            name,
        } => sink.emit(ClientEvent::Thread {
            channel: channel.to_string(),
            root: root.to_string(),
            replies,
            last: last.map(|m| m.to_string()),
            name,
        }),
        Event::ThreadNamed {
            channel,
            root,
            name,
        } => sink.emit(ClientEvent::ThreadNamed {
            channel: channel.to_string(),
            root: root.to_string(),
            name,
        }),
        Event::Friend { user, state } => sink.emit(ClientEvent::Friend {
            user: user.to_string(),
            state: state.to_string(),
        }),
        Event::FriendRemoved { user } => sink.emit(ClientEvent::FriendRemoved {
            user: user.to_string(),
        }),
        Event::Group { id, name, members } => sink.emit(ClientEvent::Group {
            id: id.to_string(),
            name,
            members: members.iter().map(|m| m.to_string()).collect(),
        }),
        Event::GroupMember {
            group,
            user,
            action,
        } => sink.emit(ClientEvent::GroupMember {
            group: group.to_string(),
            user: user.to_string(),
            action: action.to_string(),
        }),
        Event::GroupCallState { group, user, state } => sink.emit(ClientEvent::GroupCallState {
            group: group.to_string(),
            user: user.to_string(),
            state: state.to_string(),
        }),
        Event::CallRing { from, room } => sink.emit(ClientEvent::CallRing {
            from: from.to_string(),
            room,
        }),
        Event::CallState { user, state } => sink.emit(ClientEvent::CallState {
            user: user.to_string(),
            state: state.to_string(),
        }),
        Event::CallMedia {
            room,
            mode,
            token,
            endpoint,
        } => sink.emit(ClientEvent::CallMedia {
            room,
            mode: mode.to_string(),
            token,
            endpoint,
        }),
        Event::Caps {
            account,
            scope,
            caps,
        } => sink.emit(ClientEvent::Caps {
            account: account.to_string(),
            scope,
            caps,
        }),
        Event::Role {
            scope,
            color,
            caps,
            hoist,
            position,
            name,
        } => sink.emit(ClientEvent::Role {
            scope,
            color,
            caps,
            hoist,
            position,
            name,
        }),
        Event::RoleMember {
            scope,
            account,
            roles,
        } => sink.emit(ClientEvent::RoleMember {
            scope,
            account: account.to_string(),
            roles,
        }),
        Event::Chanmeta {
            channel,
            key,
            value,
        } => sink.emit(ClientEvent::Chanmeta {
            channel: channel.to_string(),
            key,
            value,
        }),
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
        } => sink.emit(ClientEvent::NsMeta {
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
        }),
        Event::ChannelLayout {
            channel,
            category,
            position,
            kind,
        } => sink.emit(ClientEvent::ChannelLayout {
            channel: channel.to_string(),
            category,
            position,
            channel_kind: kind.to_string(),
        }),
        Event::ChannelRenamed { old, new } => sink.emit(ClientEvent::ChannelRenamed {
            old: old.to_string(),
            new: new.to_string(),
        }),
        Event::More { cursor } => sink.emit(ClientEvent::More { cursor }),
        Event::Token { subject, scope, .. } => sink.emit(ClientEvent::Token { subject, scope }),
        Event::Invited {
            scope,
            invite_id,
            link,
            max_uses,
            ..
        } => sink.emit(ClientEvent::Invited {
            scope,
            invite_id,
            link,
            max_uses,
        }),
        Event::InviteInfo {
            scope,
            invite_id,
            creator,
            uses_left,
            expiry,
        } => sink.emit(ClientEvent::InviteInfo {
            scope,
            invite_id,
            creator: creator.to_string(),
            uses_left,
            expiry,
        }),
        Event::Reported { report_id } => sink.emit(ClientEvent::Reported { report_id }),
        Event::ReportFiled {
            report_id,
            msgid,
            category,
            state,
            scope,
            reporter,
        } => sink.emit(ClientEvent::ReportFiled {
            report_id,
            msgid: msgid.to_string(),
            category,
            state: state.to_string(),
            scope: scope.to_string(),
            reporter,
        }),
        Event::ReportResolved {
            report_id,
            action,
            note,
            ..
        } => sink.emit(ClientEvent::ReportResolved {
            report_id,
            action: action.to_string(),
            note,
        }),
        Event::Edited {
            target,
            user,
            edit_of,
            body,
            ..
        } => sink.emit(ClientEvent::Edited {
            target: target.to_string(),
            sender: user.account.to_string(),
            edit_of: edit_of.to_string(),
            body,
        }),
        Event::Deleted { target, msgid, .. } => sink.emit(ClientEvent::Deleted {
            target: target.to_string(),
            msgid: msgid.to_string(),
        }),
        Event::Reaction {
            target,
            msgid,
            emoji,
            op,
            by,
        } => sink.emit(ClientEvent::Reaction {
            target: target.to_string(),
            msgid: msgid.to_string(),
            emoji,
            op: op.to_string(),
            by: by.account.to_string(),
        }),
        Event::Reactions {
            target,
            msgid,
            emoji,
            count,
            by,
        } => sink.emit(ClientEvent::Reactions {
            target: target.to_string(),
            msgid: msgid.to_string(),
            emoji,
            count,
            by: by.iter().map(|u| u.account.to_string()).collect(),
        }),
        Event::Moderated {
            scope,
            account,
            action,
            by,
            reason,
        } => sink.emit(ClientEvent::Moderated {
            scope,
            account: account.to_string(),
            action: action.to_string(),
            by: by.map(|a| a.to_string()),
            reason,
        }),
        // §10.3 display profiles.
        Event::Profile {
            user,
            display,
            avatar,
        } => sink.emit(ClientEvent::Profile {
            account: user.account.to_string(),
            network: user.network.to_string(),
            display,
            avatar,
        }),
        // §10.5 account verification claims (owner-only).
        Event::Verified {
            kind,
            subject,
            state,
        } => sink.emit(ClientEvent::Verified {
            claim_kind: kind,
            subject,
            state: state.to_string(),
        }),
        // §16 WEFT-RT voice signaling.
        Event::VoiceOffer {
            channel,
            mode,
            token,
            room,
            endpoint,
        } => sink.emit(ClientEvent::VoiceOffer {
            channel: channel.to_string(),
            mode: mode.to_string(),
            token,
            room,
            endpoint,
        }),
        Event::VoiceState {
            channel,
            user,
            action,
            muted,
            deaf,
            speaking,
        } => sink.emit(ClientEvent::VoiceState {
            channel: channel.to_string(),
            user: user.account.to_string(),
            action: action.to_string(),
            muted,
            deaf,
            speaking,
        }),
        Event::VoiceDesc { channel, sdp } => sink.emit(ClientEvent::VoiceDesc {
            channel: channel.to_string(),
            sdp,
        }),
        Event::VoiceCand { channel, candidate } => sink.emit(ClientEvent::VoiceCand {
            channel: channel.to_string(),
            candidate,
        }),
        Event::Err(e) => sink.emit(ClientEvent::Error {
            code: e.code.to_string(),
            text: e.text,
        }),
        // Federation (§11): bridge manifests + netblock notifications.
        Event::Manifest {
            peer,
            version,
            state,
            channels,
            history,
            media,
            typing,
            voice,
        } => sink.emit(ClientEvent::Manifest {
            peer: peer.to_string(),
            version,
            state: state.to_string(),
            channels: channels.iter().map(|c| c.to_string()).collect(),
            history: history.to_string(),
            media: media.to_string(),
            typing,
            voice,
        }),
        Event::Netblocked { network, reason } => sink.emit(ClientEvent::Netblocked {
            network: network.to_string(),
            reason,
        }),
        // Keepalive answers are internal — never shown.
        Event::Pong { .. } => {}
        // Batches, reactions, presence, etc. — surfaced raw for now.
        _ => sink.emit(ClientEvent::Raw {
            line: raw.to_string(),
        }),
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
pub fn build_msg(
    target: &str,
    body: &str,
    reply_to: Option<String>,
    attachments: Vec<String>,
    thread: Option<String>,
) -> Result<String, String> {
    let target: Target = target.parse().map_err(|_| "bad target".to_string())?;
    let reply_to = match reply_to.filter(|r| !r.is_empty()) {
        Some(r) => Some(
            r.parse::<MsgId>()
                .map_err(|_| "bad reply-to msgid".to_string())?,
        ),
        None => None,
    };
    let thread = match thread.filter(|t| !t.is_empty()) {
        Some(t) => Some(
            t.parse::<MsgId>()
                .map_err(|_| "bad thread msgid".to_string())?,
        ),
        None => None,
    };
    let meta = weft_proto::MsgMeta {
        // The client composes in markdown; tag it so peers render it (§9.4).
        fmt: Some("md".to_string()),
        reply_to,
        thread,
        attachments,
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

/// `INVITE REVOKE-ALL <scope>` — close every invite for the scope's namespace.
pub fn build_invite_revoke_all(scope: &str) -> Result<String, String> {
    Request::new(Command::InviteRevokeAll {
        scope: scope.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `INVITE LIST <scope>` — the live invites at the scope (a `BATCH`).
pub fn build_invite_list(scope: &str) -> Result<String, String> {
    Request::new(Command::InviteList {
        scope: scope.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

// ---- federation (§11): netblocks + bridges (operator surface) ----

/// `NETBLOCK ADD <network> [:reason]` (§11.6). Cap `netblock` at `*`.
pub fn build_netblock_add(network: &str, reason: Option<&str>) -> Result<String, String> {
    let network: weft_proto::NetworkName =
        network.parse().map_err(|_| "bad network".to_string())?;
    Request::new(Command::NetblockAdd {
        network,
        reason: reason.filter(|r| !r.is_empty()).map(String::from),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `NETBLOCK REMOVE <network>` (§11.6).
pub fn build_netblock_remove(network: &str) -> Result<String, String> {
    let network: weft_proto::NetworkName =
        network.parse().map_err(|_| "bad network".to_string())?;
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
    let history: weft_proto::HistoryMode = history
        .parse()
        .map_err(|_| "bad history mode".to_string())?;
    let media: weft_proto::MediaMode = media.parse().map_err(|_| "bad media mode".to_string())?;
    Request::new(Command::BridgePropose {
        scope: scope.to_string(),
        peer,
        history,
        media,
        typing,
        // §16 an explicit operator propose is strictest-safe (voice off); voice
        // federation opts in via §11.10 auto-federation. A UI toggle is deferred.
        voice: false,
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
        "mute" => Command::Mute {
            scope,
            account: acct,
            reason,
        },
        "unmute" => Command::Unmute {
            scope,
            account: acct,
        },
        "ban" => Command::Ban {
            scope,
            account: acct,
            reason,
        },
        "unban" => Command::Unban {
            scope,
            account: acct,
        },
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
    hoist: bool,
    position: i32,
    name: &str,
) -> Result<String, String> {
    Request::new(Command::RoleCreate {
        scope: scope.to_string(),
        color: color.to_string(),
        caps: caps.to_string(),
        hoist,
        position,
        name: name.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

pub fn build_roles_reorder(scope: &str, order: &[String]) -> Result<String, String> {
    Request::new(Command::RolesReorder {
        scope: scope.to_string(),
        order: order.to_vec(),
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

pub fn build_role_rename(scope: &str, old: &str, new: &str) -> Result<String, String> {
    // Both names ride one trailing as a comma pair, so neither may contain one.
    if old.contains(',') || new.contains(',') {
        return Err("a role name cannot contain a comma".to_string());
    }
    Request::new(Command::RoleRename {
        scope: scope.to_string(),
        old: old.to_string(),
        new: new.to_string(),
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

/// `EMOJI ADD <ns> <name> <media>` — add/replace a namespace custom emoji.
pub fn build_emoji_add(namespace: &str, name: &str, media: &str) -> Result<String, String> {
    let namespace: weft_proto::NamespaceName =
        namespace.parse().map_err(|_| "bad namespace".to_string())?;
    Request::new(Command::EmojiAdd {
        namespace,
        name: name.to_string(),
        media: media.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `EMOJI REMOVE <ns> <name>` — remove a namespace custom emoji.
pub fn build_emoji_remove(namespace: &str, name: &str) -> Result<String, String> {
    let namespace: weft_proto::NamespaceName =
        namespace.parse().map_err(|_| "bad namespace".to_string())?;
    Request::new(Command::EmojiRemove {
        namespace,
        name: name.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `EMOJI LIST <ns>` — a namespace's custom emoji as a `BATCH`.
pub fn build_emoji_list(namespace: &str) -> Result<String, String> {
    let namespace: weft_proto::NamespaceName =
        namespace.parse().map_err(|_| "bad namespace".to_string())?;
    Request::new(Command::EmojiList { namespace })
        .serialize()
        .map_err(|e| e.to_string())
}

/// `SEARCH <#chan> :<query>` — message search; matches return as a `BATCH`.
pub fn build_search(channel: &str, query: &str) -> Result<String, String> {
    let channel: weft_proto::ChannelName =
        channel.parse().map_err(|_| "bad channel".to_string())?;
    Request::new(Command::Search {
        channel,
        query: query.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `THREADS <#chan>` — list the channel's threads as a `BATCH` (§9.4).
pub fn build_threads(channel: &str) -> Result<String, String> {
    let channel: weft_proto::ChannelName =
        channel.parse().map_err(|_| "bad channel".to_string())?;
    Request::new(Command::Threads { channel })
        .serialize()
        .map_err(|e| e.to_string())
}

/// `THREAD NAME <#chan> <root> [:name]` — set/clear a thread's name (§9.4).
/// An empty `name` clears it.
pub fn build_thread_name(channel: &str, root: &str, name: &str) -> Result<String, String> {
    let channel: weft_proto::ChannelName =
        channel.parse().map_err(|_| "bad channel".to_string())?;
    let root: weft_proto::MsgId = root.parse().map_err(|_| "bad msgid".to_string())?;
    Request::new(Command::ThreadName {
        channel,
        root,
        name: Some(name.to_string()).filter(|n| !n.is_empty()),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `FRIEND ADD <user@net>` — send/accept a friend request (social layer).
/// `user` must be fully qualified (`account@network`); the caller qualifies
/// bare handles to the local network first.
pub fn build_friend_add(user: &str) -> Result<String, String> {
    Request::new(Command::FriendAdd {
        user: friend_user(user)?,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `FRIEND ACCEPT <user@net>` — accept a pending incoming request.
pub fn build_friend_accept(user: &str) -> Result<String, String> {
    Request::new(Command::FriendAccept {
        user: friend_user(user)?,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `FRIEND REMOVE <user@net>` — unfriend / cancel / decline.
pub fn build_friend_remove(user: &str) -> Result<String, String> {
    Request::new(Command::FriendRemove {
        user: friend_user(user)?,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `FRIENDS` — list friends + pending requests (a `BATCH` of `FRIEND`).
pub fn build_friends() -> Result<String, String> {
    Request::new(Command::Friends)
        .serialize()
        .map_err(|e| e.to_string())
}

fn friend_user(user: &str) -> Result<weft_proto::UserRef, String> {
    user.parse()
        .map_err(|_| "friend must be account@network".to_string())
}

// ---- group DMs (social layer) ----

/// `GROUP CREATE <user@net>…` — `members` are qualified `account@network`.
pub fn build_group_create(members: &[String]) -> Result<String, String> {
    let members = members
        .iter()
        .map(|m| m.parse())
        .collect::<Result<Vec<weft_proto::UserRef>, _>>()
        .map_err(|_| "members must be account@network".to_string())?;
    Request::new(Command::GroupCreate { members })
        .serialize()
        .map_err(|e| e.to_string())
}

fn group_id(id: &str) -> Result<weft_proto::GroupId, String> {
    id.parse().map_err(|_| "bad group id".to_string())
}

pub fn build_group_add(group: &str, user: &str) -> Result<String, String> {
    Request::new(Command::GroupAdd {
        group: group_id(group)?,
        user: friend_user(user)?,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

pub fn build_group_remove(group: &str, user: &str) -> Result<String, String> {
    Request::new(Command::GroupRemove {
        group: group_id(group)?,
        user: friend_user(user)?,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

pub fn build_group_leave(group: &str) -> Result<String, String> {
    Request::new(Command::GroupLeave {
        group: group_id(group)?,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

pub fn build_group_name(group: &str, name: &str) -> Result<String, String> {
    Request::new(Command::GroupName {
        group: group_id(group)?,
        name: Some(name.to_string()).filter(|n| !n.is_empty()),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

pub fn build_groups() -> Result<String, String> {
    Request::new(Command::Groups)
        .serialize()
        .map_err(|e| e.to_string())
}

pub fn build_group_call(group: &str) -> Result<String, String> {
    Request::new(Command::GroupCall {
        group: group_id(group)?,
        media: None, // the host network mints the relay leg
    })
    .serialize()
    .map_err(|e| e.to_string())
}

pub fn build_group_call_leave(group: &str) -> Result<String, String> {
    Request::new(Command::GroupCallLeave {
        group: group_id(group)?,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

// ---- friend calls (social layer; 1:1, keyed by peer account@network) ----
pub fn build_call(user: &str) -> Result<String, String> {
    Request::new(Command::Call {
        user: friend_user(user)?,
        media: None, // the caller's network pre-mints cross-network media
    })
    .serialize()
    .map_err(|e| e.to_string())
}
pub fn build_call_accept(user: &str) -> Result<String, String> {
    Request::new(Command::CallAccept {
        user: friend_user(user)?,
    })
    .serialize()
    .map_err(|e| e.to_string())
}
pub fn build_call_decline(user: &str) -> Result<String, String> {
    Request::new(Command::CallDecline {
        user: friend_user(user)?,
    })
    .serialize()
    .map_err(|e| e.to_string())
}
pub fn build_call_end(user: &str) -> Result<String, String> {
    Request::new(Command::CallEnd {
        user: friend_user(user)?,
    })
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

/// `MODLIST <scope>` — list the moderation deny-list (mutes + bans, §6.7).
pub fn build_mod_list(scope: &str) -> Result<String, String> {
    Request::new(Command::ModList {
        scope: scope.to_string(),
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

/// `CHANNEL CREATE <#chan> [policy] [voice]` — optional retention (else server
/// default) and kind (§6.3, §16). `kind` is `"voice"` for a voice channel, else
/// text.
pub fn build_channel_create(
    channel: &str,
    policy: Option<&str>,
    kind: Option<&str>,
) -> Result<String, String> {
    let channel: weft_proto::ChannelName =
        channel.parse().map_err(|_| "bad channel".to_string())?;
    let policy = match policy {
        Some(p) if !p.is_empty() => Some(
            p.parse::<weft_proto::RetentionPolicy>()
                .map_err(|_| "bad policy".to_string())?,
        ),
        _ => None,
    };
    let kind = match kind.filter(|k| !k.is_empty()) {
        Some(k) => k.parse().map_err(|_| "bad channel kind".to_string())?,
        None => weft_proto::ChannelKind::Text,
    };
    Request::new(Command::ChannelCreate {
        channel,
        policy,
        kind,
    })
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

/// `HISTORY <target> [before=] [thread=] limit=50` — a backfill page (§6.4).
/// `before` is the oldest msgid already held (scroll-up paging); `thread`
/// restricts to a single thread (§9.4).
pub fn build_history(
    target: &str,
    before: Option<String>,
    thread: Option<String>,
) -> Result<String, String> {
    let target: Target = target.parse().map_err(|_| "bad target".to_string())?;
    let before = match before.filter(|b| !b.is_empty()) {
        Some(b) => Some(
            b.parse::<MsgId>()
                .map_err(|_| "bad before msgid".to_string())?,
        ),
        None => None,
    };
    let thread = match thread.filter(|t| !t.is_empty()) {
        Some(t) => Some(
            t.parse::<MsgId>()
                .map_err(|_| "bad thread msgid".to_string())?,
        ),
        None => None,
    };
    Request::new(Command::History {
        target,
        before,
        after: None,
        limit: Some(50),
        thread,
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
    let t = target
        .trim()
        .strip_prefix("weft://")
        .unwrap_or(target.trim());
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

// ---- §10.3 display profiles ----

/// `PROFILE SET` — set your own display name + avatar (§10.3). Each arg is
/// `None` to leave that field unchanged, `Some("")` to clear it, or `Some(v)`
/// to set it (`avatar` is the blob's BLAKE3 hash).
pub fn build_profile_set(display: Option<&str>, avatar: Option<&str>) -> Result<String, String> {
    Request::new(Command::ProfileSet {
        display: display.map(String::from),
        avatar: avatar.map(String::from),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `PROFILES <account>...` — query display profiles (§10.3).
pub fn build_profiles_query(accounts: Vec<String>) -> Result<String, String> {
    if accounts.is_empty() {
        return Err("no accounts".to_string());
    }
    Request::new(Command::ProfilesQuery { accounts })
        .serialize()
        .map_err(|e| e.to_string())
}

/// `VERIFY EMAIL <address>` (§10.5) — claim an email; the server mails a code.
pub fn build_verify_email(address: &str) -> Result<String, String> {
    Request::new(Command::VerifyEmail {
        address: address.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `VERIFY BIRTHDAY <YYYY-MM-DD>` (§10.5) — self-attest a birth date.
pub fn build_verify_birthday(date: &str) -> Result<String, String> {
    Request::new(Command::VerifyBirthday {
        date: date.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `VERIFY CONFIRM <kind> <code>` (§10.5) — prove a claim with its mailed code.
pub fn build_verify_confirm(kind: &str, code: &str) -> Result<String, String> {
    Request::new(Command::VerifyConfirm {
        kind: kind.to_string(),
        code: code.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `VERIFY LIST` (§10.5) — the caller's own verification claims.
pub fn build_verify_list() -> Result<String, String> {
    Request::new(Command::VerifyList)
        .serialize()
        .map_err(|e| e.to_string())
}

// ---- §16 WEFT-RT voice signaling ----

/// `VOICE JOIN <#chan>` — request to join a channel's voice room (§16).
pub fn build_voice_join(channel: &str) -> Result<String, String> {
    let channel: weft_proto::ChannelName =
        channel.parse().map_err(|_| "bad channel".to_string())?;
    Request::new(Command::VoiceJoin { channel })
        .serialize()
        .map_err(|e| e.to_string())
}

/// `VOICE LEAVE <#chan>` — leave a channel's voice room (§16).
pub fn build_voice_leave(channel: &str) -> Result<String, String> {
    let channel: weft_proto::ChannelName =
        channel.parse().map_err(|_| "bad channel".to_string())?;
    Request::new(Command::VoiceLeave { channel })
        .serialize()
        .map_err(|e| e.to_string())
}

/// `VOICE DESC <#chan> :<sdp>` — an SDP offer for the channel's peer (§16). The
/// raw SDP rides the trailing; the codec escapes its CR/LF for the wire.
pub fn build_voice_desc(channel: &str, sdp: &str) -> Result<String, String> {
    let channel: weft_proto::ChannelName =
        channel.parse().map_err(|_| "bad channel".to_string())?;
    Request::new(Command::VoiceDesc {
        channel,
        sdp: sdp.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `VOICE CAND <#chan> :<ice-candidate>` — a trickle-ICE candidate (§16).
pub fn build_voice_cand(channel: &str, candidate: &str) -> Result<String, String> {
    let channel: weft_proto::ChannelName =
        channel.parse().map_err(|_| "bad channel".to_string())?;
    Request::new(Command::VoiceCand {
        channel,
        candidate: candidate.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}
