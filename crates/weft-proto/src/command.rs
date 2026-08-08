//! Client → server commands: the M0 session + relay verb set (§6.1, §6.3,
//! §6.4). Unknown verbs decode to [`Command::Unknown`] — never an error (§4).

use crate::error::{ParseError, SerializeError};
use crate::foreign::{ForeignUri, Scheme};
use crate::id::MsgId;
use crate::line::{label_from_tags, write_label, Args, Line, Tags};
use crate::name::{Account, ChannelName, GroupId, NetworkName, Target, UserRef};
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

/// The caller network's **relay leg** carried on a **federated** `CALL`
/// (cross-network calls, §16 M-lk-3b cascade): its LiveKit `room`, a relay
/// `token` authorizing the callee network's relay to join that room, and the
/// `endpoint` URL. The callee's network bridges its own room to this leg so
/// neither client touches the other network's LiveKit. Never present on a
/// client-originated `CALL`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallMediaGrant {
    /// The caller network's LiveKit room id (`call:<ulid>`) the relay joins.
    pub room: String,
    /// A relay token for that room (identity `relay@<callee-network>`).
    pub token: String,
    /// The caller network's LiveKit server URL the relay connects to.
    pub endpoint: Option<String>,
}

/// The `detail` selector of an `NS INFO` query (§6.2). New moderator views are
/// added here as subcommands; unknown selectors are a typed parse error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NsInfoKind {
    /// `MEMBERS` — the namespace roster with per-member join time + roles.
    Members,
}

impl NsInfoKind {
    /// The uppercase wire token (strict-out).
    pub fn as_wire(self) -> &'static str {
        match self {
            NsInfoKind::Members => "MEMBERS",
        }
    }
}

