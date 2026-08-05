//! The daemon's core against a mock homeserver: provisioning, both traffic
//! directions, and ban enforcement — everything short of a live weftd, whose
//! side of the contract is pinned by `weft-core`'s own provider tests.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::routing::{get, post, put};
use serde_json::{json, Value};
use weft_appservice::Realm;
use weft_matrix::bridge::Bridge;
use weft_matrix::hs::Hs;
use weft_matrix::ident;
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
            }),
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
        domain: "test.example".into(),
        puppet_prefix: "weft_".into(),
        bot_localpart: "weftbot".into(),
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
        .on_incoming(weft_appservice::Incoming::Event(
            weft_proto::Event::Message(Box::new(weft_proto::MessageEvent {
                target: weft_proto::Target::Channel(channel.parse().unwrap()),
                sender: "ada@test.example".parse().unwrap(),
                msgid: msgid.clone(),
                body: "hi from weft".into(),
                meta: weft_proto::MsgMeta::default(),
                edited: None,
                edited_at: None,
            })),
        ))
        .await;

    let recorded = calls.lock().unwrap().clone();
    let puppet = format!("weft_{ADA_ULID}");
    assert!(
        recorded
            .iter()
            .any(|(what, _, body)| what == "POST register" && body["username"] == *puppet),
        "puppet registered by ULID: {recorded:?}"
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
        .on_incoming(weft_appservice::Incoming::Event(
            weft_proto::Event::Bridging {
                namespace: ns_id.parse().unwrap(),
                state: weft_proto::BridgingState::Banned,
            },
        ))
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
        .on_incoming(weft_appservice::Incoming::Event(
            weft_proto::Event::Message(Box::new(weft_proto::MessageEvent {
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
        ))
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
