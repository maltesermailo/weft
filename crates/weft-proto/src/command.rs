//! Client → server commands: the M0 session + relay verb set (§6.1, §6.3,
//! §6.4). Unknown verbs decode to [`Command::Unknown`] — never an error (§4).

use crate::error::{ParseError, SerializeError};
use crate::id::MsgId;
use crate::line::{label_from_tags, write_label, Args, Line, Tags};
use crate::name::{Account, ChannelName, NamespaceName, NetworkName, Target};
use crate::types::{
    report_category_ok, HistoryMode, MediaMode, MsgMeta, PresenceStatus, ReportScope, ReportStatus,
    ResolveAction, StreamMode, TypingState, Visibility,
};

/// A command plus its optional `label` (§3.5). The label is echoed on every
/// direct response — including `ERR` — and never on broadcast copies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub label: Option<String>,
    pub command: Command,
}

impl Request {
    pub fn new(command: Command) -> Self {
        Self {
            label: None,
            command,
        }
    }

    pub fn with_label(command: Command, label: impl Into<String>) -> Self {
        Self {
            label: Some(label.into()),
            command,
        }
    }

    pub fn parse(input: &str) -> Result<Self, ParseError> {
        Self::from_line(&Line::parse(input)?)
    }

    pub fn from_line(line: &Line) -> Result<Self, ParseError> {
        Ok(Request {
            label: label_from_tags(&line.tags)?,
            command: Command::from_line(line)?,
        })
    }

    pub fn to_line(&self) -> Result<Line, SerializeError> {
        let mut line = self.command.to_line()?;
        write_label(&mut line.tags, self.label.as_deref())?;
        Ok(line)
    }

    pub fn serialize(&self) -> Result<String, SerializeError> {
        self.to_line()?.serialize()
    }
}

/// The four ops of a federation-session frame (§11.10). `Cmd`/`Event` carry a
/// full inner control line verbatim (in the frame's trailing), so the tunnel is
/// transport-agnostic — H re-parses the inner line, F re-emits the inner reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FSessionOp {
    /// F opens a session for its local user `account` (H forms `account@<peer>`).
    Open { account: String },
    /// A command line from that user (F→H).
    Cmd { line: String },
    /// The **direct reply** to one of her commands (H→F) — a labeled ack or
    /// `ERR`. Broadcast events never tunnel here; they ride the namespace mirror
    /// (§10.3), so the session carries only the request/response pair.
    Reply { line: String },
    /// Close the sub-session.
    Close,
}

/// M0 verb set. Extra params or an unexpected trailing are ignored
/// (lenient-in); missing or malformed required parts are typed errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `HELLO <version>` (§3.6).
    Hello { version: String },
    /// `REGISTER <account> :<password>` (§6.1).
    Register { account: Account, password: String },
    /// `AUTH PASSWORD <account> :<password>`.
    AuthPassword { account: Account, password: String },
    /// `AUTH KEY <account> <b64-ed25519-pubkey>` — starts challenge-response.
    AuthKey { account: Account, pubkey: String },
    /// `AUTH PROOF <b64-sig(nonce ‖ network-name)>` (§6.1: anti cross-network replay).
    AuthProof { signature: String },
    /// `AUTH ENROLL <b64-pubkey>` — add a device while authed.
    AuthEnroll { pubkey: String },
    /// `QUIT [:reason]`.
    Quit { reason: Option<String> },
    /// `PING [token]` (§3.4).
    Ping { token: Option<String> },
    /// `PONG [token]` — answering is mandatory even when QUIC keeps alive.
    Pong { token: Option<String> },
    /// `PRESENCE <status>` (§6.1).
    Presence { status: PresenceStatus },
    /// `JOIN <#chan> [invite-ref]` — JOIN never auto-creates (§6.3).
    Join {
        channel: ChannelName,
        invite: Option<String>,
    },
    /// `PART <#chan> [:reason]`.
    Part {
        channel: ChannelName,
        reason: Option<String>,
    },
    /// `TYPING <#chan> <start|stop>`.
    Typing {
        channel: ChannelName,
        state: TypingState,
    },
    /// `MARK <#chan> <msgid>` — read marker (§6.3).
    Mark { channel: ChannelName, msgid: MsgId },
    /// `UNREAD [<#chan>]` — request server-computed unread counts (§6.3).
    /// No channel = every joined channel.
    Unread { channel: Option<ChannelName> },
    /// `MEMBERS <#chan> [cursor]` — roster snapshot (§6.3), membership-gated.
    Members {
        channel: ChannelName,
        cursor: Option<String>,
    },
    /// `PIN <msgid>` — pin a message in its channel (§6.4). Cap: `pin`.
    Pin { msgid: MsgId },
    /// `UNPIN <msgid>` — unpin a message (§6.4). Cap: `pin`.
    Unpin { msgid: MsgId },
    /// `PINS <#chan>` — list pinned messages (§6.4), membership-gated.
    Pins { channel: ChannelName },
    /// `SEARCH <#chan> :<query>` — full-text message search in a channel
    /// (§6.4), membership-gated. Matching messages return as a `BATCH`.
    Search { channel: ChannelName, query: String },
    /// `CAPS <account> <scope>` — query an account's effective caps at a scope
    /// (§10.4). Public: any member may ask (caps aren't secret). → `CAPS` event.
    Caps { account: Account, scope: String },
    /// `MSG <#chan|@user> [:body]` — empty body legal iff attachments (§6.4;
    /// enforced by the session layer, not the codec).
    Msg {
        target: Target,
        body: Option<String>,
        meta: MsgMeta,
    },
    /// `STREAM OFFER <media|backfill> <mime> <bytes>` (§13, §6) — request a
    /// data-plane transfer. The server checks the `attach` cap + size config and
    /// replies `STREAM ACCEPT <token>`; the bytes then ride the data plane. For
    /// `backfill`, `mime` is a pseudo-type and `bytes` an estimate (M-media-4).
    StreamOffer {
        mode: StreamMode,
        mime: String,
        bytes: u64,
    },
    /// `EDIT <msgid> :<new>` — edit-own only, honored at origin (§6.4).
    Edit { msgid: MsgId, body: String },
    /// `DELETE <msgid>` — tombstone (§6.4).
    Delete { msgid: MsgId },
    /// `REACT <msgid> <emoji>` — idempotent (§6.4).
    React { msgid: MsgId, emoji: String },
    /// `UNREACT <msgid> <emoji>`.
    Unreact { msgid: MsgId, emoji: String },
    /// `HISTORY <target> [before=] [after=] [limit=] [thread=]` —
    /// key=value middle params, any order (§6.4).
    History {
        target: Target,
        before: Option<MsgId>,
        after: Option<MsgId>,
        limit: Option<u32>,
        thread: Option<MsgId>,
    },
    /// `GRANT <subject> <scope> <caps> [expiry=<s>]` (§6.5). `subject` is an
    /// account or b64 pubkey, `scope` is `#chan|ns:<name>|*`, `caps` a comma
    /// list — all validated by the capability layer, not the codec.
    Grant {
        subject: String,
        scope: String,
        caps: String,
        expiry: Option<u64>,
    },
    /// `REVOKE <subject> <scope> [caps=<list>] [epoch]` (§6.5). No caps and
    /// no epoch = revoke everything for the subject at the scope.
    Revoke {
        subject: String,
        scope: String,
        caps: Option<String>,
        /// Bumps the scope revocation epoch (§10.4).
        epoch: Option<u64>,
    },
    /// `ROLE CREATE <scope> <color> <caps> :<name>` (§6.5) — define/replace a
    /// named, colored capability-token bundle at a scope. `caps` a comma list;
    /// the display `name` (may contain spaces) rides the trailing.
    RoleCreate {
        scope: String,
        color: String,
        caps: String,
        name: String,
    },
    /// `ROLE DELETE <scope> :<name>` — remove a role definition (§6.5).
    RoleDelete { scope: String, name: String },
    /// `ROLE ASSIGN <scope> <account> :<name>` — grant the role's token bundle
    /// to an account and record explicit membership (§6.5).
    RoleAssign {
        scope: String,
        /// Local account name **or** foreign `account@network` (§10.4) — a role
        /// can be worn by a federated user, so this is a subject string.
        account: String,
        name: String,
    },
    /// `ROLE UNASSIGN <scope> <account> :<name>` — drop membership + revoke the
    /// role's caps (§6.5).
    RoleUnassign {
        scope: String,
        account: String,
        name: String,
    },
    /// `ROLES <scope>` — list the role definitions at a scope (§6.5) → a BATCH
    /// of `ROLE` events.
    RolesList { scope: String },
    /// `ROLES-OF <scope> <account>` — the roles an account is assigned at a
    /// scope (§6.5) → a `ROLE-MEMBER` event.
    RolesOf { scope: String, account: String },
    /// `CHANNEL CREATE <#chan> [policy] [text|voice]` — default `retained:90d`,
    /// `text` (§6.3). A `voice` channel is a WEFT-RT voice room (§16, voice-only).
    ChannelCreate {
        channel: ChannelName,
        policy: Option<crate::RetentionPolicy>,
        kind: crate::ChannelKind,
    },
    /// `CHANNEL POLICY <#chan> <policy> [purge]` (§6.3).
    ChannelPolicy {
        channel: ChannelName,
        policy: crate::RetentionPolicy,
        purge: bool,
    },
    /// `CHANNEL META <#chan> <topic|view-gated> :<value>` (§6.3) → `CHANMETA`.
    ChannelMeta {
        channel: ChannelName,
        key: String,
        value: String,
    },
    /// `CHANNEL DELETE <#chan> <#chan>` — confirmed by repetition (§6.3).
    ChannelDelete {
        channel: ChannelName,
        confirm: ChannelName,
    },
    /// `CHANNEL RENAME <#old> <#new>` — change a channel's identity within its
    /// namespace (§6.3). The server re-keys everything scoped to the name
    /// (grants, membership, roles, holds, pins, history) and emits
    /// `CHANNEL-RENAMED` to members.
    ChannelRename {
        channel: ChannelName,
        new_name: ChannelName,
    },
    /// `INVITE MINT <scope> [max-uses=] [expiry=]` (§6.5) → `INVITED`.
    InviteMint {
        scope: String,
        max_uses: Option<u32>,
        expiry: Option<u64>,
    },
    /// `INVITE REVOKE <invite-id>` — closes the counter (§6.5).
    InviteRevoke { invite_id: String },
    /// `INVITE REDEEM <b64>` — verifies chain + counter, mints a member
    /// token bound to the redeemer (§6.5).
    InviteRedeem { token: String },
    /// `EMOJI ADD <ns> <name> <media>` — add/replace a namespace custom emoji
    /// (§9.4); cap `ns-admin`. `media` is a `weft-media://…` reference.
    EmojiAdd {
        namespace: NamespaceName,
        name: String,
        media: String,
    },
    /// `EMOJI REMOVE <ns> <name>` — remove a namespace emoji (§9.4).
    EmojiRemove {
        namespace: NamespaceName,
        name: String,
    },
    /// `EMOJI LIST <ns>` — a namespace's emoji → an `EMOJI` batch (§9.4).
    EmojiList { namespace: NamespaceName },
    /// `NS CREATE <name> [tier]` with `@root=<b64-pubkey>` (§6.2). The
    /// client generates the namespace root key and submits its pubkey.
    NsCreate {
        name: NamespaceName,
        visibility: Visibility,
        root_key: String,
    },
    /// `NS META <name> <title|description|icon> :<value>` (§6.2).
    NsMeta {
        name: NamespaceName,
        key: String,
        value: String,
    },
    /// `NS VISIBILITY <name> <tier>` (§6.2).
    NsVisibility {
        name: NamespaceName,
        visibility: Visibility,
    },
    /// `NS DELEGATE <name> <account|pubkey> <cap>[,...]` — sugar for GRANT
    /// at `ns:` scope (§6.2).
    NsDelegate {
        name: NamespaceName,
        subject: String,
        caps: String,
    },
    /// `NS DELETE <name> <name>` — confirmed by repetition (§6.2).
    NsDelete {
        name: NamespaceName,
        confirm: NamespaceName,
    },
    /// `NS JOIN <name>` — join every channel in the namespace the caller can
    /// see (§6.2); view-gated and banned channels are skipped.
    NsJoin { name: NamespaceName },
    /// `NS TRANSFER <name> <account>` with `@sig=<b64>` — rung-1 succession,
    /// signed by the current root (§2.4).
    NsTransfer {
        name: NamespaceName,
        new_owner: Account,
        signature: String,
    },
    /// `NS RECOVERY SET <name> <m> <key1,key2,...>` — designate the M-of-N
    /// recovery quorum (§2.4). Root only.
    NsRecoverySet {
        name: NamespaceName,
        m: u32,
        keys: String,
    },
    /// `NS RECOVER <name> <b64-rotation-record>` — submit a quorum-signed
    /// (rung 2) or operator-signed (rung 3) rotation; starts the delay
    /// window (§2.4).
    NsRecover {
        name: NamespaceName,
        rotation: String,
    },
    /// `NS RECOVERY CANCEL <name>` with `@sig=<b64>` — the current root
    /// vetoes a pending recovery (§2.4).
    NsRecoveryCancel {
        name: NamespaceName,
        signature: String,
    },
    /// `DISCOVER [cursor]` — public namespace directory (§6.2).
    Discover { cursor: Option<String> },
    /// `CHANNELS <ns>` — the ordered channel layout of a namespace (spec
    /// extension: Discord-style categories + order).
    Channels { namespace: NamespaceName },
    /// `REPORT <msgid> <category> [scope] [:note]` (§6.7). Routed to the
    /// reporter's home network; `scope` defaults to `ns`.
    Report {
        msgid: MsgId,
        category: String,
        scope: ReportScope,
        note: Option<String>,
    },
    /// `REPORTS LIST <scope> [status=open|resolved] [cursor]` (§6.7) — the
    /// handler queue. `scope` is the concrete cap scope (`ns:<name>` or `*`),
    /// not the ns/net routing hint. Cap: `reports` at that scope.
    ReportsList {
        scope: String,
        status: Option<ReportStatus>,
        cursor: Option<String>,
    },
    /// `REPORTS RESOLVE <report-id> <action> [:note]` (§6.7).
    ReportsResolve {
        report_id: String,
        action: ResolveAction,
        note: Option<String>,
    },
    /// `AUTH BRIDGE <peer-network> <b64-token>` — a peer network opens a
    /// bridge session by presenting a `bridge` capability token (§11.2); the
    /// server then challenges to prove control of the token's subject key,
    /// reusing the §6.1 `CHALLENGE`/`AUTH PROOF` flow.
    AuthBridge { network: NetworkName, token: String },
    /// `BRIDGE PROPOSE <scope> <peer> [history=] [media=] [typing=] [voice=]` with
    /// the signed manifest in a `manifest=<b64>` tag (§6.6, §11.1). `scope` is
    /// `#chan|ns:<name>|*`, validated by the capability layer. `voice` (§16) opts
    /// the scope's voice channels into federation.
    BridgePropose {
        scope: String,
        peer: NetworkName,
        history: HistoryMode,
        media: MediaMode,
        typing: bool,
        voice: bool,
        manifest: Option<String>,
    },
    /// `BRIDGE ACCEPT <peer> <version>` — live on mutual ack (§6.6).
    BridgeAccept { peer: NetworkName, version: u64 },
    /// `BRIDGE ADD <peer> <#chan>` — amend, v+1, requires re-ack (§6.6).
    BridgeAdd {
        peer: NetworkName,
        channel: ChannelName,
    },
    /// `BRIDGE REMOVE <peer> <#chan>` — v+1, unilateral, immediate (§6.6).
    BridgeRemove {
        peer: NetworkName,
        channel: ChannelName,
    },
    /// `BRIDGE SEVER <peer>` — unilateral teardown (§6.6).
    BridgeSever { peer: NetworkName },
    /// `BRIDGE REQUEST <ns>` (§11.10) — ask the peer to offer a manifest for one
    /// of *its* namespaces. Bridge-session-only; the peer answers with
    /// `BRIDGE PROPOSE` iff the namespace is auto-federation-reachable.
    BridgeRequest { ns: NamespaceName },
    /// `FEDERATE <network>/<namespace>` (§11.10) — a local user asks their home
    /// network to auto-establish a bridge to a foreign namespace on demand.
    Federate {
        network: NetworkName,
        namespace: NamespaceName,
    },
    /// `FSESSION <fsid> <OPEN <account>|CMD :<line>|EVENT :<line>|CLOSE>`
    /// (§11.10) — federation-session multiplexing over a bridge (homeserver
    /// authority): F tunnels one of its users' command sessions to H, so a
    /// federated user wields her caps on H without ever connecting to it (IP
    /// non-exposure). Bridge-session-only.
    FSession { fsid: String, op: FSessionOp },
    /// `NETBLOCK ADD <network> [:reason]` (§6.6, §11.6). Cap `netblock` at `*`.
    NetblockAdd {
        network: NetworkName,
        reason: Option<String>,
    },
    /// `NETBLOCK REMOVE <network>` — lifts the block.
    NetblockRemove { network: NetworkName },
    /// `NETBLOCK LIST` — the operator's blocklist (§11.6).
    NetblockList,
    /// `MEDIA BLOCK <hash> [:reason]` (§13) — block a BLAKE3 media hash
    /// network-wide: delete it and reject re-upload + mirror. Cap `media-block`
    /// at `*`. `hash` is the bare content hash (validated by the media layer).
    MediaBlock {
        hash: String,
        reason: Option<String>,
    },
    /// `MEDIA UNBLOCK <hash>` — lift a hash block (§13).
    MediaUnblock { hash: String },
    /// `MEDIA BLOCKS` — the media hash blocklist (§13). → `MEDIA-BLOCKED` per entry.
    MediaBlocks,
    /// `REPORT-FORWARD <report-id> <msgid> <category> [:note]` — bridge-session
    /// only (§11.9). Reporter identity is stripped by the forwarder; the origin
    /// treats it as a net-scope, `unverified` signal.
    ReportForward {
        report_id: String,
        msgid: MsgId,
        category: String,
        note: Option<String>,
    },
    /// `MUTE <scope> <account> [:reason]` — deny `send` to an account at a
    /// scope (`#chan|ns:<name>|*`, §6.7). Cap `mute` at the scope.
    Mute {
        scope: String,
        account: Account,
        reason: Option<String>,
    },
    /// `UNMUTE <scope> <account>` — lift a mute.
    Unmute { scope: String, account: Account },
    /// `BAN <scope> <account> [:reason]` — deny join + send at a scope. Cap `ban`.
    Ban {
        scope: String,
        account: Account,
        reason: Option<String>,
    },
    /// `UNBAN <scope> <account>` — lift a ban.
    Unban { scope: String, account: Account },
    /// `KICK <#chan> <account> [:reason]` — force-part (no persistent state).
    /// Channel-only. Cap `kick`.
    Kick {
        channel: ChannelName,
        account: Account,
        reason: Option<String>,
    },
    /// `MODLIST <scope>` — list the moderation deny-list (mutes + bans) at a
    /// scope (`#chan|ns:<name>|*`, §6.7). Cap `mute` or `ban` at the scope.
    /// Answered as a `BATCH` of `MODERATED` events (each a current mute/ban).
    ModList { scope: String },
    /// `VOICE JOIN <#chan>` (§16, WEFT-RT) — request to join a channel's voice
    /// room. The server checks `listen`/`speak` caps + membership + mutes, then
    /// answers `VOICE OFFER` with a media token; media rides WebRTC, not this line.
    VoiceJoin { channel: ChannelName },
    /// `VOICE LEAVE <#chan>` — leave the voice room; the SFU tears the peer down.
    VoiceLeave { channel: ChannelName },
    /// `VOICE DESC <#chan> :<sdp>` (§16) — an SDP offer/answer for the channel's
    /// peer connection. Same verb both directions; the raw SDP rides the trailing
    /// (CR/LF survive the wire as `\r`/`\n`, like any message body).
    VoiceDesc { channel: ChannelName, sdp: String },
    /// `VOICE CAND <#chan> :<ice-candidate>` (§16) — a trickle-ICE candidate.
    /// Optional: non-trickle clients gather candidates into the `VOICE DESC` SDP.
    VoiceCand {
        channel: ChannelName,
        candidate: String,
    },
    /// `VOICE REQUEST <scope> <#chan>` (§16 federated voice) — a **bridge-only**
    /// verb: the home network asks a peer to relay one of the peer's voice
    /// channels. The peer answers `VOICE GRANT` iff the channel is in the acked
    /// manifest with `voice=on` and the requester isn't netblocked, else
    /// `NO-SUCH-TARGET` (invariant 1). `scope` is the manifest scope the request
    /// rides (`#chan|ns:<name>|*`).
    VoiceRequest { scope: String, channel: ChannelName },
    /// `PROFILE SET` with `@display=`/`@avatar=` tags (§10.3) — set your own
    /// display name + avatar (the avatar's BLAKE3 hash). A **present** tag sets
    /// the field (empty value clears it); an **absent** tag leaves it unchanged
    /// (partial update). Tags escape spaces, so a display name may contain them.
    ProfileSet {
        display: Option<String>,
        avatar: Option<String>,
    },
    /// `PROFILES <account> [account...]` (§10.3) — query display profiles; the
    /// server answers a `PROFILE` event per known account.
    ProfilesQuery { accounts: Vec<String> },
    /// `VERIFY EMAIL <address>` (§10.5) — claim an email address; the server
    /// mails a one-time code and records a `pending` claim (`VERIFIED … pending`).
    VerifyEmail { address: String },
    /// `VERIFY BIRTHDAY <YYYY-MM-DD>` (§10.5) — self-attest a birth date; recorded
    /// and `confirmed` immediately (self-declared, not server-proven).
    VerifyBirthday { date: String },
    /// `VERIFY CONFIRM <kind> <code>` (§10.5) — prove a `pending` claim with the
    /// mailed code; on match the claim becomes `confirmed`.
    VerifyConfirm { kind: String, code: String },
    /// `VERIFY LIST` (§10.5) — the caller's own claims, one `VERIFIED` per claim.
    VerifyList,
    /// Any verb outside the known set. Servers ignore it silently (§4).
    Unknown { verb: String },
}

