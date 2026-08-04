//! Shared server context handed to every session.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use weft_crypto::{
    Attestation, Capability, Grant, Keypair, Profile, PublicKey, SignedProfile, Subject, TokenScope,
};
use weft_proto::{
    Account, CallMediaGrant, ChannelName, ErrCode, GroupId, MsgId, NamespaceName, NetworkName,
    RetentionPolicy, UserRef,
};
use weft_store::{
    AccountStore, BlobStore, CapabilityStore, ChannelStore, EmojiStore, EventStore, FriendStore,
    GroupStore, InviteStore, MediaBlocklistStore, MediaStore, MembershipStore, ModerationStore,
    NamespaceStore, NetblockStore, NickStore, PeerStore, PinStore, ProfileStore, ReportStore,
    RoleStore, StoreError,
};

use crate::accounts::Accounts;
use crate::directory::Directory;
use crate::media::{MediaRegistry, UploadGrant};
use crate::registry::Registry;

/// The only protocol version this server speaks (§3.6).
pub const PROTOCOL_VERSION: &str = "weft/1";

/// Who is acting in a session: a **local** account, or a **federated** user
/// (`account@network`) whose home network vouches for her over a bridge (§10.4,
/// homeserver authority — F proves its network key, then speaks for its users).
/// Enforcement keys by the subject; local-only authority (operator / namespace
/// owner) never applies to a foreign actor.
#[derive(Debug, Clone)]
pub enum Actor {
    Local(Account),
    /// `account@network`.
    Foreign(String),
}

impl Actor {
    /// The local account, if this actor is local (a foreign actor → `None`) —
    /// for owner/operator comparisons a federated user can never satisfy.
    pub fn local(&self) -> Option<&Account> {
        match self {
            Actor::Local(account) => Some(account),
            Actor::Foreign(_) => None,
        }
    }
}

impl std::fmt::Display for Actor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Actor::Local(account) => write!(f, "{account}"),
            Actor::Foreign(user) => write!(f, "{user}"),
        }
    }
}

/// Attestation lifetime (§10.2: rotation = superseding attestation, so
/// lifetimes stay short-ish; re-auth refreshes well before expiry).
const ATTESTATION_TTL_SECS: u64 = 30 * 24 * 3600;

/// How long a cross-network group post's echo token is kept before it's GC'd —
/// a home that hasn't echoed the message back within this window is treated as
/// unreachable (the message still arrives via backfill, just without the label).
const GROUP_ECHO_TTL_MS: u64 = 60_000;

/// Static identity/config of this network, from weftd's config file.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub network: NetworkName,
    /// WELCOME trailing text (§3.6).
    pub motd: Option<String>,
    /// `features=` flags for WELCOME. Empty in M1 — media/voice/e2ee/
    /// backfill/presence all land in later milestones.
    pub features: Vec<String>,
}

/// §11 federation config: which peer networks this server will bridge with and
/// how it treats incoming proposals.
///
/// Two trust modes for opening a bridge session (§11.2):
/// - **Pinned** (default, closed): only peers in `peer_keys` may bridge, and
///   only with the exact key pinned there — key rotation is a config change,
///   not a bypass. The secure default.
/// - **Accept-any** (`accept_any = true`, open federation): any non-blocked
///   network may open a bridge by asserting a key and proving control of it
///   (challenge/proof). The session is then bound to *that* key — a peer can
///   only speak for the identity it holds a key for, but nothing external
///   confirms the key really is that network's (no pin / well-known check), so
///   this is trust-on-first-use. `NETBLOCK` is the escape hatch.
///
/// A pinned key always wins over accept-any for that network.
#[derive(Debug, Clone, Default)]
pub struct FederationConfig {
    /// peer network → its pinned Ed25519 signing key.
    pub peer_keys: HashMap<NetworkName, PublicKey>,
    /// Accept a bridge from *any* non-blocked network (open federation),
    /// trusting the key it proves control of. Pinned peers still take their
    /// pinned key.
    pub accept_any: bool,
    /// Auto-accept incoming `BRIDGE PROPOSE` (reference convenience). When
    /// false, an operator must `BRIDGE ACCEPT` explicitly.
    pub auto_accept: bool,
}

