//! Conformance: black-box protocol tests against a real in-process weftd —
//! genuine QUIC (ALPN `weft/1`) and WebSocket connections on ephemeral
//! ports (CLAUDE.md M1: `tests/conformance/`).

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use livekit_api::access_token::TokenVerifier;
use tokio_tungstenite::tungstenite::Message;
use weft_proto::{ErrCode, Event, Reply, VoiceTransport};
use weft_transport::insecure::client_endpoint;
use weft_transport::QuicControlStream;
use weftd::config::{ChannelConfig, Config, Identity, Listen, LiveKit, VoiceBackendKind};

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

    /// One reply, verbatim (no filtering).
    async fn recv_reply(&mut self) -> Reply {
        let line = tokio::time::timeout(Duration::from_secs(5), self.stream.recv_line())
            .await
            .expect("timed out")
            .expect("recv")
            .expect("stream closed");
        Reply::parse(&line).expect("unparseable server line")
    }

    /// A reply, skipping server-generated system messages (join/part lines) so
    /// they don't perturb flows that don't assert on them.
    async fn recv(&mut self) -> Reply {
        loop {
            let reply = self.recv_reply().await;
            if matches!(&reply.event, Event::Message(m) if m.meta.system.is_some()) {
                continue;
            }
            return reply;
        }
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

    /// §6/§13 pull a backfill batch on a data-plane bidi stream, returning the
    /// serialized `Reply` lines (`OK <len>\n<body>`).
    async fn backfill_pull(&self, token: &str) -> Vec<u8> {
        let (mut send, mut recv) = self._connection.open_bi().await.expect("open data stream");
        send.write_all(format!("BACKFILL {token}\n").as_bytes())
            .await
            .unwrap();
        let _ = send.finish();
        let resp = recv
            .read_to_end(600 * 1024 * 1024)
            .await
            .expect("backfill response");
        let nl = resp
            .iter()
            .position(|&b| b == b'\n')
            .expect("response header");
        let header = String::from_utf8_lossy(&resp[..nl]).into_owned();
        assert!(header.starts_with("OK "), "backfill failed: {header}");
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
    let removed = weft_core::gc_orphan_blobs(
        &server.ctx().media_refs,
        &server.ctx().blobs,
        &server.ctx().profiles,
        u64::MAX,
    )
    .await;
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

/// §6/§13 M-media-4: a HISTORY page over the stream threshold is served as a
/// data-plane stream (`STREAM ACCEPT` → `BACKFILL`) instead of hundreds of
/// inline `BATCH` lines — proven over **both** QUIC and HTTP. The pulled body is
/// the serialized batch the client folds exactly like an inline `BATCH`.
#[tokio::test]
async fn large_scrollback_transfers_as_a_backfill_stream() {
    let server = start_server(&["#general"]).await;
    let http = server.http_addr.expect("http enabled");
    let mut ada = QuicClient::connect(server.quic_addr).await;
    ada.ready("ada").await;
    ada.join("#general").await;

    // Post one message past the stream threshold.
    let n = weft_proto::HISTORY_STREAM_THRESHOLD + 1;
    for i in 0..n {
        ada.send(&format!("MSG #general :m{i}")).await;
        assert!(matches!(ada.recv().await.event, Event::Message(_)));
    }

    // Each HISTORY over the threshold yields a one-time stream token.
    async fn stream_token(ada: &mut QuicClient) -> String {
        ada.send("HISTORY #general limit=500").await;
        match ada.recv().await.event {
            Event::StreamAccept { token } => token,
            other => panic!("expected STREAM ACCEPT for a large page, got {other:?}"),
        }
    }

    let quic_token = stream_token(&mut ada).await;
    let quic_body = ada.backfill_pull(&quic_token).await;

    let http_token = stream_token(&mut ada).await;
    let (status, http_body) = http_get(http, &format!("/backfill?t={http_token}"), None).await;
    assert_eq!(status, 200, "HTTP backfill pull");

    // Both transports carry the same complete, foldable batch.
    for body in [quic_body, http_body] {
        let text = String::from_utf8(body).expect("utf-8 batch");
        let events: Vec<Event> = text
            .lines()
            .map(|l| Reply::parse(l).expect("parseable batch line").event)
            .collect();
        assert!(matches!(events.first(), Some(Event::BatchStart { .. })));
        assert!(matches!(events.last(), Some(Event::BatchEnd { .. })));
        let bodies: std::collections::HashSet<String> = events
            .iter()
            .filter_map(|e| match e {
                Event::Message(m) => Some(m.body.clone()),
                _ => None,
            })
            .collect();
        for i in 0..n {
            assert!(
                bodies.contains(&format!("m{i}")),
                "m{i} missing from stream"
            );
        }
    }

    // One-time: re-pulling a spent token is uniformly "not found" (invariant 1).
    assert_eq!(
        http_get(http, &format!("/backfill?t={http_token}"), None)
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
    weft_core::gc_orphan_blobs(
        &server.ctx().media_refs,
        &server.ctx().blobs,
        &server.ctx().profiles,
        u64::MAX,
    )
    .await;
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

/// §13 M-media-5: MEDIA BLOCK deletes a blob and makes it dead on arrival —
/// a member's fetch 404s and a re-upload of the identical bytes is rejected
/// (content = identity, so re-uploads can't evade). The same `is_blob_blocked`
/// gate guards the mirror path (§11.8).
#[tokio::test]
async fn media_block_deletes_and_rejects_reupload() {
    let server = start_with(&["#general"], |c| c.operators = vec!["admin".to_string()]).await;
    let http = server.http_addr.expect("http enabled");

    let mut ada = QuicClient::connect(server.quic_addr).await;
    let bearer = ada.ready("ada").await;
    ada.join("#general").await;

    // Upload + post an attachment; a member can fetch it.
    let data = b"blockable media bytes".to_vec();
    let (uri, hash) = upload_blob(&mut ada, "text/plain", &data).await;
    ada.send(&format!("@attach.1={uri} MSG #general :look"))
        .await;
    ada.recv_until(|r| matches!(&r.event, Event::Message(m) if m.body.contains("look")))
        .await;
    assert_eq!(
        http_get(http, &format!("/media/{hash}?t={bearer}"), None)
            .await
            .0,
        200,
        "member fetch works before the block"
    );

    // An operator blocks the hash.
    let mut admin = QuicClient::connect(server.quic_addr).await;
    admin.ready("admin").await;
    admin.send(&format!("MEDIA BLOCK {hash} :csam")).await;
    assert!(matches!(
        admin.recv().await.event,
        Event::MediaBlocked { .. }
    ));

    // Fetch now 404 (bytes deleted + gated, uniform with absent — invariant 1).
    assert_eq!(
        http_get(http, &format!("/media/{hash}?t={bearer}"), None)
            .await
            .0,
        404,
        "blocked blob is dead on arrival"
    );

    // Re-uploading the identical bytes is rejected (403) — content is identity.
    assert_eq!(
        http_post(http, &format!("/media?t={bearer}"), &data)
            .await
            .0,
        403,
        "re-upload of a blocked hash is rejected"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn membership_join_line_is_a_persistent_system_message() {
    let server = start_server(&["#general"]).await;

    // ada joins — the actor emits + stores a `system=join` message. A live
    // observer would see it (skipped by our recv); here we prove it PERSISTS.
    let mut ada = QuicClient::connect(server.quic_addr).await;
    ada.ready("ada").await;
    ada.join("#general").await;
    ada.send("@label=m MSG #general :hi").await; // ensure the join was processed
    ada.recv_until(|r| r.label.as_deref() == Some("m")).await;

    // A fresh member pulls HISTORY and finds ada's join line durably recorded.
    let mut cara = QuicClient::connect(server.quic_addr).await;
    cara.ready("cara").await;
    cara.join("#general").await;
    cara.send("HISTORY #general").await;
    let mut found = false;
    loop {
        let reply = cara.recv_reply().await; // raw: don't skip system messages
        match &reply.event {
            Event::Message(m)
                if m.meta.system.as_deref() == Some("join")
                    && m.sender.account.as_str() == "ada" =>
            {
                found = true;
            }
            Event::BatchEnd { .. } => break,
            _ => {}
        }
    }
    assert!(
        found,
        "ada's join must persist as a system message in HISTORY"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn reconnect_does_not_repost_the_join_line() {
    let server = start_server(&["#general"]).await;

    // ada's genuine first join posts one system "join".
    let mut ada = QuicClient::connect(server.quic_addr).await;
    ada.ready("ada").await;
    ada.join("#general").await;
    ada.send("@label=a MSG #general :hi").await;
    ada.recv_until(|r| r.label.as_deref() == Some("a")).await;

    // Simulate a client reload: drop the connection, then reconnect the same
    // account. welcome_authed auto-rejoins #general (membership persists) — which
    // must NOT post a second join line.
    drop(ada);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut ada2 = QuicClient::connect(server.quic_addr).await;
    ada2.send("HELLO weft/1").await;
    assert!(matches!(ada2.recv().await.event, Event::Welcome { .. }));
    ada2.send(&format!("AUTH PASSWORD ada :{PASSWORD}")).await;
    assert!(matches!(ada2.recv().await.event, Event::Welcome { .. }));
    // Drain the auto-rejoin traffic by waiting for our own marker's echo.
    ada2.send("@label=r MSG #general :back").await;
    ada2.recv_until(|r| r.label.as_deref() == Some("r")).await;

    // A fresh member counts ada's join lines in HISTORY — exactly one.
    let mut cara = QuicClient::connect(server.quic_addr).await;
    cara.ready("cara").await;
    cara.join("#general").await;
    cara.send("HISTORY #general").await;
    let mut joins = 0;
    loop {
        let reply = cara.recv_reply().await;
        match &reply.event {
            Event::Message(m)
                if m.meta.system.as_deref() == Some("join")
                    && m.sender.account.as_str() == "ada" =>
            {
                joins += 1;
            }
            Event::BatchEnd { .. } => break,
            _ => {}
        }
    }
    assert_eq!(
        joins, 1,
        "reconnect/auto-rejoin must not repost the join line"
    );

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
                let reply = Reply::parse(&line).expect("unparseable server line");
                // Skip server-generated system messages (join/part lines).
                if matches!(&reply.event, Event::Message(m) if m.meta.system.is_some()) {
                    continue;
                }
                return reply;
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

/// WC7: a suspended account can't authenticate — uniform AUTH-FAILED at the
/// session chokepoint — and unsuspending restores access.
#[tokio::test]
async fn suspended_account_cannot_authenticate() {
    let server = start_server(&["#general"]).await;

    // Register ada.
    let mut c = QuicClient::connect(server.quic_addr).await;
    c.send("HELLO weft/1").await;
    c.recv().await;
    c.send(&format!("REGISTER ada :{PASSWORD}")).await;
    assert!(matches!(c.recv().await.event, Event::Welcome { .. }));

    // Suspend her via the server context.
    let ada: weft_proto::Account = "ada".parse().unwrap();
    server
        .ctx()
        .accounts
        .set_suspended(&ada, true)
        .await
        .unwrap();

    // A fresh AUTH PASSWORD is now rejected — same code as bad credentials.
    let mut c2 = QuicClient::connect(server.quic_addr).await;
    c2.send("HELLO weft/1").await;
    c2.recv().await;
    c2.send(&format!("AUTH PASSWORD ada :{PASSWORD}")).await;
    assert!(matches!(&c2.recv().await.event, Event::Err(e) if e.code == ErrCode::AuthFailed));

    // Unsuspend → auth works again.
    server
        .ctx()
        .accounts
        .set_suspended(&ada, false)
        .await
        .unwrap();
    let mut c3 = QuicClient::connect(server.quic_addr).await;
    c3.send("HELLO weft/1").await;
    c3.recv().await;
    c3.send(&format!("AUTH PASSWORD ada :{PASSWORD}")).await;
    assert!(matches!(c3.recv().await.event, Event::Welcome { .. }));

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
                kind: None,
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
        weftd::dialer::run_peer_bridge(
            &endpoint,
            f_addr,
            &f_net,
            f_key,
            &home_key,
            &h_net,
            ctx,
            weftd::dialer::PeerLinks::new(),
        )
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

    // A member of the namespace channel on F, with some pre-bridge history.
    let mut bob = QuicClient::connect(f.quic_addr).await;
    bob.ready("bob").await;
    bob.join("#gaming/general").await;
    bob.send("MSG #gaming/general :pre-bridge history").await;
    assert!(matches!(bob.recv().await.event, Event::Message(_)));

    // H: a different network that dials + requests `gaming`.
    let h = start_with(&["#gaming/general"], |c| {
        c.network = "home.example".to_string();
        c.identity.key_file = Some(key_path.clone());
    })
    .await;
    let h_net: weft_proto::NetworkName = "home.example".parse().unwrap();

    // A member on H, present as the auto-bridge comes up.
    let mut ada = QuicClient::connect(h.quic_addr).await;
    ada.ready("ada").await;
    ada.join("#gaming/general").await;

    let endpoint = weft_transport::insecure::client_endpoint(weft_transport::ALPN).unwrap();
    let ctx = h.ctx().clone();
    let f_addr = f.quic_addr;
    // Register in H's own PeerLinks so its backfill consumer pulls over this
    // connection (auto-federation offers `history=full`, §11.10).
    let links = h.peer_links();
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
            links,
        )
        .await
    });

    // F announces the live bridge to the namespace's members → bob sees it.
    assert!(
        matches!(bob.recv().await.event, Event::Manifest { .. }),
        "the requested bridge should go live"
    );

    // Bridge live → wait for H's manifest announce to ada, then she asks for
    // history. `history=full` auto-federation means the on-demand pull reaches
    // F's *pre-bridge* scrollback, so bob's earlier message lands on H.
    ada.recv_until(|r| matches!(r.event, Event::Manifest { .. }))
        .await;
    ada.send("HISTORY #gaming/general limit=500").await;
    let seen = ada
        .recv_until(|r| {
            matches!(&r.event, Event::Message(m)
                if m.msgid.origin().as_str() == "test.example"
                    && m.body.contains("pre-bridge history"))
        })
        .await;
    assert!(matches!(&seen.event, Event::Message(_)));

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

/// §11.8 federation media mirroring end-to-end over two live weftds: F posts an
/// image on a bridged channel; H ingests the message with F's `weft-media://`
/// URI intact, then pulls the blob back over the bridge (a signed `MIRROR`) and
/// stores it — so a member on H fetches the media from *H*, never touching F.
#[tokio::test]
async fn federated_media_mirrors_over_the_bridge() {
    // Persisted keys so both boot with a known identity we can pin mutually.
    let f_kp = weft_core::Keypair::generate();
    let h_kp = weft_core::Keypair::generate();
    let f_key_path = std::env::temp_dir().join("weft-mirror-f.key");
    let h_key_path = std::env::temp_dir().join("weft-mirror-h.key");
    std::fs::write(&f_key_path, f_kp.seed_b64()).unwrap();
    std::fs::write(&h_key_path, h_kp.seed_b64()).unwrap();

    // F (origin, test.example): hosts #general, pins H (to verify H's MIRROR
    // pulls + accept its bridge), auto-accepts the proposal. It never dials H.
    let f = start_with(&["#general"], |c| {
        c.identity.key_file = Some(f_key_path.clone());
        c.federation.auto_accept = true;
        c.peers = vec![weftd::config::Peer {
            network: "home.example".to_string(),
            // Unresolvable (`.invalid` never resolves): F pins H's key but never
            // actually dials it — the bridge is H→F, and F only needs the key.
            endpoint: "h.invalid:1".to_string(),
            key: h_kp.public().to_b64(),
        }];
    })
    .await;
    let f_net: weft_proto::NetworkName = "test.example".parse().unwrap();

    // H (receiver, home.example): pins F, boots with its key + an operator.
    let h = start_with(&["#general"], |c| {
        c.network = "home.example".to_string();
        c.identity.key_file = Some(h_key_path.clone());
        c.operators = vec!["admin".to_string()];
        c.peers = vec![weftd::config::Peer {
            network: "test.example".to_string(),
            endpoint: f.quic_addr.to_string(),
            key: f_kp.public().to_b64(),
        }];
    })
    .await;
    let h_net: weft_proto::NetworkName = "home.example".parse().unwrap();

    // Members watching #general on each side.
    let mut bob = QuicClient::connect(f.quic_addr).await; // origin poster
    bob.ready("bob").await;
    bob.join("#general").await;
    let mut ada = QuicClient::connect(h.quic_addr).await; // receiver member
    ada.ready("ada").await;
    ada.join("#general").await;

    // admin on H proposes bridging #general to F (compiles + stores it).
    let mut admin = QuicClient::connect(h.quic_addr).await;
    admin.ready("admin").await;
    admin.send("BRIDGE PROPOSE #general test.example").await;
    assert!(matches!(admin.recv().await.event, Event::Manifest { .. }));

    // Drive H's outbound bridge by hand, but register it in H's *own* PeerLinks
    // so the in-process mirror consumer pulls over this very connection.
    let endpoint = weft_transport::insecure::client_endpoint(weft_transport::ALPN).unwrap();
    let ctx = h.ctx().clone();
    let f_addr = f.quic_addr;
    let links = h.peer_links();
    let bridge = tokio::spawn(async move {
        weftd::dialer::run_peer_bridge(
            &endpoint,
            f_addr,
            &f_net,
            f_kp.public(),
            &h_kp,
            &h_net,
            ctx,
            links,
        )
        .await
    });

    // Both sides announce the live bridge to #general members.
    bob.recv_until(|r| matches!(r.event, Event::Manifest { .. }))
        .await;
    ada.recv_until(|r| matches!(r.event, Event::Manifest { .. }))
        .await;

    // F uploads a real image + posts it as an attachment on #general.
    let img = image::RgbImage::from_fn(80, 60, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 7])
    });
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let png = png.into_inner();
    let (uri, hash) = upload_blob(&mut bob, "image/png", &png).await;
    assert!(
        uri.starts_with("weft-media://test.example/"),
        "origin uri: {uri}"
    );
    bob.send(&format!("@attach.1={uri} MSG #general :from F"))
        .await;
    bob.recv_until(|r| matches!(&r.event, Event::Message(m) if m.body.contains("from F")))
        .await;

    // H ingests the forwarded message with the *foreign* attachment intact.
    let msg = ada
        .recv_until(|r| matches!(&r.event, Event::Message(m) if m.body.contains("from F")))
        .await;
    let Event::Message(m) = &msg.event else {
        unreachable!()
    };
    assert_eq!(
        m.meta.attachments,
        vec![uri.clone()],
        "origin URI preserved"
    );

    // H's mirror consumer pulls the blob from F. Poll H's store until it lands.
    let mut mirrored = false;
    for _ in 0..50 {
        if matches!(h.ctx().media_refs.blob_meta(&hash).await, Ok(Some(_))) {
            mirrored = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(mirrored, "H must mirror F's blob within the timeout");

    // A member on H fetches the mirrored bytes *from H* — never touching F.
    let ada_bearer = h.ctx().mint_media_bearer("ada".parse().unwrap());
    assert_eq!(
        ada.blob_download(&ada_bearer, &hash, None).await,
        png,
        "mirrored bytes match the origin's"
    );

    bridge.abort();
    f.shutdown().await;
    h.shutdown().await;
    let _ = std::fs::remove_file(&f_key_path);
    let _ = std::fs::remove_file(&h_key_path);
}

/// §10.3 federated **display profiles** over two live weftds: bob on F sets his
/// display name + avatar; the home-network-signed profile crosses the bridge, H
/// verifies it against F's key, stores it keyed by `bob@test.example`, and
/// mirrors the avatar blob — so ada on H sees bob's name + avatar, served by H.
#[tokio::test]
async fn federated_profile_and_avatar_over_the_bridge() {
    let f_kp = weft_core::Keypair::generate();
    let h_kp = weft_core::Keypair::generate();
    let f_key_path = std::env::temp_dir().join("weft-prof-f.key");
    let h_key_path = std::env::temp_dir().join("weft-prof-h.key");
    std::fs::write(&f_key_path, f_kp.seed_b64()).unwrap();
    std::fs::write(&h_key_path, h_kp.seed_b64()).unwrap();

    let f = start_with(&["#general"], |c| {
        c.identity.key_file = Some(f_key_path.clone());
        c.federation.auto_accept = true;
        c.peers = vec![weftd::config::Peer {
            network: "home.example".to_string(),
            endpoint: "h.invalid:1".to_string(),
            key: h_kp.public().to_b64(),
        }];
    })
    .await;
    let f_net: weft_proto::NetworkName = "test.example".parse().unwrap();

    let h = start_with(&["#general"], |c| {
        c.network = "home.example".to_string();
        c.identity.key_file = Some(h_key_path.clone());
        c.operators = vec!["admin".to_string()];
        c.peers = vec![weftd::config::Peer {
            network: "test.example".to_string(),
            endpoint: f.quic_addr.to_string(),
            key: f_kp.public().to_b64(),
        }];
    })
    .await;
    let h_net: weft_proto::NetworkName = "home.example".parse().unwrap();

    let mut bob = QuicClient::connect(f.quic_addr).await;
    bob.ready("bob").await;
    bob.join("#general").await;
    let mut ada = QuicClient::connect(h.quic_addr).await;
    ada.ready("ada").await;
    ada.join("#general").await;

    let mut admin = QuicClient::connect(h.quic_addr).await;
    admin.ready("admin").await;
    admin.send("BRIDGE PROPOSE #general test.example").await;
    assert!(matches!(admin.recv().await.event, Event::Manifest { .. }));

    let endpoint = weft_transport::insecure::client_endpoint(weft_transport::ALPN).unwrap();
    let ctx = h.ctx().clone();
    let f_addr = f.quic_addr;
    let links = h.peer_links();
    let bridge = tokio::spawn(async move {
        weftd::dialer::run_peer_bridge(
            &endpoint,
            f_addr,
            &f_net,
            f_kp.public(),
            &h_kp,
            &h_net,
            ctx,
            links,
        )
        .await
    });
    bob.recv_until(|r| matches!(r.event, Event::Manifest { .. }))
        .await;
    ada.recv_until(|r| matches!(r.event, Event::Manifest { .. }))
        .await;

    // bob uploads an avatar + sets his profile (display name with a space).
    let img =
        image::RgbImage::from_fn(48, 48, |x, y| image::Rgb([(x * 5) as u8, (y * 5) as u8, 9]));
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let png = png.into_inner();
    let (_uri, avatar_hash) = upload_blob(&mut bob, "image/png", &png).await;
    bob.send(&format!(
        "@display=Bob\\sF;avatar={avatar_hash} PROFILE SET"
    ))
    .await;
    bob.recv_until(|r| matches!(r.event, Event::Profile { .. }))
        .await; // his own ack

    // H verifies + stores bob's federated profile (keyed by his handle).
    let mut stored = None;
    for _ in 0..50 {
        if let Ok(Some(p)) = h.ctx().profiles.profile("bob@test.example").await {
            stored = Some(p);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let profile = stored.expect("H stores bob's federated profile");
    assert_eq!(profile.display.as_deref(), Some("Bob F"));
    assert_eq!(profile.avatar.as_deref(), Some(avatar_hash.as_str()));

    // The avatar blob mirrors to H (BLAKE3-verified).
    let mut mirrored = false;
    for _ in 0..50 {
        if matches!(
            h.ctx().media_refs.blob_meta(&avatar_hash).await,
            Ok(Some(_))
        ) {
            mirrored = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(mirrored, "H must mirror bob's avatar");

    // ada on H queries bob's profile and gets his name + avatar.
    ada.send("@label=q PROFILES bob@test.example").await;
    let reply = ada
        .recv_until(|r| matches!(r.event, Event::Profile { .. }))
        .await;
    let Event::Profile {
        user,
        display,
        avatar,
    } = &reply.event
    else {
        unreachable!()
    };
    assert_eq!(user.to_string(), "bob@test.example");
    assert_eq!(display.as_deref(), Some("Bob F"));
    assert_eq!(avatar.as_deref(), Some(avatar_hash.as_str()));

    // ada fetches bob's avatar bytes from H (never touching F).
    let ada_bearer = h.ctx().mint_media_bearer("ada".parse().unwrap());
    assert_eq!(
        ada.blob_download(&ada_bearer, &avatar_hash, None).await,
        png
    );

    bridge.abort();
    f.shutdown().await;
    h.shutdown().await;
    let _ = std::fs::remove_file(&f_key_path);
    let _ = std::fs::remove_file(&h_key_path);
}

/// §11.7 M-media-4 federated **backfill over STREAM** across two live weftds: F
/// hosts a large pre-bridge scrollback on #general. When H dials + bridges F, H
/// pulls that scrollback — F serves it as a data-plane stream (the page exceeds
/// the inline threshold), H opens a `BACKFILL` stream over the bridge, ingests
/// every event (origin msgids intact, invariant 2), and a member on H sees F's
/// history delivered locally — never having issued a HISTORY herself.
#[tokio::test]
async fn federated_backfill_streams_over_the_bridge() {
    let f_kp = weft_core::Keypair::generate();
    let h_kp = weft_core::Keypair::generate();
    let f_key_path = std::env::temp_dir().join("weft-backfill-f.key");
    let h_key_path = std::env::temp_dir().join("weft-backfill-h.key");
    std::fs::write(&f_key_path, f_kp.seed_b64()).unwrap();
    std::fs::write(&h_key_path, h_kp.seed_b64()).unwrap();

    // F (origin): hosts #general, pins H (to accept its bridge), auto-accepts.
    let f = start_with(&["#general"], |c| {
        c.identity.key_file = Some(f_key_path.clone());
        c.federation.auto_accept = true;
        c.peers = vec![weftd::config::Peer {
            network: "home.example".to_string(),
            endpoint: "h.invalid:1".to_string(), // F never dials H
            key: h_kp.public().to_b64(),
        }];
    })
    .await;
    let f_net: weft_proto::NetworkName = "test.example".parse().unwrap();

    // H (receiver): pins F, boots with its key + an operator.
    let h = start_with(&["#general"], |c| {
        c.network = "home.example".to_string();
        c.identity.key_file = Some(h_key_path.clone());
        c.operators = vec!["admin".to_string()];
        c.peers = vec![weftd::config::Peer {
            network: "test.example".to_string(),
            endpoint: f.quic_addr.to_string(),
            key: f_kp.public().to_b64(),
        }];
    })
    .await;
    let h_net: weft_proto::NetworkName = "home.example".parse().unwrap();

    // F builds a scrollback that exceeds the inline stream threshold.
    let mut bob = QuicClient::connect(f.quic_addr).await;
    bob.ready("bob").await;
    bob.join("#general").await;
    let n = weft_proto::HISTORY_STREAM_THRESHOLD + 1;
    for i in 0..n {
        bob.send(&format!("MSG #general :old{i}")).await;
        assert!(matches!(bob.recv().await.event, Event::Message(_)));
    }

    // A member on H, present before the bridge goes live.
    let mut ada = QuicClient::connect(h.quic_addr).await;
    ada.ready("ada").await;
    ada.join("#general").await;

    // admin on H proposes bridging #general to F.
    let mut admin = QuicClient::connect(h.quic_addr).await;
    admin.ready("admin").await;
    // `history=full` so the pre-bridge scrollback is in scope (from-epoch would
    // serve only post-manifest history, §11.7).
    admin
        .send("BRIDGE PROPOSE #general test.example history=full")
        .await;
    assert!(matches!(admin.recv().await.event, Event::Manifest { .. }));

    // Drive H's outbound bridge, registered in H's own PeerLinks so the
    // in-process backfill consumer pulls over this very connection.
    let endpoint = weft_transport::insecure::client_endpoint(weft_transport::ALPN).unwrap();
    let ctx = h.ctx().clone();
    let f_addr = f.quic_addr;
    let links = h.peer_links();
    let bridge = tokio::spawn(async move {
        weftd::dialer::run_peer_bridge(
            &endpoint,
            f_addr,
            &f_net,
            f_kp.public(),
            &h_kp,
            &h_net,
            ctx,
            links,
        )
        .await
    });

    // Wait for the bridge to go live — H announces the manifest to #general's
    // members (ada). Nothing is backfilled yet: it's fetched only on demand.
    ada.recv_until(|r| matches!(r.event, Event::Manifest { .. }))
        .await;

    // Lazy: with the bridge live but no client asking, H holds none of F's
    // scrollback (only local join lines) — we never eagerly pull it.
    let scope = weft_store::Scope::Channel("#general".parse().unwrap());
    let page = || weft_store::Page {
        before: None,
        after: None,
        limit: 500,
    };
    let before_ask = h.ctx().events.roots(&scope, page()).await.unwrap().len();
    assert!(
        before_ask < n,
        "no eager backfill before a client asks ({before_ask})"
    );

    // A client asks for history → H lazily pulls F's scrollback from the peer.
    // The large page streams over the bridge.
    ada.send("HISTORY #general limit=500").await;

    // The whole scrollback now lands in H's own store (F's origin msgids intact).
    let mut count = 0;
    for _ in 0..50 {
        count = h.ctx().events.roots(&scope, page()).await.unwrap().len();
        if count >= n {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        count >= n,
        "H ingested F's whole scrollback (got {count} of {n})"
    );

    bridge.abort();
    f.shutdown().await;
    h.shutdown().await;
    let _ = std::fs::remove_file(&f_key_path);
    let _ = std::fs::remove_file(&h_key_path);
}

// ---- §10.5 account verification (black-box over QUIC) ----

/// The self-attested birthday + pending-email + list flow over a real server.
/// The email *code* confirmation isn't checked here (the code is delivered by the
/// mailer, out of band) — that round-trip is covered by the core session test.
#[tokio::test]
async fn verify_birthday_email_pending_and_list() {
    let server = start_server(&["#general"]).await;
    let mut client = QuicClient::connect(server.quic_addr).await;

    client.send("HELLO weft/1").await;
    assert!(matches!(client.recv().await.event, Event::Welcome { .. }));
    client.send(&format!("REGISTER ada :{PASSWORD}")).await;
    client
        .recv_until(|r| matches!(r.event, Event::Welcome { .. }))
        .await;

    // Self-attested birthday → confirmed immediately.
    client.send("@label=b VERIFY BIRTHDAY 2000-05-15").await;
    let reply = client
        .recv_until(|r| matches!(r.event, Event::Verified { .. }))
        .await;
    let Event::Verified {
        kind,
        subject,
        state,
    } = &reply.event
    else {
        unreachable!()
    };
    assert_eq!(kind, "birthday");
    assert_eq!(subject, "2000-05-15");
    assert_eq!(*state, weft_proto::VerifyState::Confirmed);
    assert_eq!(reply.label.as_deref(), Some("b"));

    // Email claim → pending (a code is mailed; the dev log-mailer just prints it).
    client.send("@label=e VERIFY EMAIL ada@example.com").await;
    let reply = client
        .recv_until(|r| matches!(r.event, Event::Verified { .. }))
        .await;
    assert!(matches!(
        &reply.event,
        Event::Verified { kind, state, .. }
            if kind == "email" && *state == weft_proto::VerifyState::Pending
    ));

    // LIST → both claims come back.
    client.send("@label=l VERIFY LIST").await;
    let mut seen = std::collections::HashSet::new();
    for _ in 0..2 {
        let reply = client
            .recv_until(|r| matches!(r.event, Event::Verified { .. }))
            .await;
        if let Event::Verified { kind, .. } = &reply.event {
            seen.insert(kind.clone());
        }
    }
    assert!(
        seen.contains("email") && seen.contains("birthday"),
        "{seen:?}"
    );

    server.shutdown().await;
}

// ---- §16 WEFT-RT voice signaling (M-voice-1c) ----

/// A zero-voice server (the default) advertises no `features=voice` and answers
/// the VOICE verbs with UNSUPPORTED.
#[tokio::test]
async fn voice_disabled_by_default_is_unsupported() {
    let server = start_server(&["#general"]).await;
    let mut client = QuicClient::connect(server.quic_addr).await;

    client.send("HELLO weft/1").await;
    let Event::Welcome { features, .. } = client.recv().await.event else {
        panic!("expected WELCOME");
    };
    assert!(
        !features.iter().any(|f| f == "voice"),
        "no-voice server must not advertise voice: {features:?}"
    );
    client.send(&format!("REGISTER ada :{PASSWORD}")).await;
    assert!(matches!(client.recv().await.event, Event::Welcome { .. }));

    client.send("@label=v VOICE JOIN #general").await;
    let reply = client
        .recv_until(|r| matches!(r.event, Event::Err(_)))
        .await;
    let Event::Err(err) = &reply.event else {
        unreachable!()
    };
    assert_eq!(err.code, ErrCode::Unsupported);
    assert_eq!(reply.label.as_deref(), Some("v"));

    server.shutdown().await;
}

/// With the SFU enabled: WELCOME advertises voice, `VOICE JOIN` returns a real
/// `VOICE OFFER` (the SFU allocated a peer connection), and a bad `VOICE DESC`
/// is rejected by the SFU over QUIC — the whole control→SFU path end to end.
#[cfg(feature = "voice")]
#[tokio::test]
async fn voice_enabled_signaling_over_quic() {
    let server = start_with(&[], |c| {
        // §16 a voice channel is seeded via config (voice-only, not text-joinable).
        c.channels.push(ChannelConfig::Detailed {
            name: "#lounge".to_string(),
            policy: "retained:90d".to_string(),
            kind: Some("voice".to_string()),
        });
        c.voice.enabled = true;
        c.voice.udp_port_min = 42000;
        c.voice.udp_port_max = 42099;
        c.voice.stun = vec![]; // offline: host candidates only
    })
    .await;
    let mut client = QuicClient::connect(server.quic_addr).await;

    client.send("HELLO weft/1").await;
    let Event::Welcome { features, .. } = client.recv().await.event else {
        panic!("expected WELCOME");
    };
    assert!(
        features.iter().any(|f| f == "voice"),
        "voice server advertises it: {features:?}"
    );
    client.send(&format!("REGISTER ada :{PASSWORD}")).await;
    assert!(matches!(client.recv().await.event, Event::Welcome { .. }));
    assert!(matches!(
        client.recv().await.event,
        Event::MediaToken { .. }
    ));

    // A voice channel is entered via VOICE JOIN (never a text JOIN) → VOICE OFFER.
    client.send("@label=j VOICE JOIN #lounge").await;
    let reply = client
        .recv_until(|r| matches!(r.event, Event::VoiceOffer { .. }))
        .await;
    let Event::VoiceOffer { channel, token, .. } = &reply.event else {
        unreachable!()
    };
    assert_eq!(channel.as_str(), "#lounge");
    assert!(!token.is_empty(), "VOICE OFFER carries a media token");
    assert_eq!(reply.label.as_deref(), Some("j"));

    // A malformed SDP reaches the SFU and is rejected as MALFORMED.
    client
        .send("@label=d VOICE DESC #lounge :not-a-valid-sdp")
        .await;
    let reply = client
        .recv_until(|r| matches!(r.event, Event::Err(_)))
        .await;
    let Event::Err(err) = &reply.event else {
        unreachable!()
    };
    assert_eq!(err.code, ErrCode::Malformed);

    server.shutdown().await;
}

// ---- §16 M-lk-0: LiveKit voice backend (token minting, no `voice` feature) ----

/// The shared LiveKit API secret, used to both sign (in weftd) and verify (here).
const LIVEKIT_SECRET: &str = "livekit-shared-secret-xyz";

/// Configure a weftd whose voice backend is LiveKit, seeding one voice channel
/// (`#lounge`) plus one text channel (`#general`).
async fn start_livekit_voice() -> weftd::Server {
    start_with(&["#general"], |c| {
        c.channels.push(ChannelConfig::Detailed {
            name: "#lounge".to_string(),
            policy: "retained:90d".to_string(),
            kind: Some("voice".to_string()),
        });
        c.voice.enabled = true;
        c.voice.backend = VoiceBackendKind::Livekit;
        c.voice.livekit = LiveKit {
            url: "wss://livekit.test.example".to_string(),
            api_url: String::new(),
            api_key: "APItest".to_string(),
            api_secret: LIVEKIT_SECRET.to_string(),
            token_ttl_secs: 600,
        };
    })
    .await
}

/// With `backend = livekit`: WELCOME advertises voice, and `VOICE JOIN` answers a
/// LiveKit-mode `VOICE OFFER` whose token is a genuine JWT — verifiable with the
/// shared secret and carrying the room + publish/subscribe grants the authz gate
/// mapped from this participant's caps.
#[tokio::test]
async fn voice_livekit_offer_mints_a_scoped_token() {
    let server = start_livekit_voice().await;
    let mut client = QuicClient::connect(server.quic_addr).await;

    client.send("HELLO weft/1").await;
    let Event::Welcome { features, .. } = client.recv().await.event else {
        panic!("expected WELCOME");
    };
    assert!(
        features.iter().any(|f| f == "voice"),
        "livekit voice advertises the feature: {features:?}"
    );
    client.send(&format!("REGISTER ada :{PASSWORD}")).await;
    client
        .recv_until(|r| matches!(r.event, Event::Welcome { .. }))
        .await;

    // VOICE JOIN a voice channel → a LiveKit-mode VOICE OFFER (the labeled ack).
    client.send("@label=j VOICE JOIN #lounge").await;
    let reply = client
        .recv_until(|r| matches!(r.event, Event::VoiceOffer { .. }))
        .await;
    let Event::VoiceOffer {
        channel,
        mode,
        token,
        room,
        endpoint,
    } = &reply.event
    else {
        unreachable!()
    };
    assert_eq!(reply.label.as_deref(), Some("j"));
    assert_eq!(channel.as_str(), "#lounge");
    assert_eq!(*mode, VoiceTransport::Livekit);
    assert_eq!(endpoint.as_deref(), Some("wss://livekit.test.example"));
    assert_eq!(room.as_deref(), Some("wv:test.example:#lounge"));

    // The token is a real LiveKit access JWT: LiveKit's own `TokenVerifier`
    // validates it under the shared secret, and its `video` grant maps 1:1 from
    // the WEFT gate (open channel → publish).
    let claims = TokenVerifier::with_api_key("APItest", LIVEKIT_SECRET)
        .verify(token)
        .expect("LiveKit JWT verifies with the shared secret");

    assert_eq!(claims.sub, "ada@test.example", "identity = user@network");
    assert_eq!(claims.video.room, "wv:test.example:#lounge");
    assert!(claims.video.room_join);
    assert!(claims.video.can_publish, "open voice channel → can_publish");
    assert!(claims.video.can_subscribe);

    // The signature is genuine: a different secret must not verify it.
    let forged = TokenVerifier::with_api_key("APItest", "not-the-secret").verify(token);
    assert!(
        forged.is_err(),
        "token must not verify under a wrong secret"
    );

    server.shutdown().await;
}

/// A `VOICE JOIN` on a *text* channel mints no token — it's the uniform
/// NO-SUCH-TARGET (invariant 1), identical whether the channel is text, missing,
/// or private. The gate refuses before the LiveKit backend is ever consulted.
#[tokio::test]
async fn voice_livekit_refuses_non_voice_channel() {
    let server = start_livekit_voice().await;
    let mut client = QuicClient::connect(server.quic_addr).await;

    client.send("HELLO weft/1").await;
    assert!(matches!(client.recv().await.event, Event::Welcome { .. }));
    client.send(&format!("REGISTER ada :{PASSWORD}")).await;
    client
        .recv_until(|r| matches!(r.event, Event::Welcome { .. }))
        .await;

    client.send("@label=j VOICE JOIN #general").await;
    let reply = client
        .recv_until(|r| matches!(r.event, Event::Err(_)))
        .await;
    let Event::Err(err) = &reply.event else {
        unreachable!()
    };
    assert_eq!(err.code, ErrCode::NoSuchTarget);
    assert_eq!(reply.label.as_deref(), Some("j"));

    server.shutdown().await;
}
