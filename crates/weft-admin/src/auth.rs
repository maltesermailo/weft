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
use sha2::Sha256;
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
    /// Observability — every read. The baseline for any panel access.
    Read,
    /// Structural moderation: mute/ban/kick, resolve reports, media blocks.
    Moderate,
    /// Irreversible: delete account, delete message.
    Destroy,
    /// Federation controls: netblocks (peers/transit later).
    Federation,
    /// Device/token/revocation management (reserved for WC6).
    Keys,
}

impl AdminScope {
    pub const ALL: [AdminScope; 5] = [
        AdminScope::Read,
        AdminScope::Moderate,
        AdminScope::Destroy,
        AdminScope::Federation,
        AdminScope::Keys,
    ];

    /// The canonical wire string (also the capability name at scope `admin`).
    pub fn as_str(self) -> &'static str {
        match self {
            AdminScope::Read => "admin.read",
            AdminScope::Moderate => "admin.moderate",
            AdminScope::Destroy => "admin.destroy",
            AdminScope::Federation => "admin.federation",
            AdminScope::Keys => "admin.keys",
        }
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

/// Compute an account's admin scopes. Operators (config) hold all; otherwise the
/// live (unexpired) `admin`-scope capability grants for the account's ULID.
/// `None` = holds no admin access at all. Revocation is by `REVOKE` (the grant
/// row disappears) or expiry — both reflected here on the next request.
pub(crate) async fn admin_scopes(st: &AdminState, account: &Account) -> Option<AdminScopes> {
    if st.auth.operators.contains(account) {
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
            }
        }
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

/// Convenience for callers that build the config.
pub fn config(secret: Vec<u8>, operators: impl IntoIterator<Item = Account>) -> AuthConfig {
    AuthConfig {
        secret,
        operators: operators.into_iter().collect::<HashSet<_>>(),
    }
}
