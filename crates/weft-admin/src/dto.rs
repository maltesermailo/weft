//! Typed response bodies for the admin JSON API. Every handler serializes one
//! of these (or a `Vec`), so the wire shape is a named contract rather than an
//! ad-hoc `json!` — the SPA and any future API client read exactly these keys.
//! Field names and shapes are the API; don't rename them without versioning.

use serde::Serialize;
use weft_store::{
    AuditRecord, HistoryItem, MediaBlockRecord, ModRecord, NamespaceRecord, NetblockRecord,
    PeerRecord, ReactionSummary, ReportRecord,
};

/// `GET /me` — who the session belongs to, and the admin scopes they hold
/// (WC2). The SPA hides actions whose scope is absent; the server enforces
/// regardless.
#[derive(Serialize)]
pub struct Me {
    pub account: String,
    pub network: String,
    pub scopes: Vec<String>,
    /// True **operator** authority (config seed or DB flag) — not merely holding
    /// every scope, which a delegated `admin.*` grant also confers. Only an
    /// operator may change other accounts' permissions, so the SPA keys the
    /// permission controls off this rather than off the scope set.
    pub operator: bool,
}

/// `GET /stats` — dashboard counters. `live_connections` is `None` standalone
/// (a separate process can't see the count).
#[derive(Serialize)]
pub struct Stats {
    pub accounts: usize,
    pub channels: usize,
    pub namespaces: usize,
    pub open_reports: usize,
    pub peers: usize,
    pub netblocks: usize,
    pub blocked_media: usize,
    pub live_connections: Option<usize>,
}

/// One row of `GET /accounts` — the enriched account list.
#[derive(Serialize)]
pub struct AccountSummary {
    pub account: String,
    pub ulid: String,
    pub operator: bool,
    pub caps: Vec<String>,
    pub muted: bool,
    pub banned: bool,
    /// WC3: scheduled hard-delete time (ms) when pending deletion, else `None`.
    pub deletion_scheduled: Option<u64>,
    /// WC7: the account is suspended (blocked from authenticating).
    pub suspended: bool,
}

/// `DELETE /accounts/:name` response — the account was scheduled for deletion
/// (WC3 soft delete), finalized at `purge_at` unless restored first.
#[derive(Serialize)]
pub struct DeletionScheduled {
    pub purge_at: u64,
}

/// A capability grant. `subject` is present in the scope-wide `GET /grants`
/// listing and omitted in an account's own detail (it's implied).
#[derive(Serialize)]
pub struct Grant {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub scope: String,
    pub caps: Vec<String>,
    pub epoch: u64,
    pub expiry: Option<u64>,
}

/// A verification claim (email/age/…) — the value lives in `subject`.
#[derive(Serialize)]
pub struct Verification {
    pub kind: String,
    pub subject: String,
    pub verified: bool,
}

/// `GET /accounts/:name` — one account's full operator view.
#[derive(Serialize)]
pub struct AccountDetail {
    pub account: String,
    pub ulid: String,
    pub operator: bool,
    pub grants: Vec<Grant>,
    pub memberships: Vec<String>,
    pub verifications: Vec<Verification>,
    pub muted: bool,
    pub banned: bool,
    /// WC3: scheduled hard-delete time (ms) when pending deletion, else `None`.
    pub deletion_scheduled: Option<u64>,
    /// WC7: the account is suspended (blocked from authenticating).
    pub suspended: bool,
    /// WC4: enrolled device fingerprints (truncated hex of the Ed25519 pubkey).
    pub devices: Vec<String>,
    /// WC4 "find related": other accounts sharing this account's email domain
    /// (empty when it has no email claim). The spam-wave pivot.
    pub related: Vec<String>,
}

/// `GET /channels/:name` — channel lookup detail (WC4). Members are the
/// persistent roster (§6.3), offline members included.
#[derive(Serialize)]
pub struct ChannelDetail {
    pub name: String,
    pub policy: String,
    pub members: Vec<String>,
    /// WC7 posting state: `frozen` refuses everyone but `ns-admin`; `restricted`
    /// delegates posting to the `send` capability. Independent — both can hold.
    pub frozen: bool,
    pub restricted: bool,
}