/// Parse a `yes|no` flag value (manifest `typing=`).
fn yes_no(verb: &'static str, what: &'static str, value: &str) -> Result<bool, ParseError> {
    match value.to_ascii_lowercase().as_str() {
        "yes" => Ok(true),
        "no" => Ok(false),
        _ => Err(ParseError::BadParam {
            verb,
            what,
            value: value.to_string(),
        }),
    }
}

/// Comma-separated cap list as a middle param (no spaces).
fn caps_ok(caps: &str) -> bool {
    !caps.is_empty() && !caps.contains(' ')
}

/// Read the mandatory `@sig=` tag for signed NS verbs (§2.4).
fn ns_sig_tag(line: &Line) -> Result<String, ParseError> {
    line.tags
        .get("sig")
        .filter(|v| !v.is_empty())
        .cloned()
        .ok_or(ParseError::MissingParam {
            verb: "NS",
            what: "sig tag (root signature)",
        })
}

/// Scan middle params for an optional `key=<u64>`.
fn kv_u64(line: &Line, verb: &'static str, key: &'static str) -> Result<Option<u64>, ParseError> {
    for param in &line.params {
        if let Some(value) = param.strip_prefix(key).and_then(|r| r.strip_prefix('=')) {
            return Ok(Some(value.parse().map_err(|_| ParseError::BadParam {
                verb,
                what: key,
                value: value.to_string(),
            })?));
        }
    }
    Ok(None)
}

/// §6.4 emoji, ≤32 bytes. The `:shortcode:` form conflicts with the §4
/// grammar (a leading `:` starts the trailing) — flagged in spec §18 #8;
/// until that's decided, shortcodes travel bare and a leading colon is
/// rejected. Middle-param grammar already excludes spaces.
fn emoji_ok(emoji: &str) -> bool {
    !emoji.is_empty() && emoji.len() <= crate::line::MAX_EMOJI_BYTES && !emoji.starts_with(':')
}

