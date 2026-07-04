//! Conformance: black-box protocol tests against a real in-process weftd —
//! genuine QUIC (ALPN `weft/1`) and WebSocket connections on ephemeral
//! ports (CLAUDE.md M1: `tests/conformance/`).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use quinn::crypto::rustls::QuicClientConfig;
use tokio_tungstenite::tungstenite::Message;
use weft_proto::{ErrCode, Event, Reply};
use weft_transport::QuicControlStream;
use weftd::config::{Config, Listen};

// ---- harness ----

async fn start_server(channels: &[&str]) -> weftd::Server {
    let config = Config {
        network: "test.example".to_string(),
        motd: Some("conformance".to_string()),
        channels: channels.iter().map(|c| c.to_string()).collect(),
        listen: Listen {
            quic: "127.0.0.1:0".parse().unwrap(),
            ws: Some("127.0.0.1:0".parse().unwrap()),
        },
    };
    weftd::start(config).await.expect("server start")
}

/// Test-only TLS verifier: the M1 server runs on a fresh self-signed cert
/// (nothing to pin until well-known lands in M2), so conformance clients
/// accept any certificate. Never ship this pattern in a real client.
#[derive(Debug)]
struct AcceptAnyCert(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn client_endpoint(alpn: &[u8]) -> quinn::Endpoint {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider)))
        .with_no_client_auth();
    tls.alpn_protocols = vec![alpn.to_vec()];
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(tls).unwrap(),
    )));
    endpoint
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
        let endpoint = client_endpoint(weft_transport::ALPN);
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

    /// HELLO + anonymous AUTH, draining both WELCOMEs.
    async fn ready(&mut self, account: &str) {
        self.send("HELLO weft/1").await;
        assert!(matches!(self.recv().await.event, Event::Welcome { .. }));
        self.send(&format!("AUTH PASSWORD {account} :pw")).await;
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

    client.send("AUTH PASSWORD ada :pw").await;
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
async fn quic_rejects_wrong_alpn() {
    let server = start_server(&[]).await;
    let endpoint = client_endpoint(b"not-weft");
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
    client.send("AUTH PASSWORD ada :pw").await;
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
    ws.send("AUTH PASSWORD bob :pw").await;
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
