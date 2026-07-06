//! End-to-end: boot the router against an in-memory store and exercise the auth
//! gate + an authed read, so the scaffold is proven, not just compiled.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use tower::ServiceExt; // oneshot
use weft_admin::{auth, AdminState};
use weft_crypto::PasswordHash;
use weft_store::{AccountStore, MemoryStore};

const PASSWORD: &str = "correct-horse-battery";

async fn build() -> axum::Router {
    let store = Arc::new(MemoryStore::default());
    let admin: weft_proto::Account = "admin".parse().unwrap();
    store
        .register(&admin, PasswordHash::new(PASSWORD).as_phc())
        .await
        .unwrap();
    // A non-operator account, to prove operator gating.
    let user: weft_proto::Account = "mallory".parse().unwrap();
    store
        .register(&user, PasswordHash::new(PASSWORD).as_phc())
        .await
        .unwrap();

    let auth = auth::config(b"a-test-session-secret".to_vec(), [admin]);
    weft_admin::router(AdminState::from_store(store, auth, "test.net".into()))
}

fn get(path: &str, cookie: Option<&str>) -> Request<Body> {
    let mut b = Request::get(path);
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, c);
    }
    b.body(Body::empty()).unwrap()
}

fn login(account: &str, password: &str) -> Request<Body> {
    Request::post("/admin/api/login")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(format!(
            r#"{{"account":"{account}","password":"{password}"}}"#
        )))
        .unwrap()
}

fn post_json(path: &str, cookie: &str, body: &str) -> Request<Body> {
    Request::post(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, cookie)
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Log in as the operator and return the session cookie.
async fn session(app: &axum::Router) -> String {
    let res = app.clone().oneshot(login("admin", PASSWORD)).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    res.headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

/// Records live calls so tests can assert the embedded path fired.
#[derive(Clone, Default)]
struct MockLive {
    ejects: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    deletes: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl weft_admin::Live for MockLive {
    async fn eject(&self, channel: &weft_proto::ChannelName, account: &weft_proto::Account) {
        self.ejects
            .lock()
            .unwrap()
            .push((channel.to_string(), account.to_string()));
    }
    async fn delete_message(&self, msgid: &weft_proto::MsgId, _by: &weft_proto::Account) -> bool {
        self.deletes.lock().unwrap().push(msgid.to_string());
        true
    }
}

#[tokio::test]
async fn kick_requires_live_then_ejects() {
    // Standalone (no live): kick is 501.
    let app = build().await;
    let cookie = session(&app).await;
    let res = app
        .oneshot(post_json(
            "/admin/api/moderation",
            &cookie,
            r##"{"verb":"kick","scope":"#general","account":"mallory"}"##,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_IMPLEMENTED);

    // Embedded (with live): kick 204s and force-parts the target.
    let live = MockLive::default();
    let app = build_with_live(Arc::new(live.clone())).await;
    let cookie = session(&app).await;
    let res = app
        .oneshot(post_json(
            "/admin/api/moderation",
            &cookie,
            r##"{"verb":"kick","scope":"#general","account":"mallory"}"##,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        live.ejects.lock().unwrap().as_slice(),
        &[("#general".to_string(), "mallory".to_string())]
    );
}

#[tokio::test]
async fn delete_message_requires_live() {
    // A syntactically valid msgid: `<network>/<ULID>` (the slash rides the
    // wildcard route).
    let msgid = "test.net/01ARZ3NDEKTSV4RRFFQ69G5FAV";

    // Standalone (no live): 501.
    let app = build().await;
    let cookie = session(&app).await;
    let res = app
        .oneshot(Request::delete(format!("/admin/api/messages/{msgid}")).header(header::COOKIE, &cookie).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_IMPLEMENTED);

    // Embedded (with live): 204 + recorded.
    let live = MockLive::default();
    let app = build_with_live(Arc::new(live.clone())).await;
    let cookie = session(&app).await;
    let res = app
        .oneshot(Request::delete(format!("/admin/api/messages/{msgid}")).header(header::COOKIE, &cookie).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert_eq!(live.deletes.lock().unwrap().len(), 1);
}

async fn build_with_live(live: Arc<dyn weft_admin::Live>) -> axum::Router {
    let store = Arc::new(MemoryStore::default());
    let admin: weft_proto::Account = "admin".parse().unwrap();
    store
        .register(&admin, PasswordHash::new(PASSWORD).as_phc())
        .await
        .unwrap();
    let auth = auth::config(b"a-test-session-secret".to_vec(), [admin]);
    weft_admin::router(AdminState::from_store(store, auth, "test.net".into()).with_live(live))
}

#[tokio::test]
async fn serves_the_spa() {
    // The SPA shell is public (the API under it is gated); it loads at /admin.
    let res = build().await.oneshot(get("/admin", None)).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let ct = res.headers().get(header::CONTENT_TYPE).unwrap().to_str().unwrap();
    assert!(ct.contains("text/html"));
}

#[tokio::test]
async fn auth_gate_and_operator_login() {
    let app = build().await;

    // Unauthed reads are rejected.
    let res = app.clone().oneshot(get("/admin/api/stats", None)).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // Wrong password → uniform 401.
    let res = app.clone().oneshot(login("admin", "wrong")).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // A valid account that isn't an operator → 401 (same code, anti-enumeration).
    let res = app.clone().oneshot(login("mallory", PASSWORD)).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // Operator login → 200 + session cookie.
    let res = app.clone().oneshot(login("admin", PASSWORD)).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let cookie = res
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();

    // The cookie unlocks reads.
    let res = app.clone().oneshot(get("/admin/api/stats", Some(&cookie))).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let res = app.oneshot(get("/admin/api/accounts", Some(&cookie))).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // A tampered cookie is rejected.
    let bad = format!("{cookie}tamper");
    let res = build().await.oneshot(get("/admin/api/stats", Some(&bad))).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}
