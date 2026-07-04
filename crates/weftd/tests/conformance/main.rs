//! Conformance: black-box protocol tests against a real in-process weftd —
//! genuine QUIC (ALPN `weft/1`) and WebSocket connections on ephemeral
//! ports (CLAUDE.md M1: `tests/conformance/`).

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use weft_proto::{ErrCode, Event, Reply};
use weft_transport::insecure::client_endpoint;
use weft_transport::QuicControlStream;
use weftd::config::{ChannelConfig, Config, Identity, Listen};

// ---- harness ----

const PASSWORD: &str = "conformance-pw-123";

async fn start_server(channels: &[&str]) -> weftd::Server {
    start_with(channels, |_| {}).await
}

async fn start_with(channels: &[&str], tweak: impl FnOnce(&mut Config)) -> weftd::Server {
    let mut config = Config {
        network: "test.example".to_string(),
        motd: Some("conformance".to_string()),
        channels: channels
            .iter()
            .map(|c| ChannelConfig::Name(c.to_string()))
            .collect(),
        listen: Listen {
            quic: "127.0.0.1:0".parse().unwrap(),
            ws: Some("127.0.0.1:0".parse().unwrap()),
            http: Some("127.0.0.1:0".parse().unwrap()),
        },
        identity: Identity { key_file: None }, // ephemeral key per test
        ..Config::default()
    };
    tweak(&mut config);
    weftd::start(config).await.expect("server start")
}

/// A QUIC conformance client. Keeps endpoint + connection alive for the
/// stream's lifetime.
struct QuicClient {
    _endpoint: quinn::Endpoint,
    _connection: quinn::Connection,
    stream: QuicControlStream,
}

impl QuicClient {
    async fn connect(addr: SocketAddr) -> Self {
        let endpoint = client_endpoint(weft_transport::ALPN).unwrap();
        let connection = endpoint
            .connect(addr, "localhost")
            .unwrap()
            .await
            .expect("QUIC connect");
        let stream = QuicControlStream::open(&connection)
            .await
            .expect("control stream");
        Self {
            _endpoint: endpoint,
            _connection: connection,
            stream,
        }
    }

    async fn send(&mut self, line: &str) {
        self.stream.send_line(line).await.expect("send");
    }

    async fn recv(&mut self) -> Reply {
        let line = tokio::time::timeout(Duration::from_secs(5), self.stream.recv_line())
            .await
            .expect("timed out")
            .expect("recv")
            .expect("stream closed");
        Reply::parse(&line).expect("unparseable server line")
    }

    /// HELLO + REGISTER (registration doubles as auth, §6.1).
    async fn ready(&mut self, account: &str) {
        self.send("HELLO weft/1").await;
        assert!(matches!(self.recv().await.event, Event::Welcome { .. }));
        self.send(&format!("REGISTER {account} :{PASSWORD}")).await;
        assert!(matches!(self.recv().await.event, Event::Welcome { .. }));
    }

    async fn join(&mut self, channel: &str) {
        self.send(&format!("JOIN {channel}")).await;
        assert!(matches!(self.recv().await.event, Event::Member { .. }));
        assert!(matches!(self.recv().await.event, Event::Policy { .. }));
    }
}

// ---- QUIC ----

#[tokio::test]
async fn quic_full_session_flow() {
    let server = start_server(&["#general"]).await;
    let mut client = QuicClient::connect(server.quic_addr).await;

    client.send("@label=h1 HELLO weft/1").await;
    let welcome = client.recv().await;
    assert_eq!(welcome.label.as_deref(), Some("h1"));
    let Event::Welcome { network, motd, .. } = &welcome.event else {
        panic!("expected WELCOME, got {welcome:?}");
    };
    assert_eq!(network.as_str(), "test.example");
    assert_eq!(motd.as_deref(), Some("conformance"));

    client.send(&format!("REGISTER ada :{PASSWORD}")).await;
    assert!(matches!(client.recv().await.event, Event::Welcome { .. }));
    client.join("#general").await;

    client.send("@label=m1 MSG #general :over real QUIC").await;
    let echo = client.recv().await;
    assert_eq!(echo.label.as_deref(), Some("m1"));
    let Event::Message(msg) = &echo.event else {
        panic!("expected MESSAGE echo, got {echo:?}");
    };
    assert_eq!(msg.body, "over real QUIC");
    assert_eq!(msg.msgid.origin().as_str(), "test.example");

    server.shutdown().await;
}

