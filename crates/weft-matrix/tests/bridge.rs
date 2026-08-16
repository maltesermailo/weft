//! The daemon's core against a mock homeserver: provisioning, both traffic
//! directions, and ban enforcement — everything short of a live weftd, whose
//! side of the contract is pinned by `weft-core`'s own provider tests.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post, put};
use serde_json::{json, Value};
use weft_appservice::Realm;
use weft_matrix::bridge::Bridge;
use weft_matrix::hs::Hs;
use weft_matrix::ident;
use weft_matrix::pending::PendingByLabel;
use weft_matrix::store::Store;

/// Every write the daemon made to the "homeserver": (method+path, query, body).
type Calls = Arc<Mutex<Vec<(String, String, Value)>>>;

#[derive(Clone)]
struct MockHs {
    calls: Calls,
    /// room id → its state events.
    state: Arc<BTreeMap<String, Vec<Value>>>,
}

async fn mock_hs(state: BTreeMap<String, Vec<Value>>) -> (String, Calls) {
    let calls: Calls = Arc::default();
    let hs = MockHs {
        calls: calls.clone(),
        state: Arc::new(state),
    };

    let app = axum::Router::new()
        .route(
            "/_matrix/client/v3/directory/room/:alias",
            get(|| async {
                axum::Json(json!({ "room_id": "!space:kde.org", "servers": ["kde.org"] }))
            })
            // Publishing/retiring the human-typeable vanity alias.
            .put(
                |State(hs): State<MockHs>,
                 Path(alias): Path<String>,
                 axum::Json(body): axum::Json<Value>| async move {
                    hs.calls.lock().unwrap().push((
                        format!("PUT alias/{alias}"),
                        String::new(),
                        body,
                    ));
                    axum::Json(json!({}))
                },
            ),
        )
        .route(
            "/_matrix/client/v3/directory/room/:alias",
            delete(
                |State(hs): State<MockHs>, Path(alias): Path<String>| async move {
                    hs.calls.lock().unwrap().push((
                        format!("DELETE alias/{alias}"),
                        String::new(),
                        Value::Null,
                    ));
                    axum::Json(json!({}))
                },
            ),
        )
        .route(
            "/_matrix/client/v3/directory/list/room/:room",
            put(
                |State(hs): State<MockHs>,
                 Path(room): Path<String>,
                 axum::Json(body): axum::Json<Value>| async move {
                    hs.calls.lock().unwrap().push((
                        format!("PUT list/{room}"),
                        String::new(),
                        body,
                    ));
                    axum::Json(json!({}))
                },
            ),
        )
        .route(
            "/_matrix/client/v3/join/:room",
            post(
                |State(hs): State<MockHs>,
                 Path(room): Path<String>,
                 axum::extract::RawQuery(q): axum::extract::RawQuery| async move {
                    hs.calls.lock().unwrap().push((
                        format!("POST join/{room}"),
                        q.unwrap_or_default(),
                        Value::Null,
                    ));
                    axum::Json(json!({ "room_id": room }))
                },
            ),
        )
        .route(
            "/_matrix/client/v3/rooms/:room/leave",
            post(
                |State(hs): State<MockHs>, Path(room): Path<String>| async move {
                    hs.calls.lock().unwrap().push((
                        format!("POST leave/{room}"),
                        String::new(),
                        Value::Null,
                    ));
                    axum::Json(json!({}))
                },
            ),
        )
        .route(
            "/_matrix/client/v3/rooms/:room/context/:event",
            get(
                |State(hs): State<MockHs>, Path((room, event)): Path<(String, String)>| async move {
                    hs.calls.lock().unwrap().push((
                        format!("GET context/{room}/{event}"),
                        String::new(),
                        Value::Null,
                    ));
                    axum::Json(json!({ "start": format!("tok-{event}") }))
                },
            ),
        )
        .route(
            "/_matrix/client/v3/rooms/:room/messages",
            get(
                |State(hs): State<MockHs>,
                 Path(room): Path<String>,
                 axum::extract::RawQuery(q): axum::extract::RawQuery| async move {
                    hs.calls.lock().unwrap().push((
                        format!("GET messages/{room}"),
                        q.unwrap_or_default(),
                        Value::Null,
                    ));
                    let chunk = hs.state.get("__messages__").cloned().unwrap_or_default();
                    axum::Json(json!({ "chunk": chunk }))
                },
            ),
        )
        .route(
            "/_matrix/client/v3/joined_rooms",
            get(|State(hs): State<MockHs>| async move {
                let joined = hs.state.get("__joined__").cloned().unwrap_or_default();
                axum::Json(json!({ "joined_rooms": joined }))
            }),
        )
        .route(
            "/_matrix/client/v3/user/:user/account_data/:kind",
            get(|State(hs): State<MockHs>| async move {
                match hs.state.get("__account_data__").and_then(|d| d.first()) {
                    Some(data) => axum::Json(data.clone()).into_response(),
                    None => (
                        axum::http::StatusCode::NOT_FOUND,
                        axum::Json(json!({ "errcode": "M_NOT_FOUND" })),
                    )
                        .into_response(),
                }
            })
            .put(
                |State(hs): State<MockHs>, axum::Json(body): axum::Json<Value>| async move {
                    hs.calls.lock().unwrap().push((
                        "PUT account_data".to_string(),
                        String::new(),
                        body,
                    ));
                    axum::Json(json!({}))
                },
            ),
        )
        .route(
            "/_matrix/client/v3/rooms/:room/state",
            get(
                |State(hs): State<MockHs>, Path(room): Path<String>| async move {
                    axum::Json(Value::Array(
                        hs.state.get(&room).cloned().unwrap_or_default(),
                    ))
                },
            ),
        )
        .route(
            "/_matrix/client/v3/rooms/:room/send/:kind/:txn",
            put(
                |State(hs): State<MockHs>,
                 Path((room, kind, txn)): Path<(String, String, String)>,
                 axum::extract::RawQuery(q): axum::extract::RawQuery,
                 axum::Json(body): axum::Json<Value>| async move {
                    hs.calls.lock().unwrap().push((
                        format!("PUT send/{room}/{kind}"),
                        q.unwrap_or_default(),
                        body,
                    ));
                    axum::Json(json!({ "event_id": format!("$sent-{txn}") }))
                },
            ),
        )
        .route(
            "/_matrix/client/v3/presence/:user/status",
            put(
                |State(hs): State<MockHs>,
                 Path(user): Path<String>,
                 axum::Json(body): axum::Json<Value>| async move {
                    hs.calls.lock().unwrap().push((
                        format!("PUT presence/{user}"),
                        String::new(),
                        body,
                    ));
                    axum::Json(json!({}))
                },
            ),
        )
        .route(
            "/_matrix/client/v3/rooms/:room/redact/:event/:txn",
            put(
                |State(hs): State<MockHs>,
                 Path((room, event, _txn)): Path<(String, String, String)>,
                 axum::extract::RawQuery(q): axum::extract::RawQuery| async move {
                    hs.calls.lock().unwrap().push((
                        format!("PUT redact/{room}/{event}"),
                        q.unwrap_or_default(),
                        Value::Null,
                    ));
                    axum::Json(json!({ "event_id": "$redaction" }))
                },
            ),
        )
        .route(
            "/_matrix/client/v3/createRoom",
            post(
                |State(hs): State<MockHs>,
                 axum::extract::RawQuery(q): axum::extract::RawQuery,
                 axum::Json(body): axum::Json<Value>| async move {
                    let alias = body["room_alias_name"].as_str().unwrap_or("noalias");
                    let room_id = format!("!{alias}:test.example");
                    hs.calls.lock().unwrap().push((
                        "POST createRoom".to_string(),
                        q.unwrap_or_default(),
                        body,
                    ));
                    axum::Json(json!({ "room_id": room_id }))
                },
            ),
        )
        .route(
            "/_matrix/client/v3/rooms/:room/state/:kind/:key",
            put(
                |State(hs): State<MockHs>,
                 Path((room, kind, key)): Path<(String, String, String)>,
                 axum::Json(body): axum::Json<Value>| async move {
                    hs.calls.lock().unwrap().push((
                        format!("PUT state/{room}/{kind}/{key}"),
                        String::new(),
                        body,
                    ));
                    axum::Json(json!({ "event_id": "$state" }))
                },
            )
            .get(|| async {
                (
                    axum::http::StatusCode::NOT_FOUND,
                    axum::Json(json!({ "errcode": "M_NOT_FOUND", "error": "no state" })),
                )
            }),
        )
        .route(
            "/_matrix/client/v3/rooms/:room/state/:kind",
            put(
                |State(hs): State<MockHs>,
                 Path((room, kind)): Path<(String, String)>,
                 axum::Json(body): axum::Json<Value>| async move {
                    hs.calls.lock().unwrap().push((
                        format!("PUT state/{room}/{kind}/"),
                        String::new(),
                        body,
                    ));
                    axum::Json(json!({ "event_id": "$state" }))
                },
            )
            .get(|| async {
                (
                    axum::http::StatusCode::NOT_FOUND,
                    axum::Json(json!({ "errcode": "M_NOT_FOUND", "error": "no state" })),
                )
            }),
        )
        .route(
            "/_matrix/client/v3/rooms/:room/invite",
            post(
                |State(hs): State<MockHs>,
                 Path(room): Path<String>,
                 axum::Json(body): axum::Json<Value>| async move {
                    hs.calls.lock().unwrap().push((
                        format!("POST invite/{room}"),
                        String::new(),
                        body,
                    ));
                    axum::Json(json!({}))
                },
            ),
        )
        .route(
            "/_matrix/client/v3/rooms/:room/typing/:user",
            put(
                |State(hs): State<MockHs>,
                 Path((room, user)): Path<(String, String)>,
                 axum::extract::RawQuery(q): axum::extract::RawQuery,
                 axum::Json(body): axum::Json<Value>| async move {
                    hs.calls.lock().unwrap().push((
                        format!("PUT typing/{room}/{user}"),
                        q.unwrap_or_default(),
                        body,
                    ));
                    axum::Json(json!({}))
                },
            ),
        )
        .route(
            "/_matrix/client/v3/rooms/:room/kick",
            post(
                |State(hs): State<MockHs>,
                 Path(room): Path<String>,
                 axum::Json(body): axum::Json<Value>| async move {
                    hs.calls.lock().unwrap().push((
                        format!("POST kick/{room}"),
                        String::new(),
                        body,
                    ));
                    axum::Json(json!({}))
                },
            ),
        )
        .route(
            "/_matrix/client/v3/rooms/:room/ban",
            post(
                |State(hs): State<MockHs>,
                 Path(room): Path<String>,
                 axum::Json(body): axum::Json<Value>| async move {
                    hs.calls.lock().unwrap().push((
                        format!("POST ban/{room}"),
                        String::new(),
                        body,
                    ));
                    axum::Json(json!({}))
                },
            ),
        )
        .route(
            "/_matrix/client/v3/rooms/:room/unban",
            post(
                |State(hs): State<MockHs>,
                 Path(room): Path<String>,
                 axum::Json(body): axum::Json<Value>| async move {
                    hs.calls.lock().unwrap().push((
                        format!("POST unban/{room}"),
                        String::new(),
                        body,
                    ));
                    axum::Json(json!({}))
                },
            ),
        )
        .route(
            "/_matrix/client/v1/media/download/:server/:id",
            get(
                |State(hs): State<MockHs>, Path((server, id)): Path<(String, String)>| async move {
                    hs.calls.lock().unwrap().push((
                        format!("GET media/download/{server}/{id}"),
                        String::new(),
                        Value::Null,
                    ));
                    // A PNG magic number, so the sniffer has something real.
                    (
                        [(axum::http::header::CONTENT_TYPE, "image/png")],
                        vec![0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
                    )
                },
            ),
        )
        .route(
            "/_matrix/media/v3/upload",
            post(
                |State(hs): State<MockHs>, _body: axum::body::Bytes| async move {
                    hs.calls.lock().unwrap().push((
                        "POST matrix-upload".to_string(),
                        String::new(),
                        Value::Null,
                    ));
                    axum::Json(json!({ "content_uri": "mxc://test.example/uploaded" }))
                },
            ),
        )
        // weftd's media plane, stood up on the same mock for the test.
        .route(
            "/media",
            post(
                |State(hs): State<MockHs>,
                 axum::extract::RawQuery(q): axum::extract::RawQuery,
                 _body: axum::body::Bytes| async move {
                    hs.calls.lock().unwrap().push((
                        "POST media".to_string(),
                        q.unwrap_or_default(),
                        Value::Null,
                    ));
                    axum::Json(json!({ "hash": "deadbeef" }))
                },
            ),
        )
        .route(
            "/media/:hash",
            get(
                |State(hs): State<MockHs>, Path(hash): Path<String>| async move {
                    hs.calls.lock().unwrap().push((
                        format!("GET weft-media/{hash}"),
                        String::new(),
                        Value::Null,
                    ));
                    vec![0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
                },
            ),
        )
        .route(
            "/_matrix/client/v3/profile/:user/displayname",
            put(
                |State(hs): State<MockHs>,
                 Path(user): Path<String>,
                 axum::Json(body): axum::Json<Value>| async move {
                    hs.calls.lock().unwrap().push((
                        format!("PUT displayname/{user}"),
                        String::new(),
                        body,
                    ));
                    axum::Json(json!({}))
                },
            ),
        )
        .route(
            "/_matrix/client/v3/register",
            post(
                |State(hs): State<MockHs>, axum::Json(body): axum::Json<Value>| async move {
                    hs.calls.lock().unwrap().push((
                        "POST register".to_string(),
                        String::new(),
                        body,
                    ));
                    axum::Json(json!({ "user_id": "ok" }))
                },
            ),
        )
        .with_state(hs);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    (format!("http://{addr}"), calls)
}

/// A space on kde.org with one plain room, one encrypted room, and a remote
/// member in the plain one.
fn kde_space() -> BTreeMap<String, Vec<Value>> {
    let mut rooms = BTreeMap::new();
    rooms.insert(
        "!space:kde.org".to_string(),
        vec![
            json!({ "type": "m.room.name", "state_key": "", "content": { "name": "Community" } }),
            json!({ "type": "m.space.child", "state_key": "!gen:kde.org",
                    "content": { "via": ["kde.org"], "order": "a" } }),
            json!({ "type": "m.space.child", "state_key": "!sec:kde.org",
                    "content": { "via": ["kde.org"], "order": "b" } }),
        ],
    );
    rooms.insert(
        "!gen:kde.org".to_string(),
        vec![
            json!({ "type": "m.room.name", "state_key": "", "content": { "name": "General Chat" } }),
            json!({ "type": "m.room.member", "state_key": "@carol:kde.org",
                    "content": { "membership": "join" } }),
        ],
    );
    rooms.insert(
        "!sec:kde.org".to_string(),
        vec![json!({ "type": "m.room.encryption", "state_key": "",
                     "content": { "algorithm": "m.megolm.v1.aes-sha2" } })],
    );

    rooms
}

async fn bridge_with(
    state: BTreeMap<String, Vec<Value>>,
) -> (Bridge, tokio::sync::mpsc::Receiver<String>, Calls) {
    let (url, calls) = mock_hs(state).await;
    let (realm, lines) = Realm::capture("test.example");
    let bridge = Bridge {
        realm,
        hs: Hs::new(&url, "as-token"),
        store: Store::in_memory(),
        identity: weft_matrix::ident::MatrixIdentity::new("test.example", "weft_", "weftbot"),
        pending_layouts: Default::default(),
        pending_injections: PendingByLabel::new("inj"),
        pending_acts: PendingByLabel::new("act"),
        flows: Default::default(),
        weft_media: Some(weft_matrix::media::WeftMedia::new(&url)),
        pending_uploads: PendingByLabel::new("up"),
        dm_txn: 0,
        admins: vec!["@boss:test.example".into()],
        local_roster: Default::default(),
        typing_now: Default::default(),
    };

    (bridge, lines, calls)
}

fn drain(lines: &mut tokio::sync::mpsc::Receiver<String>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(line) = lines.try_recv() {
        out.push(line);
    }
    out
}

/// ada's stable identity — what weftd's `ulid=` tag carries on her relays.
const ADA_ULID: &str = "01arz3ndektsv4rrffq69g5fav";

/// Deliver the membership relay that introduces ada to the bridge — the only
/// door a local user comes in through, and what populates the ULID↔name map
/// the fan-out paths resolve against.
async fn join_ada(bridge: &mut Bridge, ns_id: &str) {
    bridge
        .on_incoming(weft_appservice::Incoming::Command {
            label: None,
            as_user: Some("ada@test.example".into()),
            as_ulid: Some(ADA_ULID.into()),
            command: weft_proto::Command::NsJoin {
                ns: ns_id.parse().unwrap(),
            },
        })
        .await;
}

#[tokio::test]
async fn provisioning_asserts_the_space_and_excludes_encrypted_rooms() {
    let (mut bridge, mut lines, _calls) = bridge_with(kde_space()).await;

    let ok = bridge
        .provision("matrix://kde.org/community")
        .await
        .unwrap();
    assert!(ok, "a resolvable space provisions");

    let sent = drain(&mut lines);
    let ns_id = ident::stable_ulid("!space:kde.org");
    let chan_id = ident::stable_ulid("!gen:kde.org");

    // The namespace assertion carries the deterministic id + Matrix's
    // capability profile (levels authority, native roles editor hidden).
    let ns = sent
        .iter()
        .find(|l| l.contains("NS-META"))
        .expect("NS-META");
    assert!(ns.contains(&format!("id={ns_id}")), "{ns}");
    assert!(ns.contains("authority=levels"), "{ns}");
    assert!(
        ns.contains("NS-META matrix://kde.org/community public"),
        "{ns}"
    );

    // The plain room is asserted; the encrypted one is absent — a channel for
    // it would violate invariant 8, and its id must appear nowhere.
    let chan = sent
        .iter()
        .find(|l| l.contains("CHANNEL-LAYOUT"))
        .expect("CHANNEL-LAYOUT");
    assert!(chan.contains(&format!("id={chan_id}")), "{chan}");
    assert!(chan.contains("vanity=general-chat"), "{chan}");
    let sec_id = ident::stable_ulid("!sec:kde.org");
    assert!(
        !sent.iter().any(|l| l.contains(&sec_id)),
        "the encrypted room must not be asserted: {sent:?}"
    );

    // The remote member is stated (the realm is the membership authority).
    let member = sent
        .iter()
        .find(|l| l.contains("NS-MEMBER"))
        .expect("NS-MEMBER");
    assert!(member.contains("carol@kde.org join"), "{member}");

    // The structure maps persisted for the traffic path.
    let (room, _) = bridge
        .store
        .state
        .channel_of_room("!gen:kde.org")
        .expect("mapped");
    assert_eq!(room.channel, format!("#{ns_id}/{chan_id}"));
}

#[tokio::test]
async fn a_nameless_space_is_still_asserted_with_a_title() {
    // An assertion is the whole truth about the namespace (§7a.0e): a field we
    // omit is a field weftd *clears*, and we re-assert on every reconnect. So
    // sending no title because the Space has no `m.room.name` does not mean
    // "leave the name alone", it means "erase it" — which is how a namespace that
    // looked right at login renamed itself to a placeholder later on.
    let mut rooms = BTreeMap::new();
    rooms.insert(
        "!space:kde.org".to_string(),
        // No `m.room.name` at all, and a blank one would be no better.
        vec![json!({ "type": "m.room.topic", "state_key": "", "content": { "topic": "hi" } })],
    );
    let (mut bridge, mut lines, _calls) = bridge_with(rooms).await;

    bridge
        .provision("matrix://kde.org/community")
        .await
        .unwrap();

    let sent = drain(&mut lines);
    let meta = sent
        .iter()
        .find(|l| l.contains("NS-META"))
        .expect("the space is asserted");
    // The alias localpart: always present, stable across restarts, and what a
    // user typed to reach the space.
    assert!(
        meta.contains("title=community"),
        "a nameless space must still carry a title: {meta}"
    );
}

#[tokio::test]
async fn joining_the_namespace_joins_the_space_itself() {
    // Joining a namespace here IS joining the Space there (owner directive
    // 2026-08-07) — and it is load-bearing, not decoration: a restricted child
    // room admits Space members, so a puppet outside the Space is refused every
    // child with "you do not belong to any of the required rooms/spaces".
    let (mut bridge, mut lines, calls) = bridge_with(kde_space()).await;
    bridge
        .provision("matrix://kde.org/community")
        .await
        .unwrap();
    drain(&mut lines);
    let ns_id = bridge
        .store
        .state
        .spaces
        .get("matrix://kde.org/community")
        .expect("stored")
        .ns_id
        .clone();

    calls.lock().unwrap().clear();
    join_ada(&mut bridge, &ns_id).await;

    let joins: Vec<String> = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(path, _, _)| path.starts_with("POST join/"))
        .map(|(path, query, _)| format!("{path}?{query}"))
        .collect();
    assert!(
        joins.iter().any(|j| j.contains("!space:kde.org")),
        "the puppet must join the Space: {joins:?}"
    );
    assert!(
        joins.iter().any(|j| j.contains("!gen:kde.org")),
        "…and its rooms: {joins:?}"
    );
    // The Space is joined by ID, so it needs `via` servers like any other room —
    // a v12+ room ID carries no server part to fall back on.
    assert!(
        joins
            .iter()
            .any(|j| j.contains("!space:kde.org") && j.contains("server_name=kde.org")),
        "the Space join needs via servers: {joins:?}"
    );

    // Leaving is symmetric — otherwise the user stays in the Space and keeps
    // access to its restricted rooms.
    calls.lock().unwrap().clear();
    bridge
        .on_incoming(weft_appservice::Incoming::Command {
            label: None,
            as_user: Some("ada@test.example".into()),
            as_ulid: Some(ADA_ULID.into()),
            command: weft_proto::Command::NsLeave {
                ns: ns_id.parse().unwrap(),
            },
        })
        .await;

    let leaves: Vec<String> = calls
        .lock()
        .unwrap()
        .iter()
        .map(|(path, _, _)| path.clone())
        .filter(|path| path.contains("leave"))
        .collect();
    assert!(
        leaves.iter().any(|l| l.contains("!space:kde.org")),
        "the puppet must leave the Space too: {leaves:?}"
    );
}

#[tokio::test]
async fn a_leave_during_downtime_reconciles_on_reconnect() {
    // weftd applies `NS LEAVE` whether or not we are connected and its pushes are
    // live-only, so one that happened while the daemon was down never reached us.
    // The statement it sends on registration is the whole of what it holds, so a
    // puppet joined foreign-side and absent from it has left.
    let mut rooms = kde_space();
    // ada's puppet is in the Space and in !gen, as a real join would have left it.
    let puppet = format!("@weft_{ADA_ULID}:test.example");
    for room in ["!space:kde.org", "!gen:kde.org"] {
        rooms.get_mut(room).unwrap().push(json!({
            "type": "m.room.member", "state_key": puppet,
            "content": { "membership": "join" } }));
    }
    let (mut bridge, mut lines, calls) = bridge_with(rooms).await;
    bridge
        .provision("matrix://kde.org/community")
        .await
        .unwrap();
    let ns_id = bridge
        .store
        .state
        .spaces
        .get("matrix://kde.org/community")
        .expect("stored")
        .ns_id
        .clone();

    // Introduce ada so her ULID→name mapping exists, which is what identifies the
    // puppet as ours during the reconcile.
    join_ada(&mut bridge, &ns_id).await;
    drain(&mut lines);
    calls.lock().unwrap().clear();

    // weftd now states its local membership — and ada is not in it, because she
    // left while we were away. An empty roster is the whole point: the namespace
    // is never mentioned, so it must reconcile against an empty set.
    bridge
        .on_incoming(weft_appservice::Incoming::Event {
            event: weft_proto::Event::BatchEnd {
                id: "ni1".into(),
                truncated: false,
            },
            label: None,
            actor_ulid: None,
        })
        .await;

    let leaves: Vec<String> = calls
        .lock()
        .unwrap()
        .iter()
        .map(|(path, _, _)| path.clone())
        .filter(|path| path.starts_with("POST leave/"))
        .collect();
    assert!(
        leaves.iter().any(|l| l.contains("!space:kde.org")),
        "the departed member's puppet must leave the Space: {leaves:?}"
    );
    assert!(
        leaves.iter().any(|l| l.contains("!gen:kde.org")),
        "…and its rooms: {leaves:?}"
    );

    // And a member weftd *does* still hold is left alone.
    calls.lock().unwrap().clear();
    bridge
        .on_incoming(weft_appservice::Incoming::Event {
            event: weft_proto::Event::NsMemberInfo {
                namespace: ns_id.parse().unwrap(),
                user: "ada@test.example".parse().unwrap(),
                joined_ms: 0,
                roles: Vec::new(),
            },
            label: None,
            actor_ulid: None,
        })
        .await;
    bridge
        .on_incoming(weft_appservice::Incoming::Event {
            event: weft_proto::Event::BatchEnd {
                id: "ni2".into(),
                truncated: false,
            },
            label: None,
            actor_ulid: None,
        })
        .await;

    let leaves: Vec<String> = calls
        .lock()
        .unwrap()
        .iter()
        .map(|(path, _, _)| path.clone())
        .filter(|path| path.starts_with("POST leave/"))
        .collect();
    assert!(
        leaves.is_empty(),
        "a member weftd still holds must be left in place: {leaves:?}"
    );
}

#[tokio::test]
async fn an_empty_space_provisions_as_an_empty_namespace() {
    // Spaces exist without chats (owner directive 2026-08-06): they map like
    // an empty WEFT namespace, not NO-SUCH-TARGET. An encrypted-only space is
    // the same case — its rooms are simply absent (invariant 8).
    let mut rooms = BTreeMap::new();
    rooms.insert(
        "!space:kde.org".to_string(),
        vec![
            json!({ "type": "m.room.name", "state_key": "", "content": { "name": "Lonely" } }),
            json!({ "type": "m.space.child", "state_key": "!sec:kde.org",
                    "content": { "via": ["kde.org"] } }),
        ],
    );
    rooms.insert(
        "!sec:kde.org".to_string(),
        vec![json!({ "type": "m.room.encryption", "state_key": "",
                     "content": { "algorithm": "m.megolm.v1.aes-sha2" } })],
    );
    let (mut bridge, mut lines, _calls) = bridge_with(rooms).await;

    let ok = bridge
        .provision("matrix://kde.org/community")
        .await
        .unwrap();
    assert!(ok, "an empty space provisions");

    let sent = drain(&mut lines);
    assert!(sent.iter().any(|l| l.contains("NS-META")), "{sent:?}");
    assert!(
        !sent.iter().any(|l| l.contains("CHANNEL-LAYOUT")),
        "no rooms, no channels: {sent:?}"
    );

    let space = bridge
        .store
        .state
        .spaces
        .get("matrix://kde.org/community")
        .expect("stored");
    assert!(space.rooms.is_empty());
    let ns_id = space.ns_id.clone();

    // …and it must be joinable: with no rooms there is nothing to do
    // foreign-side, so the membership statement confirms immediately.
    join_ada(&mut bridge, &ns_id).await;
    let sent = drain(&mut lines);
    assert!(
        sent.iter()
            .any(|l| l.contains("NS-MEMBER") && l.contains("ada@test.example join")),
        "an empty namespace must confirm the join: {sent:?}"
    );
}

#[tokio::test]
async fn matrix_traffic_ingests_and_weft_traffic_relays() {
    let (mut bridge, mut lines, calls) = bridge_with(kde_space()).await;
    bridge
        .provision("matrix://kde.org/community")
        .await
        .unwrap();
    drain(&mut lines);
    let channel = bridge
        .store
        .state
        .channel_of_room("!gen:kde.org")
        .unwrap()
        .0
        .channel
        .clone();

    // Matrix → WEFT: a remote user's message ingests with their native
    // identity and a msgid minted under the realm at the event's timestamp.
    bridge
        .on_matrix_event(json!({
            "type": "m.room.message",
            "room_id": "!gen:kde.org",
            "event_id": "$m1",
            "sender": "@carol:kde.org",
            "origin_server_ts": 1_722_000_000_000u64,
            "content": { "msgtype": "m.text", "body": "hello from kde" },
        }))
        .await;

    let sent = drain(&mut lines);
    let msg = sent.iter().find(|l| l.contains("MSG")).expect("MSG");
    assert!(msg.contains("as=carol@kde.org"), "{msg}");
    assert!(msg.contains("msgid=kde.org/"), "{msg}");
    assert!(
        msg.contains(&format!("MSG {channel} :hello from kde")),
        "{msg}"
    );

    // …and a reaction to it round-trips through the event map.
    bridge
        .on_matrix_event(json!({
            "type": "m.reaction",
            "room_id": "!gen:kde.org",
            "event_id": "$r1",
            "sender": "@carol:kde.org",
            "origin_server_ts": 1_722_000_000_001u64,
            "content": { "m.relates_to": {
                "rel_type": "m.annotation", "event_id": "$m1", "key": "👍",
            }},
        }))
        .await;
    let sent = drain(&mut lines);
    assert!(
        sent.iter()
            .any(|l| l.contains("REACT") && l.contains("as=carol@kde.org")),
        "{sent:?}"
    );

    // Our own puppet's echo must NOT re-ingest — it is the relay of a WEFT
    // event, and echoing it back would double every bridged message.
    bridge
        .on_matrix_event(json!({
            "type": "m.room.message",
            "room_id": "!gen:kde.org",
            "event_id": "$echo",
            "sender": "@weft_ada:test.example",
            "origin_server_ts": 1_722_000_000_002u64,
            "content": { "msgtype": "m.text", "body": "echo" },
        }))
        .await;
    assert!(drain(&mut lines).is_empty(), "puppet echo re-ingested");

    // WEFT → Matrix: a local user's relayed message goes out as their puppet.
    // She enters through the membership relay (the only door), which registers
    // the puppet **keyed by her ULID** — a rename must never change it.
    let ns_id = bridge
        .store
        .state
        .channel_of_room("!gen:kde.org")
        .unwrap()
        .1
        .ns_id
        .clone();
    join_ada(&mut bridge, &ns_id).await;
    drain(&mut lines);

    let msgid: weft_proto::MsgId = format!("test.example/{}", ulid::Ulid::new())
        .to_lowercase()
        .parse()
        .unwrap();
    bridge
        .on_incoming(weft_appservice::Incoming::Event {
            label: None,
            actor_ulid: Some(ADA_ULID.into()),
            event: weft_proto::Event::Message(Box::new(weft_proto::MessageEvent {
                target: weft_proto::Target::Channel(channel.parse().unwrap()),
                sender: "ada@test.example".parse().unwrap(),
                msgid: msgid.clone(),
                body: "hi from weft".into(),
                meta: weft_proto::MsgMeta::default(),
                edited: None,
                edited_at: None,
            })),
        })
        .await;

    let recorded = calls.lock().unwrap().clone();
    let puppet = format!("weft_{ADA_ULID}");
    assert!(
        recorded
            .iter()
            .any(|(what, _, body)| what == "POST register" && body["username"] == *puppet),
        "puppet registered by ULID: {recorded:?}"
    );
    // The display name is where the account *label* lives Matrix-side: without
    // it users see the raw ULID, and recovery has no name to read back.
    assert!(
        recorded.iter().any(|(what, _, body)| what
            == &format!("PUT displayname/@{puppet}:test.example")
            && body["displayname"] == "ada"),
        "the puppet was named: {recorded:?}"
    );
    let (_, query, body) = recorded
        .iter()
        .find(|(what, _, _)| what == "PUT send/!gen:kde.org/m.room.message")
        .expect("relayed to Matrix");
    assert!(
        query.contains(&format!("user_id=%40{puppet}%3Atest.example"))
            || query.contains(&format!("user_id=@{puppet}:test.example")),
        "{query}"
    );
    assert_eq!(body["body"], "hi from weft");

    // The send response's event id is linked, so a remote reaction to the
    // bridged copy resolves back to the WEFT msgid.
    assert!(bridge
        .store
        .state
        .links
        .event_of(&msgid.to_string())
        .is_some());
}

#[tokio::test]
async fn a_banned_space_stops_bridging_in_both_directions() {
    let (mut bridge, mut lines, calls) = bridge_with(kde_space()).await;
    bridge
        .provision("matrix://kde.org/community")
        .await
        .unwrap();
    drain(&mut lines);
    let ns_id = ident::stable_ulid("!space:kde.org");
    let channel = bridge
        .store
        .state
        .channel_of_room("!gen:kde.org")
        .unwrap()
        .0
        .channel
        .clone();

    // The operator bans the space in the admin panel; weftd tells us once.
    bridge
        .on_incoming(weft_appservice::Incoming::Event {
            label: None,
            actor_ulid: None,
            event: weft_proto::Event::Bridging {
                namespace: ns_id.parse().unwrap(),
                state: weft_proto::BridgingState::Banned,
            },
        })
        .await;

    // Inbound stops…
    bridge
        .on_matrix_event(json!({
            "type": "m.room.message",
            "room_id": "!gen:kde.org",
            "event_id": "$banned1",
            "sender": "@carol:kde.org",
            "origin_server_ts": 1_722_000_000_000u64,
            "content": { "msgtype": "m.text", "body": "into a banned space" },
        }))
        .await;
    assert!(drain(&mut lines).is_empty(), "banned space still ingested");

    // …outbound stops…
    let sends_before = calls.lock().unwrap().len();
    bridge
        .on_incoming(weft_appservice::Incoming::Event {
            label: None,
            actor_ulid: Some(ADA_ULID.into()),
            event: weft_proto::Event::Message(Box::new(weft_proto::MessageEvent {
                target: weft_proto::Target::Channel(channel.parse().unwrap()),
                sender: "ada@test.example".parse().unwrap(),
                msgid: format!("test.example/{}", ulid::Ulid::new())
                    .to_lowercase()
                    .parse()
                    .unwrap(),
                body: "out to a banned space".into(),
                meta: weft_proto::MsgMeta::default(),
                edited: None,
                edited_at: None,
            })),
        })
        .await;
    assert_eq!(
        calls.lock().unwrap().len(),
        sends_before,
        "banned space still relayed"
    );

    // …and a re-provision is refused. The ban also survived a store reload —
    // weftd never repeats it, so persistence is the enforcement.
    assert!(bridge.store.state.bans.is_banned(&ns_id));
    let ok = bridge
        .provision("matrix://kde.org/community")
        .await
        .unwrap();
    assert!(!ok, "banned space must not re-provision");
}

/// Shorthand: a weftd event delivery on the provider session.
async fn deliver(
    bridge: &mut Bridge,
    event: weft_proto::Event,
    label: Option<&str>,
    ulid: Option<&str>,
) {
    bridge
        .on_incoming(weft_appservice::Incoming::Event {
            event,
            label: label.map(String::from),
            actor_ulid: ulid.map(String::from),
        })
        .await;
}

#[tokio::test]
async fn a_projected_namespace_becomes_a_space_and_bridges_both_directions() {
    // The daemon half of outbound projection: weftd pushes the structure
    // (NS-META with bridges= + CHANNEL-LAYOUT + POLICY), the daemon mirrors it
    // as a Space with rooms (§3 rules applied), local traffic goes out via
    // ULID-keyed puppets, and Matrix traffic comes back through the injection
    // door — no msgid, labeled echo as the ack.
    let (mut bridge, mut lines, calls) = bridge_with(BTreeMap::new()).await;
    let ns_id = ulid::Ulid::new().to_string().to_lowercase();
    let chan_id = ulid::Ulid::new().to_string().to_lowercase();
    let channel = format!("#{ns_id}/{chan_id}");

    // Structure push → Space + room. The retained channel must NOT project.
    deliver(
        &mut bridge,
        weft_proto::Event::NsMeta {
            id: ns_id.parse().unwrap(),
            vanity: "gaming".parse().unwrap(),
            visibility: weft_proto::Visibility::Public,
            owner: Some("ada".into()),
            title: Some("The Lounge".into()),
            description: None,
            icon: None,
            recovery_set: false,
            recovery_pending: None,
            categories: Vec::new(),
            federation: false,
            welcome: None,
            origin: None,
            provider_online: None,
            authority: None,
            settings_disabled: Vec::new(),
            bridges: vec!["matrix".parse().unwrap()],
        },
        None,
        None,
    )
    .await;
    deliver(
        &mut bridge,
        weft_proto::Event::ChannelLayout {
            channel: channel.parse().unwrap(),
            category: None,
            position: 0,
            kind: weft_proto::ChannelKind::Text,
            vanity: "general".into(),
            origin: None,
        },
        None,
        None,
    )
    .await;
    deliver(
        &mut bridge,
        weft_proto::Event::Policy {
            channel: channel.parse().unwrap(),
            policy: weft_proto::RetentionPolicy::Permanent,
        },
        None,
        None,
    )
    .await;

    // A retained channel in the same namespace: absent by rule (§3).
    let ephemeral = format!("#{ns_id}/{}", ulid::Ulid::new().to_string().to_lowercase());
    deliver(
        &mut bridge,
        weft_proto::Event::ChannelLayout {
            channel: ephemeral.parse().unwrap(),
            category: None,
            position: 1,
            kind: weft_proto::ChannelKind::Text,
            vanity: "fleeting".into(),
            origin: None,
        },
        None,
        None,
    )
    .await;
    deliver(
        &mut bridge,
        weft_proto::Event::Policy {
            channel: ephemeral.parse().unwrap(),
            policy: "retained:30d".parse().unwrap(),
        },
        None,
        None,
    )
    .await;

    {
        let recorded = calls.lock().unwrap();
        let creates: Vec<_> = recorded
            .iter()
            .filter(|(what, _, _)| what == "POST createRoom")
            .collect();
        assert_eq!(
            creates.len(),
            2,
            "Space + one projectable room: {creates:?}"
        );
        assert_eq!(creates[0].2["creation_content"]["type"], "m.space");
        assert_eq!(creates[0].2["room_alias_name"], format!("weft_{ns_id}"));
        assert_eq!(creates[0].2["name"], "The Lounge");
        // Published in the room directory, or a Matrix user browsing this server
        // never sees the namespace (`preset` governs join rules, not listing).
        assert_eq!(creates[0].2["visibility"], "public");
        // …and findable by its **vanity**: the canonical alias is the 26-char
        // ULID, which nobody can type, so the vanity rides alongside it.
        assert!(
            recorded
                .iter()
                .any(|(what, _, _)| what == "PUT alias/#gaming:test.example"),
            "the vanity alias must be published: {recorded:?}"
        );
        // Listed in the room directory, or nobody browsing this server finds it.
        assert!(
            recorded
                .iter()
                .any(|(what, _, body)| what.starts_with("PUT list/")
                    && body["visibility"] == "public"),
            "the Space must be published in the directory: {recorded:?}"
        );
        assert_eq!(creates[1].2["name"], "general");
        assert!(
            recorded.iter().any(
                |(what, _, _)| what.starts_with("PUT state/") && what.contains("m.space.child")
            ),
            "the room is linked under the Space"
        );
    }
    let room_id = format!("!weft_{chan_id}:test.example");
    assert_eq!(
        bridge
            .store
            .state
            .channel_of_projected_room(&room_id)
            .map(|(c, _)| c.to_string()),
        Some(channel.clone())
    );

    // WEFT → Matrix: ada posts; the stamped ULID registers her puppet.
    deliver(
        &mut bridge,
        weft_proto::Event::Message(Box::new(weft_proto::MessageEvent {
            target: weft_proto::Target::Channel(channel.parse().unwrap()),
            sender: "ada@test.example".parse().unwrap(),
            msgid: format!("test.example/{}", ulid::Ulid::new())
                .to_lowercase()
                .parse()
                .unwrap(),
            body: "hello matrix".into(),
            meta: weft_proto::MsgMeta::default(),
            edited: None,
            edited_at: None,
        })),
        None,
        Some(ADA_ULID),
    )
    .await;
    {
        let recorded = calls.lock().unwrap();
        let (_, query, body) = recorded
            .iter()
            .find(|(what, _, _)| what == &format!("PUT send/{room_id}/m.room.message"))
            .expect("projected outbound send");
        assert!(query.contains(&format!("weft_{ADA_ULID}")), "{query}");
        assert_eq!(body["body"], "hello matrix");
    }

    // Matrix → WEFT: carol posts in the projected room — the injection line
    // carries @as + label and NO msgid (the home mints).
    bridge
        .on_matrix_event(json!({
            "type": "m.room.message",
            "room_id": room_id,
            "event_id": "$carol1",
            "sender": "@carol:kde.org",
            "origin_server_ts": 1_722_000_000_000u64,
            "content": { "msgtype": "m.text", "body": "hi from matrix" },
        }))
        .await;
    let sent = drain(&mut lines);
    let inject = sent.iter().find(|l| l.contains("MSG")).expect("injection");
    assert!(inject.contains("as=carol@kde.org"), "{inject}");
    assert!(inject.contains("label=inj-"), "{inject}");
    assert!(!inject.contains("msgid="), "the home mints: {inject}");
    let label = inject
        .split("label=")
        .nth(1)
        .unwrap()
        .split([';', ' '])
        .next()
        .unwrap()
        .to_string();

    // The labeled echo returns with the minted id → linked, never re-relayed.
    let minted = format!("test.example/{}", ulid::Ulid::new()).to_lowercase();
    let sends_before = calls.lock().unwrap().len();
    deliver(
        &mut bridge,
        weft_proto::Event::Message(Box::new(weft_proto::MessageEvent {
            target: weft_proto::Target::Channel(channel.parse().unwrap()),
            sender: "carol@kde.org".parse().unwrap(),
            msgid: minted.parse().unwrap(),
            body: "hi from matrix".into(),
            meta: weft_proto::MsgMeta::default(),
            edited: None,
            edited_at: None,
        })),
        Some(&label),
        None,
    )
    .await;
    let canonical = minted.parse::<weft_proto::MsgId>().unwrap().to_string();
    assert_eq!(
        bridge.store.state.links.msgid_of("$carol1"),
        Some(canonical.as_str()),
        "the echo linked the minted id"
    );
    assert_eq!(
        calls.lock().unwrap().len(),
        sends_before,
        "an echo is an ack, not relay fodder"
    );

    // §8 outbound sense: carol's first projected-room join is the namespace
    // join statement; leaving her last room is the part.
    bridge
        .on_matrix_event(json!({
            "type": "m.room.member",
            "room_id": room_id,
            "event_id": "$j1",
            "sender": "@carol:kde.org",
            "state_key": "@carol:kde.org",
            "origin_server_ts": 1_722_000_000_001u64,
            "content": { "membership": "join" },
        }))
        .await;
    let sent = drain(&mut lines);
    assert!(
        sent.iter()
            .any(|l| l.contains("NS-MEMBER") && l.contains("carol@kde.org join")),
        "{sent:?}"
    );
    bridge
        .on_matrix_event(json!({
            "type": "m.room.member",
            "room_id": room_id,
            "event_id": "$l1",
            "sender": "@carol:kde.org",
            "state_key": "@carol:kde.org",
            "origin_server_ts": 1_722_000_000_002u64,
            "content": { "membership": "leave" },
        }))
        .await;
    let sent = drain(&mut lines);
    assert!(
        sent.iter()
            .any(|l| l.contains("NS-MEMBER") && l.contains("carol@kde.org part")),
        "{sent:?}"
    );
}

#[tokio::test]
async fn authority_translates_both_directions() {
    // §10: capabilities here, power levels there — a WEFT grant becomes a
    // level write; a Matrix PL change becomes the acting moderator's
    // attributed GRANT/REVOKE, which weftd checks against *their* grants.
    let (mut bridge, mut lines, calls) = bridge_with(BTreeMap::new()).await;
    let ns_id = ulid::Ulid::new().to_string().to_lowercase();
    let chan_id = ulid::Ulid::new().to_string().to_lowercase();
    let channel = format!("#{ns_id}/{chan_id}");

    // A projected namespace with one room, and ada known to the bridge.
    deliver(
        &mut bridge,
        weft_proto::Event::NsMeta {
            id: ns_id.parse().unwrap(),
            vanity: "gaming".parse().unwrap(),
            visibility: weft_proto::Visibility::Public,
            owner: Some("ada".into()),
            title: None,
            description: None,
            icon: None,
            recovery_set: false,
            recovery_pending: None,
            categories: Vec::new(),
            federation: false,
            welcome: None,
            origin: None,
            provider_online: None,
            authority: None,
            settings_disabled: Vec::new(),
            bridges: vec!["matrix".parse().unwrap()],
        },
        None,
        None,
    )
    .await;
    deliver(
        &mut bridge,
        weft_proto::Event::ChannelLayout {
            channel: channel.parse().unwrap(),
            category: None,
            position: 0,
            kind: weft_proto::ChannelKind::Text,
            vanity: "general".into(),
            origin: None,
        },
        None,
        None,
    )
    .await;
    deliver(
        &mut bridge,
        weft_proto::Event::Policy {
            channel: channel.parse().unwrap(),
            policy: weft_proto::RetentionPolicy::Permanent,
        },
        None,
        None,
    )
    .await;
    join_ada(&mut bridge, &ns_id).await;
    drain(&mut lines);
    let room_id = format!("!weft_{chan_id}:test.example");

    // WEFT → Matrix: a bare grant relay (weftd tells the fact, the level is
    // ours): carol becomes a moderator → 50 in every room of the space.
    bridge
        .on_incoming(weft_appservice::Incoming::Command {
            label: None,
            as_user: None,
            as_ulid: None,
            command: weft_proto::Command::Grant {
                subject: "carol@kde.org".into(),
                scope: format!("ns:{ns_id}"),
                caps: "delete-any".into(),
                expiry: None,
            },
        })
        .await;
    {
        let recorded = calls.lock().unwrap();
        let pl_writes: Vec<_> = recorded
            .iter()
            .filter(|(what, _, _)| what.contains("m.room.power_levels"))
            .collect();
        assert_eq!(pl_writes.len(), 2, "the room and the space: {pl_writes:?}");
        assert_eq!(pl_writes[0].2["users"]["@carol:kde.org"], 50);
    }

    // …and a local subject addresses their ULID-keyed puppet.
    bridge
        .on_incoming(weft_appservice::Incoming::Command {
            label: None,
            as_user: None,
            as_ulid: Some(ADA_ULID.into()),
            command: weft_proto::Command::Grant {
                subject: "ada".into(),
                scope: format!("ns:{ns_id}"),
                caps: "ns-admin".into(),
                expiry: None,
            },
        })
        .await;
    // (the relay carries the subject's ULID, so a grant for a local user the
    // bridge has not seen post yet still addresses the right puppet)
    {
        let recorded = calls.lock().unwrap();
        let last = recorded
            .iter()
            .rev()
            .find(|(what, _, _)| what.contains("m.room.power_levels"))
            .unwrap();
        assert_eq!(
            last.2["users"][format!("@weft_{ADA_ULID}:test.example")],
            90
        );
    }

    // Matrix → WEFT: a kde.org moderator raises carol — the diff becomes an
    // attributed revoke-then-grant of the mapped tier.
    bridge
        .on_matrix_event(json!({
            "type": "m.room.power_levels",
            "room_id": room_id,
            "event_id": "$pl1",
            "sender": "@mod:kde.org",
            "origin_server_ts": 1_722_000_000_000u64,
            "content": { "users": { "@carol:kde.org": 50 } },
        }))
        .await;
    let sent = drain(&mut lines);
    assert!(
        sent.iter()
            .any(|l| l.contains("REVOKE carol@kde.org") && l.contains("as=mod@kde.org")),
        "{sent:?}"
    );
    assert!(
        sent.iter().any(|l| l.contains("GRANT carol@kde.org")
            && l.contains("mute,ban,kick,delete-any")
            && l.contains("as=mod@kde.org")),
        "{sent:?}"
    );

    // The same map again: the baseline moved, so nothing changes — no lines.
    bridge
        .on_matrix_event(json!({
            "type": "m.room.power_levels",
            "room_id": room_id,
            "event_id": "$pl2",
            "sender": "@mod:kde.org",
            "origin_server_ts": 1_722_000_000_001u64,
            "content": { "users": { "@carol:kde.org": 50 } },
        }))
        .await;
    assert!(
        drain(&mut lines).is_empty(),
        "an unchanged map must not re-grant"
    );

    // A Matrix mod bans ada's puppet: the attributed BAN, target = the bare
    // local account — weftd checks the actor's grants, not us.
    bridge
        .on_matrix_event(json!({
            "type": "m.room.member",
            "room_id": room_id,
            "event_id": "$ban1",
            "sender": "@mod:kde.org",
            "state_key": format!("@weft_{ADA_ULID}:test.example"),
            "origin_server_ts": 1_722_000_000_002u64,
            "content": { "membership": "ban", "reason": "spam" },
        }))
        .await;
    let sent = drain(&mut lines);
    assert!(
        sent.iter().any(|l| l.contains("BAN")
            && l.contains(&format!("ns:{ns_id} ada"))
            && l.contains("as=mod@kde.org")),
        "{sent:?}"
    );
}

/// A projected namespace with one room, ada known — the substrate the
/// management flows act on.
async fn projected_fixture(
    bridge: &mut Bridge,
    lines: &mut tokio::sync::mpsc::Receiver<String>,
) -> (String, String, String) {
    let ns_id = ulid::Ulid::new().to_string().to_lowercase();
    let chan_id = ulid::Ulid::new().to_string().to_lowercase();
    let channel = format!("#{ns_id}/{chan_id}");

    deliver(
        bridge,
        weft_proto::Event::NsMeta {
            id: ns_id.parse().unwrap(),
            vanity: "gaming".parse().unwrap(),
            visibility: weft_proto::Visibility::Public,
            owner: Some("ada".into()),
            title: None,
            description: None,
            icon: None,
            recovery_set: false,
            recovery_pending: None,
            categories: Vec::new(),
            federation: false,
            welcome: None,
            origin: None,
            provider_online: None,
            authority: None,
            settings_disabled: Vec::new(),
            bridges: vec!["matrix".parse().unwrap()],
        },
        None,
        None,
    )
    .await;
    deliver(
        bridge,
        weft_proto::Event::ChannelLayout {
            channel: channel.parse().unwrap(),
            category: None,
            position: 0,
            kind: weft_proto::ChannelKind::Text,
            vanity: "general".into(),
            origin: None,
        },
        None,
        None,
    )
    .await;
    deliver(
        bridge,
        weft_proto::Event::Policy {
            channel: channel.parse().unwrap(),
            policy: weft_proto::RetentionPolicy::Permanent,
        },
        None,
        None,
    )
    .await;
    join_ada(bridge, &ns_id).await;
    drain(lines);

    (ns_id, channel, format!("!weft_{chan_id}:test.example"))
}

#[tokio::test]
async fn management_flows_open_views_and_act_as_the_invoker() {
    // Slice 11's SDUI half: a management action opens a view, and its submit
    // issues **attributed** commands — the invoker's authority, not ours.
    let (mut bridge, mut lines, calls) = bridge_with(BTreeMap::new()).await;
    let (ns_id, channel, room_id) = projected_fixture(&mut bridge, &mut lines).await;

    // Power Levels: the view lists the live map and offers the mapped tiers.
    bridge
        .on_invoke("v1", "power-levels", Some(&ns_id), Some("ada@test.example"))
        .await;
    let sent = drain(&mut lines);
    let view = sent
        .iter()
        .find(|l| l.contains("PLUGIN-VIEW"))
        .expect("a view opened");
    assert!(view.contains("v1"), "{view}");

    // Its submit writes the level on Matrix **and** mirrors the mapped caps as
    // ada's own GRANT — labeled, so a refusal can be reverted (§10).
    bridge
        .on_step(
            "v1",
            None,
            &[
                ("mxid".to_string(), json!("@carol:kde.org")),
                ("level".to_string(), json!("50")),
            ]
            .into_iter()
            .collect(),
            false,
        )
        .await;

    {
        let recorded = calls.lock().unwrap();
        let pl = recorded
            .iter()
            .rev()
            .find(|(what, _, _)| what.contains("m.room.power_levels"))
            .expect("the Matrix write happened");
        assert_eq!(pl.2["users"]["@carol:kde.org"], 50);
    }
    let sent = drain(&mut lines);
    assert!(
        sent.iter().any(|l| l.contains("GRANT carol@kde.org")
            && l.contains("as=ada@test.example")
            && l.contains("label=act-")),
        "the grant is the invoker's, and labeled: {sent:?}"
    );
    // Terminal: the flow answered and closed.
    assert!(sent.iter().any(|l| l.contains("PLUGIN-RESULT")), "{sent:?}");

    // Invite: opens on a channel, and invites through the HS.
    bridge
        .on_invoke("v2", "invite", Some(&channel), Some("ada@test.example"))
        .await;
    drain(&mut lines);
    bridge
        .on_step(
            "v2",
            None,
            &[("mxid".to_string(), json!("@dave:kde.org"))]
                .into_iter()
                .collect(),
            false,
        )
        .await;
    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .any(|(what, _, body)| what.starts_with("POST invite")
                && body["user_id"] == "@dave:kde.org"),
        "the invite reached the homeserver"
    );

    // An action on something unbridged refuses rather than half-acting.
    bridge
        .on_invoke(
            "v3",
            "power-levels",
            Some("01hxnope"),
            Some("ada@test.example"),
        )
        .await;
    let sent = drain(&mut lines);
    assert!(
        sent.iter().any(|l| l.contains("PLUGIN-RESULT")),
        "an unbridged target answers, never hangs: {sent:?}"
    );

    // Dismissing a flow is terminal and leaks nothing.
    bridge
        .on_invoke("v4", "moderate", Some("bob"), Some("ada@test.example"))
        .await;
    drain(&mut lines);
    bridge.on_step("v4", None, &Default::default(), true).await;
    assert!(bridge.flows.is_empty(), "a closed flow is forgotten");

    let _ = room_id;
}

