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

use crate::auth::{is_operator, AdminScope, AdminScopes};
use crate::{dto, AdminState};

/// The capability scope delegated admin grants live at (§10.4 dogfooding —
/// `GRANT admin admin.moderate <account>`). Also the key the panel reads and
/// rewrites when an operator edits someone's permissions.
const ADMIN_GRANT_SCOPE: &str = "admin";

/// All operator-gated routes (the auth layer is applied by `router`).
pub fn routes() -> Router<AdminState> {
    Router::new()
        .route("/api/v1/me", get(me))
        .route("/api/v1/stats", get(stats))
        .route("/api/v1/reports", get(list_reports))
        .route("/api/v1/reports/:id", get(report_detail))
        .route("/api/v1/reports/:id/resolve", post(resolve_report))
        .route("/api/v1/reports/bulk-resolve", post(bulk_resolve_reports))
        .route("/api/v1/accounts", get(list_accounts))
        .route(
            "/api/v1/accounts/:name",
            get(account_detail).delete(delete_account),
        )
        .route("/api/v1/accounts/:name/messages", get(account_messages))
        .route("/api/v1/accounts/:name/dms", get(account_dms))
        .route("/api/v1/accounts/:name/restore", post(restore_account))
        .route("/api/v1/accounts/:name/suspend", post(suspend_account))
        .route("/api/v1/accounts/:name/unsuspend", post(unsuspend_account))
        .route(
            "/api/v1/accounts/:name/disconnect",
            post(disconnect_account),
        )
        .route("/api/v1/channels", get(list_channels))
        .route("/api/v1/channels/:name/detail", get(channel_detail))
        .route("/api/v1/channels/:name/freeze", post(freeze_channel))
        .route("/api/v1/channels/:name/unfreeze", post(unfreeze_channel))
        .route(
            "/api/v1/channels/:name",
            axum::routing::delete(delete_channel),
        )
        .route("/api/v1/namespaces", get(list_namespaces))
        .route("/api/v1/namespaces/:name/detail", get(namespace_detail))
        .route(
            "/api/v1/namespaces/:name/visibility",
            post(set_ns_visibility),
        )
        .route(
            "/api/v1/namespaces/:name/federation",
            post(set_ns_federation),
        )
        .route(
            "/api/v1/namespaces/:name/takeover",
            post(takeover_namespace),
        )
        .route("/api/v1/namespaces/:name/freeze", post(freeze_namespace))
        .route(
            "/api/v1/namespaces/:name/unfreeze",
            post(unfreeze_namespace),
        )
        .route("/api/v1/grants", get(list_grants))
        .route("/api/v1/moderation", get(list_moderation).post(moderate))
        .route("/api/v1/peers", get(list_peers))
        .route("/api/v1/peers/:name/detail", get(peer_detail))
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
        .route("/api/v1/dms/:a/:b/messages", get(browse_dm))
        // msgids are `<network>/<ULID>` — they contain a slash, so capture the
        // whole tail with a wildcard.
        .route(
            "/api/v1/messages/*msgid",
            axum::routing::delete(delete_message),
        )
        .route("/api/v1/admins", get(list_admins))
        .route(
            "/api/v1/accounts/:name/admin-scopes",
            post(set_admin_scopes),
        )
        .route("/api/v1/accounts/:name/operator", post(promote_operator))
        .route("/api/v1/accounts/:name/unoperator", post(demote_operator))
        .route("/api/v1/audit", get(list_audit))
        .route("/api/v1/tokens/inspect", post(inspect_tokens))
        .route(
            "/api/v1/revocations",
            get(get_revocations).post(bump_revocation),
        )
}

// ---- read ----