#[tokio::test]
async fn quic_relays_between_connections() {
    let server = start_server(&["#general"]).await;
    let mut ada = QuicClient::connect(server.quic_addr).await;
    let mut bob = QuicClient::connect(server.quic_addr).await;
    ada.ready("ada").await;
    bob.ready("bob").await;
    ada.join("#general").await;
    bob.join("#general").await;
    ada.recv().await; // bob's MEMBER join broadcast

    ada.send("@label=x MSG #general :hi bob").await;
    let echo = ada.recv().await;
    let Event::Message(sent) = &echo.event else {
        panic!()
    };

    let copy = bob.recv().await;
    assert_eq!(copy.label, None, "broadcast copies never carry labels");
    let Event::Message(received) = &copy.event else {
        panic!("expected MESSAGE, got {copy:?}");
    };
    assert_eq!(received.msgid, sent.msgid);
    assert_eq!(received.sender.to_string(), "ada@test.example");

    server.shutdown().await;
}

#[tokio::test]
#[ignore = "slow (~45 s): exercises the idle/keepalive windows; run with --ignored"]
async fn quic_survives_a_long_quiet_gap_with_keepalive() {
    // Regression: quinn's default 30 s max_idle_timeout once silently killed
    // quiet-but-healthy connections. A client keeping the §3.4 cadence
    // (PING every 10 s, answered) must survive an arbitrarily long lull —
    // neither the transport idle limit (120 s) nor the session's liveness
    // window (~30 s of line silence) may fire.
    let server = start_server(&["#general"]).await;
    let mut client = QuicClient::connect(server.quic_addr).await;
    client.ready("ada").await;
    client.join("#general").await;

    for i in 0..4 {
        tokio::time::sleep(Duration::from_secs(10)).await;
        client.send(&format!("PING k{i}")).await;
        let reply = client.recv().await;
        assert!(
            matches!(&reply.event, Event::Pong { token: Some(t) } if t == &format!("k{i}")),
            "keepalive {i} not answered: {reply:?}"
        );
    }

    client.send("@label=s1 MSG #general :still here").await;
    let echo = client.recv().await;
    assert_eq!(echo.label.as_deref(), Some("s1"));
    server.shutdown().await;
}

#[tokio::test]
async fn quic_rejects_wrong_alpn() {
    let server = start_server(&[]).await;
    let endpoint = client_endpoint(b"not-weft").unwrap();
    let result = endpoint
        .connect(server.quic_addr, "localhost")
        .unwrap()
        .await;
    assert!(result.is_err(), "handshake must fail without ALPN weft/1");
    server.shutdown().await;
}