#[tokio::test]
async fn a_refused_act_is_reverted_on_the_matrix_side() {
    // §10's other half: the Matrix state changed before WEFT agreed, so a
    // refusal must undo it — otherwise Matrix shows authority WEFT denied.
    let (mut bridge, mut lines, calls) = bridge_with(BTreeMap::new()).await;
    let (ns_id, _channel, room_id) = projected_fixture(&mut bridge, &mut lines).await;

    // A kde.org moderator promotes carol; we translate (and park the undo).
    bridge
        .on_matrix_event(json!({
            "type": "m.room.power_levels",
            "room_id": room_id,
            "event_id": "$pl1",
            "sender": "@mod:kde.org",
            "origin_server_ts": 1_722_000_000_000u64,
            "content": { "users": { "@carol:kde.org": 50 } },
        }))
        .await;
    let sent = drain(&mut lines);
    let label = sent
        .iter()
        .find_map(|l| l.split("label=").nth(1))
        .map(|l| l.split([';', ' ']).next().unwrap().to_string())
        .expect("the act was labeled");
    let before = calls.lock().unwrap().len();

    // weftd refuses: the moderator holds no `grant:mute` here.
    deliver(
        &mut bridge,
        weft_proto::Event::Err(weft_proto::ErrEvent {
            code: weft_proto::ErrCode::CapRequired,
            context: Some("grant:mute".into()),
            text: "not permitted".into(),
            retry_after: None,
            max: None,
        }),
        Some(&label),
        None,
    )
    .await;

    // Snapshot rather than hold the guard: the assertions below straddle an
    // await, and a std MutexGuard must not.
    let after: Vec<(String, String, Value)> =
        calls.lock().unwrap().iter().skip(before).cloned().collect();
    // The level went back to its previous value (absent = removed)…
    let reverted = after
        .iter()
        .find(|(what, _, _)| what.contains("m.room.power_levels"))
        .expect("the level was reverted");
    assert!(
        reverted.2["users"].get("@carol:kde.org").is_none(),
        "carol's level is gone again: {:?}",
        reverted.2
    );
    // …and the actor was told why (§10: revert **and** notice).
    let notice = after
        .iter()
        .find(|(what, _, _)| what.contains("m.room.message"))
        .expect("a notice was posted");
    assert_eq!(notice.2["msgtype"], "m.notice");
    assert!(
        notice.2["body"].as_str().unwrap().contains("not permitted"),
        "{:?}",
        notice.2
    );

    // The label is spent: a second ERR cannot re-revert.
    let before = calls.lock().unwrap().len();
    deliver(
        &mut bridge,
        weft_proto::Event::Err(weft_proto::ErrEvent {
            code: weft_proto::ErrCode::CapRequired,
            context: None,
            text: "again".into(),
            retry_after: None,
            max: None,
        }),
        Some(&label),
        None,
    )
    .await;
    assert_eq!(calls.lock().unwrap().len(), before, "one revert per act");
    let _ = ns_id;
}