/// Everything a session needs: identity, accounts, channels, events, and
/// the capability layer (§6.5, §10.4).
pub struct ServerCtx {
    pub info: ServerInfo,
    pub registry: Registry,
    pub accounts: Accounts,
    /// Read path for HISTORY; the write path goes through channel actors.
    pub events: Arc<dyn EventStore>,
    /// §9.5: one network-config retention policy for all DMs.
    pub dm_policy: weft_proto::RetentionPolicy,
    /// Account-scoped routing: DMs + MARK sync.
    pub(crate) directory: Directory,
    /// §6.1: `registration: open` — closed networks answer REGISTER with
    /// FORBIDDEN.
    pub registration_open: bool,
    /// Grants + revocation epochs (§10.4).
    pub(crate) caps: Arc<dyn CapabilityStore>,
    /// Invite lifecycle (§6.5).
    pub(crate) invites: Arc<dyn InviteStore>,
    /// Channel settings (topic, view-gated) beyond the running actors.
    pub(crate) channel_store: Arc<dyn ChannelStore>,
    /// User-owned namespaces (§2.1).
    pub(crate) namespaces: Arc<dyn NamespaceStore>,
    /// Report queue + retention holds (§6.7).
    pub(crate) reports: Arc<dyn ReportStore>,
    /// Bridge peerings + signed manifests (§11.1).
    pub(crate) peers: Arc<dyn PeerStore>,
    /// Operator network blocklist (§11.6).
    pub(crate) netblocks: Arc<dyn NetblockStore>,
    /// Mute/ban deny-list (§6.7).
    pub(crate) moderation: Arc<dyn ModerationStore>,
    /// Pinned messages, per channel (§6.4).
    pub(crate) pins: Arc<dyn PinStore>,
    /// Custom emoji, per namespace (§9.4).
    pub emoji: Arc<dyn EmojiStore>,
    /// Persistent channel membership for auto-rejoin (§6.3).
    pub(crate) memberships: Arc<dyn MembershipStore>,
    /// Role definitions — named capability-token bundles per scope (§6.5).
    pub(crate) roles: Arc<dyn RoleStore>,
    /// §10.3 display profiles (nick + avatar) keyed by account handle.
    pub profiles: Arc<dyn ProfileStore>,
    /// §10.3 per-namespace server nicknames.
    pub(crate) nicks: Arc<dyn NickStore>,
    /// Social graph (friends + pending requests). Federation-able — peers are
    /// full `UserRef`s.
    pub(crate) friends: Arc<dyn FriendStore>,
    /// Group DMs: identity + membership (messages live in `events` under
    /// `Scope::Group`). Federation-able members.
    pub(crate) groups: Arc<dyn GroupStore>,
    /// §6.1 live presence, in-memory only (never stored, never bridged).
    /// account → last non-invisible status; served with MEMBERS for correct
    /// roster dots.
    pub(crate) presence:
        std::sync::Mutex<std::collections::HashMap<Account, weft_proto::PresenceStatus>>,
    /// §13 content-addressed blob storage (fs CAS in weftd; memory in tests).
    /// The DS never reads blob bytes for meaning — it just stores/serves them.
    pub blobs: Arc<dyn BlobStore>,
    /// §13 media reference index (blob⇄message) — membership gating + refcount GC.
    pub media_refs: Arc<dyn MediaStore>,
    /// §13 media hash blocklist — a blocked BLAKE3 hash is dead on arrival.
    pub(crate) media_blocks: Arc<dyn MediaBlocklistStore>,
    /// §13 media data-plane token registry (upload grants + fetch bearers).
    media: MediaRegistry,
    /// §11 federation config: pinned peer keys + auto-accept.
    pub(crate) federation: FederationConfig,
    /// Foreign-bridge framework (`docs/architecture/foreign-bridge-framework.md`
    /// §3): pinned adapter keys, each authorized for one scheme
    /// (`[[foreign_bridge]]` config; multiple entries). An adapter authenticates
    /// by proving control of a pinned key (`AUTH ADAPTER`); `REALM REGISTER` /
    /// `REALM ASSERT` then require the proven key to be pinned for the asserted
    /// scheme. A handful of entries — a linear scan, no hashing of keys.
    pub(crate) foreign_adapters: Vec<(weft_proto::Scheme, PublicKey)>,
    /// Foreign-bridge framework (§3.3): parked `NS JOIN <uri>` requests awaiting a
    /// `PROVISION-OK` / `PROVISION-ERR`, keyed by job token.
    pending_provision: std::sync::Mutex<HashMap<String, PendingProvision>>,
    /// Monotonic source of provisioning job tokens (§3.3).
    provision_seq: AtomicU64,
    /// Plugin system (`docs/architecture/plugin-spec.md` §4.2): pinned remote-plugin
    /// keys → their `[[plugin.remote]]` id. Proven at `AUTH ADAPTER`.
    pub(crate) remote_plugins: Vec<(String, PublicKey, Vec<weft_proto::Scheme>)>,
    /// The unified **provider registry** (plugin-spec §18): id → its writer,
    /// declared actions, and the schemes it provisions. Populated by
    /// `PLUGIN-REGISTER` and/or `REALM REGISTER`; dropped when the session ends.
    plugin_registry: std::sync::Mutex<HashMap<String, PluginReg>>,
    /// Parked client invocations awaiting a plugin's `PLUGIN-VIEW`/`-RESULT`,
    /// keyed by the minted view-id (§11.1) → the requester's writer + request label.
    pending_invoke: std::sync::Mutex<HashMap<String, PendingReply>>,
    /// Monotonic source of view-ids (§11.1).
    view_seq: AtomicU64,
    /// §2.2 namespace creation: `open` (any account, up to `ns_quota`) or
    /// gated (needs the `ns-create` cap).
    pub(crate) ns_creation_open: bool,
    pub(crate) ns_quota: u64,
    /// §6.7 moderation: lowercased substrings barred from new **usernames** and
    /// **namespace vanities** (REGISTER + NS CREATE). Empty = no substring filter.
    pub(crate) banned_substrings: Vec<String>,
    /// §6.7 case-insensitive regex patterns barred from the same names. Compiled
    /// at boot; invalid patterns are dropped with a warning. Empty = no regex filter.
    pub(crate) banned_regexes: Vec<regex::Regex>,
    /// §6.1 whether REGISTER must carry a contact email (verify-later, §10.5).
    /// Off by default; weftd only turns it on when SMTP is configured (a reset
    /// code has to be deliverable). The WEFT-IRC gateway is exempt — it
    /// auto-registers emailless accounts, which simply can't password-reset.
    pub(crate) require_email: bool,
    /// Operator accounts: they hold the network key's authority — every
    /// capability at `*` (§11.3). This is how the first admin exists;
    /// everyone else's caps chain from a GRANT.
    operators: HashSet<Account>,
    /// The network signing key (§10.2): signs device attestations AND the
    /// capability tokens rooted at `*`/`#chan` scope (§11.3).
    identity: Keypair,
    next_session: AtomicU64,
    /// Live session count — inc/dec per connection in `run_session`. Read by the
    /// admin panel's `/stats`; never affects protocol behavior.
    pub connections: Arc<std::sync::atomic::AtomicUsize>,
    /// Graceful-shutdown signal. Cancelled once on shutdown; sessions finish
    /// their current command and close, accept loops stop, maintenance exits.
    pub shutdown: tokio_util::sync::CancellationToken,
    /// §11.10 auto-federation trigger port: `FEDERATE` (weft-core) can't dial
    /// (no transport), so it hands requests to weftd (L3), which owns the
    /// dialer. `None` = the network's `auto_bridge` policy is off.
    auto_bridge_tx: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<AutoBridgeRequest>>,
    /// §11.8 media-mirror port: on ingesting a bridged message with attachments,
    /// core (socket-free) hands weftd the pull requests; weftd fetches the blobs
    /// over the bridge data plane. `None` = no sink installed.
    mirror_tx: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<MirrorRequest>>,
    /// Social-layer federation port: core hands weftd a [`FriendDeliver`] when a
    /// local friend action targets a user on another network; weftd tunnels the
    /// command to that peer (§11.10 home-side). `None` = no sink installed.
    friend_deliver_tx: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<FriendDeliver>>,
    /// §11.7 bridge-backfill port: when a peer answers our federated HISTORY with
    /// `STREAM ACCEPT <token>` (large page), core (socket-free) hands weftd the
    /// pull; weftd opens a data stream on the bridge, drains it, and ingests each
    /// line. `None` = no sink installed.
    backfill_tx: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<BackfillPull>>,
    /// §11.7 lazy backfill demand: each **outbound** bridge session registers a
    /// sender here; a local client's HISTORY that runs out of local scrollback
    /// for a forwardable channel signals every registered bridge, which fetches
    /// that window from its peer on demand (we never eagerly pull a whole
    /// federated scrollback nobody has asked to see). Closed senders are pruned
    /// on send, so a dropped bridge deregisters itself.
    backfill_demand: std::sync::Mutex<Vec<tokio::sync::mpsc::UnboundedSender<BackfillReq>>>,
    /// §11.14 live outbound-bridge line queues: `peer → that bridge session's
    /// writer`. A local session's `@as` command to a bridged peer rides its
    /// persistent bridge (one hop, homeserver authority) — no ephemeral dial —
    /// and the command's effects return as events over the same bridge.
    /// Registered when an outbound bridge enters `State::Bridge`, removed when it
    /// closes.
    bridge_out: std::sync::Mutex<HashMap<NetworkName, tokio::sync::mpsc::Sender<String>>>,
    /// §11.10 per-account cooldown on `FEDERATE` — a light dial-storm guard even
    /// under the open trigger policy (§6).
    federate_cooldown: std::sync::Mutex<HashMap<Account, std::time::Instant>>,
    /// §16 WEFT-RT voice: the SFU backend weftd installs behind a feature flag.
    /// `None` = this server has no voice (advertises no `features=voice`; voice
    /// verbs answer `UNSUPPORTED`). Set once at boot, like the sink ports.
    voice: std::sync::OnceLock<Arc<dyn crate::voice::VoiceBackend>>,
    /// §16 the live voice roster per channel: `channel → session → member`. The
    /// source of the `VOICE STATE` snapshot a joiner gets, and how a moderator's
    /// `MUTE` finds a target's live voice sessions.
    voice_rooms: std::sync::Mutex<HashMap<ChannelName, HashMap<u64, crate::voice::VoiceMember>>>,
    /// Social layer: live 1:1 friend calls, keyed by the canonical account pair
    /// (same-network). Value = the ad-hoc room + whether it's been accepted.
    calls: std::sync::Mutex<HashMap<(UserRef, UserRef), CallInfo>>,
    /// Social layer: live **group DM** calls, keyed by group. Value = this
    /// network's room for the call + the local accounts currently in it. Members
    /// join/leave; the entry is dropped when the last local participant leaves.
    group_calls: std::sync::Mutex<HashMap<GroupId, GroupCallInfo>>,
    /// Cross-network group posts awaiting their home-minted echo: correlation
    /// token → (the spoke poster's session, when it was registered). Swept on
    /// insert so a token whose echo never comes back (the home was unreachable)
    /// can't leak.
    group_echoes: std::sync::Mutex<HashMap<String, (u64, u64)>>,
    /// §16 M-lk-3b federated voice: the media-relay driver weftd installs (a
    /// libwebrtc `livekit`-SDK relay, or the no-op default). `None` = no relaying.
    voice_relay: std::sync::OnceLock<Arc<dyn crate::voice::VoiceRelay>>,
    /// §16 M-lk-3b relay refcounts: `(peer, key) → live local participants`,
    /// where `key` is a foreign channel name or a cross-network call room. A relay
    /// starts on the first local joiner and stops on the last (or on
    /// `SEVER`/`NETBLOCK`, invariant 7).
    voice_relays: std::sync::Mutex<HashMap<(NetworkName, String), usize>>,
    /// §10.5 the email sender weftd installs (SMTP, or a dev log-mailer).
    mailer: std::sync::OnceLock<Arc<dyn crate::mailer::Mailer>>,
    /// Framework §7a.0a: answers whether a domain runs a WEFT network, so a
    /// bridge cannot claim a realm its owner has already given to WEFT.
    probe: std::sync::OnceLock<Arc<dyn crate::probe::NetworkProbe>>,
    /// §10.5 pending email verification codes: `(account, kind) → (code,
    /// expiry-ms)`. In-memory + short-lived — a restart just means re-request.
    verify_codes: std::sync::Mutex<HashMap<(Account, String), (String, u64)>>,
}

/// §11.8 a blob to mirror from a bridge peer, handed core→weftd.
#[derive(Debug, Clone)]
pub struct MirrorRequest {
    /// The origin network to pull from (the blob's `weft-media://<origin>/…`).
    pub peer: NetworkName,
    /// The BLAKE3 content hash to fetch + verify.
    pub hash: String,
}

/// §11.7 a federated backfill to pull, handed core→weftd: the peer offered a
/// `STREAM ACCEPT <token>` in response to our HISTORY, so weftd opens a data
/// stream on the bridge to `peer`, sends `BACKFILL <token>`, and ingests the
/// serialized events it streams back (origin-authority-checked, invariant 2).
#[derive(Debug, Clone)]
pub struct BackfillPull {
    /// The peer network serving the backfill (and the origin of its events).
    pub peer: NetworkName,
    /// The one-time backfill grant token the peer minted.
    pub token: String,
}

/// §11.7 a local client's on-demand backfill need: fetch history for `channel`
/// older than `before` (the client's oldest, or `None` for the recent page)
/// from whichever bridge peer forwards it. Broadcast to every outbound bridge
/// session; each ignores channels it doesn't forward.
#[derive(Debug, Clone)]
pub struct BackfillReq {
    pub channel: ChannelName,
    pub before: Option<MsgId>,
}

/// A live 1:1 friend call (social layer). For a cross-network call **each network
/// hosts its own LiveKit room** for its local participant; a relay bridges the two
/// rooms so neither client ever connects to the other network's LiveKit
/// (protecting client IPs). A same-network call is one room, minted at accept.
#[derive(Debug, Clone)]
pub(crate) struct CallInfo {
    /// Who placed the call (the other side is the callee).
    pub caller: UserRef,
    /// The LiveKit room **this** network hosts (same-network: the single shared
    /// room). Keys the relay on the callee's side.
    pub room: String,
    /// False while ringing, true once accepted.
    pub active: bool,
    /// The credential for **this** network's local participant, delivered as
    /// `CALL-MEDIA` when they accept. `Some` on a cross-network call (pre-minted
    /// for our own LiveKit room); `None` same-network (minted at accept).
    pub local_media: Option<CallMediaGrant>,
    /// Set only on the **callee's** network of a cross-network call: the caller
    /// network's relay leg (its room + a relay token + URL) to bridge our room to.
    /// Present ⇒ spawn a media relay on accept, tear it down on end.
    pub relay_leg: Option<CallMediaGrant>,
}

/// The outcome of placing a call.
pub(crate) enum CallPlace {
    /// Ringing — carries the room to advertise to the callee.
    Ringing(String),
    /// The callee is already in a call.
    Busy,
    /// A call between these two already exists (ringing or active).
    Exists,
}

/// A live group DM call on **this** network: the room its local members share and
/// who is currently in it. Cross-network members join their own network's room,
/// bridged to the **host** network's by a relay (a star, hub = host — §16 M-lk-3b).
#[derive(Debug, Clone)]
pub(crate) struct GroupCallInfo {
    /// The LiveKit room this network hosts for the group call.
    pub room: String,
    /// Local accounts currently in the call.
    pub participants: std::collections::HashSet<Account>,
    /// Set only when **we are a spoke** (the call started on another network): the
    /// host network we bridge our room to, and its relay leg (its room + a relay
    /// token + URL). `None` ⇒ we are the host.
    pub host_net: Option<NetworkName>,
    pub host_leg: Option<CallMediaGrant>,
}

/// The result of a local member joining a group call.
pub(crate) struct GroupCallJoin {
    pub room: String,
    /// True if `account` was newly added (false = a re-join, already present).
    pub newly: bool,
    /// True if this was the first local participant (the call just became active
    /// on this network).
    pub first: bool,
    /// `Some((host, leg))` when we are a **spoke** and just became active — spawn
    /// the media relay bridging our room to the host's.
    pub spoke: Option<(NetworkName, CallMediaGrant)>,
}

