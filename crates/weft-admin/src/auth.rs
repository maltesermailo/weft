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
/// when embedded; a config value standalone). `operators` gate who may log in.
pub struct AuthConfig {
    pub secret: Vec<u8>,
    pub operators: HashSet<Account>,
}

#[derive(Deserialize)]
pub struct LoginReq {
    account: String,
    password: String,
}

/// `POST /api/login` — verify password + operator status, set the session cookie.
pub async fn login(State(st): State<AdminState>, Json(req): Json<LoginReq>) -> Response {
    // Uniform failure — never distinguish "no such account" from "bad password"
    // from "not an operator" (anti-enumeration, mirrors AUTH-FAILED).
    let Ok(account) = req.account.parse::<Account>() else {
        return unauthorized();
    };
    if !st.auth.operators.contains(&account) {
        return unauthorized();
    }
    let ok = matches!(st.accounts.password_phc(&account).await, Ok(Some(phc))
        if PasswordHash::from_phc(&phc).map(|h| h.verify(&req.password)).unwrap_or(false));
    if !ok {
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

/// Gate `/api/*`: a valid session cookie for a current operator. Injects the
/// acting `Account` into request extensions for handlers (the moderator).
pub async fn require_operator(State(st): State<AdminState>, mut req: Request, next: Next) -> Response {
    let account = req
        .headers()
        .get(header::COOKIE)
        .and_then(|c| c.to_str().ok())
        .and_then(session_cookie)
        .and_then(|tok| verify_token(&st.auth.secret, &tok));

    match account {
        Some(account) if st.auth.operators.contains(&account) => {
            req.extensions_mut().insert(account);
            next.run(req).await
        }
        _ => unauthorized(),
    }
}

pub(crate) fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "unauthorized" }))).into_response()
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