#[tokio::test]
async fn create_room_acts_on_whichever_side_owns_the_room() {
    // Projected namespace: the WEFT channel is the real object, so the flow
    // issues the **invoker's** CHANNEL CREATE with `permanent` retention (the
    // only policy that projects, §3) and creates nothing on Matrix — weftd's
    // structure push brings the room back through the ordinary path.
    let (mut bridge, mut lines, calls) = bridge_with(BTreeMap::new()).await;
    let (ns_id, _channel, _room) = projected_fixture(&mut bridge, &mut lines).await;

    bridge
        .on_invoke("c1", "create-room", Some(&ns_id), Some("ada@test.example"))
        .await;
    drain(&mut lines);
    let creates_before = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(what, _, _)| what == "POST createRoom")
        .count();

    bridge
        .on_step(
            "c1",
            None,
            &[("name".to_string(), json!("Announcements!"))]
                .into_iter()
                .collect(),
            false,
        )
        .await;

    let sent = drain(&mut lines);
    let create = sent
        .iter()
        .find(|l| l.contains("CHANNEL CREATE"))
        .expect("the channel create went out");
    assert!(create.contains("as=ada@test.example"), "{create}");
    assert!(
        create.contains("permanent"),
        "only permanent projects: {create}"
    );
    assert!(
        create.contains("announcements"),
        "the name is vanity-cased: {create}"
    );
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(what, _, _)| what == "POST createRoom")
            .count(),
        creates_before,
        "the projected path creates no room itself — weftd's push does"
    );

    // Consumed space: weftd refuses local creates in a replica, so the room is
    // created on Matrix and asserted back — and filed in the **consumed** map,
    // because its events are realm-minted.
    let (mut bridge, mut lines, calls) = bridge_with(kde_space()).await;
    bridge
        .provision("matrix://kde.org/community")
        .await
        .unwrap();
    drain(&mut lines);
    let ns_id = ident::stable_ulid("!space:kde.org");

    bridge
        .on_invoke("c2", "create-room", Some(&ns_id), Some("ada@test.example"))
        .await;
    drain(&mut lines);
    bridge
        .on_step(
            "c2",
            None,
            &[("name".to_string(), json!("Offtopic"))]
                .into_iter()
                .collect(),
            false,
        )
        .await;

    {
        let recorded = calls.lock().unwrap();
        assert!(
            recorded
                .iter()
                .any(|(what, _, body)| what == "POST createRoom" && body["name"] == "Offtopic"),
            "the room was created on Matrix: {recorded:?}"
        );
        assert!(
            recorded
                .iter()
                .any(|(what, _, _)| what.contains("m.space.child")),
            "…and linked under the Space"
        );
    }
    let sent = drain(&mut lines);
    assert!(
        sent.iter().any(|l| l.contains("CHANNEL-LAYOUT")),
        "the room is asserted into WEFT: {sent:?}"
    );
    assert!(
        !sent.iter().any(|l| l.contains("CHANNEL CREATE")),
        "a replica's channels are asserted, never created locally: {sent:?}"
    );

    // Filed as consumed (realm-minted), not as a projection (home-minted):
    // the projection map would route its events through the injection door,
    // where the *home* mints — two ids for every message.
    let space = bridge
        .store
        .state
        .spaces
        .get("matrix://kde.org/community")
        .expect("the space is stored");
    assert_eq!(
        space.rooms.len(),
        2,
        "the provisioned room plus the new one: {:?}",
        space.rooms
    );
    let new_room = space
        .rooms
        .keys()
        .find(|id| id.as_str() != "!gen:kde.org")
        .cloned()
        .expect("the new room is in the consumed map");
    assert!(
        bridge
            .store
            .state
            .channel_of_projected_room(&new_room)
            .is_none(),
        "…and not in the projection map"
    );
}