async fn me(
    Extension(who): Extension<Account>,
    Extension(scopes): Extension<AdminScopes>,
    State(st): State<AdminState>,
) -> Response {
    let operator = is_operator(&st, &who).await;
    Json(dto::Me {
        account: who.to_string(),
        network: st.network.clone(),
        scopes: scopes.as_strings(),
        operator,
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
            let deletion_scheduled = st.accounts.deletion_scheduled(&account).await?;
            let suspended = st.accounts.is_suspended(&account).await?;
            out.push(dto::AccountSummary {
                operator: st.auth.operators.contains(&account),
                caps: caps_by_ulid.get(&ulid).cloned().unwrap_or_default(),
                muted,
                banned,
                deletion_scheduled,
                suspended,
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
        let verification_records = st.accounts.verifications(&account).await?;
        // WC4 "find related": accounts sharing this account's email domain.
        let related = match verification_records.iter().find(|v| v.kind == "email") {
            Some(email) => {
                let domain = email.subject.rsplit('@').next().unwrap_or("");
                st.accounts
                    .accounts_by_email_domain(domain)
                    .await?
                    .into_iter()
                    .filter(|a| a != &account)
                    .map(|a| a.to_string())
                    .collect()
            }
            None => Vec::new(),
        };
        let verifications: Vec<dto::Verification> = verification_records
            .into_iter()
            .map(|v| dto::Verification {
                kind: v.kind,
                subject: v.subject,
                verified: v.verified_at.is_some(),
            })
            .collect();
        // WC4 device list: truncated fingerprint of each enrolled Ed25519 pubkey.
        let devices: Vec<String> = st
            .accounts
            .devices(&account)
            .await?
            .iter()
            .map(device_fingerprint)
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
            deletion_scheduled: st.accounts.deletion_scheduled(&account).await?,
            suspended: st.accounts.is_suspended(&account).await?,
            devices,
            related,
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
#[derive(Deserialize)]
struct ConfirmQuery {
    /// Typed-name confirmation — must echo the target's name (WC3).
    confirm: Option<String>,
}

/// WC3 soft delete: **schedule** the account's hard-delete `delete_grace_ms` in
/// the future (recoverable via `restore` until the maintenance pass finalizes
/// it). Guarded by `admin.destroy` + **typed-name confirmation** (`?confirm=`
/// must echo the account name) + the no-self-delete rule.
async fn delete_account(
    State(st): State<AdminState>,
    Extension(who): Extension<Account>,
    Extension(scopes): Extension<AdminScopes>,
    Path(name): Path<String>,
    Query(q): Query<ConfirmQuery>,
) -> Response {
    if let Some(r) = require(&scopes, AdminScope::Destroy) {
        return r;
    }
    let Ok(account) = name.parse::<Account>() else {
        return (StatusCode::BAD_REQUEST, "bad account").into_response();
    };
    if account == who {
        return (StatusCode::FORBIDDEN, "cannot delete your own account").into_response();
    }
    // Typed-name confirmation: the caller must prove intent by echoing the name.
    if q.confirm.as_deref() != Some(name.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            "confirmation does not match the account name",
        )
            .into_response();
    }

    let purge_at = now_ms() + st.delete_grace_ms;
    match st.accounts.schedule_deletion(&account, purge_at).await {
        Ok(true) => {
            audit(
                &st,
                &who,
                "account.schedule_delete",
                &account.to_string(),
                &json!({ "account": account.to_string(), "purge_at": purge_at }),
            )
            .await;
            Json(dto::DeletionScheduled { purge_at }).into_response()
        }
        Ok(false) => (StatusCode::NOT_FOUND, "no such account").into_response(),
        Err(e) => internal(e),
    }
}

/// WC3: cancel a scheduled account deletion (restore). `admin.destroy`.
async fn restore_account(
    State(st): State<AdminState>,
    Extension(who): Extension<Account>,
    Extension(scopes): Extension<AdminScopes>,
    Path(name): Path<String>,
) -> Response {
    if let Some(r) = require(&scopes, AdminScope::Destroy) {
        return r;
    }
    let Ok(account) = name.parse::<Account>() else {
        return (StatusCode::BAD_REQUEST, "bad account").into_response();
    };
    match st.accounts.cancel_deletion(&account).await {
        Ok(true) => {
            audit(
                &st,
                &who,
                "account.restore",
                &account.to_string(),
                &json!({ "account": account.to_string() }),
            )
            .await;
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => (StatusCode::NOT_FOUND, "not scheduled for deletion").into_response(),
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

// ---- WC2 permission management (operator-only) ----

/// Gate for changing *who is an admin*. Deliberately stricter than
/// `AdminScope::Destroy`: a delegated `admin.*` grant confers every scope, so
/// scope-gating this would let a delegated admin promote itself or mint peers.
/// Only a real operator (config seed or DB flag) may edit permissions.
async fn require_operator(st: &AdminState, who: &Account) -> Option<Response> {
    (!is_operator(st, who).await).then(|| {
        (
            StatusCode::FORBIDDEN,
            "only an operator can change admin permissions",
        )
            .into_response()
    })
}

/// `GET /admins` — everyone with panel access: operators (config-seeded or
/// flagged) plus accounts holding an `admin`-scope grant, with their scopes.
async fn list_admins(State(st): State<AdminState>) -> Response {
    let rows = async {
        let mut out: Vec<dto::AdminEntry> = Vec::new();

        // Operators: the config seed set ∪ the DB-flagged ones.
        let flagged = st.accounts.list_operators().await?;
        let mut operators: Vec<Account> = flagged.clone();
        for a in &st.auth.operators {
            if !operators.contains(a) {
                operators.push(a.clone());
            }
        }
        operators.sort_by_key(|a| a.to_string());
        for account in operators {
            out.push(dto::AdminEntry {
                config_operator: st.auth.operators.contains(&account),
                operator: true,
                scopes: AdminScope::ALL.iter().map(|s| s.as_str().into()).collect(),
                expiry: None,
                account: account.to_string(),
            });
        }

        // Delegated admins: `admin`-scope grants, keyed by account ULID. Grants
        // are stored by ULID, so map each back to its handle for display.
        for grant in st.caps.grants_at_scope(ADMIN_GRANT_SCOPE).await? {
            let mut scopes: Vec<String> = Vec::new();
            for cap in &grant.caps {
                if cap == "*" || cap == "admin.*" {
                    scopes = AdminScope::ALL.iter().map(|s| s.as_str().into()).collect();
                    break;
                }
                if let Some(s) = AdminScope::parse(cap) {
                    scopes.push(s.as_str().to_string());
                }
            }
            if scopes.is_empty() {
                continue;
            }
            scopes.sort();
            let Some(account) = resolve_subject_handle(&st, &grant.subject).await? else {
                continue; // a grant whose account is gone — nothing to show
            };
            if out.iter().any(|e| e.account == account) {
                continue; // already listed as an operator (which outranks it)
            }
            out.push(dto::AdminEntry {
                account,
                operator: false,
                config_operator: false,
                scopes,
                expiry: grant.expiry,
            });
        }
        Ok::<_, StoreError>(out)
    }
    .await;
    match rows {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => internal(e),
    }
}

/// Map a grant subject (an account ULID) back to its account handle.
async fn resolve_subject_handle(
    st: &AdminState,
    subject: &str,
) -> Result<Option<String>, StoreError> {
    for account in st.accounts.list_accounts().await? {
        if st.accounts.account_ulid(&account).await?.as_deref() == Some(subject) {
            return Ok(Some(account.to_string()));
        }
    }
    Ok(None)
}

#[derive(Deserialize)]
struct SetScopesReq {
    /// The complete scope set the account should hold — this *replaces* whatever
    /// it had. An empty list revokes panel access entirely.
    scopes: Vec<String>,
}

/// `POST /accounts/:name/admin-scopes` — set an account's delegated admin
/// scopes (operator-only). Replaces the existing `admin` grant wholesale, so the
/// body is the desired end state, not a delta.
async fn set_admin_scopes(
    State(st): State<AdminState>,
    Extension(who): Extension<Account>,
    Path(name): Path<String>,
    Json(req): Json<SetScopesReq>,
) -> Response {
    if let Some(r) = require_operator(&st, &who).await {
        return r;
    }
    let Ok(account) = name.parse::<Account>() else {
        return (StatusCode::BAD_REQUEST, "bad account").into_response();
    };

    // Parse strictly: a typo'd scope must fail loudly, never silently grant less.
    let mut caps: Vec<String> = Vec::new();
    for raw in &req.scopes {
        match AdminScope::parse(raw) {
            Some(s) => caps.push(s.as_str().to_string()),
            None => {
                return (StatusCode::BAD_REQUEST, format!("unknown scope: {raw}")).into_response()
            }
        }
    }
    caps.sort();
    caps.dedup();
    // `admin.read` is the panel baseline — any other scope is unusable without
    // it, so grant it implicitly rather than handing out a set that can't log in.
    if !caps.is_empty() && !caps.iter().any(|c| c == AdminScope::Read.as_str()) {
        caps.push(AdminScope::Read.as_str().to_string());
        caps.sort();
    }

    // An operator's authority comes from the flag, not a grant — editing scopes
    // for one would look like it worked while changing nothing.
    if is_operator(&st, &account).await {
        return (
            StatusCode::CONFLICT,
            "that account is an operator and already holds every scope — demote it first",
        )
            .into_response();
    }

    let ulid = match st.accounts.account_ulid(&account).await {
        Ok(Some(u)) => u,
        Ok(None) => return (StatusCode::NOT_FOUND, "no such account").into_response(),
        Err(e) => return internal(e),
    };

    let result = async {
        if caps.is_empty() {
            st.caps
                .revoke_grants(&ulid, ADMIN_GRANT_SCOPE, None)
                .await?;
        } else {
            let epoch = st.caps.scope_epoch(ADMIN_GRANT_SCOPE).await?;
            // `record_grant` replaces the (subject, scope) row, so this is a
            // wholesale set — no stale scope survives an edit.
            st.caps
                .record_grant(&ulid, ADMIN_GRANT_SCOPE, &caps, epoch, None)
                .await?;
        }
        Ok::<_, StoreError>(())
    }
    .await;
    if let Err(e) = result {
        return internal(e);
    }

    audit(
        &st,
        &who,
        "admin.scopes",
        &account.to_string(),
        &json!({ "account": account.to_string(), "scopes": caps }),
    )
    .await;
    StatusCode::NO_CONTENT.into_response()
}

/// `POST /accounts/:name/operator` — grant full operator authority.
async fn promote_operator(
    st: State<AdminState>,
    who: Extension<Account>,
    name: Path<String>,
) -> Response {
    set_operator_flag(st, who, name, true).await
}

/// `POST /accounts/:name/unoperator` — revoke it.
async fn demote_operator(
    st: State<AdminState>,
    who: Extension<Account>,
    name: Path<String>,
) -> Response {
    set_operator_flag(st, who, name, false).await
}

async fn set_operator_flag(
    State(st): State<AdminState>,
    Extension(who): Extension<Account>,
    Path(name): Path<String>,
    operator: bool,
) -> Response {
    if let Some(r) = require_operator(&st, &who).await {
        return r;
    }
    let Ok(account) = name.parse::<Account>() else {
        return (StatusCode::BAD_REQUEST, "bad account").into_response();
    };
    if !operator {
        // Two lockout guards: never demote yourself (you'd lose the ability to
        // undo it), and never remove the last operator (nobody could promote
        // anyone again — the panel would be permanently un-administrable).
        if account == who {
            return (
                StatusCode::FORBIDDEN,
                "cannot remove your own operator status",
            )
                .into_response();
        }
        // A config-seeded operator's authority comes from weftd.toml; clearing
        // the DB flag would not remove it, so say so instead of lying.
        if st.auth.operators.contains(&account) {
            return (
                StatusCode::CONFLICT,
                "that operator is seeded from weftd.toml — remove it there",
            )
                .into_response();
        }
        match st.accounts.list_operators().await {
            Ok(ops) if ops.len() <= 1 && ops.contains(&account) && st.auth.operators.is_empty() => {
                return (
                    StatusCode::CONFLICT,
                    "that is the last operator — promote another first",
                )
                    .into_response()
            }
            Ok(_) => {}
            Err(e) => return internal(e),
        }
    }
    match st.accounts.set_operator(&account, operator).await {
        Ok(true) => {
            audit(
                &st,
                &who,
                if operator {
                    "admin.promote"
                } else {
                    "admin.demote"
                },
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

/// WC7: suspend (or unsuspend) an account. A suspended account can't
/// authenticate (uniform AUTH-FAILED at the session layer), freezing its tokens.
/// `admin.moderate`, audited. `suspend` picks the direction via the last path
/// segment. Blocks self-suspend (don't lock yourself out).
async fn set_suspended(
    State(st): State<AdminState>,
    Extension(who): Extension<Account>,
    Extension(scopes): Extension<AdminScopes>,
    Path(name): Path<String>,
    suspend: bool,
) -> Response {
    if let Some(r) = require(&scopes, AdminScope::Moderate) {
        return r;
    }
    let Ok(account) = name.parse::<Account>() else {
        return (StatusCode::BAD_REQUEST, "bad account").into_response();
    };
    if suspend && account == who {
        return (StatusCode::FORBIDDEN, "cannot suspend your own account").into_response();
    }
    match st.accounts.set_suspended(&account, suspend).await {
        Ok(true) => {
            let action = if suspend {
                "account.suspend"
            } else {
                "account.unsuspend"
            };
            // A suspension that leaves the account's current sessions running is
            // only half a suspension — it could keep posting until it happened
            // to disconnect. Cut them as part of the same action.
            let cut = if suspend {
                cut_sessions(&st, &account).await
            } else {
                0
            };
            audit(
                &st,
                &who,
                action,
                &account.to_string(),
                &json!({ "account": account.to_string(), "sessions_closed": cut }),
            )
            .await;
            Json(json!({ "sessions_closed": cut })).into_response()
        }
        Ok(false) => (StatusCode::NOT_FOUND, "no such account").into_response(),
        Err(e) => internal(e),
    }
}

/// Close an account's live sessions, if this process has the live server (the
/// panel can run standalone, where there are no sessions to reach).
async fn cut_sessions(st: &AdminState, account: &Account) -> usize {
    match &st.live {
        Some(live) => live.disconnect_account(account).await,
        None => 0,
    }
}

/// `POST /accounts/:name/disconnect` — WC7 forced device logout. Ends every live
/// session without changing the account's state, so it can immediately sign back
/// in; pair it with suspend to keep it out. `admin.moderate`, audited.
async fn disconnect_account(
    State(st): State<AdminState>,
    Extension(who): Extension<Account>,
    Extension(scopes): Extension<AdminScopes>,
    Path(name): Path<String>,
) -> Response {
    if let Some(r) = require(&scopes, AdminScope::Moderate) {
        return r;
    }
    let Ok(account) = name.parse::<Account>() else {
        return (StatusCode::BAD_REQUEST, "bad account").into_response();
    };
    if st.live.is_none() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "no live server in this process",
        )
            .into_response();
    }
    match st.accounts.account_ulid(&account).await {
        Ok(Some(_)) => {}
        Ok(None) => return (StatusCode::NOT_FOUND, "no such account").into_response(),
        Err(e) => return internal(e),
    }
    let cut = cut_sessions(&st, &account).await;
    audit(
        &st,
        &who,
        "account.disconnect",
        &account.to_string(),
        &json!({ "account": account.to_string(), "sessions_closed": cut }),
    )
    .await;
    Json(json!({ "sessions_closed": cut })).into_response()
}

async fn suspend_account(
    st: State<AdminState>,
    who: Extension<Account>,
    scopes: Extension<AdminScopes>,
    name: Path<String>,
) -> Response {
    set_suspended(st, who, scopes, name, true).await
}

async fn unsuspend_account(
    st: State<AdminState>,
    who: Extension<Account>,
    scopes: Extension<AdminScopes>,
    name: Path<String>,
) -> Response {
    set_suspended(st, who, scopes, name, false).await
}

/// WC4 channel lookup detail: retention policy + the persistent member roster
/// (§6.3, offline members included).
async fn channel_detail(State(st): State<AdminState>, Path(name): Path<String>) -> Response {
    let Ok(channel) = name.parse::<ChannelName>() else {
        return (StatusCode::BAD_REQUEST, "bad channel name").into_response();
    };
    let detail = async {
        let Some(record) = st.channels.channel(&channel).await? else {
            return Ok(None);
        };
        let members: Vec<String> = st
            .memberships
            .members(&channel)
            .await?
            .into_iter()
            .map(|a| a.to_string())
            .collect();
        Ok::<_, StoreError>(Some(dto::ChannelDetail {
            name: channel.to_string(),
            policy: record.policy.to_string(),
            members,
            frozen: record.frozen,
            restricted: record.restricted,
        }))
    }
    .await;
    match detail {
        Ok(Some(v)) => Json(v).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no such channel").into_response(),
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
    Extension(scopes): Extension<AdminScopes>,
    Json(req): Json<ModerateReq>,
) -> Response {
    if let Some(r) = require(&scopes, AdminScope::Moderate) {
        return r;
    }
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
    Extension(scopes): Extension<AdminScopes>,
    Path(id): Path<String>,
    Json(req): Json<ResolveReq>,
) -> Response {
    if let Some(r) = require(&scopes, AdminScope::Moderate) {
        return r;
    }
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

#[derive(Deserialize)]
struct BulkResolveReq {
    ids: Vec<String>,
    /// dismissed | content-removed | user-actioned | escalated
    action: String,
    note: Option<String>,
}

/// `POST /reports/bulk-resolve` — WC7 bulk actions: apply one resolution to many
/// reports (the spam-wave case, where a queue fills with the same complaint).
/// Partial success is normal and reported honestly: each id's outcome comes back
/// so the operator sees exactly what landed, rather than one all-or-nothing
/// status hiding the ones that were already resolved or never existed.
async fn bulk_resolve_reports(
    State(st): State<AdminState>,
    Extension(who): Extension<Account>,
    Extension(scopes): Extension<AdminScopes>,
    Json(req): Json<BulkResolveReq>,
) -> Response {
    if let Some(r) = require(&scopes, AdminScope::Moderate) {
        return r;
    }
    let Ok(action) = req.action.parse::<ResolveAction>() else {
        return (StatusCode::BAD_REQUEST, "unknown action").into_response();
    };
    if req.ids.is_empty() {
        return (StatusCode::BAD_REQUEST, "no report ids given").into_response();
    }
    // A bounded batch: this is a moderation convenience, not a bulk-import API.
    const MAX_BULK: usize = 500;
    if req.ids.len() > MAX_BULK {
        return (
            StatusCode::BAD_REQUEST,
            format!("at most {MAX_BULK} reports per call"),
        )
            .into_response();
    }

    let now = now_ms();
    let (mut resolved, mut missing) = (Vec::new(), Vec::new());
    for id in &req.ids {
        let resolution = ReportResolution {
            action,
            note: req.note.clone(),
            resolved_by: who.to_string(),
            at_ms: now,
            hold_release_at: now + 7 * 24 * 60 * 60 * 1000, // §12.1 grace
        };
        match st.reports.resolve_report(id, resolution).await {
            Ok(true) => resolved.push(id.clone()),
            // Already resolved or unknown — indistinguishable at the store, and
            // both mean "this one didn't change".
            Ok(false) => missing.push(id.clone()),
            Err(e) => return internal(e),
        }
    }
    audit(
        &st,
        &who,
        "report.bulk_resolve",
        &format!("{} reports", resolved.len()),
        &json!({ "action": req.action, "resolved": resolved, "unchanged": missing }),
    )
    .await;
    Json(json!({ "resolved": resolved, "unchanged": missing })).into_response()
}

// ---- WC7 room actions ----

/// `POST /channels/:name/freeze` — lock a channel; only `ns-admin` may post.
async fn freeze_channel(
    st: State<AdminState>,
    who: Extension<Account>,
    scopes: Extension<AdminScopes>,
    name: Path<String>,
) -> Response {
    set_frozen(st, who, scopes, name, true).await
}

/// `POST /channels/:name/unfreeze` — lift it.
async fn unfreeze_channel(
    st: State<AdminState>,
    who: Extension<Account>,
    scopes: Extension<AdminScopes>,
    name: Path<String>,
) -> Response {
    set_frozen(st, who, scopes, name, false).await
}

async fn set_frozen(
    State(st): State<AdminState>,
    Extension(who): Extension<Account>,
    Extension(scopes): Extension<AdminScopes>,
    Path(name): Path<String>,
    frozen: bool,
) -> Response {
    if let Some(r) = require(&scopes, AdminScope::Moderate) {
        return r;
    }
    let Ok(channel) = name.parse::<ChannelName>() else {
        return (StatusCode::BAD_REQUEST, "bad channel name").into_response();
    };
    match st.channels.channel(&channel).await {
        Ok(Some(_)) => {}
        Ok(None) => return (StatusCode::NOT_FOUND, "no such channel").into_response(),
        Err(e) => return internal(e),
    }
    if let Err(e) = st.channels.set_channel_frozen(&channel, frozen).await {
        return internal(e);
    }
    audit(
        &st,
        &who,
        if frozen {
            "channel.freeze"
        } else {
            "channel.unfreeze"
        },
        &channel.to_string(),
        &json!({ "channel": channel.to_string() }),
    )
    .await;
    StatusCode::NO_CONTENT.into_response()
}

/// `GET /namespaces/:name/detail` — everything the namespace detail page's
/// sub-tabs render: metadata, its channels, its roles, and its member set.
async fn namespace_detail(State(st): State<AdminState>, Path(name): Path<String>) -> Response {
    let Ok(ns) = name.parse::<weft_proto::NamespaceName>() else {
        return (StatusCode::BAD_REQUEST, "bad namespace name").into_response();
    };
    let detail = async {
        let Some(record) = st.namespaces.namespace(&ns).await? else {
            return Ok(None);
        };
        // A namespace's channels are the ones prefixed `#<ns>/`.
        let prefix = format!("#{ns}/");
        let mut channels = Vec::new();
        let mut members: Vec<String> = Vec::new();
        for (chan, _) in st.channels.list_channels().await? {
            if !chan.as_str().starts_with(&prefix) {
                continue;
            }
            if let Some(c) = st.channels.channel(&chan).await? {
                channels.push(dto::NamespaceChannel {
                    name: chan.to_string(),
                    kind: c.kind.to_string(),
                    policy: c.policy.to_string(),
                    category: c.category,
                    position: c.position,
                    frozen: c.frozen,
                    restricted: c.restricted,
                });
            }
            for m in st.memberships.members(&chan).await? {
                members.push(m.to_string());
            }
        }
        channels.sort_by(|a, b| {
            a.category
                .cmp(&b.category)
                .then(a.position.cmp(&b.position))
                .then(a.name.cmp(&b.name))
        });
        members.sort();
        members.dedup();

        let roles = st
            .roles
            .roles(&format!("ns:{ns}"))
            .await?
            .into_iter()
            .map(|r| dto::Role {
                name: r.name,
                color: r.color,
                caps: r.caps,
                hoist: r.hoist,
                position: r.position,
            })
            .collect();

        Ok::<_, StoreError>(Some(dto::NamespaceDetail {
            name: record.name.to_string(),
            owner: record.owner.to_string(),
            visibility: record.visibility,
            title: record.title,
            description: record.description,
            icon: record.icon,
            frozen: record.frozen,
            federation: record.federation,
            categories: record.categories,
            // Existence only — never who is in the quorum (§2.4).
            recovery_set: record.recovery_set.is_some(),
            root_key: record.root_key,
            channels,
            roles,
            members,
        }))
    }
    .await;
    match detail {
        Ok(Some(v)) => Json(v).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no such namespace").into_response(),
        Err(e) => internal(e),
    }
}

/// `GET /accounts/:name/dms` — the account's DM correspondents, so the user
/// detail can list conversations and open each one's thread.
async fn account_dms(State(st): State<AdminState>, Path(name): Path<String>) -> Response {
    let Ok(account) = name.parse::<Account>() else {
        return (StatusCode::BAD_REQUEST, "bad account").into_response();
    };
    match st.events.dm_partners(&account).await {
        Ok(list) => Json(
            list.into_iter()
                .map(|a| dto::DmPartner {
                    account: a.to_string(),
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
struct VisibilityReq {
    visibility: String,
}

/// `POST /namespaces/:name/visibility` — set the discovery tier (§2.2).
async fn set_ns_visibility(
    State(st): State<AdminState>,
    Extension(who): Extension<Account>,
    Extension(scopes): Extension<AdminScopes>,
    Path(name): Path<String>,
    Json(req): Json<VisibilityReq>,
) -> Response {
    if let Some(r) = require(&scopes, AdminScope::Moderate) {
        return r;
    }
    let Ok(ns) = name.parse::<weft_proto::NamespaceName>() else {
        return (StatusCode::BAD_REQUEST, "bad namespace name").into_response();
    };
    if !matches!(req.visibility.as_str(), "public" | "unlisted" | "private") {
        return (
            StatusCode::BAD_REQUEST,
            "visibility must be public|unlisted|private",
        )
            .into_response();
    }
    match st.namespaces.namespace(&ns).await {
        Ok(Some(_)) => {}
        Ok(None) => return (StatusCode::NOT_FOUND, "no such namespace").into_response(),
        Err(e) => return internal(e),
    }
    if let Err(e) = st
        .namespaces
        .set_namespace_visibility(&ns, &req.visibility)
        .await
    {
        return internal(e);
    }
    audit(
        &st,
        &who,
        "namespace.visibility",
        ns.as_str(),
        &json!({ "namespace": ns.to_string(), "visibility": req.visibility }),
    )
    .await;
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
struct FederationReq {
    /// `open` | `closed` (§11.10).
    mode: String,
}

/// `POST /namespaces/:name/federation` — toggle §11.10 auto-federation
/// reachability. `open` only has effect while the namespace is also `public`.
async fn set_ns_federation(
    State(st): State<AdminState>,
    Extension(who): Extension<Account>,
    Extension(scopes): Extension<AdminScopes>,
    Path(name): Path<String>,
    Json(req): Json<FederationReq>,
) -> Response {
    if let Some(r) = require(&scopes, AdminScope::Moderate) {
        return r;
    }
    let Ok(ns) = name.parse::<weft_proto::NamespaceName>() else {
        return (StatusCode::BAD_REQUEST, "bad namespace name").into_response();
    };
    let open = match req.mode.as_str() {
        "open" => true,
        "closed" => false,
        _ => return (StatusCode::BAD_REQUEST, "mode must be open|closed").into_response(),
    };
    let record = match st.namespaces.namespace(&ns).await {
        Ok(Some(r)) => r,
        Ok(None) => return (StatusCode::NOT_FOUND, "no such namespace").into_response(),
        Err(e) => return internal(e),
    };
    // Say why rather than storing a flag that silently does nothing (§11.10).
    if open && record.visibility != "public" {
        return (
            StatusCode::CONFLICT,
            "auto-federation needs public visibility — change that first",
        )
            .into_response();
    }
    if let Err(e) = st.namespaces.set_namespace_federation(&ns, open).await {
        return internal(e);
    }
    audit(
        &st,
        &who,
        "namespace.federation",
        ns.as_str(),
        &json!({ "namespace": ns.to_string(), "mode": req.mode }),
    )
    .await;
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
struct TakeoverReq {
    new_owner: String,
}

/// `POST /namespaces/:name/takeover` — §2.4 **rung 3 operator takeover**.
/// Applies immediately: rotates the root key, hands ownership to `new_owner`,
/// and records the rotation as operator-initiated in `root-history` forever.
///
/// Operator-only, deliberately not `admin.destroy`: seizing a namespace is the
/// single most powerful action the panel exposes, and a delegated admin holding
/// every scope must not be able to take communities over.
///
/// The wire path (`NS RECOVER`) needs a rotation signed by the **network key**,
/// which the human operator doesn't hold — the server does. So the panel runs
/// the rotation server-side through the store, which is the same operation the
/// scheduler applies, with the same audit mark.
async fn takeover_namespace(
    State(st): State<AdminState>,
    Extension(who): Extension<Account>,
    Path(name): Path<String>,
    Json(req): Json<TakeoverReq>,
) -> Response {
    if let Some(r) = require_operator(&st, &who).await {
        return r;
    }
    let Ok(ns) = name.parse::<weft_proto::NamespaceName>() else {
        return (StatusCode::BAD_REQUEST, "bad namespace name").into_response();
    };
    let Ok(new_owner) = req.new_owner.parse::<Account>() else {
        return (StatusCode::BAD_REQUEST, "bad account").into_response();
    };
    match st.namespaces.namespace(&ns).await {
        Ok(Some(_)) => {}
        Ok(None) => return (StatusCode::NOT_FOUND, "no such namespace").into_response(),
        Err(e) => return internal(e),
    }
    // Handing a namespace to an account that doesn't exist would leave it
    // ownerless — worse than the state we're fixing.
    match st.accounts.account_ulid(&new_owner).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return (StatusCode::NOT_FOUND, "no such account to hand it to").into_response()
        }
        Err(e) => return internal(e),
    }

    // A fresh root key the new owner will hold. The old one is superseded, so a
    // seizure the previous owner could undo with their key is not a seizure.
    let new_root = weft_crypto::Keypair::generate();
    if let Err(e) = st
        .namespaces
        .rotate_root(
            &ns,
            new_owner.as_str(),
            &new_root.public().to_b64(),
            true, // operator_initiated — permanent
            now_ms(),
        )
        .await
    {
        return internal(e);
    }
    audit(
        &st,
        &who,
        "namespace.takeover",
        ns.as_str(),
        &json!({
            "namespace": ns.to_string(),
            "new_owner": new_owner.to_string(),
            "new_root_key": new_root.public().to_b64(),
        }),
    )
    .await;
    // The new root **secret** is returned exactly once — it is not stored, so if
    // it isn't handed to the new owner now it is gone and the namespace will
    // need another takeover.
    Json(json!({
        "namespace": ns.to_string(),
        "new_owner": new_owner.to_string(),
        "new_root_key": new_root.public().to_b64(),
        "new_root_seed": new_root.seed_b64(),
    }))
    .into_response()
}

/// `POST /namespaces/:name/freeze` — WC7 **full freeze**: lock every channel in
/// a namespace. One rung above the per-channel freeze — only the namespace owner
/// and network operators may post through it, so a delegated `ns-admin` can't
/// talk (or quietly lift it). Applying it is `admin.destroy`, not `moderate`:
/// silencing a whole community is not a routine moderation call.
async fn freeze_namespace(
    st: State<AdminState>,
    who: Extension<Account>,
    scopes: Extension<AdminScopes>,
    name: Path<String>,
) -> Response {
    set_ns_frozen(st, who, scopes, name, true).await
}

/// `POST /namespaces/:name/unfreeze` — lift the full freeze.
async fn unfreeze_namespace(
    st: State<AdminState>,
    who: Extension<Account>,
    scopes: Extension<AdminScopes>,
    name: Path<String>,
) -> Response {
    set_ns_frozen(st, who, scopes, name, false).await
}

async fn set_ns_frozen(
    State(st): State<AdminState>,
    Extension(who): Extension<Account>,
    Extension(scopes): Extension<AdminScopes>,
    Path(name): Path<String>,
    frozen: bool,
) -> Response {
    if let Some(r) = require(&scopes, AdminScope::Destroy) {
        return r;
    }
    let Ok(ns) = name.parse::<weft_proto::NamespaceName>() else {
        return (StatusCode::BAD_REQUEST, "bad namespace name").into_response();
    };
    match st.namespaces.namespace(&ns).await {
        Ok(Some(_)) => {}
        Ok(None) => return (StatusCode::NOT_FOUND, "no such namespace").into_response(),
        Err(e) => return internal(e),
    }
    if let Err(e) = st.namespaces.set_namespace_frozen(&ns, frozen).await {
        return internal(e);
    }
    audit(
        &st,
        &who,
        if frozen {
            "namespace.freeze"
        } else {
            "namespace.unfreeze"
        },
        ns.as_str(),
        &json!({ "namespace": ns.to_string() }),
    )
    .await;
    StatusCode::NO_CONTENT.into_response()
}

/// `DELETE /channels/:name?confirm=<name>` — WC7 channel delete, reusing WC3's
/// typed-name gate. Drops the channel record; its messages are purged with it by
/// the store. Irreversible, so `admin.destroy`.
async fn delete_channel(
    State(st): State<AdminState>,
    Extension(who): Extension<Account>,
    Extension(scopes): Extension<AdminScopes>,
    Path(name): Path<String>,
    Query(q): Query<ConfirmQuery>,
) -> Response {
    if let Some(r) = require(&scopes, AdminScope::Destroy) {
        return r;
    }
    let Ok(channel) = name.parse::<ChannelName>() else {
        return (StatusCode::BAD_REQUEST, "bad channel name").into_response();
    };
    // The typed-name gate: the caller must echo the exact channel name.
    if q.confirm.as_deref() != Some(channel.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            "confirm= must echo the exact channel name",
        )
            .into_response();
    }
    match st.channels.delete_channel(&channel).await {
        Ok(true) => {
            audit(
                &st,
                &who,
                "channel.delete",
                &channel.to_string(),
                &json!({ "channel": channel.to_string() }),
            )
            .await;
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => (StatusCode::NOT_FOUND, "no such channel").into_response(),
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

/// WC4 DM-thread browse (§0 content boundary): the materialized conversation
/// between two accounts. An `e2ee` DM policy is "unavailable by policy" — no
/// plaintext is held or materialized (invariant 8).
async fn browse_dm(
    State(st): State<AdminState>,
    Path((a, b)): Path<(String, String)>,
    Query(q): Query<ScopeQuery>,
) -> Response {
    let (Ok(a), Ok(b)) = (a.parse::<Account>(), b.parse::<Account>()) else {
        return (StatusCode::BAD_REQUEST, "bad account").into_response();
    };
    let scope = Scope::dm(a.clone(), b.clone());
    let policy = st.dm_policy.to_string();

    if matches!(st.dm_policy, weft_proto::RetentionPolicy::E2ee) {
        return Json(dto::ThreadBrowse {
            participants: [a.to_string(), b.to_string()],
            policy,
            unavailable: true,
            messages: Vec::new(),
        })
        .into_response();
    }
    match browse(&st, scope, q.limit.unwrap_or(100)).await {
        Ok(messages) => Json(dto::ThreadBrowse {
            participants: [a.to_string(), b.to_string()],
            policy,
            unavailable: false,
            messages,
        })
        .into_response(),
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
    Extension(scopes): Extension<AdminScopes>,
    Path(msgid): Path<String>,
) -> Response {
    if let Some(r) = require(&scopes, AdminScope::Destroy) {
        return r;
    }
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

/// WC5 peer detail: the record plus the parsed signed manifest (shared channels,
/// pinned key fingerprint, negotiated history/media/typing/voice) and whether
/// the peer's network is netblocked (§11.6 — a netblock is how you sever a peer).
async fn peer_detail(State(st): State<AdminState>, Path(name): Path<String>) -> Response {
    let Ok(network) = name.parse::<weft_proto::NetworkName>() else {
        return (StatusCode::BAD_REQUEST, "bad network").into_response();
    };
    let detail = async {
        let Some(rec) = st.peers.peer(&network).await? else {
            return Ok(None);
        };
        let netblocked = st.netblocks.is_netblocked(&network).await?;
        // Prefer the mutually-acked manifest; fall back to the current proposal.
        let m_b64 = rec.acked_manifest.as_deref().unwrap_or(&rec.manifest);
        let manifest =
            weft_crypto::SignedManifest::from_b64(m_b64)
                .ok()
                .map(|sm| dto::PeerManifest {
                    key_fingerprint: fingerprint_hex(sm.signer().as_bytes()),
                    verified: sm.verify(),
                    channels: sm.manifest.channels.clone(),
                    history: sm.manifest.history.clone(),
                    media: sm.manifest.media.clone(),
                    typing: sm.manifest.typing,
                    voice: sm.manifest.voice,
                });
        Ok::<_, StoreError>(Some(dto::PeerDetail {
            peer: rec.peer.to_string(),
            scope: rec.scope,
            version: rec.version,
            acked: rec.acked_manifest.is_some(),
            severed: rec.severed,
            netblocked,
            created_ms: rec.created_ms,
            updated_ms: rec.updated_ms,
            manifest,
        }))
    }
    .await;
    match detail {
        Ok(Some(v)) => Json(v).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no such peer").into_response(),
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
    Extension(scopes): Extension<AdminScopes>,
    Json(req): Json<NetblockReq>,
) -> Response {
    if let Some(r) = require(&scopes, AdminScope::Federation) {
        return r;
    }
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
    Extension(scopes): Extension<AdminScopes>,
    Path(network): Path<String>,
) -> Response {
    if let Some(r) = require(&scopes, AdminScope::Federation) {
        return r;
    }
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
    Extension(scopes): Extension<AdminScopes>,
    Json(req): Json<BlockMediaReq>,
) -> Response {
    if let Some(r) = require(&scopes, AdminScope::Moderate) {
        return r;
    }
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
    Extension(scopes): Extension<AdminScopes>,
    Path(hash): Path<String>,
) -> Response {
    if let Some(r) = require(&scopes, AdminScope::Moderate) {
        return r;
    }
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

// ---- WC6 trust & keys: capability-token inspector + revocation epochs ----

#[derive(Deserialize)]
struct InspectReq {
    /// Root→leaf capability tokens (base64), one delegation link each.
    tokens: Vec<String>,
}

/// Render a token subject for display (never used for authorization).
fn render_subject(subject: &weft_crypto::Subject) -> String {
    match subject {
        weft_crypto::Subject::Key(k) => format!("key {}", fingerprint_hex(k.as_bytes())),
        weft_crypto::Subject::Account(ulid) => format!("account {ulid}"),
        weft_crypto::Subject::Foreign(user) => format!("foreign {user}"),
        weft_crypto::Subject::Unbound => "unbound (invite)".to_string(),
    }
}

/// WC6 capability-token inspector: parse a delegation chain and describe each
/// link (issuer, subject, scope, caps, expiry, revocation status, parent
/// linkage). A debugging tool — it does not assert authority (that's the
/// enforcement layer + the scope authority key); it reports what the tokens say
/// plus expiry/epoch status from the store. `admin.keys`.
async fn inspect_tokens(
    State(st): State<AdminState>,
    Extension(scopes): Extension<AdminScopes>,
    Json(req): Json<InspectReq>,
) -> Response {
    if let Some(r) = require(&scopes, AdminScope::Keys) {
        return r;
    }
    if req.tokens.is_empty() {
        return (StatusCode::BAD_REQUEST, "no tokens").into_response();
    }

    let mut parsed = Vec::with_capacity(req.tokens.len());
    for raw in &req.tokens {
        match weft_crypto::Token::from_b64(raw.trim()) {
            Ok(tok) => parsed.push(tok),
            Err(e) => {
                return Json(dto::TokenInspection {
                    links: Vec::new(),
                    chain_linked: false,
                    error: Some(format!("parse error: {e}")),
                })
                .into_response()
            }
        }
    }

    let now = now_ms() / 1000; // token expiry/epoch are unix seconds
    let mut chain_linked = parsed[0].grant.parent.is_none(); // root must be unparented
    let mut links = Vec::with_capacity(parsed.len());
    for (i, tok) in parsed.iter().enumerate() {
        let g = &tok.grant;
        let current_epoch = st.caps.scope_epoch(&g.scope.as_str()).await.unwrap_or(0);
        let parent_linked = if i == 0 {
            g.parent.is_none()
        } else {
            g.parent == Some(parsed[i - 1].hash())
        };
        if !parent_linked {
            chain_linked = false;
        }
        links.push(dto::TokenLink {
            issuer_fingerprint: fingerprint_hex(g.issuer.as_bytes()),
            subject: render_subject(&g.subject),
            scope: g.scope.as_str(),
            caps: g.caps.iter().map(|c| c.to_string()).collect(),
            epoch: g.epoch,
            expiry: g.expiry,
            expired: g.expiry != 0 && g.expiry < now,
            rooted: g.parent.is_none(),
            parent_linked,
            revoked: g.epoch < current_epoch,
            token_hash: fingerprint_hex(&tok.hash()),
        });
    }

    Json(dto::TokenInspection {
        links,
        chain_linked,
        error: None,
    })
    .into_response()
}

/// WC6 revocation set: a scope's current epoch + the grants a bump invalidates.
/// `admin.keys`.
async fn get_revocations(
    State(st): State<AdminState>,
    Extension(scopes): Extension<AdminScopes>,
    Query(q): Query<ScopeQuery>,
) -> Response {
    if let Some(r) = require(&scopes, AdminScope::Keys) {
        return r;
    }
    let scope = q.scope.as_deref().unwrap_or("*");
    let out = async {
        let epoch = st.caps.scope_epoch(scope).await?;
        let grants = st
            .caps
            .grants_at_scope(scope)
            .await?
            .into_iter()
            .map(|g| dto::Grant {
                subject: Some(g.subject),
                scope: g.scope,
                caps: g.caps,
                epoch: g.epoch,
                expiry: g.expiry,
            })
            .collect();
        Ok::<_, StoreError>(dto::RevocationScope {
            scope: scope.to_string(),
            epoch,
            grants,
        })
    }
    .await;
    match out {
        Ok(v) => Json(v).into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
struct BumpReq {
    scope: String,
}

/// WC6: bump a scope's revocation epoch — invalidates every grant/token issued
/// before it (§10.4). `admin.keys`, audited.
async fn bump_revocation(
    State(st): State<AdminState>,
    Extension(who): Extension<Account>,
    Extension(scopes): Extension<AdminScopes>,
    Json(req): Json<BumpReq>,
) -> Response {
    if let Some(r) = require(&scopes, AdminScope::Keys) {
        return r;
    }
    match st.caps.bump_epoch(&req.scope).await {
        Ok(epoch) => {
            audit(
                &st,
                &who,
                "revocation.bump",
                &req.scope,
                &json!({ "scope": req.scope, "epoch": epoch }),
            )
            .await;
            Json(dto::EpochBumped {
                scope: req.scope,
                epoch,
            })
            .into_response()
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

/// A short, human-readable key fingerprint: the first 10 bytes as uppercase
/// hex, grouped in 4s (e.g. `7F2A 91C4 …`). Display/identification only — never
/// used for authentication.
fn fingerprint_hex(bytes: &[u8]) -> String {
    let take = bytes.len().min(10);
    hex(&bytes[..take])
        .to_uppercase()
        .as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap_or_default())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The fingerprint of an enrolled Ed25519 device pubkey (WC4).
fn device_fingerprint(pubkey: &[u8; 32]) -> String {
    fingerprint_hex(pubkey)
}

// ---- helpers ----

fn internal(e: impl std::fmt::Display) -> Response {
    tracing::error!("admin store error: {e}");
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
}

/// WC2 write gate: returns a 403 response to short-circuit with unless the
/// caller holds the required admin scope; `None` = allowed. Reads need no guard
/// — the `require_admin` middleware already enforces the `admin.read` baseline
/// for every `/api/v1/*` route.
fn require(scopes: &AdminScopes, need: AdminScope) -> Option<Response> {
    (!scopes.has(need))
        .then(|| (StatusCode::FORBIDDEN, format!("requires {}", need.as_str())).into_response())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
