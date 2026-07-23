//! Operator authentication: an HTTP login (the WEFT auth flow is over QUIC/WS,
//! so the panel needs its own) → an HMAC-signed, http-only session cookie →
//! middleware that gates every `/api/*` route on a valid cookie for an account
//! that is still an operator.

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use weft_crypto::PasswordHash;
use weft_proto::Account;

use crate::AdminState;

const COOKIE: &str = "weft_admin";
const SESSION_TTL_SECS: u64 = 12 * 60 * 60;

/// Operator auth policy. `secret` signs session cookies (the network signing key
/// when embedded; a config value standalone). `operators` (config `[operators]`)
/// auto-hold **every** admin scope — the bootstrap admins.
pub struct AuthConfig {
    pub secret: Vec<u8>,
    pub operators: HashSet<Account>,
}

/// The admin capability scopes (WC2). RBAC replaces the old binary operator:
/// operators (config) hold all; delegated admins hold a subset via `admin`-scope
/// capability grants (dogfoods §10.4 — `GRANT admin admin.moderate <account>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdminScope {
    /// Observability — every non-content read. The baseline for any panel
    /// access: lists and entity details, but *not* message bodies.
    Read,
    /// Read message **content**: channel and DM history, and the context
    /// attached to a report. Split from `Read` because triaging a queue and
    /// reading everyone's conversations are very different powers — a junior
    /// moderator can do the former without the latter.
    Messages,
    /// Structural moderation: mute / ban / kick / unmute / unban, and freezing
    /// a channel.
    Moderate,
    /// Work the reports queue: resolve individually or in bulk.
    Reports,
    /// Account state: suspend / unsuspend / force logout. Not deletion.
    Accounts,
    /// The §13 media hash blocklist.
    Media,
    /// Remove **content**: delete a message. Reversible in the sense that a
    /// tombstone is a record, but the body is gone.
    Delete,
    /// Irreversible structural destruction: delete an account or a channel, and
    /// the namespace-wide full freeze.
    Destroy,
    /// Federation controls: netblocks, peers, sever/re-weave.
    Federation,
    /// Device / token / revocation-epoch management.
    Keys,
    /// Read the audit log — what every other admin has done. Separate because
    /// it is the surface that watches the watchers.
    Audit,
}

impl AdminScope {
    pub const ALL: [AdminScope; 11] = [
        AdminScope::Read,
        AdminScope::Messages,
        AdminScope::Moderate,
        AdminScope::Reports,
        AdminScope::Accounts,
        AdminScope::Media,
        AdminScope::Delete,
        AdminScope::Destroy,
        AdminScope::Federation,
        AdminScope::Keys,
        AdminScope::Audit,
    ];

    /// The canonical wire string (also the capability name at scope `admin`).
    pub fn as_str(self) -> &'static str {
        match self {
            AdminScope::Read => "admin.read",
            AdminScope::Messages => "admin.messages",
            AdminScope::Moderate => "admin.moderate",
            AdminScope::Reports => "admin.reports",
            AdminScope::Accounts => "admin.accounts",
            AdminScope::Media => "admin.media",
            AdminScope::Delete => "admin.delete",
            AdminScope::Destroy => "admin.destroy",
            AdminScope::Federation => "admin.federation",
            AdminScope::Keys => "admin.keys",
            AdminScope::Audit => "admin.audit",
        }
    }

    /// Scopes a **stored grant** of `self` also confers.
    ///
    /// The finer scopes above were split out of the original five, so an
    /// existing `admin.moderate` or `admin.destroy` grant must keep meaning
    /// exactly what it did when it was issued — otherwise this refactor would
    /// silently demote every delegated admin on every live deployment. New
    /// grants can name the leaf scopes directly.
    pub fn implied(self) -> &'static [AdminScope] {
        match self {
            // `moderate` used to cover the reports queue, media blocks, account
            // suspension and reading the content you were moderating.
            AdminScope::Moderate => &[
                AdminScope::Reports,
                AdminScope::Accounts,
                AdminScope::Media,
                AdminScope::Messages,
            ],
            // `destroy` used to cover message deletion too.
            AdminScope::Destroy => &[AdminScope::Delete],
            _ => &[],
        }
    }

    /// Parse a capability string — accepts the full `admin.read` or bare `read`.
    pub fn parse(cap: &str) -> Option<AdminScope> {
        Self::from_cap(cap)
    }

    /// Parse a capability string — accepts the full `admin.read` or bare `read`.
    fn from_cap(cap: &str) -> Option<AdminScope> {
        AdminScope::ALL.into_iter().find(|s| {
            let full = s.as_str();
            cap == full || cap == full.trim_start_matches("admin.")
        })
    }
}