#[tokio::test]
async fn the_kick_flow_picks_its_scope_and_reverts_when_refused() {
    // §13.2: a member action's ctx-ref is `user@net` — no channel — so the kick
    // asks which one, and the ban derives its namespace from that answer.
    let (mut bridge, mut lines, calls) = bridge_with(BTreeMap::new()).await;
    let (ns_id, channel, room_id) = projected_fixture(&mut bridge, &mut lines).await;

    bridge
        .on_invoke(
            "k1",
            "moderate",
            Some("carol@kde.org"),
            Some("ada@test.example"),
        )
        .await;
    let sent = drain(&mut lines);
    let view = sent
        .iter()
        .find(|l| l.contains("PLUGIN-VIEW"))
        .expect("the moderate view opened");
    let payload = view
        .split("view=")
        .nth(1)
        .and_then(|v| v.split([';', ' ']).next())
        .expect("a view payload");
    let decoded: weft_proto::View = weft_proto::plugin_from_b64(payload).expect("decodes");
    let Some(weft_proto::Component::Select { options, .. }) = decoded
        .blocks
        .iter()
        .find(|b| matches!(b, weft_proto::Component::Select { .. }))
    else {
        panic!("the scope picker lists the bridged channels");
    };
    assert!(
        options.iter().any(|o| o.value == channel),
        "the projected channel is offered: {options:?}"
    );

    // A **foreign** member takes the ordinary WEFT path now that §6.7's verbs
    // name a member rather than an account: an attributed `KICK` that weftd
    // capability-checks. This action used to call the homeserver directly,
    // because `carol@kde.org` could not be expressed on the wire at all — which
    // made the one moderation act that skipped weftd's check the one aimed at
    // the people least able to answer for it.
    bridge
        .on_step(
            "k1",
            Some("kick"),
            &[
                ("channel".to_string(), json!(channel)),
                ("reason".to_string(), json!("spam")),
            ]
            .into_iter()
            .collect(),
            false,
        )
        .await;
    let kick = drain(&mut lines);
    let kick = kick
        .iter()
        .find(|l| l.contains("KICK"))
        .expect("an attributed WEFT KICK");
    assert!(
        kick.contains("carol@kde.org") && kick.contains("as=ada@test.example"),
        "the kick names the foreign member and its actor: {kick}"
    );
    assert!(
        !calls
            .lock()
            .unwrap()
            .iter()
            .any(|(what, _, _)| what.starts_with("POST kick/")),
        "nothing happens on Matrix until weftd authorizes it"
    );

    // …and when weftd does authorize it, the act comes back to be applied.
    bridge
        .on_incoming(weft_appservice::Incoming::Command {
            label: None,
            as_user: None,
            as_ulid: None,
            command: weft_proto::Command::Kick {
                channel: channel.parse().unwrap(),
                member: "carol@kde.org".parse().unwrap(),
                reason: Some("spam".to_string()),
            },
        })
        .await;
    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .any(|(what, _, body)| what == &format!("POST kick/{room_id}")
                && body["user_id"] == "@carol:kde.org"
                && body["reason"] == "spam"),
        "the authorized kick removed her from the room"
    );

    // A **local** member takes the WEFT path: an attributed BAN whose scope is
    // derived from the chosen channel — no guessing at `*`.
    join_ada(&mut bridge, &ns_id).await;
    drain(&mut lines);
    bridge
        .on_invoke("k2", "moderate", Some("bob"), Some("ada@test.example"))
        .await;
    drain(&mut lines);
    bridge
        .on_step(
            "k2",
            Some("ban"),
            &[("channel".to_string(), json!(channel))]
                .into_iter()
                .collect(),
            false,
        )
        .await;
    let sent = drain(&mut lines);
    let ban = sent
        .iter()
        .find(|l| l.contains("BAN"))
        .expect("the ban went out");
    assert!(ban.contains(&format!("BAN ns:{ns_id} bob")), "{ban}");
    assert!(ban.contains("as=ada@test.example"), "{ban}");
    let label = ban
        .split("label=")
        .nth(1)
        .map(|l| l.split([';', ' ']).next().unwrap().to_string())
        .expect("the ban is labeled");

    // Refused → the Matrix-side ban is lifted and the actor is told.
    let before = calls.lock().unwrap().len();
    deliver(
        &mut bridge,
        weft_proto::Event::Err(weft_proto::ErrEvent {
            code: weft_proto::ErrCode::CapRequired,
            context: Some("ban".into()),
            text: "not a moderator here".into(),
            retry_after: None,
            max: None,
        }),
        Some(&label),
        None,
    )
    .await;
    let after: Vec<(String, String, Value)> =
        calls.lock().unwrap().iter().skip(before).cloned().collect();
    // bob has no puppet (he never reached this bridge), so there is no
    // Matrix-side ban to lift — the notice is then the *whole* remedy, and it
    // still names the reason. A refused act always reports.
    assert!(
        !after
            .iter()
            .any(|(what, _, _)| what.starts_with("POST unban/")),
        "nothing to unban: {after:?}"
    );
    assert!(
        after
            .iter()
            .any(|(what, _, body)| what.contains("m.room.message")
                && body["msgtype"] == "m.notice"
                && body["body"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("not a moderator here")),
        "…but the refusal was reported: {after:?}"
    );
    let _ = room_id;
}

