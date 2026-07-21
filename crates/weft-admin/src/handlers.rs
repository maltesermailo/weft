//! Admin API handlers. Reads go straight to the store; moderation writes use
//! the same `ModerationStore`/`ReportStore` mutations the protocol handlers use.
//!
//! Wired: reads (reports + materialized context, accounts, channels,
//! namespaces, grants, moderation, message history), plus moderation actions
//! (mute/ban/unmute/unban) and report resolve. Still TODO: `DELETE /messages`
//! and kick — both need the live channel actor (ULID single-writer + broadcast),
//! so they land with the embedded live-broadcast slice.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use weft_proto::{Account, ChannelName, MsgId, ResolveAction, Ulid};
use weft_store::{
    materialize, AuditEntry, ModKind, ModRecord, Page, ReportResolution, Scope, StoreError,
};

use crate::{dto, AdminState};

/// All operator-gated routes (the auth layer is applied by `router`).
pub fn routes() -> Router<AdminState> {
    Router::new()
        .route("/api/v1/me", get(me))
        .route("/api/v1/stats", get(stats))
        .route("/api/v1/reports", get(list_reports))
        .route("/api/v1/reports/:id", get(report_detail))
        .route("/api/v1/reports/:id/resolve", post(resolve_report))
        .route("/api/v1/accounts", get(list_accounts))
        .route(
            "/api/v1/accounts/:name",
            get(account_detail).delete(delete_account),
        )
        .route("/api/v1/accounts/:name/messages", get(account_messages))
        .route("/api/v1/channels", get(list_channels))
        .route("/api/v1/namespaces", get(list_namespaces))
        .route("/api/v1/grants", get(list_grants))
        .route("/api/v1/moderation", get(list_moderation).post(moderate))
        .route("/api/v1/peers", get(list_peers))
        .route("/api/v1/netblocks", get(list_netblocks).post(add_netblock))
        .route(
            "/api/v1/netblocks/:network",
            axum::routing::delete(remove_netblock),
        )
        .route(
            "/api/v1/media-blocks",
            get(list_media_blocks).post(block_media),
        )
        .route(
            "/api/v1/media-blocks/:hash",
            axum::routing::delete(unblock_media),
        )
        .route("/api/v1/channels/:name/messages", get(browse_messages))
        // msgids are `<network>/<ULID>` — they contain a slash, so capture the
        // whole tail with a wildcard.
        .route(
            "/api/v1/messages/*msgid",
            axum::routing::delete(delete_message),
        )
        .route("/api/v1/audit", get(list_audit))
}

// ---- read ----

async fn me(Extension(who): Extension<Account>, State(st): State<AdminState>) -> Response {
    Json(dto::Me {
        account: who.to_string(),
        network: st.network.clone(),
    })
    .into_response()
}