/// The result of a local member leaving a group call.
pub(crate) struct GroupCallLeave {
    pub room: String,
    /// True if the last local participant left (the call ended on this network).
    pub empty: bool,
    /// The host network to release the relay for, when we were a spoke.
    pub host_net: Option<NetworkName>,
}

/// A `FEDERATE` request handed from weft-core to weftd's dialer (§11.10).
#[derive(Debug, Clone)]
pub struct AutoBridgeRequest {
    pub network: NetworkName,
    /// The foreign namespace's human **vanity** name (the user typed
    /// `network/vanity`); the peer resolves it to its own id (v0.13).
    pub namespace: weft_proto::VanityName,
    /// A foreign-namespace invite the user holds (§11.10) — unlocks a
    /// non-public but `federation`-open namespace; `None` for a public one.
    pub invite: Option<String>,
}

/// A parked `NS JOIN <uri>` awaiting the adapter's `PROVISION-OK`/`PROVISION-ERR`
/// (foreign-bridge framework §3.3). The completion (roster or NO-SUCH-TARGET) is
/// pushed to the requester session's raw-line writer, echoing the join's label.
pub(crate) struct PendingProvision {
    pub(crate) reply: tokio::sync::mpsc::Sender<String>,
    pub(crate) label: Option<String>,
    /// The provider the PROVISION was routed to — so its death can fail this
    /// parked join instead of leaving the client hanging (§3.5 label discipline).
    pub(crate) provider: String,
    /// The joined URI + requester, for the PROVISION-OK completion (resolve the
    /// asserted namespace by origin, add the membership, ack the join).
    pub(crate) uri: weft_proto::ForeignUri,
    pub(crate) account: Account,
}

/// A parked requester's raw-line writer + request label, for a cross-session
/// completion (foreign-bridge provisioning §3.3; plugin invocation §11.1).
type PendingReply = (tokio::sync::mpsc::Sender<String>, Option<String>);

/// A registered remote plugin (App Service, plugin-spec.md §12.3): the writer
/// weftd pushes routed invocations to, plus the plugin's declared actions (the
/// catalog it serves to clients) and display metadata.
struct PluginReg {
    out: tokio::sync::mpsc::Sender<String>,
    name: String,
    icon: Option<String>,
    actions: Vec<weft_proto::ActionDecl>,
    /// Schemes this provider provisions (`NS JOIN <scheme>://…` routing, §18).
    schemes: Vec<weft_proto::Scheme>,
}

/// Deliver a control line to a **peer network** (§11.14). Preferred path: the
/// peer's persistent bridge (see [`ServerCtx::request_friend_deliver`]); the
/// ephemeral tunnel driver is the fallback for a never-bridged peer. A user
/// command carries `from` (run `@as=<from>`); a home-fan-out event carries
/// `from = None`. Fire-and-forget: effects return as events over the bridge.
/// §11.14 attribute a serialized command line to the acting local account by
/// adding the `@as=<account>` tag. The peer reconstructs `account@<our-network>`
/// from its authenticated bridge and enforces the command against its own grants.
/// `None` if the line does not parse (never emitted by our own serializer).
fn with_as_tag(line: &str, from: &Account) -> Option<String> {
    let mut parsed = weft_proto::Line::parse(line).ok()?;
    parsed.tags.insert("as".to_string(), from.to_string());
    parsed.serialize().ok()
}

#[derive(Debug, Clone)]
pub struct FriendDeliver {
    /// The peer network to deliver to.
    pub peer: NetworkName,
    /// The acting local account when this is a **user command** — the peer runs
    /// it `@as=<from>` (§11.14). `None` when the line is an **event** the home
    /// fans out to a member network (a group `MESSAGE`/`EDITED`/roster) — events
    /// carry no actor tag and are ingested as bridged events.
    pub from: Option<Account>,
    /// The serialized line: a §6 command (when `from` is set) or a §7 event.
    pub line: String,
}