#[tokio::test]
async fn categories_become_subspaces_and_parent_their_rooms() {
    // matrix.md §6 / locked decision 4: a WEFT category is a child Space, and a
    // categorized channel's room hangs under it rather than the top Space.
    let (mut bridge, mut lines, calls) = bridge_with(BTreeMap::new()).await;
    let ns_id = ulid::Ulid::new().to_string().to_lowercase();
    let chan_id = ulid::Ulid::new().to_string().to_lowercase();
    let channel = format!("#{ns_id}/{chan_id}");

    let meta = |cats: Vec<String>| weft_proto::Event::NsMeta {
        id: ns_id.parse().unwrap(),
        vanity: "gaming".parse().unwrap(),
        visibility: weft_proto::Visibility::Public,
        owner: Some("ada".into()),
        title: None,
        description: None,
        icon: None,
        recovery_set: false,
        recovery_pending: None,
        categories: cats,
        federation: false,
        welcome: None,
        origin: None,
        provider_online: None,
        authority: None,
        settings_disabled: Vec::new(),
        bridges: vec!["matrix".parse().unwrap()],
    };

    deliver(
        &mut bridge,
        meta(vec!["Text".into(), "Voice".into()]),
        None,
        None,
    )
    .await;

    {
        let recorded = calls.lock().unwrap();
        let spaces: Vec<_> = recorded
            .iter()
            .filter(|(what, _, body)| {
                what == "POST createRoom" && body["creation_content"]["type"] == "m.space"
            })
            .collect();
        assert_eq!(
            spaces.len(),
            3,
            "the top Space plus two sub-spaces: {spaces:?}"
        );
        assert_eq!(spaces[1].2["name"], "Text");
        assert_eq!(spaces[2].2["name"], "Voice");
    }
    let text_space = bridge.store.state.projections[&ns_id].categories["Text"].clone();

    // A categorized channel is parented under its category's sub-space.
    deliver(
        &mut bridge,
        weft_proto::Event::ChannelLayout {
            channel: channel.parse().unwrap(),
            category: Some("Text".into()),
            position: 3,
            kind: weft_proto::ChannelKind::Text,
            vanity: "general".into(),
            origin: None,
        },
        None,
        None,
    )
    .await;
    deliver(
        &mut bridge,
        weft_proto::Event::Policy {
            channel: channel.parse().unwrap(),
            policy: weft_proto::RetentionPolicy::Permanent,
        },
        None,
        None,
    )
    .await;

    {
        let recorded = calls.lock().unwrap();
        assert!(
            recorded.iter().any(|(what, _, body)| what
                .starts_with(&format!("PUT state/{text_space}/m.space.child"))
                && body["order"] == "0000000003"),
            "the room hangs under the Text sub-space, ordered by position: {recorded:?}"
        );
    }

    // Re-delivering the same list is idempotent — no duplicate sub-spaces.
    let before = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(what, _, _)| what == "POST createRoom")
        .count();
    deliver(
        &mut bridge,
        meta(vec!["Text".into(), "Voice".into()]),
        None,
        None,
    )
    .await;
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(what, _, _)| what == "POST createRoom")
            .count(),
        before,
        "an unchanged list creates nothing"
    );

    // The create-subspace flow appends to **weftd's** declared list, so a
    // category weftd knows but we never projected cannot be deleted by it.
    bridge
        .store
        .state
        .projections
        .get_mut(&ns_id)
        .unwrap()
        .declared_categories = vec!["Text".into(), "Voice".into(), "Archive".into()];
    bridge
        .on_invoke(
            "s1",
            "create-subspace",
            Some(&ns_id),
            Some("ada@test.example"),
        )
        .await;
    drain(&mut lines);
    bridge
        .on_step(
            "s1",
            None,
            &[("name".to_string(), json!("Announcements"))]
                .into_iter()
                .collect(),
            false,
        )
        .await;

    let sent = drain(&mut lines);
    let meta_set = sent
        .iter()
        .find(|l| l.contains("NS META"))
        .expect("the category list was set");
    assert!(
        meta_set.contains("Text,Voice,Archive,Announcements"),
        "appended to weftd's list, not to our sub-spaces: {meta_set}"
    );
    assert!(meta_set.contains("as=ada@test.example"), "{meta_set}");

    // A comma would corrupt the list; a duplicate is refused.
    for bad in ["Text", "A,B"] {
        bridge
            .on_invoke(
                "s2",
                "create-subspace",
                Some(&ns_id),
                Some("ada@test.example"),
            )
            .await;
        drain(&mut lines);
        bridge
            .on_step(
                "s2",
                None,
                &[("name".to_string(), json!(bad))].into_iter().collect(),
                false,
            )
            .await;
        let sent = drain(&mut lines);
        assert!(
            sent.iter().any(|l| l.contains("PLUGIN-RESULT")),
            "{bad} is refused with an answer: {sent:?}"
        );
        assert!(
            !sent.iter().any(|l| l.contains("NS META")),
            "{bad} must not reach weftd: {sent:?}"
        );
    }
}

#[tokio::test]
async fn backfill_replays_a_window_as_ordinary_ingestion() {
    // Protocol doc §8: weftd's HISTORY is answered by replaying the window as
    // ordinary ingestion — no separate ingress. The replay is oldest-first
    // (the replica orders by ULID time) and idempotent (msgids derive from the
    // event id + its timestamp).
    let mut rooms = kde_space();
    // The room's scrollback, newest-first as Matrix returns it.
    rooms.insert(
        "__messages__".to_string(),
        vec![
            json!({ "type": "m.room.message", "event_id": "$old3", "sender": "@carol:kde.org",
                    "origin_server_ts": 3_000u64, "content": { "body": "third" } }),
            json!({ "type": "m.room.message", "event_id": "$old2", "sender": "@dave:kde.org",
                    "origin_server_ts": 2_000u64, "content": { "body": "second" } }),
            // Our own puppet: already WEFT-origin, must not be ingested back.
            json!({ "type": "m.room.message", "event_id": "$mine", "sender": "@weft_ada:test.example",
                    "origin_server_ts": 1_500u64, "content": { "body": "ours" } }),
            json!({ "type": "m.room.message", "event_id": "$old1", "sender": "@carol:kde.org",
                    "origin_server_ts": 1_000u64, "content": { "body": "first" } }),
            // Not a message: skipped in v1.
            json!({ "type": "m.reaction", "event_id": "$r", "sender": "@carol:kde.org",
                    "origin_server_ts": 900u64, "content": {} }),
        ],
    );
    let (mut bridge, mut lines, calls) = bridge_with(rooms).await;
    bridge
        .provision("matrix://kde.org/community")
        .await
        .unwrap();
    drain(&mut lines);
    let channel = bridge
        .store
        .state
        .channel_of_room("!gen:kde.org")
        .unwrap()
        .0
        .channel
        .clone();

    // weftd asks for the window before the oldest message it holds. Anchor it
    // on a message we already know, so the token can be resolved.
    let anchor = format!("kde.org/{}", ulid::Ulid::new().to_string().to_lowercase());
    bridge.store.link("$anchor", &anchor, "!gen:kde.org").await;
    bridge
        .on_incoming(weft_appservice::Incoming::Command {
            label: None,
            as_user: None,
            as_ulid: None,
            command: weft_proto::Command::History {
                target: weft_proto::Target::Channel(channel.parse().unwrap()),
                before: Some(anchor.parse().unwrap()),
                after: None,
                limit: Some(50),
                thread: None,
            },
        })
        .await;

    // The anchor was resolved through /context, then a backwards page fetched.
    {
        let recorded = calls.lock().unwrap();
        assert!(
            recorded
                .iter()
                .any(|(what, _, _)| what.contains("context/") && what.contains("$anchor")),
            "the anchor became a pagination token: {recorded:?}"
        );
        assert!(
            recorded
                .iter()
                .any(|(what, q, _)| what.contains("messages") && q.contains("dir=b")),
            "…and the page walks backwards: {recorded:?}"
        );
    }

    // Replayed oldest-first, ours and non-messages skipped.
    let sent = drain(&mut lines);
    let bodies: Vec<&str> = sent
        .iter()
        .filter(|l| l.contains("MSG"))
        .filter_map(|l| l.split(" :").nth(1))
        .collect();
    assert_eq!(
        bodies,
        ["first", "second", "third"],
        "oldest first: {sent:?}"
    );
    assert!(
        !sent.iter().any(|l| l.contains("ours")),
        "our own puppet's message must not be ingested back: {sent:?}"
    );

    // Deterministic ids: replaying the same window sends nothing new.
    let before = drain(&mut lines).len();
    bridge
        .on_incoming(weft_appservice::Incoming::Command {
            label: None,
            as_user: None,
            as_ulid: None,
            command: weft_proto::Command::History {
                target: weft_proto::Target::Channel(channel.parse().unwrap()),
                before: Some(anchor.parse().unwrap()),
                after: None,
                limit: Some(50),
                thread: None,
            },
        })
        .await;
    assert_eq!(
        drain(&mut lines).len(),
        before,
        "a re-fetched window replays nothing — every event is already linked"
    );
}

#[tokio::test]
async fn media_crosses_both_ways_as_a_copy() {
    // matrix.md §12: neither side can fetch the other's blobs, so each
    // direction downloads and re-uploads. Inbound waits for weftd's upload
    // grant before sending the message — a reference to a blob weftd does not
    // hold yet renders as a broken attachment.
    let (mut bridge, mut lines, calls) = bridge_with(kde_space()).await;
    bridge
        .provision("matrix://kde.org/community")
        .await
        .unwrap();
    drain(&mut lines);
    let channel = bridge
        .store
        .state
        .channel_of_room("!gen:kde.org")
        .unwrap()
        .0
        .channel
        .clone();

    // Matrix → WEFT: an m.image is downloaded, then offered.
    bridge
        .on_matrix_event(json!({
            "type": "m.room.message",
            "room_id": "!gen:kde.org",
            "event_id": "$img1",
            "sender": "@carol:kde.org",
            "origin_server_ts": 1_722_000_000_000u64,
            "content": {
                "msgtype": "m.image",
                "body": "cat.png",
                "url": "mxc://kde.org/blob1",
                "info": { "mimetype": "image/png" },
            },
        }))
        .await;

    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .any(|(what, _, _)| what.contains("media/download") && what.contains("blob1")),
        "the blob was downloaded from the homeserver"
    );
    let sent = drain(&mut lines);
    let offer = sent
        .iter()
        .find(|l| l.contains("STREAM OFFER"))
        .expect("an upload grant was requested");
    assert!(offer.contains("image/png"), "{offer}");
    assert!(
        !sent.iter().any(|l| l.contains("MSG")),
        "the message waits for the blob: {sent:?}"
    );
    let label = offer
        .split("label=")
        .nth(1)
        .map(|l| l.split([';', ' ']).next().unwrap().to_string())
        .expect("the offer is labeled");

    // The grant arrives → the blob is posted and the message references it.
    deliver(
        &mut bridge,
        weft_proto::Event::StreamAccept {
            token: "grant-1".into(),
        },
        Some(&label),
        None,
    )
    .await;

    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .any(|(what, q, _)| what == "POST media" && q.contains("t=grant-1")),
        "the blob was posted with its grant"
    );
    let sent = drain(&mut lines);
    let msg = sent
        .iter()
        .find(|l| l.contains("MSG"))
        .expect("the message");
    assert!(msg.contains("attach.1=weft-media://"), "{msg}");
    assert!(msg.contains("as=carol@kde.org"), "{msg}");

    // WEFT → Matrix: a local message's attachment becomes its own event.
    join_ada(&mut bridge, &ident::stable_ulid("!space:kde.org")).await;
    drain(&mut lines);
    let msgid: weft_proto::MsgId = format!("test.example/{}", ulid::Ulid::new())
        .to_lowercase()
        .parse()
        .unwrap();
    deliver(
        &mut bridge,
        weft_proto::Event::Message(Box::new(weft_proto::MessageEvent {
            target: weft_proto::Target::Channel(channel.parse().unwrap()),
            sender: "ada@test.example".parse().unwrap(),
            msgid,
            body: "look".into(),
            meta: weft_proto::MsgMeta {
                attachments: vec!["weft-media://cafebabe".into()],
                ..weft_proto::MsgMeta::default()
            },
            edited: None,
            edited_at: None,
        })),
        None,
        Some(ADA_ULID),
    )
    .await;

    let recorded = calls.lock().unwrap().clone();
    assert!(
        recorded
            .iter()
            .any(|(what, _, _)| what == "GET weft-media/cafebabe"),
        "the blob was fetched from weftd: {recorded:?}"
    );
    assert!(
        recorded
            .iter()
            .any(|(what, _, _)| what == "POST matrix-upload"),
        "…uploaded to the homeserver: {recorded:?}"
    );
    let (_, _, body) = recorded
        .iter()
        .rev()
        .find(|(what, _, _)| what.contains("m.room.message"))
        .expect("an attachment event");
    assert_eq!(body["msgtype"], "m.image", "sniffed from the bytes");
    assert!(
        body["url"]
            .as_str()
            .unwrap_or_default()
            .starts_with("mxc://"),
        "{body}"
    );
}

#[tokio::test]
async fn dms_and_typing_cross_the_bridge() {
    // Protocol doc §5 + matrix.md §15. A bridged DM is a first-class WEFT DM,
    // and the Matrix side of it is a real DM room owned by the two people in it
    // — created as the puppet, not as the bridge bot.
    let (mut bridge, mut lines, calls) = bridge_with(kde_space()).await;
    bridge
        .provision("matrix://kde.org/community")
        .await
        .unwrap();
    drain(&mut lines);
    let ns_id = ident::stable_ulid("!space:kde.org");
    let channel = bridge
        .store
        .state
        .channel_of_room("!gen:kde.org")
        .unwrap()
        .0
        .channel
        .clone();
    join_ada(&mut bridge, &ns_id).await;
    drain(&mut lines);

    // WEFT → Matrix: ada DMs carol. The room is opened on first use.
    bridge
        .on_incoming(weft_appservice::Incoming::Command {
            label: None,
            as_user: Some("ada@test.example".into()),
            as_ulid: Some(ADA_ULID.into()),
            command: weft_proto::Command::Msg {
                target: weft_proto::Target::User {
                    account: "carol".parse().unwrap(),
                    network: Some("kde.org".parse().unwrap()),
                },
                body: Some("hi carol".into()),
                meta: weft_proto::MsgMeta::default(),
            },
        })
        .await;

    let dm_room = {
        let recorded = calls.lock().unwrap();
        let (_, query, body) = recorded
            .iter()
            .find(|(what, _, body)| what == "POST createRoom" && body["is_direct"] == true)
            .expect("a DM room was created");
        assert!(
            query.contains(&format!("weft_{ADA_ULID}")),
            "created as ada's puppet, not the bot: {query}"
        );
        assert_eq!(body["invite"][0], "@carol:kde.org");
        format!("!{}:test.example", "noalias")
    };
    let sent_dm = calls.lock().unwrap().iter().any(|(what, q, body)| {
        what.starts_with("PUT send/")
            && what.contains("m.room.message")
            && q.contains(&format!("weft_{ADA_ULID}"))
            && body["body"] == "hi carol"
    });
    assert!(sent_dm, "the DM was sent as her puppet");

    // Matrix → WEFT: carol replies in that room; it ingests as a WEFT DM
    // addressed to ada, not as a channel message.
    bridge
        .on_matrix_event(json!({
            "type": "m.room.message",
            "room_id": dm_room,
            "event_id": "$dm1",
            "sender": "@carol:kde.org",
            "origin_server_ts": 1_722_000_000_000u64,
            "content": { "msgtype": "m.text", "body": "hi ada" },
        }))
        .await;
    let sent = drain(&mut lines);
    let dm = sent
        .iter()
        .find(|l| l.contains("MSG @ada"))
        .expect("the DM ingested to ada");
    assert!(dm.contains("as=carol@kde.org"), "{dm}");
    assert!(
        dm.contains("msgid=kde.org/"),
        "the realm mints its own: {dm}"
    );

    // §15 typing: ada's indicator becomes her puppet's typing EDU.
    deliver(
        &mut bridge,
        weft_proto::Event::Typing {
            channel: channel.parse().unwrap(),
            user: "ada@test.example".parse().unwrap(),
            state: weft_proto::TypingState::Start,
        },
        None,
        Some(ADA_ULID),
    )
    .await;
    assert!(
        calls.lock().unwrap().iter().any(|(what, q, body)| what
            .starts_with("PUT typing/!gen:kde.org")
            && q.contains(&format!("weft_{ADA_ULID}"))
            && body["typing"] == true),
        "typing was mirrored as her puppet"
    );

    // …and `stop` clears it rather than waiting for the TTL.
    deliver(
        &mut bridge,
        weft_proto::Event::Typing {
            channel: channel.parse().unwrap(),
            user: "ada@test.example".parse().unwrap(),
            state: weft_proto::TypingState::Stop,
        },
        None,
        Some(ADA_ULID),
    )
    .await;
    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .any(|(what, _, body)| what.starts_with("PUT typing/") && body["typing"] == false),
        "stop clears the indicator"
    );
}

