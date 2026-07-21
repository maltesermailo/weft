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
async fn account_delete_is_typed_name_confirmed_and_self_delete_blocked() {
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

    // WC3: delete WITHOUT the typed-name confirmation → 400 (no effect).
    let res = app
        .clone()
        .oneshot(del("/admin/api/v1/accounts/mallory", &cookie))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    // A mismatched confirmation → 400.
    let res = app
        .clone()
        .oneshot(del("/admin/api/v1/accounts/mallory?confirm=molly", &cookie))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // An operator can't delete themselves (checked before confirmation).
    let res = app
        .clone()
        .oneshot(del("/admin/api/v1/accounts/admin?confirm=admin", &cookie))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn account_delete_schedules_a_grace_window_and_restores() {
    use weft_store::AccountStore;

    let store = Arc::new(MemoryStore::default());
    for name in ["admin", "mallory"] {
        store
            .register(&name.parse().unwrap(), PasswordHash::new(PASSWORD).as_phc())
            .await
            .unwrap();
    }
    let auth = auth::config(
        b"a-test-session-secret".to_vec(),
        ["admin".parse().unwrap()],
    );
    let app = weft_admin::router(AdminState::from_store(
        Arc::clone(&store),
        auth,
        "test.net".into(),
    ));
    let cookie = session(&app).await;
    let mallory: weft_proto::Account = "mallory".parse().unwrap();

    // Confirmed delete → the account is *scheduled*, not gone: still listed,
    // now flagged with a purge time (recoverable during the grace window).
    let res = app
        .clone()
        .oneshot(del(
            "/admin/api/v1/accounts/mallory?confirm=mallory",
            &cookie,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(body_string(res).await.contains("purge_at"));
    assert!(store.deletion_scheduled(&mallory).await.unwrap().is_some());
    let list = body_string(
        app.clone()
            .oneshot(get("/admin/api/v1/accounts", Some(&cookie)))
            .await
            .unwrap(),
    )
    .await;
    assert!(
        list.contains("mallory") && list.contains("deletion_scheduled"),
        "still present during the grace window, flagged pending: {list}"
    );

    // Restore cancels it → no longer scheduled; a second restore is 404.
    let res = app
        .clone()
        .oneshot(post_json(
            "/admin/api/v1/accounts/mallory/restore",
            &cookie,
            "",
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert!(store.deletion_scheduled(&mallory).await.unwrap().is_none());
    let res = app
        .oneshot(post_json(
            "/admin/api/v1/accounts/mallory/restore",
            &cookie,
            "",
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
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

#[tokio::test]
async fn account_detail_carries_devices_and_related_and_channel_roster() {
    use weft_proto::{ChannelKind, RetentionPolicy};
    use weft_store::{AccountStore, ChannelStore, MembershipStore};

    let store = Arc::new(MemoryStore::default());
    for name in ["admin", "alice", "bob", "carol"] {
        store
            .register(&name.parse().unwrap(), PasswordHash::new(PASSWORD).as_phc())
            .await
            .unwrap();
    }
    let alice: weft_proto::Account = "alice".parse().unwrap();
    let bob: weft_proto::Account = "bob".parse().unwrap();
    let carol: weft_proto::Account = "carol".parse().unwrap();
    // alice + bob share the corp.test domain; carol is elsewhere.
    store
        .upsert_verification(&alice, "email", "alice@corp.test")
        .await
        .unwrap();
    store
        .upsert_verification(&bob, "email", "bob@corp.test")
        .await
        .unwrap();
    store
        .upsert_verification(&carol, "email", "carol@other.test")
        .await
        .unwrap();
    // alice has one enrolled device.
    store
        .enroll_device(
            &alice,
            [
                0x7f, 0x2a, 0x91, 0xc4, 0xe8, 0x0b, 0xd3, 0xf6, 0x55, 0xa1, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ],
        )
        .await
        .unwrap();
    // A channel with alice + bob as persistent members.
    let chan: weft_proto::ChannelName = "#team".parse().unwrap();
    store
        .upsert_channel(&chan, RetentionPolicy::Permanent, ChannelKind::Text)
        .await
        .unwrap();
    store.set_membership(&alice, &chan).await.unwrap();
    store.set_membership(&bob, &chan).await.unwrap();

    let auth = auth::config(
        b"a-test-session-secret".to_vec(),
        ["admin".parse().unwrap()],
    );
    let app = weft_admin::router(AdminState::from_store(store, auth, "test.net".into()));
    let cookie = session(&app).await;

    // alice's detail: her device fingerprint + bob as a related account (not carol).
    let detail = body_string(
        app.clone()
            .oneshot(get("/admin/api/v1/accounts/alice", Some(&cookie)))
            .await
            .unwrap(),
    )
    .await;
    assert!(detail.contains("7F2A 91C4"), "device fingerprint: {detail}");
    assert!(
        detail.contains("bob") && !detail.contains("carol"),
        "related by domain: {detail}"
    );

    // The channel roster lists both members.
    let chan_detail = body_string(
        app.oneshot(get("/admin/api/v1/channels/%23team/detail", Some(&cookie)))
            .await
            .unwrap(),
    )
    .await;
    assert!(
        chan_detail.contains("alice") && chan_detail.contains("bob"),
        "{chan_detail}"
    );
    assert!(chan_detail.contains("permanent"), "{chan_detail}");
}

#[tokio::test]
async fn dm_thread_browse_reads_non_e2ee_and_gates_e2ee() {
    use weft_store::{EventKind, EventRecord, EventStore, Scope};

    let store = Arc::new(MemoryStore::default());
    store
        .register(
            &"admin".parse().unwrap(),
            PasswordHash::new(PASSWORD).as_phc(),
        )
        .await
        .unwrap();
    // A DM message from ada → bob (scope normalizes participant order).
    let ada: weft_proto::UserRef = "ada@test.net".parse().unwrap();
    let scope = Scope::dm("ada".parse().unwrap(), "bob".parse().unwrap());
    let ulid = weft_proto::Ulid::new();
    let msgid = weft_proto::MsgId::new("test.net".parse().unwrap(), ulid);
    store
        .append(EventRecord {
            scope,
            msgid: msgid.clone(),
            root: msgid,
            sender: ada,
            kind: EventKind::Message {
                body: "secret plan".into(),
                meta: Default::default(),
            },
        })
        .await
        .unwrap();

    let auth = || {
        auth::config(
            b"a-test-session-secret".to_vec(),
            ["admin".parse().unwrap()],
        )
    };

    // Non-e2ee DM policy → the thread is readable (order-independent path).
    let app = weft_admin::router(
        AdminState::from_store(Arc::clone(&store), auth(), "test.net".into())
            .with_dm_policy(weft_proto::RetentionPolicy::Permanent),
    );
    let cookie = session(&app).await;
    let body = body_string(
        app.oneshot(get("/admin/api/v1/dms/bob/ada/messages", Some(&cookie)))
            .await
            .unwrap(),
    )
    .await;
    assert!(
        body.contains("secret plan") && body.contains("\"unavailable\":false"),
        "{body}"
    );

    // e2ee DM policy → "unavailable by policy", no plaintext materialized.
    let app = weft_admin::router(
        AdminState::from_store(Arc::clone(&store), auth(), "test.net".into())
            .with_dm_policy(weft_proto::RetentionPolicy::E2ee),
    );
    let cookie = session(&app).await;
    let body = body_string(
        app.oneshot(get("/admin/api/v1/dms/ada/bob/messages", Some(&cookie)))
            .await
            .unwrap(),
    )
    .await;
    assert!(
        body.contains("\"unavailable\":true") && !body.contains("secret plan"),
        "{body}"
    );
}

#[tokio::test]
async fn peer_detail_parses_manifest_and_shows_shared_channels() {
    use weft_crypto::{Keypair, Manifest};
    use weft_store::{PeerRecord, PeerStore};

    let store = Arc::new(MemoryStore::default());
    store
        .register(
            &"admin".parse().unwrap(),
            PasswordHash::new(PASSWORD).as_phc(),
        )
        .await
        .unwrap();

    // A peer bridging two channels under a signed manifest.
    let key = Keypair::generate();
    let signed = Manifest {
        peer: "thread.example.net".to_string(),
        version: 7,
        channels: vec!["#gaming/general".to_string(), "#gaming/dev".to_string()],
        history: "recent".to_string(),
        media: "mirror".to_string(),
        typing: true,
        voice: false,
        created: 1_000,
        updated: 2_000,
    }
    .sign(&key);
    store
        .upsert_peer(PeerRecord {
            peer: "thread.example.net".parse().unwrap(),
            scope: "*".to_string(),
            manifest: signed.to_b64(),
            version: 7,
            acked_manifest: Some(signed.to_b64()),
            severed: false,
            created_ms: 1_000,
            updated_ms: 2_000,
        })
        .await
        .unwrap();

    let auth = auth::config(
        b"a-test-session-secret".to_vec(),
        ["admin".parse().unwrap()],
    );
    let app = weft_admin::router(AdminState::from_store(store, auth, "test.net".into()));
    let cookie = session(&app).await;

    let body = body_string(
        app.oneshot(get(
            "/admin/api/v1/peers/thread.example.net/detail",
            Some(&cookie),
        ))
        .await
        .unwrap(),
    )
    .await;
    // Shared channels + negotiated modes + a verified pinned key + not netblocked.
    assert!(
        body.contains("#gaming/general") && body.contains("#gaming/dev"),
        "{body}"
    );
    assert!(
        body.contains("\"verified\":true") && body.contains("\"media\":\"mirror\""),
        "{body}"
    );
    assert!(body.contains("\"netblocked\":false"), "{body}");
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

// ---- WC2 capability RBAC ----

/// Build a router where `delegate` is a **non-operator** admin holding exactly
/// the given `admin`-scope capabilities (e.g. `["admin.read","admin.moderate"]`).
/// `admin` is the config operator; `target` is a registered account to act on.
async fn build_with_delegate(caps: &[&str]) -> axum::Router {
    use weft_store::CapabilityStore;
    let store = Arc::new(MemoryStore::default());
    for name in ["admin", "delegate", "target"] {
        store
            .register(&name.parse().unwrap(), PasswordHash::new(PASSWORD).as_phc())
            .await
            .unwrap();
    }
    // Grant the delegate its admin scopes, keyed by its stable ULID (§10.4).
    let delegate: weft_proto::Account = "delegate".parse().unwrap();
    let ulid = store.account_ulid(&delegate).await.unwrap().unwrap();
    let caps: Vec<String> = caps.iter().map(|s| s.to_string()).collect();
    store
        .record_grant(&ulid, "admin", &caps, 0, None)
        .await
        .unwrap();

    let auth = auth::config(
        b"a-test-session-secret".to_vec(),
        ["admin".parse().unwrap()],
    );
    weft_admin::router(AdminState::from_store(store, auth, "test.net".into()))
}

/// Log in as `account` and return the session cookie.
async fn login_as(app: &axum::Router, account: &str) -> String {
    let res = app.clone().oneshot(login(account, PASSWORD)).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "login {account}");
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

#[tokio::test]
async fn read_only_admin_reads_but_cannot_write() {
    let app = build_with_delegate(&["admin.read"]).await;
    let cookie = login_as(&app, "delegate").await;

    // Reads work, and /me reports exactly the read scope.
    let res = app
        .clone()
        .oneshot(get("/admin/api/v1/accounts", Some(&cookie)))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let me = body_string(
        app.clone()
            .oneshot(get("/admin/api/v1/me", Some(&cookie)))
            .await
            .unwrap(),
    )
    .await;
    assert!(
        me.contains("admin.read") && !me.contains("admin.moderate"),
        "{me}"
    );

    // Every write scope is denied — 403, not 401 (the session is valid).
    let moderate = app
        .clone()
        .oneshot(post_json(
            "/admin/api/v1/moderation",
            &cookie,
            r#"{"verb":"ban","scope":"*","account":"target"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(moderate.status(), StatusCode::FORBIDDEN);
    let destroy = app
        .clone()
        .oneshot(del("/admin/api/v1/accounts/target", &cookie))
        .await
        .unwrap();
    assert_eq!(destroy.status(), StatusCode::FORBIDDEN);
    let federation = app
        .oneshot(post_json(
            "/admin/api/v1/netblocks",
            &cookie,
            r#"{"network":"evil.example"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(federation.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn moderate_admin_moderates_but_cannot_destroy() {
    let app = build_with_delegate(&["admin.read", "admin.moderate"]).await;
    let cookie = login_as(&app, "delegate").await;

    // A ban at `*` is a deny-list write — no live server needed → 204.
    let res = app
        .clone()
        .oneshot(post_json(
            "/admin/api/v1/moderation",
            &cookie,
            r#"{"verb":"ban","scope":"*","account":"target"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);

    // But destroy is still denied.
    let res = app
        .oneshot(del("/admin/api/v1/accounts/target", &cookie))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_registered_non_admin_cannot_log_in() {
    // `target` is registered but holds no admin scope → uniform 401.
    let app = build_with_delegate(&["admin.read"]).await;
    let res = app.oneshot(login("target", PASSWORD)).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}