/// `GET /dms/:a/:b/messages` — a DM thread browse (WC4, §0 content boundary).
/// `unavailable` is `true` when the DM policy is `e2ee`: no plaintext is held
/// or materialized (invariant 8), so `messages` is empty.
#[derive(Serialize)]
pub struct ThreadBrowse {
    pub participants: [String; 2],
    pub policy: String,
    pub unavailable: bool,
    pub messages: Vec<Msg>,
}

/// One row of `GET /accounts/:name/messages`.
#[derive(Serialize)]
pub struct AccountMessage {
    pub msgid: String,
    pub scope: String,
    pub sender: String,
    pub body: String,
    pub at_ms: u64,
}

/// One row of `GET /channels`.
#[derive(Serialize)]
pub struct Channel {
    pub name: String,
    pub policy: String,
}

/// One row of `GET /moderation` — a mute/ban deny record.
#[derive(Serialize)]
pub struct Moderation {
    pub scope: String,
    pub account: String,
    pub kind: String,
    pub actor: String,
    pub reason: Option<String>,
    pub at_ms: u64,
}

impl From<ModRecord> for Moderation {
    fn from(m: ModRecord) -> Self {
        Self {
            scope: m.scope,
            account: m.account.to_string(),
            kind: m.kind.as_str().to_string(),
            actor: m.actor,
            reason: m.reason,
            at_ms: m.at_ms,
        }
    }
}

/// One row of `GET /namespaces`.
#[derive(Serialize)]
pub struct Namespace {
    pub name: String,
    pub owner: String,
    pub visibility: String,
    pub title: Option<String>,
    pub description: Option<String>,
    /// WC7 full freeze — every channel locked to the owner + network operators.
    pub frozen: bool,
}

impl From<NamespaceRecord> for Namespace {
    fn from(n: NamespaceRecord) -> Self {
        Self {
            name: n.name.to_string(),
            owner: n.owner.to_string(),
            visibility: n.visibility,
            title: n.title,
            description: n.description,
            frozen: n.frozen,
        }
    }
}

/// A per-emoji reaction summary on a materialized message.
#[derive(Serialize)]
pub struct Reaction {
    pub emoji: String,
    pub count: u64,
}

impl From<ReactionSummary> for Reaction {
    fn from(r: ReactionSummary) -> Self {
        Self {
            emoji: r.emoji,
            count: r.count,
        }
    }
}

/// Edit summary: how many edits and when the last one landed.
#[derive(Serialize)]
pub struct Edited {
    pub count: u64,
    pub at_ms: u64,
}

/// A materialized history item — either the final message or a tombstone.
/// Untagged: a message carries content fields; a tombstone carries `deleted`.
#[derive(Serialize)]
#[serde(untagged)]
pub enum Msg {
    Message {
        msgid: String,
        sender: String,
        body: String,
        at_ms: u64,
        edited: Option<Edited>,
        reactions: Vec<Reaction>,
    },
    Tombstone {
        msgid: String,
        deleted: bool,
        by: String,
    },
}

impl From<HistoryItem> for Msg {
    fn from(item: HistoryItem) -> Self {
        match item {
            HistoryItem::Message {
                msgid,
                sender,
                body,
                edited,
                reactions,
                ..
            } => Msg::Message {
                at_ms: msgid.timestamp_ms(),
                msgid: msgid.to_string(),
                sender: sender.to_string(),
                body,
                edited: edited.map(|(count, at_ms)| Edited { count, at_ms }),
                reactions: reactions.into_iter().map(Reaction::from).collect(),
            },
            HistoryItem::Tombstone { msgid, by } => Msg::Tombstone {
                msgid: msgid.to_string(),
                deleted: true,
                by: by.to_string(),
            },
        }
    }
}

/// A report as listed and shown. Reporter is exposed to operators here;
/// per-config anonymization (§6.7) is a later refinement.
#[derive(Serialize)]
pub struct Report {
    pub id: String,
    pub msgid: String,
    pub scope: String,
    pub category: String,
    pub state: String,
    pub status: String,
    pub reporter: String,
    pub note: Option<String>,
    pub filed_at_ms: u64,
    pub resolution: Option<String>,
}

impl From<ReportRecord> for Report {
    fn from(r: ReportRecord) -> Self {
        Self {
            id: r.id,
            msgid: r.msgid.to_string(),
            scope: r.scope.as_key(),
            category: r.category,
            state: format!("{:?}", r.state).to_lowercase(),
            status: format!("{:?}", r.status).to_lowercase(),
            reporter: r.reporter.to_string(),
            note: r.note,
            filed_at_ms: r.filed_at_ms,
            resolution: r
                .resolution
                .map(|res| format!("{:?}", res.action).to_lowercase()),
        }
    }
}

