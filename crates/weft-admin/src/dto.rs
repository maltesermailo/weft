//! Typed response bodies for the admin JSON API. Every handler serializes one
//! of these (or a `Vec`), so the wire shape is a named contract rather than an
//! ad-hoc `json!` — the SPA and any future API client read exactly these keys.
//! Field names and shapes are the API; don't rename them without versioning.

use serde::Serialize;
use weft_store::{
    AuditRecord, HistoryItem, MediaBlockRecord, ModRecord, NamespaceRecord, NetblockRecord,
    PeerRecord, ReactionSummary, ReportRecord,
};

/// `GET /me` — who the session belongs to.
#[derive(Serialize)]
pub struct Me {
    pub account: String,
    pub network: String,
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
}

impl From<NamespaceRecord> for Namespace {
    fn from(n: NamespaceRecord) -> Self {
        Self {
            name: n.name.to_string(),
            owner: n.owner.to_string(),
            visibility: n.visibility,
            title: n.title,
            description: n.description,
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
