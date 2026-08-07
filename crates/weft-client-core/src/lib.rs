//! Portable WEFT client codec (native + wasm): reply-line parsing into
//! structured `ClientEvent`s, the §6.1/§3.3 auth FSM, and command-line
//! builders. No transport, runtime, or UI toolkit — bindings own the loop,
//! the stream, and the `EventSink`.

use serde::Serialize;
use weft_crypto::{sign_challenge, signature_to_b64, Keypair};
use weft_proto::{Command, Event, MsgId, NsInfoKind, Reply, Request, Target};

/// Client-core-owned application model + the reduce step (the model migration:
/// `docs/architecture/client-core-model-migration.md`). Kept strictly separate
/// from this codec layer: `ClientEvent` above is the *wire* vocabulary; the model
/// emits its own `StateDiff` vocabulary, and per-domain event handlers live in
/// `model::<domain>` (mirroring the TS per-domain handler maps). Pure, WASM-safe.
pub mod model;

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
    /// Handshake only: send HELLO, read the negotiation WELCOME to learn the
    /// server's `features=` (notably `email-required`), emit `ServerInfo`, and
    /// stop — never authenticate. The connect screen uses it to decide whether
    /// to show the REGISTER email field before the user submits credentials.
    Probe,
}

impl Mode {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "login" => Ok(Mode::Login),
            "register" => Ok(Mode::Register),
            "key" => Ok(Mode::Key),
            "probe" => Ok(Mode::Probe),
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
    /// §12.5 the action catalog — every registered plugin and what it declared,
    /// so the client knows which surfaces to offer. Answers `PLUGINS`.
    PluginManifest {
        /// The catalog as JSON, decoded from the wire's b64 CBOR so the frontend
        /// does not carry a CBOR decoder.
        catalog: String,
    },
    /// §11.2 a plugin answered an invocation (or a step) with a view to render.
    /// `view_id` correlates every later step, and `label` echoes the request that
    /// produced it — `None` on an unsolicited push.
    PluginView {
        view_id: String,
        view: String,
        label: Option<String>,
    },
    /// §11.4 an update to an open view, unsolicited — the client applies the ops
    /// rather than replacing what it has.
    PluginPatch {
        view_id: String,
        patch: String,
    },
    /// §11.5 the flow ended: a toast, a navigation, a close, a refresh. The view
    /// is gone after this.
    PluginResult {
        view_id: String,
        result: String,
        label: Option<String>,
    },
    /// The negotiation WELCOME, surfaced before auth. Carries what the connect
    /// screen needs to shape the login/register form for this homeserver —
    /// notably whether an email is required at REGISTER (`features=email-required`,
    /// §3.6). Emitted on the first WELCOME of every connection, including a
    /// `Mode::Probe` handshake that stops right here.
    ServerInfo {
        network: String,
        email_required: bool,
        /// The server has a real mailer configured (`features=email`, §10.5), so
        /// verification/reset codes are actually deliverable. The connect screen
        /// and the "no email on file" nudge use this — there's no point asking
        /// for an email a server can never mail to.
        email_available: bool,
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
        /// The request `label` (§3.5) when this is our own echoed copy — the key an
        /// optimistic send reconciles against, local or federated (§11.13).
        label: Option<String>,
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
    /// `GRANT-INFO <scope> <subject> :<caps>` — one per-subject grant at a
    /// scope, in the `GRANTS` batch (§6.5).
    GrantInfo {
        scope: String,
        subject: String,
        caps: String,
    },
    /// `ROLE <scope> <color> <caps> :<name>` — a role definition (§6.5).
    Role {
        scope: String,
        /// Stable role ULID id (v0.13) — the identity commands address; `name`
        /// is the mutable display label.
        role: String,
        color: String,
        caps: String,
        hoist: bool,
        pingable: bool,
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
        /// Stable namespace ULID id (v0.13) — the identity commands address.
        id: String,
        /// The mutable per-network-unique vanity name (display + IRC addressing).
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
        /// Foreign-bridge §7a.2: the origin URI of a provider-managed replica
        /// (badge "Matrix · matrix.org"); `None` = native.
        origin: Option<String>,
        /// Provider liveness for a replica (`None` = native): the client shows a
        /// "bridge offline" indicator when `Some(false)`.
        provider_online: Option<bool>,
        /// matrix.md §17.1 outbound projection opt-ins: the foreign schemes this
        /// **native** namespace is projected into (`["matrix"]`). Empty = not
        /// projected. The owner toggles it per scheme in Server Settings, so the
        /// client needs the current state to render the switch.
        bridges: Vec<String>,
        /// §7a.3 the capability profile: how the client should render this
        /// namespace's authority (`roles` | `levels` | `none`), and which native
        /// settings surfaces to hide. Absent authority = the native default.
        ///
        /// **Display gating only** — it grants nothing and enforces nothing; the
        /// point is not to offer buttons the server would refuse.
        authority: Option<String>,
        settings_disabled: Vec<String>,
    },
    /// `CHANNEL-LAYOUT <#chan> <position>` with optional `category=`/`kind=` (§7).
    /// `channel_kind` is `text` (default) or `voice` (§16 voice-only room) —
    /// named to avoid clashing with the enum's `kind` serde tag.
    ChannelLayout {
        channel: String,
        category: Option<String>,
        position: i64,
        channel_kind: String,
        /// Human display name for the channel (v0.13); empty for none.
        vanity: String,
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
    /// `NETBLOCK-REMOVED <network>` — a network was un-blocked (§11.6).
    NetblockRemoved {
        network: String,
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
        /// How many times this invite has been redeemed (§6.5).
        used: u32,
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
    /// `SYNC END` (v0.12 §6.9) — a `SYNC` response finished; `cursor` is the
    /// opaque token to store on this device and echo on the next `SYNC since=`.
    SyncEnd {
        cursor: String,
    },
    /// `CHANSYNC` (v0.12 §7.9) — a per-channel header in a SYNC body/delta.
    /// `reset` means drop cached rows for the channel; `expired_before` is the
    /// retention watermark (evict older).
    ChanSync {
        channel: String,
        expired_before: Option<String>,
        reset: bool,
    },
    /// `NS-MEMBER` (v0.12 §7.4) — namespace-level join/part; the client expands
    /// it across the namespace's visible channels.
    NsMember {
        namespace: String,
        user: String,
        network: String,
        action: String,
        count: Option<u64>,
    },
    /// `NS-MEMBER-INFO <ns> <user@net> <joined-ms> [roles=…]` — one row of the
    /// `NS INFO MEMBERS` moderator roster: a member with their join time (ms,
    /// `0` when unknown) and assigned ns-scoped role names.
    NsMemberInfo {
        namespace: String,
        user: String,
        network: String,
        joined_ms: u64,
        roles: Vec<String>,
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
        /// §10.3 free-text bio.
        about: Option<String>,
        /// §10.3 free-text custom status (shown inline in member lists).
        status: Option<String>,
    },
    /// §10.3 a per-namespace display name (server nickname) change.
    Nick {
        scope: String,
        account: String,
        network: String,
        nick: String,
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
        /// §3.5: the label of the request this answers, echoed on every direct
        /// response *including* `ERR`. Carrying it is what lets the UI say what
        /// failed — §8 codes are deliberately uninformative on their own
        /// (invariant 1: one code for absent / private / gated), so context has to
        /// come from the request the client itself sent.
        label: Option<String>,
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
    email: Option<&str>,
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
        (
            Phase::HelloSent,
            Event::Welcome {
                network, features, ..
            },
        ) => {
            *net_name = network.to_string(); // needed to sign the key challenge
                                             // Surface the server's shape before we authenticate (§3.6).
            sink.emit(ClientEvent::ServerInfo {
                network: network.to_string(),
                email_required: features.iter().any(|f| f == "email-required"),
                email_available: features.iter().any(|f| f == "email"),
            });
            // A probe stops here — it only wanted the WELCOME. The binding tears
            // the connection down once it has the `ServerInfo`.
            if mode == Mode::Probe {
                return None;
            }
            *phase = Phase::AuthSent;
            return Some(match mode {
                Mode::Login => format!("AUTH PASSWORD {account} :{password}"),
                Mode::Register => match email {
                    // §6.1: `REGISTER <account> [<email>] :<password>`.
                    Some(email) if !email.is_empty() => {
                        format!("REGISTER {account} {email} :{password}")
                    }
                    _ => format!("REGISTER {account} :{password}"),
                },
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
                Mode::Probe => return None, // handled above, before auth
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
    // §3.5 the request label rides our own echoed copies (MESSAGE/EDITED/…) — the
    // client reconciles an optimistic send by matching it (§11.13).
    let label = reply.label.clone();
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
        // v0.12 SYNC framing (§6.9/§7.9). The body/delta message rows arrive as
        // ordinary Message/Reactions/Deleted events the frontend already
        // upserts; these carry the cursor + per-channel headers around them.
        Event::SyncEnd { cursor } => sink.emit(ClientEvent::SyncEnd { cursor }),
        Event::ChanSync {
            channel,
            expired_before,
            reset,
        } => sink.emit(ClientEvent::ChanSync {
            channel: channel.to_string(),
            expired_before: expired_before.map(|m| m.to_string()),
            reset,
        }),
        // SYNC START just brackets the body stream; the frontend keys off the
        // per-channel CHANSYNC headers, so it needs no distinct event.
        Event::SyncStart | Event::SyncBody { .. } => {}
        Event::NsMember {
            namespace,
            user,
            action,
            count,
            ..
        } => sink.emit(ClientEvent::NsMember {
            namespace: namespace.to_string(),
            user: user.account.to_string(),
            network: user.network.to_string(),
            action: action.to_string(),
            count,
        }),
        Event::NsMemberInfo {
            namespace,
            user,
            joined_ms,
            roles,
        } => sink.emit(ClientEvent::NsMemberInfo {
            namespace: namespace.to_string(),
            user: user.account.to_string(),
            network: user.network.to_string(),
            joined_ms,
            roles,
        }),
        Event::MediaToken { token } => sink.emit(ClientEvent::MediaToken { token }),
        // §6/§13 a HISTORY over the stream threshold — pull it off the data plane.
        Event::StreamAccept { token } => sink.emit(ClientEvent::Backfill { token }),
        Event::Message(m) => sink.emit(ClientEvent::Message {
            target: m.target.to_string(),
            sender: m.sender.account.to_string(),
            network: m.sender.network.to_string(),
            msgid: m.msgid.to_string(),
            // Identity is `account@network`, never the account alone: a bridged
            // realm can carry the *same* handle on a different network (your
            // Matrix `ada@teamnight.app` beside your local `ada`), and comparing
            // bare names badged that stranger's messages as your own.
            own: m.sender.account.as_str() == account && m.sender.network.as_str() == net_name,
            history: *in_batch,
            edited: m.edited.is_some(),
            reply_to: m.meta.reply_to.as_ref().map(|r| r.to_string()),
            thread: m.meta.thread.as_ref().map(|t| t.to_string()),
            md: m.meta.fmt.as_deref() == Some("md"),
            attachments: m.meta.attachments.clone(),
            system: m.meta.system.clone(),
            label,
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
        Event::GrantInfo {
            scope,
            subject,
            caps,
        } => sink.emit(ClientEvent::GrantInfo {
            scope,
            subject: subject.to_string(),
            caps,
        }),
        Event::Role {
            scope,
            role,
            color,
            caps,
            hoist,
            pingable,
            position,
            name,
        } => sink.emit(ClientEvent::Role {
            scope,
            role: role.to_string(),
            color,
            caps,
            hoist,
            pingable,
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
        // §12.5/§11: the plugin surface. The wire carries these as base64 CBOR;
        // decode them to JSON here so the frontend needs no CBOR decoder — and so
        // a malformed payload is dropped at the boundary rather than blowing up a
        // renderer.
        Event::PluginManifest { catalog } => {
            if let Some(catalog) = cbor_b64_to_json::<weft_proto::Catalog>(&catalog) {
                sink.emit(ClientEvent::PluginManifest { catalog });
            }
        }
        Event::PluginView { view_id, view } => {
            if let Some(view) = cbor_b64_to_json::<weft_proto::View>(&view) {
                sink.emit(ClientEvent::PluginView {
                    view_id,
                    view,
                    label: label.clone(),
                });
            }
        }
        Event::PluginPatch { view_id, patch } => {
            if let Some(patch) = cbor_b64_to_json::<Vec<weft_proto::PatchOp>>(&patch) {
                sink.emit(ClientEvent::PluginPatch { view_id, patch });
            }
        }
        Event::PluginResult { view_id, result } => {
            if let Some(result) = cbor_b64_to_json::<weft_proto::ViewResult>(&result) {
                sink.emit(ClientEvent::PluginResult {
                    view_id,
                    result,
                    label: label.clone(),
                });
            }
        }
        Event::NsMeta {
            id,
            vanity,
            visibility,
            owner,
            title,
            description,
            recovery_set,
            recovery_pending,
            categories,
            federation,
            origin,
            provider_online,
            authority,
            settings_disabled,
            bridges,
            ..
        } => sink.emit(ClientEvent::NsMeta {
            id: id.to_string(),
            name: vanity.to_string(),
            visibility: visibility.to_string(),
            owner,
            title,
            description,
            recovery_set,
            recovery_eta: recovery_pending.map(|(eta, _)| eta),
            recovery_rung: recovery_pending.map(|(_, rung)| rung),
            categories,
            federation,
            origin: origin.map(|o| o.to_string()),
            authority: authority.map(|a| a.to_string()),
            settings_disabled,
            provider_online,
            bridges: bridges.iter().map(|b| b.to_string()).collect(),
        }),
        Event::ChannelLayout {
            channel,
            category,
            position,
            kind,
            vanity,
            ..
        } => sink.emit(ClientEvent::ChannelLayout {
            channel: channel.to_string(),
            category,
            position,
            channel_kind: kind.to_string(),
            vanity,
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
            used,
            expiry,
        } => sink.emit(ClientEvent::InviteInfo {
            scope,
            invite_id,
            creator: creator.to_string(),
            uses_left,
            used,
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
            ..
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
            about,
            status,
        } => sink.emit(ClientEvent::Profile {
            account: user.account.to_string(),
            network: user.network.to_string(),
            display,
            avatar,
            about,
            status,
        }),
        // §10.3 per-namespace server nicknames.
        Event::Nick { scope, user, nick } => sink.emit(ClientEvent::Nick {
            scope,
            account: user.account.to_string(),
            network: user.network.to_string(),
            nick,
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
            label,
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
        Event::NetblockRemoved { network } => sink.emit(ClientEvent::NetblockRemoved {
            network: network.to_string(),
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
    label: Option<String>,
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
    let cmd = weft_proto::Command::Msg {
        target,
        body: Some(body.to_string()),
        meta,
    };
    // §3.5/§11.13 the label is the send correlation: the server echoes it on our
    // own `MESSAGE` copy (locally, or re-attached by our server for a message a
    // home-authoritative channel minted elsewhere), so the client reconciles its
    // optimistic placeholder by label — one mechanism, local or federated.
    match label.filter(|l| !l.is_empty()) {
        Some(l) => weft_proto::Request::with_label(cmd, &l),
        None => weft_proto::Request::new(cmd),
    }
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

/// `INVITE MINT <scope> [max-uses=] [expiry=]` — shareable invite for a
/// channel/namespace (§6.5). `max_uses`/`expiry` (TTL seconds) `None` = unlimited.
pub fn build_invite_mint(
    scope: &str,
    max_uses: Option<u32>,
    expiry: Option<u64>,
) -> Result<String, String> {
    Request::new(Command::InviteMint {
        scope: scope.to_string(),
        max_uses,
        expiry,
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
pub fn build_mark(channel: &str, msgid: &str, label: Option<&str>) -> Result<String, String> {
    let channel: weft_proto::ChannelName =
        channel.parse().map_err(|_| "bad channel".to_string())?;
    let msgid: MsgId = msgid.parse().map_err(|_| "bad msgid".to_string())?;
    let command = Command::Mark { channel, msgid };

    // Labelled because the client marks read on its own initiative: an unjoined
    // channel answers `CAP-REQUIRED`, which is ours to swallow (§3.5).
    let request = match label {
        Some(label) => Request::with_label(command, label),
        None => Request::new(command),
    };

    request.serialize().map_err(|e| e.to_string())
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

/// `GRANTS <scope>` — per-subject grants at a scope (the channel-permission
/// editor's individual-member overrides) as a `BATCH` of `GRANT-INFO` (§6.5).
pub fn build_grants_at(scope: &str) -> Result<String, String> {
    Request::new(Command::GrantsAt {
        scope: scope.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `NS INFO MEMBERS <ns>` — the moderator roster (members + join times +
/// assigned roles) as a `BATCH` of `NS-MEMBER-INFO`.
pub fn build_ns_info_members(namespace: &str) -> Result<String, String> {
    let ns: weft_proto::NamespaceId = namespace.parse().map_err(|_| "bad namespace".to_string())?;
    Request::new(Command::NsInfo {
        ns,
        detail: NsInfoKind::Members,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

#[allow(clippy::too_many_arguments)]
pub fn build_role_create(
    scope: &str,
    color: &str,
    caps: &str,
    hoist: bool,
    pingable: bool,
    position: i32,
    name: &str,
) -> Result<String, String> {
    Request::new(Command::RoleCreate {
        scope: scope.to_string(),
        color: color.to_string(),
        caps: caps.to_string(),
        hoist,
        pingable,
        position,
        name: name.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

pub fn build_roles_reorder(scope: &str, order: &[String]) -> Result<String, String> {
    // Order is a list of role **ids** (v0.13).
    let order = order
        .iter()
        .map(|r| {
            r.parse::<weft_proto::RoleId>()
                .map_err(|_| "bad role id".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    Request::new(Command::RolesReorder {
        scope: scope.to_string(),
        order,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

pub fn build_role_delete(scope: &str, role: &str) -> Result<String, String> {
    Request::new(Command::RoleDelete {
        scope: scope.to_string(),
        role: role.parse().map_err(|_| "bad role id".to_string())?,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `ROLE UPDATE <scope> <role-id> …` — edit a role in place by its id (v0.13);
/// subsumes the old `ROLE RENAME` (pass the new label as `name`).
#[allow(clippy::too_many_arguments)]
pub fn build_role_update(
    scope: &str,
    role: &str,
    color: &str,
    caps: &str,
    hoist: bool,
    pingable: bool,
    position: i32,
    name: &str,
) -> Result<String, String> {
    Request::new(Command::RoleUpdate {
        scope: scope.to_string(),
        role: role.parse().map_err(|_| "bad role id".to_string())?,
        color: color.to_string(),
        caps: caps.to_string(),
        hoist,
        pingable,
        position,
        name: name.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

pub fn build_role_assign(scope: &str, account: &str, role: &str) -> Result<String, String> {
    Request::new(Command::RoleAssign {
        scope: scope.to_string(),
        account: account.parse().map_err(|_| "bad account".to_string())?,
        role: role.parse().map_err(|_| "bad role id".to_string())?,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

pub fn build_role_unassign(scope: &str, account: &str, role: &str) -> Result<String, String> {
    Request::new(Command::RoleUnassign {
        scope: scope.to_string(),
        account: account.parse().map_err(|_| "bad account".to_string())?,
        role: role.parse().map_err(|_| "bad role id".to_string())?,
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
    let namespace: weft_proto::NamespaceId =
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
    let namespace: weft_proto::NamespaceId =
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
    let namespace: weft_proto::NamespaceId =
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
///
/// The label matters here because the client fetches a roster *speculatively*,
/// on opening a channel, and the answer can be a §8 error the user never asked
/// for — `CAP-REQUIRED` when our belief that we're joined is stale. Labelling
/// the request is what lets the frontend recognise its own background fetch
/// (§3.5) instead of toasting a bare wire code.
pub fn build_members_labeled(channel: &str, label: Option<&str>) -> Result<String, String> {
    let channel: weft_proto::ChannelName =
        channel.parse().map_err(|_| "bad channel".to_string())?;
    let command = Command::Members {
        channel,
        cursor: None,
    };

    let request = match label {
        Some(label) => Request::with_label(command, label),
        None => Request::new(command),
    };

    request.serialize().map_err(|e| e.to_string())
}

/// `MEMBERS <#chan>` with no label — a roster the user explicitly asked for.
pub fn build_members(channel: &str) -> Result<String, String> {
    build_members_labeled(channel, None)
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
    let namespace: weft_proto::NamespaceId =
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
    label: Option<&str>,
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
    let command = Command::History {
        target,
        before,
        after: None,
        limit: Some(50),
        thread,
    };

    // Labelled: history is fetched on opening a channel, not on a user's command,
    // so its refusal is the client's to handle rather than the user's to read.
    let request = match label {
        Some(label) => Request::with_label(command, label),
        None => Request::new(command),
    };

    request.serialize().map_err(|e| e.to_string())
}

/// `NS CREATE <name> <tier>` with `@root=<b64-pubkey>` (§6.2). The keypair is
/// generated + stored by [`crate::keys`]; only the public key rides the wire.
pub fn build_ns_create(vanity: &str, visibility: &str, root_key: &str) -> Result<String, String> {
    let vanity: weft_proto::VanityName =
        vanity.parse().map_err(|_| "bad vanity name".to_string())?;
    let visibility: weft_proto::Visibility = visibility
        .parse()
        .map_err(|_| "bad visibility".to_string())?;
    Request::new(Command::NsCreate {
        vanity,
        visibility,
        root_key: root_key.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `NS META <ns-id> <key> :<value>` — title/description/icon/vanity (§6.2). The
/// `vanity` key renames the namespace's mutable label (v0.13).
pub fn build_ns_meta(ns: &str, key: &str, value: &str) -> Result<String, String> {
    Request::new(Command::NsMeta {
        ns: ns.parse().map_err(|_| "bad namespace id".to_string())?,
        key: key.to_string(),
        value: value.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `FEDERATE <network>/<namespace>` with an optional `@invite=` (§11.10) —
/// request an on-demand bridge to a foreign namespace. Accepts
/// `network/namespace` or a `weft://<net>/<ns>` link. A non-empty `invite`
/// unlocks a non-public but federation-open foreign namespace.
pub fn build_federate(target: &str, invite: Option<&str>) -> Result<String, String> {
    let t = target
        .trim()
        .strip_prefix("weft://")
        .unwrap_or(target.trim());
    let (net, ns) = t.split_once('/').ok_or("expected network/namespace")?;
    Request::new(Command::Federate {
        network: net.parse().map_err(|_| "bad network".to_string())?,
        namespace: ns.parse().map_err(|_| "bad namespace".to_string())?,
        invite: invite
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `NS VISIBILITY <name> <tier>` (§6.2).
pub fn build_ns_visibility(ns: &str, visibility: &str) -> Result<String, String> {
    Request::new(Command::NsVisibility {
        ns: ns.parse().map_err(|_| "bad namespace id".to_string())?,
        visibility: visibility
            .parse()
            .map_err(|_| "bad visibility".to_string())?,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `NS DELEGATE <ns-id> <subject> <caps>` — delegate ns caps (§6.2).
pub fn build_ns_delegate(ns: &str, subject: &str, caps: &str) -> Result<String, String> {
    Request::new(Command::NsDelegate {
        ns: ns.parse().map_err(|_| "bad namespace id".to_string())?,
        subject: subject.to_string(),
        caps: caps.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `NS DELETE <ns-id> <ns-id>` — confirmed by repetition (§6.2).
pub fn build_ns_delete(ns: &str) -> Result<String, String> {
    let ns: weft_proto::NamespaceId = ns.parse().map_err(|_| "bad namespace id".to_string())?;
    Request::new(Command::NsDelete { ns, confirm: ns })
        .serialize()
        .map_err(|e| e.to_string())
}

/// `NS LEAVE <ns-id>` — drop your own membership in a namespace (§6.2).
pub fn build_ns_leave(ns: &str) -> Result<String, String> {
    let ns: weft_proto::NamespaceId = ns.parse().map_err(|_| "bad namespace id".to_string())?;
    Request::new(Command::NsLeave { ns })
        .serialize()
        .map_err(|e| e.to_string())
}

/// `NS TRANSFER <ns-id> <account>` with `@sig=` — root-signed succession (§2.4).
/// The signature is produced from the stored root key by the caller.
pub fn build_ns_transfer(ns: &str, new_owner: &str, signature: &str) -> Result<String, String> {
    Request::new(Command::NsTransfer {
        ns: ns.parse().map_err(|_| "bad namespace id".to_string())?,
        new_owner: new_owner.parse().map_err(|_| "bad account".to_string())?,
        signature: signature.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `NS RECOVERY SET <ns-id> <m> <keys>` — designate the M-of-N quorum (§2.4).
pub fn build_ns_recovery_set(ns: &str, m: u32, keys: &str) -> Result<String, String> {
    Request::new(Command::NsRecoverySet {
        ns: ns.parse().map_err(|_| "bad namespace id".to_string())?,
        m,
        keys: keys.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `NS RECOVER <ns-id> <b64-rotation-record>` — submit a co-signed rotation.
pub fn build_ns_recover(ns: &str, rotation: &str) -> Result<String, String> {
    Request::new(Command::NsRecover {
        ns: ns.parse().map_err(|_| "bad namespace id".to_string())?,
        rotation: rotation.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `NS RECOVERY CANCEL <ns-id>` with `@sig=` — root veto of a pending recovery.
pub fn build_ns_recovery_cancel(ns: &str, signature: &str) -> Result<String, String> {
    Request::new(Command::NsRecoveryCancel {
        ns: ns.parse().map_err(|_| "bad namespace id".to_string())?,
        signature: signature.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `NS JOIN <target>` — the id or vanity name of a local namespace (§6.2, §2.2
/// unlisted-by-name), **or** a `<scheme>://<realm>/<space>` URI to consume a
/// foreign one through a registered provider (framework §3.3).
///
/// Routes on the target's *shape*, which is what the server's own parser does with
/// the same verb — so one function can't drift from the wire, and a caller doesn't
/// have to know which kind of thing it holds. `matrix://teamnight.org/myspace` and
/// `gaming` both belong here.
///
/// `label` correlates the failure back to this attempt (§3.5). Worth passing for a
/// foreign join especially: it can fail for reasons the server will not distinguish,
/// and the label is what lets the UI explain the possibilities instead of printing
/// `NO-SUCH-TARGET`.
pub fn build_ns_join_labeled(target: &str, label: Option<&str>) -> Result<String, String> {
    let command = if target.contains("://") {
        weft_proto::Command::NsJoinForeign {
            uri: target.parse().map_err(|_| {
                format!(
                    "not a foreign namespace URI: {target} (expected <scheme>://<realm>/<space>)"
                )
            })?,
        }
    } else {
        weft_proto::Command::NsJoin {
            ns: target.parse().map_err(|_| "bad namespace".to_string())?,
        }
    };

    let request = match label {
        Some(label) => weft_proto::Request::with_label(command, label),
        None => weft_proto::Request::new(command),
    };

    request.serialize().map_err(|e| e.to_string())
}

/// `NS JOIN` without correlation — for callers with nothing to say about a failure.
pub fn build_ns_join(target: &str) -> Result<String, String> {
    build_ns_join_labeled(target, None)
}

/// Decode a wire `b64(CBOR)` plugin payload into JSON for the frontend.
///
/// Round-tripping through the typed value rather than passing the blob through
/// means a payload the codec cannot read is dropped **here**, at the boundary,
/// instead of reaching a renderer that has to guess what to do with it.
fn cbor_b64_to_json<T>(payload: &str) -> Option<String>
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let value = weft_proto::plugin_from_b64::<T>(payload).ok()?;

    serde_json::to_string(&value).ok()
}

// ---- §12 plugin surface (plugin-spec.md §11–§13) ----

/// Encode a submitted form's values for the wire.
///
/// The UI works in JSON; the wire wants base64 CBOR. Doing the conversion here
/// keeps CBOR out of the frontend entirely, and rejects malformed input at the
/// boundary rather than sending a payload the plugin cannot read.
///
/// Values are deliberately untyped — the shape is whatever the plugin's own
/// components declared — so this validates that it is a JSON object of
/// `component-id → value`, and nothing further. A `BTreeMap` keeps the encoding
/// deterministic, matching the rest of the codebase.
pub fn plugin_values(json: Option<String>) -> Result<Option<String>, String> {
    let Some(json) = json else {
        return Ok(None);
    };

    let values: std::collections::BTreeMap<String, serde_json::Value> = serde_json::from_str(&json)
        .map_err(|e| format!("form values must be a JSON object: {e}"))?;

    weft_proto::plugin_to_b64(&values)
        .map(Some)
        .map_err(|e| e.to_string())
}

/// `PLUGINS` — fetch the action catalog.
pub fn build_plugins() -> Result<String, String> {
    weft_proto::Request::new(weft_proto::Command::Plugins)
        .serialize()
        .map_err(|e| e.to_string())
}

/// `PLUGIN INVOKE <plugin> <action>` — open a flow. `ctx_ref` names what the
/// action was invoked on (a msgid, a channel, a member), `params` carries any
/// inputs the declaration asked for up front.
pub fn build_plugin_invoke(
    plugin: &str,
    action: &str,
    ctx_ref: Option<String>,
    params: Option<String>,
) -> Result<String, String> {
    weft_proto::Request::new(weft_proto::Command::PluginInvoke {
        plugin: plugin.to_string(),
        action: action.to_string(),
        ctx_ref,
        params,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `PLUGIN SUBMIT <view-id>` — send a form step's values.
pub fn build_plugin_submit(view_id: &str, values: Option<String>) -> Result<String, String> {
    weft_proto::Request::new(weft_proto::Command::PluginSubmit {
        view_id: view_id.to_string(),
        values,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `PLUGIN ACTION <view-id> <button>` — a control click, with the form's current
/// values so a button can act on what is on screen.
pub fn build_plugin_action(
    view_id: &str,
    button: &str,
    values: Option<String>,
) -> Result<String, String> {
    weft_proto::Request::new(weft_proto::Command::PluginAction {
        view_id: view_id.to_string(),
        button: button.to_string(),
        values,
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `PLUGIN SUBSCRIBE|UNSUBSCRIBE <view-id>` — §11.3 panel liveness. A panel only
/// receives patches while subscribed, so a client that hides one should say so
/// rather than letting the plugin push into nothing.
pub fn build_plugin_subscribe(view_id: &str, on: bool) -> Result<String, String> {
    let cmd = if on {
        weft_proto::Command::PluginSubscribe {
            view_id: view_id.to_string(),
        }
    } else {
        weft_proto::Command::PluginUnsubscribe {
            view_id: view_id.to_string(),
        }
    };

    weft_proto::Request::new(cmd)
        .serialize()
        .map_err(|e| e.to_string())
}

/// `PLUGIN CLOSE <view-id>` — the user dismissed it. Terminal: the flow is freed
/// server-side, so nothing more will arrive for this view.
pub fn build_plugin_close(view_id: &str) -> Result<String, String> {
    weft_proto::Request::new(weft_proto::Command::PluginClose {
        view_id: view_id.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

// ---- §10.3 display profiles ----

/// `PROFILE SET` — set your own display name + avatar (§10.3). Each arg is
/// `None` to leave that field unchanged, `Some("")` to clear it, or `Some(v)`
/// to set it (`avatar` is the blob's BLAKE3 hash).
pub fn build_profile_set(
    display: Option<&str>,
    avatar: Option<&str>,
    about: Option<&str>,
    status: Option<&str>,
) -> Result<String, String> {
    Request::new(Command::ProfileSet {
        display: display.map(String::from),
        avatar: avatar.map(String::from),
        about: about.map(String::from),
        status: status.map(String::from),
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

/// `NICK <scope> <account> :<nick>` — set a per-namespace display name (§10.3).
/// Empty `nick` clears it. Own → needs `nick`; another's → `manage-nicks`.
pub fn build_nick(scope: &str, account: &str, nick: &str) -> Result<String, String> {
    Request::new(Command::Nick {
        scope: scope.to_string(),
        account: account.parse().map_err(|_| "bad account".to_string())?,
        nick: nick.to_string(),
    })
    .serialize()
    .map_err(|e| e.to_string())
}

/// `NICKS <scope>` — query a namespace's server nicknames (§10.3).
pub fn build_nicks(scope: &str) -> Result<String, String> {
    Request::new(Command::Nicks {
        scope: scope.to_string(),
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Collect(RefCell<Vec<ClientEvent>>);
    impl EventSink for Collect {
        fn emit(&self, event: ClientEvent) {
            self.0.borrow_mut().push(event);
        }
    }

    /// Feed one already-authed line and return the events it emits.
    fn feed(line: &str) -> Vec<ClientEvent> {
        let sink = Collect::default();
        let mut net = "test.example".to_string();
        let mut phase = Phase::Ready;
        let mut in_batch = false;
        let mut close = false;
        on_line(
            &sink,
            "ada",
            "",
            None,
            Mode::Login,
            None,
            &mut net,
            &mut phase,
            &mut in_batch,
            &mut close,
            line,
        );
        sink.0.into_inner()
    }

    #[test]
    fn own_messages_are_identified_by_account_and_network() {
        /// Was the one emitted message flagged as mine?
        fn own_of(line: &str) -> bool {
            match feed(line).into_iter().next() {
                Some(ClientEvent::Message { own, .. }) => own,
                _ => panic!("expected exactly one MESSAGE event"),
            }
        }

        // The harness is `ada` on `test.example`.
        assert!(own_of(
            "@msgid=test.example/01arz3ndektsv4rrffq69g5fav MESSAGE #general ada@test.example :hi"
        ));

        // A bridged realm can carry the SAME handle on another network — your
        // Matrix self beside your local self. Comparing bare account names
        // badged that stranger's messages as yours ("you" in the client).
        assert!(
            !own_of("@msgid=test.example/01arz3ndektsv4rrffq69g5fbv MESSAGE #general ada@teamnight.app :hi"),
            "same name, different network, is not me"
        );

        // And a plain stranger stays a stranger.
        assert!(!own_of(
            "@msgid=test.example/01arz3ndektsv4rrffq69g5fcv MESSAGE #general bob@test.example :hi"
        ));
    }

    #[test]
    fn a_plugin_view_reaches_the_frontend_as_json() {
        // The wire carries plugin payloads as base64 CBOR. Decoding here means
        // the frontend needs no CBOR decoder, and the label rides along so a
        // client can match the view to the step that asked for it.
        let view = weft_proto::plugin_to_b64(&weft_proto::View {
            container: weft_proto::Container::Modal,
            title: Some("Ban".into()),
            panel_key: None,
            submit_label: Some("Confirm".into()),
            blocks: vec![],
            widget: None,
            params: vec![],
        })
        .unwrap();

        // The payload rides the trailing (§4: a tag value caps at 1024 B, and an
        // SDUI view is a document, not metadata).
        let events = feed(&format!("@label=i1 PLUGIN-VIEW modq:1 :{view}"));
        let [ClientEvent::PluginView {
            view_id,
            view,
            label,
        }] = events.as_slice()
        else {
            panic!("expected exactly one PluginView");
        };
        assert_eq!(view_id, "modq:1");
        assert_eq!(label.as_deref(), Some("i1"));
        assert!(view.contains("\"title\":\"Ban\""), "{view}");
        assert!(view.contains("modal"), "{view}");
    }

    #[test]
    fn an_undecodable_plugin_payload_is_dropped_at_the_boundary() {
        // A payload the codec cannot read must not reach a renderer that would
        // have to guess what to do with it. Dropped here, where the types are.
        assert!(feed("PLUGIN-VIEW modq:1 :not-base64-cbor").is_empty());
        assert!(feed("PLUGIN-PATCH modq:1 :%%%%").is_empty());
    }

    #[test]
    fn plugin_verbs_build_the_lines_the_server_routes() {
        assert_eq!(build_plugins().unwrap(), "PLUGINS");
        assert_eq!(
            build_plugin_invoke("modq", "open", None, None).unwrap(),
            "PLUGIN INVOKE modq open"
        );
        assert_eq!(build_plugin_close("modq:1").unwrap(), "PLUGIN CLOSE modq:1");
        // Subscribe and unsubscribe are one call with a flag — a panel toggling
        // liveness should not be two spellings of the same thought.
        assert!(build_plugin_subscribe("modq:1", true)
            .unwrap()
            .contains("SUBSCRIBE"));
        assert!(build_plugin_subscribe("modq:1", false)
            .unwrap()
            .contains("UNSUBSCRIBE"));
    }

    /// `NS JOIN` takes both kinds of target, and the client decides which by shape
    /// — the same way the server's parser does. Without this the client could only
    /// ever emit the local form, so a bridged Matrix space was unreachable from the
    /// UI however well the bridge worked.
    #[test]
    fn ns_join_routes_on_the_targets_shape() {
        // Local: id or vanity.
        assert_eq!(build_ns_join("gaming").unwrap(), "NS JOIN gaming");

        // Foreign: a `<scheme>://<realm>/<space>` URI reaches the provider path.
        // The server parses this line back to `Command::NsJoinForeign`.
        assert_eq!(
            build_ns_join("matrix://teamnight.org/myspace").unwrap(),
            "NS JOIN matrix://teamnight.org/myspace"
        );

        // A malformed URI is refused here, with the expected form in the message,
        // rather than travelling as a nonsense namespace name.
        let err = build_ns_join("matrix://").expect_err("no realm");
        assert!(err.contains("<scheme>://<realm>/<space>"), "{err}");

        // A bare name that isn't a legal namespace ref is still refused.
        assert!(build_ns_join("Not A Namespace").is_err());
    }

    #[test]
    fn form_values_cross_the_json_to_cbor_boundary_here() {
        // The UI speaks JSON, the wire speaks CBOR. Converting here keeps CBOR
        // out of the frontend and rejects a bad payload before it is sent.
        let encoded = plugin_values(Some(r#"{"reason":"spam","days":7}"#.into()))
            .expect("valid values")
            .expect("some");
        let round_tripped: std::collections::BTreeMap<String, serde_json::Value> =
            weft_proto::plugin_from_b64(&encoded).expect("decodes");
        assert_eq!(round_tripped["reason"], "spam");
        assert_eq!(round_tripped["days"], 7);

        assert!(plugin_values(None).unwrap().is_none());
        assert!(plugin_values(Some("not json".into())).is_err());
        // A bare array is not `component-id → value`.
        assert!(plugin_values(Some("[1,2]".into())).is_err());
    }

    #[test]
    fn sync_end_surfaces_the_cursor() {
        let events = feed("@cursor=e7:8412 SYNC END");
        assert!(matches!(
            events.as_slice(),
            [ClientEvent::SyncEnd { cursor }] if cursor == "e7:8412"
        ));
    }

    #[test]
    fn ns_member_maps_join() {
        // v0.13: NS-MEMBER addresses the namespace by its ULID id.
        let ns_id = "01arz3ndektsv4rrffq69g5fav";
        let events = feed(&format!(
            "@count=42 NS-MEMBER {ns_id} ada@test.example join"
        ));
        assert!(matches!(
            events.as_slice(),
            [ClientEvent::NsMember { namespace, action, count: Some(42), .. }]
                if namespace == ns_id && action == "join"
        ));
    }

    #[test]
    fn chansync_maps_reset_flag() {
        let events = feed("@reset CHANSYNC #gaming/general");
        assert!(matches!(
            events.as_slice(),
            [ClientEvent::ChanSync { channel, reset: true, .. }] if channel == "#gaming/general"
        ));
    }

    #[test]
    fn sync_start_and_body_are_silent() {
        assert!(feed("SYNC START").is_empty());
        assert!(feed("SYNC BODY s_9f3c").is_empty());
    }
}