#[tokio::test]
async fn a_weft_moderation_act_is_applied_on_matrix() {
    // Owner request 2026-08-11, the foreign half: weftd moderated a member of a
    // bridged namespace and relays the act here. A ban must cover every room of
    // the namespace **and the Space** — a restricted room authorizes entry by
    // Space membership, so leaving the Space out would leave the door open.
    let (mut bridge, mut lines, calls) = bridge_with(BTreeMap::new()).await;
    let (ns_id, channel, room_id) = projected_fixture(&mut bridge, &mut lines).await;
    let space_room = bridge.store.state.projections[&ns_id].space_room.clone();

    bridge
        .on_incoming(weft_appservice::Incoming::Command {
            label: None,
            as_user: None,
            as_ulid: None,
            command: weft_proto::Command::Ban {
                scope: format!("ns:{ns_id}"),
                member: "carol@kde.org".parse().unwrap(),
                reason: Some("raiding".to_string()),
            },
        })
        .await;
    {
        let recorded = calls.lock().unwrap();
        for room in [&room_id, &space_room] {
            assert!(
                recorded
                    .iter()
                    .any(|(what, _, body)| what == &format!("POST ban/{room}")
                        && body["user_id"] == "@carol:kde.org"
                        && body["reason"] == "raiding"),
                "banned in {room}: {recorded:?}"
            );
        }
    }

    // Unban lifts it in the same set.
    bridge
        .on_incoming(weft_appservice::Incoming::Command {
            label: None,
            as_user: None,
            as_ulid: None,
            command: weft_proto::Command::Unban {
                scope: format!("ns:{ns_id}"),
                member: "carol@kde.org".parse().unwrap(),
            },
        })
        .await;
    assert!(
        calls.lock().unwrap().iter().any(|(what, _, body)| what
            == &format!("POST unban/{space_room}")
            && body["user_id"] == "@carol:kde.org"),
        "the unban reaches the Space too"
    );

    // A kick names one channel, so it touches that room alone — matching WEFT,
    // where a kick is a force-part they may undo by rejoining.
    bridge
        .on_incoming(weft_appservice::Incoming::Command {
            label: None,
            as_user: None,
            as_ulid: None,
            command: weft_proto::Command::Kick {
                channel: channel.parse().unwrap(),
                member: "carol@kde.org".parse().unwrap(),
                reason: None,
            },
        })
        .await;
    {
        let recorded = calls.lock().unwrap();
        assert!(
            recorded
                .iter()
                .any(|(what, _, _)| what == &format!("POST kick/{room_id}")),
            "kicked from the channel's room: {recorded:?}"
        );
        assert!(
            !recorded
                .iter()
                .any(|(what, _, _)| what == &format!("POST kick/{space_room}")),
            "a kick is not a namespace act — the Space is untouched"
        );
    }

    // A **channel-scope** ban is a channel-scope ban: that room, not the Space.
    // Resolving rooms by the kind of act rather than the shape of the scope made
    // this case resolve to no rooms at all — the ban stood on the WEFT side while
    // Matrix never heard of it.
    bridge
        .on_incoming(weft_appservice::Incoming::Command {
            label: None,
            as_user: None,
            as_ulid: None,
            command: weft_proto::Command::Ban {
                scope: channel.clone(),
                member: "carol@kde.org".parse().unwrap(),
                reason: None,
            },
        })
        .await;
    {
        let recorded = calls.lock().unwrap();
        assert!(
            recorded
                .iter()
                .filter(|(what, _, _)| what == &format!("POST ban/{room_id}"))
                .count()
                >= 2,
            "the channel-scope ban reached that room too: {recorded:?}"
        );
        assert_eq!(
            recorded
                .iter()
                .filter(|(what, _, _)| what == &format!("POST ban/{space_room}"))
                .count(),
            1,
            "…and only the earlier namespace ban touched the Space"
        );
    }
}

#[tokio::test]
async fn unmuting_restores_the_default_level_but_never_demotes() {
    // A mute is a negative power level (below `events_default`, so the
    // homeserver refuses their messages). Lifting one must not be a blanket
    // write of 0: a moderator sitting at 50 would be silently demoted by every
    // unmute, and the WEFT side would never know it had happened.
    let (mut bridge, mut lines, calls) = bridge_with(BTreeMap::new()).await;
    let (ns_id, _channel, room_id) = projected_fixture(&mut bridge, &mut lines).await;
    let scope = format!("ns:{ns_id}");

    let mute = weft_appservice::Incoming::Command {
        label: None,
        as_user: None,
        as_ulid: None,
        command: weft_proto::Command::Mute {
            scope: scope.clone(),
            member: "carol@kde.org".parse().unwrap(),
            reason: None,
        },
    };
    bridge.on_incoming(mute).await;
    assert_eq!(
        bridge.store.state.room_levels[&room_id]["@carol:kde.org"], -1,
        "a mute writes a negative level"
    );

    bridge
        .on_incoming(weft_appservice::Incoming::Command {
            label: None,
            as_user: None,
            as_ulid: None,
            command: weft_proto::Command::Unmute {
                scope: scope.clone(),
                member: "carol@kde.org".parse().unwrap(),
            },
        })
        .await;
    assert!(
        !bridge.store.state.room_levels[&room_id].contains_key("@carol:kde.org"),
        "lifting it restores the default (0 = absent from the users map)"
    );

    // Now the case the guard exists for: a moderator at 50 who is not muted.
    bridge
        .on_incoming(weft_appservice::Incoming::Command {
            label: None,
            as_user: None,
            as_ulid: None,
            command: weft_proto::Command::Grant {
                subject: "dave@kde.org".to_string(),
                scope: scope.clone(),
                caps: "ban,kick".to_string(),
                expiry: None,
            },
        })
        .await;
    assert_eq!(
        bridge.store.state.room_levels[&room_id]["@dave:kde.org"],
        50
    );
    let writes_before = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(what, _, _)| what.starts_with("PUT state/"))
        .count();

    bridge
        .on_incoming(weft_appservice::Incoming::Command {
            label: None,
            as_user: None,
            as_ulid: None,
            command: weft_proto::Command::Unmute {
                scope,
                member: "dave@kde.org".parse().unwrap(),
            },
        })
        .await;
    assert_eq!(
        bridge.store.state.room_levels[&room_id]["@dave:kde.org"], 50,
        "unmuting someone who was not muted leaves their level alone"
    );
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(what, _, _)| what.starts_with("PUT state/"))
            .count(),
        writes_before,
        "…and writes nothing at all"
    );
}

#[tokio::test]
async fn recovery_restores_the_matrix_members_of_a_projected_room() {
    // Reported 2026-08-11: Matrix members of a projected server did not appear
    // in the WEFT roster. Only *transitions* are stated (`member_joined` returns
    // an action for the first room only), and a projection's member set was
    // memory-only — so anyone who joined before the last bridge restart was
    // never mentioned to weftd again. No membership row means no roster entry
    // and, because weftd drops presence for a member it does not know, no
    // presence either. Nothing short of leaving and rejoining fixed it.
    let ns_id = ident::stable_ulid("!space:test.example");

    let mut rooms = BTreeMap::new();
    rooms.insert(
        "!space:test.example".to_string(),
        vec![json!({ "type": "dev.weft.space", "state_key": "",
                     "content": { "kind": "projected", "ns": ns_id } })],
    );
    rooms.insert(
        "!gen:test.example".to_string(),
        vec![
            json!({ "type": "m.space.parent", "state_key": "!space:test.example",
                    "content": { "via": ["test.example"], "canonical": true } }),
            // Two Matrix members, one who left, and one of our own puppets —
            // which is a relay of a local user, never a member in its own right.
            json!({ "type": "m.room.member", "state_key": "@carol:kde.org",
                    "content": { "membership": "join" } }),
            json!({ "type": "m.room.member", "state_key": "@dave:kde.org",
                    "content": { "membership": "join" } }),
            json!({ "type": "m.room.member", "state_key": "@eve:kde.org",
                    "content": { "membership": "leave" } }),
            json!({ "type": "m.room.member", "state_key": format!("@weft_{ADA_ULID}:test.example"),
                    "content": { "membership": "join", "displayname": "ada" } }),
        ],
    );
    rooms.insert(
        "__joined__".to_string(),
        vec![json!("!space:test.example"), json!("!gen:test.example")],
    );

    let (mut bridge, mut lines, _calls) = bridge_with(rooms).await;
    let found = bridge.recover().await.expect("recovery ran");

    assert_eq!(found.members, 2, "the two joined Matrix members: {found}");
    let stated = drain(&mut lines);
    for who in ["carol@kde.org", "dave@kde.org"] {
        assert!(
            stated
                .iter()
                .any(|l| l.contains("NS-MEMBER") && l.contains(who) && l.contains("join")),
            "{who} was stated to weftd: {stated:?}"
        );
    }
    assert!(
        !stated.iter().any(|l| l.contains("eve@kde.org")),
        "someone who left is not a member: {stated:?}"
    );
    assert!(
        !stated
            .iter()
            .any(|l| l.contains("NS-MEMBER") && l.contains("ada")),
        "a puppet is our own user's relay, not a foreign member: {stated:?}"
    );
    assert_eq!(
        bridge.store.state.projections[&ns_id].member_rooms.len(),
        2,
        "and the room-set is restored, so the next leave is recognised as one"
    );

    // Idempotent — the roster is already stated, so a second pass announces
    // nothing new.
    let again = bridge.recover().await.expect("recovery repeats");
    assert_eq!(
        again.members, 0,
        "a member weftd already holds is not re-announced: {again}"
    );
}

#[tokio::test]
async fn state_is_rebuilt_from_matrix_after_a_database_loss() {
    // The recovery story (owner requirement 2026-08-06): the daemon's database
    // is a cache. Structure ids are deterministic and Matrix holds the markers,
    // so a wiped store rebuilds itself — and the one thing Matrix does not know
    // (the bridging bans) lives in the bot's account data, outside our database
    // precisely so it survives losing it.
    let ns_id = ident::stable_ulid("!space:kde.org");
    let chan_id = ident::stable_ulid("!gen:kde.org");
    let banned_ns = "01bx5zzkbkactav9wevgemmvrz";

    // A homeserver that already carries a bridged world: a marked consumed
    // Space, a channel room under it, a marked DM, and a puppet.
    let mut rooms = BTreeMap::new();
    rooms.insert(
        "!space:kde.org".to_string(),
        vec![
            json!({ "type": "dev.weft.space", "state_key": "",
                    "content": { "kind": "consumed", "ns": ns_id,
                                 "uri": "matrix://kde.org/community" } }),
            json!({ "type": "m.space.child", "state_key": "!gen:kde.org",
                    "content": { "via": ["kde.org"] } }),
        ],
    );
    rooms.insert(
        "!gen:kde.org".to_string(),
        vec![
            json!({ "type": "m.space.parent", "state_key": "!space:kde.org",
                    "content": { "via": ["kde.org"], "canonical": true } }),
            json!({ "type": "m.room.member", "state_key": format!("@weft_{ADA_ULID}:test.example"),
                    "content": { "membership": "join", "displayname": "ada" } }),
            json!({ "type": "m.room.power_levels", "state_key": "",
                    "content": { "users": { "@mod:kde.org": 50 } } }),
        ],
    );
    rooms.insert(
        "!dm:test.example".to_string(),
        vec![json!({ "type": "dev.weft.dm", "state_key": "",
                     "content": { "account": "ada", "mxid": "@carol:kde.org" } })],
    );
    rooms.insert(
        "__joined__".to_string(),
        vec![
            json!("!space:kde.org"),
            json!("!gen:kde.org"),
            json!("!dm:test.example"),
        ],
    );
    rooms.insert(
        "__account_data__".to_string(),
        vec![json!({ "banned": [banned_ns] })],
    );

    let (mut bridge, _lines, _calls) = bridge_with(rooms).await;
    assert!(
        bridge.store.state.spaces.is_empty(),
        "starting from nothing"
    );

    let found = bridge.recover().await.expect("recovery ran");

    // The consumed Space and its room, with the ids re-derived — not guessed.
    assert_eq!((found.spaces, found.rooms), (1, 1), "{found}");
    let (room, space) = bridge
        .store
        .state
        .channel_of_room("!gen:kde.org")
        .expect("the room was re-attached");
    assert_eq!(space.ns_id, ns_id);
    assert_eq!(
        room.channel,
        format!("#{ns_id}/{chan_id}"),
        "the same channel name as before the loss — deterministic ids are the point"
    );

    // The DM, the puppet (ULID from its localpart, name from its display name),
    // and the power-level baseline.
    assert_eq!(found.dms, 1, "{found}");
    assert_eq!(
        bridge.store.state.dm_of_room("!dm:test.example"),
        Some(("ada", "@carol:kde.org"))
    );
    let (ulid, user) = bridge
        .store
        .state
        .users
        .by_account("ada")
        .expect("the puppet was recovered");
    assert_eq!(ulid, ADA_ULID);
    assert_eq!(user.localpart, format!("weft_{ADA_ULID}"));
    assert_eq!(
        bridge.store.state.room_levels["!gen:kde.org"]["@mod:kde.org"], 50,
        "the live map IS the baseline — without it every level re-translates"
    );

    // The ban survived our database because it never lived there.
    assert_eq!(found.bans, 1, "{found}");
    assert!(bridge.store.state.bans.is_banned(banned_ns));

    // Idempotent: recovery on a healthy daemon changes nothing.
    let again = bridge.recover().await.expect("recovery repeats");
    assert_eq!(again.spaces, 1);
    assert_eq!(bridge.store.state.spaces.len(), 1, "no duplicate space");
    assert_eq!(
        bridge.store.state.spaces["matrix://kde.org/community"]
            .rooms
            .len(),
        1,
        "no duplicate room"
    );
}

