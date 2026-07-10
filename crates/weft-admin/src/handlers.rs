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
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use weft_proto::{Account, ChannelName, MsgId, ResolveAction, Ulid};
use weft_store::{
    materialize, HistoryItem, ModKind, ModRecord, Page, ReportResolution, Scope, StoreError,
};

use crate::AdminState;

/// All operator-gated routes (the auth layer is applied by `router`).
pub fn routes() -> Router<AdminState> {
    Router::new()
        .route("/api/me", get(me))
        .route("/api/stats", get(stats))
        .route("/api/reports", get(list_reports))
        .route("/api/reports/:id", get(report_detail))
        .route("/api/reports/:id/resolve", post(resolve_report))
        .route("/api/accounts", get(list_accounts))
        .route("/api/channels", get(list_channels))
        .route("/api/namespaces", get(list_namespaces))
        .route("/api/grants", get(list_grants))
        .route("/api/moderation", get(list_moderation).post(moderate))
        .route("/api/channels/:name/messages", get(browse_messages))
        // msgids are `<network>/<ULID>` — they contain a slash, so capture the
        // whole tail with a wildcard.
        .route(
            "/api/messages/*msgid",
            axum::routing::delete(delete_message),
        )
}

// ---- read ----

async fn me(Extension(who): Extension<Account>, State(st): State<AdminState>) -> Response {
    Json(json!({ "account": who.to_string(), "network": st.network })).into_response()
}

async fn stats(State(st): State<AdminState>) -> Response {
    let accounts = st.accounts.list_accounts().await.map(|a| a.len());
    let channels = st.channels.list_channels().await.map(|c| c.len());
    match (accounts, channels) {
        (Ok(accounts), Ok(channels)) => Json(json!({
            "accounts": accounts,
            "channels": channels,
            "live_connections": st
                .live_connections
                .as_ref()
                .map(|c| c.load(std::sync::atomic::Ordering::Relaxed)),
        }))
        .into_response(),
        (Err(e), _) | (_, Err(e)) => internal(e),
    }
}

#[derive(Deserialize)]
struct ScopeQuery {
    scope: Option<String>,
    #[allow(dead_code)]
    limit: Option<usize>,
}

#[derive(Serialize)]
struct ReportDto {
    id: String,
    msgid: String,
    scope: String,
    category: String,
    state: String,
    status: String,
    // TODO(§6.7): honor per-config reporter anonymization before exposing.
    reporter: String,
    note: Option<String>,
    filed_at_ms: u64,
    resolution: Option<String>,
}

impl From<weft_store::ReportRecord> for ReportDto {
    fn from(r: weft_store::ReportRecord) -> Self {
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

async fn list_reports(State(st): State<AdminState>, Query(q): Query<ScopeQuery>) -> Response {
    let scope = q.scope.as_deref().unwrap_or("*");
    match st
        .reports
        .list_reports(scope, None, None, q.limit.unwrap_or(200))
        .await
    {
        Ok(list) => Json(list.into_iter().map(ReportDto::from).collect::<Vec<_>>()).into_response(),
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
async fn report_with_context(st: &AdminState, id: &str) -> Result<Option<Value>, StoreError> {
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
    let context: Vec<Value> = materialize(roots, children)
        .into_iter()
        .map(msg_dto)
        .collect();

    Ok(Some(json!({
        "report": ReportDto::from(report),
        "reported_msgid": reported_msgid,
        "context": context,
    })))
}

async fn list_accounts(State(st): State<AdminState>) -> Response {
    match st.accounts.list_accounts().await {
        // TODO: enrich with presence / device count / *-roles per account.
        Ok(accts) => {
            Json(accts.into_iter().map(|a| a.to_string()).collect::<Vec<_>>()).into_response()
        }
        Err(e) => internal(e),
    }
}

async fn list_channels(State(st): State<AdminState>) -> Response {
    match st.channels.list_channels().await {
        Ok(chans) => Json(
            chans
                .into_iter()
                .map(|(name, policy)| json!({ "name": name.to_string(), "policy": policy.to_string() }))
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
                .map(|m| {
                    json!({
                        "scope": m.scope,
                        "account": m.account.to_string(),
                        "kind": format!("{:?}", m.kind).to_lowercase(),
                        "actor": m.actor,
                        "reason": m.reason,
                        "at_ms": m.at_ms,
                    })
                })
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
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "no such report").into_response(),
        Err(e) => internal(e),
    }
}

async fn list_namespaces(State(st): State<AdminState>) -> Response {
    match st.namespaces.list_public(None, 500).await {
        Ok(list) => Json(
            list.into_iter()
                .map(|n| {
                    json!({
                        "name": n.name.to_string(),
                        "owner": n.owner.to_string(),
                        "visibility": n.visibility,
                        "title": n.title,
                        "description": n.description,
                    })
                })
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
                .map(|g| {
                    json!({
                        "subject": g.subject,
                        "scope": g.scope,
                        "caps": g.caps,
                        "epoch": g.epoch,
                        "expiry": g.expiry,
                    })
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

async fn browse(st: &AdminState, scope: Scope, limit: usize) -> Result<Vec<Value>, StoreError> {
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
        .map(msg_dto)
        .collect())
}

/// Render one materialized item as JSON (shared by browse + report context).
fn msg_dto(item: HistoryItem) -> Value {
    match item {
        HistoryItem::Message {
            msgid,
            sender,
            body,
            edited,
            reactions,
            ..
        } => json!({
            "msgid": msgid.to_string(),
            "sender": sender.to_string(),
            "body": body,
            "at_ms": msgid.timestamp_ms(),
            "edited": edited.map(|(count, at_ms)| json!({ "count": count, "at_ms": at_ms })),
            "reactions": reactions
                .iter()
                .map(|r| json!({ "emoji": r.emoji, "count": r.count }))
                .collect::<Vec<_>>(),
        }),
        HistoryItem::Tombstone { msgid, by } => json!({
            "msgid": msgid.to_string(),
            "deleted": true,
            "by": by.to_string(),
        }),
    }
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
        StatusCode::NO_CONTENT.into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            "no such message (or not a live channel)",
        )
            .into_response()
    }
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