/// The set of admin scopes an account holds this request. Injected into request
/// extensions by [`require_admin`]; handlers read it to gate writes.
#[derive(Debug, Clone, Default)]
pub struct AdminScopes(HashSet<AdminScope>);

impl AdminScopes {
    /// Every scope — an operator (config), or a `*`/`admin.*` grant.
    pub fn all() -> Self {
        Self(AdminScope::ALL.into_iter().collect())
    }

    pub fn has(&self, scope: AdminScope) -> bool {
        self.0.contains(&scope)
    }

    /// The held scopes as sorted wire strings (for `/me`).
    pub fn as_strings(&self) -> Vec<String> {
        let mut v: Vec<String> = self.0.iter().map(|s| s.as_str().to_string()).collect();
        v.sort();
        v
    }
}

/// Whether an account holds **operator** authority — the config seed set or the
/// DB-backed flag (§10.4). Distinct from "holds every admin scope": a delegated
/// `admin.*` grant also yields every scope, but only a true operator may change
/// *who is an admin*. Gating permission management on this closes the
/// privilege-escalation path where a delegated admin promotes itself.
pub(crate) async fn is_operator(st: &AdminState, account: &Account) -> bool {
    st.auth.operators.contains(account) || st.accounts.is_operator(account).await.unwrap_or(false)
}

/// Compute an account's admin scopes. Operators (config) hold all; otherwise the
/// live (unexpired) `admin`-scope capability grants for the account's ULID.
/// `None` = holds no admin access at all. Revocation is by `REVOKE` (the grant
/// row disappears) or expiry — both reflected here on the next request.
pub(crate) async fn admin_scopes(st: &AdminState, account: &Account) -> Option<AdminScopes> {
    // Operators hold all admin scopes — from the config seed set OR the
    // DB-backed flag (managed via `weftd admin`, §10.4).
    if is_operator(st, account).await {
        return Some(AdminScopes::all());
    }

    let ulid = st.accounts.account_ulid(account).await.ok()??;
    let grants = st.caps.grants_for(&ulid).await.ok()?;
    let now = now();

    let mut set = HashSet::new();
    for grant in grants.into_iter().filter(|g| g.scope == "admin") {
        if grant.expiry.is_some_and(|e| e < now) {
            continue; // expired grant — ignore
        }
        for cap in &grant.caps {
            if cap == "*" || cap == "admin.*" {
                set.extend(AdminScope::ALL);
            } else if let Some(scope) = AdminScope::from_cap(cap) {
                set.insert(scope);
                // Grants issued before the finer scopes existed keep their
                // original reach (see `implied`).
                set.extend(scope.implied().iter().copied());
            }
        }
    }
    // Anything at all implies the read baseline — a scope set that can't sign in
    // would be a grant that silently does nothing.
    if !set.is_empty() {
        set.insert(AdminScope::Read);
    }

    (!set.is_empty()).then_some(AdminScopes(set))
}

#[derive(Deserialize)]
pub struct LoginReq {
    account: String,
    password: String,
}

/// `POST /api/login` — verify password + admin access, set the session cookie.
pub async fn login(State(st): State<AdminState>, Json(req): Json<LoginReq>) -> Response {
    // Uniform failure — never distinguish "no such account" from "bad password"
    // from "not an admin" (anti-enumeration, mirrors AUTH-FAILED).
    let Ok(account) = req.account.parse::<Account>() else {
        return unauthorized();
    };
    let ok = matches!(st.accounts.password_phc(&account).await, Ok(Some(phc))
        if PasswordHash::from_phc(&phc).map(|h| h.verify(&req.password)).unwrap_or(false));
    if !ok {
        return unauthorized();
    }
    // Panel access requires the `admin.read` baseline (operators auto-hold it).
    match admin_scopes(&st, &account).await {
        Some(s) if s.has(AdminScope::Read) => {}
        _ => return unauthorized(),
    }
    // WC7: a suspended account can't use the panel either (uniform failure).
    if st.accounts.is_suspended(&account).await.unwrap_or(false) {
        return unauthorized();
    }

    let token = make_token(&st.auth.secret, &account, now() + SESSION_TTL_SECS);
    let cookie = format!(
        "{COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={SESSION_TTL_SECS}; Secure"
    );
    (
        [(header::SET_COOKIE, cookie)],
        Json(serde_json::json!({ "account": account.to_string() })),
    )
        .into_response()
}