/// M0 verb set. Extra params or an unexpected trailing are ignored
/// (lenient-in); missing or malformed required parts are typed errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `HELLO <version>` (§3.6).
    Hello { version: String },
    /// `REGISTER <account> [<email>] :<password>` (§6.1). The optional middle
    /// param is a contact email (verify-later, §10.5); a network with
    /// `require_email` on refuses REGISTER without it (the WEFT-IRC gateway,
    /// which auto-registers, is exempt).
    Register {
        account: Account,
        email: Option<String>,
        password: String,
    },
    /// `AUTH PASSWORD <identifier> :<password>`. The identifier is either the
    /// account name or a registered email (§6.1) — the server resolves it, so a
    /// name can change later without breaking sign-in. Kept a free string (not an
    /// `Account`) precisely because an email is not a valid account name.
    AuthPassword {
        identifier: String,
        password: String,
    },
    /// `AUTH KEY <account> <b64-ed25519-pubkey>` — starts challenge-response.
    AuthKey { account: Account, pubkey: String },
    /// `AUTH PROOF <b64-sig(nonce ‖ network-name)>` (§6.1: anti cross-network replay).
    AuthProof { signature: String },
    /// `AUTH ENROLL <b64-pubkey>` — add a device while authed.
    AuthEnroll { pubkey: String },
    /// `RESET REQUEST <email>` (§6.1) — ask for a password-reset code, mailed to
    /// `email` if it belongs to an account. Anti-enumeration: the response is
    /// uniform whether or not the email is known. Valid only while UNAUTHED.
    ResetRequest { email: String },
    /// `RESET CONFIRM <email> <code> :<new-password>` (§6.1) — set a new password
    /// with the one-time code from `RESET REQUEST`. Valid only while UNAUTHED.
    ResetConfirm {
        email: String,
        code: String,
        password: String,
    },
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
    /// `DELIVERED <msgid>` (framework §7a) — a provider confirming it put one of
    /// our messages into the foreign system. Provider sessions only.
    ///
    /// weftd's own echo acks *local* storage, which is all it can honestly
    /// promise; nothing said whether the message reached the realm. Without this,
    /// a post made in the window before weftd noticed the bridge was gone was
    /// stored, echoed, and silently never delivered.
    Delivered { msgid: MsgId },
    /// `UNDELIVERED <msgid> [:reason]` — the provider could not deliver it, and
    /// will not retry. The author is told rather than left believing it landed.
    Undelivered {
        msgid: MsgId,
        reason: Option<String>,
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
    /// `THREADS <#chan>` — list the channel's threads (§9.4 amendment),
    /// membership-gated. → a `BATCH` of `THREAD` events.
    Threads { channel: ChannelName },
    /// `THREAD NAME <#chan> <root> [:name]` — set (or clear, if the name is
    /// omitted) a thread's display name (§9.4 amendment). Requires the same
    /// authority as posting in the channel. → broadcasts `THREAD-NAMED`.
    ThreadName {
        channel: ChannelName,
        root: MsgId,
        name: Option<String>,
    },
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
    /// `SYNC [since=<cursor>] [preview=<n>]` (§6.9) — one-shot client state
    /// sync. No `since=` → fresh login (inline skeleton + a `SYNC BODY` token);
    /// with `since=` → a delta of everything changed since the **opaque** cursor
    /// (clients echo it verbatim, never parse it). `preview` caps per-channel
    /// message previews; `preview=0` is skeleton-only.
    Sync {
        since: Option<String>,
        preview: Option<u32>,
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
        /// Discord-style "display separately in the member list" (§6.5).
        hoist: bool,
        /// Whether members may `@`-mention this role to notify its holders (§9.3).
        pingable: bool,
        /// Sort position in the role list / member-list grouping (§6.5).
        position: i32,
        name: String,
    },
    /// `ROLE UPDATE <scope> <role-id> <color> <caps> [meta] :<name>` (§6.5, v0.13)
    /// — edit an existing role by its ULID id. Subsumes the old `ROLE RENAME`
    /// (the label is just the trailing here) and the "re-create to edit" path,
    /// now that roles have a stable id independent of the display name.
    RoleUpdate {
        scope: String,
        role: crate::RoleId,
        color: String,
        caps: String,
        hoist: bool,
        pingable: bool,
        position: i32,
        name: String,
    },
    /// `ROLE REORDER <scope> :<id1,id2,…>` — set every role's position from its
    /// index in the list (§6.5). Order is a list of role **ids** (v0.13).
    RolesReorder {
        scope: String,
        order: Vec<crate::RoleId>,
    },
    /// `ROLE DELETE <scope> <role-id>` — remove a role definition (§6.5).
    RoleDelete { scope: String, role: crate::RoleId },
    /// `ROLE ASSIGN <scope> <account> <role-id>` — grant the role's token bundle
    /// to an account and record explicit membership (§6.5).
    RoleAssign {
        scope: String,
        /// Local account name **or** foreign `account@network` (§10.4) — a role
        /// can be worn by a federated user, so this is a subject string.
        account: String,
        role: crate::RoleId,
    },
    /// `ROLE UNASSIGN <scope> <account> <role-id>` — drop membership + revoke the
    /// role's caps (§6.5).
    RoleUnassign {
        scope: String,
        account: String,
        role: crate::RoleId,
    },
    /// `ROLES <scope>` — list the role definitions at a scope (§6.5) → a BATCH
    /// of `ROLE` events.
    RolesList { scope: String },
    /// `ROLES-OF <scope> <account>` — the roles an account is assigned at a
    /// scope (§6.5) → a `ROLE-MEMBER` event.
    RolesOf { scope: String, account: String },
    /// `GRANTS <scope>` — list the per-subject capability grants at a scope
    /// (§6.5), so the channel-permission editor can surface individual-member
    /// overrides → a BATCH of `GRANT-INFO` events. Cap-gated to scope admins.
    GrantsAt { scope: String },
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
    /// `INVITE REVOKE-ALL <scope>` — closes every invite belonging to the
    /// scope's namespace in one shot (bulk revoke, §6.5).
    InviteRevokeAll { scope: String },
    /// `INVITE REDEEM <b64>` — verifies chain + counter, mints a member
    /// token bound to the redeemer (§6.5).
    InviteRedeem { token: String },
    /// `INVITE LIST <scope>` — the live invites at `scope` (cap `invite`);
    /// answered as a `BATCH` of `INVITE-INFO` events.
    InviteList { scope: String },
    /// `EMOJI ADD <ns> <name> <media>` — add/replace a namespace custom emoji
    /// (§9.4); cap `ns-admin`. `media` is a `weft-media://…` reference.
    EmojiAdd {
        namespace: crate::NamespaceId,
        name: String,
        media: String,
    },
    /// `EMOJI REMOVE <ns-id> <name>` — remove a namespace emoji (§9.4).
    EmojiRemove {
        namespace: crate::NamespaceId,
        name: String,
    },
    /// `EMOJI LIST <ns-id>` — a namespace's emoji → an `EMOJI` batch (§9.4).
    EmojiList { namespace: crate::NamespaceId },
    /// `NS CREATE <vanity> [tier]` with `@root=<b64-pubkey>` (§6.2, v0.13). The
    /// client generates the namespace root key and submits its pubkey + a desired
    /// **vanity** name; the server mints the namespace ULID id and returns it in
    /// the `NS-META` reply.
    NsCreate {
        vanity: crate::VanityName,
        visibility: Visibility,
        root_key: String,
    },
    /// `NS META <ns-id> <title|description|icon|vanity> :<value>` (§6.2). The
    /// `vanity` key renames the namespace's mutable label (refused if locked).
    NsMeta {
        ns: crate::NamespaceId,
        key: String,
        value: String,
    },
    /// `NS VISIBILITY <ns-id> <tier>` (§6.2).
    NsVisibility {
        ns: crate::NamespaceId,
        visibility: Visibility,
    },
    /// `NS DELEGATE <ns-id> <account|pubkey> <cap>[,...]` — sugar for GRANT
    /// at `ns:<id>` scope (§6.2).
    NsDelegate {
        ns: crate::NamespaceId,
        subject: String,
        caps: String,
    },
    /// `NS DELETE <ns-id> <ns-id>` — confirmed by repetition of the id (§6.2).
    NsDelete {
        ns: crate::NamespaceId,
        confirm: crate::NamespaceId,
    },
    /// `NS JOIN <ns-id|vanity>` — become a member of the namespace (§6.2); one
    /// `(account, ns)` row, channel access derived. View-gated channels the
    /// caller cannot see stay hidden. Accepts the id or the vanity name so an
    /// *unlisted* namespace stays joinable by exact name (§2.2); the server
    /// resolves either form to the id.
    NsJoin { ns: crate::NamespaceRef },
    /// `NS JOIN <scheme>://<realm>/<space>` — foreign-bridge framework
    /// (`docs/architecture/foreign-bridge-framework.md` §3.3): the same `NS JOIN`
    /// verb with a foreign-realm URI target, so weftd routes to the scheme's
    /// adapter to provision the space on first contact (afterward it is an ordinary
    /// local namespace). Foreignness is a property of the *target*, not a new verb
    /// — the parser routes here when the target contains `://`.
    NsJoinForeign { uri: crate::ForeignUri },
    /// `NS LEAVE <ns-id>` (§6.2) — drop namespace membership + all hide
    /// overrides + ns-scoped role assignments. Also reachable as the
    /// `PART ns:<id>` alias (lenient-in; strict-out is always `NS LEAVE`).
    NsLeave { ns: crate::NamespaceId },
    /// `NS TRANSFER <ns-id> <account>` with `@sig=<b64>` — rung-1 succession,
    /// signed by the current root (§2.4).
    NsTransfer {
        ns: crate::NamespaceId,
        new_owner: Account,
        signature: String,
    },
    /// `NS RECOVERY SET <ns-id> <m> <key1,key2,...>` — designate the M-of-N
    /// recovery quorum (§2.4). Root only.
    NsRecoverySet {
        ns: crate::NamespaceId,
        m: u32,
        keys: String,
    },
    /// `NS RECOVER <ns-id> <b64-rotation-record>` — submit a quorum-signed
    /// (rung 2) or operator-signed (rung 3) rotation; starts the delay
    /// window (§2.4).
    NsRecover {
        ns: crate::NamespaceId,
        rotation: String,
    },
    /// `NS RECOVERY CANCEL <ns-id>` with `@sig=<b64>` — the current root
    /// vetoes a pending recovery (§2.4).
    NsRecoveryCancel {
        ns: crate::NamespaceId,
        signature: String,
    },
    /// `NS INFO <detail> <ns-id>` — moderator-only fetch of server-relevant
    /// namespace details (§6.2). `detail` selects the query; the response is a
    /// `BATCH`. Cap-gated to holders of a moderation capability at
    /// `ns:<id>` (owner / ns-admin / ban / kick / mute / reports).
    NsInfo {
        ns: crate::NamespaceId,
        detail: NsInfoKind,
    },
    /// `DISCOVER [cursor]` — public namespace directory (§6.2).
    Discover { cursor: Option<String> },
    /// `CHANNELS <ns-id>` — the ordered channel layout of a namespace (spec
    /// extension: Discord-style categories + order).
    Channels { namespace: crate::NamespaceId },
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
    /// `AUTH ADAPTER <pubkey>` — foreign-bridge framework
    /// (`docs/architecture/foreign-bridge-framework.md` §3): a pinned adapter
    /// daemon proves control of its `[[foreign_bridge]]` key to enter
    /// `State::ForeignBridge`, reusing the §6.1 `CHALLENGE`/`AUTH PROOF` flow. The
    /// scheme(s) it may speak for are checked later at `REALM REGISTER`/`ASSERT`.
    AuthAdapter { pubkey: String },
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
    /// `BRIDGE REQUEST <ns>` with an optional `@invite=<token>` (§11.10) — ask
    /// the peer to offer a manifest for one of *its* namespaces. Bridge-session-
    /// only; the peer answers with `BRIDGE PROPOSE` iff the namespace is
    /// auto-federation-reachable — `public`, or (with a valid `invite`) an
    /// `unlisted`/`private` namespace that has `federation` open.
    BridgeRequest {
        /// The peer's **vanity** name for one of its namespaces — the peer
        /// resolves it to its own ns id and offers a manifest pinned to that id
        /// (§11.10). Cross-network, so the requester knows only the vanity.
        ns: crate::VanityName,
        invite: Option<String>,
    },
    /// `FEDERATE <network>/<vanity>` with an optional `@invite=<token>`
    /// (§11.10) — a local user asks their home network to auto-establish a
    /// bridge to a foreign namespace on demand, named by the peer's **vanity**
    /// (resolved + pinned to the peer's ns id during the handshake). The `invite`
    /// (a foreign-ns invite the user holds) unlocks non-public namespaces.
    Federate {
        network: NetworkName,
        namespace: crate::VanityName,
        invite: Option<String>,
    },
    /// `REALM REGISTER <scheme>` — foreign-bridge framework
    /// (`docs/architecture/foreign-bridge-framework.md` §3.3): a pinned adapter's
    /// **control link** declares a scheme it handles. Named `REALM REGISTER` (not
    /// the design's bare `REGISTER`) to avoid colliding with account registration.
    /// Honored only on a `State::ForeignBridge` session.
    RealmRegister { scheme: Scheme },
    /// `REALM ASSERT <scheme>://<realm>` — the connect-time binding of a **data
    /// connection** to a single realm (framework §3.1). The URI is realm-only
    /// (no path); a path makes it a parse error. Honored only on a
    /// `State::ForeignBridge` session; a NETBLOCKed realm is refused here.
    RealmAssert { realm: ForeignUri },
    /// `REALM WITHDRAW` — graceful teardown of the connection's bound realm
    /// (framework §3.1): weftd withdraws that realm's foreign namespaces. Distinct
    /// from an operator `NETBLOCK REALM` (a block, not a disconnect).
    RealmWithdraw,
    /// `PROVISION-OK <job>` — the provider finished provisioning the space for
    /// `job`: it has already `NS-ASSERT`ed the namespace (same session, ordered),
    /// so weftd resolves the pending URI by origin and completes the parked
    /// `NS JOIN` (framework §3.3). No asserted namespace ⇒ the join fails
    /// `NO-SUCH-TARGET` (a provider bug, logged).
    ProvisionOk { job: String },
    /// `PROVISION-ERR <job>` — control-link: provisioning failed (absent / private
    /// / encrypted / unjoinable). weftd answers the parked join `NO-SUCH-TARGET`,
    /// uniform in code + timing with a nonexistent local namespace (invariant 1).
    ProvisionErr { job: String },
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
    /// `NICK <scope> <account> :<nick>` (§10.3) — set a per-namespace display
    /// name (server nickname). Empty trailing clears it. Setting your OWN
    /// requires the `nick` cap; setting another member's requires `manage-nicks`.
    Nick {
        scope: String,
        account: Account,
        nick: String,
    },
    /// `NICKS <scope>` — list a namespace's server nicknames. Answered as a
    /// `BATCH` of `NICK` events (each a current nickname).
    Nicks { scope: String },
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
        /// `@about=` free-text bio; present sets (empty clears), absent leaves as-is.
        about: Option<String>,
        /// `@status=` free-text custom status (§10.3); present sets (empty
        /// clears), absent leaves as-is. Same partial-update rule as the others.
        status: Option<String>,
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
    /// `FRIEND ADD <user@net>` (social layer) — send a friend request, or, if
    /// the peer already has a request out to us, accept it. Federation-able:
    /// the peer is a full `UserRef` and may be on another network.
    FriendAdd { user: UserRef },
    /// `FRIEND ACCEPT <user@net>` — accept a pending incoming request.
    FriendAccept { user: UserRef },
    /// `FRIEND REMOVE <user@net>` — unfriend, cancel an outgoing request, or
    /// decline an incoming one (one verb removes the edge whatever its state).
    FriendRemove { user: UserRef },
    /// `FRIENDS` — list the caller's friends + pending requests; the server
    /// answers a `FRIEND` event per relationship (a `BATCH`).
    Friends,
    /// `GROUP CREATE <user@net> [user@net...]` (social layer) — open a group DM
    /// with the caller + the listed members. The server mints a `GroupId` and
    /// replies with a `GROUP` event. Members are full `UserRef`s (federation-able).
    GroupCreate { members: Vec<UserRef> },
    /// `GROUP ADD <&group> <user@net>` — add a member to a group DM.
    GroupAdd { group: GroupId, user: UserRef },
    /// `GROUP REMOVE <&group> <user@net>` — remove a member from a group DM.
    GroupRemove { group: GroupId, user: UserRef },
    /// `GROUP LEAVE <&group>` — leave a group DM.
    GroupLeave { group: GroupId },
    /// `GROUP NAME <&group> [:name]` — set, or (empty) clear, a group's name.
    GroupName {
        group: GroupId,
        name: Option<String>,
    },
    /// `GROUPS` — list the caller's group DMs; one `GROUP` event each (a `BATCH`).
    Groups,
    /// `GROUP CALL <&group>` (social layer) — start or join the group's voice
    /// call. Members are notified (`GROUP-CALL … active`); the caller gets a
    /// `CALL-MEDIA` credential for the shared room.
    ///
    /// `media` is set **only on the federated (tunnelled) ring**: the call's host
    /// network carries its **relay leg** here so a foreign member's network can
    /// bridge its own room into the host's (the §16 M-lk-3b group-call relay
    /// star). A client's `GROUP CALL` has `None`.
    GroupCall {
        group: GroupId,
        media: Option<CallMediaGrant>,
    },
    /// `GROUP HANGUP <&group>` — leave the group's voice call.
    GroupCallLeave { group: GroupId },
    /// `@reply=yes GROUP ROSTER <&group> <user@net> <active|ended>` — a
    /// **federation-internal** cross-network group-call roster update: one member
    /// on the sending network joined (`active`) or left (`ended`) the call; the
    /// receiving network re-emits it as a `GROUP-CALL` event to its local members
    /// so every network's roster is complete. `reply=yes` asks the receiver to
    /// send back its own current participants (the snapshot for a fresh joiner);
    /// a reply carries `reply=no` to avoid a loop. Never sent by a client.
    GroupCallRoster {
        group: GroupId,
        user: UserRef,
        active: bool,
        reply: bool,
    },
    /// `CALL <user@net>` (social layer) — place a 1:1 friend call; the callee's
    /// sessions ring (`CALL-RING`). On accept both join an ad-hoc voice room.
    ///
    /// `media` is set **only on the federated (tunnelled) path**, never by a
    /// client: for a cross-network call the caller's network pre-mints the callee
    /// a token for the shared LiveKit room (which one network hosts) and carries
    /// it here, so the callee's server can proxy the credential to its client
    /// (`@room=`/`@token=`/`@endpoint=` tags). A bare client `CALL` has `None`.
    Call {
        user: UserRef,
        media: Option<CallMediaGrant>,
    },
    /// `CALL ACCEPT <user@net>` — accept an incoming call from `user`.
    CallAccept { user: UserRef },
    /// `CALL DECLINE <user@net>` — decline an incoming call.
    CallDecline { user: UserRef },
    /// `CALL END <user@net>` — hang up / cancel a call with `user`.
    CallEnd { user: UserRef },
    /// Plugin system (`docs/architecture/plugin-spec.md` §12.1). `PLUGINS` — ask
    /// for the action catalog.
    Plugins,
    /// `PLUGIN INVOKE <plugin> <action> [<ctx-ref>]` with `@params=<b64cbor>`
    /// (§12.1). `params` is the input-schema values, carried opaque here (a
    /// base64-CBOR blob the plugin host decodes via [`crate::plugin_from_b64`]).
    PluginInvoke {
        plugin: String,
        action: String,
        ctx_ref: Option<String>,
        params: Option<String>,
    },
    /// `PLUGIN SUBMIT <view-id>` with `@values=<b64cbor>` — submit a form step.
    PluginSubmit {
        view_id: String,
        values: Option<String>,
    },
    /// `PLUGIN ACTION <view-id> <button-id>` (optional `@values=`) — a control click.
    PluginAction {
        view_id: String,
        button: String,
        values: Option<String>,
    },
    /// `PLUGIN SUBSCRIBE <view-id>` — panel/widget liveness (§12.1).
    PluginSubscribe { view_id: String },
    /// `PLUGIN UNSUBSCRIBE <view-id>`.
    PluginUnsubscribe { view_id: String },
    /// `PLUGIN CLOSE <view-id>` — user dismissed a view.
    PluginClose { view_id: String },
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

/// Parse a role ULID id from a wire token (§6.5, v0.13). Typed error on garbage.
fn role_id(raw: &str) -> Result<crate::RoleId, ParseError> {
    raw.parse().map_err(|_| ParseError::BadParam {
        verb: "ROLE",
        what: "role-id",
        value: raw.to_string(),
    })
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
                // account is the first middle param; an optional second middle
                // param is the contact email (`REGISTER <acct> [<email>] :<pw>`).
                let account = args.req("account")?.parse()?;
                let email = args.opt().map(str::to_string);
                Ok(Command::Register {
                    account,
                    email,
                    password: args.trailing_req("password")?.to_string(),
                })
            }
            "RESET" => {
                let mut args = Args::new(line, "RESET");
                let sub = args.req("subcommand")?.to_ascii_uppercase();
                match sub.as_str() {
                    "REQUEST" => Ok(Command::ResetRequest {
                        email: args.req("email")?.to_string(),
                    }),
                    "CONFIRM" => Ok(Command::ResetConfirm {
                        email: args.req("email")?.to_string(),
                        code: args.req("code")?.to_string(),
                        password: args.trailing_req("password")?.to_string(),
                    }),
                    _ => Err(ParseError::BadParam {
                        verb: "RESET",
                        what: "subcommand",
                        value: sub,
                    }),
                }
            }
            "AUTH" => {
                let mut args = Args::new(line, "AUTH");
                let sub = args.req("subcommand")?.to_ascii_uppercase();
                match sub.as_str() {
                    "PASSWORD" => Ok(Command::AuthPassword {
                        identifier: args.req("identifier")?.to_string(),
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
                    "ADAPTER" => Ok(Command::AuthAdapter {
                        pubkey: args.req("pubkey")?.to_string(),
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
                let target = args.req("channel")?.to_string();
                // `PART ns:<name>` is an alias for `NS LEAVE <name>` (§6.2) —
                // convenient for the IRC gateway, which has no NS verbs.
                if let Some(ns) = target.strip_prefix("ns:") {
                    return Ok(Command::NsLeave { ns: ns.parse()? });
                }
                Ok(Command::Part {
                    channel: target.parse()?,
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
            "DELIVERED" => Ok(Command::Delivered {
                msgid: Args::new(line, "DELIVERED").req("msgid")?.parse()?,
            }),
            "UNDELIVERED" => Ok(Command::Undelivered {
                msgid: Args::new(line, "UNDELIVERED").req("msgid")?.parse()?,
                reason: line.trailing.clone().filter(|r| !r.is_empty()),
            }),
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
            "THREADS" => Ok(Command::Threads {
                channel: Args::new(line, "THREADS").req("channel")?.parse()?,
            }),
            "THREAD" => {
                let mut args = Args::new(line, "THREAD");
                let sub = args.req("subcommand")?.to_ascii_uppercase();
                match sub.as_str() {
                    "NAME" => Ok(Command::ThreadName {
                        channel: args.req("channel")?.parse()?,
                        root: args.req("root")?.parse()?,
                        // Omitted or empty trailing clears the name.
                        name: line.trailing.clone().filter(|n| !n.is_empty()),
                    }),
                    _ => Err(ParseError::BadParam {
                        verb: "THREAD",
                        what: "subcommand",
                        value: sub,
                    }),
                }
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
            "SYNC" => {
                let mut args = Args::new(line, "SYNC");
                // key=value params in any order; unknown keys ignored
                // (lenient-in). `since=` is an opaque cursor we never parse.
                let mut since = None;
                let mut preview = None;
                while let Some(param) = args.opt() {
                    let Some((key, value)) = param.split_once('=') else {
                        continue;
                    };
                    match key {
                        "since" => since = Some(value.to_string()),
                        "preview" => {
                            preview = Some(value.parse().map_err(|_| ParseError::BadParam {
                                verb: "SYNC",
                                what: "preview",
                                value: value.to_string(),
                            })?)
                        }
                        _ => {}
                    }
                }
                Ok(Command::Sync { since, preview })
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
                        // Optional metadata as key=value middle params (like
                        // INVITE MINT): `hoist=1 pingable=1 pos=<n>`. Absent ⇒ defaults.
                        let mut hoist = false;
                        let mut pingable = false;
                        let mut position = 0i32;
                        while let Some(param) = args.opt() {
                            if let Some(v) = param.strip_prefix("hoist=") {
                                hoist = v == "1"
                                    || v.eq_ignore_ascii_case("yes")
                                    || v.eq_ignore_ascii_case("true");
                            } else if let Some(v) = param.strip_prefix("pingable=") {
                                pingable = v == "1"
                                    || v.eq_ignore_ascii_case("yes")
                                    || v.eq_ignore_ascii_case("true");
                            } else if let Some(v) = param.strip_prefix("pos=") {
                                position = v.parse().unwrap_or(0);
                            }
                        }
                        let name = line.trailing.clone().ok_or(ParseError::MissingParam {
                            verb: "ROLE",
                            what: "name",
                        })?;
                        Ok(Command::RoleCreate {
                            scope,
                            color,
                            caps,
                            hoist,
                            pingable,
                            position,
                            name,
                        })
                    }
                    "UPDATE" => {
                        let scope = args.req("scope")?.to_string();
                        let role = role_id(args.req("role-id")?)?;
                        let color = args.req("color")?.to_string();
                        let caps = args.req("caps")?.to_string();
                        if !caps_ok(&caps) {
                            return Err(ParseError::BadParam {
                                verb: "ROLE",
                                what: "caps",
                                value: caps,
                            });
                        }
                        let mut hoist = false;
                        let mut pingable = false;
                        let mut position = 0i32;
                        while let Some(param) = args.opt() {
                            if let Some(v) = param.strip_prefix("hoist=") {
                                hoist = v == "1"
                                    || v.eq_ignore_ascii_case("yes")
                                    || v.eq_ignore_ascii_case("true");
                            } else if let Some(v) = param.strip_prefix("pingable=") {
                                pingable = v == "1"
                                    || v.eq_ignore_ascii_case("yes")
                                    || v.eq_ignore_ascii_case("true");
                            } else if let Some(v) = param.strip_prefix("pos=") {
                                position = v.parse().unwrap_or(0);
                            }
                        }
                        let name = line.trailing.clone().ok_or(ParseError::MissingParam {
                            verb: "ROLE",
                            what: "name",
                        })?;
                        Ok(Command::RoleUpdate {
                            scope,
                            role,
                            color,
                            caps,
                            hoist,
                            pingable,
                            position,
                            name,
                        })
                    }
                    "REORDER" => Ok(Command::RolesReorder {
                        scope: args.req("scope")?.to_string(),
                        order: line
                            .trailing
                            .clone()
                            .unwrap_or_default()
                            .split(',')
                            .filter_map(|s| s.parse().ok())
                            .collect(),
                    }),
                    "DELETE" => Ok(Command::RoleDelete {
                        scope: args.req("scope")?.to_string(),
                        role: role_id(args.req("role-id")?)?,
                    }),
                    "ASSIGN" => Ok(Command::RoleAssign {
                        scope: args.req("scope")?.to_string(),
                        account: args.req("account")?.to_string(),
                        role: role_id(args.req("role-id")?)?,
                    }),
                    "UNASSIGN" => Ok(Command::RoleUnassign {
                        scope: args.req("scope")?.to_string(),
                        account: args.req("account")?.to_string(),
                        role: role_id(args.req("role-id")?)?,
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
            "GRANTS" => {
                let mut args = Args::new(line, "GRANTS");
                Ok(Command::GrantsAt {
                    scope: args.req("scope")?.to_string(),
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
                    "REVOKE-ALL" => Ok(Command::InviteRevokeAll {
                        scope: args.req("scope")?.to_string(),
                    }),
                    "REDEEM" => Ok(Command::InviteRedeem {
                        token: args.req("token")?.to_string(),
                    }),
                    "LIST" => Ok(Command::InviteList {
                        scope: args.req("scope")?.to_string(),
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
                        let vanity = args.req("vanity")?.parse()?;
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
                            vanity,
                            visibility,
                            root_key,
                        })
                    }
                    "META" => Ok(Command::NsMeta {
                        ns: args.req("ns")?.parse()?,
                        key: args.req("key")?.to_string(),
                        value: args.trailing_req("value")?.to_string(),
                    }),
                    "VISIBILITY" => Ok(Command::NsVisibility {
                        ns: args.req("ns")?.parse()?,
                        visibility: args.req("tier")?.parse()?,
                    }),
                    "DELEGATE" => {
                        let ns = args.req("ns")?.parse()?;
                        let subject = args.req("subject")?.to_string();
                        let caps = args.req("caps")?.to_string();
                        if !caps_ok(&caps) {
                            return Err(ParseError::BadParam {
                                verb: "NS",
                                what: "caps",
                                value: caps,
                            });
                        }
                        Ok(Command::NsDelegate { ns, subject, caps })
                    }
                    "DELETE" => Ok(Command::NsDelete {
                        ns: args.req("ns")?.parse()?,
                        confirm: args.req("confirmation")?.parse()?,
                    }),
                    "JOIN" => {
                        // Foreignness is a target property: a `<scheme>://…` URI
                        // routes to the provisioning path, a bare ref is a local
                        // namespace join (§3.3).
                        let target = args.req("ns")?;

                        if target.contains("://") {
                            Ok(Command::NsJoinForeign {
                                uri: target.parse()?,
                            })
                        } else {
                            Ok(Command::NsJoin {
                                ns: target.parse()?,
                            })
                        }
                    }
                    "LEAVE" => Ok(Command::NsLeave {
                        ns: args.req("ns")?.parse()?,
                    }),
                    "TRANSFER" => {
                        let ns = args.req("ns")?.parse()?;
                        let new_owner = args.req("account")?.parse()?;
                        let signature = ns_sig_tag(line)?;
                        Ok(Command::NsTransfer {
                            ns,
                            new_owner,
                            signature,
                        })
                    }
                    "RECOVER" => Ok(Command::NsRecover {
                        ns: args.req("ns")?.parse()?,
                        rotation: args.req("rotation-record")?.to_string(),
                    }),
                    "INFO" => {
                        let detail = args.req("detail")?.to_ascii_uppercase();
                        let ns = args.req("ns")?.parse()?;
                        let detail = match detail.as_str() {
                            "MEMBERS" => NsInfoKind::Members,
                            _ => {
                                return Err(ParseError::BadParam {
                                    verb: "NS",
                                    what: "info detail",
                                    value: detail,
                                })
                            }
                        };
                        Ok(Command::NsInfo { ns, detail })
                    }
                    // Three-word: NS RECOVERY SET | NS RECOVERY CANCEL.
                    "RECOVERY" => {
                        let action = args.req("action")?.to_ascii_uppercase();
                        match action.as_str() {
                            "SET" => {
                                let ns = args.req("ns")?.parse()?;
                                let m = args.req("m")?;
                                let m = m.parse().map_err(|_| ParseError::BadParam {
                                    verb: "NS",
                                    what: "m",
                                    value: m.to_string(),
                                })?;
                                let keys = args.req("keys")?.to_string();
                                Ok(Command::NsRecoverySet { ns, m, keys })
                            }
                            "CANCEL" => {
                                let ns = args.req("ns")?.parse()?;
                                Ok(Command::NsRecoveryCancel {
                                    ns,
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
                    invite: line.tags.get("invite").filter(|v| !v.is_empty()).cloned(),
                })
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
                        invite: line.tags.get("invite").filter(|v| !v.is_empty()).cloned(),
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
            "REALM" => {
                let mut args = Args::new(line, "REALM");
                let sub = args.req("subcommand")?.to_ascii_uppercase();
                match sub.as_str() {
                    "REGISTER" => Ok(Command::RealmRegister {
                        scheme: args.req("scheme")?.parse()?,
                    }),
                    "ASSERT" => {
                        let realm: ForeignUri = args.req("realm")?.parse()?;

                        // The binding names a realm, never a space/channel within it.
                        if realm.path().is_empty() {
                            Ok(Command::RealmAssert { realm })
                        } else {
                            Err(ParseError::BadParam {
                                verb: "REALM",
                                what: "realm",
                                value: realm.to_string(),
                            })
                        }
                    }
                    "WITHDRAW" => Ok(Command::RealmWithdraw),
                    _ => Err(ParseError::BadParam {
                        verb: "REALM",
                        what: "subcommand",
                        value: sub,
                    }),
                }
            }
            "PROVISION-OK" => Ok(Command::ProvisionOk {
                job: Args::new(line, "PROVISION-OK").req("job")?.to_string(),
            }),
            "PROVISION-ERR" => Ok(Command::ProvisionErr {
                job: Args::new(line, "PROVISION-ERR").req("job")?.to_string(),
            }),
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
            "NICK" => {
                let mut args = Args::new(line, "NICK");
                Ok(Command::Nick {
                    scope: args.req("scope")?.to_string(),
                    account: args.req("account")?.parse()?,
                    // Present-but-empty trailing clears the nickname.
                    nick: args.trailing_opt().unwrap_or_default(),
                })
            }
            "NICKS" => {
                let mut args = Args::new(line, "NICKS");
                Ok(Command::Nicks {
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
                        about: line.tags.get("about").cloned(),
                        status: line.tags.get("status").cloned(),
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
            "FRIENDS" => Ok(Command::Friends),
            "FRIEND" => {
                let mut args = Args::new(line, "FRIEND");
                let sub = args.req("subcommand")?.to_ascii_uppercase();
                match sub.as_str() {
                    "ADD" => Ok(Command::FriendAdd {
                        user: args.req("user")?.parse()?,
                    }),
                    "ACCEPT" => Ok(Command::FriendAccept {
                        user: args.req("user")?.parse()?,
                    }),
                    "REMOVE" => Ok(Command::FriendRemove {
                        user: args.req("user")?.parse()?,
                    }),
                    _ => Err(ParseError::BadParam {
                        verb: "FRIEND",
                        what: "subcommand",
                        value: sub,
                    }),
                }
            }
            "CALL" => {
                let mut args = Args::new(line, "CALL");
                let first = args.req("subcommand-or-user")?.to_string();
                match first.to_ascii_uppercase().as_str() {
                    "ACCEPT" => Ok(Command::CallAccept {
                        user: args.req("user")?.parse()?,
                    }),
                    "DECLINE" => Ok(Command::CallDecline {
                        user: args.req("user")?.parse()?,
                    }),
                    "END" => Ok(Command::CallEnd {
                        user: args.req("user")?.parse()?,
                    }),
                    // Bare `CALL <user@net>` = place a call. A federated CALL also
                    // carries the callee's pre-minted LiveKit credential as tags.
                    _ => {
                        let media = match (line.tags.get("room"), line.tags.get("token")) {
                            (Some(room), Some(token)) if !room.is_empty() && !token.is_empty() => {
                                Some(CallMediaGrant {
                                    room: room.clone(),
                                    token: token.clone(),
                                    endpoint: line
                                        .tags
                                        .get("endpoint")
                                        .filter(|v| !v.is_empty())
                                        .cloned(),
                                })
                            }
                            _ => None,
                        };
                        Ok(Command::Call {
                            user: first.parse()?,
                            media,
                        })
                    }
                }
            }
            "GROUPS" => Ok(Command::Groups),
            "GROUP" => {
                let mut args = Args::new(line, "GROUP");
                let sub = args.req("subcommand")?.to_ascii_uppercase();
                match sub.as_str() {
                    "CREATE" => {
                        let mut members = Vec::new();
                        while let Some(m) = args.opt() {
                            members.push(m.parse()?);
                        }
                        if members.is_empty() {
                            return Err(ParseError::MissingParam {
                                verb: "GROUP",
                                what: "member",
                            });
                        }
                        Ok(Command::GroupCreate { members })
                    }
                    "ADD" => Ok(Command::GroupAdd {
                        group: args.req("group")?.parse()?,
                        user: args.req("user")?.parse()?,
                    }),
                    "REMOVE" => Ok(Command::GroupRemove {
                        group: args.req("group")?.parse()?,
                        user: args.req("user")?.parse()?,
                    }),
                    "LEAVE" => Ok(Command::GroupLeave {
                        group: args.req("group")?.parse()?,
                    }),
                    "NAME" => Ok(Command::GroupName {
                        group: args.req("group")?.parse()?,
                        name: line.trailing.clone().filter(|n| !n.is_empty()),
                    }),
                    "CALL" => {
                        let group = args.req("group")?.parse()?;
                        // A federated ring carries the host network's relay leg.
                        let media = match (line.tags.get("room"), line.tags.get("token")) {
                            (Some(room), Some(token)) if !room.is_empty() && !token.is_empty() => {
                                Some(CallMediaGrant {
                                    room: room.clone(),
                                    token: token.clone(),
                                    endpoint: line
                                        .tags
                                        .get("endpoint")
                                        .filter(|v| !v.is_empty())
                                        .cloned(),
                                })
                            }
                            _ => None,
                        };
                        Ok(Command::GroupCall { group, media })
                    }
                    "HANGUP" => Ok(Command::GroupCallLeave {
                        group: args.req("group")?.parse()?,
                    }),
                    "ROSTER" => Ok(Command::GroupCallRoster {
                        group: args.req("group")?.parse()?,
                        user: args.req("user")?.parse()?,
                        active: args.req("state")?.eq_ignore_ascii_case("active"),
                        reply: line.tags.get("reply").is_some_and(|v| v == "yes"),
                    }),
                    _ => Err(ParseError::BadParam {
                        verb: "GROUP",
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
            "PLUGINS" => Ok(Command::Plugins),
            "PLUGIN" => {
                let mut args = Args::new(line, "PLUGIN");
                let sub = args.req("subcommand")?.to_ascii_uppercase();
                // Structured payloads (`params`/`values`) ride opaque in b64-CBOR
                // tags; the plugin host decodes them (plugin-spec.md §12.1).
                let tag = |k: &str| line.tags.get(k).filter(|v| !v.is_empty()).cloned();
                match sub.as_str() {
                    "INVOKE" => Ok(Command::PluginInvoke {
                        plugin: args.req("plugin")?.to_string(),
                        action: args.req("action")?.to_string(),
                        ctx_ref: args.opt().map(str::to_string),
                        params: tag("params"),
                    }),
                    "SUBMIT" => Ok(Command::PluginSubmit {
                        view_id: args.req("view-id")?.to_string(),
                        values: tag("values"),
                    }),
                    "ACTION" => Ok(Command::PluginAction {
                        view_id: args.req("view-id")?.to_string(),
                        button: args.req("button-id")?.to_string(),
                        values: tag("values"),
                    }),
                    "SUBSCRIBE" => Ok(Command::PluginSubscribe {
                        view_id: args.req("view-id")?.to_string(),
                    }),
                    "UNSUBSCRIBE" => Ok(Command::PluginUnsubscribe {
                        view_id: args.req("view-id")?.to_string(),
                    }),
                    "CLOSE" => Ok(Command::PluginClose {
                        view_id: args.req("view-id")?.to_string(),
                    }),
                    _ => Err(ParseError::BadParam {
                        verb: "PLUGIN",
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
            Command::Register {
                account,
                email,
                password,
            } => {
                // Emit the email only when present, keeping the bare
                // `REGISTER <acct> :<pw>` form byte-for-byte round-trippable.
                let mut params = vec![account.to_string()];
                if let Some(email) = email {
                    params.push(email.clone());
                }
                ("REGISTER", params, Some(password.clone()))
            }
            Command::ResetRequest { email } => {
                ("RESET", vec!["REQUEST".to_string(), email.clone()], None)
            }
            Command::ResetConfirm {
                email,
                code,
                password,
            } => (
                "RESET",
                vec!["CONFIRM".to_string(), email.clone(), code.clone()],
                Some(password.clone()),
            ),
            Command::AuthPassword {
                identifier,
                password,
            } => (
                "AUTH",
                vec!["PASSWORD".to_string(), identifier.clone()],
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
            Command::Delivered { msgid } => ("DELIVERED", vec![msgid.to_string()], None),
            Command::Undelivered { msgid, reason } => {
                ("UNDELIVERED", vec![msgid.to_string()], reason.clone())
            }
            Command::Pin { msgid } => ("PIN", vec![msgid.to_string()], None),
            Command::Unpin { msgid } => ("UNPIN", vec![msgid.to_string()], None),
            Command::Pins { channel } => ("PINS", vec![channel.to_string()], None),
            Command::Search { channel, query } => {
                ("SEARCH", vec![channel.to_string()], Some(query.clone()))
            }
            Command::Threads { channel } => ("THREADS", vec![channel.to_string()], None),
            Command::ThreadName {
                channel,
                root,
                name,
            } => (
                "THREAD",
                vec!["NAME".to_string(), channel.to_string(), root.to_string()],
                // A present-but-empty name is unrepresentable — clearing is
                // the omitted-trailing form (lenient-in above filters empty).
                name.clone().filter(|n| !n.is_empty()),
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
            Command::Sync { since, preview } => {
                let mut params = Vec::new();
                if let Some(since) = since {
                    params.push(format!("since={since}"));
                }
                if let Some(preview) = preview {
                    params.push(format!("preview={preview}"));
                }
                ("SYNC", params, None)
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
                hoist,
                pingable,
                position,
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
                        format!("hoist={}", if *hoist { 1 } else { 0 }),
                        format!("pingable={}", if *pingable { 1 } else { 0 }),
                        format!("pos={position}"),
                    ],
                    Some(name.clone()),
                )
            }
            Command::RoleUpdate {
                scope,
                role,
                color,
                caps,
                hoist,
                pingable,
                position,
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
                        "UPDATE".to_string(),
                        scope.clone(),
                        role.to_string(),
                        color.clone(),
                        caps.clone(),
                        format!("hoist={}", if *hoist { 1 } else { 0 }),
                        format!("pingable={}", if *pingable { 1 } else { 0 }),
                        format!("pos={position}"),
                    ],
                    Some(name.clone()),
                )
            }
            Command::RolesReorder { scope, order } => (
                "ROLE",
                vec!["REORDER".to_string(), scope.clone()],
                Some(
                    order
                        .iter()
                        .map(|r| r.to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                ),
            ),
            Command::RoleDelete { scope, role } => (
                "ROLE",
                vec!["DELETE".to_string(), scope.clone(), role.to_string()],
                None,
            ),
            Command::RoleAssign {
                scope,
                account,
                role,
            } => (
                "ROLE",
                vec![
                    "ASSIGN".to_string(),
                    scope.clone(),
                    account.to_string(),
                    role.to_string(),
                ],
                None,
            ),
            Command::RoleUnassign {
                scope,
                account,
                role,
            } => (
                "ROLE",
                vec![
                    "UNASSIGN".to_string(),
                    scope.clone(),
                    account.to_string(),
                    role.to_string(),
                ],
                None,
            ),
            Command::RolesList { scope } => ("ROLES", vec![scope.clone()], None),
            Command::RolesOf { scope, account } => {
                ("ROLES-OF", vec![scope.clone(), account.to_string()], None)
            }
            Command::GrantsAt { scope } => ("GRANTS", vec![scope.clone()], None),
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
            Command::InviteRevokeAll { scope } => (
                "INVITE",
                vec!["REVOKE-ALL".to_string(), scope.clone()],
                None,
            ),
            Command::InviteRedeem { token } => {
                ("INVITE", vec!["REDEEM".to_string(), token.clone()], None)
            }
            Command::InviteList { scope } => {
                ("INVITE", vec!["LIST".to_string(), scope.clone()], None)
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
                vanity,
                visibility,
                root_key,
            } => {
                tags.insert("root".to_string(), root_key.clone());
                (
                    "NS",
                    vec![
                        "CREATE".to_string(),
                        vanity.to_string(),
                        visibility.to_string(),
                    ],
                    None,
                )
            }
            Command::NsMeta { ns, key, value } => (
                "NS",
                vec!["META".to_string(), ns.to_string(), key.clone()],
                Some(value.clone()),
            ),
            Command::NsVisibility { ns, visibility } => (
                "NS",
                vec![
                    "VISIBILITY".to_string(),
                    ns.to_string(),
                    visibility.to_string(),
                ],
                None,
            ),
            Command::NsDelegate { ns, subject, caps } => (
                "NS",
                vec![
                    "DELEGATE".to_string(),
                    ns.to_string(),
                    subject.clone(),
                    caps.clone(),
                ],
                None,
            ),
            Command::NsDelete { ns, confirm } => (
                "NS",
                vec!["DELETE".to_string(), ns.to_string(), confirm.to_string()],
                None,
            ),
            Command::NsJoin { ns } => ("NS", vec!["JOIN".to_string(), ns.to_string()], None),
            Command::NsJoinForeign { uri } => {
                ("NS", vec!["JOIN".to_string(), uri.to_string()], None)
            }
            Command::NsLeave { ns } => ("NS", vec!["LEAVE".to_string(), ns.to_string()], None),
            Command::NsTransfer {
                ns,
                new_owner,
                signature,
            } => {
                tags.insert("sig".to_string(), signature.clone());
                (
                    "NS",
                    vec![
                        "TRANSFER".to_string(),
                        ns.to_string(),
                        new_owner.to_string(),
                    ],
                    None,
                )
            }
            Command::NsRecoverySet { ns, m, keys } => (
                "NS",
                vec![
                    "RECOVERY".to_string(),
                    "SET".to_string(),
                    ns.to_string(),
                    m.to_string(),
                    keys.clone(),
                ],
                None,
            ),
            Command::NsRecover { ns, rotation } => (
                "NS",
                vec!["RECOVER".to_string(), ns.to_string(), rotation.clone()],
                None,
            ),
            Command::NsInfo { ns, detail } => (
                "NS",
                vec![
                    "INFO".to_string(),
                    detail.as_wire().to_string(),
                    ns.to_string(),
                ],
                None,
            ),
            Command::NsRecoveryCancel { ns, signature } => {
                tags.insert("sig".to_string(), signature.clone());
                (
                    "NS",
                    vec!["RECOVERY".to_string(), "CANCEL".to_string(), ns.to_string()],
                    None,
                )
            }
            Command::Discover { cursor } => ("DISCOVER", cursor.iter().cloned().collect(), None),
            Command::Federate {
                network,
                namespace,
                invite,
            } => {
                if let Some(invite) = invite {
                    tags.insert("invite".to_string(), invite.clone());
                }
                ("FEDERATE", vec![format!("{network}/{namespace}")], None)
            }
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
            Command::AuthAdapter { pubkey } => {
                ("AUTH", vec!["ADAPTER".to_string(), pubkey.clone()], None)
            }
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
            Command::BridgeRequest { ns, invite } => {
                if let Some(invite) = invite {
                    tags.insert("invite".to_string(), invite.clone());
                }
                ("BRIDGE", vec!["REQUEST".to_string(), ns.to_string()], None)
            }
            Command::RealmRegister { scheme } => (
                "REALM",
                vec!["REGISTER".to_string(), scheme.to_string()],
                None,
            ),
            Command::RealmAssert { realm } => {
                ("REALM", vec!["ASSERT".to_string(), realm.to_string()], None)
            }
            Command::RealmWithdraw => ("REALM", vec!["WITHDRAW".to_string()], None),
            Command::ProvisionOk { job } => ("PROVISION-OK", vec![job.clone()], None),
            Command::ProvisionErr { job } => ("PROVISION-ERR", vec![job.clone()], None),
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
            Command::Nick {
                scope,
                account,
                nick,
            } => (
                "NICK",
                vec![scope.clone(), account.to_string()],
                Some(nick.clone()),
            ),
            Command::Nicks { scope } => ("NICKS", vec![scope.clone()], None),
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
            Command::ProfileSet {
                display,
                avatar,
                about,
                status,
            } => {
                if let Some(display) = display {
                    tags.insert("display".to_string(), display.clone());
                }
                if let Some(avatar) = avatar {
                    tags.insert("avatar".to_string(), avatar.clone());
                }
                if let Some(about) = about {
                    tags.insert("about".to_string(), about.clone());
                }
                if let Some(status) = status {
                    tags.insert("status".to_string(), status.clone());
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
            Command::FriendAdd { user } => {
                ("FRIEND", vec!["ADD".to_string(), user.to_string()], None)
            }
            Command::FriendAccept { user } => {
                ("FRIEND", vec!["ACCEPT".to_string(), user.to_string()], None)
            }
            Command::FriendRemove { user } => {
                ("FRIEND", vec!["REMOVE".to_string(), user.to_string()], None)
            }
            Command::Friends => ("FRIENDS", vec![], None),
            Command::GroupCreate { members } => (
                "GROUP",
                std::iter::once("CREATE".to_string())
                    .chain(members.iter().map(|m| m.to_string()))
                    .collect(),
                None,
            ),
            Command::GroupAdd { group, user } => (
                "GROUP",
                vec!["ADD".to_string(), group.to_string(), user.to_string()],
                None,
            ),
            Command::GroupRemove { group, user } => (
                "GROUP",
                vec!["REMOVE".to_string(), group.to_string(), user.to_string()],
                None,
            ),
            Command::GroupLeave { group } => {
                ("GROUP", vec!["LEAVE".to_string(), group.to_string()], None)
            }
            Command::GroupName { group, name } => (
                "GROUP",
                vec!["NAME".to_string(), group.to_string()],
                name.clone().filter(|n| !n.is_empty()),
            ),
            Command::GroupCall { group, media } => {
                if let Some(m) = media {
                    tags.insert("room".to_string(), m.room.clone());
                    tags.insert("token".to_string(), m.token.clone());
                    if let Some(endpoint) = &m.endpoint {
                        tags.insert("endpoint".to_string(), endpoint.clone());
                    }
                }
                ("GROUP", vec!["CALL".to_string(), group.to_string()], None)
            }
            Command::GroupCallLeave { group } => {
                ("GROUP", vec!["HANGUP".to_string(), group.to_string()], None)
            }
            Command::GroupCallRoster {
                group,
                user,
                active,
                reply,
            } => {
                if *reply {
                    tags.insert("reply".to_string(), "yes".to_string());
                }
                (
                    "GROUP",
                    vec![
                        "ROSTER".to_string(),
                        group.to_string(),
                        user.to_string(),
                        if *active { "active" } else { "ended" }.to_string(),
                    ],
                    None,
                )
            }
            Command::Groups => ("GROUPS", vec![], None),
            Command::Call { user, media } => {
                if let Some(m) = media {
                    tags.insert("room".to_string(), m.room.clone());
                    tags.insert("token".to_string(), m.token.clone());
                    if let Some(endpoint) = &m.endpoint {
                        tags.insert("endpoint".to_string(), endpoint.clone());
                    }
                }
                ("CALL", vec![user.to_string()], None)
            }
            Command::CallAccept { user } => {
                ("CALL", vec!["ACCEPT".to_string(), user.to_string()], None)
            }
            Command::CallDecline { user } => {
                ("CALL", vec!["DECLINE".to_string(), user.to_string()], None)
            }
            Command::CallEnd { user } => ("CALL", vec!["END".to_string(), user.to_string()], None),
            Command::Plugins => ("PLUGINS", vec![], None),
            Command::PluginInvoke {
                plugin,
                action,
                ctx_ref,
                params,
            } => {
                if let Some(params) = params {
                    tags.insert("params".to_string(), params.clone());
                }

                let mut v = vec!["INVOKE".to_string(), plugin.clone(), action.clone()];
                if let Some(ctx_ref) = ctx_ref {
                    v.push(ctx_ref.clone());
                }

                ("PLUGIN", v, None)
            }
            Command::PluginSubmit { view_id, values } => {
                if let Some(values) = values {
                    tags.insert("values".to_string(), values.clone());
                }
                ("PLUGIN", vec!["SUBMIT".to_string(), view_id.clone()], None)
            }
            Command::PluginAction {
                view_id,
                button,
                values,
            } => {
                if let Some(values) = values {
                    tags.insert("values".to_string(), values.clone());
                }
                (
                    "PLUGIN",
                    vec!["ACTION".to_string(), view_id.clone(), button.clone()],
                    None,
                )
            }
            Command::PluginSubscribe { view_id } => (
                "PLUGIN",
                vec!["SUBSCRIBE".to_string(), view_id.clone()],
                None,
            ),
            Command::PluginUnsubscribe { view_id } => (
                "PLUGIN",
                vec!["UNSUBSCRIBE".to_string(), view_id.clone()],
                None,
            ),
            Command::PluginClose { view_id } => {
                ("PLUGIN", vec!["CLOSE".to_string(), view_id.clone()], None)
            }
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

    const MSGID: &str = "weft.example/01ARZ3NDEKTSV4RRFFQ69G5FAV";

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
            email: None,
            password: "correct horse battery".into(),
        });
        round_trip(&request);
        // No email → the bare form, byte-for-byte.
        assert_eq!(
            request.serialize().unwrap(),
            "REGISTER ada :correct horse battery"
        );
    }

    #[test]
    fn register_with_email_round_trips() {
        let request = Request::new(Command::Register {
            account: "ada".parse().unwrap(),
            email: Some("ada@example.com".into()),
            password: "correct horse battery".into(),
        });
        round_trip(&request);
        assert_eq!(
            request.serialize().unwrap(),
            "REGISTER ada ada@example.com :correct horse battery"
        );
        // The email is the optional second middle param.
        assert_eq!(
            parse("REGISTER ada ada@example.com :hunter2hunter2"),
            Command::Register {
                account: "ada".parse().unwrap(),
                email: Some("ada@example.com".into()),
                password: "hunter2hunter2".into(),
            }
        );
    }

    #[test]
    fn reset_flow_round_trips() {
        round_trip(&Request::new(Command::ResetRequest {
            email: "ada@example.com".into(),
        }));
        let confirm = Request::new(Command::ResetConfirm {
            email: "ada@example.com".into(),
            code: "123456".into(),
            password: "brand new passphrase".into(),
        });
        round_trip(&confirm);
        assert_eq!(
            confirm.serialize().unwrap(),
            "RESET CONFIRM ada@example.com 123456 :brand new passphrase"
        );
    }

    #[test]
    fn bad_reset_subcommand_is_typed_error() {
        assert_eq!(
            Request::parse("RESET TELEPATHY ada@example.com"),
            Err(ParseError::BadParam {
                verb: "RESET",
                what: "subcommand",
                value: "TELEPATHY".into()
            })
        );
    }

    #[test]
    fn auth_password_round_trips() {
        round_trip(&Request::new(Command::AuthPassword {
            identifier: "ada".into(),
            password: ":p4ss with space".into(),
        }));
    }

    #[test]
    fn auth_password_accepts_an_email_identifier() {
        // §6.1: login by email — the identifier is a free string, not an Account.
        let request = Request::parse("AUTH PASSWORD ada@example.com :hunter2hunter2").unwrap();
        let Command::AuthPassword {
            identifier,
            password,
        } = request.command
        else {
            panic!("not AUTH PASSWORD: {:?}", request.command);
        };
        assert_eq!(identifier, "ada@example.com");
        assert_eq!(password, "hunter2hunter2");
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
        round_trip(&Request::new(Command::Delivered {
            msgid: MSGID.parse().unwrap(),
        }));
        round_trip(&Request::new(Command::Undelivered {
            msgid: MSGID.parse().unwrap(),
            reason: Some("homeserver refused the puppet".into()),
        }));
        round_trip(&Request::new(Command::Undelivered {
            msgid: MSGID.parse().unwrap(),
            reason: None,
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
        round_trip(&Request::new(Command::Threads {
            channel: "#general".parse().unwrap(),
        }));
        round_trip(&Request::new(Command::ThreadName {
            channel: "#general".parse().unwrap(),
            root: "weft.example/01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap(),
            name: Some("Release planning".to_string()),
        }));
        // Omitted trailing = clear the name; must round-trip as None.
        round_trip(&Request::new(Command::ThreadName {
            channel: "#general".parse().unwrap(),
            root: "weft.example/01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap(),
            name: None,
        }));
        assert_eq!(
            Request::parse("THREAD NAME #general weft.example/01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
            Request::new(Command::ThreadName {
                channel: "#general".parse().unwrap(),
                root: "weft.example/01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap(),
                name: None,
            })
        );
        round_trip(&Request::new(Command::Caps {
            account: "ada".parse().unwrap(),
            scope: "#general".to_string(),
        }));
        round_trip(&Request::new(Command::RoleCreate {
            scope: "ns:gaming".to_string(),
            color: "#e8b93d".to_string(),
            caps: "mute,ban,kick,pin".to_string(),
            hoist: true,
            pingable: true,
            position: 3,
            name: "Head Moderator".to_string(),
        }));
        round_trip(&Request::new(Command::RoleUpdate {
            scope: "ns:gaming".to_string(),
            role: "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap(),
            color: "#e8b93d".to_string(),
            caps: "mute,ban".to_string(),
            hoist: false,
            pingable: true,
            position: 1,
            name: "Lead Mod".to_string(),
        }));
        round_trip(&Request::new(Command::RolesReorder {
            scope: "ns:gaming".to_string(),
            order: vec![
                "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap(),
                "01BX5ZZKBKACTAV9WEVGEMMVRZ".parse().unwrap(),
            ],
        }));
        round_trip(&Request::new(Command::RoleDelete {
            scope: "ns:gaming".to_string(),
            role: "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap(),
        }));
        round_trip(&Request::new(Command::RoleAssign {
            scope: "ns:gaming".to_string(),
            account: "bob".parse().unwrap(),
            role: "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap(),
        }));
        round_trip(&Request::new(Command::RoleUnassign {
            scope: "ns:gaming".to_string(),
            account: "bob".parse().unwrap(),
            role: "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap(),
        }));
        // v0.13: roles are addressed by ULID id — DELETE/ASSIGN take it
        // positionally, UPDATE subsumes the old RENAME (label is the trailing).
        assert_eq!(
            Request::parse("ROLE DELETE ns:gaming 01ARZ3NDEKTSV4RRFFQ69G5FAV")
                .unwrap()
                .command,
            Command::RoleDelete {
                scope: "ns:gaming".to_string(),
                role: "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap(),
            }
        );
        assert!(matches!(
            Request::parse("ROLE DELETE ns:gaming not-a-ulid"),
            Err(ParseError::BadParam { verb: "ROLE", .. })
        ));
        round_trip(&Request::new(Command::RolesList {
            scope: "ns:gaming".to_string(),
        }));
        round_trip(&Request::new(Command::RolesOf {
            scope: "ns:gaming".to_string(),
            account: "bob".parse().unwrap(),
        }));
        round_trip(&Request::new(Command::GrantsAt {
            scope: "#gaming/general".to_string(),
        }));
        assert_eq!(
            Request::parse("GRANTS #gaming/general"),
            Ok(Request::new(Command::GrantsAt {
                scope: "#gaming/general".to_string(),
            }))
        );
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
                attachments: vec!["weft-media://weft.example/b3hash".into()],
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
        round_trip(&Request::new(Command::InviteList {
            scope: "ns:gaming".into(),
        }));
        round_trip(&Request::with_label(
            Command::InviteRevokeAll {
                scope: "ns:gaming".into(),
            },
            "ra1",
        ));
        round_trip(&Request::new(Command::InviteRedeem {
            token: "B64TOKEN==".into(),
        }));
    }

    #[test]
    fn emoji_verbs_round_trip() {
        let ns = "01arz3ndektsv4rrffq69g5fav";
        let add = Request::with_label(
            Command::EmojiAdd {
                namespace: ns.parse().unwrap(),
                name: "partyblob".into(),
                media: "weft-media://weft.example/abc123".into(),
            },
            "e1",
        );
        assert_eq!(
            add.serialize().unwrap(),
            format!("@label=e1 EMOJI ADD {ns} partyblob weft-media://weft.example/abc123")
        );
        round_trip(&add);
        round_trip(&Request::new(Command::EmojiRemove {
            namespace: ns.parse().unwrap(),
            name: "partyblob".into(),
        }));
        round_trip(&Request::new(Command::EmojiList {
            namespace: ns.parse().unwrap(),
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
                vanity: "gaming".parse().unwrap(),
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

        // v0.13: existing namespaces are addressed by ULID id (creation carries
        // the desired vanity above).
        let ns = "01arz3ndektsv4rrffq69g5fav";
        round_trip(&Request::new(Command::NsMeta {
            ns: ns.parse().unwrap(),
            key: "title".into(),
            value: "The Gaming Lounge".into(),
        }));
        round_trip(&Request::new(Command::NsVisibility {
            ns: ns.parse().unwrap(),
            visibility: crate::types::Visibility::Private,
        }));
        round_trip(&Request::new(Command::NsDelegate {
            ns: ns.parse().unwrap(),
            subject: "ada".into(),
            caps: "ban,kick".into(),
        }));
        round_trip(&Request::new(Command::NsDelete {
            ns: ns.parse().unwrap(),
            confirm: ns.parse().unwrap(),
        }));
        round_trip(&Request::new(Command::NsJoin {
            ns: ns.parse().unwrap(),
        }));
        // NS JOIN also accepts a vanity name (§2.2 unlisted-by-name).
        round_trip(&Request::new(Command::NsJoin {
            ns: "my-server".parse().unwrap(),
        }));
        // §3.3 a `<scheme>://` target routes to the foreign-provisioning variant,
        // on the same `NS JOIN` verb.
        round_trip(&Request::with_label(
            Command::NsJoinForeign {
                uri: "matrix://matrix.org/gaming".parse().unwrap(),
            },
            "j1",
        ));
        assert!(matches!(
            Request::parse("NS JOIN matrix://matrix.org/gaming")
                .unwrap()
                .command,
            Command::NsJoinForeign { .. }
        ));
        round_trip(&Request::new(Command::NsLeave {
            ns: ns.parse().unwrap(),
        }));
        // `PART ns:<id>` is a lenient-in alias that parses to NS LEAVE; the
        // canonical strict-out form is always `NS LEAVE`.
        assert_eq!(
            Request::parse(&format!("PART ns:{ns}")),
            Ok(Request::new(Command::NsLeave {
                ns: ns.parse().unwrap(),
            }))
        );
        assert_eq!(
            Request::new(Command::NsLeave {
                ns: ns.parse().unwrap(),
            })
            .serialize()
            .unwrap(),
            format!("NS LEAVE {ns}")
        );
        assert!(Request::parse("NS FROB x").is_err());
    }

    #[test]
    fn ns_info_round_trip() {
        let ns = "01arz3ndektsv4rrffq69g5fav";
        let req = Request::with_label(
            Command::NsInfo {
                ns: ns.parse().unwrap(),
                detail: NsInfoKind::Members,
            },
            "i1",
        );
        assert_eq!(
            req.serialize().unwrap(),
            format!("@label=i1 NS INFO MEMBERS {ns}")
        );
        round_trip(&req);

        // Detail selector is case-insensitive (lenient-in).
        assert_eq!(
            Request::parse(&format!("NS INFO members {ns}")),
            Ok(Request::new(Command::NsInfo {
                ns: ns.parse().unwrap(),
                detail: NsInfoKind::Members,
            }))
        );
        // Unknown detail is a typed error, not a silent Unknown.
        assert!(Request::parse(&format!("NS INFO FROB {ns}")).is_err());
    }

    #[test]
    fn sync_round_trip() {
        // Fresh login: no cursor, just a preview cap.
        let fresh = Request::with_label(
            Command::Sync {
                since: None,
                preview: Some(30),
            },
            "s1",
        );
        assert_eq!(fresh.serialize().unwrap(), "@label=s1 SYNC preview=30");
        round_trip(&fresh);

        // Delta: opaque cursor echoed verbatim.
        round_trip(&Request::new(Command::Sync {
            since: Some("s_9f3c".into()),
            preview: Some(30),
        }));

        // Skeleton-only mode.
        round_trip(&Request::new(Command::Sync {
            since: None,
            preview: Some(0),
        }));

        // Bare `SYNC` is a fresh login with server-default preview.
        assert_eq!(
            Request::parse("SYNC"),
            Ok(Request::new(Command::Sync {
                since: None,
                preview: None,
            }))
        );
        // Unknown params are ignored (lenient-in).
        assert_eq!(
            Request::parse("SYNC since=abc preview=5 frob=nope"),
            Ok(Request::new(Command::Sync {
                since: Some("abc".into()),
                preview: Some(5),
            }))
        );
        assert!(Request::parse("SYNC preview=notanumber").is_err());
    }

    #[test]
    fn ns_recovery_verbs_round_trip() {
        let ns = "01arz3ndektsv4rrffq69g5fav";
        let transfer = Request::with_label(
            Command::NsTransfer {
                ns: ns.parse().unwrap(),
                new_owner: "bob".parse().unwrap(),
                signature: "B64SIG==".into(),
            },
            "t1",
        );
        assert!(transfer.serialize().unwrap().contains("sig=B64SIG=="));
        round_trip(&transfer);
        assert_eq!(
            Request::parse(&format!("NS TRANSFER {ns} bob")),
            Err(ParseError::MissingParam {
                verb: "NS",
                what: "sig tag (root signature)"
            })
        );

        round_trip(&Request::new(Command::NsRecoverySet {
            ns: ns.parse().unwrap(),
            m: 2,
            keys: "K1==,K2==,K3==".into(),
        }));
        round_trip(&Request::new(Command::NsRecover {
            ns: ns.parse().unwrap(),
            rotation: "B64ROTATION==".into(),
        }));
        round_trip(&Request::with_label(
            Command::NsRecoveryCancel {
                ns: ns.parse().unwrap(),
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
            namespace: "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap(),
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
            network: "weft.example".parse().unwrap(),
            token: "B64BRIDGETOKEN==".into(),
        }));
        assert_eq!(
            Request::new(Command::AuthBridge {
                network: "weft.example".parse().unwrap(),
                token: "T==".into(),
            })
            .serialize()
            .unwrap(),
            "AUTH BRIDGE weft.example T=="
        );
    }

    #[test]
    fn bridge_verbs_round_trip() {
        let propose = Request::with_label(
            Command::BridgePropose {
                scope: "#general".into(),
                peer: "weft.example".parse().unwrap(),
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
            wire.contains("BRIDGE PROPOSE #general weft.example history=full media=mirror-max:1048576 typing=yes voice=yes"),
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
            peer: "weft.example".parse().unwrap(),
            version: 3,
        }));
        round_trip(&Request::new(Command::BridgeAdd {
            peer: "weft.example".parse().unwrap(),
            channel: "#gaming/lobby".parse().unwrap(),
        }));
        round_trip(&Request::new(Command::BridgeRemove {
            peer: "weft.example".parse().unwrap(),
            channel: "#general".parse().unwrap(),
        }));
        round_trip(&Request::new(Command::BridgeSever {
            peer: "weft.example".parse().unwrap(),
        }));
        round_trip(&Request::new(Command::BridgeRequest {
            ns: "gaming".parse().unwrap(),
            invite: None,
        }));
        // With an invite (unlocks a non-public federating namespace).
        let br = Request::new(Command::BridgeRequest {
            ns: "gaming".parse().unwrap(),
            invite: Some("inv_abc123".into()),
        });
        assert!(br.serialize().unwrap().contains("invite=inv_abc123"));
        round_trip(&br);
        round_trip(&Request::new(Command::Federate {
            network: "weft.example".parse().unwrap(),
            namespace: "gaming".parse().unwrap(),
            invite: None,
        }));
        let fed = Request::new(Command::Federate {
            network: "weft.example".parse().unwrap(),
            namespace: "gaming".parse().unwrap(),
            invite: Some("inv_abc123".into()),
        });
        assert!(fed.serialize().unwrap().contains("invite=inv_abc123"));
        round_trip(&fed);
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
    fn foreign_bridge_verbs_round_trip() {
        round_trip(&Request::with_label(
            Command::AuthAdapter {
                pubkey: "Zm9vYmFy".into(),
            },
            "a1",
        ));
        round_trip(&Request::new(Command::RealmRegister {
            scheme: "matrix".parse().unwrap(),
        }));
        round_trip(&Request::with_label(
            Command::RealmAssert {
                realm: "matrix://matrix.org".parse().unwrap(),
            },
            "b1",
        ));
        round_trip(&Request::new(Command::RealmWithdraw));
        round_trip(&Request::new(Command::ProvisionOk { job: "j1".into() }));
        round_trip(&Request::new(Command::ProvisionErr { job: "j1".into() }));

        assert_eq!(
            Request::new(Command::RealmAssert {
                realm: "matrix://matrix.org".parse().unwrap(),
            })
            .serialize()
            .unwrap(),
            "REALM ASSERT matrix://matrix.org"
        );

        // ASSERT binds a realm, not a space/channel within it.
        assert!(Request::parse("REALM ASSERT matrix://matrix.org/gaming").is_err());
        assert!(Request::parse("REALM FROB matrix").is_err());
        assert!(Request::parse("PROVISION-OK").is_err()); // job required
    }

    #[test]
    fn plugin_verbs_round_trip() {
        round_trip(&Request::new(Command::Plugins));
        round_trip(&Request::with_label(
            Command::PluginInvoke {
                plugin: "translate".into(),
                action: "translate".into(),
                ctx_ref: Some("net.example/01ARZ3NDEKTSV4RRFFQ69G5FAV".into()),
                params: Some("Zm9vYmFy".into()),
            },
            "i1",
        ));
        // No ctx-ref, no params (a global action).
        round_trip(&Request::new(Command::PluginInvoke {
            plugin: "modq".into(),
            action: "open".into(),
            ctx_ref: None,
            params: None,
        }));
        round_trip(&Request::new(Command::PluginSubmit {
            view_id: "translate:ab12:1".into(),
            values: Some("Zm9v".into()),
        }));
        round_trip(&Request::new(Command::PluginAction {
            view_id: "v:ab12:2".into(),
            button: "save".into(),
            values: None,
        }));
        round_trip(&Request::new(Command::PluginSubscribe {
            view_id: "v:ab12:3".into(),
        }));
        round_trip(&Request::new(Command::PluginUnsubscribe {
            view_id: "v:ab12:3".into(),
        }));
        round_trip(&Request::new(Command::PluginClose {
            view_id: "v:ab12:3".into(),
        }));

        assert!(matches!(
            Request::parse("@params=Zm9v PLUGIN INVOKE p a").unwrap().command,
            Command::PluginInvoke { params: Some(p), ctx_ref: None, .. } if p == "Zm9v"
        ));
        assert!(Request::parse("PLUGIN FROB x").is_err());
        assert!(Request::parse("PLUGIN SUBMIT").is_err()); // view-id required
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
                note: Some("forwarded from weft.example".into()),
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
                about: Some("Cryptographer & poet.".into()),
                status: Some("🎧 in the zone".into()),
            },
            "p1",
        );
        let wire = set.serialize().unwrap();
        assert!(wire.contains("display=Ada\\sL."), "{wire}");
        assert!(wire.contains("avatar=b3-abc"), "{wire}");
        assert!(wire.contains("about="), "{wire}");
        assert!(wire.contains("status="), "{wire}");
        round_trip(&set);

        // Partial update: avatar only (display + about + status left unchanged → absent tags).
        round_trip(&Request::new(Command::ProfileSet {
            display: None,
            avatar: Some("b3-xyz".into()),
            about: None,
            status: None,
        }));
        // Clear fields: a present-but-empty tag distinguishes clear from absent.
        round_trip(&Request::new(Command::ProfileSet {
            display: Some(String::new()),
            avatar: None,
            about: Some(String::new()),
            status: Some(String::new()),
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
    fn nick_verbs_round_trip() {
        let set = Request::with_label(
            Command::Nick {
                scope: "ns:gaming".into(),
                account: "bob".parse().unwrap(),
                nick: "Cool Bob".into(),
            },
            "n1",
        );
        assert!(set
            .serialize()
            .unwrap()
            .contains("NICK ns:gaming bob :Cool Bob"));
        round_trip(&set);
        // Clearing = a present-but-empty trailing.
        round_trip(&Request::new(Command::Nick {
            scope: "ns:gaming".into(),
            account: "ada".parse().unwrap(),
            nick: String::new(),
        }));
        round_trip(&Request::with_label(
            Command::Nicks {
                scope: "ns:gaming".into(),
            },
            "q1",
        ));
        assert!(Request::parse("NICKS").is_err()); // needs a scope
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
    fn friend_verbs_round_trip() {
        let add = Request::with_label(
            Command::FriendAdd {
                user: "bob@other.example".parse().unwrap(),
            },
            "f1",
        );
        assert_eq!(
            add.serialize().unwrap(),
            "@label=f1 FRIEND ADD bob@other.example"
        );
        round_trip(&add);
        round_trip(&Request::new(Command::FriendAccept {
            user: "ada@home.example".parse().unwrap(),
        }));
        round_trip(&Request::new(Command::FriendRemove {
            user: "carol@home.example".parse().unwrap(),
        }));
        round_trip(&Request::new(Command::Friends));

        // A friend target must be fully qualified (federation-able).
        assert!(Request::parse("FRIEND ADD bob").is_err());
        assert!(Request::parse("FRIEND FROB bob@x.example").is_err()); // bad subcommand
    }

    #[test]
    fn group_dm_verbs_round_trip() {
        const G: &str = "&01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let create = Request::with_label(
            Command::GroupCreate {
                members: vec![
                    "bob@home.example".parse().unwrap(),
                    "carol@peer.example".parse().unwrap(),
                ],
            },
            "g1",
        );
        assert_eq!(
            create.serialize().unwrap(),
            "@label=g1 GROUP CREATE bob@home.example carol@peer.example"
        );
        round_trip(&create);
        round_trip(&Request::new(Command::GroupAdd {
            group: G.parse().unwrap(),
            user: "dave@home.example".parse().unwrap(),
        }));
        round_trip(&Request::new(Command::GroupRemove {
            group: G.parse().unwrap(),
            user: "dave@home.example".parse().unwrap(),
        }));
        round_trip(&Request::new(Command::GroupLeave {
            group: G.parse().unwrap(),
        }));
        round_trip(&Request::new(Command::GroupName {
            group: G.parse().unwrap(),
            name: Some("weekend plans".to_string()),
        }));
        round_trip(&Request::new(Command::GroupName {
            group: G.parse().unwrap(),
            name: None,
        }));
        round_trip(&Request::new(Command::Groups));
        round_trip(&Request::new(Command::GroupCall {
            group: G.parse().unwrap(),
            media: None,
        }));
        // A federated ring carries the host network's relay leg.
        round_trip(&Request::new(Command::GroupCall {
            group: G.parse().unwrap(),
            media: Some(CallMediaGrant {
                room: "gcall:01ARZ".into(),
                token: "relay.tok".into(),
                endpoint: Some("wss://lk.host.example".into()),
            }),
        }));
        round_trip(&Request::new(Command::GroupCallLeave {
            group: G.parse().unwrap(),
        }));
        for (active, reply) in [(true, true), (false, false)] {
            round_trip(&Request::new(Command::GroupCallRoster {
                group: G.parse().unwrap(),
                user: "carol@peer.example".parse().unwrap(),
                active,
                reply,
            }));
        }
        // A group target is `&<ulid>`; MSG to it round-trips.
        round_trip(&Request::new(Command::Msg {
            target: G.parse().unwrap(),
            body: Some("hi group".to_string()),
            meta: Default::default(),
        }));

        assert!(Request::parse("GROUP CREATE").is_err()); // needs ≥1 member
        assert!(Request::parse("GROUP FROB &x").is_err()); // bad subcommand
    }

    #[test]
    fn call_verbs_round_trip() {
        let place = Request::with_label(
            Command::Call {
                user: "bob@peer.example".parse().unwrap(),
                media: None,
            },
            "c1",
        );
        assert_eq!(
            place.serialize().unwrap(),
            "@label=c1 CALL bob@peer.example"
        );
        round_trip(&place);
        round_trip(&Request::new(Command::CallAccept {
            user: "ada@home.example".parse().unwrap(),
        }));
        round_trip(&Request::new(Command::CallDecline {
            user: "ada@home.example".parse().unwrap(),
        }));
        round_trip(&Request::new(Command::CallEnd {
            user: "bob@peer.example".parse().unwrap(),
        }));
        // A call target must be fully qualified.
        assert!(Request::parse("CALL bob").is_err());

        // A federated CALL carries the callee's pre-minted LiveKit credential.
        let fed = Request::new(Command::Call {
            user: "bob@peer.example".parse().unwrap(),
            media: Some(CallMediaGrant {
                room: "call:01ARZ".to_string(),
                token: "jwt.abc.def".to_string(),
                endpoint: Some("wss://lk.home.example".to_string()),
            }),
        });
        round_trip(&fed);
        // The tags survive the round trip and reconstruct the grant.
        let Command::Call { media, .. } =
            Request::parse(&fed.serialize().unwrap()).unwrap().command
        else {
            panic!("expected CALL");
        };
        assert_eq!(media.unwrap().room, "call:01ARZ");
        // Endpoint is optional (a deployment that hands the URL out of band).
        round_trip(&Request::new(Command::Call {
            user: "bob@peer.example".parse().unwrap(),
            media: Some(CallMediaGrant {
                room: "call:01ARZ".to_string(),
                token: "jwt.abc.def".to_string(),
                endpoint: None,
            }),
        }));
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