impl Command {
    pub fn from_line(line: &Line) -> Result<Self, ParseError> {
        let verb = line.verb.as_str();
        match verb {
            "HELLO" => {
                let mut args = Args::new(line, "HELLO");
                Ok(Command::Hello {
                    version: args.req("version")?.to_string(),
                })
            }
            "REGISTER" => {
                let mut args = Args::new(line, "REGISTER");
                Ok(Command::Register {
                    account: args.req("account")?.parse()?,
                    password: args.trailing_req("password")?.to_string(),
                })
            }
            "AUTH" => {
                let mut args = Args::new(line, "AUTH");
                let sub = args.req("subcommand")?.to_ascii_uppercase();
                match sub.as_str() {
                    "PASSWORD" => Ok(Command::AuthPassword {
                        account: args.req("account")?.parse()?,
                        password: args.trailing_req("password")?.to_string(),
                    }),
                    "KEY" => Ok(Command::AuthKey {
                        account: args.req("account")?.parse()?,
                        pubkey: args.req("pubkey")?.to_string(),
                    }),
                    "PROOF" => Ok(Command::AuthProof {
                        signature: args.req("signature")?.to_string(),
                    }),
                    "ENROLL" => Ok(Command::AuthEnroll {
                        pubkey: args.req("pubkey")?.to_string(),
                    }),
                    "BRIDGE" => Ok(Command::AuthBridge {
                        network: args.req("network")?.parse()?,
                        token: args.req("token")?.to_string(),
                    }),
                    _ => Err(ParseError::BadParam {
                        verb: "AUTH",
                        what: "subcommand",
                        value: sub,
                    }),
                }
            }
            "QUIT" => Ok(Command::Quit {
                reason: line.trailing.clone(),
            }),
            "PING" => Ok(Command::Ping {
                token: Args::new(line, "PING").opt().map(str::to_string),
            }),
            "PONG" => Ok(Command::Pong {
                token: Args::new(line, "PONG").opt().map(str::to_string),
            }),
            "PRESENCE" => {
                let mut args = Args::new(line, "PRESENCE");
                Ok(Command::Presence {
                    status: args.req("status")?.parse()?,
                })
            }
            "JOIN" => {
                let mut args = Args::new(line, "JOIN");
                Ok(Command::Join {
                    channel: args.req("channel")?.parse()?,
                    invite: args.opt().map(str::to_string),
                })
            }
            "PART" => {
                let mut args = Args::new(line, "PART");
                Ok(Command::Part {
                    channel: args.req("channel")?.parse()?,
                    reason: args.trailing_opt(),
                })
            }
            "TYPING" => {
                let mut args = Args::new(line, "TYPING");
                Ok(Command::Typing {
                    channel: args.req("channel")?.parse()?,
                    state: args.req("state")?.parse()?,
                })
            }
            "MARK" => {
                let mut args = Args::new(line, "MARK");
                Ok(Command::Mark {
                    channel: args.req("channel")?.parse()?,
                    msgid: args.req("msgid")?.parse()?,
                })
            }
            "UNREAD" => {
                let mut args = Args::new(line, "UNREAD");
                Ok(Command::Unread {
                    channel: args.opt().map(|s| s.parse()).transpose()?,
                })
            }
            "MEMBERS" => {
                let mut args = Args::new(line, "MEMBERS");
                Ok(Command::Members {
                    channel: args.req("channel")?.parse()?,
                    cursor: args.opt().map(str::to_string),
                })
            }
            "PIN" => Ok(Command::Pin {
                msgid: Args::new(line, "PIN").req("msgid")?.parse()?,
            }),
            "UNPIN" => Ok(Command::Unpin {
                msgid: Args::new(line, "UNPIN").req("msgid")?.parse()?,
            }),
            "PINS" => Ok(Command::Pins {
                channel: Args::new(line, "PINS").req("channel")?.parse()?,
            }),
            "SEARCH" => {
                let mut args = Args::new(line, "SEARCH");
                Ok(Command::Search {
                    channel: args.req("channel")?.parse()?,
                    query: args.trailing_req("query")?.to_string(),
                })
            }
            "CAPS" => {
                let mut args = Args::new(line, "CAPS");
                Ok(Command::Caps {
                    account: args.req("account")?.parse()?,
                    scope: args.req("scope")?.to_string(),
                })
            }
            "MSG" => {
                let mut args = Args::new(line, "MSG");
                Ok(Command::Msg {
                    target: args.req("target")?.parse()?,
                    body: args.trailing_opt(),
                    meta: MsgMeta::from_tags(&line.tags)?,
                })
            }
            "EDIT" => {
                let mut args = Args::new(line, "EDIT");
                Ok(Command::Edit {
                    msgid: args.req("msgid")?.parse()?,
                    body: args.trailing_req("new body")?.to_string(),
                })
            }
            "DELETE" => {
                let mut args = Args::new(line, "DELETE");
                Ok(Command::Delete {
                    msgid: args.req("msgid")?.parse()?,
                })
            }
            "REACT" | "UNREACT" => {
                let react = verb == "REACT";
                let mut args = Args::new(line, if react { "REACT" } else { "UNREACT" });
                let msgid = args.req("msgid")?.parse()?;
                let emoji = args.req("emoji")?.to_string();
                if !emoji_ok(&emoji) {
                    return Err(ParseError::BadParam {
                        verb: if react { "REACT" } else { "UNREACT" },
                        what: "emoji",
                        value: emoji,
                    });
                }
                Ok(if react {
                    Command::React { msgid, emoji }
                } else {
                    Command::Unreact { msgid, emoji }
                })
            }
            "HISTORY" => {
                let mut args = Args::new(line, "HISTORY");
                let target = args.req("target")?.parse()?;
                // key=value params in any order; unknown keys ignored
                // (lenient-in), duplicates last-wins.
                let mut before = None;
                let mut after = None;
                let mut limit = None;
                let mut thread = None;
                while let Some(param) = args.opt() {
                    let Some((key, value)) = param.split_once('=') else {
                        continue;
                    };
                    match key {
                        "before" => before = Some(value.parse()?),
                        "after" => after = Some(value.parse()?),
                        "thread" => thread = Some(value.parse()?),
                        "limit" => {
                            limit = Some(value.parse().map_err(|_| ParseError::BadParam {
                                verb: "HISTORY",
                                what: "limit",
                                value: value.to_string(),
                            })?)
                        }
                        _ => {}
                    }
                }
                Ok(Command::History {
                    target,
                    before,
                    after,
                    limit,
                    thread,
                })
            }
            "GRANT" => {
                let mut args = Args::new(line, "GRANT");
                let subject = args.req("subject")?.to_string();
                let scope = args.req("scope")?.to_string();
                let caps = args.req("caps")?.to_string();
                if !caps_ok(&caps) {
                    return Err(ParseError::BadParam {
                        verb: "GRANT",
                        what: "caps",
                        value: caps,
                    });
                }
                Ok(Command::Grant {
                    subject,
                    scope,
                    caps,
                    expiry: kv_u64(line, "GRANT", "expiry")?,
                })
            }
            "REVOKE" => {
                let mut args = Args::new(line, "REVOKE");
                let subject = args.req("subject")?.to_string();
                let scope = args.req("scope")?.to_string();
                // Remaining params: `caps=<list>` and/or a bare epoch number.
                let mut caps = None;
                let mut epoch = None;
                while let Some(param) = args.opt() {
                    if let Some(list) = param.strip_prefix("caps=") {
                        caps = Some(list.to_string());
                    } else if let Ok(n) = param.parse::<u64>() {
                        epoch = Some(n);
                    }
                }
                Ok(Command::Revoke {
                    subject,
                    scope,
                    caps,
                    epoch,
                })
            }
            "ROLE" => {
                let mut args = Args::new(line, "ROLE");
                let sub = args.req("subcommand")?.to_ascii_uppercase();
                match sub.as_str() {
                    "CREATE" => {
                        let scope = args.req("scope")?.to_string();
                        let color = args.req("color")?.to_string();
                        let caps = args.req("caps")?.to_string();
                        if !caps_ok(&caps) {
                            return Err(ParseError::BadParam {
                                verb: "ROLE",
                                what: "caps",
                                value: caps,
                            });
                        }
                        let name = line.trailing.clone().ok_or(ParseError::MissingParam {
                            verb: "ROLE",
                            what: "name",
                        })?;
                        Ok(Command::RoleCreate {
                            scope,
                            color,
                            caps,
                            name,
                        })
                    }
                    "DELETE" => Ok(Command::RoleDelete {
                        scope: args.req("scope")?.to_string(),
                        name: line.trailing.clone().ok_or(ParseError::MissingParam {
                            verb: "ROLE",
                            what: "name",
                        })?,
                    }),
                    "ASSIGN" => Ok(Command::RoleAssign {
                        scope: args.req("scope")?.to_string(),
                        account: args.req("account")?.to_string(),
                        name: line.trailing.clone().ok_or(ParseError::MissingParam {
                            verb: "ROLE",
                            what: "name",
                        })?,
                    }),
                    "UNASSIGN" => Ok(Command::RoleUnassign {
                        scope: args.req("scope")?.to_string(),
                        account: args.req("account")?.to_string(),
                        name: line.trailing.clone().ok_or(ParseError::MissingParam {
                            verb: "ROLE",
                            what: "name",
                        })?,
                    }),
                    other => Ok(Command::Unknown {
                        verb: format!("ROLE {other}"),
                    }),
                }
            }
            "ROLES" => {
                let mut args = Args::new(line, "ROLES");
                Ok(Command::RolesList {
                    scope: args.req("scope")?.to_string(),
                })
            }
            "ROLES-OF" => {
                let mut args = Args::new(line, "ROLES-OF");
                Ok(Command::RolesOf {
                    scope: args.req("scope")?.to_string(),
                    account: args.req("account")?.to_string(),
                })
            }
            "CHANNEL" => {
                let mut args = Args::new(line, "CHANNEL");
                let sub = args.req("subcommand")?.to_ascii_uppercase();
                match sub.as_str() {
                    "CREATE" => {
                        let channel = args.req("channel")?.parse()?;
                        // `[policy]` and `[text|voice]` are optional bare tokens in
                        // either order; a token that parses as a kind is the kind,
                        // otherwise it's the retention policy (lenient-in).
                        let mut policy = None;
                        let mut kind = crate::ChannelKind::Text;
                        while let Some(tok) = args.opt() {
                            if let Ok(k) = tok.parse::<crate::ChannelKind>() {
                                kind = k;
                            } else {
                                policy = Some(tok.parse()?);
                            }
                        }
                        Ok(Command::ChannelCreate {
                            channel,
                            policy,
                            kind,
                        })
                    }
                    "POLICY" => {
                        let channel = args.req("channel")?.parse()?;
                        let policy = args.req("policy")?.parse()?;
                        // `purge` is a bare flag keyword after the policy.
                        let purge = args.opt().is_some_and(|p| p.eq_ignore_ascii_case("purge"));
                        Ok(Command::ChannelPolicy {
                            channel,
                            policy,
                            purge,
                        })
                    }
                    "META" => Ok(Command::ChannelMeta {
                        channel: args.req("channel")?.parse()?,
                        key: args.req("key")?.to_string(),
                        value: args.trailing_req("value")?.to_string(),
                    }),
                    "DELETE" => Ok(Command::ChannelDelete {
                        channel: args.req("channel")?.parse()?,
                        confirm: args.req("confirmation")?.parse()?,
                    }),
                    "RENAME" => Ok(Command::ChannelRename {
                        channel: args.req("channel")?.parse()?,
                        new_name: args.req("new-name")?.parse()?,
                    }),
                    _ => Err(ParseError::BadParam {
                        verb: "CHANNEL",
                        what: "subcommand",
                        value: sub,
                    }),
                }
            }
            "INVITE" => {
                let mut args = Args::new(line, "INVITE");
                let sub = args.req("subcommand")?.to_ascii_uppercase();
                match sub.as_str() {
                    "MINT" => {
                        let scope = args.req("scope")?.to_string();
                        let mut max_uses = None;
                        let mut expiry = None;
                        while let Some(param) = args.opt() {
                            if let Some(v) = param.strip_prefix("max-uses=") {
                                max_uses = Some(v.parse().map_err(|_| ParseError::BadParam {
                                    verb: "INVITE",
                                    what: "max-uses",
                                    value: v.to_string(),
                                })?);
                            } else if let Some(v) = param.strip_prefix("expiry=") {
                                expiry = Some(v.parse().map_err(|_| ParseError::BadParam {
                                    verb: "INVITE",
                                    what: "expiry",
                                    value: v.to_string(),
                                })?);
                            }
                        }
                        Ok(Command::InviteMint {
                            scope,
                            max_uses,
                            expiry,
                        })
                    }
                    "REVOKE" => Ok(Command::InviteRevoke {
                        invite_id: args.req("invite-id")?.to_string(),
                    }),
                    "REDEEM" => Ok(Command::InviteRedeem {
                        token: args.req("token")?.to_string(),
                    }),
                    _ => Err(ParseError::BadParam {
                        verb: "INVITE",
                        what: "subcommand",
                        value: sub,
                    }),
                }
            }
            "EMOJI" => {
                let mut args = Args::new(line, "EMOJI");
                let sub = args.req("subcommand")?.to_ascii_uppercase();
                match sub.as_str() {
                    "ADD" => Ok(Command::EmojiAdd {
                        namespace: args.req("namespace")?.parse()?,
                        name: args.req("name")?.to_string(),
                        media: args.req("media")?.to_string(),
                    }),
                    "REMOVE" => Ok(Command::EmojiRemove {
                        namespace: args.req("namespace")?.parse()?,
                        name: args.req("name")?.to_string(),
                    }),
                    "LIST" => Ok(Command::EmojiList {
                        namespace: args.req("namespace")?.parse()?,
                    }),
                    _ => Err(ParseError::BadParam {
                        verb: "EMOJI",
                        what: "subcommand",
                        value: sub,
                    }),
                }
            }
            "NS" => {
                let mut args = Args::new(line, "NS");
                let sub = args.req("subcommand")?.to_ascii_uppercase();
                match sub.as_str() {
                    "CREATE" => {
                        let name = args.req("name")?.parse()?;
                        // Default tier is `unlisted` (§6.2).
                        let visibility = args
                            .opt()
                            .map(str::parse)
                            .transpose()?
                            .unwrap_or(Visibility::Unlisted);
                        let root_key = line
                            .tags
                            .get("root")
                            .filter(|v| !v.is_empty())
                            .cloned()
                            .ok_or(ParseError::MissingParam {
                                verb: "NS",
                                what: "root tag (namespace root pubkey)",
                            })?;
                        Ok(Command::NsCreate {
                            name,
                            visibility,
                            root_key,
                        })
                    }
                    "META" => Ok(Command::NsMeta {
                        name: args.req("name")?.parse()?,
                        key: args.req("key")?.to_string(),
                        value: args.trailing_req("value")?.to_string(),
                    }),
                    "VISIBILITY" => Ok(Command::NsVisibility {
                        name: args.req("name")?.parse()?,
                        visibility: args.req("tier")?.parse()?,
                    }),
                    "DELEGATE" => {
                        let name = args.req("name")?.parse()?;
                        let subject = args.req("subject")?.to_string();
                        let caps = args.req("caps")?.to_string();
                        if !caps_ok(&caps) {
                            return Err(ParseError::BadParam {
                                verb: "NS",
                                what: "caps",
                                value: caps,
                            });
                        }
                        Ok(Command::NsDelegate {
                            name,
                            subject,
                            caps,
                        })
                    }
                    "DELETE" => Ok(Command::NsDelete {
                        name: args.req("name")?.parse()?,
                        confirm: args.req("confirmation")?.parse()?,
                    }),
                    "JOIN" => Ok(Command::NsJoin {
                        name: args.req("name")?.parse()?,
                    }),
                    "TRANSFER" => {
                        let name = args.req("name")?.parse()?;
                        let new_owner = args.req("account")?.parse()?;
                        let signature = ns_sig_tag(line)?;
                        Ok(Command::NsTransfer {
                            name,
                            new_owner,
                            signature,
                        })
                    }
                    "RECOVER" => Ok(Command::NsRecover {
                        name: args.req("name")?.parse()?,
                        rotation: args.req("rotation-record")?.to_string(),
                    }),
                    // Three-word: NS RECOVERY SET | NS RECOVERY CANCEL.
                    "RECOVERY" => {
                        let action = args.req("action")?.to_ascii_uppercase();
                        match action.as_str() {
                            "SET" => {
                                let name = args.req("name")?.parse()?;
                                let m = args.req("m")?;
                                let m = m.parse().map_err(|_| ParseError::BadParam {
                                    verb: "NS",
                                    what: "m",
                                    value: m.to_string(),
                                })?;
                                let keys = args.req("keys")?.to_string();
                                Ok(Command::NsRecoverySet { name, m, keys })
                            }
                            "CANCEL" => {
                                let name = args.req("name")?.parse()?;
                                Ok(Command::NsRecoveryCancel {
                                    name,
                                    signature: ns_sig_tag(line)?,
                                })
                            }
                            _ => Err(ParseError::BadParam {
                                verb: "NS",
                                what: "recovery action",
                                value: action,
                            }),
                        }
                    }
                    _ => Err(ParseError::BadParam {
                        verb: "NS",
                        what: "subcommand",
                        value: sub,
                    }),
                }
            }
            "DISCOVER" => Ok(Command::Discover {
                cursor: Args::new(line, "DISCOVER").opt().map(str::to_string),
            }),
            "CHANNELS" => Ok(Command::Channels {
                namespace: Args::new(line, "CHANNELS").req("namespace")?.parse()?,
            }),
            "FEDERATE" => {
                let target = Args::new(line, "FEDERATE").req("target")?.to_string();
                let (network, namespace) =
                    target.split_once('/').ok_or_else(|| ParseError::BadParam {
                        verb: "FEDERATE",
                        what: "target (expected <network>/<namespace>)",
                        value: target.clone(),
                    })?;
                Ok(Command::Federate {
                    network: network.parse()?,
                    namespace: namespace.parse()?,
                })
            }
            "FSESSION" => {
                let mut args = Args::new(line, "FSESSION");
                let fsid = args.req("fsid")?.to_string();
                let op = match args.req("op")?.to_ascii_uppercase().as_str() {
                    "OPEN" => FSessionOp::Open {
                        account: args.req("account")?.to_string(),
                    },
                    "CMD" => FSessionOp::Cmd {
                        line: line.trailing.clone().unwrap_or_default(),
                    },
                    "REPLY" => FSessionOp::Reply {
                        line: line.trailing.clone().unwrap_or_default(),
                    },
                    "CLOSE" => FSessionOp::Close,
                    other => {
                        return Ok(Command::Unknown {
                            verb: format!("FSESSION {other}"),
                        })
                    }
                };
                Ok(Command::FSession { fsid, op })
            }
            "REPORT" => {
                let mut args = Args::new(line, "REPORT");
                let msgid = args.req("msgid")?.parse()?;
                let category = args.req("category")?.to_string();
                if !report_category_ok(&category) {
                    return Err(ParseError::BadParam {
                        verb: "REPORT",
                        what: "category",
                        value: category,
                    });
                }
                // Optional scope defaults to `ns` (§6.7).
                let scope = args
                    .opt()
                    .map(str::parse)
                    .transpose()?
                    .unwrap_or(ReportScope::Ns);
                Ok(Command::Report {
                    msgid,
                    category,
                    scope,
                    note: args.trailing_opt(),
                })
            }
            "REPORTS" => {
                let mut args = Args::new(line, "REPORTS");
                let sub = args.req("subcommand")?.to_ascii_uppercase();
                match sub.as_str() {
                    "LIST" => {
                        let scope = args.req("scope")?.to_string();
                        // `status=` param and a bare cursor, any order.
                        let mut status = None;
                        let mut cursor = None;
                        while let Some(param) = args.opt() {
                            if let Some(v) = param.strip_prefix("status=") {
                                status = Some(v.parse()?);
                            } else {
                                cursor = Some(param.to_string());
                            }
                        }
                        Ok(Command::ReportsList {
                            scope,
                            status,
                            cursor,
                        })
                    }
                    "RESOLVE" => Ok(Command::ReportsResolve {
                        report_id: args.req("report-id")?.to_string(),
                        action: args.req("action")?.parse()?,
                        note: args.trailing_opt(),
                    }),
                    _ => Err(ParseError::BadParam {
                        verb: "REPORTS",
                        what: "subcommand",
                        value: sub,
                    }),
                }
            }
            "BRIDGE" => {
                let mut args = Args::new(line, "BRIDGE");
                let sub = args.req("subcommand")?.to_ascii_uppercase();
                match sub.as_str() {
                    "PROPOSE" => {
                        let scope = args.req("scope")?.to_string();
                        let peer = args.req("peer")?.parse()?;
                        // Strictest-safe defaults (§11.1): no history, no media,
                        // no typing, no voice unless the proposal opts in.
                        let mut history = HistoryMode::FromEpoch;
                        let mut media = MediaMode::None;
                        let mut typing = false;
                        let mut voice = false;
                        while let Some(param) = args.opt() {
                            if let Some(v) = param.strip_prefix("history=") {
                                history = v.parse()?;
                            } else if let Some(v) = param.strip_prefix("media=") {
                                media = v.parse()?;
                            } else if let Some(v) = param.strip_prefix("typing=") {
                                typing = yes_no("BRIDGE", "typing", v)?;
                            } else if let Some(v) = param.strip_prefix("voice=") {
                                voice = yes_no("BRIDGE", "voice", v)?;
                            }
                        }
                        Ok(Command::BridgePropose {
                            scope,
                            peer,
                            history,
                            media,
                            typing,
                            voice,
                            manifest: line.tags.get("manifest").filter(|v| !v.is_empty()).cloned(),
                        })
                    }
                    "ACCEPT" => {
                        let peer = args.req("peer")?.parse()?;
                        let version = args.req("version")?;
                        let version = version.parse().map_err(|_| ParseError::BadParam {
                            verb: "BRIDGE",
                            what: "version",
                            value: version.to_string(),
                        })?;
                        Ok(Command::BridgeAccept { peer, version })
                    }
                    "ADD" => Ok(Command::BridgeAdd {
                        peer: args.req("peer")?.parse()?,
                        channel: args.req("channel")?.parse()?,
                    }),
                    "REMOVE" => Ok(Command::BridgeRemove {
                        peer: args.req("peer")?.parse()?,
                        channel: args.req("channel")?.parse()?,
                    }),
                    "SEVER" => Ok(Command::BridgeSever {
                        peer: args.req("peer")?.parse()?,
                    }),
                    "REQUEST" => Ok(Command::BridgeRequest {
                        ns: args.req("ns")?.parse()?,
                    }),
                    _ => Err(ParseError::BadParam {
                        verb: "BRIDGE",
                        what: "subcommand",
                        value: sub,
                    }),
                }
            }
            "NETBLOCK" => {
                let mut args = Args::new(line, "NETBLOCK");
                let sub = args.req("subcommand")?.to_ascii_uppercase();
                match sub.as_str() {
                    "ADD" => Ok(Command::NetblockAdd {
                        network: args.req("network")?.parse()?,
                        reason: args.trailing_opt(),
                    }),
                    "REMOVE" => Ok(Command::NetblockRemove {
                        network: args.req("network")?.parse()?,
                    }),
                    "LIST" => Ok(Command::NetblockList),
                    _ => Err(ParseError::BadParam {
                        verb: "NETBLOCK",
                        what: "subcommand",
                        value: sub,
                    }),
                }
            }
            "MEDIA" => {
                let mut args = Args::new(line, "MEDIA");
                let sub = args.req("subcommand")?.to_ascii_uppercase();
                match sub.as_str() {
                    "BLOCK" => Ok(Command::MediaBlock {
                        hash: args.req("hash")?.to_string(),
                        reason: args.trailing_opt(),
                    }),
                    "UNBLOCK" => Ok(Command::MediaUnblock {
                        hash: args.req("hash")?.to_string(),
                    }),
                    "BLOCKS" => Ok(Command::MediaBlocks),
                    _ => Err(ParseError::BadParam {
                        verb: "MEDIA",
                        what: "subcommand",
                        value: sub,
                    }),
                }
            }
            "MUTE" | "UNMUTE" | "BAN" | "UNBAN" => {
                let verb = match verb {
                    "MUTE" => "MUTE",
                    "UNMUTE" => "UNMUTE",
                    "BAN" => "BAN",
                    _ => "UNBAN",
                };
                let mut args = Args::new(line, verb);
                let scope = args.req("scope")?.to_string();
                let account = args.req("account")?.parse()?;
                let reason = args.trailing_opt();
                Ok(match verb {
                    "MUTE" => Command::Mute {
                        scope,
                        account,
                        reason,
                    },
                    "UNMUTE" => Command::Unmute { scope, account },
                    "BAN" => Command::Ban {
                        scope,
                        account,
                        reason,
                    },
                    _ => Command::Unban { scope, account },
                })
            }
            "KICK" => {
                let mut args = Args::new(line, "KICK");
                Ok(Command::Kick {
                    channel: args.req("channel")?.parse()?,
                    account: args.req("account")?.parse()?,
                    reason: args.trailing_opt(),
                })
            }
            "MODLIST" => {
                let mut args = Args::new(line, "MODLIST");
                Ok(Command::ModList {
                    scope: args.req("scope")?.to_string(),
                })
            }
            "REPORT-FORWARD" => {
                let mut args = Args::new(line, "REPORT-FORWARD");
                let report_id = args.req("report-id")?.to_string();
                let msgid = args.req("msgid")?.parse()?;
                let category = args.req("category")?.to_string();
                if !report_category_ok(&category) {
                    return Err(ParseError::BadParam {
                        verb: "REPORT-FORWARD",
                        what: "category",
                        value: category,
                    });
                }
                Ok(Command::ReportForward {
                    report_id,
                    msgid,
                    category,
                    note: args.trailing_opt(),
                })
            }
            "STREAM" => {
                let mut args = Args::new(line, "STREAM");
                let sub = args.req("subcommand")?.to_ascii_uppercase();
                match sub.as_str() {
                    "OFFER" => Ok(Command::StreamOffer {
                        mode: args.req("mode")?.parse()?,
                        mime: args.req("mime")?.to_string(),
                        bytes: args
                            .req("bytes")?
                            .parse()
                            .map_err(|_| ParseError::BadParam {
                                verb: "STREAM",
                                what: "bytes",
                                value: line.params.get(3).cloned().unwrap_or_default(),
                            })?,
                    }),
                    _ => Err(ParseError::BadParam {
                        verb: "STREAM",
                        what: "subcommand",
                        value: sub,
                    }),
                }
            }
            "PROFILE" => {
                let mut args = Args::new(line, "PROFILE");
                let sub = args.req("subcommand")?.to_ascii_uppercase();
                match sub.as_str() {
                    // Present tag (even empty) = set/clear; absent = leave as-is.
                    "SET" => Ok(Command::ProfileSet {
                        display: line.tags.get("display").cloned(),
                        avatar: line.tags.get("avatar").cloned(),
                    }),
                    _ => Err(ParseError::BadParam {
                        verb: "PROFILE",
                        what: "subcommand",
                        value: sub,
                    }),
                }
            }
            "PROFILES" => {
                let mut args = Args::new(line, "PROFILES");
                let mut accounts = Vec::new();
                while let Some(a) = args.opt() {
                    accounts.push(a.to_string());
                }
                if accounts.is_empty() {
                    return Err(ParseError::MissingParam {
                        verb: "PROFILES",
                        what: "account",
                    });
                }
                Ok(Command::ProfilesQuery { accounts })
            }
            "VERIFY" => {
                let mut args = Args::new(line, "VERIFY");
                let sub = args.req("subcommand")?.to_ascii_uppercase();
                match sub.as_str() {
                    "EMAIL" => Ok(Command::VerifyEmail {
                        address: args.req("address")?.to_string(),
                    }),
                    "BIRTHDAY" => Ok(Command::VerifyBirthday {
                        date: args.req("date")?.to_string(),
                    }),
                    "CONFIRM" => Ok(Command::VerifyConfirm {
                        kind: args.req("kind")?.to_string(),
                        code: args.req("code")?.to_string(),
                    }),
                    "LIST" => Ok(Command::VerifyList),
                    _ => Err(ParseError::BadParam {
                        verb: "VERIFY",
                        what: "subcommand",
                        value: sub,
                    }),
                }
            }
            "VOICE" => {
                let mut args = Args::new(line, "VOICE");
                let sub = args.req("subcommand")?.to_ascii_uppercase();
                match sub.as_str() {
                    "JOIN" => Ok(Command::VoiceJoin {
                        channel: args.req("channel")?.parse()?,
                    }),
                    "LEAVE" => Ok(Command::VoiceLeave {
                        channel: args.req("channel")?.parse()?,
                    }),
                    "DESC" => Ok(Command::VoiceDesc {
                        channel: args.req("channel")?.parse()?,
                        sdp: args.trailing_req("sdp")?.to_string(),
                    }),
                    "CAND" => Ok(Command::VoiceCand {
                        channel: args.req("channel")?.parse()?,
                        candidate: args.trailing_req("candidate")?.to_string(),
                    }),
                    "REQUEST" => Ok(Command::VoiceRequest {
                        scope: args.req("scope")?.to_string(),
                        channel: args.req("channel")?.parse()?,
                    }),
                    _ => Err(ParseError::BadParam {
                        verb: "VOICE",
                        what: "subcommand",
                        value: sub,
                    }),
                }
            }
            _ => Ok(Command::Unknown {
                verb: verb.to_string(),
            }),
        }
    }