/// `GET /reports/:id` — the report plus the reported message and its
/// retention-held context (invariant 11), materialized.
#[derive(Serialize)]
pub struct ReportDetail {
    pub report: Report,
    pub reported_msgid: String,
    pub context: Vec<Msg>,
}

/// One row of `GET /peers`. `acked` = a mutually-acked manifest exists.
#[derive(Serialize)]
pub struct Peer {
    pub peer: String,
    pub scope: String,
    pub version: u64,
    pub acked: bool,
    pub severed: bool,
    pub updated_ms: u64,
}

impl From<PeerRecord> for Peer {
    fn from(p: PeerRecord) -> Self {
        Self {
            peer: p.peer.to_string(),
            scope: p.scope,
            version: p.version,
            acked: p.acked_manifest.is_some(),
            severed: p.severed,
            updated_ms: p.updated_ms,
        }
    }
}

/// `GET /peers/:peer` — federation peer detail (WC5). Parses the stored signed
/// manifest for the shared channel set, pinned key, and negotiated modes.
/// `netblocked` ties in the §11.6 sever mechanism (a netblock is how you sever a
/// whole peer network).
#[derive(Serialize)]
pub struct PeerDetail {
    pub peer: String,
    pub scope: String,
    pub version: u64,
    pub acked: bool,
    pub severed: bool,
    pub netblocked: bool,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub manifest: Option<PeerManifest>,
}

/// The parsed signed manifest a peer bridges under (§11.3).
#[derive(Serialize)]
pub struct PeerManifest {
    /// Pinned signing-key fingerprint (truncated hex of the Ed25519 pubkey).
    pub key_fingerprint: String,
    /// The embedded signature self-verifies over the manifest body.
    pub verified: bool,
    /// Shared channels — the "shared-room count" is `channels.len()`.
    pub channels: Vec<String>,
    pub history: String,
    pub media: String,
    pub typing: bool,
    pub voice: bool,
}

/// One row of `GET /netblocks`.
#[derive(Serialize)]
pub struct Netblock {
    pub network: String,
    pub reason: Option<String>,
    pub actor: String,
    pub added_ms: u64,
}

impl From<NetblockRecord> for Netblock {
    fn from(n: NetblockRecord) -> Self {
        Self {
            network: n.network.to_string(),
            reason: n.reason,
            actor: n.actor,
            added_ms: n.added_ms,
        }
    }
}

/// One row of `GET /media-blocks`.
#[derive(Serialize)]
pub struct MediaBlock {
    pub hash: String,
    pub reason: Option<String>,
    pub actor: String,
    pub added_ms: u64,
}

impl From<MediaBlockRecord> for MediaBlock {
    fn from(b: MediaBlockRecord) -> Self {
        Self {
            hash: b.hash,
            reason: b.reason,
            actor: b.actor,
            added_ms: b.added_ms,
        }
    }
}

/// One link of an inspected capability-token chain (WC6). Self-describing — the
/// operator debugging tool for "why can/can't X do Y" (§10.4).
#[derive(Serialize)]
pub struct TokenLink {
    /// The signing key's fingerprint (root = scope authority; child = parent's
    /// subject key).
    pub issuer_fingerprint: String,
    /// Rendered subject: `key <fp>` / `account <ulid>` / `foreign <user>` /
    /// `unbound (invite)`.
    pub subject: String,
    pub scope: String,
    pub caps: Vec<String>,
    /// The scope revocation epoch this token was issued at.
    pub epoch: u64,
    /// Unix seconds; `0` = no expiry.
    pub expiry: u64,
    pub expired: bool,
    /// This is a root token (unparented, signed by the scope authority).
    pub rooted: bool,
    /// `parent` hash links to the previous token in the chain (root: unparented).
    pub parent_linked: bool,
    /// The token's issue epoch is behind the scope's current epoch — revoked.
    pub revoked: bool,
    /// Short content-hash fingerprint, for reference.
    pub token_hash: String,
}