/// `POST /api/logout` — clear the cookie.
pub async fn logout() -> Response {
    let cookie = format!("{COOKIE}=; HttpOnly; Path=/; Max-Age=0");
    ([(header::SET_COOKIE, cookie)], StatusCode::NO_CONTENT).into_response()
}

/// Gate `/api/*`: a valid session cookie for a current admin (holds the
/// `admin.read` baseline). Injects the acting `Account` **and** its
/// [`AdminScopes`] into request extensions — handlers read the scopes to gate
/// writes. The gate is uniform 401 (anti-enumeration); per-scope write denials
/// are 403 inside the handlers.
pub async fn require_admin(State(st): State<AdminState>, mut req: Request, next: Next) -> Response {
    let account = req
        .headers()
        .get(header::COOKIE)
        .and_then(|c| c.to_str().ok())
        .and_then(session_cookie)
        .and_then(|tok| verify_token(&st.auth.secret, &tok));

    let Some(account) = account else {
        return unauthorized();
    };
    match admin_scopes(&st, &account).await {
        Some(scopes) if scopes.has(AdminScope::Read) => {
            req.extensions_mut().insert(account);
            req.extensions_mut().insert(scopes);
            next.run(req).await
        }
        _ => unauthorized(),
    }
}

pub(crate) fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({ "error": "unauthorized" })),
    )
        .into_response()
}

fn session_cookie(header: &str) -> Option<String> {
    header
        .split(';')
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE)
        .map(|(_, v)| v.to_string())
}

// ---- signed tokens: `<account>|<exp>|<hmac-sha256>` ----

type HmacSha256 = Hmac<Sha256>;
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

fn make_token(secret: &[u8], account: &Account, exp: u64) -> String {
    let msg = format!("{account}|{exp}");
    let sig = sign(secret, msg.as_bytes());
    format!("{msg}|{}", B64.encode(sig))
}

fn verify_token(secret: &[u8], token: &str) -> Option<Account> {
    let mut parts = token.splitn(3, '|');
    let account = parts.next()?;
    let exp = parts.next()?;
    let sig = B64.decode(parts.next()?).ok()?;
    let mut mac = HmacSha256::new_from_slice(secret).ok()?;
    mac.update(format!("{account}|{exp}").as_bytes());
    mac.verify_slice(&sig).ok()?; // constant-time
    if now() > exp.parse::<u64>().ok()? {
        return None;
    }
    account.parse().ok()
}

fn sign(secret: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(msg);
    mac.finalize().into_bytes().to_vec()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Domain-separation label so the derived cookie key is independent of the
/// input secret's other uses. Bump the version suffix to force re-login.
const COOKIE_KEY_LABEL: &[u8] = b"weft-admin-cookie-key-v1";

/// Convenience for callers that build the config.
///
/// `secret` may be a high-value key with other duties — in the embedded server
/// it is the network Ed25519 signing-key seed. To avoid cross-primitive key
/// reuse (threat-model F-1), the cookie-signing key is **derived** from it via a
/// labeled SHA-256 rather than used raw: `SHA-256(label ‖ secret)`. Learning the
/// derived cookie key therefore no longer reveals the input secret.
pub fn config(secret: Vec<u8>, operators: impl IntoIterator<Item = Account>) -> AuthConfig {
    let mut h = Sha256::new();
    h.update(COOKIE_KEY_LABEL);
    h.update(&secret);
    let derived = h.finalize().to_vec();
    AuthConfig {
        secret: derived,
        operators: operators.into_iter().collect::<HashSet<_>>(),
    }
}