#[tokio::test]
async fn the_console_answers_only_configured_admins() {
    let (mut bridge, _lines, calls) = bridge_with(BTreeMap::new()).await;

    let say = |sender: &str, body: &str| {
        json!({
            "type": "m.room.message",
            "room_id": "!ops:test.example",
            "event_id": format!("$c{}", ulid::Ulid::new()),
            "sender": sender,
            "origin_server_ts": 1_722_000_000_000u64,
            "content": { "msgtype": "m.text", "body": body },
        })
    };

    // A stranger's command is not a command — no reply, no action.
    bridge
        .on_matrix_event(say("@nobody:kde.org", "!weft status"))
        .await;
    assert!(
        !calls
            .lock()
            .unwrap()
            .iter()
            .any(|(what, _, _)| what.contains("m.room.message")),
        "an unauthorized console line is ignored entirely"
    );

    // The configured admin gets an answer.
    bridge
        .on_matrix_event(say("@boss:test.example", "!weft status"))
        .await;
    let replied = calls.lock().unwrap().iter().any(|(what, _, body)| {
        what.contains("m.room.message")
            && body["msgtype"] == "m.notice"
            && body["body"]
                .as_str()
                .unwrap_or_default()
                .contains("consumed:")
    });
    assert!(replied, "the admin's status query was answered");

    // A malformed command explains itself rather than failing silently.
    bridge
        .on_matrix_event(say("@boss:test.example", "!weft attach-dm ada"))
        .await;
    let usage = calls.lock().unwrap().iter().any(|(_, _, body)| {
        body["body"]
            .as_str()
            .unwrap_or_default()
            .contains("usage: !weft attach-dm")
    });
    assert!(usage, "the usage line came back");

    // And an attach re-points state by hand, for what recovery cannot infer.
    bridge
        .on_matrix_event(say(
            "@boss:test.example",
            &format!("!weft attach-puppet @weft_{ADA_ULID}:test.example {ADA_ULID} ada"),
        ))
        .await;
    assert_eq!(
        bridge
            .store
            .state
            .users
            .by_account("ada")
            .map(|(ulid, _)| ulid.to_string()),
        Some(ADA_ULID.to_string())
    );
}

/// A Space that was EMPTY when it was consumed must still grow channels when rooms
/// are added to it later. The regression this pins: `m.space.child` arrives in the
/// *space* room, which is never a mapped channel, so the ingest path dropped it as
/// noise — and the provisioning comment claiming "rooms added later arrive by
/// re-assertion" described an intention nobody had implemented. An empty Space
/// therefore stayed empty forever.
#[tokio::test]
async fn a_room_added_to_a_consumed_space_later_becomes_a_channel() {
    // The space has no children at provision time.
    let mut state = BTreeMap::new();
    state.insert(
        "!space:kde.org".to_string(),
        vec![json!({ "type": "m.room.name", "state_key": "",
                     "content": { "name": "Community" } })],
    );
    // The room exists on the homeserver, it just isn't in the space yet.
    state.insert(
        "!later:kde.org".to_string(),
        vec![json!({ "type": "m.room.name", "state_key": "",
                     "content": { "name": "Added Later" } })],
    );

    let (mut bridge, mut lines, _calls) = bridge_with(state).await;
    assert!(bridge
        .provision("matrix://kde.org/community")
        .await
        .unwrap());

    let after_provision = drain(&mut lines);
    assert!(
        !after_provision.iter().any(|l| l.contains("CHANNEL-LAYOUT")),
        "an empty space asserts no channels: {after_provision:?}"
    );

    // Someone adds a room to the space.
    bridge
        .on_matrix_event(json!({
            "type": "m.space.child",
            "room_id": "!space:kde.org",
            "state_key": "!later:kde.org",
            "sender": "@admin:kde.org",
            "content": { "via": ["kde.org"], "order": "a" },
        }))
        .await;

    let sent = drain(&mut lines);
    let chan_id = ident::stable_ulid("!later:kde.org");
    let channel = sent
        .iter()
        .find(|l| l.contains("CHANNEL-LAYOUT"))
        .unwrap_or_else(|| panic!("the added room becomes a channel: {sent:?}"));
    assert!(channel.contains(&chan_id), "{channel}");

    // And removing it again stops tracking it, so a re-add provisions cleanly
    // rather than being mistaken for an already-bridged room.
    bridge
        .on_matrix_event(json!({
            "type": "m.space.child",
            "room_id": "!space:kde.org",
            "state_key": "!later:kde.org",
            "sender": "@admin:kde.org",
            "content": {},
        }))
        .await;
    bridge
        .on_matrix_event(json!({
            "type": "m.space.child",
            "room_id": "!space:kde.org",
            "state_key": "!later:kde.org",
            "sender": "@admin:kde.org",
            "content": { "via": ["kde.org"], "order": "a" },
        }))
        .await;

    let readded = drain(&mut lines);
    assert!(
        readded.iter().any(|l| l.contains("CHANNEL-LAYOUT")),
        "a re-added room is provisioned again: {readded:?}"
    );
}

#[tokio::test]
async fn re_projecting_republishes_an_existing_space() {
    // Every projected Space is public by design (owner directive): projection only
    // happens for a `public` namespace, so there is nothing visibility was hiding,
    // and a Space nobody can find defeats the point.
    //
    // `createRoom`'s `visibility` only covers a *fresh* Space, so a Space created
    // before that flag existed — or created unlisted — would stay invisible
    // forever. The idempotent re-projection path must repair it, which is what
    // makes an already-deployed Space heal on the next reconnect instead of
    // needing to be published by hand.
    let (mut bridge, mut lines, calls) = bridge_with(BTreeMap::new()).await;
    let ns_id = ulid::Ulid::new().to_string().to_lowercase();

    let meta = |ns: &str| weft_proto::Event::NsMeta {
        id: ns.parse().unwrap(),
        vanity: "gaming".parse().unwrap(),
        visibility: weft_proto::Visibility::Public,
        owner: Some("ada".into()),
        title: Some("The Lounge".into()),
        description: None,
        icon: None,
        recovery_set: false,
        recovery_pending: None,
        categories: Vec::new(),
        federation: false,
        welcome: None,
        origin: None,
        provider_online: None,
        authority: None,
        settings_disabled: Vec::new(),
        bridges: vec!["matrix".parse().unwrap()],
    };

    deliver(&mut bridge, meta(&ns_id), None, None).await;
    drain(&mut lines);

    // Second push: the Space exists, so this takes the refresh branch — no
    // `createRoom`, and therefore no `visibility` to ride along.
    calls.lock().unwrap().clear();
    deliver(&mut bridge, meta(&ns_id), None, None).await;

    let recorded = calls.lock().unwrap();
    assert!(
        !recorded
            .iter()
            .any(|(what, _, _)| what == "POST createRoom"),
        "re-projection must not create a second Space: {recorded:?}"
    );
    assert!(
        recorded
            .iter()
            .any(|(what, _, body)| what.starts_with("PUT list/") && body["visibility"] == "public"),
        "re-projection must (re)publish the Space in the directory: {recorded:?}"
    );
}

#[tokio::test]
async fn making_a_channel_permanent_projects_it_later() {
    // matrix.md §3: "anything → permanent creates it". A channel defaults to
    // `retained`, so a freshly projected namespace has a Space, its categories, and
    // no rooms — and the fix for that is to make a channel permanent.
    //
    // That arrives as a later POLICY with no fresh CHANNEL-LAYOUT beside it, so
    // consuming the layout on first use left nothing to pair with and the room was
    // never created. The layout is retained instead.
    let (mut bridge, mut lines, calls) = bridge_with(BTreeMap::new()).await;
    let ns_id = ulid::Ulid::new().to_string().to_lowercase();
    let chan_id = ulid::Ulid::new().to_string().to_lowercase();
    let channel = format!("#{ns_id}/{chan_id}");

    deliver(
        &mut bridge,
        weft_proto::Event::NsMeta {
            id: ns_id.parse().unwrap(),
            vanity: "gaming".parse().unwrap(),
            visibility: weft_proto::Visibility::Public,
            owner: Some("ada".into()),
            title: None,
            description: None,
            icon: None,
            recovery_set: false,
            recovery_pending: None,
            categories: Vec::new(),
            federation: false,
            welcome: None,
            origin: None,
            provider_online: None,
            authority: None,
            settings_disabled: Vec::new(),
            bridges: vec!["matrix".parse().unwrap()],
        },
        None,
        None,
    )
    .await;
    deliver(
        &mut bridge,
        weft_proto::Event::ChannelLayout {
            channel: channel.parse().unwrap(),
            category: None,
            position: 0,
            kind: weft_proto::ChannelKind::Text,
            vanity: "general".parse().unwrap(),
            origin: None,
        },
        None,
        None,
    )
    .await;
    // Retained: by rule, no room.
    deliver(
        &mut bridge,
        weft_proto::Event::Policy {
            channel: channel.parse().unwrap(),
            policy: "retained:90d".parse().unwrap(),
        },
        None,
        None,
    )
    .await;
    drain(&mut lines);
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(what, _, body)| what == "POST createRoom" && body["name"] == "general")
            .count(),
        0,
        "a retained channel must not project"
    );

    // The owner sets it permanent. Only a POLICY arrives — no second layout.
    calls.lock().unwrap().clear();
    deliver(
        &mut bridge,
        weft_proto::Event::Policy {
            channel: channel.parse().unwrap(),
            policy: weft_proto::RetentionPolicy::Permanent,
        },
        None,
        None,
    )
    .await;

    {
        let recorded = calls.lock().unwrap();
        assert!(
            recorded
                .iter()
                .any(|(what, _, body)| what == "POST createRoom" && body["name"] == "general"),
            "making it permanent must project the room: {recorded:?}"
        );
    }

    // …and back off `permanent` tombstones it (matrix.md §3): the promise that
    // justified the room is gone, so the room must stop rather than linger with a
    // retention rule Matrix cannot keep.
    calls.lock().unwrap().clear();
    deliver(
        &mut bridge,
        weft_proto::Event::Policy {
            channel: channel.parse().unwrap(),
            policy: "retained:30d".parse().unwrap(),
        },
        None,
        None,
    )
    .await;

    let recorded = calls.lock().unwrap();
    assert!(
        recorded
            .iter()
            .any(|(what, _, _)| what.contains("m.room.tombstone")),
        "losing `permanent` must tombstone the room: {recorded:?}"
    );
    // Successor-less on purpose — nothing should carry the conversation forward.
    assert!(
        recorded
            .iter()
            .any(|(what, _, body)| what.contains("m.room.tombstone")
                && body["replacement_room"] == ""),
        "the tombstone must name no successor: {recorded:?}"
    );
}

#[tokio::test]
async fn provisioning_seeds_authority_from_existing_power_levels() {
    // `on_power_levels_event` translates *diffs*, so a level someone already holds
    // produces nothing. A Matrix admin who was an admin before the bridge ever saw
    // the room therefore had no WEFT authority — and since a replica's
    // owner-shortcut is gated off (the realm governs it), they could not administer
    // their own namespace from WEFT at all. The fix was to demote and re-promote
    // yourself in Element to manufacture a diff, which is not a fix.
    let mut rooms = kde_space();
    rooms.get_mut("!space:kde.org").unwrap().push(json!({
        "type": "m.room.power_levels", "state_key": "",
        "content": { "users": {
            "@carol:kde.org": 100,   // admin
            "@dave:kde.org": 50,     // moderator
            "@erin:kde.org": 0,      // nothing to grant
            "@weftbot:test.example": 100, // ours — needs no WEFT authority
        } }
    }));
    let (mut bridge, mut lines, _calls) = bridge_with(rooms).await;

    bridge
        .provision("matrix://kde.org/community")
        .await
        .unwrap();

    let sent = drain(&mut lines);
    let grants: Vec<&String> = sent.iter().filter(|l| l.contains("GRANT")).collect();

    assert!(
        grants.iter().any(|l| l.contains("carol")),
        "an existing Matrix admin must be granted: {grants:?}"
    );
    assert!(
        grants.iter().any(|l| l.contains("dave")),
        "an existing moderator too: {grants:?}"
    );
    assert!(
        !grants.iter().any(|l| l.contains("erin")),
        "a default-level user grants nothing: {grants:?}"
    );
    assert!(
        !grants.iter().any(|l| l.contains("weftbot")),
        "our own bot needs no WEFT authority: {grants:?}"
    );
}

#[tokio::test]
async fn replies_map_to_each_sides_relation() {
    // §9.3 ⇄ Matrix rich replies. The two protocols point at the same root by
    // different names, so the link table is the whole translation — and Matrix's
    // quoted fallback must not survive into a WEFT body, which renders the root
    // itself and would otherwise quote it twice.
    let (mut bridge, mut lines, calls) = bridge_with(kde_space()).await;
    bridge
        .provision("matrix://kde.org/community")
        .await
        .unwrap();
    drain(&mut lines);
    let (channel, ns_id) = {
        let (chan, space) = bridge.store.state.channel_of_room("!gen:kde.org").unwrap();

        (chan.channel.clone(), space.ns_id.clone())
    };

    // A root from Matrix, so both sides have an id for it.
    bridge
        .on_matrix_event(json!({
            "type": "m.room.message",
            "room_id": "!gen:kde.org",
            "event_id": "$root",
            "sender": "@carol:kde.org",
            "origin_server_ts": 1_722_000_000_000u64,
            "content": { "msgtype": "m.text", "body": "the original" },
        }))
        .await;
    let sent = drain(&mut lines);
    let root_msgid = sent
        .iter()
        .find_map(|l| l.split([';', ' ']).find_map(|t| t.strip_prefix("msgid=")))
        .expect("the root's minted msgid")
        .to_string();

    // Matrix → WEFT: the relation becomes `reply-to=`, and the fallback quote is
    // gone from the body.
    bridge
        .on_matrix_event(json!({
            "type": "m.room.message",
            "room_id": "!gen:kde.org",
            "event_id": "$answer",
            "sender": "@carol:kde.org",
            "origin_server_ts": 1_722_000_000_001u64,
            "content": {
                "msgtype": "m.text",
                "body": "> <@carol:kde.org> the original\n\nmy answer",
                "m.relates_to": { "m.in_reply_to": { "event_id": "$root" } },
            },
        }))
        .await;
    let sent = drain(&mut lines);
    let reply = sent.iter().find(|l| l.contains("MSG")).expect("MSG");
    // Case-insensitively: a msgid reaches us in both the wire (lowercase) and
    // canonical (uppercase ULID) spellings, and the link map keys by one of them.
    assert!(
        reply
            .to_lowercase()
            .contains(&format!("reply-to={}", root_msgid.to_lowercase())),
        "the WEFT reply names the root: {reply}"
    );
    assert!(
        reply.ends_with(":my answer"),
        "the quoted fallback must be stripped: {reply}"
    );

    // WEFT → Matrix: a local member's reply to that same root goes out with the
    // Matrix relation pointing at the event the root came from.
    join_ada(&mut bridge, &ns_id).await;
    drain(&mut lines);
    calls.lock().unwrap().clear();

    let mut line = weft_proto::Request::new(weft_proto::Command::Msg {
        target: weft_proto::Target::Channel(channel.parse().unwrap()),
        body: Some("answering from weft".into()),
        meta: weft_proto::MsgMeta {
            reply_to: Some(root_msgid.parse().unwrap()),
            ..weft_proto::MsgMeta::default()
        },
    })
    .to_line()
    .unwrap();
    line.tags.insert("as".into(), "ada@test.example".into());
    line.tags.insert("ulid".into(), ADA_ULID.into());
    line.tags.insert("label".into(), "B-matrix-1".into());
    bridge
        .on_incoming(weft_appservice::Incoming::Command {
            label: Some("B-matrix-1".into()),
            as_user: Some("ada@test.example".into()),
            as_ulid: Some(ADA_ULID.into()),
            command: weft_proto::Request::from_line(&line).unwrap().command,
        })
        .await;

    let recorded = calls.lock().unwrap().clone();
    let (_, _, body) = recorded
        .iter()
        .find(|(what, _, _)| what == "PUT send/!gen:kde.org/m.room.message")
        .expect("the reply reached Matrix");
    assert_eq!(body["m.relates_to"]["m.in_reply_to"]["event_id"], "$root");
    assert_eq!(
        body["body"], "answering from weft",
        "no fallback is invented on the way out"
    );
}