/// `POST /tokens/inspect` — the parsed delegation chain (WC6).
#[derive(Serialize)]
pub struct TokenInspection {
    pub links: Vec<TokenLink>,
    /// Every child links to its parent's hash and the root is unparented.
    pub chain_linked: bool,
    /// A parse error, if the input wasn't a valid token chain.
    pub error: Option<String>,
}

/// `GET /revocations?scope=` — a scope's current revocation epoch and the grants
/// a bump would invalidate (WC6).
#[derive(Serialize)]
pub struct RevocationScope {
    pub scope: String,
    pub epoch: u64,
    pub grants: Vec<Grant>,
}

/// `POST /revocations` — the scope's new epoch after a bump.
#[derive(Serialize)]
pub struct EpochBumped {
    pub scope: String,
    pub epoch: u64,
}

/// One row of `GET /audit` — a hash-chained audit record (`ts_ms` surfaces as
/// `at_ms` for consistency with the other timestamped rows).
#[derive(Serialize)]
pub struct Audit {
    pub seq: u64,
    pub operator: String,
    pub action: String,
    pub target: String,
    pub at_ms: u64,
    pub payload_digest: String,
    pub prev_hash: String,
    pub hash: String,
}

impl From<AuditRecord> for Audit {
    fn from(r: AuditRecord) -> Self {
        Self {
            seq: r.seq,
            operator: r.operator,
            action: r.action,
            target: r.target,
            at_ms: r.ts_ms,
            payload_digest: r.payload_digest,
            prev_hash: r.prev_hash,
            hash: r.hash,
        }
    }
}

/// One row of `GET /admins` — an account holding panel access, and how.
/// `operator` accounts implicitly hold every scope; `config_operator` marks the
/// ones seeded from `weftd.toml`, whose flag the panel cannot revoke.
#[derive(Serialize)]
pub struct AdminEntry {
    pub account: String,
    pub operator: bool,
    pub config_operator: bool,
    /// The effective scopes — every scope for an operator, else the granted set.
    pub scopes: Vec<String>,
    /// Unix seconds; `None` = no expiry. Only meaningful for granted admins.
    pub expiry: Option<u64>,
}

/// `GET /namespaces/:name/detail` — one namespace's full operator view, the
/// backing data for its detail page's sub-tabs.
#[derive(Serialize)]
pub struct NamespaceDetail {
    pub name: String,
    pub owner: String,
    pub visibility: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub icon: Option<String>,
    /// WC7 full freeze — every channel locked to the owner + network operators.
    pub frozen: bool,
    /// §11.10 auto-federation reachability.
    pub federation: bool,
    /// Server-authoritative channel categories (empty ones included).
    pub categories: Vec<String>,
    /// Whether an M-of-N recovery quorum is designated (§2.4). Membership of the
    /// quorum is deliberately not exposed — existence only.
    pub recovery_set: bool,
    pub root_key: String,
    pub channels: Vec<NamespaceChannel>,
    pub roles: Vec<Role>,
    /// Distinct accounts holding membership in any of the namespace's channels.
    pub members: Vec<String>,
}

/// A channel row inside a namespace detail.
#[derive(Serialize)]
pub struct NamespaceChannel {
    pub name: String,
    pub kind: String,
    pub policy: String,
    pub category: Option<String>,
    pub position: i64,
    pub frozen: bool,
    pub restricted: bool,
}

/// A role definition (§6.5) as the panel shows it.
#[derive(Serialize)]
pub struct Role {
    pub name: String,
    pub color: String,
    pub caps: Vec<String>,
    pub hoist: bool,
    pub position: i32,
}

/// One row of `GET /accounts/:name/dms` — a correspondent plus the conversation
/// key the message browser uses.
#[derive(Serialize)]
pub struct DmPartner {
    pub account: String,
}

/// `GET /messages/lookup/:msgid` — "someone handed me an ID, what is it?".
/// Resolves a full `network/ULID` msgid or a bare ULID to the message, where it
/// lives, and its surrounding conversation.
#[derive(Serialize)]
pub struct MessageLookup {
    pub msgid: String,
    /// The store scope key — a channel name, or `dm:<a>:<b>`.
    pub scope: String,
    /// `channel` | `dm`, so the UI knows which detail page to offer.
    pub scope_kind: String,
    pub sender: String,
    pub at_ms: u64,
    pub deleted: bool,
    /// The scope's materialized messages, the looked-up one among them.
    pub context: Vec<Msg>,
}