    pub fn to_line(&self) -> Result<Line, SerializeError> {
        let mut tags = Tags::new();
        let (verb, params, trailing): (&str, Vec<String>, Option<String>) = match self {
            Command::Hello { version } => ("HELLO", vec![version.clone()], None),
            Command::Register { account, password } => (
                "REGISTER",
                vec![account.to_string()],
                Some(password.clone()),
            ),
            Command::AuthPassword { account, password } => (
                "AUTH",
                vec!["PASSWORD".to_string(), account.to_string()],
                Some(password.clone()),
            ),
            Command::AuthKey { account, pubkey } => (
                "AUTH",
                vec!["KEY".to_string(), account.to_string(), pubkey.clone()],
                None,
            ),
            Command::AuthProof { signature } => {
                ("AUTH", vec!["PROOF".to_string(), signature.clone()], None)
            }
            Command::AuthEnroll { pubkey } => {
                ("AUTH", vec!["ENROLL".to_string(), pubkey.clone()], None)
            }
            Command::Quit { reason } => ("QUIT", vec![], reason.clone()),
            Command::Ping { token } => ("PING", token.iter().cloned().collect(), None),
            Command::Pong { token } => ("PONG", token.iter().cloned().collect(), None),
            Command::Presence { status } => ("PRESENCE", vec![status.to_string()], None),
            Command::Join { channel, invite } => {
                let mut params = vec![channel.to_string()];
                params.extend(invite.iter().cloned());
                ("JOIN", params, None)
            }
            Command::Part { channel, reason } => {
                ("PART", vec![channel.to_string()], reason.clone())
            }
            Command::Typing { channel, state } => {
                ("TYPING", vec![channel.to_string(), state.to_string()], None)
            }
            Command::Mark { channel, msgid } => {
                ("MARK", vec![channel.to_string(), msgid.to_string()], None)
            }
            Command::Unread { channel } => (
                "UNREAD",
                channel.iter().map(|c| c.to_string()).collect(),
                None,
            ),
            Command::Members { channel, cursor } => (
                "MEMBERS",
                std::iter::once(channel.to_string())
                    .chain(cursor.clone())
                    .collect(),
                None,
            ),
            Command::Pin { msgid } => ("PIN", vec![msgid.to_string()], None),
            Command::Unpin { msgid } => ("UNPIN", vec![msgid.to_string()], None),
            Command::Pins { channel } => ("PINS", vec![channel.to_string()], None),
            Command::Search { channel, query } => (
                "SEARCH",
                vec![channel.to_string()],
                Some(query.clone()),
            ),
            Command::Caps { account, scope } => {
                ("CAPS", vec![account.to_string(), scope.clone()], None)
            }
            Command::Msg { target, body, meta } => {
                meta.write_tags(&mut tags)?;
                ("MSG", vec![target.to_string()], body.clone())
            }
            Command::StreamOffer { mode, mime, bytes } => (
                "STREAM",
                vec![
                    "OFFER".to_string(),
                    mode.to_string(),
                    mime.clone(),
                    bytes.to_string(),
                ],
                None,
            ),
            Command::Edit { msgid, body } => ("EDIT", vec![msgid.to_string()], Some(body.clone())),
            Command::Delete { msgid } => ("DELETE", vec![msgid.to_string()], None),
            Command::React { msgid, emoji } | Command::Unreact { msgid, emoji } => {
                if !emoji_ok(emoji) {
                    return Err(SerializeError::BadParam {
                        param: emoji.clone(),
                        reason: "emoji must be 1..=32 bytes",
                    });
                }
                let verb = if matches!(self, Command::React { .. }) {
                    "REACT"
                } else {
                    "UNREACT"
                };
                (verb, vec![msgid.to_string(), emoji.clone()], None)
            }
            Command::History {
                target,
                before,
                after,
                limit,
                thread,
            } => {
                let mut params = vec![target.to_string()];
                if let Some(before) = before {
                    params.push(format!("before={before}"));
                }
                if let Some(after) = after {
                    params.push(format!("after={after}"));
                }
                if let Some(limit) = limit {
                    params.push(format!("limit={limit}"));
                }
                if let Some(thread) = thread {
                    params.push(format!("thread={thread}"));
                }
                ("HISTORY", params, None)
            }
            Command::Grant {
                subject,
                scope,
                caps,
                expiry,
            } => {
                if !caps_ok(caps) {
                    return Err(SerializeError::BadParam {
                        param: caps.clone(),
                        reason: "caps must be a non-empty space-free list",
                    });
                }
                let mut params = vec![subject.clone(), scope.clone(), caps.clone()];
                if let Some(expiry) = expiry {
                    params.push(format!("expiry={expiry}"));
                }
                ("GRANT", params, None)
            }
            Command::Revoke {
                subject,
                scope,
                caps,
                epoch,
            } => {
                let mut params = vec![subject.clone(), scope.clone()];
                if let Some(caps) = caps {
                    params.push(format!("caps={caps}"));
                }
                if let Some(epoch) = epoch {
                    params.push(epoch.to_string());
                }
                ("REVOKE", params, None)
            }
            Command::RoleCreate {
                scope,
                color,
                caps,
                name,
            } => {
                if !caps_ok(caps) {
                    return Err(SerializeError::BadParam {
                        param: caps.clone(),
                        reason: "caps must be a non-empty space-free list",
                    });
                }
                (
                    "ROLE",
                    vec![
                        "CREATE".to_string(),
                        scope.clone(),
                        color.clone(),
                        caps.clone(),
                    ],
                    Some(name.clone()),
                )
            }
            Command::RoleDelete { scope, name } => (
                "ROLE",
                vec!["DELETE".to_string(), scope.clone()],
                Some(name.clone()),
            ),
            Command::RoleAssign {
                scope,
                account,
                name,
            } => (
                "ROLE",
                vec!["ASSIGN".to_string(), scope.clone(), account.to_string()],
                Some(name.clone()),
            ),
            Command::RoleUnassign {
                scope,
                account,
                name,
            } => (
                "ROLE",
                vec!["UNASSIGN".to_string(), scope.clone(), account.to_string()],
                Some(name.clone()),
            ),
            Command::RolesList { scope } => ("ROLES", vec![scope.clone()], None),
            Command::RolesOf { scope, account } => {
                ("ROLES-OF", vec![scope.clone(), account.to_string()], None)
            }
            Command::ChannelCreate {
                channel,
                policy,
                kind,
            } => {
                let mut params = vec!["CREATE".to_string(), channel.to_string()];
                if let Some(policy) = policy {
                    params.push(policy.to_string());
                }
                // `text` is the default — only emit an explicit `voice`.
                if *kind != crate::ChannelKind::Text {
                    params.push(kind.to_string());
                }
                ("CHANNEL", params, None)
            }
            Command::ChannelPolicy {
                channel,
                policy,
                purge,
            } => {
                let mut params = vec![
                    "POLICY".to_string(),
                    channel.to_string(),
                    policy.to_string(),
                ];
                if *purge {
                    params.push("purge".to_string());
                }
                ("CHANNEL", params, None)
            }
            Command::ChannelMeta {
                channel,
                key,
                value,
            } => (
                "CHANNEL",
                vec!["META".to_string(), channel.to_string(), key.clone()],
                Some(value.clone()),
            ),
            Command::ChannelDelete { channel, confirm } => (
                "CHANNEL",
                vec![
                    "DELETE".to_string(),
                    channel.to_string(),
                    confirm.to_string(),
                ],
                None,
            ),
            Command::ChannelRename { channel, new_name } => (
                "CHANNEL",
                vec![
                    "RENAME".to_string(),
                    channel.to_string(),
                    new_name.to_string(),
                ],
                None,
            ),
            Command::InviteMint {
                scope,
                max_uses,
                expiry,
            } => {
                let mut params = vec!["MINT".to_string(), scope.clone()];
                if let Some(max_uses) = max_uses {
                    params.push(format!("max-uses={max_uses}"));
                }
                if let Some(expiry) = expiry {
                    params.push(format!("expiry={expiry}"));
                }
                ("INVITE", params, None)
            }
            Command::InviteRevoke { invite_id } => (
                "INVITE",
                vec!["REVOKE".to_string(), invite_id.clone()],
                None,
            ),
            Command::InviteRedeem { token } => {
                ("INVITE", vec!["REDEEM".to_string(), token.clone()], None)
            }
            Command::EmojiAdd {
                namespace,
                name,
                media,
            } => (
                "EMOJI",
                vec![
                    "ADD".to_string(),
                    namespace.to_string(),
                    name.clone(),
                    media.clone(),
                ],
                None,
            ),
            Command::EmojiRemove { namespace, name } => (
                "EMOJI",
                vec!["REMOVE".to_string(), namespace.to_string(), name.clone()],
                None,
            ),
            Command::EmojiList { namespace } => (
                "EMOJI",
                vec!["LIST".to_string(), namespace.to_string()],
                None,
            ),
            Command::NsCreate {
                name,
                visibility,
                root_key,
            } => {
                tags.insert("root".to_string(), root_key.clone());
                (
                    "NS",
                    vec![
                        "CREATE".to_string(),
                        name.to_string(),
                        visibility.to_string(),
                    ],
                    None,
                )
            }
            Command::NsMeta { name, key, value } => (
                "NS",
                vec!["META".to_string(), name.to_string(), key.clone()],
                Some(value.clone()),
            ),
            Command::NsVisibility { name, visibility } => (
                "NS",
                vec![
                    "VISIBILITY".to_string(),
                    name.to_string(),
                    visibility.to_string(),
                ],
                None,
            ),
            Command::NsDelegate {
                name,
                subject,
                caps,
            } => (
                "NS",
                vec![
                    "DELEGATE".to_string(),
                    name.to_string(),
                    subject.clone(),
                    caps.clone(),
                ],
                None,
            ),
            Command::NsDelete { name, confirm } => (
                "NS",
                vec!["DELETE".to_string(), name.to_string(), confirm.to_string()],
                None,
            ),
            Command::NsJoin { name } => ("NS", vec!["JOIN".to_string(), name.to_string()], None),
            Command::NsTransfer {
                name,
                new_owner,
                signature,
            } => {
                tags.insert("sig".to_string(), signature.clone());
                (
                    "NS",
                    vec![
                        "TRANSFER".to_string(),
                        name.to_string(),
                        new_owner.to_string(),
                    ],
                    None,
                )
            }
            Command::NsRecoverySet { name, m, keys } => (
                "NS",
                vec![
                    "RECOVERY".to_string(),
                    "SET".to_string(),
                    name.to_string(),
                    m.to_string(),
                    keys.clone(),
                ],
                None,
            ),
            Command::NsRecover { name, rotation } => (
                "NS",
                vec!["RECOVER".to_string(), name.to_string(), rotation.clone()],
                None,
            ),
            Command::NsRecoveryCancel { name, signature } => {
                tags.insert("sig".to_string(), signature.clone());
                (
                    "NS",
                    vec![
                        "RECOVERY".to_string(),
                        "CANCEL".to_string(),
                        name.to_string(),
                    ],
                    None,
                )
            }
            Command::Discover { cursor } => ("DISCOVER", cursor.iter().cloned().collect(), None),
            Command::Federate { network, namespace } => {
                ("FEDERATE", vec![format!("{network}/{namespace}")], None)
            }
            Command::FSession { fsid, op } => match op {
                FSessionOp::Open { account } => (
                    "FSESSION",
                    vec![fsid.clone(), "OPEN".to_string(), account.clone()],
                    None,
                ),
                FSessionOp::Cmd { line } => (
                    "FSESSION",
                    vec![fsid.clone(), "CMD".to_string()],
                    Some(line.clone()),
                ),
                FSessionOp::Reply { line } => (
                    "FSESSION",
                    vec![fsid.clone(), "REPLY".to_string()],
                    Some(line.clone()),
                ),
                FSessionOp::Close => ("FSESSION", vec![fsid.clone(), "CLOSE".to_string()], None),
            },
            Command::Channels { namespace } => ("CHANNELS", vec![namespace.to_string()], None),
            Command::Report {
                msgid,
                category,
                scope,
                note,
            } => {
                if !report_category_ok(category) {
                    return Err(SerializeError::BadParam {
                        param: category.clone(),
                        reason: "category must be normative or x- prefixed, no spaces",
                    });
                }
                // `ns` is the default — emit it only when it differs, so the
                // canonical form of a bare report stays minimal (and a note
                // never gets mistaken for the optional scope on re-parse).
                let mut params = vec![msgid.to_string(), category.clone()];
                if *scope != ReportScope::Ns || note.is_some() {
                    params.push(scope.to_string());
                }
                ("REPORT", params, note.clone())
            }
            Command::ReportsList {
                scope,
                status,
                cursor,
            } => {
                let mut params = vec!["LIST".to_string(), scope.clone()];
                if let Some(status) = status {
                    params.push(format!("status={status}"));
                }
                if let Some(cursor) = cursor {
                    params.push(cursor.clone());
                }
                ("REPORTS", params, None)
            }
            Command::ReportsResolve {
                report_id,
                action,
                note,
            } => (
                "REPORTS",
                vec!["RESOLVE".to_string(), report_id.clone(), action.to_string()],
                note.clone(),
            ),
            Command::AuthBridge { network, token } => (
                "AUTH",
                vec!["BRIDGE".to_string(), network.to_string(), token.clone()],
                None,
            ),
            Command::BridgePropose {
                scope,
                peer,
                history,
                media,
                typing,
                voice,
                manifest,
            } => {
                if let Some(manifest) = manifest {
                    tags.insert("manifest".to_string(), manifest.clone());
                }
                (
                    "BRIDGE",
                    vec![
                        "PROPOSE".to_string(),
                        scope.clone(),
                        peer.to_string(),
                        format!("history={history}"),
                        format!("media={media}"),
                        format!("typing={}", if *typing { "yes" } else { "no" }),
                        format!("voice={}", if *voice { "yes" } else { "no" }),
                    ],
                    None,
                )
            }
            Command::BridgeAccept { peer, version } => (
                "BRIDGE",
                vec!["ACCEPT".to_string(), peer.to_string(), version.to_string()],
                None,
            ),
            Command::BridgeAdd { peer, channel } => (
                "BRIDGE",
                vec!["ADD".to_string(), peer.to_string(), channel.to_string()],
                None,
            ),
            Command::BridgeRemove { peer, channel } => (
                "BRIDGE",
                vec!["REMOVE".to_string(), peer.to_string(), channel.to_string()],
                None,
            ),
            Command::BridgeSever { peer } => {
                ("BRIDGE", vec!["SEVER".to_string(), peer.to_string()], None)
            }
            Command::BridgeRequest { ns } => {
                ("BRIDGE", vec!["REQUEST".to_string(), ns.to_string()], None)
            }
            Command::NetblockAdd { network, reason } => (
                "NETBLOCK",
                vec!["ADD".to_string(), network.to_string()],
                reason.clone(),
            ),
            Command::NetblockRemove { network } => (
                "NETBLOCK",
                vec!["REMOVE".to_string(), network.to_string()],
                None,
            ),
            Command::NetblockList => ("NETBLOCK", vec!["LIST".to_string()], None),
            Command::MediaBlock { hash, reason } => (
                "MEDIA",
                vec!["BLOCK".to_string(), hash.clone()],
                reason.clone(),
            ),
            Command::MediaUnblock { hash } => {
                ("MEDIA", vec!["UNBLOCK".to_string(), hash.clone()], None)
            }
            Command::MediaBlocks => ("MEDIA", vec!["BLOCKS".to_string()], None),
            Command::ReportForward {
                report_id,
                msgid,
                category,
                note,
            } => {
                if !report_category_ok(category) {
                    return Err(SerializeError::BadParam {
                        param: category.clone(),
                        reason: "category must be normative or x- prefixed, no spaces",
                    });
                }
                (
                    "REPORT-FORWARD",
                    vec![report_id.clone(), msgid.to_string(), category.clone()],
                    note.clone(),
                )
            }
            Command::Mute {
                scope,
                account,
                reason,
            } => (
                "MUTE",
                vec![scope.clone(), account.to_string()],
                reason.clone(),
            ),
            Command::Unmute { scope, account } => {
                ("UNMUTE", vec![scope.clone(), account.to_string()], None)
            }
            Command::Ban {
                scope,
                account,
                reason,
            } => (
                "BAN",
                vec![scope.clone(), account.to_string()],
                reason.clone(),
            ),
            Command::Unban { scope, account } => {
                ("UNBAN", vec![scope.clone(), account.to_string()], None)
            }
            Command::Kick {
                channel,
                account,
                reason,
            } => (
                "KICK",
                vec![channel.to_string(), account.to_string()],
                reason.clone(),
            ),
            Command::ModList { scope } => ("MODLIST", vec![scope.clone()], None),
            Command::VoiceJoin { channel } => {
                ("VOICE", vec!["JOIN".to_string(), channel.to_string()], None)
            }
            Command::VoiceLeave { channel } => (
                "VOICE",
                vec!["LEAVE".to_string(), channel.to_string()],
                None,
            ),
            Command::VoiceDesc { channel, sdp } => (
                "VOICE",
                vec!["DESC".to_string(), channel.to_string()],
                Some(sdp.clone()),
            ),
            Command::VoiceCand { channel, candidate } => (
                "VOICE",
                vec!["CAND".to_string(), channel.to_string()],
                Some(candidate.clone()),
            ),
            Command::VoiceRequest { scope, channel } => (
                "VOICE",
                vec!["REQUEST".to_string(), scope.clone(), channel.to_string()],
                None,
            ),
            Command::ProfileSet { display, avatar } => {
                if let Some(display) = display {
                    tags.insert("display".to_string(), display.clone());
                }
                if let Some(avatar) = avatar {
                    tags.insert("avatar".to_string(), avatar.clone());
                }
                ("PROFILE", vec!["SET".to_string()], None)
            }
            Command::ProfilesQuery { accounts } => ("PROFILES", accounts.clone(), None),
            Command::VerifyEmail { address } => {
                ("VERIFY", vec!["EMAIL".to_string(), address.clone()], None)
            }
            Command::VerifyBirthday { date } => {
                ("VERIFY", vec!["BIRTHDAY".to_string(), date.clone()], None)
            }
            Command::VerifyConfirm { kind, code } => (
                "VERIFY",
                vec!["CONFIRM".to_string(), kind.clone(), code.clone()],
                None,
            ),
            Command::VerifyList => ("VERIFY", vec!["LIST".to_string()], None),
            Command::Unknown { .. } => {
                return Err(SerializeError::Unrepresentable("unknown command"));
            }
        };
        Ok(Line {
            tags,
            verb: verb.to_string(),
            params,
            trailing,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::line::MAX_LABEL_BYTES;

    const MSGID: &str = "hda.example/01ARZ3NDEKTSV4RRFFQ69G5FAV";

    /// Serialize → parse must reproduce the request exactly.
    fn round_trip(request: &Request) {
        let wire = request.serialize().unwrap();
        assert_eq!(&Request::parse(&wire).unwrap(), request, "wire: {wire}");
    }

    fn parse(input: &str) -> Command {
        Request::parse(input).unwrap().command
    }

    #[test]
    fn hello_round_trips() {
        let request = Request::new(Command::Hello {
            version: "weft/1".into(),
        });
        assert_eq!(request.serialize().unwrap(), "HELLO weft/1"); // spec §3.6 example
        round_trip(&request);
    }

    #[test]
    fn register_keeps_spaces_in_password() {
        let request = Request::new(Command::Register {
            account: "ada".parse().unwrap(),
            password: "correct horse battery".into(),
        });
        round_trip(&request);
        assert_eq!(
            request.serialize().unwrap(),
            "REGISTER ada :correct horse battery"
        );
    }

    #[test]
    fn auth_password_round_trips() {
        round_trip(&Request::new(Command::AuthPassword {
            account: "ada".parse().unwrap(),
            password: ":p4ss with space".into(),
        }));
    }

    #[test]
    fn auth_key_flow_round_trips() {
        round_trip(&Request::new(Command::AuthKey {
            account: "ada".parse().unwrap(),
            pubkey: "BASE64KEY==".into(),
        }));
        round_trip(&Request::new(Command::AuthProof {
            signature: "BASE64SIG==".into(),
        }));
        round_trip(&Request::new(Command::AuthEnroll {
            pubkey: "BASE64KEY2==".into(),
        }));
    }

    #[test]
    fn bad_auth_subcommand_is_typed_error() {
        assert_eq!(
            Request::parse("AUTH TELEPATHY ada"),
            Err(ParseError::BadParam {
                verb: "AUTH",
                what: "subcommand",
                value: "TELEPATHY".into()
            })
        );
    }

    #[test]
    fn quit_ping_pong_round_trip() {
        round_trip(&Request::new(Command::Quit { reason: None }));
        round_trip(&Request::new(Command::Quit {
            reason: Some("bye now".into()),
        }));
        round_trip(&Request::new(Command::Ping {
            token: Some("t1".into()),
        }));
        round_trip(&Request::new(Command::Pong { token: None }));
    }

    #[test]
    fn presence_all_statuses_round_trip() {
        for status in [
            PresenceStatus::Online,
            PresenceStatus::Away,
            PresenceStatus::Dnd,
            PresenceStatus::Invisible,
            PresenceStatus::Offline,
        ] {
            round_trip(&Request::new(Command::Presence { status }));
        }
        assert!(Request::parse("PRESENCE sleeping").is_err());
    }

    #[test]
    fn join_part_typing_mark_round_trip() {
        round_trip(&Request::new(Command::Join {
            channel: "#gaming/general".parse().unwrap(),
            invite: Some("INVREF".into()),
        }));
        round_trip(&Request::new(Command::Part {
            channel: "#general".parse().unwrap(),
            reason: Some("afk".into()),
        }));
        round_trip(&Request::new(Command::Typing {
            channel: "#general".parse().unwrap(),
            state: TypingState::Stop,
        }));
        round_trip(&Request::new(Command::Mark {
            channel: "#general".parse().unwrap(),
            msgid: MSGID.parse().unwrap(),
        }));
        round_trip(&Request::new(Command::Unread { channel: None }));
        round_trip(&Request::new(Command::Unread {
            channel: Some("#general".parse().unwrap()),
        }));
        round_trip(&Request::new(Command::Members {
            channel: "#general".parse().unwrap(),
            cursor: None,
        }));
        round_trip(&Request::new(Command::Members {
            channel: "#general".parse().unwrap(),
            cursor: Some("c2".to_string()),
        }));
        round_trip(&Request::new(Command::Pin {
            msgid: MSGID.parse().unwrap(),
        }));
        round_trip(&Request::new(Command::Unpin {
            msgid: MSGID.parse().unwrap(),
        }));
        round_trip(&Request::new(Command::Pins {
            channel: "#general".parse().unwrap(),
        }));
        round_trip(&Request::new(Command::Search {
            channel: "#general".parse().unwrap(),
            query: "deploy plan v2".to_string(),
        }));
        round_trip(&Request::new(Command::Caps {
            account: "ada".parse().unwrap(),
            scope: "#general".to_string(),
        }));
        round_trip(&Request::new(Command::RoleCreate {
            scope: "ns:gaming".to_string(),
            color: "#e8b93d".to_string(),
            caps: "mute,ban,kick,pin".to_string(),
            name: "Head Moderator".to_string(),
        }));
        round_trip(&Request::new(Command::RoleDelete {
            scope: "ns:gaming".to_string(),
            name: "Head Moderator".to_string(),
        }));
        round_trip(&Request::new(Command::RoleAssign {
            scope: "ns:gaming".to_string(),
            account: "bob".parse().unwrap(),
            name: "Head Moderator".to_string(),
        }));
        round_trip(&Request::new(Command::RoleUnassign {
            scope: "ns:gaming".to_string(),
            account: "bob".parse().unwrap(),
            name: "Head Moderator".to_string(),
        }));
        round_trip(&Request::new(Command::RolesList {
            scope: "ns:gaming".to_string(),
        }));
        round_trip(&Request::new(Command::RolesOf {
            scope: "ns:gaming".to_string(),
            account: "bob".parse().unwrap(),
        }));
        assert_eq!(
            Request::parse("JOIN"),
            Err(ParseError::MissingParam {
                verb: "JOIN",
                what: "channel"
            })
        );
    }

    #[test]
    fn msg_channel_with_meta_round_trips() {
        round_trip(&Request::with_label(
            Command::Msg {
                target: "#general".parse().unwrap(),
                body: Some("hello world".into()),
                meta: MsgMeta {
                    fmt: Some("md".into()),
                    reply_to: Some(MSGID.parse().unwrap()),
                    thread: Some(MSGID.parse().unwrap()),
                    attachments: vec![],
                    system: None,
                },
            },
            "req-1",
        ));
    }

    #[test]
    fn msg_dm_target_round_trips() {
        let request = Request::new(Command::Msg {
            target: "@ada".parse().unwrap(),
            body: Some("hi".into()),
            meta: MsgMeta::default(),
        });
        assert_eq!(request.serialize().unwrap(), "MSG @ada :hi");
        round_trip(&request);
    }

    #[test]
    fn stream_offer_round_trips() {
        let request = Request::new(Command::StreamOffer {
            mode: StreamMode::Media,
            mime: "image/png".into(),
            bytes: 20480,
        });
        assert_eq!(
            request.serialize().unwrap(),
            "STREAM OFFER media image/png 20480"
        );
        round_trip(&request);

        // backfill mode shares the shape (M-media-4 wires the transfer).
        round_trip(&Request::new(Command::StreamOffer {
            mode: StreamMode::Backfill,
            mime: "application/weft-history".into(),
            bytes: 0,
        }));
    }

    #[test]
    fn stream_offer_rejects_bad_params() {
        assert!(Request::parse("STREAM OFFER media image/png notanumber").is_err());
        assert!(Request::parse("STREAM OFFER bogus image/png 1").is_err());
        assert!(Request::parse("STREAM WAT").is_err());
        // A bare STREAM with no subcommand is malformed, not Unknown.
        assert!(Request::parse("STREAM").is_err());
    }

    #[test]
    fn msg_attachments_only_and_limits() {
        // Empty trailing (bare media, §13) is preserved as Some("").
        let request = Request::new(Command::Msg {
            target: "#general".parse().unwrap(),
            body: Some(String::new()),
            meta: MsgMeta {
                attachments: vec!["weft-media://hda.example/b3hash".into()],
                ..MsgMeta::default()
            },
        });
        round_trip(&request);

        let over = Request::new(Command::Msg {
            target: "#general".parse().unwrap(),
            body: None,
            meta: MsgMeta {
                attachments: vec!["m".into(); 11],
                ..MsgMeta::default()
            },
        });
        assert_eq!(over.serialize(), Err(SerializeError::TooManyAttachments));
        assert_eq!(
            Request::parse("@attach.11=x MSG #a :"),
            Err(ParseError::TooManyAttachments)
        );
    }

    #[test]
    fn attachment_indices_sort_numerically() {
        // BTreeMap would yield attach.10 < attach.2 lexically; codec must not.
        let line = "@attach.1=a;attach.2=b;attach.10=j MSG #c :";
        // attach.10 alone is fine — but 10 items max, index ≤ 10, so this parses.
        let Command::Msg { meta, .. } = parse(line) else {
            panic!()
        };
        assert_eq!(meta.attachments, vec!["a", "b", "j"]);
    }

    #[test]
    fn edit_delete_round_trip() {
        let edit = Request::with_label(
            Command::Edit {
                msgid: MSGID.parse().unwrap(),
                body: "fixed the typo".into(),
            },
            "e1",
        );
        assert_eq!(
            edit.serialize().unwrap(),
            format!("@label=e1 EDIT {MSGID} :fixed the typo")
        );
        round_trip(&edit);
        round_trip(&Request::new(Command::Delete {
            msgid: MSGID.parse().unwrap(),
        }));
        // EDIT requires a body (empty trailing is a meaningful empty body).
        assert!(Request::parse(&format!("EDIT {MSGID}")).is_err());
    }

    #[test]
    fn react_unreact_round_trip_and_emoji_limits() {
        round_trip(&Request::new(Command::React {
            msgid: MSGID.parse().unwrap(),
            emoji: "🦀".into(),
        }));
        round_trip(&Request::new(Command::Unreact {
            msgid: MSGID.parse().unwrap(),
            emoji: "ferris".into(), // bare shortcode (spec §18 #8)
        }));
        // >32 bytes rejected both directions.
        assert!(Request::parse(&format!("REACT {MSGID} {}", "x".repeat(33))).is_err());
        let over = Request::new(Command::React {
            msgid: MSGID.parse().unwrap(),
            emoji: "x".repeat(33),
        });
        assert!(over.serialize().is_err());
        // Leading colon collides with the trailing marker (§4).
        assert!(Request::parse(&format!("REACT {MSGID} :colon:")).is_err());
    }

    #[test]
    fn history_params_any_order_round_trip() {
        let request = Request::with_label(
            Command::History {
                target: "#general".parse().unwrap(),
                before: Some(MSGID.parse().unwrap()),
                after: None,
                limit: Some(50),
                thread: None,
            },
            "h1",
        );
        assert_eq!(
            request.serialize().unwrap(),
            format!("@label=h1 HISTORY #general before={MSGID} limit=50")
        );
        round_trip(&request);

        // Any order, DM targets, unknown keys ignored (lenient-in).
        let parsed =
            Request::parse(&format!("HISTORY @ada limit=10 x-custom=1 after={MSGID}")).unwrap();
        let Command::History {
            target,
            after: Some(_),
            limit: Some(10),
            before: None,
            thread: None,
        } = parsed.command
        else {
            panic!("bad parse: {parsed:?}");
        };
        assert_eq!(target.to_string(), "@ada");

        assert!(Request::parse("HISTORY #general limit=abc").is_err());
    }

    #[test]
    fn grant_revoke_round_trip() {
        round_trip(&Request::with_label(
            Command::Grant {
                subject: "ada".into(),
                scope: "ns:gaming".into(),
                caps: "ban,grant:send".into(),
                expiry: Some(3600),
            },
            "g1",
        ));
        assert_eq!(
            Request::new(Command::Grant {
                subject: "B64KEY==".into(),
                scope: "#general".into(),
                caps: "send".into(),
                expiry: None,
            })
            .serialize()
            .unwrap(),
            "GRANT B64KEY== #general send"
        );
        // Caps with a space are rejected both ways.
        assert!(
            Request::parse("GRANT ada * send react").is_err()
                || matches!(
                    Request::parse("GRANT ada * send react").unwrap().command,
                    Command::Grant { caps, .. } if caps == "send"
                )
        );
        // REVOKE: caps=list and a bare epoch, any order.
        let parsed = Request::parse("REVOKE ada ns:x caps=ban,kick 7").unwrap();
        let Command::Revoke { caps, epoch, .. } = parsed.command else {
            panic!()
        };
        assert_eq!(caps.as_deref(), Some("ban,kick"));
        assert_eq!(epoch, Some(7));
        round_trip(&Request::new(Command::Revoke {
            subject: "ada".into(),
            scope: "#general".into(),
            caps: None,
            epoch: None,
        }));
    }

    #[test]
    fn channel_verbs_round_trip() {
        round_trip(&Request::new(Command::ChannelCreate {
            channel: "#new".parse().unwrap(),
            policy: Some("retained:30d".parse().unwrap()),
            kind: crate::ChannelKind::Text,
        }));
        assert_eq!(
            Request::new(Command::ChannelCreate {
                channel: "#new".parse().unwrap(),
                policy: None,
                kind: crate::ChannelKind::Text,
            })
            .serialize()
            .unwrap(),
            "CHANNEL CREATE #new"
        );
        // A voice channel: the `voice` kind rides after the (optional) policy and
        // round-trips; parse order is lenient (kind before or after policy).
        round_trip(&Request::new(Command::ChannelCreate {
            channel: "#lounge".parse().unwrap(),
            policy: None,
            kind: crate::ChannelKind::Voice,
        }));
        assert_eq!(
            Request::new(Command::ChannelCreate {
                channel: "#lounge".parse().unwrap(),
                policy: None,
                kind: crate::ChannelKind::Voice,
            })
            .serialize()
            .unwrap(),
            "CHANNEL CREATE #lounge voice"
        );
        assert_eq!(
            Request::parse("CHANNEL CREATE #lounge voice")
                .unwrap()
                .command,
            Command::ChannelCreate {
                channel: "#lounge".parse().unwrap(),
                policy: None,
                kind: crate::ChannelKind::Voice,
            }
        );
        round_trip(&Request::new(Command::ChannelPolicy {
            channel: "#c".parse().unwrap(),
            policy: "ephemeral".parse().unwrap(),
            purge: true,
        }));
        round_trip(&Request::new(Command::ChannelMeta {
            channel: "#c".parse().unwrap(),
            key: "topic".into(),
            value: "the new topic here".into(),
        }));
        round_trip(&Request::new(Command::ChannelDelete {
            channel: "#c".parse().unwrap(),
            confirm: "#c".parse().unwrap(),
        }));
        round_trip(&Request::new(Command::ChannelRename {
            channel: "#ns/old".parse().unwrap(),
            new_name: "#ns/new".parse().unwrap(),
        }));
        assert_eq!(
            Request::parse("CHANNEL RENAME #ns/old #ns/new")
                .unwrap()
                .command,
            Command::ChannelRename {
                channel: "#ns/old".parse().unwrap(),
                new_name: "#ns/new".parse().unwrap(),
            }
        );
        assert_eq!(
            Request::parse("CHANNEL FROB #x"),
            Err(ParseError::BadParam {
                verb: "CHANNEL",
                what: "subcommand",
                value: "FROB".into()
            })
        );
    }

    #[test]
    fn invite_verbs_round_trip() {
        round_trip(&Request::with_label(
            Command::InviteMint {
                scope: "ns:gaming".into(),
                max_uses: Some(10),
                expiry: Some(86400),
            },
            "i1",
        ));
        round_trip(&Request::new(Command::InviteRevoke {
            invite_id: "inv-abc".into(),
        }));
        round_trip(&Request::new(Command::InviteRedeem {
            token: "B64TOKEN==".into(),
        }));
    }

    #[test]
    fn emoji_verbs_round_trip() {
        let add = Request::with_label(
            Command::EmojiAdd {
                namespace: "gaming".parse().unwrap(),
                name: "partyblob".into(),
                media: "weft-media://hda.example/abc123".into(),
            },
            "e1",
        );
        assert_eq!(
            add.serialize().unwrap(),
            "@label=e1 EMOJI ADD gaming partyblob weft-media://hda.example/abc123"
        );
        round_trip(&add);
        round_trip(&Request::new(Command::EmojiRemove {
            namespace: "gaming".parse().unwrap(),
            name: "partyblob".into(),
        }));
        round_trip(&Request::new(Command::EmojiList {
            namespace: "gaming".parse().unwrap(),
        }));
        assert_eq!(
            Request::parse("EMOJI FROB gaming"),
            Err(ParseError::BadParam {
                verb: "EMOJI",
                what: "subcommand",
                value: "FROB".into()
            })
        );
    }

    #[test]
    fn ns_verbs_round_trip() {
        let create = Request::with_label(
            Command::NsCreate {
                name: "gaming".parse().unwrap(),
                visibility: crate::types::Visibility::Public,
                root_key: "B64ROOTKEY==".into(),
            },
            "n1",
        );
        let wire = create.serialize().unwrap();
        assert!(wire.contains("root=B64ROOTKEY=="), "{wire}");
        assert!(wire.contains("NS CREATE gaming public"), "{wire}");
        round_trip(&create);
        // Default tier is unlisted; root tag mandatory.
        let parsed = Request::parse("@root=K== NS CREATE gaming").unwrap();
        assert!(matches!(
            parsed.command,
            Command::NsCreate {
                visibility: crate::types::Visibility::Unlisted,
                ..
            }
        ));
        assert_eq!(
            Request::parse("NS CREATE gaming"),
            Err(ParseError::MissingParam {
                verb: "NS",
                what: "root tag (namespace root pubkey)"
            })
        );

        round_trip(&Request::new(Command::NsMeta {
            name: "gaming".parse().unwrap(),
            key: "title".into(),
            value: "The Gaming Lounge".into(),
        }));
        round_trip(&Request::new(Command::NsVisibility {
            name: "gaming".parse().unwrap(),
            visibility: crate::types::Visibility::Private,
        }));
        round_trip(&Request::new(Command::NsDelegate {
            name: "gaming".parse().unwrap(),
            subject: "ada".into(),
            caps: "ban,kick".into(),
        }));
        round_trip(&Request::new(Command::NsDelete {
            name: "gaming".parse().unwrap(),
            confirm: "gaming".parse().unwrap(),
        }));
        round_trip(&Request::new(Command::NsJoin {
            name: "gaming".parse().unwrap(),
        }));
        assert!(Request::parse("NS FROB x").is_err());
    }

    #[test]
    fn ns_recovery_verbs_round_trip() {
        let transfer = Request::with_label(
            Command::NsTransfer {
                name: "gaming".parse().unwrap(),
                new_owner: "bob".parse().unwrap(),
                signature: "B64SIG==".into(),
            },
            "t1",
        );
        assert!(transfer.serialize().unwrap().contains("sig=B64SIG=="));
        round_trip(&transfer);
        assert_eq!(
            Request::parse("NS TRANSFER gaming bob"),
            Err(ParseError::MissingParam {
                verb: "NS",
                what: "sig tag (root signature)"
            })
        );

        round_trip(&Request::new(Command::NsRecoverySet {
            name: "gaming".parse().unwrap(),
            m: 2,
            keys: "K1==,K2==,K3==".into(),
        }));
        round_trip(&Request::new(Command::NsRecover {
            name: "gaming".parse().unwrap(),
            rotation: "B64ROTATION==".into(),
        }));
        round_trip(&Request::with_label(
            Command::NsRecoveryCancel {
                name: "gaming".parse().unwrap(),
                signature: "B64SIG==".into(),
            },
            "c1",
        ));
        // NS RECOVERY FROB is a bad recovery action.
        assert!(Request::parse("NS RECOVERY FROB gaming").is_err());
    }

    #[test]
    fn channels_round_trips() {
        round_trip(&Request::new(Command::Channels {
            namespace: "gaming".parse().unwrap(),
        }));
    }

    #[test]
    fn report_round_trips() {
        // Bare report: scope defaults to ns, stays minimal on the wire.
        let bare = Request::with_label(
            Command::Report {
                msgid: MSGID.parse().unwrap(),
                category: "harassment".into(),
                scope: ReportScope::Ns,
                note: None,
            },
            "r1",
        );
        assert_eq!(
            bare.serialize().unwrap(),
            format!("@label=r1 REPORT {MSGID} harassment")
        );
        round_trip(&bare);
        // Net scope + note round-trips; note travels in the trailing.
        round_trip(&Request::new(Command::Report {
            msgid: MSGID.parse().unwrap(),
            category: "csam".into(),
            scope: ReportScope::Net,
            note: Some("see the attached screenshot".into()),
        }));
        // ns-scope report *with* a note: scope must be emitted so the note
        // is not re-parsed as the optional scope.
        round_trip(&Request::new(Command::Report {
            msgid: MSGID.parse().unwrap(),
            category: "x-doxxing".into(),
            scope: ReportScope::Ns,
            note: Some("posted my address".into()),
        }));
        // Unknown category rejected both ways.
        assert!(Request::parse(&format!("REPORT {MSGID} slander")).is_err());
    }

    #[test]
    fn reports_list_resolve_round_trip() {
        round_trip(&Request::with_label(
            Command::ReportsList {
                scope: "ns:gaming".into(),
                status: Some(ReportStatus::Open),
                cursor: Some("cur-9".into()),
            },
            "l1",
        ));
        round_trip(&Request::new(Command::ReportsList {
            scope: "*".into(),
            status: None,
            cursor: None,
        }));
        round_trip(&Request::new(Command::ReportsResolve {
            report_id: "rep-42".into(),
            action: ResolveAction::ContentRemoved,
            note: Some("removed and warned".into()),
        }));
        round_trip(&Request::new(Command::ReportsResolve {
            report_id: "rep-7".into(),
            action: ResolveAction::Dismissed,
            note: None,
        }));
        assert!(Request::parse("REPORTS FROB ns").is_err());
    }

    #[test]
    fn discover_round_trips() {
        round_trip(&Request::new(Command::Discover { cursor: None }));
        round_trip(&Request::with_label(
            Command::Discover {
                cursor: Some("cur-42".into()),
            },
            "d1",
        ));
    }

    #[test]
    fn auth_bridge_round_trips() {
        round_trip(&Request::new(Command::AuthBridge {
            network: "hda.example".parse().unwrap(),
            token: "B64BRIDGETOKEN==".into(),
        }));
        assert_eq!(
            Request::new(Command::AuthBridge {
                network: "hda.example".parse().unwrap(),
                token: "T==".into(),
            })
            .serialize()
            .unwrap(),
            "AUTH BRIDGE hda.example T=="
        );
    }

    #[test]
    fn bridge_verbs_round_trip() {
        let propose = Request::with_label(
            Command::BridgePropose {
                scope: "#general".into(),
                peer: "hda.example".parse().unwrap(),
                history: HistoryMode::Full,
                media: MediaMode::MirrorMax(1_048_576),
                typing: true,
                voice: true,
                manifest: Some("B64MANIFEST==".into()),
            },
            "b1",
        );
        let wire = propose.serialize().unwrap();
        assert!(wire.contains("manifest=B64MANIFEST=="), "{wire}");
        assert!(
            wire.contains("BRIDGE PROPOSE #general hda.example history=full media=mirror-max:1048576 typing=yes voice=yes"),
            "{wire}"
        );
        round_trip(&propose);

        // Minimal propose: strictest-safe defaults, no manifest tag.
        let minimal = Request::parse("BRIDGE PROPOSE ns:gaming peer.example").unwrap();
        assert_eq!(
            minimal.command,
            Command::BridgePropose {
                scope: "ns:gaming".into(),
                peer: "peer.example".parse().unwrap(),
                history: HistoryMode::FromEpoch,
                media: MediaMode::None,
                typing: false,
                voice: false,
                manifest: None,
            }
        );

        round_trip(&Request::new(Command::BridgeAccept {
            peer: "hda.example".parse().unwrap(),
            version: 3,
        }));
        round_trip(&Request::new(Command::BridgeAdd {
            peer: "hda.example".parse().unwrap(),
            channel: "#gaming/lobby".parse().unwrap(),
        }));
        round_trip(&Request::new(Command::BridgeRemove {
            peer: "hda.example".parse().unwrap(),
            channel: "#general".parse().unwrap(),
        }));
        round_trip(&Request::new(Command::BridgeSever {
            peer: "hda.example".parse().unwrap(),
        }));
        round_trip(&Request::new(Command::BridgeRequest {
            ns: "gaming".parse().unwrap(),
        }));
        round_trip(&Request::new(Command::Federate {
            network: "hda.example".parse().unwrap(),
            namespace: "gaming".parse().unwrap(),
        }));
        // §11.10 federation-session tunnel frames.
        round_trip(&Request::new(Command::FSession {
            fsid: "7".to_string(),
            op: FSessionOp::Open {
                account: "alice".to_string(),
            },
        }));
        round_trip(&Request::new(Command::FSession {
            fsid: "7".to_string(),
            op: FSessionOp::Cmd {
                // The inner line keeps its own tags + trailing verbatim.
                line: "@label=m1 MUTE #gaming/general bob :spam".to_string(),
            },
        }));
        round_trip(&Request::new(Command::FSession {
            fsid: "7".to_string(),
            op: FSessionOp::Reply {
                line: "MODERATED #gaming/general bob mute".to_string(),
            },
        }));
        round_trip(&Request::new(Command::FSession {
            fsid: "7".to_string(),
            op: FSessionOp::Close,
        }));
        assert!(Request::parse("FEDERATE nonslash").is_err());
        assert!(Request::parse("BRIDGE FROB peer.example").is_err());
        assert!(Request::parse("BRIDGE ACCEPT peer.example notanumber").is_err());
    }

    #[test]
    fn netblock_verbs_round_trip() {
        round_trip(&Request::with_label(
            Command::NetblockAdd {
                network: "evil.example".parse().unwrap(),
                reason: Some("spam floods".into()),
            },
            "n1",
        ));
        assert_eq!(
            Request::new(Command::NetblockAdd {
                network: "evil.example".parse().unwrap(),
                reason: None,
            })
            .serialize()
            .unwrap(),
            "NETBLOCK ADD evil.example"
        );
        round_trip(&Request::new(Command::NetblockRemove {
            network: "evil.example".parse().unwrap(),
        }));
        round_trip(&Request::new(Command::NetblockList));
        assert!(Request::parse("NETBLOCK FROB x.example").is_err());
    }

    #[test]
    fn media_block_round_trips() {
        round_trip(&Request::with_label(
            Command::MediaBlock {
                hash: "b3hashhex".to_string(),
                reason: Some("csam".to_string()),
            },
            "m1",
        ));
        assert_eq!(
            Request::new(Command::MediaBlock {
                hash: "b3hashhex".to_string(),
                reason: None,
            })
            .serialize()
            .unwrap(),
            "MEDIA BLOCK b3hashhex"
        );
        round_trip(&Request::new(Command::MediaUnblock {
            hash: "b3hashhex".to_string(),
        }));
        round_trip(&Request::new(Command::MediaBlocks));
        assert!(Request::parse("MEDIA FROB x").is_err());
    }

    #[test]
    fn report_forward_round_trips() {
        round_trip(&Request::with_label(
            Command::ReportForward {
                report_id: "rep-42".into(),
                msgid: MSGID.parse().unwrap(),
                category: "harassment".into(),
                note: Some("forwarded from hda.example".into()),
            },
            "f1",
        ));
        round_trip(&Request::new(Command::ReportForward {
            report_id: "rep-9".into(),
            msgid: MSGID.parse().unwrap(),
            category: "csam".into(),
            note: None,
        }));
        // Unknown category rejected both ways.
        assert!(Request::parse(&format!("REPORT-FORWARD rep-1 {MSGID} slander")).is_err());
    }

    #[test]
    fn moderation_verbs_round_trip() {
        round_trip(&Request::with_label(
            Command::Mute {
                scope: "#general".into(),
                account: "bob".parse().unwrap(),
                reason: Some("spamming".into()),
            },
            "m1",
        ));
        assert_eq!(
            Request::new(Command::Mute {
                scope: "ns:gaming".into(),
                account: "bob".parse().unwrap(),
                reason: None,
            })
            .serialize()
            .unwrap(),
            "MUTE ns:gaming bob"
        );
        round_trip(&Request::new(Command::Unmute {
            scope: "*".into(),
            account: "bob".parse().unwrap(),
        }));
        round_trip(&Request::new(Command::Ban {
            scope: "*".into(),
            account: "eve".parse().unwrap(),
            reason: Some("raid".into()),
        }));
        round_trip(&Request::new(Command::Unban {
            scope: "#general".into(),
            account: "eve".parse().unwrap(),
        }));
        round_trip(&Request::new(Command::Kick {
            channel: "#general".parse().unwrap(),
            account: "eve".parse().unwrap(),
            reason: None,
        }));
        round_trip(&Request::new(Command::ModList {
            scope: "ns:games".into(),
        }));
    }

    #[test]
    fn voice_verbs_round_trip() {
        round_trip(&Request::with_label(
            Command::VoiceJoin {
                channel: "#gaming/lounge".parse().unwrap(),
            },
            "v1",
        ));
        assert_eq!(
            Request::new(Command::VoiceLeave {
                channel: "#general".parse().unwrap(),
            })
            .serialize()
            .unwrap(),
            "VOICE LEAVE #general"
        );

        // A real SDP carries CR/LF; it must survive the wire (escaped as
        // `\r`/`\n` in the trailing) and round-trip byte-for-byte.
        let sdp = "v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\n";
        let desc = Request::with_label(
            Command::VoiceDesc {
                channel: "#general".parse().unwrap(),
                sdp: sdp.to_string(),
            },
            "v2",
        );
        let wire = desc.serialize().unwrap();
        assert!(!wire.contains('\n'), "SDP newlines must be escaped: {wire}");
        round_trip(&desc);

        round_trip(&Request::new(Command::VoiceCand {
            channel: "#general".parse().unwrap(),
            candidate: "candidate:1 1 UDP 2130706431 192.0.2.1 54321 typ host".to_string(),
        }));

        // §16 federated voice: the bridge-only VOICE REQUEST.
        let req = Request::with_label(
            Command::VoiceRequest {
                scope: "ns:gaming".into(),
                channel: "#gaming/lounge".parse().unwrap(),
            },
            "vr",
        );
        assert_eq!(
            req.serialize().unwrap(),
            "@label=vr VOICE REQUEST ns:gaming #gaming/lounge"
        );
        round_trip(&req);

        // Missing channel / SDP are hard errors; unknown subcommand too.
        assert!(Request::parse("VOICE JOIN").is_err());
        assert!(Request::parse("VOICE DESC #general").is_err());
        assert!(Request::parse("VOICE REQUEST ns:gaming").is_err()); // channel required
        assert!(Request::parse("VOICE FROB #general").is_err());
    }

    #[test]
    fn profile_verbs_round_trip() {
        // Full set: display (with a space, escaped in the tag) + avatar.
        let set = Request::with_label(
            Command::ProfileSet {
                display: Some("Ada L.".into()),
                avatar: Some("b3-abc".into()),
            },
            "p1",
        );
        let wire = set.serialize().unwrap();
        assert!(wire.contains("display=Ada\\sL."), "{wire}");
        assert!(wire.contains("avatar=b3-abc"), "{wire}");
        round_trip(&set);

        // Partial update: avatar only (display left unchanged → absent tag).
        round_trip(&Request::new(Command::ProfileSet {
            display: None,
            avatar: Some("b3-xyz".into()),
        }));
        // Clear display: a present-but-empty tag distinguishes clear from absent.
        round_trip(&Request::new(Command::ProfileSet {
            display: Some(String::new()),
            avatar: None,
        }));

        round_trip(&Request::with_label(
            Command::ProfilesQuery {
                accounts: vec!["ada".into(), "bob".into()],
            },
            "q1",
        ));
        assert!(Request::parse("PROFILES").is_err()); // needs ≥1 account
        assert!(Request::parse("PROFILE FROB").is_err()); // bad subcommand
    }

    #[test]
    fn verify_verbs_round_trip() {
        let email = Request::with_label(
            Command::VerifyEmail {
                address: "ada@example.com".into(),
            },
            "e1",
        );
        assert_eq!(
            email.serialize().unwrap(),
            "@label=e1 VERIFY EMAIL ada@example.com"
        );
        round_trip(&email);

        round_trip(&Request::new(Command::VerifyBirthday {
            date: "2000-05-15".into(),
        }));
        round_trip(&Request::new(Command::VerifyConfirm {
            kind: "email".into(),
            code: "482913".into(),
        }));
        round_trip(&Request::new(Command::VerifyList));

        assert!(Request::parse("VERIFY EMAIL").is_err()); // address required
        assert!(Request::parse("VERIFY CONFIRM email").is_err()); // code required
        assert!(Request::parse("VERIFY FROB").is_err()); // bad subcommand
    }

    #[test]
    fn unknown_verb_is_not_an_error() {
        assert_eq!(
            parse("FROBNICATE a b :c"),
            Command::Unknown {
                verb: "FROBNICATE".into()
            }
        );
        // ...but has no wire form on the way out.
        let request = Request::new(Command::Unknown { verb: "X".into() });
        assert_eq!(
            request.serialize(),
            Err(SerializeError::Unrepresentable("unknown command"))
        );
    }

    #[test]
    fn label_limits() {
        let request = Request::parse("@label=abc123 PING").unwrap();
        assert_eq!(request.label.as_deref(), Some("abc123"));

        let long = format!("@label={} PING", "x".repeat(MAX_LABEL_BYTES + 1));
        assert_eq!(Request::parse(&long), Err(ParseError::LabelTooLong));

        let request = Request::with_label(
            Command::Ping { token: None },
            "y".repeat(MAX_LABEL_BYTES + 1),
        );
        assert_eq!(request.serialize(), Err(SerializeError::LabelTooLong));
    }
}