#[tokio::test]
async fn quic_version_mismatch_gets_unsupported() {
    let server = start_server(&[]).await;
    let mut client = QuicClient::connect(server.quic_addr).await;
    client.send("HELLO weft/2").await;
    let reply = client.recv().await;
    assert!(
        matches!(&reply.event, Event::Err(e) if e.code == ErrCode::Unsupported),
        "got {reply:?}"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn quic_malformed_line_gets_err() {
    let server = start_server(&[]).await;
    let mut client = QuicClient::connect(server.quic_addr).await;
    client.send("@label=b1 JOIN not-a-channel").await;
    let reply = client.recv().await;
    assert_eq!(reply.label.as_deref(), Some("b1"));
    assert!(matches!(&reply.event, Event::Err(e) if e.code == ErrCode::Malformed));
    server.shutdown().await;
}

// ---- WebSocket fallback ----

struct WsClient {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

impl WsClient {
    async fn connect(addr: SocketAddr) -> Self {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
            .await
            .expect("WS connect");
        Self { ws }
    }

    async fn send(&mut self, line: &str) {
        self.ws
            .send(Message::Text(line.to_string()))
            .await
            .expect("ws send");
    }

    async fn recv(&mut self) -> Reply {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(5), self.ws.next())
                .await
                .expect("timed out")
                .expect("ws closed")
                .expect("ws error");
            if let Message::Text(line) = msg {
                return Reply::parse(&line).expect("unparseable server line");
            }
        }
    }
}

#[tokio::test]
async fn ws_fallback_speaks_the_same_protocol() {
    let server = start_server(&["#general"]).await;
    let mut client = WsClient::connect(server.ws_addr.expect("ws enabled")).await;

    client.send("HELLO weft/1").await;
    assert!(matches!(client.recv().await.event, Event::Welcome { .. }));
    client.send(&format!("REGISTER ada :{PASSWORD}")).await;
    assert!(matches!(client.recv().await.event, Event::Welcome { .. }));
    client.send("JOIN #general").await;
    assert!(matches!(client.recv().await.event, Event::Member { .. }));
    assert!(matches!(client.recv().await.event, Event::Policy { .. }));

    client.send("@label=w1 MSG #general :over websocket").await;
    let echo = client.recv().await;
    assert_eq!(echo.label.as_deref(), Some("w1"));
    assert!(matches!(&echo.event, Event::Message(m) if m.body == "over websocket"));

    server.shutdown().await;
}

#[tokio::test]
async fn quic_and_ws_share_channels() {
    let server = start_server(&["#general"]).await;
    let mut quic = QuicClient::connect(server.quic_addr).await;
    quic.ready("ada").await;
    quic.join("#general").await;

    let mut ws = WsClient::connect(server.ws_addr.expect("ws enabled")).await;
    ws.send("HELLO weft/1").await;
    ws.recv().await;
    ws.send(&format!("REGISTER bob :{PASSWORD}")).await;
    ws.recv().await;
    ws.send("JOIN #general").await;
    ws.recv().await;
    ws.recv().await;
    quic.recv().await; // bob's MEMBER join broadcast

    // Transport is invisible at the protocol layer: QUIC → WS relay.
    quic.send("MSG #general :cross-transport").await;
    let Event::Message(echo) = quic.recv().await.event else {
        panic!()
    };
    let copy = ws.recv().await;
    let Event::Message(received) = &copy.event else {
        panic!("expected MESSAGE, got {copy:?}");
    };
    assert_eq!(received.msgid, echo.msgid);

    server.shutdown().await;
}

// ---- M2: identity ----

/// Raw HTTP GET, no client dep: fetch the §10.2 well-known document.
async fn fetch_wellknown(addr: SocketAddr) -> serde_json::Value {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut tcp = tokio::net::TcpStream::connect(addr).await.expect("connect");
    tcp.write_all(
        b"GET /.well-known/weft HTTP/1.1\r\nHost: test.example\r\nConnection: close\r\n\r\n",
    )
    .await
    .expect("request");
    let mut response = String::new();
    tcp.read_to_string(&mut response).await.expect("response");
    let (head, body) = response.split_once("\r\n\r\n").expect("http response");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    serde_json::from_str(body.trim()).expect("json body")
}

/// The whole §6.1 + §10.2 story, black-box: register, enroll a device,
/// key-auth on a fresh connection, and verify the attestation against the
/// signing key published at /.well-known/weft — exactly what a remote
/// network would do.
#[tokio::test]
async fn key_auth_attestation_verifies_against_wellknown() {
    let server = start_server(&["#general"]).await;
    let device = weft_crypto::Keypair::generate();

    let mut first = QuicClient::connect(server.quic_addr).await;
    first.ready("ada").await;
    first
        .send(&format!("AUTH ENROLL {}", device.public().to_b64()))
        .await;
    let reply = first.recv().await;
    assert!(
        matches!(
            &reply.event,
            Event::Welcome {
                attestation: Some(_),
                ..
            }
        ),
        "ENROLL must return an attestation, got {reply:?}"
    );

    // Fresh connection, challenge-response.
    let mut second = QuicClient::connect(server.quic_addr).await;
    second.send("HELLO weft/1").await;
    second.recv().await;
    second
        .send(&format!("AUTH KEY ada {}", device.public().to_b64()))
        .await;
    let reply = second.recv().await;
    let Event::Challenge { nonce } = &reply.event else {
        panic!("expected CHALLENGE, got {reply:?}");
    };
    let nonce = weft_crypto::b64::decode(nonce).unwrap();
    let sig = weft_crypto::sign_challenge(&device, &nonce, "test.example");
    second
        .send(&format!(
            "AUTH PROOF {}",
            weft_crypto::signature_to_b64(&sig)
        ))
        .await;
    let reply = second.recv().await;
    let Event::Welcome {
        attestation: Some(blob),
        ..
    } = &reply.event
    else {
        panic!("expected WELCOME + attestation, got {reply:?}");
    };

    // Remote-verifier path: well-known key ⇒ attestation checks out.
    let doc = fetch_wellknown(server.http_addr.expect("http enabled")).await;
    assert_eq!(doc["network"], "test.example");
    let signing_key =
        weft_crypto::PublicKey::from_b64(doc["signing-key"].as_str().expect("signing-key"))
            .expect("valid published key");
    let attestation = weft_crypto::Attestation::from_b64(blob).expect("parseable attestation");
    attestation
        .verify(&signing_key, 0)
        .expect("attestation must verify");
    assert_eq!(attestation.account, "ada");
    assert_eq!(attestation.device, device.public());

    // Key-authed session is READY.
    second.join("#general").await;
    server.shutdown().await;
}

#[tokio::test]
async fn wrong_password_and_closed_registration() {
    let server = start_with(&[], |config| {
        config.registration = weftd::config::Registration::Closed;
    })
    .await;
    let mut client = QuicClient::connect(server.quic_addr).await;
    client.send("HELLO weft/1").await;
    client.recv().await;
    client.send(&format!("REGISTER ada :{PASSWORD}")).await;
    let reply = client.recv().await;
    assert!(matches!(&reply.event, Event::Err(e) if e.code == ErrCode::Forbidden));
    client.send("AUTH PASSWORD ada :wrong-password!!").await;
    let reply = client.recv().await;
    assert!(matches!(&reply.event, Event::Err(e) if e.code == ErrCode::AuthFailed));
    server.shutdown().await;
}

/// "Real certs": the operator-supplied PEM path must produce a working
/// QUIC endpoint (self-signed here, but exercising exactly the load path).
#[tokio::test]
async fn operator_pem_certificates_are_accepted() {
    let dir = std::env::temp_dir().join(format!("weftd-tls-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cert = rcgen::generate_simple_self_signed(vec!["test.example".to_string()]).unwrap();
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();
    std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();

    let server = start_with(&[], |config| {
        config.tls = Some(weftd::config::Tls {
            cert: cert_path.clone(),
            key: key_path.clone(),
        });
    })
    .await;
    let mut client = QuicClient::connect(server.quic_addr).await;
    client.send("HELLO weft/1").await;
    assert!(matches!(client.recv().await.event, Event::Welcome { .. }));
    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- M3a: persistence, mutations, HISTORY ----

/// The full message lifecycle over real QUIC: post, edit, react, delete,
/// then fetch HISTORY and check the batch is the §12.1 materialization.
#[tokio::test]
async fn message_lifecycle_and_history_over_quic() {
    let server = start_with(&[], |config| {
        config.channels = vec![
            ChannelConfig::Name("#kept".to_string()),
            ChannelConfig::Detailed {
                name: "#volatile".to_string(),
                policy: "ephemeral".to_string(),
            },
        ];
    })
    .await;
    let mut ada = QuicClient::connect(server.quic_addr).await;
    ada.ready("ada").await;
    ada.join("#kept").await;

    // The configured non-default policy is announced on join (§5.2).
    ada.send("JOIN #volatile").await;
    ada.recv().await; // MEMBER
    let reply = ada.recv().await;
    assert!(
        matches!(&reply.event, Event::Policy { policy, .. } if policy.to_string() == "ephemeral"),
        "got {reply:?}"
    );

    ada.send("MSG #kept :draft").await;
    let Event::Message(msg) = ada.recv().await.event else {
        panic!()
    };
    let msgid = msg.msgid.to_string();
    ada.send(&format!("EDIT {msgid} :final")).await;
    assert!(matches!(ada.recv().await.event, Event::Edited { .. }));
    ada.send(&format!("REACT {msgid} 🚀")).await;
    assert!(matches!(ada.recv().await.event, Event::Reaction { .. }));

    ada.send("@label=h1 HISTORY #kept").await;
    assert!(matches!(ada.recv().await.event, Event::BatchStart { .. }));
    let Event::Message(materialized) = ada.recv().await.event else {
        panic!("expected materialized MESSAGE")
    };
    assert_eq!(materialized.body, "final");
    assert_eq!(materialized.edited, Some(1));
    let Event::Reactions { emoji, count, .. } = ada.recv().await.event else {
        panic!("expected REACTIONS summary")
    };
    assert_eq!((emoji.as_str(), count), ("🚀", 1));
    let Event::BatchEnd { compacted, .. } = ada.recv().await.event else {
        panic!()
    };
    assert!(compacted);

    // Ephemeral channel: honest empty batch.
    ada.send("MSG #volatile :vanishes").await;
    ada.recv().await;
    ada.send("HISTORY #volatile").await;
    assert!(matches!(ada.recv().await.event, Event::BatchStart { .. }));
    let Event::BatchEnd {
        truncated: true, ..
    } = ada.recv().await.event
    else {
        panic!("ephemeral history must be empty + truncated")
    };
    server.shutdown().await;
}

// ---- M3b: DMs + MARK sync ----

#[tokio::test]
async fn dms_and_mark_sync_over_quic() {
    let server = start_server(&["#general"]).await;
    let mut ada = QuicClient::connect(server.quic_addr).await;
    let mut bob = QuicClient::connect(server.quic_addr).await;
    ada.ready("ada").await;
    bob.ready("bob").await;

    // DM: labeled echo to sender, copy to recipient, history symmetric.
    ada.send("@label=d1 MSG @bob :hey bob").await;
    let echo = ada.recv().await;
    assert_eq!(echo.label.as_deref(), Some("d1"));
    let Event::Message(sent) = &echo.event else {
        panic!()
    };
    let copy = bob.recv().await;
    let Event::Message(received) = &copy.event else {
        panic!("expected DM MESSAGE, got {copy:?}");
    };
    assert_eq!(received.msgid, sent.msgid);

    bob.send("HISTORY @ada").await;
    assert!(matches!(bob.recv().await.event, Event::BatchStart { .. }));
    let Event::Message(item) = bob.recv().await.event else {
        panic!()
    };
    assert_eq!(item.body, "hey bob");
    assert!(matches!(bob.recv().await.event, Event::BatchEnd { .. }));

    // Unknown recipient: the anti-enumeration code (§2.2).
    ada.send("MSG @nobody :hello?").await;
    let reply = ada.recv().await;
    assert!(matches!(&reply.event, Event::Err(e) if e.code == ErrCode::NoSuchTarget));

    // MARK: echo to the marker, sync to the same account's other device.
    let mut ada2 = QuicClient::connect(server.quic_addr).await;
    ada2.send("HELLO weft/1").await;
    ada2.recv().await;
    ada2.send(&format!("AUTH PASSWORD ada :{PASSWORD}")).await;
    ada2.recv().await;

    ada.send("JOIN #general").await;
    ada.recv().await;
    ada.recv().await;
    ada.send("MSG #general :mark me").await;
    let Event::Message(msg) = ada.recv().await.event else {
        panic!()
    };
    ada.send(&format!("@label=k1 MARK #general {}", msg.msgid))
        .await;
    let echo = ada.recv().await;
    assert_eq!(echo.label.as_deref(), Some("k1"));
    assert!(matches!(&echo.event, Event::Marked { .. }));
    let sync = ada2.recv().await;
    assert!(
        matches!(&sync.event, Event::Marked { msgid, .. } if *msgid == msg.msgid),
        "second device must get the MARKED sync, got {sync:?}"
    );
    server.shutdown().await;
}

// ---- M4c: namespace ownership + signed recovery over real QUIC ----

#[tokio::test]
async fn namespace_transfer_signed_over_quic() {
    let server = start_server(&[]).await;
    let mut ada = QuicClient::connect(server.quic_addr).await;
    ada.ready("ada").await;

    // Create a namespace with a client-held root key.
    let root = weft_crypto::Keypair::generate();
    ada.send(&format!(
        "@root={} NS CREATE gaming public",
        root.public().to_b64()
    ))
    .await;
    assert!(matches!(ada.recv().await.event, Event::NsMeta { .. }));

    // A forged transfer signature is FORBIDDEN.
    ada.send("@sig=Zm9yZ2Vk NS TRANSFER gaming bob").await;
    assert!(matches!(&ada.recv().await.event, Event::Err(e) if e.code == ErrCode::Forbidden));

    // A real root signature over (namespace, new_owner) transfers ownership.
    let sig = weft_crypto::sign_transfer(&root, "gaming", "bob");
    ada.send(&format!(
        "@sig={} NS TRANSFER gaming bob",
        weft_crypto::signature_to_b64(&sig)
    ))
    .await;
    let reply = ada.recv().await;
    assert!(
        matches!(&reply.event, Event::NsMeta { owner: Some(o), .. } if o == "bob"),
        "transfer should hand ownership to bob, got {reply:?}"
    );

    // bob now administers; ada does not.
    let mut bob = QuicClient::connect(server.quic_addr).await;
    bob.ready("bob").await;
    bob.send("NS VISIBILITY gaming unlisted").await;
    assert!(matches!(bob.recv().await.event, Event::NsMeta { .. }));
    ada.send("NS VISIBILITY gaming public").await;
    assert!(matches!(&ada.recv().await.event, Event::Err(e) if e.code == ErrCode::CapRequired));

    server.shutdown().await;
}