async fn stats(State(st): State<AdminState>) -> Response {
    // Each count is a cheap list-and-len; the admin dashboard isn't a hot path.
    let counts = async {
        Ok::<_, StoreError>(dto::Stats {
            accounts: st.accounts.list_accounts().await?.len(),
            channels: st.channels.list_channels().await?.len(),
            namespaces: st.namespaces.list_public(None, 10_000).await?.len(),
            open_reports: st
                .reports
                .list_reports("*", Some(weft_proto::ReportStatus::Open), None, 10_000)
                .await?
                .len(),
            peers: st.peers.list_peers().await?.len(),
            netblocks: st.netblocks.list_netblocks().await?.len(),
            blocked_media: st.media_blocks.list_blocked_hashes().await?.len(),
            live_connections: st
                .live_connections
                .as_ref()
                .map(|c| c.load(std::sync::atomic::Ordering::Relaxed)),
        })
    }
    .await;
    match counts {
        Ok(v) => Json(v).into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
struct ScopeQuery {
    scope: Option<String>,
    #[allow(dead_code)]
    limit: Option<usize>,
}

async fn list_reports(State(st): State<AdminState>, Query(q): Query<ScopeQuery>) -> Response {
    let scope = q.scope.as_deref().unwrap_or("*");
    match st
        .reports
        .list_reports(scope, None, None, q.limit.unwrap_or(200))
        .await
    {
        Ok(list) => {
            Json(list.into_iter().map(dto::Report::from).collect::<Vec<_>>()).into_response()
        }
        Err(e) => internal(e),
    }
}

async fn report_detail(State(st): State<AdminState>, Path(id): Path<String>) -> Response {
    match report_with_context(&st, &id).await {
        Ok(Some(v)) => Json(v).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no such report").into_response(),
        Err(e) => internal(e),
    }
}

/// The report + the reported message and its surrounding context, materialized.
/// Context is the retention-held roots (invariant 11 keeps them queryable); if
/// none are held, just the reported message. e2ee / purged content simply won't
/// resolve — shown as absent, never reconstructed (invariant 8).
async fn report_with_context(
    st: &AdminState,
    id: &str,
) -> Result<Option<dto::ReportDetail>, StoreError> {
    let Some(report) = st.reports.report(id).await? else {
        return Ok(None);
    };
    let reported_msgid = report.msgid.to_string();
    let scope = report.scope.clone();

    let mut root_ulids: Vec<Ulid> = report.held_roots.clone();
    if root_ulids.is_empty() {
        root_ulids.push(report.msgid.ulid());
    }
    let mut roots = Vec::new();
    for ulid in &root_ulids {
        if let Some(rec) = st.events.find_root(*ulid).await? {
            roots.push(rec);
        }
    }
    let children = st.events.children(&scope, &root_ulids).await?;
    let context = materialize(roots, children)
        .into_iter()
        .map(dto::Msg::from)
        .collect();

    Ok(Some(dto::ReportDetail {
        report: dto::Report::from(report),
        reported_msgid,
        context,
    }))
}

/// Enriched account list: name, ULID, operator flag, caps at `*`, and whether
/// muted/banned network-wide. `*`-grants + `*`-moderation are each fetched once
/// and joined, so it stays a couple of queries plus one ULID lookup per account.
async fn list_accounts(State(st): State<AdminState>) -> Response {
    let enriched = async {
        let accounts = st.accounts.list_accounts().await?;
        // ULID → caps at `*` (grants key by the account's stable ULID, §10.4).
        let star_grants = st.caps.grants_at_scope("*").await?;
        let caps_by_ulid: std::collections::HashMap<String, Vec<String>> = star_grants
            .into_iter()
            .map(|g| (g.subject, g.caps))
            .collect();
        // account → mod kinds at `*`.
        let star_mod = st.moderation.list_moderation("*").await?;
        let mut out = Vec::with_capacity(accounts.len());
        for account in accounts {
            let ulid = st
                .accounts
                .account_ulid(&account)
                .await?
                .unwrap_or_default();
            let muted = star_mod
                .iter()
                .any(|m| m.account == account && matches!(m.kind, ModKind::Mute));
            let banned = star_mod
                .iter()
                .any(|m| m.account == account && matches!(m.kind, ModKind::Ban));
            out.push(dto::AccountSummary {
                operator: st.auth.operators.contains(&account),
                caps: caps_by_ulid.get(&ulid).cloned().unwrap_or_default(),
                muted,
                banned,
                account: account.to_string(),
                ulid,
            });
        }
        Ok::<_, StoreError>(out)
    }
    .await;
    match enriched {
        Ok(list) => Json(list).into_response(),
        Err(e) => internal(e),
    }
}

/// One account's full operator view: ULID, operator flag, channel memberships,
/// every capability grant (across scopes), verification claims, and its `*`
/// moderation state. Messages are browsed separately.
async fn account_detail(State(st): State<AdminState>, Path(name): Path<String>) -> Response {
    let Ok(account) = name.parse::<Account>() else {
        return (StatusCode::BAD_REQUEST, "bad account").into_response();
    };
    let detail = async {
        let Some(ulid) = st.accounts.account_ulid(&account).await? else {
            return Ok(None);
        };
        let grants: Vec<dto::Grant> = st
            .caps
            .grants_for(&ulid)
            .await?
            .into_iter()
            .map(|g| dto::Grant {
                subject: None,
                scope: g.scope,
                caps: g.caps,
                epoch: g.epoch,
                expiry: g.expiry,
            })
            .collect();
        let memberships: Vec<String> = st
            .memberships
            .memberships(&account)
            .await?
            .into_iter()
            .map(|c| c.to_string())
            .collect();
        let verifications: Vec<dto::Verification> = st
            .accounts
            .verifications(&account)
            .await?
            .into_iter()
            .map(|v| dto::Verification {
                kind: v.kind,
                subject: v.subject,
                verified: v.verified_at.is_some(),
            })
            .collect();
        Ok::<_, StoreError>(Some(dto::AccountDetail {
            operator: st.auth.operators.contains(&account),
            grants,
            memberships,
            verifications,
            muted: st
                .moderation
                .is_moderated(&account, &["*".to_string()], ModKind::Mute)
                .await?,
            banned: st
                .moderation
                .is_moderated(&account, &["*".to_string()], ModKind::Ban)
                .await?,
            account: account.to_string(),
            ulid,
        }))
    }
    .await;
    match detail {
        Ok(Some(v)) => Json(v).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no such account").into_response(),
        Err(e) => internal(e),
    }
}

/// Operator hard-delete of an account (+ its per-account data; messages kept).
/// An operator can't delete themselves (avoid locking out the live session).
async fn delete_account(
    State(st): State<AdminState>,
    Extension(who): Extension<Account>,
    Path(name): Path<String>,
) -> Response {
    let Ok(account) = name.parse::<Account>() else {
        return (StatusCode::BAD_REQUEST, "bad account").into_response();
    };
    if account == who {
        return (StatusCode::FORBIDDEN, "cannot delete your own account").into_response();
    }
    match st.accounts.delete_account(&account).await {
        Ok(true) => {
            audit(
                &st,
                &who,
                "account.delete",
                &account.to_string(),
                &json!({ "account": account.to_string() }),
            )
            .await;
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => (StatusCode::NOT_FOUND, "no such account").into_response(),
        Err(e) => internal(e),
    }
}

/// Every message a user authored, across all channels/DMs, newest-first — the
/// operator "all their messages" view. Bodies are the stored originals (edits
/// are separate rows); each is deletable by its msgid.
async fn account_messages(
    State(st): State<AdminState>,
    Path(name): Path<String>,
    Query(q): Query<ScopeQuery>,
) -> Response {
    let Ok(account) = name.parse::<Account>() else {
        return (StatusCode::BAD_REQUEST, "bad account").into_response();
    };
    let sender = format!("{account}@{}", st.network);
    match st
        .events
        .messages_by_sender(&sender, q.limit.unwrap_or(200))
        .await
    {
        Ok(rows) => Json(
            rows.into_iter()
                .map(|r| {
                    let at_ms = r.msgid.timestamp_ms();
                    let body = match r.kind {
                        weft_store::EventKind::Message { body, .. } => body,
                        _ => String::new(),
                    };
                    dto::AccountMessage {
                        msgid: r.msgid.to_string(),
                        scope: r.scope.as_key(),
                        sender: r.sender.to_string(),
                        body,
                        at_ms,
                    }
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => internal(e),
    }
}

async fn list_channels(State(st): State<AdminState>) -> Response {
    match st.channels.list_channels().await {
        Ok(chans) => Json(
            chans
                .into_iter()
                .map(|(name, policy)| dto::Channel {
                    name: name.to_string(),
                    policy: policy.to_string(),
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => internal(e),
    }
}

async fn list_moderation(State(st): State<AdminState>, Query(q): Query<ScopeQuery>) -> Response {
    let scope = q.scope.as_deref().unwrap_or("*");
    match st.moderation.list_moderation(scope).await {
        Ok(list) => Json(
            list.into_iter()
                .map(dto::Moderation::from)
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => internal(e),
    }
}

// ---- moderation actions ----

#[derive(Deserialize)]
struct ModerateReq {
    /// mute | ban | unmute | unban
    verb: String,
    scope: String,
    account: String,
    reason: Option<String>,
}

async fn moderate(
    State(st): State<AdminState>,
    Extension(who): Extension<Account>,
    Json(req): Json<ModerateReq>,
) -> Response {
    let Ok(account) = req.account.parse::<Account>() else {
        return (StatusCode::BAD_REQUEST, "bad account").into_response();
    };

    // Kick is transient (no deny-list record) — it force-parts via the live
    // channel actor, so it's embedded-only and its scope must be a channel.
    if req.verb == "kick" {
        return match (&st.live, req.scope.parse::<ChannelName>()) {
            (Some(live), Ok(channel)) => {
                live.eject(&channel, &account).await;
                audit(
                    &st,
                    &who,
                    "moderation.kick",
                    &format!("{}/{account}", req.scope),
                    &json!({ "scope": req.scope, "account": account.to_string() }),
                )
                .await;
                StatusCode::NO_CONTENT.into_response()
            }
            (None, _) => (
                StatusCode::NOT_IMPLEMENTED,
                "kick requires the embedded server",
            )
                .into_response(),
            (_, Err(_)) => {
                (StatusCode::BAD_REQUEST, "kick scope must be a channel").into_response()
            }
        };
    }

    let kind = match req.verb.as_str() {
        "mute" | "unmute" => ModKind::Mute,
        "ban" | "unban" => ModKind::Ban,
        other => return (StatusCode::BAD_REQUEST, format!("unknown verb {other}")).into_response(),
    };
    let result = if req.verb.starts_with("un") {
        st.moderation
            .clear_moderation(&req.scope, &account, kind)
            .await
            .map(|_| ())
    } else {
        st.moderation
            .set_moderation(ModRecord {
                scope: req.scope.clone(),
                account: account.clone(),
                kind,
                actor: who.to_string(),
                reason: req.reason,
                at_ms: now_ms(),
            })
            .await
    };
    if let Err(e) = result {
        return internal(e);
    }

    // A fresh channel-scope ban force-parts the target (matching the protocol
    // ban), when we can reach the live actor.
    if req.verb == "ban" {
        if let (Some(live), Ok(channel)) = (&st.live, req.scope.parse::<ChannelName>()) {
            live.eject(&channel, &account).await;
        }
    }

    audit(
        &st,
        &who,
        &format!("moderation.{}", req.verb),
        &format!("{}/{account}", req.scope),
        &json!({ "scope": req.scope, "account": account.to_string(), "verb": req.verb }),
    )
    .await;
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
struct ResolveReq {
    /// dismissed | content-removed | user-actioned | escalated
    action: String,
    note: Option<String>,
}

async fn resolve_report(
    State(st): State<AdminState>,
    Extension(who): Extension<Account>,
    Path(id): Path<String>,
    Json(req): Json<ResolveReq>,
) -> Response {
    let Ok(action) = req.action.parse::<ResolveAction>() else {
        return (StatusCode::BAD_REQUEST, "unknown action").into_response();
    };
    let now = now_ms();
    let resolution = ReportResolution {
        action,
        note: req.note,
        resolved_by: who.to_string(),
        at_ms: now,
        hold_release_at: now + 7 * 24 * 60 * 60 * 1000, // §12.1 grace
    };
    match st.reports.resolve_report(&id, resolution).await {
        Ok(true) => {
            audit(
                &st,
                &who,
                "report.resolve",
                &id,
                &json!({ "action": req.action }),
            )
            .await;
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => (StatusCode::NOT_FOUND, "no such report").into_response(),
        Err(e) => internal(e),
    }
}

async fn list_namespaces(State(st): State<AdminState>) -> Response {
    match st.namespaces.list_public(None, 500).await {
        Ok(list) => Json(
            list.into_iter()
                .map(dto::Namespace::from)
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => internal(e),
    }
}

async fn list_grants(State(st): State<AdminState>, Query(q): Query<ScopeQuery>) -> Response {
    let scope = q.scope.as_deref().unwrap_or("*");
    match st.caps.grants_at_scope(scope).await {
        Ok(list) => Json(
            list.into_iter()
                .map(|g| dto::Grant {
                    subject: Some(g.subject),
                    scope: g.scope,
                    caps: g.caps,
                    epoch: g.epoch,
                    expiry: g.expiry,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => internal(e),
    }
}

/// Browse a channel's history, fully materialized (final bodies, `edited`,
/// reaction summaries, tombstones) — the same view `HISTORY` serves.
async fn browse_messages(
    State(st): State<AdminState>,
    Path(name): Path<String>,
    Query(q): Query<ScopeQuery>,
) -> Response {
    let Ok(channel) = name.parse::<ChannelName>() else {
        return (StatusCode::BAD_REQUEST, "bad channel name").into_response();
    };
    match browse(&st, Scope::Channel(channel), q.limit.unwrap_or(100)).await {
        Ok(items) => Json(items).into_response(),
        Err(e) => internal(e),
    }
}

async fn browse(st: &AdminState, scope: Scope, limit: usize) -> Result<Vec<dto::Msg>, StoreError> {
    let page = Page {
        before: None,
        after: None,
        limit,
    };
    let roots = st.events.roots(&scope, page).await?;
    let root_ulids: Vec<Ulid> = roots.iter().map(|r| r.msgid.ulid()).collect();
    let children = st.events.children(&scope, &root_ulids).await?;
    Ok(materialize(roots, children)
        .into_iter()
        .map(dto::Msg::from)
        .collect())
}

/// Operator delete-any: tombstone the message via its channel actor.
async fn delete_message(
    State(st): State<AdminState>,
    Extension(who): Extension<Account>,
    Path(msgid): Path<String>,
) -> Response {
    let Some(live) = &st.live else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            "delete requires the embedded server",
        )
            .into_response();
    };
    let Ok(id) = msgid.parse::<MsgId>() else {
        return (StatusCode::BAD_REQUEST, "bad msgid").into_response();
    };
    if live.delete_message(&id, &who).await {
        audit(
            &st,
            &who,
            "message.delete",
            &id.to_string(),
            &json!({ "msgid": id.to_string() }),
        )
        .await;
        StatusCode::NO_CONTENT.into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            "no such message (or not a live channel)",
        )
            .into_response()
    }
}

// ---- §11 federation: peers + netblocks ----

async fn list_peers(State(st): State<AdminState>) -> Response {
    match st.peers.list_peers().await {
        Ok(list) => Json(list.into_iter().map(dto::Peer::from).collect::<Vec<_>>()).into_response(),
        Err(e) => internal(e),
    }
}

async fn list_netblocks(State(st): State<AdminState>) -> Response {
    match st.netblocks.list_netblocks().await {
        Ok(list) => Json(
            list.into_iter()
                .map(dto::Netblock::from)
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
struct NetblockReq {
    network: String,
    reason: Option<String>,
}

async fn add_netblock(
    State(st): State<AdminState>,
    Extension(who): Extension<Account>,
    Json(req): Json<NetblockReq>,
) -> Response {
    let Ok(network) = req.network.parse::<weft_proto::NetworkName>() else {
        return (StatusCode::BAD_REQUEST, "bad network").into_response();
    };
    let net_str = network.to_string();
    match st
        .netblocks
        .add_netblock(weft_store::NetblockRecord {
            network,
            reason: req.reason,
            added_ms: now_ms(),
            actor: who.to_string(),
        })
        .await
    {
        Ok(()) => {
            audit(
                &st,
                &who,
                "netblock.add",
                &net_str,
                &json!({ "network": net_str }),
            )
            .await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => internal(e),
    }
}

async fn remove_netblock(
    State(st): State<AdminState>,
    Extension(who): Extension<Account>,
    Path(network): Path<String>,
) -> Response {
    let Ok(network) = network.parse::<weft_proto::NetworkName>() else {
        return (StatusCode::BAD_REQUEST, "bad network").into_response();
    };
    match st.netblocks.remove_netblock(&network).await {
        Ok(true) => {
            audit(
                &st,
                &who,
                "netblock.remove",
                &network.to_string(),
                &json!({ "network": network.to_string() }),
            )
            .await;
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => (StatusCode::NOT_FOUND, "not blocked").into_response(),
        Err(e) => internal(e),
    }
}

// ---- §13 media hash blocklist ----

async fn list_media_blocks(State(st): State<AdminState>) -> Response {
    match st.media_blocks.list_blocked_hashes().await {
        Ok(list) => Json(
            list.into_iter()
                .map(dto::MediaBlock::from)
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
struct BlockMediaReq {
    hash: String,
    reason: Option<String>,
}

/// Record a hash block. NOTE: this admin path records the blocklist entry (so
/// re-upload/mirror/fetch are rejected) but does **not** itself delete already
/// stored bytes — the blob store lives in weftd, not the store roles here. The
/// wire `MEDIA BLOCK` verb (operator over a session) does the byte deletion; the
/// GC + the fetch gate cover blobs blocked via this panel.
async fn block_media(
    State(st): State<AdminState>,
    Extension(who): Extension<Account>,
    Json(req): Json<BlockMediaReq>,
) -> Response {
    let hash = req.hash.trim();
    if hash.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty hash").into_response();
    }
    match st
        .media_blocks
        .block_hash(weft_store::MediaBlockRecord {
            hash: hash.to_string(),
            reason: req.reason,
            added_ms: now_ms(),
            actor: who.to_string(),
        })
        .await
    {
        Ok(()) => {
            audit(&st, &who, "media.block", hash, &json!({ "hash": hash })).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => internal(e),
    }
}

async fn unblock_media(
    State(st): State<AdminState>,
    Extension(who): Extension<Account>,
    Path(hash): Path<String>,
) -> Response {
    match st.media_blocks.unblock_hash(&hash).await {
        Ok(true) => {
            audit(&st, &who, "media.unblock", &hash, &json!({ "hash": hash })).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => (StatusCode::NOT_FOUND, "not blocked").into_response(),
        Err(e) => internal(e),
    }
}

// ---- WC1 audit trail ----

#[derive(Deserialize)]
struct AuditQuery {
    operator: Option<String>,
    action: Option<String>,
    limit: Option<usize>,
}

/// The hash-chained audit log, newest-first, optionally filtered by operator
/// and/or action. Each row carries its chain fields so a reader can verify the
/// log wasn't tampered with (recompute `hash`, follow `prev_hash`).
async fn list_audit(State(st): State<AdminState>, Query(q): Query<AuditQuery>) -> Response {
    match st
        .audit
        .list_audit(
            q.operator.as_deref(),
            q.action.as_deref(),
            q.limit.unwrap_or(200),
        )
        .await
    {
        Ok(list) => {
            Json(list.into_iter().map(dto::Audit::from).collect::<Vec<_>>()).into_response()
        }
        Err(e) => internal(e),
    }
}

/// Emit an audit record for a completed write action. The payload is digested,
/// never stored raw (it may carry reasons/notes). A store failure is logged but
/// never fails the action the operator already performed — the audit log is a
/// record of what happened, and the mutation has already happened.
async fn audit(st: &AdminState, who: &Account, action: &str, target: &str, payload: &Value) {
    let entry = AuditEntry {
        operator: who.to_string(),
        action: action.to_string(),
        target: target.to_string(),
        ts_ms: now_ms(),
        payload_digest: hex(&Sha256::digest(payload.to_string().as_bytes())),
    };
    if let Err(e) = st.audit.append_audit(entry).await {
        tracing::error!("audit append failed for {action} on {target}: {e}");
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

// ---- helpers ----

fn internal(e: impl std::fmt::Display) -> Response {
    tracing::error!("admin store error: {e}");
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
