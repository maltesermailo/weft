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
    Request::post("/admin/api/v1/login")
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
            "/admin/api/v1/moderation",
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
            "/admin/api/v1/moderation",
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
        .oneshot(
            Request::delete(format!("/admin/api/v1/messages/{msgid}"))
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_IMPLEMENTED);

    // Embedded (with live): 204 + recorded.
    let live = MockLive::default();
    let app = build_with_live(Arc::new(live.clone())).await;
    let cookie = session(&app).await;
    let res = app
        .oneshot(
            Request::delete(format!("/admin/api/v1/messages/{msgid}"))
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert_eq!(live.deletes.lock().unwrap().len(), 1);
}

fn del(path: &str, cookie: &str) -> Request<Body> {
    Request::delete(path)
        .header(header::COOKIE, cookie)
        .body(Body::empty())
        .unwrap()
}

async fn body_string(res: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn operator_deletes_a_user_but_not_themselves() {
    let app = build().await;
    let cookie = session(&app).await;

    // The enriched account list carries mallory before the delete.
    let list = body_string(
        app.clone()
            .oneshot(get("/admin/api/v1/accounts", Some(&cookie)))
            .await
            .unwrap(),
    )
    .await;
    assert!(list.contains("mallory") && list.contains("\"operator\":true"));

    // Delete mallory → 204, then she's gone and a second delete is 404.
    let res = app
        .clone()
        .oneshot(del("/admin/api/v1/accounts/mallory", &cookie))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let list = body_string(
        app.clone()
            .oneshot(get("/admin/api/v1/accounts", Some(&cookie)))
            .await
            .unwrap(),
    )
    .await;
    assert!(!list.contains("mallory"));
    let res = app
        .clone()
        .oneshot(del("/admin/api/v1/accounts/mallory", &cookie))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // An operator can't delete themselves.
    let res = app
        .oneshot(del("/admin/api/v1/accounts/admin", &cookie))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn account_messages_lists_a_users_posts() {
    use weft_store::{EventKind, EventRecord, EventStore, Scope};
    let store = Arc::new(MemoryStore::default());
    let admin: weft_proto::Account = "admin".parse().unwrap();
    store
        .register(&admin, PasswordHash::new(PASSWORD).as_phc())
        .await
        .unwrap();
    // Two messages authored by `poster@test.net` in #general.
    let poster: weft_proto::UserRef = "poster@test.net".parse().unwrap();
    for (n, body) in [("A", "hello"), ("B", "world")] {
        let ulid = weft_proto::Ulid::new();
        let msgid = weft_proto::MsgId::new("test.net".parse().unwrap(), ulid);
        store
            .append(EventRecord {
                scope: Scope::Channel("#general".parse().unwrap()),
                msgid: msgid.clone(),
                root: msgid,
                sender: poster.clone(),
                kind: EventKind::Message {
                    body: format!("{n}:{body}"),
                    meta: Default::default(),
                },
            })
            .await
            .unwrap();
    }
    let auth = auth::config(b"a-test-session-secret".to_vec(), [admin]);
    let app = weft_admin::router(AdminState::from_store(store, auth, "test.net".into()));
    let cookie = session(&app).await;

    let body = body_string(
        app.oneshot(get("/admin/api/v1/accounts/poster/messages", Some(&cookie)))
            .await
            .unwrap(),
    )
    .await;
    assert!(
        body.contains("A:hello") && body.contains("B:world"),
        "{body}"
    );
    assert!(body.contains("#general"));
}

#[tokio::test]
async fn netblock_and_media_block_endpoints() {
    let app = build().await;
    let cookie = session(&app).await;

    // Netblock: add → list contains it → remove.
    let res = app
        .clone()
        .oneshot(post_json(
            "/admin/api/v1/netblocks",
            &cookie,
            r#"{"network":"evil.example","reason":"spam"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let list = body_string(
        app.clone()
            .oneshot(get("/admin/api/v1/netblocks", Some(&cookie)))
            .await
            .unwrap(),
    )
    .await;
    assert!(list.contains("evil.example"));
    let res = app
        .clone()
        .oneshot(del("/admin/api/v1/netblocks/evil.example", &cookie))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);

    // Media block: add → list contains it → unblock.
    let res = app
        .clone()
        .oneshot(post_json(
            "/admin/api/v1/media-blocks",
            &cookie,
            r#"{"hash":"b3deadbeef","reason":"csam"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let list = body_string(
        app.clone()
            .oneshot(get("/admin/api/v1/media-blocks", Some(&cookie)))
            .await
            .unwrap(),
    )
    .await;
    assert!(list.contains("b3deadbeef"));
    let res = app
        .oneshot(del("/admin/api/v1/media-blocks/b3deadbeef", &cookie))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn write_actions_land_in_the_audit_log() {
    let app = build().await;
    let cookie = session(&app).await;

    // The log starts empty.
    let empty = body_string(
        app.clone()
            .oneshot(get("/admin/api/v1/audit", Some(&cookie)))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(empty, "[]");

    // Two write actions: a netblock add, then a media block.
    for (path, body) in [
        (
            "/admin/api/v1/netblocks",
            r#"{"network":"evil.example","reason":"spam"}"#,
        ),
        (
            "/admin/api/v1/media-blocks",
            r#"{"hash":"b3deadbeef","reason":"csam"}"#,
        ),
    ] {
        let res = app
            .clone()
            .oneshot(post_json(path, &cookie, body))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
    }

    // Both are recorded, newest-first, attributed to the operator, chained
    // (seq 2's prev_hash == seq 1's hash), and the reason is NOT stored raw.
    let log: serde_json::Value = serde_json::from_str(
        &body_string(
            app.clone()
                .oneshot(get("/admin/api/v1/audit", Some(&cookie)))
                .await
                .unwrap(),
        )
        .await,
    )
    .unwrap();
    let rows = log.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["action"], "media.block"); // newest first
    assert_eq!(rows[0]["target"], "b3deadbeef");
    assert_eq!(rows[0]["operator"], "admin");
    assert_eq!(rows[1]["action"], "netblock.add");
    assert_eq!(rows[1]["seq"], 1);
    assert_eq!(rows[0]["seq"], 2);
    assert_eq!(rows[0]["prev_hash"], rows[1]["hash"], "chain links");
    let raw = log.to_string();
    assert!(
        !raw.contains("spam") && !raw.contains("csam"),
        "reasons digested, not stored raw"
    );

    // The action filter narrows the log.
    let filtered = body_string(
        app.oneshot(get("/admin/api/v1/audit?action=media.block", Some(&cookie)))
            .await
            .unwrap(),
    )
    .await;
    assert!(filtered.contains("b3deadbeef") && !filtered.contains("evil.example"));
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
    let ct = res
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(ct.contains("text/html"));
}

#[tokio::test]
async fn auth_gate_and_operator_login() {
    let app = build().await;

    // Unauthed reads are rejected.
    let res = app
        .clone()
        .oneshot(get("/admin/api/v1/stats", None))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // Wrong password → uniform 401.
    let res = app.clone().oneshot(login("admin", "wrong")).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // A valid account that isn't an operator → 401 (same code, anti-enumeration).
    let res = app
        .clone()
        .oneshot(login("mallory", PASSWORD))
        .await
        .unwrap();
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
    let res = app
        .clone()
        .oneshot(get("/admin/api/v1/stats", Some(&cookie)))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let res = app
        .oneshot(get("/admin/api/v1/accounts", Some(&cookie)))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // A tampered cookie is rejected.
    let bad = format!("{cookie}tamper");
    let res = build()
        .await
        .oneshot(get("/admin/api/v1/stats", Some(&bad)))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}
