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
            https: None,
            irc: None,
            web: false,
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

    /// HELLO + REGISTER (registration doubles as auth, §6.1). Returns the §13
    /// media fetch bearer the server pushes right after auth.
    async fn ready(&mut self, account: &str) -> String {
        self.send("HELLO weft/1").await;
        assert!(matches!(self.recv().await.event, Event::Welcome { .. }));
        self.send(&format!("REGISTER {account} :{PASSWORD}")).await;
        assert!(matches!(self.recv().await.event, Event::Welcome { .. }));
        let Event::MediaToken { token } = self.recv().await.event else {
            panic!("expected MEDIA TOKEN after auth");
        };
        token
    }

    async fn join(&mut self, channel: &str) {
        self.send(&format!("JOIN {channel}")).await;
        assert!(matches!(self.recv().await.event, Event::Member { .. }));
        assert!(matches!(self.recv().await.event, Event::Policy { .. }));
    }

    /// Receive, skipping events (e.g. interleaved broadcast copies) until one
    /// matches `want`. Bounded so a genuinely-missing event fails, not hangs.
    async fn recv_until(&mut self, want: impl Fn(&Reply) -> bool) -> Reply {
        for _ in 0..16 {
            let reply = self.recv().await;
            if want(&reply) {
                return reply;
            }
        }
        panic!("expected event never arrived");
    }

    /// §13 upload a blob on a data-plane bidi stream; returns its weft-media URI.
    async fn blob_upload(&self, token: &str, bytes: &[u8]) -> String {
        let (mut send, mut recv) = self._connection.open_bi().await.expect("open data stream");
        send.write_all(format!("PUT {token}\n").as_bytes())
            .await
            .unwrap();
        send.write_all(bytes).await.unwrap();
        let _ = send.finish();
        let resp = recv.read_to_end(64 * 1024).await.expect("upload response");
        let line = String::from_utf8_lossy(&resp);
        line.trim()
            .strip_prefix("OK ")
            .unwrap_or_else(|| panic!("upload failed: {}", line.trim()))
            .to_string()
    }

    /// §13 fetch a blob on a data-plane bidi stream (optional `start-end` range).
    async fn blob_download(&self, bearer: &str, hash: &str, range: Option<&str>) -> Vec<u8> {
        let (mut send, mut recv) = self._connection.open_bi().await.expect("open data stream");
        let req = match range {
            Some(r) => format!("GET {bearer} {hash} {r}\n"),
            None => format!("GET {bearer} {hash}\n"),
        };
        send.write_all(req.as_bytes()).await.unwrap();
        let _ = send.finish();
        let resp = recv
            .read_to_end(600 * 1024 * 1024)
            .await
            .expect("download response");
        let nl = resp
            .iter()
            .position(|&b| b == b'\n')
            .expect("response header");
        let header = String::from_utf8_lossy(&resp[..nl]).into_owned();
        assert!(header.starts_with("OK "), "download failed: {header}");
        resp[nl + 1..].to_vec()
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
    assert!(matches!(
        client.recv().await.event,
        Event::MediaToken { .. }
    ));
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

// ---- §13 media: data plane (M-media-0) + posting/gating/GC (M-media-1) ----

/// Upload a blob (control OFFER→ACCEPT + data-plane PUT), returning `(uri, hash)`.
async fn upload_blob(client: &mut QuicClient, mime: &str, data: &[u8]) -> (String, String) {
    client
        .send(&format!("STREAM OFFER media {mime} {}", data.len()))
        .await;
    let Event::StreamAccept { token } = client.recv().await.event else {
        panic!("expected STREAM ACCEPT");
    };
    let uri = client.blob_upload(&token, data).await;
    let hash = uri.rsplit('/').next().unwrap().to_string();
    (uri, hash)
}

#[tokio::test]
async fn media_attachment_message_gates_fetch_and_gcs_on_delete() {
    let server = start_server(&["#general"]).await;
    let mut ada = QuicClient::connect(server.quic_addr).await;
    ada.ready("ada").await;
    ada.join("#general").await;

    let data = b"hello media data plane".to_vec();
    let (uri, hash) = upload_blob(&mut ada, "text/plain", &data).await;
    assert!(uri.starts_with("weft-media://test.example/"), "uri: {uri}");

    // Dedup: a second upload of identical bytes yields the SAME hash.
    let (uri2, _) = upload_blob(&mut ada, "text/plain", &data).await;
    assert_eq!(uri, uri2, "identical bytes must dedupe to one content hash");

    // A blob nobody has posted yet is NOT fetchable (no scope references it).
    let ada_bearer = server.ctx().mint_media_bearer("ada".parse().unwrap());
    assert!(
        fetch_fails(&ada, &ada_bearer, &hash).await,
        "unreferenced blob is gated"
    );

    // Post it as an attachment (empty body is legal with attachments, §6.4).
    ada.send(&format!("@attach.1={uri} MSG #general :look"))
        .await;
    let echo = ada.recv().await;
    let Event::Message(m) = &echo.event else {
        panic!("expected the MESSAGE echo, got {echo:?}");
    };
    assert_eq!(m.meta.attachments, vec![uri.clone()]);
    let msgid = m.msgid.to_string();

    // Now a MEMBER of #general (which references the blob) may fetch it.
    assert_eq!(ada.blob_download(&ada_bearer, &hash, None).await, data);
    assert_eq!(
        ada.blob_download(&ada_bearer, &hash, Some("0-4")).await,
        b"hello"
    );

    // A NON-member is denied — invariant 1: gated is indistinguishable from absent.
    let bob_bearer = server.ctx().mint_media_bearer("bob".parse().unwrap());
    assert!(
        fetch_fails(&ada, &bob_bearer, &hash).await,
        "non-member must be denied"
    );

    // Delete the message → its blob reference drops → refcount hits 0.
    ada.send(&format!("DELETE {msgid}")).await;
    assert!(matches!(ada.recv().await.event, Event::Deleted { .. }));

    // A GC pass (cutoff in the far future ⇒ grace elapsed) collects the orphan.
    let removed =
        weft_core::gc_orphan_blobs(&server.ctx().media_refs, &server.ctx().blobs, u64::MAX).await;
    assert!(removed >= 1, "GC should collect the now-orphaned blob");
    assert!(
        fetch_fails(&ada, &ada_bearer, &hash).await,
        "GC'd blob is gone"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn media_posting_and_gated_fetch_over_http() {
    let server = start_server(&["#general"]).await;
    let http = server.http_addr.expect("http enabled");

    // Browser shape: WS/QUIC control session + HTTP media transfer.
    let mut ada = QuicClient::connect(server.quic_addr).await;
    ada.ready("ada").await;
    ada.join("#general").await;

    // Upload via HTTP POST (token minted over the control session).
    let data = b"http media bytes".to_vec();
    ada.send(&format!("STREAM OFFER media text/plain {}", data.len()))
        .await;
    let Event::StreamAccept { token } = ada.recv().await.event else {
        panic!("expected STREAM ACCEPT");
    };
    let (status, body) = http_post(http, &format!("/media?t={token}"), &data).await;
    assert_eq!(status, 200, "upload: {}", String::from_utf8_lossy(&body));
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let hash = json["hash"].as_str().unwrap().to_string();
    let uri = json["media"].as_str().unwrap().to_string();

    // Post it so a member may fetch it.
    ada.send(&format!("@attach.1={uri} MSG #general :look"))
        .await;
    assert!(matches!(ada.recv().await.event, Event::Message(_)));

    // Member fetch over HTTP (bearer in the query).
    let bearer = server.ctx().mint_media_bearer("ada".parse().unwrap());
    let (status, got) = http_get(http, &format!("/media/{hash}?t={bearer}"), None).await;
    assert_eq!(status, 200);
    assert_eq!(got, data);

    // Ranged GET → 206 Partial Content.
    let (status, head) = http_get(
        http,
        &format!("/media/{hash}?t={bearer}"),
        Some("bytes=0-3"),
    )
    .await;
    assert_eq!(status, 206);
    assert_eq!(head, b"http");

    // Non-member bearer AND bad bearer both → 404 (gated == absent, invariant 1).
    let bob = server.ctx().mint_media_bearer("bob".parse().unwrap());
    assert_eq!(
        http_get(http, &format!("/media/{hash}?t={bob}"), None)
            .await
            .0,
        404
    );
    assert_eq!(
        http_get(http, &format!("/media/{hash}?t=nope"), None)
            .await
            .0,
        404
    );

    server.shutdown().await;
}

#[tokio::test]
async fn media_image_probes_dimensions_and_generates_thumbnail() {
    let server = start_server(&["#general"]).await;
    let http = server.http_addr.expect("http enabled");
    let mut ada = QuicClient::connect(server.quic_addr).await;
    ada.ready("ada").await;
    ada.join("#general").await;

    // A real 300×200 PNG.
    let img = image::RgbImage::from_fn(300, 200, |x, _| image::Rgb([(x % 256) as u8, 10, 20]));
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let png = png.into_inner();

    // Upload via HTTP; the response carries probed dimensions + a thumbnail URI.
    ada.send(&format!("STREAM OFFER media image/png {}", png.len()))
        .await;
    let Event::StreamAccept { token } = ada.recv().await.event else {
        panic!("expected STREAM ACCEPT");
    };
    let (status, body) = http_post(http, &format!("/media?t={token}"), &png).await;
    assert_eq!(status, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["width"], 300);
    assert_eq!(json["height"], 200);
    let media_uri = json["media"].as_str().unwrap().to_string();
    let hash = json["hash"].as_str().unwrap().to_string();
    let thumb_uri = json["thumb"]
        .as_str()
        .expect("a thumbnail was generated")
        .to_string();
    let thumb_hash = thumb_uri.rsplit('/').next().unwrap().to_string();
    assert_ne!(thumb_hash, hash, "the thumbnail is a distinct blob");

    // Post the image — the actor references BOTH it and its thumbnail.
    ada.send(&format!("@attach.1={media_uri} MSG #general :pic"))
        .await;
    let Event::Message(m) = &ada.recv().await.event else {
        panic!("expected MESSAGE echo");
    };
    let msgid = m.msgid.to_string();

    // A MEMBER fetches the image and the (gated) thumbnail; the thumb fits 256px.
    let bearer = server.ctx().mint_media_bearer("ada".parse().unwrap());
    assert_eq!(ada.blob_download(&bearer, &hash, None).await, png);
    let thumb_bytes = ada.blob_download(&bearer, &thumb_hash, None).await;
    let thumb_img = image::load_from_memory(&thumb_bytes).expect("thumbnail decodes");
    assert!(
        thumb_img.width() <= 256 && thumb_img.height() <= 256,
        "thumb ≤256px"
    );

    // A NON-member is denied the thumbnail too — it shares the parent's gating.
    let bob = server.ctx().mint_media_bearer("bob".parse().unwrap());
    assert!(
        fetch_fails(&ada, &bob, &thumb_hash).await,
        "thumb is member-gated"
    );

    // Deleting the message orphans BOTH; one GC pass collects them together.
    ada.send(&format!("DELETE {msgid}")).await;
    assert!(matches!(ada.recv().await.event, Event::Deleted { .. }));
    weft_core::gc_orphan_blobs(&server.ctx().media_refs, &server.ctx().blobs, u64::MAX).await;
    assert!(fetch_fails(&ada, &bearer, &hash).await, "image GC'd");
    assert!(
        fetch_fails(&ada, &bearer, &thumb_hash).await,
        "thumbnail GC'd"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn media_attach_cap_gates_a_restricted_channel() {
    // ada is an operator (holds every cap, incl. `attach`).
    let server = start_with(&["#locked"], |c| c.operators = vec!["ada".to_string()]).await;
    let mut ada = QuicClient::connect(server.quic_addr).await;
    ada.ready("ada").await;
    ada.join("#locked").await;

    // bob must exist before he can be granted a cap.
    let mut bob = QuicClient::connect(server.quic_addr).await;
    bob.ready("bob").await;

    // Restrict posting, then grant bob `send` (so he can post) but NOT `attach`.
    ada.send("CHANNEL META #locked posting :restricted").await;
    ada.recv_until(|r| matches!(r.event, Event::Chanmeta { .. }))
        .await;
    ada.send("GRANT bob #locked send").await;
    ada.recv_until(|r| matches!(r.event, Event::Token { .. }))
        .await;

    // A blob to try to attach.
    let (uri, _) = upload_blob(&mut ada, "text/plain", b"secret.txt").await;

    bob.join("#locked").await;

    // bob can post text (he has `send`)…
    bob.send("@label=t1 MSG #locked :hello").await;
    bob.recv_until(|r| r.label.as_deref() == Some("t1")).await;
    // …but attaching requires `attach`, which he lacks → CAP-REQUIRED.
    bob.send(&format!("@label=t2;attach.1={uri} MSG #locked :file"))
        .await;
    let reply = bob.recv_until(|r| r.label.as_deref() == Some("t2")).await;
    let Event::Err(err) = &reply.event else {
        panic!("expected ERR, got {reply:?}");
    };
    assert_eq!(err.code, ErrCode::CapRequired);
    assert_eq!(err.context.as_deref(), Some("attach"));

    // ada (operator, holds `attach`) attaches fine.
    ada.send(&format!("@label=z;attach.1={uri} MSG #locked :file"))
        .await;
    let ok = ada.recv_until(|r| r.label.as_deref() == Some("z")).await;
    assert!(matches!(ok.event, Event::Message(_)));

    server.shutdown().await;
}

#[tokio::test]
async fn media_session_bearer_authorizes_http_upload() {
    let server = start_server(&["#general"]).await;
    let http = server.http_addr.expect("http enabled");
    let mut ada = QuicClient::connect(server.quic_addr).await;
    // §13 the per-session fetch bearer is delivered right after auth.
    let bearer = ada.ready("ada").await;
    ada.join("#general").await;

    // Upload authorized by the bearer alone (no OFFER handshake) — the browser
    // path: one POST with a Content-Type.
    let data = b"bearer-authorized upload".to_vec();
    let head = format!(
        "POST /media?t={bearer} HTTP/1.1\r\nHost: x\r\nContent-Type: text/plain\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        data.len()
    );
    let (status, body) = http_request(http, &head, &data).await;
    assert_eq!(status, 200, "upload: {}", String::from_utf8_lossy(&body));
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let uri = json["media"].as_str().unwrap().to_string();
    let hash = json["hash"].as_str().unwrap().to_string();

    // Post + fetch with the same bearer (the round-trip the browser client does).
    ada.send(&format!("@attach.1={uri} MSG #general :file"))
        .await;
    assert!(matches!(ada.recv().await.event, Event::Message(_)));
    let (status, got) = http_get(http, &format!("/media/{hash}?t={bearer}"), None).await;
    assert_eq!(status, 200);
    assert_eq!(got, data);

    server.shutdown().await;
}

/// A data-plane GET that is expected to be refused (`ERR …`).
async fn fetch_fails(client: &QuicClient, bearer: &str, hash: &str) -> bool {
    let (mut send, mut recv) = client._connection.open_bi().await.unwrap();
    send.write_all(format!("GET {bearer} {hash}\n").as_bytes())
        .await
        .unwrap();
    let _ = send.finish();
    let resp = recv.read_to_end(1024).await.unwrap();
    String::from_utf8_lossy(&resp).starts_with("ERR")
}

/// Minimal raw HTTP/1.1 client (Connection: close ⇒ read to EOF) for the media
/// endpoints — avoids a heavy HTTP client dep in the test.
async fn http_request(addr: SocketAddr, head: &str, body: &[u8]) -> (u16, Vec<u8>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    sock.write_all(head.as_bytes()).await.unwrap();
    if !body.is_empty() {
        sock.write_all(body).await.unwrap();
    }
    let mut buf = Vec::new();
    sock.read_to_end(&mut buf).await.unwrap();
    let pos = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("http response");
    let head = String::from_utf8_lossy(&buf[..pos]).into_owned();
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .expect("http status");
    (status, buf[pos + 4..].to_vec())
}

async fn http_post(addr: SocketAddr, path: &str, body: &[u8]) -> (u16, Vec<u8>) {
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    http_request(addr, &head, body).await
}

async fn http_get(addr: SocketAddr, path: &str, range: Option<&str>) -> (u16, Vec<u8>) {
    let range_hdr = range.map(|r| format!("Range: {r}\r\n")).unwrap_or_default();
    let head =
        format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n{range_hdr}Connection: close\r\n\r\n");
    http_request(addr, &head, &[]).await
}

// ---- WebSocket fallback ----

struct WsClient {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

impl WsClient {
    async fn connect(addr: SocketAddr) -> Self {
        Self::connect_url(format!("ws://{addr}")).await
    }

    async fn connect_url(url: String) -> Self {
        let (ws, _) = tokio_tungstenite::connect_async(url)
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
    assert!(matches!(
        client.recv().await.event,
        Event::MediaToken { .. }
    ));
    client.send("JOIN #general").await;
    assert!(matches!(client.recv().await.event, Event::Member { .. }));
    assert!(matches!(client.recv().await.event, Event::Policy { .. }));

    client.send("@label=w1 MSG #general :over websocket").await;
    let echo = client.recv().await;
    assert_eq!(echo.label.as_deref(), Some("w1"));
    assert!(matches!(&echo.event, Event::Message(m) if m.body == "over websocket"));

    server.shutdown().await;
}

/// P3 web embed: the same-origin `/ws` route on the HTTP listener bridges into
/// the ordinary session path, so a browser served from `https://host/` speaks
/// WEFT back to `wss://host/ws` — no separate `[listen] ws` port needed.
#[tokio::test]
async fn same_origin_ws_route_speaks_the_protocol() {
    let server = start_with(&["#general"], |c| c.listen.web = true).await;
    let http = server.http_addr.expect("http enabled");
    let mut client = WsClient::connect_url(format!("ws://{http}/ws")).await;

    client.send("HELLO weft/1").await;
    assert!(matches!(client.recv().await.event, Event::Welcome { .. }));
    client.send(&format!("REGISTER ada :{PASSWORD}")).await;
    assert!(matches!(client.recv().await.event, Event::Welcome { .. }));
    assert!(matches!(
        client.recv().await.event,
        Event::MediaToken { .. }
    ));
    client.send("JOIN #general").await;
    assert!(matches!(client.recv().await.event, Event::Member { .. }));
    assert!(matches!(client.recv().await.event, Event::Policy { .. }));

    client.send("@label=o1 MSG #general :same-origin ws").await;
    let echo = client.recv().await;
    assert_eq!(echo.label.as_deref(), Some("o1"));
    assert!(matches!(&echo.event, Event::Message(m) if m.body == "same-origin ws"));

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
    ws.recv().await; // MEDIA TOKEN (§13)
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

    // Key-authed session is READY — consume the §13 media bearer, then join.
    assert!(matches!(
        second.recv().await.event,
        Event::MediaToken { .. }
    ));
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
    ada2.recv().await; // WELCOME
    ada2.recv().await; // §13 MEDIA TOKEN

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

#[tokio::test]
async fn report_file_list_resolve_over_quic() {
    // §6.7 end-to-end over real QUIC: file → operator queue → resolve, with
    // the reporter kept blind to the handler on resolution (invariant 12).
    let server = start_with(&["#general"], |config| {
        config.operators = vec!["op".to_string()];
    })
    .await;
    let mut ada = QuicClient::connect(server.quic_addr).await;
    ada.ready("ada").await;
    ada.join("#general").await;
    ada.send("MSG #general :abuse").await;
    let Event::Message(msg) = ada.recv().await.event else {
        panic!("expected MESSAGE echo")
    };
    let mid = msg.msgid.to_string();

    ada.send(&format!("@label=r1 REPORT {mid} harassment"))
        .await;
    let ack = ada.recv().await;
    let Event::Reported { report_id } = ack.event else {
        panic!("expected REPORTED, got {ack:?}")
    };

    // A default ns-scope report on a top-level channel routes to the
    // operator (`*`) — an operator lists and resolves it.
    let mut op = QuicClient::connect(server.quic_addr).await;
    op.ready("op").await;
    op.send("REPORTS LIST *").await;
    let filed = op.recv().await;
    assert!(
        matches!(&filed.event, Event::ReportFiled { report_id: fid, .. } if *fid == report_id),
        "operator queue should hold the report, got {filed:?}"
    );

    op.send(&format!("REPORTS RESOLVE {report_id} dismissed"))
        .await;
    let echo = op.recv().await;
    assert!(matches!(
        &echo.event,
        Event::ReportResolved { by: Some(_), .. }
    ));

    // The reporter is pushed the minimal resolution (no handler identity).
    let push = ada.recv().await;
    assert!(
        matches!(
            &push.event,
            Event::ReportResolved {
                by: None,
                note: None,
                ..
            }
        ),
        "reporter must get the minimal form, got {push:?}"
    );

    server.shutdown().await;
}

// ---- §17 WEFT-IRC gateway ----

/// A raw IRC client over a real TCP socket.
struct IrcClient {
    reader: tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: tokio::net::tcp::OwnedWriteHalf,
}

impl IrcClient {
    async fn connect(addr: SocketAddr) -> Self {
        let (r, w) = tokio::net::TcpStream::connect(addr)
            .await
            .unwrap()
            .into_split();
        Self {
            reader: tokio::io::BufReader::new(r),
            writer: w,
        }
    }

    async fn send(&mut self, line: &str) {
        use tokio::io::AsyncWriteExt;
        self.writer.write_all(line.as_bytes()).await.unwrap();
        self.writer.write_all(b"\r\n").await.unwrap();
    }

    async fn recv(&mut self) -> String {
        use tokio::io::AsyncBufReadExt;
        let mut buf = String::new();
        tokio::time::timeout(Duration::from_secs(5), self.reader.read_line(&mut buf))
            .await
            .expect("timed out waiting for an IRC line")
            .expect("IRC read error");
        buf.trim_end_matches(['\r', '\n']).to_string()
    }

    /// Read until a line containing `needle`.
    async fn recv_until(&mut self, needle: &str) -> String {
        loop {
            let line = self.recv().await;
            if line.contains(needle) {
                return line;
            }
        }
    }

    async fn register(&mut self, nick: &str) {
        self.send(&format!("NICK {nick}")).await;
        self.send(&format!("USER {nick} 0 * :{nick} tester")).await;
        self.recv_until(" 001 ").await; // RPL_WELCOME → registered
    }
}

#[tokio::test]
async fn irc_gateway_register_join_namespace_and_chat() {
    // A namespaced channel is seeded so it exists to be JOINed (§17: `JOIN
    // #ns/chan` valid natively) — this is the "namespaces can be joined" path.
    let server = start_with(&["#general", "#gaming/general"], |config| {
        config.listen.irc = Some("127.0.0.1:0".parse().unwrap());
    })
    .await;
    let irc_addr = server.irc_addr.expect("IRC gateway enabled");

    // ada registers over IRC and joins the namespaced channel.
    let mut ada = IrcClient::connect(irc_addr).await;
    ada.register("ada").await;
    ada.send("JOIN #gaming/general").await;
    let joined = ada.recv_until("JOIN").await;
    assert!(
        joined.contains(":ada!ada@test.example JOIN #gaming/general"),
        "own JOIN echo, got {joined:?}"
    );
    ada.recv_until(" 366 ").await; // end of NAMES

    // bob registers and joins the same namespaced channel; ada sees him.
    let mut bob = IrcClient::connect(irc_addr).await;
    bob.register("bob").await;
    bob.send("JOIN #gaming/general").await;
    let seen = ada.recv_until("bob").await;
    assert!(
        seen.contains(":bob!bob@test.example JOIN #gaming/general"),
        "ada should see bob join, got {seen:?}"
    );

    // bob speaks; ada receives it as a PRIVMSG (bob's own echo is suppressed).
    bob.send("PRIVMSG #gaming/general :hello from irc").await;
    let msg = ada.recv_until("PRIVMSG").await;
    assert_eq!(
        msg,
        ":bob!bob@test.example PRIVMSG #gaming/general :hello from irc"
    );

    server.shutdown().await;
}

// ---- §6.7 moderation ----

#[tokio::test]
async fn moderation_mute_refuses_send_over_quic() {
    let server = start_with(&["#general"], |config| {
        config.operators = vec!["op".to_string()];
    })
    .await;
    let mut bob = QuicClient::connect(server.quic_addr).await;
    bob.ready("bob").await;
    bob.join("#general").await;

    // An operator (global moderator) mutes bob.
    let mut op = QuicClient::connect(server.quic_addr).await;
    op.ready("op").await;
    op.send("@label=m MUTE #general bob :spamming").await;
    let reply = op.recv().await;
    assert!(
        matches!(&reply.event, Event::Moderated { action, .. } if *action == weft_proto::ModAction::Mute),
        "moderator gets MODERATED, got {reply:?}"
    );

    // bob's next message is refused with FORBIDDEN muted.
    bob.send("MSG #general :hello").await;
    let err = bob.recv().await;
    assert!(
        matches!(&err.event, Event::Err(e) if e.code == ErrCode::Forbidden && e.context.as_deref() == Some("muted")),
        "muted send must be refused, got {err:?}"
    );

    server.shutdown().await;
}

// ---- M5d outbound dialer (auto-federation P1) ----

/// The outbound bridge dialer completes the §11.2 AUTH BRIDGE handshake against
/// a real inbound weftd — two servers actually authenticating over QUIC.
#[tokio::test]
async fn outbound_bridge_dial_authenticates() {
    // F: accepts any non-blocked bridge peer (trust-on-first-use, §11.2).
    let server = start_with(&["#general"], |c| c.federation.accept_any = true).await;
    let peer: weft_proto::NetworkName = "test.example".parse().unwrap(); // F's name
    let home: weft_proto::NetworkName = "home.example".parse().unwrap();
    let identity = weft_core::Keypair::generate();
    let endpoint = weft_transport::insecure::client_endpoint(weft_transport::ALPN).unwrap();

    let link =
        weftd::dialer::dial_bridge(&endpoint, server.quic_addr, &peer, &identity, &home).await;
    assert!(
        link.is_ok(),
        "outbound bridge auth should succeed: {:?}",
        link.err()
    );
    server.shutdown().await;
}

/// Pinned-only (default): a peer whose key isn't pinned is refused — the proof
/// verifies but the key was never resolved, so it funnels to AUTH-FAILED.
#[tokio::test]
async fn outbound_bridge_dial_rejected_when_unpinned() {
    let server = start_server(&["#general"]).await; // default: pinned-only, no peers
    let peer: weft_proto::NetworkName = "test.example".parse().unwrap();
    let home: weft_proto::NetworkName = "home.example".parse().unwrap();
    let identity = weft_core::Keypair::generate();
    let endpoint = weft_transport::insecure::client_endpoint(weft_transport::ALPN).unwrap();

    let link =
        weftd::dialer::dial_bridge(&endpoint, server.quic_addr, &peer, &identity, &home).await;
    assert!(link.is_err(), "unpinned bridge auth must be rejected");
    server.shutdown().await;
}

/// End-to-end (P1b + P1c): H dials F, transmits its stored `BRIDGE PROPOSE`, F
/// auto-accepts, and a message posted on H's bridged channel forwards one hop to
/// a member on F. Two live weftds federating for real over QUIC.
#[tokio::test]
async fn outbound_bridge_forwards_messages_end_to_end() {
    // H's network key, persisted so H boots with it AND we can dial as it.
    let home_key = weft_core::Keypair::generate();
    let key_path = std::env::temp_dir().join("weft-p1bc-home.key");
    std::fs::write(&key_path, home_key.seed_b64()).unwrap();

    // F: accepts + auto-accepts bridges; hosts #general.
    let f = start_with(&["#general"], |c| {
        c.federation.accept_any = true;
        c.federation.auto_accept = true;
    })
    .await;
    let f_net: weft_proto::NetworkName = "test.example".parse().unwrap();
    let f_key = f.ctx().network_public();

    // H: a *different* network, booted with the persisted key + an operator.
    let h = start_with(&["#general"], |c| {
        c.network = "home.example".to_string();
        c.operators = vec!["admin".to_string()];
        c.identity.key_file = Some(key_path.clone());
    })
    .await;
    let h_net: weft_proto::NetworkName = "home.example".parse().unwrap();

    // Members watching #general on each side.
    let mut bob = QuicClient::connect(f.quic_addr).await;
    bob.ready("bob").await;
    bob.join("#general").await;
    let mut ada = QuicClient::connect(h.quic_addr).await;
    ada.ready("ada").await;
    ada.join("#general").await;

    // The operator on H proposes bridging #general to F (compiles + stores it).
    let mut admin = QuicClient::connect(h.quic_addr).await;
    admin.ready("admin").await;
    admin.send("BRIDGE PROPOSE #general test.example").await;
    assert!(matches!(admin.recv().await.event, Event::Manifest { .. }));

    // H dials F and runs the outbound bridge: transmit proposal → F accepts →
    // both sides start forwarders.
    let endpoint = weft_transport::insecure::client_endpoint(weft_transport::ALPN).unwrap();
    let ctx = h.ctx().clone();
    let f_addr = f.quic_addr;
    let bridge = tokio::spawn(async move {
        weftd::dialer::run_peer_bridge(&endpoint, f_addr, &f_net, f_key, &home_key, &h_net, ctx)
            .await
    });

    // Both sides announce the live bridge to their #general members. ada's
    // announce means H's forwarders are subscribed — safe to post now.
    assert!(
        matches!(bob.recv().await.event, Event::Manifest { .. }),
        "F announces the bridge"
    );
    assert!(
        matches!(ada.recv().await.event, Event::Manifest { .. }),
        "H announces the bridge"
    );

    // A message on H's #general forwards one hop to F.
    ada.send("MSG #general :hi from H").await;
    let msg = bob.recv().await;
    match &msg.event {
        Event::Message(m) => assert!(m.body.contains("hi from H"), "forwarded body: {:?}", m.body),
        other => panic!("expected forwarded MESSAGE on F, got {other:?}"),
    }

    bridge.abort();
    f.shutdown().await;
    h.shutdown().await;
    let _ = std::fs::remove_file(&key_path);
}

/// P3: on-demand auto-federation. H *requests* F's reachable namespace over a
/// freshly-dialed bridge (`BRIDGE REQUEST`), F offers its signed manifest, H
/// auto-accepts — the bridge goes live with no operator ceremony on H.
#[tokio::test]
async fn auto_bridge_requests_reachable_namespace() {
    let home_key = weft_core::Keypair::generate();
    let key_path = std::env::temp_dir().join("weft-p3-home.key");
    std::fs::write(&key_path, home_key.seed_b64()).unwrap();

    // F hosts #gaming/general and accepts bridge peers.
    let f = start_with(&["#gaming/general"], |c| c.federation.accept_any = true).await;
    let f_net: weft_proto::NetworkName = "test.example".parse().unwrap();
    let f_key = f.ctx().network_public();

    // Make `gaming` auto-federation-reachable: public + federation open.
    let ns_root = weft_core::Keypair::generate();
    let mut ada = QuicClient::connect(f.quic_addr).await;
    ada.ready("ada").await;
    ada.send(&format!(
        "@root={} NS CREATE gaming public",
        ns_root.public().to_b64()
    ))
    .await;
    assert!(matches!(ada.recv().await.event, Event::NsMeta { .. }));
    ada.send("NS META gaming federation :open").await;
    assert!(matches!(ada.recv().await.event, Event::NsMeta { .. }));

    // A member of the namespace channel on F.
    let mut bob = QuicClient::connect(f.quic_addr).await;
    bob.ready("bob").await;
    bob.join("#gaming/general").await;

    // H: a different network that dials + requests `gaming`.
    let h = start_with(&["#gaming/general"], |c| {
        c.network = "home.example".to_string();
        c.identity.key_file = Some(key_path.clone());
    })
    .await;
    let h_net: weft_proto::NetworkName = "home.example".parse().unwrap();

    let endpoint = weft_transport::insecure::client_endpoint(weft_transport::ALPN).unwrap();
    let ctx = h.ctx().clone();
    let f_addr = f.quic_addr;
    // Loopback dodges the SSRF guard (unit-tested separately) — drive the core.
    let bridge = tokio::spawn(async move {
        weftd::dialer::run_peer_requester(
            &endpoint,
            f_addr,
            &f_net,
            f_key,
            "gaming".parse().unwrap(),
            &home_key,
            &h_net,
            ctx,
        )
        .await
    });

    // F announces the live bridge to the namespace's members → bob sees it.
    assert!(
        matches!(bob.recv().await.event, Event::Manifest { .. }),
        "the requested bridge should go live"
    );

    bridge.abort();
    f.shutdown().await;
    h.shutdown().await;
    let _ = std::fs::remove_file(&key_path);
}

#[tokio::test]
async fn graceful_shutdown_drains_within_the_window() {
    let server = start_server(&["#general"]).await;
    // An active session (+ the HTTP/WS servers and maintenance task from
    // start_with) must all react to the shutdown signal. A task that ignored it
    // would hold the drain until the internal 10s grace window elapses.
    let mut ada = QuicClient::connect(server.quic_addr).await;
    ada.ready("ada").await;

    tokio::time::timeout(std::time::Duration::from_secs(9), server.shutdown())
        .await
        .expect("graceful shutdown drained well within the grace window");
}
