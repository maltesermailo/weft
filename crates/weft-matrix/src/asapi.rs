//! The appservice HTTP surface: what the companion homeserver calls *us* on.
//!
//! One endpoint matters: `PUT /_matrix/app/v1/transactions/{txnId}` — the
//! homeserver pushing batches of events. The contract is at-least-once with
//! retries until it sees 200, so the endpoint is idempotent: a replayed txn id
//! answers 200 without re-forwarding.

use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::put;
use serde_json::Value;
use tokio::sync::mpsc;

/// One pushed transaction: its id and the timeline events it carried.
#[derive(Debug)]
pub struct Txn {
    pub id: String,
    pub events: Vec<Value>,
}

#[derive(Clone)]
struct AsState {
    hs_token: String,
    txns: mpsc::Sender<Txn>,
    /// Recently seen txn ids, bounded — the homeserver only retries the txn it
    /// is stuck on, so a small window is plenty.
    seen: Arc<Mutex<(VecDeque<String>, HashSet<String>)>>,
}

const SEEN_WINDOW: usize = 256;

pub fn router(hs_token: String, txns: mpsc::Sender<Txn>) -> axum::Router {
    let state = AsState {
        hs_token,
        txns,
        seen: Arc::new(Mutex::new((VecDeque::new(), HashSet::new()))),
    };

    axum::Router::new()
        // The stable path, plus the legacy unprefixed one older homeservers use.
        .route("/_matrix/app/v1/transactions/:txn_id", put(transaction))
        .route("/transactions/:txn_id", put(transaction))
        .with_state(state)
}

async fn transaction(
    State(st): State<AsState>,
    Path(txn_id): Path<String>,
    headers: axum::http::HeaderMap,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    body: String,
) -> Response {
    if !authorized(&st.hs_token, &headers, query.as_deref()) {
        // Per spec: 401 without a usable token, 403 on a wrong one. One branch
        // suffices — the homeserver treats both as "fix your registration".
        return (StatusCode::FORBIDDEN, "{}").into_response();
    }

    // Idempotency before forwarding: a retry of a seen txn is a success we
    // already had, not new work.
    {
        let mut seen = st.seen.lock().expect("seen lock");
        if seen.1.contains(&txn_id) {
            return ok();
        }

        seen.0.push_back(txn_id.clone());
        seen.1.insert(txn_id.clone());
        if seen.0.len() > SEEN_WINDOW {
            if let Some(old) = seen.0.pop_front() {
                seen.1.remove(&old);
            }
        }
    }

    let events = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v.get("events").and_then(Value::as_array).cloned())
        .unwrap_or_default();

    // Block rather than drop: the homeserver's queue is the durable one, and
    // answering 200 for events we discarded would lose them forever.
    if st.txns.send(Txn { id: txn_id, events }).await.is_err() {
        return (StatusCode::SERVICE_UNAVAILABLE, "{}").into_response();
    }

    ok()
}

fn authorized(hs_token: &str, headers: &axum::http::HeaderMap, query: Option<&str>) -> bool {
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    // The legacy transport is `?access_token=` — still emitted by older
    // homeservers, so accepted alongside the header.
    let legacy = query.and_then(|q| {
        q.split('&')
            .find_map(|kv| kv.strip_prefix("access_token="))
            .map(str::to_string)
    });

    bearer == Some(hs_token) || legacy.as_deref() == Some(hs_token)
}

fn ok() -> Response {
    (StatusCode::OK, "{}").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt as _;

    fn put_txn(id: &str, token: Option<&str>, body: &str) -> axum::http::Request<axum::body::Body> {
        let mut req = axum::http::Request::builder()
            .method("PUT")
            .uri(format!("/_matrix/app/v1/transactions/{id}"));
        if let Some(token) = token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }

        req.body(axum::body::Body::from(body.to_string())).unwrap()
    }

    #[tokio::test]
    async fn transactions_are_authed_deduped_and_forwarded() {
        let (tx, mut rx) = mpsc::channel(8);
        let app = router("secret".into(), tx);

        // No token / wrong token: refused, nothing forwarded.
        let res = app
            .clone()
            .oneshot(put_txn("t1", None, r#"{"events":[]}"#))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let res = app
            .clone()
            .oneshot(put_txn("t1", Some("wrong"), r#"{"events":[]}"#))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // A good push forwards its events.
        let res = app
            .clone()
            .oneshot(put_txn(
                "t1",
                Some("secret"),
                r#"{"events":[{"type":"m.room.message"}]}"#,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let txn = rx.try_recv().expect("forwarded");
        assert_eq!(txn.id, "t1");
        assert_eq!(txn.events.len(), 1);

        // The homeserver retries until it sees 200 — a replay answers 200
        // without forwarding again, or every retry would double-post.
        let res = app
            .clone()
            .oneshot(put_txn(
                "t1",
                Some("secret"),
                r#"{"events":[{"type":"m.room.message"}]}"#,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(rx.try_recv().is_err(), "replay must not re-forward");
    }
}