#[tokio::test]
async fn presence_mirrors_in_both_directions() {
    // §6.1 owner directive 2026-08-09. Matrix has three states and WEFT four, so
    // the mapping is the substance: `unavailable` is "here but not attending" =
    // `away`, and `dnd` folds onto it going out. `invisible` must never leave.
    let (mut bridge, mut lines, calls) = bridge_with(kde_space()).await;
    bridge
        .provision("matrix://kde.org/community")
        .await
        .unwrap();
    drain(&mut lines);
    let ns_id = bridge
        .store
        .state
        .channel_of_room("!gen:kde.org")
        .unwrap()
        .1
        .ns_id
        .clone();

    // Matrix → WEFT: a remote user's status is replayed attributed to them, with
    // no channel — weftd fans it out to what they share with us.
    bridge
        .on_matrix_ephemeral(json!({
            "type": "m.presence",
            "sender": "@carol:kde.org",
            "content": { "presence": "unavailable" },
        }))
        .await;
    let sent = drain(&mut lines);
    let presence = sent
        .iter()
        .find(|l| l.contains("PRESENCE"))
        .expect("PRESENCE relayed to weftd");
    assert!(presence.contains("as=carol@kde.org"), "{presence}");
    assert!(
        presence.contains("away"),
        "unavailable maps to away: {presence}"
    );

    // Our own puppet's presence is not fed back — it is a reflection of a WEFT
    // account's status, and ingesting it would fight the source.
    bridge
        .on_matrix_ephemeral(json!({
            "type": "m.presence",
            "sender": "@weft_ada:test.example",
            "content": { "presence": "online" },
        }))
        .await;
    assert!(
        drain(&mut lines).is_empty(),
        "a puppet's presence looped back"
    );

    // An unknown state is not guessed at.
    bridge
        .on_matrix_ephemeral(json!({
            "type": "m.presence",
            "sender": "@carol:kde.org",
            "content": { "presence": "vibing" },
        }))
        .await;
    assert!(
        drain(&mut lines).is_empty(),
        "an unknown state was invented"
    );

    // WEFT → Matrix: ada's status is set on her puppet.
    join_ada(&mut bridge, &ns_id).await;
    drain(&mut lines);
    calls.lock().unwrap().clear();

    for (status, expected) in [
        (weft_proto::PresenceStatus::Dnd, "unavailable"),
        (weft_proto::PresenceStatus::Online, "online"),
    ] {
        bridge
            .on_incoming(weft_appservice::Incoming::Event {
                label: None,
                actor_ulid: Some(ADA_ULID.into()),
                event: weft_proto::Event::Presence {
                    user: "ada@test.example".parse().unwrap(),
                    status,
                },
            })
            .await;

        let recorded = calls.lock().unwrap().clone();
        let (_, _, body) = recorded
            .iter()
            .find(|(what, _, _)| what == &format!("PUT presence/@weft_{ADA_ULID}:test.example"))
            .unwrap_or_else(|| panic!("presence set for {status}: {recorded:?}"));
        assert_eq!(body["presence"], expected);
        calls.lock().unwrap().clear();
    }

    // Invisible is never mirrored: weftd does not announce it, and mapping it to
    // anything here would defeat the point of it.
    bridge
        .on_incoming(weft_appservice::Incoming::Event {
            label: None,
            actor_ulid: Some(ADA_ULID.into()),
            event: weft_proto::Event::Presence {
                user: "ada@test.example".parse().unwrap(),
                status: weft_proto::PresenceStatus::Invisible,
            },
        })
        .await;
    let recorded = calls.lock().unwrap().clone();
    assert!(
        !recorded
            .iter()
            .any(|(what, _, _)| what.contains("presence")),
        "invisible reached Matrix: {recorded:?}"
    );
}

#[tokio::test]
async fn matrix_typing_becomes_per_user_start_and_stop() {
    // §15 inbound. The EDU is a *set* per room; WEFT's TYPING is per-user
    // start/stop. So the transitions exist only as the difference against the
    // previous set — and Matrix's own timeout is what eventually empties it, which
    // is why "stopped typing" arrives as a shorter list rather than an event.
    let (mut bridge, mut lines, _calls) = bridge_with(kde_space()).await;
    bridge
        .provision("matrix://kde.org/community")
        .await
        .unwrap();
    drain(&mut lines);
    let channel = bridge
        .store
        .state
        .channel_of_room("!gen:kde.org")
        .unwrap()
        .0
        .channel
        .clone();

    let typing = |users: Vec<&str>| {
        json!({
            "type": "m.typing",
            "room_id": "!gen:kde.org",
            "content": { "user_ids": users },
        })
    };

    // carol starts. Our own puppet in the same set is skipped: its typing is the
    // reflection of a WEFT member's, and feeding it back would fight the source.
    bridge
        .on_matrix_ephemeral(typing(vec!["@carol:kde.org", "@weft_ada:test.example"]))
        .await;
    let sent = drain(&mut lines);
    assert_eq!(
        sent.iter().filter(|l| l.contains("TYPING")).count(),
        1,
        "one start, and not for our puppet: {sent:?}"
    );
    let start = sent.iter().find(|l| l.contains("TYPING")).unwrap();
    assert!(start.contains("as=carol@kde.org"), "{start}");
    assert!(
        start.contains(&format!("TYPING {channel} start")),
        "{start}"
    );

    // dave joins the set: carol is unchanged (no repeat), dave starts.
    bridge
        .on_matrix_ephemeral(typing(vec!["@carol:kde.org", "@dave:kde.org"]))
        .await;
    let sent = drain(&mut lines);
    let typings: Vec<_> = sent.iter().filter(|l| l.contains("TYPING")).collect();
    assert_eq!(typings.len(), 1, "only the new typist: {typings:?}");
    assert!(typings[0].contains("as=dave@kde.org"), "{:?}", typings[0]);

    // The set empties: both stop.
    bridge.on_matrix_ephemeral(typing(vec![])).await;
    let sent = drain(&mut lines);
    let stops: Vec<_> = sent
        .iter()
        .filter(|l| l.contains(&format!("TYPING {channel} stop")))
        .collect();
    assert_eq!(stops.len(), 2, "both stopped: {sent:?}");
    assert!(stops.iter().any(|l| l.contains("carol@kde.org")));
    assert!(stops.iter().any(|l| l.contains("dave@kde.org")));

    // …and the room is forgotten rather than kept as an empty set.
    assert!(
        !bridge.typing_now.contains_key("!gen:kde.org"),
        "an empty set was retained"
    );

    // A room we do not bridge is ignored.
    bridge
        .on_matrix_ephemeral(json!({
            "type": "m.typing",
            "room_id": "!elsewhere:kde.org",
            "content": { "user_ids": ["@carol:kde.org"] },
        }))
        .await;
    assert!(drain(&mut lines).is_empty(), "an unbridged room leaked");
}

#[tokio::test]
async fn a_matrix_user_can_open_the_dm() {
    // The half that was missing: only weftd relaying a local user's DM created the
    // room, so a Matrix user starting the conversation invited a puppet that never
    // joined — and every message in that room then fell through as an unmapped room.
    let (mut bridge, mut lines, calls) = bridge_with(kde_space()).await;
    bridge
        .provision("matrix://kde.org/community")
        .await
        .unwrap();
    let ns_id = ident::stable_ulid("!space:kde.org");
    join_ada(&mut bridge, &ns_id).await;
    drain(&mut lines);
    calls.lock().unwrap().clear();

    let puppet = format!("@weft_{ADA_ULID}:test.example");
    let invite = |direct: bool| {
        json!({
            "type": "m.room.member",
            "room_id": "!dm:kde.org",
            "event_id": "$inv",
            "sender": "@carol:kde.org",
            "state_key": puppet,
            "origin_server_ts": 1_722_000_000_000u64,
            "content": { "membership": "invite", "is_direct": direct },
        })
    };

    // A non-direct invite is ignored: a puppet must not be joinable into arbitrary
    // rooms by anyone who knows its MXID.
    bridge.on_matrix_event(invite(false)).await;
    assert!(
        !calls
            .lock()
            .unwrap()
            .iter()
            .any(|(what, _, _)| what.contains("join")),
        "a plain invite was accepted"
    );

    // A direct one is accepted as the puppet, and the pairing is remembered.
    bridge.on_matrix_event(invite(true)).await;
    let recorded = calls.lock().unwrap().clone();
    assert!(
        recorded
            .iter()
            .any(|(what, query, _)| what == "POST join/!dm:kde.org"
                && query
                    .to_lowercase()
                    .contains(&format!("weft_{ADA_ULID}").to_lowercase())),
        "joined as the puppet: {recorded:?}"
    );
    assert_eq!(
        bridge.store.state.dm_of_room("!dm:kde.org"),
        Some(("ada", "@carol:kde.org")),
        "the DM pairing is remembered"
    );

    // …so carol's first message ingests as an ordinary WEFT DM.
    bridge
        .on_matrix_event(json!({
            "type": "m.room.message",
            "room_id": "!dm:kde.org",
            "event_id": "$dm1",
            "sender": "@carol:kde.org",
            "origin_server_ts": 1_722_000_000_001u64,
            "content": { "msgtype": "m.text", "body": "hi ada" },
        }))
        .await;
    let sent = drain(&mut lines);
    let dm = sent
        .iter()
        .find(|l| l.contains("MSG") && l.contains("hi ada"))
        .unwrap_or_else(|| panic!("the DM did not ingest: {sent:?}"));
    assert!(dm.contains("as=carol@kde.org"), "{dm}");
    assert!(dm.contains("@ada"), "addressed to ada: {dm}");
}

#[tokio::test]
async fn the_dm_opening_message_is_fetched_back_on_join() {
    // The message that starts a DM is the one most likely to be lost: the client
    // creates the room, invites the puppet and sends it in one burst, so it is
    // written before we are a joined member — and until we are, the homeserver has
    // no reason to push us that room at all. No join is ever fast enough to win
    // that race, so the opening line has to be fetched back rather than waited for.
    const INVITED_AT: u64 = 1_722_000_000_000;
    let mut rooms = kde_space();
    // Newest-first, as Matrix pages.
    rooms.insert(
        "__messages__".to_string(),
        vec![
            json!({ "type": "m.room.message", "event_id": "$dm1", "sender": "@carol:kde.org",
                    "origin_server_ts": INVITED_AT + 1, "content": { "body": "hi ada" } }),
            // Older than the invite: a re-invite into a long-lived room must not
            // replay its scrollback as brand-new DMs.
            json!({ "type": "m.room.message", "event_id": "$old", "sender": "@carol:kde.org",
                    "origin_server_ts": INVITED_AT - 1_000, "content": { "body": "old business" } }),
        ],
    );

    let (mut bridge, mut lines, calls) = bridge_with(rooms).await;
    bridge
        .provision("matrix://kde.org/community")
        .await
        .unwrap();
    let ns_id = ident::stable_ulid("!space:kde.org");
    join_ada(&mut bridge, &ns_id).await;
    drain(&mut lines);
    calls.lock().unwrap().clear();

    bridge
        .on_matrix_event(json!({
            "type": "m.room.member",
            "room_id": "!dm:kde.org",
            "event_id": "$inv",
            "sender": "@carol:kde.org",
            "state_key": format!("@weft_{ADA_ULID}:test.example"),
            "origin_server_ts": INVITED_AT,
            "content": { "membership": "invite", "is_direct": true },
        }))
        .await;

    let sent = drain(&mut lines);
    let dm = sent
        .iter()
        .find(|l| l.contains("MSG") && l.contains("hi ada"))
        .unwrap_or_else(|| panic!("the opening message was not fetched back: {sent:?}"));
    assert!(dm.contains("as=carol@kde.org"), "{dm}");
    assert!(dm.contains("@ada"), "addressed to ada: {dm}");
    assert!(
        !sent.iter().any(|l| l.contains("old business")),
        "scrollback from before the invite was replayed: {sent:?}"
    );

    // The same message pushed live afterwards is already ours: the msgid derives
    // from the event id, so the catch-up cannot deliver it twice.
    bridge
        .on_matrix_event(json!({
            "type": "m.room.message",
            "room_id": "!dm:kde.org",
            "event_id": "$dm1",
            "sender": "@carol:kde.org",
            "origin_server_ts": INVITED_AT + 1,
            "content": { "msgtype": "m.text", "body": "hi ada" },
        }))
        .await;
    assert!(
        drain(&mut lines).is_empty(),
        "the opening message was delivered twice"
    );
}

#[tokio::test]
async fn a_post_we_cannot_place_is_reported_on_its_label() {
    // The other half of the label-keyed UNDELIVERED: weftd minted nothing for a
    // relayed post, so if we cannot place it the *only* honest answer is to say so
    // on the label it arrived under. Silence used to leave the author's client
    // waiting out its own send deadline with nothing in this log either.
    let (mut bridge, mut lines, _calls) = bridge_with(kde_space()).await;
    bridge
        .provision("matrix://kde.org/community")
        .await
        .unwrap();
    let ns_id = ident::stable_ulid("!space:kde.org");
    join_ada(&mut bridge, &ns_id).await;
    drain(&mut lines);

    // A channel we hold no room for — the reported symptom.
    bridge
        .on_incoming(weft_appservice::Incoming::Command {
            label: Some("B-matrix-1".into()),
            as_user: Some("ada@test.example".into()),
            as_ulid: Some(ADA_ULID.into()),
            command: weft_proto::Command::Msg {
                target: weft_proto::Target::Channel(
                    "#01hzzzzzzzzzzzzzzzzzzzzzzz/01hyyyyyyyyyyyyyyyyyyyyyyy"
                        .parse()
                        .unwrap(),
                ),
                body: Some("into the void".into()),
                meta: weft_proto::MsgMeta::default(),
            },
        })
        .await;

    let sent = drain(&mut lines);
    let report = sent
        .iter()
        .find(|l| l.contains("UNDELIVERED"))
        .unwrap_or_else(|| panic!("the failure was not reported: {sent:?}"));
    assert!(report.contains("label=B-matrix-1"), "{report}");
    assert!(
        report.contains("no Matrix room is mapped"),
        "the reason travels: {report}"
    );
    // No msgid: there is none to name.
    assert!(
        !report.contains("msgid="),
        "a relayed post has no msgid: {report}"
    );
}

#[tokio::test]
async fn naming_a_room_in_matrix_renames_the_channel() {
    // Reported 2026-08-09: a bridged channel kept showing a bare ULID. An unnamed
    // Matrix room has no readable name to take, so the id is the honest fallback —
    // but naming it in Element afterwards changed nothing, because the rename was
    // never consumed. Now it re-asserts, and weftd tells the members.
    let (mut bridge, mut lines, _calls) = bridge_with(kde_space()).await;
    bridge
        .provision("matrix://kde.org/community")
        .await
        .unwrap();
    drain(&mut lines);

    bridge
        .on_matrix_event(json!({
            "type": "m.room.name",
            "room_id": "!gen:kde.org",
            "event_id": "$name",
            "sender": "@carol:kde.org",
            "state_key": "",
            "origin_server_ts": 1_722_000_000_000u64,
            "content": { "name": "General Chat" },
        }))
        .await;

    let sent = drain(&mut lines);
    let layout = sent
        .iter()
        .find(|l| l.contains("CHANNEL-LAYOUT"))
        .unwrap_or_else(|| panic!("the rename was not re-asserted: {sent:?}"));
    assert!(layout.contains("vanity=general-chat"), "{layout}");

    // A canonical alias serves the same purpose for a room nobody named.
    bridge
        .on_matrix_event(json!({
            "type": "m.room.canonical_alias",
            "room_id": "!gen:kde.org",
            "event_id": "$alias",
            "sender": "@carol:kde.org",
            "state_key": "",
            "origin_server_ts": 1_722_000_000_001u64,
            "content": { "alias": "#lobby:kde.org" },
        }))
        .await;
    let sent = drain(&mut lines);
    let layout = sent
        .iter()
        .find(|l| l.contains("CHANNEL-LAYOUT"))
        .unwrap_or_else(|| panic!("the alias was not re-asserted: {sent:?}"));
    assert!(layout.contains("vanity=lobby"), "{layout}");

    // …but an event that names *nothing* must leave the name alone. A canonical
    // alias moving only its `alt_aliases`, a cleared name, a redaction: none of
    // them say what the channel is called, and an assertion is the whole truth —
    // so the old "fall back to the id" turned each of them into a silent rename to
    // an unreadable ULID. Reported 2026-08-16 after an overnight disconnect, where
    // a homeserver replaying its queued transactions is exactly how one of these
    // arrives out of nowhere.
    for quiet in [
        json!({ "alt_aliases": ["#other:kde.org"] }),
        json!({ "name": "" }),
        json!({}),
    ] {
        bridge
            .on_matrix_event(json!({
                "type": "m.room.canonical_alias",
                "room_id": "!gen:kde.org",
                "event_id": "$quiet",
                "sender": "@carol:kde.org",
                "state_key": "",
                "origin_server_ts": 1_722_000_000_002u64,
                "content": quiet,
            }))
            .await;

        let sent = drain(&mut lines);
        assert!(
            !sent.iter().any(|l| l.contains("CHANNEL-LAYOUT")),
            "an event naming nothing renamed the channel: {sent:?}"
        );
    }
}