impl ServerCtx {
    /// Spawns one actor per seeded channel; the registry mutates at runtime
    /// via CHANNEL CREATE/DELETE (§6.3). One store object backs every port.
    #[allow(clippy::too_many_arguments)]
    pub fn new<S>(
        info: ServerInfo,
        channels: impl IntoIterator<Item = (ChannelName, RetentionPolicy)>,
        identity: Keypair,
        registration_open: bool,
        store: Arc<S>,
        blobs: Arc<dyn BlobStore>,
        dm_policy: RetentionPolicy,
        operators: impl IntoIterator<Item = Account>,
        ns_creation_open: bool,
        ns_quota: u64,
        federation: FederationConfig,
    ) -> Self
    where
        S: EventStore
            + AccountStore
            + CapabilityStore
            + ChannelStore
            + InviteStore
            + NamespaceStore
            + ReportStore
            + PeerStore
            + NetblockStore
            + ModerationStore
            + PinStore
            + EmojiStore
            + MembershipStore
            + MediaStore
            + MediaBlocklistStore
            + RoleStore
            + ProfileStore
            + NickStore
            + FriendStore
            + GroupStore
            + 'static,
    {
        let events: Arc<dyn EventStore> = store.clone();
        let media_refs: Arc<dyn MediaStore> = store.clone();
        let media_blocks: Arc<dyn MediaBlocklistStore> = store.clone();
        let accounts: Arc<dyn AccountStore> = store.clone();
        let caps: Arc<dyn CapabilityStore> = store.clone();
        let invites: Arc<dyn InviteStore> = store.clone();
        let channel_store: Arc<dyn ChannelStore> = store.clone();
        let reports: Arc<dyn ReportStore> = store.clone();
        let peers: Arc<dyn PeerStore> = store.clone();
        let netblocks: Arc<dyn NetblockStore> = store.clone();
        let moderation: Arc<dyn ModerationStore> = store.clone();
        let pins: Arc<dyn PinStore> = store.clone();
        let emoji: Arc<dyn EmojiStore> = store.clone();
        let memberships: Arc<dyn MembershipStore> = store.clone();
        let roles: Arc<dyn RoleStore> = store.clone();
        let profiles: Arc<dyn ProfileStore> = store.clone();
        let nicks: Arc<dyn NickStore> = store.clone();
        let friends: Arc<dyn FriendStore> = store.clone();
        let groups: Arc<dyn GroupStore> = store.clone();
        let namespaces: Arc<dyn NamespaceStore> = store;
        let registry = Registry::spawn(
            channels,
            info.network.clone(),
            Arc::clone(&events),
            Arc::clone(&media_refs),
            Arc::clone(&pins),
        );
        let directory = crate::directory::spawn(
            info.network.clone(),
            dm_policy,
            Arc::clone(&events),
            Arc::clone(&accounts),
        );
        Self {
            info,
            registry,
            accounts: Accounts::new(accounts),
            events,
            dm_policy,
            directory,
            registration_open,
            caps,
            invites,
            channel_store,
            namespaces,
            reports,
            ns_creation_open,
            ns_quota,
            banned_substrings: Vec::new(),
            require_email: false,
            banned_regexes: Vec::new(),
            foreign_adapters: Vec::new(),
            pending_provision: std::sync::Mutex::new(HashMap::new()),
            provision_seq: AtomicU64::new(1),
            remote_plugins: Vec::new(),
            plugin_registry: std::sync::Mutex::new(HashMap::new()),
            pending_invoke: std::sync::Mutex::new(HashMap::new()),
            view_seq: AtomicU64::new(1),
            peers,
            netblocks,
            moderation,
            pins,
            emoji,
            memberships,
            roles,
            profiles,
            nicks,
            friends,
            groups,
            presence: std::sync::Mutex::new(std::collections::HashMap::new()),
            blobs,
            media_refs,
            media_blocks,
            media: MediaRegistry::default(),
            federation,
            operators: operators.into_iter().collect(),
            identity,
            next_session: AtomicU64::new(1),
            connections: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            shutdown: tokio_util::sync::CancellationToken::new(),
            auto_bridge_tx: std::sync::OnceLock::new(),
            mirror_tx: std::sync::OnceLock::new(),
            friend_deliver_tx: std::sync::OnceLock::new(),
            bridge_out: std::sync::Mutex::new(HashMap::new()),
            backfill_tx: std::sync::OnceLock::new(),
            backfill_demand: std::sync::Mutex::new(Vec::new()),
            federate_cooldown: std::sync::Mutex::new(HashMap::new()),
            voice: std::sync::OnceLock::new(),
            voice_rooms: std::sync::Mutex::new(HashMap::new()),
            calls: std::sync::Mutex::new(HashMap::new()),
            group_calls: std::sync::Mutex::new(HashMap::new()),
            group_echoes: std::sync::Mutex::new(HashMap::new()),
            voice_relay: std::sync::OnceLock::new(),
            voice_relays: std::sync::Mutex::new(HashMap::new()),
            mailer: std::sync::OnceLock::new(),
            probe: std::sync::OnceLock::new(),
            verify_codes: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// WC7 forced logout: close every live session of `account`, returning how
    /// many were cut. Each session unwinds through its ordinary cleanup, so
    /// co-members see a normal disconnect (presence offline, voice leave) and
    /// persistent membership is retained — identical to the client's own network
    /// dropping. Suspending blocks *new* logins; this cuts the existing ones.
    pub async fn disconnect_account(&self, account: &Account) -> usize {
        self.directory.disconnect(account).await
    }

    /// weftd installs the §16 voice SFU backend (enables voice; `features=voice`).
    pub fn set_voice_backend(&self, backend: Arc<dyn crate::voice::VoiceBackend>) {
        let _ = self.voice.set(backend);
    }

    /// weftd installs the §16 M-lk-3b federated-voice relay driver.
    pub fn set_voice_relay(&self, relay: Arc<dyn crate::voice::VoiceRelay>) {
        let _ = self.voice_relay.set(relay);
    }

    /// weftd installs the §10.5 email sender.
    pub fn set_mailer(&self, mailer: Arc<dyn crate::mailer::Mailer>) {
        let _ = self.mailer.set(mailer);
    }

    pub fn set_network_probe(&self, probe: Arc<dyn crate::probe::NetworkProbe>) {
        let _ = self.probe.set(probe);
    }

    /// Whether `host`'s owner has already chosen to run a **WEFT network**
    /// there. With no probe installed this reads `false` — the local checks in
    /// `realm_refusal` still stand, and a server with no outbound HTTP simply
    /// can't consult the domain.
    pub(crate) async fn host_runs_weft(&self, host: &str) -> bool {
        match self.probe.get() {
            Some(probe) => probe.is_weft_network(host).await,
            None => false,
        }
    }

    /// §10.5 record a pending verification code (replacing any prior one for the
    /// `(account, kind)`), and mail it — if a mailer is installed. Best effort.
    pub(crate) async fn verify_send_code(
        &self,
        account: &Account,
        kind: &str,
        address: &str,
        code: String,
        expiry_ms: u64,
        purpose: &str,
    ) {
        self.verify_codes.lock().expect("verify lock").insert(
            (account.clone(), kind.to_string()),
            (code.clone(), expiry_ms),
        );
        if let Some(mailer) = self.mailer.get() {
            mailer.send_code(address, &code, purpose).await;
        }
    }

    /// §10.5 check a submitted verification code for `(account, kind)`: true iff it
    /// matches and hasn't expired at `now_ms`. Consumes the code on success (and
    /// prunes it on expiry) — a code is single-use.
    pub(crate) fn verify_check_code(
        &self,
        account: &Account,
        kind: &str,
        code: &str,
        now_ms: u64,
    ) -> bool {
        let key = (account.clone(), kind.to_string());
        let mut codes = self.verify_codes.lock().expect("verify lock");
        match codes.get(&key) {
            Some((expected, expiry)) if now_ms < *expiry && expected == code => {
                codes.remove(&key);
                true
            }
            Some((_, expiry)) if now_ms >= *expiry => {
                codes.remove(&key);
                false
            }
            _ => false,
        }
    }

    /// §16 M-lk-3b: a local participant joined a bridged conversation (a foreign
    /// voice channel, or a cross-network call) identified by `spec.key`, homed on
    /// `spec.peer`. Refcount it; on the **first** joiner start the media relay
    /// bridging the peer's LiveKit room into ours. Idempotent per participant —
    /// the caller pairs it with [`relay_release`](Self::relay_release).
    pub async fn relay_acquire(&self, spec: crate::voice::RelaySpec) {
        let key = (spec.peer.clone(), spec.key.clone());
        let first = {
            let mut relays = self.voice_relays.lock().expect("relay lock");
            let count = relays.entry(key).or_insert(0);
            *count += 1;
            *count == 1
        };
        if first {
            if let Some(driver) = self.voice_relay.get() {
                driver.start(spec).await;
            }
        }
    }

    /// §16 M-lk-3b: a local participant left. On the **last** leaver, stop the
    /// relay for `(peer, key)`.
    pub async fn relay_release(&self, peer: &NetworkName, key: &str) {
        let map_key = (peer.clone(), key.to_string());
        let last = {
            let mut relays = self.voice_relays.lock().expect("relay lock");
            match relays.get_mut(&map_key) {
                Some(count) => {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        relays.remove(&map_key);
                        true
                    } else {
                        false
                    }
                }
                None => false,
            }
        };
        if last {
            if let Some(driver) = self.voice_relay.get() {
                driver.stop(peer, key).await;
            }
        }
    }

    /// §16 M-lk-3b + invariant 7: tear down **every** relay to `peer` at once (a
    /// `BRIDGE SEVER` or `NETBLOCK` stops the peer's media immediately, regardless
    /// of local refcounts).
    pub async fn relay_drop_peer(&self, peer: &NetworkName) {
        let dropped: Vec<String> = {
            let mut relays = self.voice_relays.lock().expect("relay lock");
            let keys: Vec<_> = relays.keys().filter(|(p, _)| p == peer).cloned().collect();
            for key in &keys {
                relays.remove(key);
            }
            keys.into_iter().map(|(_, k)| k).collect()
        };
        if let Some(driver) = self.voice_relay.get() {
            for key in dropped {
                driver.stop(peer, &key).await;
            }
        }
    }

    /// The installed voice backend, or `None` on a zero-voice server (§16).
    pub(crate) fn voice_backend(&self) -> Option<&Arc<dyn crate::voice::VoiceBackend>> {
        self.voice.get()
    }

    /// §16 the current voice roster of a channel (for the join snapshot).
    pub(crate) fn voice_roster(&self, channel: &ChannelName) -> Vec<crate::voice::VoiceMember> {
        self.voice_rooms
            .lock()
            .expect("voice lock")
            .get(channel)
            .map(|room| room.values().cloned().collect())
            .unwrap_or_default()
    }

    /// §16 register a session as a voice-room member.
    pub(crate) fn voice_room_join(
        &self,
        channel: &ChannelName,
        session: u64,
        member: crate::voice::VoiceMember,
    ) {
        self.voice_rooms
            .lock()
            .expect("voice lock")
            .entry(channel.clone())
            .or_default()
            .insert(session, member);
    }

    /// §16 remove a session from a voice room (leave / disconnect). Prunes the
    /// room when empty.
    pub(crate) fn voice_room_leave(&self, channel: &ChannelName, session: u64) {
        let mut rooms = self.voice_rooms.lock().expect("voice lock");
        if let Some(room) = rooms.get_mut(channel) {
            room.remove(&session);
            if room.is_empty() {
                rooms.remove(channel);
            }
        }
    }

    /// §6.7 flip an account's mute flag in every voice room it's in and return
    /// those `(channel, session)`s — a moderator's `MUTE`/`UNMUTE` uses this to
    /// silence/resume them at the SFU + reflect it in later snapshots.
    pub(crate) fn voice_set_muted(
        &self,
        account: &Account,
        muted: bool,
    ) -> Vec<(ChannelName, u64)> {
        let mut rooms = self.voice_rooms.lock().expect("voice lock");
        let mut hits = Vec::new();
        for (channel, room) in rooms.iter_mut() {
            for (session, member) in room.iter_mut() {
                if member.account == *account {
                    member.muted = muted;
                    hits.push((channel.clone(), *session));
                }
            }
        }
        hits
    }

    /// §6.7 remove an account from one channel's voice roster (a ban/kick),
    /// returning its session id so the caller can tear down the backend peer +
    /// announce. Prunes the room when empty. `None` if they weren't in it.
    pub(crate) fn voice_eject_account(
        &self,
        channel: &ChannelName,
        account: &Account,
    ) -> Option<u64> {
        let mut rooms = self.voice_rooms.lock().expect("voice lock");
        let room = rooms.get_mut(channel)?;
        let session = room
            .iter()
            .find(|(_, member)| member.account == *account)
            .map(|(session, _)| *session)?;

        room.remove(&session);
        if room.is_empty() {
            rooms.remove(channel);
        }
        Some(session)
    }

    /// weftd installs the auto-federation dialer sink (enables `FEDERATE`).
    pub fn set_auto_bridge_sink(&self, tx: tokio::sync::mpsc::UnboundedSender<AutoBridgeRequest>) {
        let _ = self.auto_bridge_tx.set(tx);
    }

    /// §6.7 configure the banned-name filter for new usernames + namespace
    /// vanities. `substrings` match case-insensitively as substrings; `regexes`
    /// are compiled case-insensitively (an invalid pattern is dropped + warned,
    /// never a hard failure — the filter is best-effort moderation).
    pub fn with_banned_words(mut self, substrings: Vec<String>, regexes: Vec<String>) -> Self {
        self.banned_substrings = substrings
            .into_iter()
            .map(|w| w.trim().to_ascii_lowercase())
            .filter(|w| !w.is_empty())
            .collect();
        self.banned_regexes = regexes
            .into_iter()
            .filter_map(|p| {
                regex::RegexBuilder::new(p.trim())
                    .case_insensitive(true)
                    .build()
                    .map_err(
                        |e| tracing::warn!(pattern = %p, "invalid banned-word regex, skipped: {e}"),
                    )
                    .ok()
            })
            .collect();
        self
    }

    /// weftd installs the pinned foreign-bridge adapters from `[[foreign_bridge]]`
    /// config (framework §3). Each entry authorizes one key for one scheme.
    pub fn with_foreign_adapters(mut self, adapters: Vec<(weft_proto::Scheme, PublicKey)>) -> Self {
        self.foreign_adapters = adapters;
        self
    }

    /// §6.1 require a contact email at REGISTER (verify-later). weftd only turns
    /// this on when SMTP is configured — see [`Session`](crate)'s register path
    /// and the gateway exemption via [`ControlStream::is_gateway`].
    ///
    /// [`ControlStream::is_gateway`]: crate::ControlStream::is_gateway
    pub fn with_require_email(mut self, on: bool) -> Self {
        self.require_email = on;
        self
    }

    /// True if `name` matches any banned substring (case-insensitive) or regex.
    /// Used to reject REGISTER usernames + NS CREATE vanities (§6.7).
    pub(crate) fn name_is_banned(&self, name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        self.banned_substrings
            .iter()
            .any(|w| lower.contains(w.as_str()))
            || self.banned_regexes.iter().any(|re| re.is_match(name))
    }

    /// §11.10 hand a `FEDERATE` request to the dialer. `false` if auto-bridging
    /// is off (no sink) or the channel is gone.
    pub(crate) fn request_auto_bridge(&self, req: AutoBridgeRequest) -> bool {
        matches!(self.auto_bridge_tx.get(), Some(tx) if tx.send(req).is_ok())
    }

    /// weftd installs the §11.8 media-mirror sink (its bridge data-plane fetcher).
    pub fn set_mirror_sink(&self, tx: tokio::sync::mpsc::UnboundedSender<MirrorRequest>) {
        let _ = self.mirror_tx.set(tx);
    }

    /// §11.8 hand a blob-mirror request to weftd. `false` if no sink is installed.
    pub(crate) fn request_mirror(&self, req: MirrorRequest) -> bool {
        matches!(self.mirror_tx.get(), Some(tx) if tx.send(req).is_ok())
    }

    /// weftd installs the social-layer friend-delivery sink (its tunnel driver).
    pub fn set_friend_deliver_sink(&self, tx: tokio::sync::mpsc::UnboundedSender<FriendDeliver>) {
        let _ = self.friend_deliver_tx.set(tx);
    }

    /// §11.14 register a live outbound bridge's writer so local `@as` commands to
    /// `peer` route over it. Called when an outbound bridge enters `State::Bridge`.
    pub(crate) fn register_bridge_out(
        &self,
        peer: NetworkName,
        tx: tokio::sync::mpsc::Sender<String>,
    ) {
        self.bridge_out
            .lock()
            .expect("bridge_out lock")
            .insert(peer, tx);
    }

    /// §11.14 drop a bridge's writer when its session ends.
    pub(crate) fn unregister_bridge_out(&self, peer: &NetworkName) {
        self.bridge_out
            .lock()
            .expect("bridge_out lock")
            .remove(peer);
    }

    fn bridge_out_for(&self, peer: &NetworkName) -> Option<tokio::sync::mpsc::Sender<String>> {
        self.bridge_out
            .lock()
            .expect("bridge_out lock")
            .get(peer)
            .cloned()
    }

    /// Send a cross-network command to `peer`, attributed to the acting local
    /// account (§11.14). Preferred path: the peer's **persistent bridge** — tag
    /// the line `@as=<from>` and hand it to that session's writer, so its effects
    /// return as events over the same bridge (one hop, homeserver authority). If
    /// we hold no live bridge to `peer` (e.g. a friend on a never-bridged
    /// network), fall back to weftd's ephemeral tunnel driver. `false` if neither
    /// is available — the local edge is still recorded, so the peer is simply not
    /// told until a bridge exists.
    pub(crate) fn request_friend_deliver(&self, req: FriendDeliver) -> bool {
        if let Some(tx) = self.bridge_out_for(&req.peer) {
            // A command is tagged `@as=<from>`; an event (`from` = None) rides as-is.
            let line = match &req.from {
                Some(from) => match with_as_tag(&req.line, from) {
                    Some(line) => line,
                    None => return false,
                },
                None => req.line.clone(),
            };
            return tx.try_send(line).is_ok();
        }
        matches!(self.friend_deliver_tx.get(), Some(tx) if tx.send(req).is_ok())
    }

    // ---- social layer: 1:1 friend calls ----

    fn call_key(a: &UserRef, b: &UserRef) -> (UserRef, UserRef) {
        if a <= b {
            (a.clone(), b.clone())
        } else {
            (b.clone(), a.clone())
        }
    }

    /// Record a placed call, returning [`CallPlace`]. `Busy` if either party is
    /// already in a call, `Exists` if this pair already has one. `local_media` is
    /// this network's participant's pre-minted credential (cross-network) or
    /// `None` (same-network, minted at accept); `relay_leg` is the caller
    /// network's relay leg on the callee's side (⇒ relay on accept).
    pub(crate) fn call_place(
        &self,
        caller: &UserRef,
        callee: &UserRef,
        room: String,
        local_media: Option<CallMediaGrant>,
        relay_leg: Option<CallMediaGrant>,
    ) -> CallPlace {
        let mut calls = self.calls.lock().expect("calls lock");
        let key = Self::call_key(caller, callee);
        if calls.contains_key(&key) {
            return CallPlace::Exists;
        }
        let in_call = |u: &UserRef| calls.keys().any(|(a, b)| a == u || b == u);
        if in_call(caller) || in_call(callee) {
            return CallPlace::Busy;
        }
        calls.insert(
            key,
            CallInfo {
                caller: caller.clone(),
                room: room.clone(),
                active: false,
                local_media,
                relay_leg,
            },
        );
        CallPlace::Ringing(room)
    }

    /// Mark the call between `a` and `b` accepted; returns its info (room, caller).
    pub(crate) fn call_accept(&self, a: &UserRef, b: &UserRef) -> Option<CallInfo> {
        let mut calls = self.calls.lock().expect("calls lock");
        let c = calls.get_mut(&Self::call_key(a, b))?;
        c.active = true;
        Some(c.clone())
    }

    /// End (remove) the call between `a` and `b`; returns its info if one existed.
    pub(crate) fn call_end(&self, a: &UserRef, b: &UserRef) -> Option<CallInfo> {
        self.calls
            .lock()
            .expect("calls lock")
            .remove(&Self::call_key(a, b))
    }

    fn new_group_call(&self) -> GroupCallInfo {
        GroupCallInfo {
            room: format!("gcall:{}", weft_proto::Ulid::new()),
            participants: std::collections::HashSet::new(),
            host_net: None,
            host_leg: None,
        }
    }

    /// A ring arrived from a network claiming to host `group`'s call. Record it as
    /// our host (making us a spoke) and return `Some(room)` **iff a relay should be
    /// spawned now** — i.e. we already have local participants to bridge (we were
    /// an active host and yielded).
    ///
    /// Split-brain guard (simultaneous start): if we are *already* an active host
    /// (`host_net` unset, participants present), we yield **only** to a network
    /// that sorts before ours — a deterministic tiebreak, so exactly one of the
    /// two racing networks becomes the single host and the other bridges to it.
    pub(crate) fn group_call_ring(
        &self,
        group: GroupId,
        host_net: NetworkName,
        host_leg: CallMediaGrant,
    ) -> Option<String> {
        let mut calls = self.group_calls.lock().expect("group calls lock");
        let info = calls.entry(group).or_insert_with(|| self.new_group_call());
        if info.host_net.is_some() {
            return None; // already a spoke — one host is enough
        }
        if info.participants.is_empty() {
            // Fresh entry (rung before any local join): become a spoke. The relay
            // spawns when a local member joins.
            info.host_net = Some(host_net);
            info.host_leg = Some(host_leg);
            return None;
        }
        // We're an active host and another host is ringing us. Tiebreak by network
        // name: yield to a smaller one (bridging our existing room into theirs);
        // ignore a larger one (they will yield to us instead).
        if host_net < self.info.network {
            let room = info.room.clone();
            info.host_net = Some(host_net);
            info.host_leg = Some(host_leg);
            Some(room)
        } else {
            None
        }
    }

    /// Join `account` to `group`'s call on this network — starting it (minting the
    /// room) if this is the first local participant. On a spoke's first joiner,
    /// `spoke` carries the host + leg so the caller spawns the bridging relay.
    pub(crate) fn group_call_join(&self, group: GroupId, account: Account) -> GroupCallJoin {
        let mut calls = self.group_calls.lock().expect("group calls lock");
        let info = calls.entry(group).or_insert_with(|| self.new_group_call());
        let was_empty = info.participants.is_empty();
        let newly = info.participants.insert(account);
        let first = was_empty && newly;
        let spoke = if first {
            info.host_net.clone().zip(info.host_leg.clone())
        } else {
            None
        };
        GroupCallJoin {
            room: info.room.clone(),
            newly,
            first,
            spoke,
        }
    }

    /// Remove `account` from `group`'s call. `None` if they weren't in it.
    pub(crate) fn group_call_leave(
        &self,
        group: GroupId,
        account: &Account,
    ) -> Option<GroupCallLeave> {
        let mut calls = self.group_calls.lock().expect("group calls lock");
        let info = calls.get_mut(&group)?;
        if !info.participants.remove(account) {
            return None; // wasn't in the call
        }
        let room = info.room.clone();
        let empty = info.participants.is_empty();
        let host_net = info.host_net.clone();
        if empty {
            calls.remove(&group);
        }
        Some(GroupCallLeave {
            room,
            empty,
            host_net,
        })
    }

    /// A spoke poster relayed a cross-network group post: remember its session
    /// against a correlation `token`, so the home's echoed copy can be delivered
    /// as that session's own (labelled) message. Expired tokens (echo never came
    /// back within [`GROUP_ECHO_TTL_MS`]) are swept here.
    pub(crate) fn register_group_echo(&self, token: String, session: u64, now_ms: u64) {
        let mut echoes = self.group_echoes.lock().expect("group echoes lock");
        echoes.retain(|_, (_, created)| now_ms.saturating_sub(*created) < GROUP_ECHO_TTL_MS);
        echoes.insert(token, (session, now_ms));
    }

    /// Consume a group-echo correlation token → the awaiting poster's session.
    pub(crate) fn take_group_echo(&self, token: &str) -> Option<u64> {
        self.group_echoes
            .lock()
            .expect("group echoes lock")
            .remove(token)
            .map(|(session, _)| session)
    }

    /// The local accounts currently in `group`'s call (for the roster snapshot).
    pub(crate) fn group_call_participants(&self, group: GroupId) -> Vec<Account> {
        self.group_calls
            .lock()
            .expect("group calls lock")
            .get(&group)
            .map(|i| i.participants.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// weftd installs the §11.7 bridge-backfill sink (its data-plane puller).
    pub fn set_backfill_sink(&self, tx: tokio::sync::mpsc::UnboundedSender<BackfillPull>) {
        let _ = self.backfill_tx.set(tx);
    }

    /// §11.7 hand a backfill pull to weftd. `false` if no sink is installed.
    pub(crate) fn request_backfill_pull(&self, req: BackfillPull) -> bool {
        matches!(self.backfill_tx.get(), Some(tx) if tx.send(req).is_ok())
    }

    /// §11.7 an outbound bridge session registers its demand inbox so local
    /// clients can trigger on-demand backfill from its peer.
    pub(crate) fn register_backfill_demand(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<BackfillReq>,
    ) {
        self.backfill_demand.lock().expect("backfill lock").push(tx);
    }

    /// §11.7 a local client ran out of local scrollback for `channel`: ask every
    /// outbound bridge to fetch that window (each ignores channels it doesn't
    /// forward). No-op with no bridges. Closed inboxes are pruned here.
    pub(crate) fn request_channel_backfill(&self, req: BackfillReq) {
        self.backfill_demand
            .lock()
            .expect("backfill lock")
            .retain(|tx| tx.send(req.clone()).is_ok());
    }

    /// §11.10 per-account cooldown: at most one `FEDERATE` per window.
    pub(crate) fn federate_allowed(&self, account: &Account) -> bool {
        const COOLDOWN: std::time::Duration = std::time::Duration::from_secs(3);
        let now = std::time::Instant::now();
        let mut recent = self.federate_cooldown.lock().expect("cooldown lock");
        match recent.get(account) {
            Some(&last) if now.duration_since(last) < COOLDOWN => false,
            _ => {
                recent.insert(account.clone(), now);
                true
            }
        }
    }

    // ---- §11 federation ----

    /// The pinned signing key for a configured peer network, if any. Bridge
    /// sessions authenticate against this (§11.2).
    pub(crate) fn peer_key(&self, network: &NetworkName) -> Option<&PublicKey> {
        self.federation.peer_keys.get(network)
    }

    pub(crate) fn bridge_auto_accept(&self) -> bool {
        self.federation.auto_accept
    }

    /// Open-federation mode: accept a bridge from any non-blocked network,
    /// trusting the key it proves control of (§11.2, trust-on-first-use).
    pub(crate) fn bridge_accept_any(&self) -> bool {
        self.federation.accept_any
    }

    // ---- foreign-bridge framework (§3) / unified providers (plugin-spec §18) ----

    /// The provider id a `[[foreign_bridge]]` pinned key authenticates as: the
    /// (first) scheme it is pinned for. Post-unification a bridge adapter is a
    /// provider like any plugin; its natural id is its scheme ("matrix").
    pub(crate) fn foreign_adapter_id(&self, key: &PublicKey) -> Option<String> {
        self.foreign_adapters
            .iter()
            .find(|(_, k)| k == key)
            .map(|(s, _)| s.as_str().to_string())
    }

    /// Whether a proven provider key is authorized to speak for `scheme` — the
    /// gate on `REALM REGISTER`/`REALM ASSERT` and `Registration.schemes` (§18
    /// capability 6). Authorized by a `[[foreign_bridge]]` pin for the scheme, or
    /// a `[[plugin.remote]]` entry carrying it in `schemes`.
    pub(crate) fn scheme_authorized(&self, key: &PublicKey, scheme: &weft_proto::Scheme) -> bool {
        self.foreign_adapters
            .iter()
            .any(|(s, k)| s == scheme && k == key)
            || self
                .remote_plugins
                .iter()
                .any(|(_, k, schemes)| k == key && schemes.contains(scheme))
    }

    /// §3.3 first contact with a foreign space: if a provider handles `scheme`,
    /// mint a job, park the requester (`reply` = its raw-line writer, `label` =
    /// the join's label), and push `PROVISION <uri> <job>` to that provider.
    /// `false` if no provider handles the scheme (→ the caller answers
    /// NO-SUCH-TARGET, uniform with a nonexistent local namespace).
    pub(crate) fn begin_provision(
        &self,
        scheme: weft_proto::Scheme,
        uri: &weft_proto::ForeignUri,
        reply: tokio::sync::mpsc::Sender<String>,
        label: Option<String>,
        account: Account,
    ) -> bool {
        let Some((provider, control)) = self.provider_for_scheme(&scheme) else {
            return false;
        };

        let job = format!(
            "p{}",
            self.provision_seq
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let line = match weft_proto::Reply::new(weft_proto::Event::Provision {
            uri: uri.clone(),
            job: job.clone(),
        })
        .serialize()
        {
            Ok(line) => line,
            Err(_) => return false,
        };

        // Park before sending so a fast PROVISION reply can never race ahead of
        // the pending entry; roll back if the control link is gone.
        self.pending_provision
            .lock()
            .expect("pending_provision lock")
            .insert(
                job.clone(),
                PendingProvision {
                    reply,
                    label,
                    provider,
                    uri: uri.clone(),
                    account,
                },
            );

        if control.try_send(line).is_err() {
            self.pending_provision
                .lock()
                .expect("pending_provision lock")
                .remove(&job);
            return false;
        }

        true
    }

    /// §3.3 remove + return a parked provisioning request by job token: its
    /// requester writer + the join's label. `None` for an unknown/expired job.
    pub(crate) fn complete_provision(&self, job: &str) -> Option<PendingProvision> {
        self.pending_provision
            .lock()
            .expect("pending_provision lock")
            .remove(job)
    }

    /// A provider session died with work in flight: fail every request parked on
    /// it, so no client hangs waiting on a reply that can never come (§3.5 label
    /// discipline — silence must never be the failure mode). Parked provisions
    /// answer `NO-SUCH-TARGET` (uniform with an unreachable space, invariant 1);
    /// parked invocations answer `INTERNAL` (spec §16: disconnect mid-flow).
    pub(crate) fn fail_provider_pending(&self, provider: &str) {
        let err_line = |code: ErrCode, text: &str, label: Option<String>| {
            let event = weft_proto::Event::Err(weft_proto::ErrEvent::new(code, text));
            let reply = match label {
                Some(label) => weft_proto::Reply::with_label(event, label),
                None => weft_proto::Reply::new(event),
            };
            reply.serialize().ok()
        };

        let provisions: Vec<PendingProvision> = {
            let mut map = self
                .pending_provision
                .lock()
                .expect("pending_provision lock");
            let jobs: Vec<String> = map
                .iter()
                .filter(|(_, p)| p.provider == provider)
                .map(|(job, _)| job.clone())
                .collect();
            jobs.into_iter().filter_map(|j| map.remove(&j)).collect()
        };

        for p in provisions {
            if let Some(line) = err_line(ErrCode::NoSuchTarget, "no such target", p.label) {
                let _ = p.reply.try_send(line);
            }
        }

        // Invocation view-ids are minted as `<provider>:<seq>` (§11.1).
        let prefix = format!("{provider}:");
        let invokes: Vec<PendingReply> = {
            let mut map = self.pending_invoke.lock().expect("pending_invoke lock");
            let keys: Vec<String> = map
                .keys()
                .filter(|k| k.starts_with(&prefix))
                .cloned()
                .collect();
            keys.into_iter().filter_map(|k| map.remove(&k)).collect()
        };

        for (reply, label) in invokes {
            if let Some(line) = err_line(ErrCode::Internal, "plugin disconnected mid-flow", label) {
                let _ = reply.try_send(line);
            }
        }
    }

    // ---- plugin system (plugin-spec.md §4.2, §11–§12) ----

    /// weftd installs the pinned remote plugins from `[[plugin.remote]]` config:
    /// `(id, key, authorized schemes)`.
    pub fn with_remote_plugins(
        mut self,
        plugins: Vec<(String, PublicKey, Vec<weft_proto::Scheme>)>,
    ) -> Self {
        self.remote_plugins = plugins;
        self
    }

    /// The `[[plugin.remote]]` id a pinned key authenticates as, if any (§4.2).
    pub(crate) fn remote_plugin_key(&self, key: &PublicKey) -> Option<String> {
        self.remote_plugins
            .iter()
            .find(|(_, k, _)| k == key)
            .map(|(id, _, _)| id.clone())
    }

    /// §12.3 register a connected provider's writer + declared actions + schemes.
    /// A `REALM REGISTER` that preceded the `PLUGIN-REGISTER` already recorded
    /// schemes — those are merged (union), so ordering between the two never
    /// drops a scheme.
    pub(crate) fn register_plugin(
        &self,
        id: String,
        out: tokio::sync::mpsc::Sender<String>,
        name: String,
        icon: Option<String>,
        actions: Vec<weft_proto::ActionDecl>,
        mut schemes: Vec<weft_proto::Scheme>,
    ) {
        let mut map = self.plugin_registry.lock().expect("plugin_registry lock");

        if let Some(prior) = map.get(&id) {
            for s in &prior.schemes {
                if !schemes.contains(s) {
                    schemes.push(s.clone());
                }
            }
        }

        map.insert(
            id,
            PluginReg {
                out,
                name,
                icon,
                actions,
                schemes,
            },
        );
    }

    /// `REALM REGISTER` (§3.3): add a scheme to a provider's registry entry,
    /// creating a minimal entry (no actions yet) if the provider has not sent
    /// `PLUGIN-REGISTER`.
    pub(crate) fn add_provider_scheme(
        &self,
        id: &str,
        scheme: weft_proto::Scheme,
        out: tokio::sync::mpsc::Sender<String>,
    ) {
        let mut map = self.plugin_registry.lock().expect("plugin_registry lock");

        let entry = map.entry(id.to_string()).or_insert_with(|| PluginReg {
            out,
            name: id.to_string(),
            icon: None,
            actions: Vec::new(),
            schemes: Vec::new(),
        });

        if !entry.schemes.contains(&scheme) {
            entry.schemes.push(scheme);
        }
    }

    /// Whether a provider currently serves `scheme` (connected + registered).
    pub(crate) fn scheme_online(&self, scheme: &weft_proto::Scheme) -> bool {
        let map = self.plugin_registry.lock().expect("plugin_registry lock");

        map.values().any(|r| r.schemes.contains(scheme))
    }

    /// Whether a *different* provider already serves `scheme` — first registrant
    /// holds a scheme until it disconnects (3b-c: deterministic routing, never
    /// HashMap-order luck between two claimants).
    pub(crate) fn scheme_held_by_other(&self, scheme: &weft_proto::Scheme, id: &str) -> bool {
        let map = self.plugin_registry.lock().expect("plugin_registry lock");

        map.iter()
            .any(|(other, r)| other != id && r.schemes.contains(scheme))
    }

    /// A namespace's provider liveness (owner directive 2026-08-04: a virtual
    /// namespace is online only while its provider is): `None` for a native
    /// namespace, else whether the origin's scheme has a live provider. An
    /// unparseable stored origin reads as offline (never produced by our own
    /// writers).
    pub(crate) fn origin_online(&self, record: &weft_store::NamespaceRecord) -> Option<bool> {
        let origin = record.origin.as_deref()?;
        let online = origin
            .parse::<weft_proto::ForeignUri>()
            .map(|uri| self.scheme_online(uri.scheme()))
            .unwrap_or(false);

        Some(online)
    }

    /// The schemes a registered provider currently serves (for its liveness
    /// fan-out on disconnect).
    pub(crate) fn provider_schemes(&self, id: &str) -> Vec<weft_proto::Scheme> {
        let map = self.plugin_registry.lock().expect("plugin_registry lock");

        map.get(id).map(|r| r.schemes.clone()).unwrap_or_default()
    }

    /// The writer of the provider that bridges `realm`, if any — a realm is a
    /// network (framework §7a.0), so this answers "is this 'network' actually a
    /// bridged realm?" and is what routes a DM to a provider rather than to a
    /// peer WEFT network.
    pub(crate) async fn provider_for_realm(
        &self,
        realm: &str,
    ) -> Option<tokio::sync::mpsc::Sender<String>> {
        let namespaces = self.namespaces.namespaces_with_origin().await.ok()?;
        let uri = namespaces.iter().find_map(|record| {
            record
                .origin
                .as_deref()
                .and_then(|o| o.parse::<weft_proto::ForeignUri>().ok())
                .filter(|uri| uri.realm() == realm)
        })?;

        self.provider_for_scheme(uri.scheme()).map(|(_, out)| out)
    }

    /// The id + writer of the provider handling `scheme`, if any (§18 cap. 6).
    pub(crate) fn provider_for_scheme(
        &self,
        scheme: &weft_proto::Scheme,
    ) -> Option<(String, tokio::sync::mpsc::Sender<String>)> {
        let map = self.plugin_registry.lock().expect("plugin_registry lock");

        map.iter()
            .find(|(_, r)| r.schemes.contains(scheme))
            .map(|(id, r)| (id.clone(), r.out.clone()))
    }

    /// Drop a plugin's registration when its session ends — only if `out` still
    /// owns the slot (a reconnected plugin that took over is left in place).
    pub(crate) fn unregister_plugin(&self, id: &str, out: &tokio::sync::mpsc::Sender<String>) {
        let mut map = self.plugin_registry.lock().expect("plugin_registry lock");

        if map.get(id).is_some_and(|r| r.out.same_channel(out)) {
            map.remove(id);
        }
    }

    /// The client-facing action catalog of every registered plugin (§12.5).
    pub(crate) fn plugin_catalog(&self) -> weft_proto::Catalog {
        let map = self.plugin_registry.lock().expect("plugin_registry lock");

        weft_proto::Catalog {
            plugins: map
                .iter()
                .map(|(id, r)| weft_proto::CatalogEntry {
                    plugin_id: id.clone(),
                    name: r.name.clone(),
                    icon: r.icon.clone(),
                    actions: r.actions.clone(),
                })
                .collect(),
        }
    }

    /// A registered plugin's writer, if it declares `action` (§12.4 routing). A
    /// client `PLUGIN INVOKE` names the plugin, so this both routes and validates
    /// the action exists — `None` ⇒ NO-SUCH-TARGET (anti-enumeration).
    pub(crate) fn plugin_out_for(
        &self,
        plugin: &str,
        action: &str,
    ) -> Option<tokio::sync::mpsc::Sender<String>> {
        let map = self.plugin_registry.lock().expect("plugin_registry lock");

        map.get(plugin)
            .filter(|r| r.actions.iter().any(|a| a.id == action))
            .map(|r| r.out.clone())
    }

    /// Mint a per-session view-id for an invocation (§11.1).
    pub(crate) fn mint_view_id(&self, plugin: &str) -> String {
        format!(
            "{plugin}:{}",
            self.view_seq
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )
    }

    /// Park a client invocation awaiting the plugin's response, keyed by view-id.
    pub(crate) fn park_invoke(
        &self,
        view_id: String,
        reply: tokio::sync::mpsc::Sender<String>,
        label: Option<String>,
    ) {
        self.pending_invoke
            .lock()
            .expect("pending_invoke lock")
            .insert(view_id, (reply, label));
    }

    /// Remove + return a parked invocation's requester writer + label by view-id
    /// (a terminal `PLUGIN-RESULT`).
    pub(crate) fn complete_invoke(&self, view_id: &str) -> Option<PendingReply> {
        self.pending_invoke
            .lock()
            .expect("pending_invoke lock")
            .remove(view_id)
    }

    /// Clone (but keep) a parked invocation's requester writer + label — for a
    /// non-terminal `PLUGIN-VIEW`/`PLUGIN-PATCH` that continues the flow (§11.2).
    pub(crate) fn peek_invoke(&self, view_id: &str) -> Option<PendingReply> {
        self.pending_invoke
            .lock()
            .expect("pending_invoke lock")
            .get(view_id)
            .map(|(tx, label)| (tx.clone(), label.clone()))
    }

    /// Sign a manifest with this network's key (§11.3). The §11.3 authority
    /// ladder is enforced *locally* before calling this (does the operator
    /// hold `bridge`/ns-owner/`*`?); the wire artifact is uniformly
    /// network-key-signed so the peer can verify it against our well-known.
    pub(crate) fn sign_manifest(&self, manifest: &weft_crypto::Manifest) -> String {
        manifest.sign(&self.identity).to_b64()
    }

    /// Our own network name as the validated type (manifests name their peer).
    pub(crate) fn network(&self) -> &NetworkName {
        &self.info.network
    }

    /// §16 sign a voice-relay grant with our network key (like `sign_manifest`),
    /// so the grantee can verify our authorization against our well-known key.
    pub(crate) fn sign_voice_relay(
        &self,
        grant: &weft_crypto::VoiceRelayGrant,
    ) -> weft_crypto::SignedVoiceRelayGrant {
        grant.sign(&self.identity)
    }

    /// §10.3 sign a display profile with our network key so a remote can verify a
    /// federated user's profile against our well-known key (like manifests).
    pub(crate) fn sign_profile(&self, profile: &Profile) -> SignedProfile {
        profile.sign(&self.identity)
    }

    /// §10.3 store a federated user's profile received over a bridge (already
    /// signature-verified by the caller), keyed by its `user@network` handle.
    pub(crate) async fn store_federated_profile(
        &self,
        handle: &str,
        record: weft_store::ProfileRecord,
    ) {
        let _ = self.profiles.set_profile(handle, record).await;
    }

    // ---- capability enforcement (§10.4, invariant 4) ----

    /// The grant-store key for an actor: a local account's immutable **ULID**
    /// (§10.4), or a foreign `account@network` verbatim. `None` = unknown local
    /// account (holds no grants).
    pub(crate) async fn actor_store_key(
        &self,
        actor: &Actor,
    ) -> Result<Option<String>, StoreError> {
        match actor {
            Actor::Local(account) => self.accounts.account_ulid(account).await,
            Actor::Foreign(user) => Ok(Some(user.clone())),
        }
    }

    /// Does `actor` hold `cap` for an object at `scope`? Operators hold
    /// everything at `*`; a namespace owner holds everything in their namespace;
    /// everyone else's authority comes from grants that cover the scope, are
    /// unexpired, and are at or above the scope's current revocation epoch.
    /// Operator/owner authority is **local-only** — a federated actor (§10.4,
    /// homeserver authority) is never a local operator or owner, so her power on
    /// H comes purely from what H granted `account@network`.
    pub(crate) async fn actor_has_cap(
        &self,
        actor: &Actor,
        cap: &Capability,
        scope: &TokenScope,
        now: u64,
    ) -> Result<bool, StoreError> {
        if let Actor::Local(account) = actor {
            // Operator authority is **network-level only**: `*` (netblock,
            // network-wide moderation, `*`-scope grants) and top-level network
            // channels the operator runs. It is deliberately NOT extended into a
            // user's namespace (`ns:<name>` / `#ns/chan`) — there an operator is
            // an ordinary member, never the implicit admin of someone else's
            // server. Cross-namespace intervention is an out-of-band act through
            // the web admin panel (store-direct), and recovery rung 3 is
            // network-key-signed, so neither depends on this. (§10.4; decision:
            // operator ≠ day-to-day control of user servers.)
            if scope_namespace(scope).is_none()
                && (self.operators.contains(account) || self.accounts.is_operator(account).await?)
            {
                return Ok(true);
            }
            if let Some(ns_id) = scope_namespace(scope) {
                if let Some(ns) = self.namespaces.namespace_by_id(&ns_id).await? {
                    // The owner-shortcut is GATED on a native namespace: a
                    // provider-managed (origin-marked) replica is governed by its
                    // provider, never by a local owner account — the sentinel
                    // owner (or a name-colliding user) confers nothing (§7,
                    // Phase-0 decision 2026-08-04).
                    if ns.origin.is_none() && ns.owner == *account {
                        return Ok(true);
                    }
                    // The **operator escape hatch** (3b, 2026-08-04): a
                    // provider-managed namespace has NO local owner, so the
                    // "operator ≠ admin of someone else's server" principle does
                    // not apply — it is server infrastructure, and without this
                    // an orphaned replica would be undeletable by anyone.
                    if ns.origin.is_some()
                        && (self.operators.contains(account)
                            || self.accounts.is_operator(account).await?)
                    {
                        return Ok(true);
                    }
                }
            }
        }
        let Some(key) = self.actor_store_key(actor).await? else {
            return Ok(false);
        };
        for grant in self.caps.grants_for(&key).await? {
            let Some(gscope) = TokenScope::parse(&grant.scope) else {
                continue;
            };
            if !gscope.covers(scope) {
                continue;
            }
            if grant.expiry.is_some_and(|e| now >= e) {
                continue;
            }
            if grant.epoch < self.caps.scope_epoch(&grant.scope).await? {
                continue;
            }
            if grant
                .caps
                .iter()
                .filter_map(|c| c.parse::<Capability>().ok())
                .any(|c| &c == cap)
            {
                return Ok(true);
            }
        }
        // Baseline: a namespace's implicit `@everyone` role grants its caps to
        // every same-network **member** — no explicit assignment or grant. Only
        // local members (never a federated actor, who holds exactly what this
        // network granted `account@F`, §11.11). Resolved live here rather than
        // materialized, so shrinking @everyone's caps takes effect immediately.
        // A channel additionally honors its own `@everyone` role: the
        // per-channel baseline set from the channel-permission editor.
        if let Actor::Local(account) = actor {
            if let Some(ns_name) = scope_namespace(scope) {
                if self
                    .memberships
                    .is_ns_member(account.as_str(), &ns_name)
                    .await?
                {
                    // Widest first (`ns:`), then the channel's own baseline.
                    let mut baseline_scopes = vec![format!("ns:{ns_name}")];
                    if let TokenScope::Channel(chan) = scope {
                        baseline_scopes.push(chan.clone());
                    }
                    for base_scope in baseline_scopes {
                        for role in self.roles.roles(&base_scope).await? {
                            if role.name != EVERYONE_ROLE {
                                continue;
                            }
                            if role
                                .caps
                                .iter()
                                .filter_map(|c| c.parse::<Capability>().ok())
                                .any(|c| &c == cap)
                            {
                                return Ok(true);
                            }
                        }
                    }
                }
            }
        }
        Ok(false)
    }

    /// Local-account convenience wrapper over [`Self::actor_has_cap`].
    pub(crate) async fn account_has_cap(
        &self,
        account: &Account,
        cap: &Capability,
        scope: &TokenScope,
        now: u64,
    ) -> Result<bool, StoreError> {
        self.actor_has_cap(&Actor::Local(account.clone()), cap, scope, now)
            .await
    }

    /// May `actor` delegate `cap` at `scope`? (Holds `grant:<cap>`, or is a local
    /// operator.)
    pub(crate) async fn actor_can_grant(
        &self,
        actor: &Actor,
        cap: &Capability,
        scope: &TokenScope,
        now: u64,
    ) -> Result<bool, StoreError> {
        if let Actor::Local(account) = actor {
            if self.operators.contains(account) || self.accounts.is_operator(account).await? {
                return Ok(true);
            }
        }
        let grant_cap = Capability::Grant(Box::new(cap.clone()));
        self.actor_has_cap(actor, &grant_cap, scope, now).await
    }

    /// Resolve a GRANT/REVOKE subject string to its stable identity: a device
    /// key, a local account's **ULID**, or a foreign `account@network` (§10.4).
    /// Returns the typed token `Subject` and the string the grant store keys by
    /// (they always agree). `None` for an account name with no such account —
    /// you can't grant to an identity that doesn't exist.
    pub(crate) async fn resolve_subject(
        &self,
        s: &str,
    ) -> Result<Option<(Subject, String)>, StoreError> {
        if let Ok(key) = PublicKey::from_b64(s) {
            return Ok(Some((Subject::Key(key), s.to_string())));
        }
        if s.contains('@') {
            return Ok(Some((Subject::Foreign(s.to_string()), s.to_string())));
        }
        let Ok(account) = s.parse::<Account>() else {
            return Ok(None);
        };
        match self.accounts.account_ulid(&account).await? {
            Some(ulid_str) => match weft_proto::Ulid::from_string(&ulid_str) {
                Ok(ulid) => Ok(Some((Subject::Account(ulid), ulid_str))),
                Err(_) => Ok(None),
            },
            None => Ok(None),
        }
    }

    /// Mint a network-key-signed token for a `*`/`#chan`-scoped grant
    /// (§11.3). `ns:` scopes need the namespace root key (M4b).
    pub(crate) fn mint_token(
        &self,
        subject: Subject,
        scope: TokenScope,
        caps: Vec<Capability>,
        epoch: u64,
        expiry: u64,
    ) -> String {
        Grant {
            issuer: self.identity.public(),
            subject,
            scope,
            caps,
            epoch,
            expiry,
            parent: None,
        }
        .sign(&self.identity)
        .to_b64()
    }

    /// The public signing key, for `/.well-known/weft` (§10.2).
    pub fn identity_public(&self) -> PublicKey {
        self.identity.public()
    }

    // ---- §13 media data-plane tokens (bytes ride the data plane in weftd) ----

    /// Mint a one-time upload grant (from `STREAM OFFER`); returns its token.
    pub(crate) fn mint_upload_token(
        &self,
        account: Account,
        mime: String,
        max_bytes: u64,
    ) -> String {
        self.media.mint_upload(UploadGrant {
            account,
            mime,
            max_bytes,
        })
    }

    /// Consume an upload grant if valid — called by weftd's data-plane handler
    /// before it accepts bytes.
    pub fn take_upload_token(&self, token: &str) -> Option<UploadGrant> {
        self.media.take_upload(token)
    }

    /// Mint a fetch bearer for an account (M-media-0: a valid bearer = may
    /// fetch; per-blob membership-gating is M-media-1).
    pub fn mint_media_bearer(&self, account: Account) -> String {
        self.media.mint_bearer(account)
    }

    /// The account a fetch bearer authorizes, if the token is valid.
    pub fn media_bearer_account(&self, token: &str) -> Option<Account> {
        self.media.bearer_account(token)
    }

    /// §6/§13 mint a one-time backfill grant holding a pre-serialized `BATCH`;
    /// returns its token (pulled once via `BACKFILL <token>` on the data plane).
    pub(crate) fn mint_backfill_token(&self, body: Vec<u8>) -> String {
        self.media.mint_backfill(body)
    }

    /// Consume a backfill grant if valid — called by weftd's data-plane handler.
    pub fn take_backfill_token(&self, token: &str) -> Option<Vec<u8>> {
        self.media.take_backfill(token)
    }

    /// §13 moderation gate: is this blob hash blocked (M-media-5)? Consulted on
    /// every upload/fetch/mirror path, so a blocked hash is dead on arrival and
    /// re-uploads can't evade it (content = identity). A store error fails
    /// closed-open (treated as not-blocked) but is logged by the store.
    pub async fn is_blob_blocked(&self, hash: &str) -> bool {
        self.media_blocks
            .is_hash_blocked(hash)
            .await
            .unwrap_or(false)
    }

    /// §13 block a media hash: delete its bytes + its derived thumbnail, forget
    /// the blob records, and record the block so re-upload/mirror are rejected.
    /// Returns the reason echoed back. Idempotent.
    pub(crate) async fn block_media_hash(
        &self,
        hash: &str,
        reason: Option<String>,
        actor: &Account,
    ) -> Result<(), StoreError> {
        self.media_blocks
            .block_hash(weft_store::MediaBlockRecord {
                hash: hash.to_string(),
                reason,
                added_ms: now_ms(),
                actor: actor.to_string(),
            })
            .await?;
        // Delete the bytes + a derived thumbnail (its own blob), and forget the
        // records so the GC + fetch gate see them gone. Best-effort: the block is
        // authoritative even if a delete lags.
        let thumb = self
            .media_refs
            .blob_meta(hash)
            .await
            .ok()
            .flatten()
            .and_then(|m| m.thumb);
        for h in std::iter::once(hash.to_string()).chain(thumb) {
            if let Some(parsed) = weft_store::BlobHash::parse(&h) {
                let _ = self.blobs.delete(&parsed).await;
            }
            let _ = self.media_refs.forget_blob(&h).await;
        }
        Ok(())
    }

    /// §13 membership-gated fetch: may `account` fetch blob `hash`? Allowed iff a
    /// scope referencing it has the account as a member (channel) or participant
    /// (DM). A gated/absent blob is uniformly "not found" to the caller
    /// (invariant 1). *(The `view`-cap path for non-members is a follow-up.)*
    pub async fn may_fetch(&self, account: &Account, hash: &str) -> bool {
        // §10.3 avatars are semi-public — any authed session may fetch one.
        if self.profiles.avatar_exists(hash).await.unwrap_or(false) {
            return true;
        }
        let Ok(scopes) = self.media_refs.blob_scopes(hash).await else {
            return false;
        };
        if scopes.is_empty() {
            return false;
        }
        let mine = self
            .memberships
            .memberships(account)
            .await
            .unwrap_or_default();
        scopes.iter().any(|scope| match scope {
            weft_store::Scope::Channel(channel) => mine.contains(channel),
            weft_store::Scope::Dm(a, b) => a == account.as_str() || b == account.as_str(),
            // Group membership is answered by the GroupStore, not this
            // channel-membership index (group messaging = next increment).
            weft_store::Scope::Group(_) => false,
        })
    }

    /// Issue a device attestation for a just-verified session (§6.1, §10.2).
    pub(crate) fn mint_attestation(
        &self,
        account: &Account,
        device: PublicKey,
        now: u64,
    ) -> Attestation {
        Attestation::sign(
            &self.identity,
            device,
            account.as_str(),
            self.info.network.as_str(),
            now + ATTESTATION_TTL_SECS,
        )
    }

    pub(crate) fn network_name(&self) -> &str {
        self.info.network.as_str()
    }

    /// This network's signing public key — a peer pins this to verify our
    /// manifests + bridge auth (§11.2). Safe to expose; the private half stays in.
    pub fn network_public(&self) -> weft_crypto::PublicKey {
        self.identity.public()
    }

    /// Operator accounts — the net-scope (`*`) report handlers (§6.7). The
    /// union of the config `[operators]` seed and the DB-backed flags (§10.4).
    pub(crate) async fn operator_accounts(&self) -> Vec<Account> {
        let mut ops: std::collections::HashSet<Account> = self.operators.iter().cloned().collect();
        if let Ok(db) = self.accounts.list_operators().await {
            ops.extend(db);
        }
        ops.into_iter().collect()
    }

    pub(crate) fn next_session_id(&self) -> u64 {
        self.next_session.fetch_add(1, Ordering::Relaxed)
    }
}

/// Wall-clock unix ms — the block timestamp (§13).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The scopes a channel's moderation checks consult, widest-covering last: the
/// channel itself, its namespace (if any), and `*`. A mute/ban at any of these
/// covers the channel, so a `*`-scope record is a network-wide action and an
/// `ns:` one is namespace-wide (§6.7).
pub(crate) fn covering_scopes(channel: &ChannelName) -> Vec<String> {
    let mut scopes = vec![channel.to_string()];
    if let Some(ns) = channel_namespace(channel) {
        scopes.push(format!("ns:{ns}"));
    }
    scopes.push("*".to_string());
    scopes
}

/// The namespace a channel belongs to, if any (`#n/chan` → n). Top-level
/// channels (`#general`) have none — they answer to the operator (§2.1).
pub(crate) fn channel_namespace(channel: &ChannelName) -> Option<NamespaceName> {
    channel
        .as_str()
        .strip_prefix('#')?
        .split_once('/')?
        .0
        .parse()
        .ok()
}

/// The reserved role name every namespace member implicitly holds — the
/// baseline-permission role (Discord's `@everyone`). Editable like any role, but
/// never assigned or deleted; its caps are resolved live in `actor_has_cap`.
pub(crate) const EVERYONE_ROLE: &str = "everyone";

/// The namespace **id** a scope belongs to, if any: `ns:<id>` → id;
/// `#<ns-id>/chan` → the ns-id (a channel names its namespace id in its first
/// segment, §2.1 / v0.13). Validated as a well-formed [`NamespaceId`].
fn scope_namespace(scope: &TokenScope) -> Option<String> {
    let id = match scope {
        TokenScope::Namespace(n) => n.clone(),
        TokenScope::Channel(c) => c.strip_prefix('#')?.split_once('/')?.0.to_string(),
        TokenScope::Wildcard => return None,
    };
    id.parse::<weft_proto::NamespaceId>().ok()?;
    Some(id)
}
