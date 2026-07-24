//! Session FSM + channel actor tests over an in-memory ControlStream —
//! the whole domain layer, no sockets (architecture doc §2).

use std::io;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use weft_core::{
    run_session, Attestation, ControlStream, Keypair, LiveKitAdmin, LiveKitBackend,
    LiveKitTokenReq, Mailer, MemoryStore, RelaySpec, ServerCtx, ServerInfo, VoiceBackend,
    VoiceError, VoiceGrant, VoiceJoinReq, VoiceRelay,
};
use weft_proto::RetentionPolicy;
use weft_proto::{CallState, ErrCode, Event, FriendState, MemberAction, Reply, VoiceAction};
use weft_store::{ChannelStore, NamespaceStore};

struct MockStream {
    from_client: mpsc::UnboundedReceiver<String>,
    to_client: mpsc::UnboundedSender<String>,
}

impl ControlStream for MockStream {
    async fn recv_line(&mut self) -> io::Result<Option<String>> {
        Ok(self.from_client.recv().await)
    }

    async fn send_line(&mut self, line: &str) -> io::Result<()> {
        self.to_client
            .send(line.to_string())
            .map_err(|_| io::Error::other("client gone"))
    }
}

struct Client {
    to_server: mpsc::UnboundedSender<String>,
    from_server: mpsc::UnboundedReceiver<String>,
    _task: JoinHandle<()>,
}

impl Client {
    fn send(&self, line: &str) {
        self.to_server.send(line.to_string()).expect("session gone");
    }

    async fn recv_raw(&mut self) -> String {
        tokio::time::timeout(Duration::from_secs(5), self.from_server.recv())
            .await
            .expect("timed out waiting for a server line")
            .expect("server closed the stream")
    }

    async fn recv(&mut self) -> Reply {
        loop {
            let raw = self.recv_raw().await;
            let reply = Reply::parse(&raw).expect("server sent an unparseable line");
            // §13 the media fetch bearer is pushed after auth; it's out-of-band
            // for these (non-media) tests, so skip it transparently.
            if matches!(reply.event, Event::MediaToken { .. }) {
                continue;
            }
            // Server-generated system messages (join/part lines) interleave with
            // most flows; skip them here — a dedicated test asserts on them.
            if matches!(&reply.event, Event::Message(m) if m.meta.system.is_some()) {
                continue;
            }
            return reply;
        }
    }

    async fn expect_err(&mut self, code: ErrCode) -> Reply {
        let reply = self.recv().await;
        match &reply.event {
            Event::Err(err) if err.code == code => reply,
            other => panic!("expected ERR {code}, got {other:?}"),
        }
    }

    /// Like [`Client::recv`], but tolerant of a long wait — for events driven by
    /// a server *timer* (idle reaping) rather than by a peer's line. Under
    /// `start_paused` the short `recv` deadline would otherwise be the next timer
    /// to fire and would trip before the one under test.
    async fn recv_slow(&mut self) -> Reply {
        loop {
            let raw = tokio::time::timeout(Duration::from_secs(600), self.from_server.recv())
                .await
                .expect("timed out waiting for a server line")
                .expect("server closed the stream");
            let reply = Reply::parse(&raw).expect("server sent an unparseable line");
            if matches!(reply.event, Event::MediaToken { .. }) {
                continue;
            }
            if matches!(&reply.event, Event::Message(m) if m.meta.system.is_some()) {
                continue;
            }
            return reply;
        }
    }

    /// Keep this client's session alive across a timer-driven test by PINGing on
    /// the §3.4 cadence, the way a real client does. Returns the task; dropping
    /// it stops the keepalive.
    fn keepalive(&self) -> JoinHandle<()> {
        let tx = self.to_server.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                if tx.send("PING :keepalive".to_string()).is_err() {
                    return;
                }
            }
        })
    }

    /// True once the server closes our stream.
    async fn closed(&mut self) -> bool {
        tokio::time::timeout(Duration::from_secs(35), self.from_server.recv())
            .await
            .map(|line| line.is_none())
            .unwrap_or(false)
    }
}

const PASSWORD: &str = "test-password-123";

fn ctx(channels: &[&str]) -> Arc<ServerCtx> {
    // §6.3 default policy.
    let channels: Vec<(&str, &str)> = channels.iter().map(|c| (*c, "retained:90d")).collect();
    ctx_full(&channels, true, &[])
}

/// Context with an operator account (holds every cap at `*`) — for the
/// capability-verb tests.
fn ctx_ops(channels: &[&str], operators: &[&str]) -> Arc<ServerCtx> {
    let channels: Vec<(&str, &str)> = channels.iter().map(|c| (*c, "retained:90d")).collect();
    ctx_full(&channels, true, operators)
}

fn ctx_with(channels: &[(&str, &str)], registration_open: bool) -> Arc<ServerCtx> {
    ctx_full(channels, registration_open, &[])
}

fn ctx_full(
    channels: &[(&str, &str)],
    registration_open: bool,
    operators: &[&str],
) -> Arc<ServerCtx> {
    ctx_full_store(channels, registration_open, operators).0
}

/// Like [`ctx_full`], but also hands back the backing store — for the handful of
/// tests that assert on state the wire has no verb for (e.g. the WC7 channel
/// freeze, which is an admin-panel action).
fn ctx_full_store(
    channels: &[(&str, &str)],
    registration_open: bool,
    operators: &[&str],
) -> (Arc<ServerCtx>, Arc<MemoryStore>) {
    let store = Arc::new(MemoryStore::default());
    let info = ServerInfo {
        network: "test.example".parse().unwrap(),
        motd: Some("welcome!".to_string()),
        features: Vec::new(),
    };
    let ctx = Arc::new(ServerCtx::new(
        info,
        channels
            .iter()
            .map(|(c, p)| (c.parse().unwrap(), p.parse::<RetentionPolicy>().unwrap())),
        Keypair::generate(),
        registration_open,
        Arc::clone(&store),
        Arc::new(weft_core::MemBlobStore::default()),
        "permanent".parse().unwrap(), // §9.5 DM default
        operators.iter().map(|o| o.parse().unwrap()),
        true, // §2.2 namespace creation open
        10,   // quota
        weft_core::FederationConfig::default(),
    ));
    (ctx, store)
}

fn connect(ctx: &Arc<ServerCtx>) -> Client {
    let (to_server, from_client) = mpsc::unbounded_channel();
    let (to_client, from_server) = mpsc::unbounded_channel();
    let stream = MockStream {
        from_client,
        to_client,
    };
    let task = tokio::spawn(run_session(stream, Arc::clone(ctx)));
    Client {
        to_server,
        from_server,
        _task: task,
    }
}

/// HELLO + REGISTER (registration doubles as authentication, §6.1);
/// drains both WELCOMEs.
async fn ready(ctx: &Arc<ServerCtx>, account: &str) -> Client {
    let mut client = connect(ctx);
    client.send("HELLO weft/1");
    assert!(matches!(client.recv().await.event, Event::Welcome { .. }));
    client.send(&format!("REGISTER {account} :{PASSWORD}"));
    assert!(matches!(client.recv().await.event, Event::Welcome { .. }));
    client
}

/// `ready` + JOIN; drains the MEMBER/POLICY join response.
async fn joined(ctx: &Arc<ServerCtx>, account: &str, channel: &str) -> Client {
    let mut client = ready(ctx, account).await;
    client.send(&format!("JOIN {channel}"));
    assert!(matches!(client.recv().await.event, Event::Member { .. }));
    assert!(matches!(client.recv().await.event, Event::Policy { .. }));
    client
}

#[tokio::test]
async fn hello_gets_welcome_with_motd_and_label() {
    let ctx = ctx(&[]);
    let mut client = connect(&ctx);
    client.send("@label=h1 HELLO weft/1");
    let reply = client.recv().await;
    assert_eq!(reply.label.as_deref(), Some("h1"));
    let Event::Welcome { network, motd, .. } = &reply.event else {
        panic!("expected WELCOME, got {reply:?}");
    };
    assert_eq!(network.as_str(), "test.example");
    assert_eq!(motd.as_deref(), Some("welcome!"));
}

#[tokio::test]
async fn wrong_version_is_unsupported_and_closes() {
    let ctx = ctx(&[]);
    let mut client = connect(&ctx);
    client.send("HELLO weft/2");
    client.expect_err(ErrCode::Unsupported).await;
    assert!(client.closed().await);
}

#[tokio::test]
async fn state_gating_rejects_early_verbs() {
    let ctx = ctx(&["#general"]);
    let mut client = connect(&ctx);
    // §3.3 NEGOTIATING: only HELLO.
    client.send("@label=j1 JOIN #general");
    let reply = client.expect_err(ErrCode::NotAuthed).await;
    assert_eq!(reply.label.as_deref(), Some("j1")); // ERR is a direct response (§3.5)

    client.send("HELLO weft/1");
    client.recv().await;
    // §3.3 UNAUTHED: only AUTH, REGISTER, PING, QUIT.
    client.send("JOIN #general");
    client.expect_err(ErrCode::NotAuthed).await;
    client.send("PING t1");
    assert!(matches!(client.recv().await.event, Event::Pong { token: Some(t) } if t == "t1"));
}

/// HELLO only — for driving auth by hand.
async fn helloed(ctx: &Arc<ServerCtx>) -> Client {
    let mut client = connect(ctx);
    client.send("HELLO weft/1");
    assert!(matches!(client.recv().await.event, Event::Welcome { .. }));
    client
}

#[tokio::test]
async fn register_then_password_auth() {
    let ctx = ctx(&[]);
    let _ada = ready(&ctx, "ada").await; // registers ada

    let mut second = helloed(&ctx).await;
    second.send(&format!("@label=a1 AUTH PASSWORD ada :{PASSWORD}"));
    let reply = second.recv().await;
    assert_eq!(reply.label.as_deref(), Some("a1"));
    let Event::Welcome { attestation, .. } = &reply.event else {
        panic!("expected WELCOME, got {reply:?}");
    };
    assert_eq!(attestation, &None); // attestations belong to key auth
}

#[tokio::test]
async fn auth_failed_is_uniform_across_causes() {
    // Invariant 5: wrong password, unknown account, and proof-without-
    // challenge are indistinguishable — same code, same text.
    let ctx = ctx(&[]);
    let _ada = ready(&ctx, "ada").await;

    let mut texts = Vec::new();
    for line in [
        "AUTH PASSWORD ada :wrong-password-here",
        "AUTH PASSWORD ghost :wrong-password-here",
        "AUTH PROOF c2lnbmF0dXJl",
    ] {
        let mut client = helloed(&ctx).await;
        client.send(line);
        let reply = client.expect_err(ErrCode::AuthFailed).await;
        let Event::Err(err) = reply.event else {
            unreachable!()
        };
        texts.push(err.text);
    }
    assert_eq!(texts[0], texts[1]);
    assert_eq!(texts[1], texts[2]);
}

#[tokio::test]
async fn register_gates_policy_conflict_and_forbidden() {
    let ctx = ctx(&[]);
    let mut client = helloed(&ctx).await;
    client.send("REGISTER ada :short"); // §6.1: password ≥ 12 B
    client.expect_err(ErrCode::Policy).await;
    client.send(&format!("REGISTER ada :{PASSWORD}"));
    assert!(matches!(client.recv().await.event, Event::Welcome { .. }));

    let mut second = helloed(&ctx).await;
    second.send(&format!("REGISTER ada :{PASSWORD}"));
    second.expect_err(ErrCode::Conflict).await; // name taken

    let closed = ctx_with(&[], false);
    let mut client = helloed(&closed).await;
    client.send(&format!("REGISTER bob :{PASSWORD}"));
    client.expect_err(ErrCode::Forbidden).await; // registration closed
}

/// Full §6.1 key-auth round trip against the real session:
/// ENROLL on a password session, then CHALLENGE/PROOF on a fresh one.
#[tokio::test]
async fn auth_key_challenge_proof_flow() {
    let ctx = ctx(&["#general"]);
    let device = Keypair::generate();

    // Enroll the device while authed; response carries an attestation.
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!(
        "@label=e1 AUTH ENROLL {}",
        device.public().to_b64()
    ));
    let reply = ada.recv().await;
    assert_eq!(reply.label.as_deref(), Some("e1"));
    let Event::Welcome {
        attestation: Some(_),
        ..
    } = &reply.event
    else {
        panic!("ENROLL must answer WELCOME + attestation, got {reply:?}");
    };

    // Fresh session: AUTH KEY → CHALLENGE → PROOF → WELCOME + attestation.
    let mut session = helloed(&ctx).await;
    session.send(&format!(
        "@label=k1 AUTH KEY ada {}",
        device.public().to_b64()
    ));
    let reply = session.recv().await;
    assert_eq!(reply.label.as_deref(), Some("k1"));
    let Event::Challenge { nonce } = &reply.event else {
        panic!("expected CHALLENGE, got {reply:?}");
    };
    let nonce = weft_crypto::b64::decode(nonce).unwrap();
    assert_eq!(nonce.len(), weft_crypto::CHALLENGE_NONCE_LEN);

    // §6.1: the proof signs nonce ‖ network-name.
    let sig = weft_crypto::sign_challenge(&device, &nonce, "test.example");
    session.send(&format!(
        "@label=k2 AUTH PROOF {}",
        weft_crypto::signature_to_b64(&sig)
    ));
    let reply = session.recv().await;
    assert_eq!(reply.label.as_deref(), Some("k2"));
    let Event::Welcome {
        attestation: Some(blob),
        ..
    } = &reply.event
    else {
        panic!("expected WELCOME + attestation, got {reply:?}");
    };

    // The attestation verifies against the network's published key and
    // names the right account/device.
    let attestation = Attestation::from_b64(blob).unwrap();
    assert!(attestation.verify(&ctx.identity_public(), 0).is_ok());
    assert_eq!(attestation.account, "ada");
    assert_eq!(attestation.network, "test.example");
    assert_eq!(attestation.device, device.public());

    // And the session is READY.
    session.send("JOIN #general");
    assert!(matches!(session.recv().await.event, Event::Member { .. }));
}

#[tokio::test]
async fn auth_key_rejects_unenrolled_device_and_replays() {
    let ctx = ctx(&[]);
    let _ada = ready(&ctx, "ada").await;
    let device = Keypair::generate(); // never enrolled

    // Valid proof, unknown device → the same uniform AUTH-FAILED.
    let mut session = helloed(&ctx).await;
    session.send(&format!("AUTH KEY ada {}", device.public().to_b64()));
    let Event::Challenge { nonce } = session.recv().await.event else {
        panic!()
    };
    let nonce = weft_crypto::b64::decode(&nonce).unwrap();
    let sig = weft_crypto::sign_challenge(&device, &nonce, "test.example");
    session.send(&format!(
        "AUTH PROOF {}",
        weft_crypto::signature_to_b64(&sig)
    ));
    session.expect_err(ErrCode::AuthFailed).await;

    // The challenge was consumed: replaying the same proof fails too.
    session.send(&format!(
        "AUTH PROOF {}",
        weft_crypto::signature_to_b64(&sig)
    ));
    session.expect_err(ErrCode::AuthFailed).await;
}

#[tokio::test]
async fn cross_network_proof_is_rejected() {
    // Invariant 5: sig(nonce ‖ other-network) must not authenticate here.
    let ctx = ctx(&[]);
    let device = Keypair::generate();
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("AUTH ENROLL {}", device.public().to_b64()));
    ada.recv().await;

    let mut session = helloed(&ctx).await;
    session.send(&format!("AUTH KEY ada {}", device.public().to_b64()));
    let Event::Challenge { nonce } = session.recv().await.event else {
        panic!()
    };
    let nonce = weft_crypto::b64::decode(&nonce).unwrap();
    let sig = weft_crypto::sign_challenge(&device, &nonce, "evil.example");
    session.send(&format!(
        "AUTH PROOF {}",
        weft_crypto::signature_to_b64(&sig)
    ));
    session.expect_err(ErrCode::AuthFailed).await;
}

#[tokio::test]
async fn unknown_verbs_are_silently_ignored() {
    let ctx = ctx(&[]);
    let mut client = ready(&ctx, "ada").await;
    client.send("FROBNICATE all the things");
    client.send("PING after");
    // The unknown verb produced nothing — the next line is the PONG (§4).
    assert!(matches!(client.recv().await.event, Event::Pong { token: Some(t) } if t == "after"));
}

#[tokio::test]
async fn join_responds_member_policy_and_broadcasts() {
    let ctx = ctx(&["#general"]);
    let mut ada = ready(&ctx, "ada").await;

    ada.send("@label=j1 JOIN #general");
    let reply = ada.recv().await;
    assert_eq!(reply.label.as_deref(), Some("j1"));
    let Event::Member {
        user,
        action: MemberAction::Join,
        count: Some(1),
        ..
    } = &reply.event
    else {
        panic!("expected MEMBER join count=1, got {reply:?}");
    };
    assert_eq!(user.to_string(), "ada@test.example");
    let reply = ada.recv().await;
    assert_eq!(reply.label.as_deref(), Some("j1"));
    assert!(
        matches!(&reply.event, Event::Policy { policy, .. } if policy.to_string() == "retained:90d")
    );

    // A second joiner is broadcast to ada — without a label (§3.5).
    let _bob = joined(&ctx, "bob", "#general").await;
    let reply = ada.recv().await;
    assert_eq!(reply.label, None);
    assert!(matches!(
        &reply.event,
        Event::Member { user, action: MemberAction::Join, count: Some(2), .. }
            if user.to_string() == "bob@test.example"
    ));
}

#[tokio::test]
async fn join_unknown_channel_is_no_such_target() {
    let ctx = ctx(&["#general"]);
    let mut client = ready(&ctx, "ada").await;
    client.send("@label=j9 JOIN #nope");
    let reply = client.expect_err(ErrCode::NoSuchTarget).await;
    assert_eq!(reply.label.as_deref(), Some("j9"));
}

#[tokio::test]
async fn msg_echo_is_the_ack_and_relays_without_label() {
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    let mut bob = joined(&ctx, "bob", "#general").await;
    ada.recv().await; // bob's MEMBER join broadcast

    ada.send("@label=m1;fmt=md MSG #general :hello *world*");
    // Sender: echo MESSAGE with the label and an assigned msgid (§9.2).
    let echo = ada.recv().await;
    assert_eq!(echo.label.as_deref(), Some("m1"));
    let Event::Message(msg) = &echo.event else {
        panic!("expected MESSAGE echo, got {echo:?}");
    };
    assert_eq!(msg.body, "hello *world*");
    assert_eq!(msg.sender.to_string(), "ada@test.example");
    assert_eq!(msg.msgid.origin().as_str(), "test.example");
    assert_eq!(msg.meta.fmt.as_deref(), Some("md"));

    // Receiver: same message, same msgid, no label.
    let copy = bob.recv().await;
    assert_eq!(copy.label, None);
    let Event::Message(bob_msg) = &copy.event else {
        panic!("expected MESSAGE, got {copy:?}");
    };
    assert_eq!(bob_msg.msgid, msg.msgid);
}

#[tokio::test]
async fn msgids_are_channel_ordered() {
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    ada.send("MSG #general :one");
    ada.send("MSG #general :two");
    let Event::Message(first) = ada.recv().await.event else {
        panic!()
    };
    let Event::Message(second) = ada.recv().await.event else {
        panic!()
    };
    assert!(
        first.msgid < second.msgid,
        "actor order must be msgid order"
    );
}

#[tokio::test]
async fn msg_retry_dedups_by_session_and_label() {
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    let mut bob = joined(&ctx, "bob", "#general").await;
    ada.recv().await; // bob's join broadcast

    ada.send("@label=m1 MSG #general :once");
    let Event::Message(original) = ada.recv().await.event else {
        panic!()
    };
    bob.recv().await; // bob's first copy

    // Retry (lost-ack simulation): same label → identical echo, no rebroadcast (§9.2).
    ada.send("@label=m1 MSG #general :once");
    let Event::Message(replay) = ada.recv().await.event else {
        panic!()
    };
    assert_eq!(replay.msgid, original.msgid);

    // Bob sees exactly one copy: the next thing he receives is the probe.
    ada.send("MSG #general :probe");
    let Event::Message(next) = bob.recv().await.event else {
        panic!()
    };
    assert_eq!(next.body, "probe");
}

#[tokio::test]
async fn msg_error_paths() {
    let ctx = ctx(&["#general", "#other"]);
    let mut client = joined(&ctx, "ada", "#general").await;

    client.send("@label=e1 MSG @ghost :hi"); // unknown DM recipient (§2.2)
    assert_eq!(
        client
            .expect_err(ErrCode::NoSuchTarget)
            .await
            .label
            .as_deref(),
        Some("e1")
    );
    client.send("MSG #general :"); // §6.4: empty body needs attachments
    client.expect_err(ErrCode::Policy).await;
    client.send("@attach.1=blob MSG #general :look"); // malformed media reference
    client.expect_err(ErrCode::Policy).await;
    client.send("MSG #other :not joined"); // exists, not a member
    let reply = client.expect_err(ErrCode::CapRequired).await;
    let Event::Err(err) = &reply.event else {
        unreachable!()
    };
    assert_eq!(err.context.as_deref(), Some("send")); // §8: names the cap
    client.send("MSG #ghost :nobody home"); // does not exist
    client.expect_err(ErrCode::NoSuchTarget).await;
}

#[tokio::test]
async fn typing_relays_without_echo() {
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    let mut bob = joined(&ctx, "bob", "#general").await;
    ada.recv().await; // bob's join broadcast

    ada.send("TYPING #general start");
    let reply = bob.recv().await;
    assert!(matches!(
        &reply.event,
        Event::Typing { user, .. } if user.to_string() == "ada@test.example"
    ));
    // No echo to the typist: their next line is the PONG.
    ada.send("PING t");
    assert!(matches!(ada.recv().await.event, Event::Pong { .. }));
}

#[tokio::test]
async fn part_acks_directly_and_broadcasts() {
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    let mut bob = joined(&ctx, "bob", "#general").await;
    ada.recv().await; // bob's join broadcast

    bob.send("@label=p1 PART #general :bye");
    let reply = bob.recv().await;
    assert_eq!(reply.label.as_deref(), Some("p1"));
    assert!(matches!(
        &reply.event,
        Event::Member {
            action: MemberAction::Part,
            ..
        }
    ));
    let reply = ada.recv().await;
    assert!(matches!(
        &reply.event,
        Event::Member { user, action: MemberAction::Part, count: Some(1), .. }
            if user.to_string() == "bob@test.example"
    ));
}

#[tokio::test]
async fn disconnect_marks_a_member_offline_not_departed() {
    // Discord-style: a disconnect retains persistent membership, so the member
    // stays in the roster and just goes offline (a presence flip) — an explicit
    // PART is what removes them (see `part_broadcasts_member_leave`).
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    let bob = joined(&ctx, "bob", "#general").await;
    ada.recv().await; // bob's join broadcast

    drop(bob); // connection drops without QUIT
    let reply = ada.recv().await;
    assert!(
        matches!(
            &reply.event,
            Event::Presence { user, status: weft_proto::PresenceStatus::Offline }
                if user.to_string() == "bob@test.example"
        ),
        "disconnect goes offline, not part: {reply:?}"
    );
}

#[tokio::test]
async fn malformed_lines_close_after_five() {
    let ctx = ctx(&[]);
    let mut client = connect(&ctx);
    for _ in 0..5 {
        client.send("P!NG not a verb");
        client.expect_err(ErrCode::Malformed).await; // §8
    }
    assert!(client.closed().await);
}

#[tokio::test(start_paused = true)]
async fn preauth_idle_times_out() {
    let ctx = ctx(&[]);
    let mut client = connect(&ctx);
    // §3.3: idle pre-auth sessions are closed after 30 s. Paused time
    // auto-advances, so this returns immediately when the timer fires.
    assert!(client.closed().await);
}

// ---- M3a: message mutations + HISTORY ----

/// Send a MSG and return the echoed msgid.
async fn say(client: &mut Client, channel: &str, body: &str) -> String {
    client.send(&format!("MSG {channel} :{body}"));
    let Event::Message(msg) = client.recv().await.event else {
        panic!("expected MESSAGE echo");
    };
    msg.msgid.to_string()
}

#[tokio::test]
async fn edit_echoes_with_label_and_broadcasts() {
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    let mut bob = joined(&ctx, "bob", "#general").await;
    ada.recv().await; // bob's join broadcast

    let msgid = say(&mut ada, "#general", "typo").await;
    bob.recv().await; // bob's copy

    ada.send(&format!("@label=e1 EDIT {msgid} :fixed"));
    let echo = ada.recv().await;
    assert_eq!(echo.label.as_deref(), Some("e1"));
    let Event::Edited {
        edit_of,
        body,
        msgid: edit_id,
        ..
    } = &echo.event
    else {
        panic!("expected EDITED echo, got {echo:?}");
    };
    assert_eq!(edit_of.to_string(), msgid);
    assert_eq!(body, "fixed");
    assert_ne!(
        edit_id.to_string(),
        msgid,
        "edits get their own msgid (§9.3)"
    );

    let copy = bob.recv().await;
    assert_eq!(copy.label, None);
    assert!(matches!(&copy.event, Event::Edited { body, .. } if body == "fixed"));
}

#[tokio::test]
async fn edit_authority_is_author_only() {
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    let mut bob = joined(&ctx, "bob", "#general").await;
    ada.recv().await;

    let msgid = say(&mut ada, "#general", "ada's message").await;
    bob.recv().await;

    // §6.4: edit-own only — no edit-any, deliberately.
    bob.send(&format!("@label=x EDIT {msgid} :bob was here"));
    let reply = bob.expect_err(ErrCode::CapRequired).await;
    let Event::Err(err) = &reply.event else {
        unreachable!()
    };
    assert_eq!(err.context.as_deref(), Some("edit-own"));

    // DELETE likewise (delete-any arrives with capability tokens, M4).
    bob.send(&format!("DELETE {msgid}"));
    bob.expect_err(ErrCode::CapRequired).await;
}

#[tokio::test]
async fn mutations_on_missing_or_foreign_msgids_are_indistinct() {
    let ctx = ctx(&["#general"]);
    let mut client = joined(&ctx, "ada", "#general").await;

    // Nonexistent local msgid → NO-SUCH-TARGET (§2.2).
    client.send("EDIT test.example/01ARZ3NDEKTSV4RRFFQ69G5FAV :x");
    client.expect_err(ErrCode::NoSuchTarget).await;
    // Foreign origin → FORBIDDEN origin (§11.4).
    client.send("EDIT other.example/01ARZ3NDEKTSV4RRFFQ69G5FAV :x");
    let reply = client.expect_err(ErrCode::Forbidden).await;
    let Event::Err(err) = &reply.event else {
        unreachable!()
    };
    assert_eq!(err.context.as_deref(), Some("origin"));
}

#[tokio::test]
async fn deleted_messages_tombstone_and_reject_further_mutation() {
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    let msgid = say(&mut ada, "#general", "regrettable").await;

    ada.send(&format!("@label=d1 DELETE {msgid}"));
    let echo = ada.recv().await;
    assert_eq!(echo.label.as_deref(), Some("d1"));
    assert!(matches!(&echo.event, Event::Deleted { msgid: m, .. } if m.to_string() == msgid));

    // §2.2: a tombstoned msgid is indistinguishable from an expired one.
    ada.send(&format!("EDIT {msgid} :necromancy"));
    ada.expect_err(ErrCode::NoSuchTarget).await;
    ada.send(&format!("REACT {msgid} 👍"));
    ada.expect_err(ErrCode::NoSuchTarget).await;
}

#[tokio::test]
async fn admin_delete_tombstones_without_membership() {
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    let msgid = say(&mut ada, "#general", "regrettable").await;

    // Operator delete-any via the channel handle — no session, and the actor
    // (the "root" moderator) is not a member. The admin panel's path.
    let channel: weft_proto::ChannelName = "#general".parse().unwrap();
    let moderator: weft_proto::Account = "root".parse().unwrap();
    ctx.registry
        .get(&channel)
        .unwrap()
        .admin_delete(msgid.parse().unwrap(), moderator)
        .await;

    // The member sees the tombstone, attributed to a moderator.
    let ev = ada.recv().await;
    assert!(
        matches!(&ev.event, Event::Deleted { msgid: m, by: Some(_), .. } if m.to_string() == msgid)
    );

    // The message is gone — further mutation is NoSuchTarget (§2.2).
    ada.send(&format!("EDIT {msgid} :necromancy"));
    ada.expect_err(ErrCode::NoSuchTarget).await;
}

#[tokio::test]
async fn reactions_relay_live() {
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    let mut bob = joined(&ctx, "bob", "#general").await;
    ada.recv().await;
    let msgid = say(&mut ada, "#general", "react to me").await;
    bob.recv().await;

    bob.send(&format!("@label=r1 REACT {msgid} 🦀"));
    let echo = bob.recv().await;
    assert_eq!(echo.label.as_deref(), Some("r1"));
    let copy = ada.recv().await;
    let Event::Reaction { emoji, op, by, .. } = &copy.event else {
        panic!("expected REACTION, got {copy:?}");
    };
    assert_eq!(emoji, "🦀");
    assert_eq!(*op, weft_proto::ReactionOp::Add);
    assert_eq!(by.to_string(), "bob@test.example");

    bob.send(&format!("UNREACT {msgid} 🦀"));
    bob.recv().await;
    let copy = ada.recv().await;
    assert!(matches!(
        &copy.event,
        Event::Reaction {
            op: weft_proto::ReactionOp::Remove,
            ..
        }
    ));
}

#[tokio::test]
async fn history_serves_compacted_batches() {
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;

    let m1 = say(&mut ada, "#general", "first").await;
    let m2 = say(&mut ada, "#general", "second v1").await;
    let m3 = say(&mut ada, "#general", "doomed").await;
    // Mutate: edit m2 twice, react to m1 (net one 👍), delete m3.
    ada.send(&format!("EDIT {m2} :second v2"));
    ada.recv().await;
    ada.send(&format!("EDIT {m2} :second final"));
    ada.recv().await;
    ada.send(&format!("REACT {m1} 👍"));
    ada.recv().await;
    ada.send(&format!("REACT {m1} 🔥"));
    ada.recv().await;
    ada.send(&format!("UNREACT {m1} 🔥"));
    ada.recv().await;
    ada.send(&format!("DELETE {m3}"));
    ada.recv().await;

    ada.send("@label=h1 HISTORY #general limit=10");
    let start = ada.recv().await;
    assert_eq!(
        start.label.as_deref(),
        Some("h1"),
        "batch lines echo the label (§3.5)"
    );
    let Event::BatchStart { id } = &start.event else {
        panic!("expected BATCH START, got {start:?}");
    };
    let batch_id = id.clone();

    // m1: original body + REACTIONS summary (👍 only — 🔥 cancelled, §12.1).
    let Event::Message(msg1) = ada.recv().await.event else {
        panic!()
    };
    assert_eq!(msg1.msgid.to_string(), m1);
    assert_eq!(msg1.body, "first");
    assert_eq!(msg1.edited, None);
    let Event::Reactions {
        emoji, count, by, ..
    } = ada.recv().await.event
    else {
        panic!("expected REACTIONS summary");
    };
    assert_eq!((emoji.as_str(), count), ("👍", 1));
    assert_eq!(by.len(), 1);

    // m2: final body + edited=2, never an EDITED chain (invariant 10).
    let Event::Message(msg2) = ada.recv().await.event else {
        panic!()
    };
    assert_eq!(msg2.msgid.to_string(), m2);
    assert_eq!(msg2.body, "second final");
    assert_eq!(msg2.edited, Some(2));
    assert!(msg2.edited_at.is_some());

    // m3: tombstone only.
    let Event::Deleted { msgid, .. } = ada.recv().await.event else {
        panic!("expected DELETED tombstone");
    };
    assert_eq!(msgid.to_string(), m3);

    let end = ada.recv().await;
    let Event::BatchEnd {
        id,
        truncated,
        compacted,
    } = &end.event
    else {
        panic!("expected BATCH END, got {end:?}");
    };
    assert_eq!(id, &batch_id);
    assert!(compacted, "wire form is always materialized (§12.1)");
    assert!(!truncated, "nothing purged yet");
}

#[tokio::test]
async fn history_pages_with_before_cursor() {
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    for i in 1..=5 {
        say(&mut ada, "#general", &format!("m{i}")).await;
    }
    ada.send("HISTORY #general limit=2");
    ada.recv().await; // START
    let Event::Message(newer) = ada.recv().await.event else {
        panic!()
    };
    assert_eq!(newer.body, "m4");
    let Event::Message(newest) = ada.recv().await.event else {
        panic!()
    };
    assert_eq!(newest.body, "m5");
    ada.recv().await; // END

    ada.send(&format!("HISTORY #general limit=2 before={}", newer.msgid));
    ada.recv().await;
    let Event::Message(m2) = ada.recv().await.event else {
        panic!()
    };
    assert_eq!(m2.body, "m2");
    let Event::Message(m3) = ada.recv().await.event else {
        panic!()
    };
    assert_eq!(m3.body, "m3");
    ada.recv().await;
}

#[tokio::test]
async fn ephemeral_history_is_empty_and_truncated() {
    let ctx = ctx_with(&[("#volatile", "ephemeral")], true);
    let mut ada = joined(&ctx, "ada", "#volatile").await;
    say(&mut ada, "#volatile", "gone with the wind").await;

    ada.send("HISTORY #volatile");
    assert!(matches!(ada.recv().await.event, Event::BatchStart { .. }));
    let end = ada.recv().await;
    let Event::BatchEnd { truncated, .. } = &end.event else {
        panic!("ephemeral batch must be empty, got {end:?}");
    };
    assert!(truncated, "silence about gaps is forbidden (§6.4)");

    // And nothing can be edited — nothing was stored.
    ada.send("EDIT test.example/01ARZ3NDEKTSV4RRFFQ69G5FAV :x");
    ada.expect_err(ErrCode::NoSuchTarget).await;
}

#[tokio::test]
async fn history_requires_membership() {
    let ctx = ctx(&["#general", "#other"]);
    let mut client = joined(&ctx, "ada", "#general").await;
    client.send("HISTORY #other");
    let reply = client.expect_err(ErrCode::CapRequired).await;
    let Event::Err(err) = &reply.event else {
        unreachable!()
    };
    assert_eq!(err.context.as_deref(), Some("view"));
    client.send("HISTORY #ghost");
    client.expect_err(ErrCode::NoSuchTarget).await;
}

// ---- M3b: DMs, MARK sync, snapshots ----

#[tokio::test]
async fn dm_echo_delivery_and_multidevice_fanout() {
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    let mut bob = ready(&ctx, "bob").await;
    // Bob's second device: AUTH PASSWORD on the same account.
    let mut bob2 = connect(&ctx);
    bob2.send("HELLO weft/1");
    bob2.recv().await;
    bob2.send(&format!("AUTH PASSWORD bob :{PASSWORD}"));
    bob2.recv().await;

    ada.send("@label=d1 MSG @bob :psst");
    let echo = ada.recv().await;
    assert_eq!(echo.label.as_deref(), Some("d1"), "DM echo is the ack");
    let Event::Message(msg) = &echo.event else {
        panic!("expected MESSAGE echo, got {echo:?}");
    };
    assert_eq!(msg.target.to_string(), "@bob");
    assert_eq!(msg.sender.to_string(), "ada@test.example");

    // Both of bob's devices receive it, without labels.
    for device in [&mut bob, &mut bob2] {
        let copy = device.recv().await;
        assert_eq!(copy.label, None);
        let Event::Message(copy_msg) = &copy.event else {
            panic!("expected MESSAGE, got {copy:?}");
        };
        assert_eq!(copy_msg.msgid, msg.msgid);
    }
}

#[tokio::test]
async fn dm_mutations_and_history() {
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    let mut bob = ready(&ctx, "bob").await;

    ada.send("MSG @bob :draft one");
    let Event::Message(msg) = ada.recv().await.event else {
        panic!()
    };
    let msgid = msg.msgid.to_string();
    bob.recv().await;

    // Author edits; peer reacts; both flow through the directory.
    ada.send(&format!("@label=e1 EDIT {msgid} :final one"));
    let echo = ada.recv().await;
    assert_eq!(echo.label.as_deref(), Some("e1"));
    assert!(matches!(&echo.event, Event::Edited { body, .. } if body == "final one"));
    assert!(matches!(bob.recv().await.event, Event::Edited { .. }));

    // Peer cannot edit the author's message (edit-own, §6.4).
    bob.send(&format!("EDIT {msgid} :bob's version"));
    bob.expect_err(ErrCode::CapRequired).await;
    bob.send(&format!("REACT {msgid} 👍"));
    bob.recv().await; // own REACTION echo
    assert!(matches!(ada.recv().await.event, Event::Reaction { .. }));

    // An outsider's mutation attempt is indistinguishable from nonexistent.
    let mut eve = ready(&ctx, "eve").await;
    eve.send(&format!("EDIT {msgid} :hijack"));
    eve.expect_err(ErrCode::NoSuchTarget).await;
    eve.send("HISTORY @ada");
    assert!(matches!(eve.recv().await.event, Event::BatchStart { .. }));
    let Event::BatchEnd { .. } = eve.recv().await.event else {
        panic!("eve must not see ada+bob's DM");
    };

    // Participant history: materialized, compacted.
    bob.send("@label=h1 HISTORY @ada");
    assert!(matches!(bob.recv().await.event, Event::BatchStart { .. }));
    let Event::Message(item) = bob.recv().await.event else {
        panic!()
    };
    assert_eq!(item.body, "final one");
    assert_eq!(item.edited, Some(1));
    let Event::Reactions { count: 1, .. } = bob.recv().await.event else {
        panic!("expected REACTIONS summary")
    };
    assert!(matches!(bob.recv().await.event, Event::BatchEnd { .. }));
}

#[tokio::test]
async fn mark_syncs_across_devices_and_snapshots_on_login() {
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    let msgid = say(&mut ada, "#general", "read me").await;

    // Second device, online now.
    let mut ada2 = connect(&ctx);
    ada2.send("HELLO weft/1");
    ada2.recv().await;
    ada2.send(&format!("AUTH PASSWORD ada :{PASSWORD}"));
    ada2.recv().await;

    ada.send(&format!("@label=k1 MARK #general {msgid}"));
    let echo = ada.recv().await;
    assert_eq!(echo.label.as_deref(), Some("k1"));
    assert!(matches!(&echo.event, Event::Marked { .. }));
    // The other device gets the sync copy (after its auto-rejoin MEMBER/POLICY,
    // §6.3 — ada2 is restored into #general on login).
    let sync = loop {
        let ev = ada2.recv().await;
        if matches!(&ev.event, Event::Marked { .. }) {
            break ev;
        }
    };
    assert!(
        matches!(&sync.event, Event::Marked { msgid: m, .. } if m.to_string() == msgid),
        "got {sync:?}"
    );

    // A third device logging in later gets the snapshot (§9.7).
    let mut ada3 = connect(&ctx);
    ada3.send("HELLO weft/1");
    ada3.recv().await;
    ada3.send(&format!("AUTH PASSWORD ada :{PASSWORD}"));
    ada3.recv().await; // WELCOME
    let snapshot = ada3.recv().await;
    assert!(
        matches!(&snapshot.event, Event::Marked { msgid: m, .. } if m.to_string() == msgid),
        "expected MARKED snapshot, got {snapshot:?}"
    );

    // MARK requires membership.
    ada.send(&format!("MARK #ghost {msgid}"));
    ada.expect_err(ErrCode::NoSuchTarget).await;
}

#[tokio::test]
async fn unread_counts_report_and_push_on_mark() {
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    // A second device for ada, to observe the cross-device counts push.
    let mut ada2 = connect(&ctx);
    ada2.send("HELLO weft/1");
    ada2.recv().await;
    ada2.send(&format!("AUTH PASSWORD ada :{PASSWORD}"));
    ada2.recv().await; // WELCOME
                       // bob's join is a system message — it must NOT count as unread below.
    let mut bob = joined(&ctx, "bob", "#general").await;

    // bob posts two messages; the second mentions ada.
    say(&mut bob, "#general", "hello there").await;
    let m2 = say(&mut bob, "#general", "@ada ping").await;

    // ada requests unread counts — the two real messages, one a mention; bob's
    // join system row is excluded.
    ada.send("@label=u1 UNREAD #general");
    let ev = loop {
        let e = ada.recv().await;
        if matches!(&e.event, Event::UnreadCounts { .. }) && e.label.as_deref() == Some("u1") {
            break e;
        }
    };
    assert!(
        matches!(&ev.event,
            Event::UnreadCounts { channel, unread: 2, mentions: 1 }
            if channel.to_string() == "#general"),
        "got {ev:?}"
    );

    // Reading up to the newest message zeroes the count; the OTHER device
    // (not the marking one) gets the refreshed count so its badge clears.
    ada.send(&format!("MARK #general {m2}"));
    assert!(matches!(ada.recv().await.event, Event::Marked { .. })); // own echo
    let synced = loop {
        let e = ada2.recv().await;
        if matches!(&e.event, Event::UnreadCounts { .. }) {
            break e;
        }
    };
    assert!(
        matches!(
            &synced.event,
            Event::UnreadCounts {
                unread: 0,
                mentions: 0,
                ..
            }
        ),
        "expected zeroed counts synced to the other device, got {synced:?}"
    );

    // UNREAD requires membership.
    ada.send("UNREAD #ghost");
    ada.expect_err(ErrCode::NoSuchTarget).await;
}

#[tokio::test]
async fn search_returns_matching_messages_newest_first() {
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;

    say(&mut ada, "#general", "deploy the reference server").await;
    say(&mut ada, "#general", "lunch time").await;
    say(&mut ada, "#general", "revised DEPLOY plan").await;

    ada.send("@label=s1 SEARCH #general :deploy");
    let start = ada.recv().await;
    assert_eq!(start.label.as_deref(), Some("s1"));
    assert!(matches!(start.event, Event::BatchStart { .. }));

    let mut bodies = Vec::new();
    loop {
        match ada.recv().await.event {
            Event::Message(m) => bodies.push(m.body.clone()),
            Event::BatchEnd { .. } => break,
            _ => {}
        }
    }
    // Both "deploy" messages, case-insensitive, newest-first; "lunch time" and
    // ada's join system row are excluded.
    assert_eq!(
        bodies,
        vec![
            "revised DEPLOY plan".to_string(),
            "deploy the reference server".to_string(),
        ]
    );

    // Search requires membership.
    ada.send("SEARCH #ghost :x");
    ada.expect_err(ErrCode::NoSuchTarget).await;
}

#[tokio::test]
async fn history_thread_filter_returns_only_the_thread() {
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;

    let root = say(&mut ada, "#general", "thread root").await;
    // A reply tagged into the thread.
    ada.send(&format!("@thread={root} MSG #general :reply in thread"));
    assert!(matches!(ada.recv().await.event, Event::Message(_))); // own echo
                                                                  // An unrelated channel message (not in the thread).
    say(&mut ada, "#general", "unrelated chatter").await;

    ada.send(&format!("@label=t1 HISTORY #general thread={root}"));
    let start = ada.recv().await;
    assert_eq!(start.label.as_deref(), Some("t1"));
    assert!(matches!(start.event, Event::BatchStart { .. }));
    let mut bodies = Vec::new();
    loop {
        match ada.recv().await.event {
            Event::Message(m) => bodies.push(m.body.clone()),
            Event::BatchEnd { .. } => break,
            _ => {}
        }
    }
    // Root + its reply, oldest-first; the unrelated message is excluded.
    assert_eq!(
        bodies,
        vec!["thread root".to_string(), "reply in thread".to_string(),]
    );
}

#[tokio::test]
async fn friend_request_accept_list_and_remove() {
    let ctx = ctx(&["#general"]);
    let mut ada = ready(&ctx, "ada").await;
    let mut bob = ready(&ctx, "bob").await;

    // ada friend-requests bob → ada's own state is outgoing.
    ada.send("@l=1 FRIEND ADD bob@test.example");
    match ada.recv().await.event {
        Event::Friend { user, state } => {
            assert_eq!(user.to_string(), "bob@test.example");
            assert_eq!(state, FriendState::Outgoing);
        }
        e => panic!("expected FRIEND outgoing, got {e:?}"),
    }
    // bob (online) is pushed the incoming request.
    match bob.recv().await.event {
        Event::Friend { user, state } => {
            assert_eq!(user.to_string(), "ada@test.example");
            assert_eq!(state, FriendState::Incoming);
        }
        e => panic!("expected FRIEND incoming push, got {e:?}"),
    }

    // bob accepts → both see `friends`.
    bob.send("FRIEND ACCEPT ada@test.example");
    assert!(matches!(
        bob.recv().await.event,
        Event::Friend {
            state: FriendState::Friends,
            ..
        }
    ));
    match ada.recv().await.event {
        Event::Friend { user, state } => {
            assert_eq!(user.to_string(), "bob@test.example");
            assert_eq!(state, FriendState::Friends);
        }
        e => panic!("expected FRIEND friends push to ada, got {e:?}"),
    }

    // ada lists — one friend, mutual.
    ada.send("@l=2 FRIENDS");
    assert!(matches!(ada.recv().await.event, Event::BatchStart { .. }));
    match ada.recv().await.event {
        Event::Friend { user, state } => {
            assert_eq!(user.to_string(), "bob@test.example");
            assert_eq!(state, FriendState::Friends);
        }
        e => panic!("expected FRIEND in list, got {e:?}"),
    }
    assert!(matches!(ada.recv().await.event, Event::BatchEnd { .. }));

    // ada removes bob → both see FRIEND-REMOVED.
    ada.send("FRIEND REMOVE bob@test.example");
    assert!(matches!(
        ada.recv().await.event,
        Event::FriendRemoved { .. }
    ));
    assert!(matches!(
        bob.recv().await.event,
        Event::FriendRemoved { .. }
    ));

    // Accepting a request that isn't there is a uniform NO-SUCH-TARGET.
    ada.send("FRIEND ACCEPT ghost@test.example");
    ada.expect_err(ErrCode::NoSuchTarget).await;
    // You cannot befriend yourself.
    ada.send("FRIEND ADD ada@test.example");
    ada.expect_err(ErrCode::NoSuchTarget).await;
}

#[tokio::test]
async fn friend_call_ring_accept_and_end() {
    let ctx = ctx(&["#general"]);
    let mut ada = ready(&ctx, "ada").await;
    let mut bob = ready(&ctx, "bob").await;

    // ada calls bob → ada sees `ringing`, bob is rung with the room.
    ada.send("@l=1 CALL bob@test.example");
    match ada.recv().await.event {
        Event::CallState { user, state } => {
            assert_eq!(user.to_string(), "bob@test.example");
            assert_eq!(state, CallState::Ringing);
        }
        e => panic!("expected CALL-STATE ringing, got {e:?}"),
    }
    let room = match bob.recv().await.event {
        Event::CallRing { from, room } => {
            assert_eq!(from.to_string(), "ada@test.example");
            room
        }
        e => panic!("expected CALL-RING, got {e:?}"),
    };
    assert!(room.starts_with("call:"));

    // A third user calling bob while he's ringing gets `busy`.
    let mut eve = ready(&ctx, "eve").await;
    eve.send("CALL bob@test.example");
    assert!(matches!(
        eve.recv().await.event,
        Event::CallState {
            state: CallState::Busy,
            ..
        }
    ));

    // bob accepts → both sides go `active`.
    bob.send("CALL ACCEPT ada@test.example");
    assert!(matches!(
        bob.recv().await.event,
        Event::CallState {
            state: CallState::Active,
            ..
        }
    ));
    match ada.recv().await.event {
        Event::CallState { user, state } => {
            assert_eq!(user.to_string(), "bob@test.example");
            assert_eq!(state, CallState::Active);
        }
        e => panic!("expected CALL-STATE active to caller, got {e:?}"),
    }

    // ada hangs up → bob is told the call ended.
    ada.send("CALL END bob@test.example");
    assert!(matches!(
        ada.recv().await.event,
        Event::CallState {
            state: CallState::Ended,
            ..
        }
    ));
    match bob.recv().await.event {
        Event::CallState { user, state } => {
            assert_eq!(user.to_string(), "ada@test.example");
            assert_eq!(state, CallState::Ended);
        }
        e => panic!("expected CALL-STATE ended to bob, got {e:?}"),
    }
}

#[tokio::test]
async fn friend_call_accept_delivers_livekit_media_to_both_parties() {
    // With a LiveKit backend installed, accepting a call mints each party its
    // own CALL-MEDIA credential for the shared room (never the peer's token).
    let ctx = ctx(&["#general"]);
    ctx.set_voice_backend(Arc::new(LiveKitBackend::new(
        Arc::new(StubLk),
        "wss://livekit.test.example".to_string(),
        "test.example".parse().unwrap(),
        600,
    )));
    let mut ada = ready(&ctx, "ada").await;
    let mut bob = ready(&ctx, "bob").await;

    ada.send("CALL bob@test.example");
    assert!(matches!(ada.recv().await.event, Event::CallState { .. })); // ringing
    let room = match bob.recv().await.event {
        Event::CallRing { room, .. } => room,
        e => panic!("expected CALL-RING, got {e:?}"),
    };

    // bob accepts. He gets his active state, then his own CALL-MEDIA.
    bob.send("CALL ACCEPT ada@test.example");
    assert!(matches!(
        bob.recv().await.event,
        Event::CallState {
            state: CallState::Active,
            ..
        }
    ));
    match bob.recv().await.event {
        Event::CallMedia {
            room: r,
            token,
            endpoint,
            ..
        } => {
            assert_eq!(r, room);
            assert_eq!(endpoint.as_deref(), Some("wss://livekit.test.example"));
            // bob's token bears bob's identity — the room is the ad-hoc call room.
            assert_eq!(token, format!("jwt:{room}:bob@test.example"));
        }
        e => panic!("expected bob's CALL-MEDIA, got {e:?}"),
    }

    // ada (the caller, on her own session) is pushed active then her CALL-MEDIA.
    assert!(matches!(
        ada.recv().await.event,
        Event::CallState {
            state: CallState::Active,
            ..
        }
    ));
    match ada.recv().await.event {
        Event::CallMedia { room: r, token, .. } => {
            assert_eq!(r, room);
            assert_eq!(token, format!("jwt:{room}:ada@test.example"));
        }
        e => panic!("expected ada's CALL-MEDIA, got {e:?}"),
    }
}

#[tokio::test]
async fn call_to_remote_user_is_tunnelled() {
    // Send side of cross-network calls: a local user calling a user on another
    // network records the call locally (ringing) AND hands weftd a tunnel
    // delivery — the same §11.10 seam as cross-network friends.
    let ctx = ctx(&["#general"]);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.set_friend_deliver_sink(tx);
    let mut ada = ready(&ctx, "ada").await;

    ada.send("CALL bob@peer.example");
    // ada sees `ringing` locally.
    assert!(matches!(
        ada.recv().await.event,
        Event::CallState {
            state: CallState::Ringing,
            ..
        }
    ));
    // And the CALL is handed to the tunnel driver for the peer network.
    let req = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("call delivery")
        .expect("sink open");
    assert_eq!(req.peer.as_str(), "peer.example");
    assert_eq!(req.from.to_string(), "ada");
    assert_eq!(req.line, "CALL bob@peer.example");

    // Hanging up also tunnels (CALL END), so the remote side clears.
    ada.send("CALL END bob@peer.example");
    assert!(matches!(ada.recv().await.event, Event::CallState { .. }));
    let end = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("end delivery")
        .expect("sink open");
    assert_eq!(end.line, "CALL END bob@peer.example");
}

#[tokio::test]
async fn federated_call_rings_a_local_user_over_the_tunnel() {
    // Receive side of cross-network calls: a user on network F calls a user on
    // network H through the §11.10 tunnel. H records the call and rings its
    // local user; the caller's `ringing` state tunnels back to F.
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "peer.example", &peer_key.public());

    // bob is a local (H = test.example) user, online to be rung.
    let mut bob = ready(&ctx, "bob").await;

    // F authenticates the bridge and tunnels alice's CALL bob@test.example.
    let mut bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    bridge.send("FSESSION 1 OPEN alice");
    bridge.send("FSESSION 1 CMD :@label=c CALL bob@test.example");

    // alice's own ringing state tunnels back to F as an FSESSION REPLY.
    let raw = bridge.recv_raw().await;
    assert!(raw.starts_with("FSESSION 1 REPLY :"), "{raw}");
    assert!(raw.contains("CALL-STATE bob@test.example ringing"), "{raw}");

    // bob (local) is rung by the federated caller — the call crossed networks.
    match bob.recv().await.event {
        Event::CallRing { from, room } => {
            assert_eq!(from.to_string(), "alice@peer.example");
            assert!(room.starts_with("call:"));
        }
        e => panic!("expected CALL-RING from federated caller, got {e:?}"),
    }
}

#[tokio::test]
async fn call_to_remote_user_mints_and_tunnels_a_relay_leg() {
    // Cross-network cascade, send side: the caller's network hosts its OWN room
    // and tunnels a *relay leg* (a relay token for that room), so the callee's
    // network can bridge into it — the callee never touches our LiveKit.
    let ctx = ctx(&["#general"]);
    ctx.set_voice_backend(Arc::new(LiveKitBackend::new(
        Arc::new(StubLk),
        "wss://livekit.test.example".to_string(),
        "test.example".parse().unwrap(),
        600,
    )));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.set_friend_deliver_sink(tx);
    let mut ada = ready(&ctx, "ada").await;

    ada.send("CALL bob@peer.example");
    assert!(matches!(
        ada.recv().await.event,
        Event::CallState {
            state: CallState::Ringing,
            ..
        }
    ));

    let req = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("call delivery")
        .expect("sink open");
    assert_eq!(req.peer.as_str(), "peer.example");

    // The tunnelled CALL carries our relay leg. Parse it back.
    let parsed = weft_proto::Request::parse(&req.line).expect("valid CALL line");
    let weft_proto::Command::Call { user, media } = parsed.command else {
        panic!("expected CALL, got {:?}", req.line);
    };
    assert_eq!(user.to_string(), "bob@peer.example");
    let leg = media.expect("caller network minted a relay leg");
    assert!(leg.room.starts_with("call:"), "{}", leg.room);
    assert_eq!(leg.endpoint.as_deref(), Some("wss://livekit.test.example"));
    // The relay token's identity is `relay@<callee network>` (StubLk = `jwt:<room>:<id>`).
    assert_eq!(leg.token, format!("jwt:{}:relay@peer.example", leg.room));
}

#[tokio::test]
async fn federated_call_bridges_via_a_relay_on_accept() {
    // Cross-network cascade, receive side: a federated CALL carries the caller
    // network's relay leg; the callee's network mints its OWN room for its user
    // and, on accept, spawns a relay bridging the two rooms — so neither client
    // connects to the other network's LiveKit (IP protection).
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "peer.example", &peer_key.public());
    ctx.set_voice_backend(Arc::new(LiveKitBackend::new(
        Arc::new(StubLk),
        "wss://lk.test.example".to_string(),
        "test.example".parse().unwrap(),
        600,
    )));
    let relay = Arc::new(MockRelay::default());
    ctx.set_voice_relay(relay.clone());

    let mut bob = ready(&ctx, "bob").await;
    let bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;

    // alice@peer calls bob@test, carrying peer's relay leg for peer's room.
    let inner = weft_proto::Request::new(weft_proto::Command::Call {
        user: "bob@test.example".parse().unwrap(),
        media: Some(weft_proto::CallMediaGrant {
            room: "call:HOME".to_string(),
            token: "relay.tok.home".to_string(),
            endpoint: Some("wss://lk.peer.example".to_string()),
        }),
    })
    .serialize()
    .unwrap();
    bridge.send("FSESSION 1 OPEN alice");
    bridge.send(&format!("FSESSION 1 CMD :{inner}"));

    // bob rings with OUR OWN room (not the caller's `call:HOME`).
    let bob_room = match bob.recv().await.event {
        Event::CallRing { from, room } => {
            assert_eq!(from.to_string(), "alice@peer.example");
            assert!(room.starts_with("call:") && room != "call:HOME", "{room}");
            room
        }
        e => panic!("expected CALL-RING, got {e:?}"),
    };

    // bob accepts → active, then CALL-MEDIA for OUR LiveKit + OUR room + his token.
    bob.send("CALL ACCEPT alice@peer.example");
    assert!(matches!(
        bob.recv().await.event,
        Event::CallState {
            state: CallState::Active,
            ..
        }
    ));
    match bob.recv().await.event {
        Event::CallMedia {
            room,
            token,
            endpoint,
            ..
        } => {
            assert_eq!(room, bob_room);
            assert_eq!(endpoint.as_deref(), Some("wss://lk.test.example"));
            assert_eq!(token, format!("jwt:{bob_room}:bob@test.example"));
        }
        e => panic!("expected bob's own CALL-MEDIA, got {e:?}"),
    }

    // A relay was spawned bridging bob's room ↔ the caller network's leg.
    let specs = relay.specs.lock().unwrap();
    assert_eq!(specs.len(), 1, "one relay spawned");
    let s = &specs[0];
    assert_eq!(s.peer.as_str(), "peer.example");
    assert_eq!(s.key, bob_room);
    assert_eq!(s.remote_room, "call:HOME");
    assert_eq!(s.remote_token, "relay.tok.home");
    assert_eq!(s.remote_url, "wss://lk.peer.example");
    assert_eq!(s.local_room, bob_room);
    assert_eq!(s.local_token, format!("jwt:{bob_room}:relay@peer.example"));
}

#[tokio::test]
async fn group_dm_create_message_and_membership() {
    let ctx = ctx(&["#general"]);
    let mut ada = ready(&ctx, "ada").await;
    let mut bob = ready(&ctx, "bob").await;

    // ada creates a group DM with bob.
    ada.send("@l=1 GROUP CREATE bob@test.example");
    let gid = match ada.recv().await.event {
        Event::Group { id, members, name } => {
            assert_eq!(members.len(), 2, "ada + bob");
            assert_eq!(name, None);
            id.to_string()
        }
        e => panic!("expected GROUP, got {e:?}"),
    };
    // bob is pushed the group too.
    assert!(matches!(bob.recv().await.event, Event::Group { .. }));

    // ada messages the group; ada gets her labelled echo, bob gets the copy.
    ada.send(&format!("@l=2 MSG {gid} :hey group"));
    match ada.recv().await.event {
        Event::Message(m) => {
            assert_eq!(m.body, "hey group");
            assert_eq!(m.target.to_string(), gid);
        }
        e => panic!("expected own group echo, got {e:?}"),
    }
    match bob.recv().await.event {
        Event::Message(m) => assert_eq!(m.body, "hey group"),
        e => panic!("expected group message to bob, got {e:?}"),
    }

    // ada lists her groups.
    ada.send("GROUPS");
    assert!(matches!(ada.recv().await.event, Event::BatchStart { .. }));
    assert!(matches!(ada.recv().await.event, Event::Group { .. }));
    assert!(matches!(ada.recv().await.event, Event::BatchEnd { .. }));

    // A non-member can't message the group — uniform NO-SUCH-TARGET.
    let mut eve = ready(&ctx, "eve").await;
    eve.send(&format!("MSG {gid} :sneaking in"));
    eve.expect_err(ErrCode::NoSuchTarget).await;

    // bob leaves; both bob (ack) and ada (push) see GROUP-MEMBER part.
    bob.send(&format!("GROUP LEAVE {gid}"));
    assert!(matches!(
        bob.recv().await.event,
        Event::GroupMember {
            action: MemberAction::Part,
            ..
        }
    ));
    match ada.recv().await.event {
        Event::GroupMember { user, action, .. } => {
            assert_eq!(user.to_string(), "bob@test.example");
            assert_eq!(action, MemberAction::Part);
        }
        e => panic!("expected GROUP-MEMBER part, got {e:?}"),
    }
}

#[tokio::test]
async fn group_dm_edit_delete_react() {
    let ctx = ctx(&["#general"]);
    let mut ada = ready(&ctx, "ada").await;
    let mut bob = ready(&ctx, "bob").await;

    ada.send("GROUP CREATE bob@test.example");
    let gid = match ada.recv().await.event {
        Event::Group { id, .. } => id.to_string(),
        e => panic!("expected GROUP, got {e:?}"),
    };
    assert!(matches!(bob.recv().await.event, Event::Group { .. })); // bob's push

    // ada posts; capture the msgid from her echo, drain bob's copy.
    ada.send(&format!("MSG {gid} :original"));
    let msgid = match ada.recv().await.event {
        Event::Message(m) => m.msgid.to_string(),
        e => panic!("expected own echo, got {e:?}"),
    };
    assert!(matches!(bob.recv().await.event, Event::Message(_)));

    // ada edits her own message → both members see EDITED for the group.
    ada.send(&format!("@label=e EDIT {msgid} :fixed"));
    match ada.recv().await.event {
        Event::Edited {
            target,
            body,
            edit_of,
            ..
        } => {
            assert_eq!(target.to_string(), gid);
            assert_eq!(body, "fixed");
            assert_eq!(edit_of.to_string(), msgid);
        }
        e => panic!("expected EDITED echo, got {e:?}"),
    }
    assert!(matches!(
        bob.recv().await.event,
        Event::Edited { body, .. } if body == "fixed"
    ));

    // bob (a member, not the author) may REACT to ada's message.
    bob.send(&format!("REACT {msgid} 🦀"));
    match bob.recv().await.event {
        Event::Reaction { target, emoji, .. } => {
            assert_eq!(target.to_string(), gid);
            assert_eq!(emoji, "🦀");
        }
        e => panic!("expected REACTION echo, got {e:?}"),
    }
    assert!(matches!(ada.recv().await.event, Event::Reaction { .. }));

    // bob cannot EDIT ada's message — not his to edit.
    bob.send(&format!("@label=x EDIT {msgid} :hijack"));
    bob.expect_err(ErrCode::CapRequired).await;

    // A non-member reacting is uniform NO-SUCH-TARGET (no leak of existence).
    let mut eve = ready(&ctx, "eve").await;
    eve.send(&format!("REACT {msgid} 👍"));
    eve.expect_err(ErrCode::NoSuchTarget).await;

    // ada deletes her message → both see DELETED (a tombstone) for the group.
    ada.send(&format!("@label=d DELETE {msgid}"));
    match ada.recv().await.event {
        Event::Deleted {
            target, msgid: m, ..
        } => {
            assert_eq!(target.to_string(), gid);
            assert_eq!(m.to_string(), msgid);
        }
        e => panic!("expected DELETED echo, got {e:?}"),
    }
    assert!(matches!(bob.recv().await.event, Event::Deleted { .. }));

    // A deleted group message is gone — editing it is NO-SUCH-TARGET.
    ada.send(&format!("EDIT {msgid} :necromancy"));
    ada.expect_err(ErrCode::NoSuchTarget).await;
}

#[tokio::test]
async fn group_dm_call_join_roster_and_leave() {
    let ctx = ctx(&["#general"]);
    ctx.set_voice_backend(Arc::new(LiveKitBackend::new(
        Arc::new(StubLk),
        "wss://lk.test.example".to_string(),
        "test.example".parse().unwrap(),
        600,
    )));
    let mut ada = ready(&ctx, "ada").await;
    let mut bob = ready(&ctx, "bob").await;

    ada.send("GROUP CREATE bob@test.example");
    let gid = match ada.recv().await.event {
        Event::Group { id, .. } => id.to_string(),
        e => panic!("expected GROUP, got {e:?}"),
    };
    assert!(matches!(bob.recv().await.event, Event::Group { .. }));

    // ada starts the call: labelled `active` ack, then her CALL-MEDIA for the room.
    ada.send(&format!("@label=c GROUP CALL {gid}"));
    let room = {
        match ada.recv().await.event {
            Event::GroupCallState { group, user, state } => {
                assert_eq!(group.to_string(), gid);
                assert_eq!(user.to_string(), "ada@test.example");
                assert_eq!(state, CallState::Active);
            }
            e => panic!("expected GROUP-CALL active ack, got {e:?}"),
        }
        match ada.recv().await.event {
            Event::CallMedia {
                room,
                token,
                endpoint,
                ..
            } => {
                assert!(room.starts_with("gcall:"), "{room}");
                assert_eq!(endpoint.as_deref(), Some("wss://lk.test.example"));
                assert_eq!(token, format!("jwt:{room}:ada@test.example"));
                room
            }
            e => panic!("expected CALL-MEDIA, got {e:?}"),
        }
    };

    // bob is notified a call is active (ada joined).
    match bob.recv().await.event {
        Event::GroupCallState { user, state, .. } => {
            assert_eq!(user.to_string(), "ada@test.example");
            assert_eq!(state, CallState::Active);
        }
        e => panic!("expected GROUP-CALL active for bob, got {e:?}"),
    }

    // bob joins: his active ack, his media (SAME group room), then the roster
    // snapshot (ada already in). ada is told bob joined.
    bob.send(&format!("GROUP CALL {gid}"));
    assert!(matches!(
        bob.recv().await.event,
        Event::GroupCallState {
            state: CallState::Active,
            ..
        }
    ));
    match bob.recv().await.event {
        Event::CallMedia { room: r, token, .. } => {
            assert_eq!(r, room, "bob joins the same group room");
            assert_eq!(token, format!("jwt:{room}:bob@test.example"));
        }
        e => panic!("expected bob's CALL-MEDIA, got {e:?}"),
    }
    // Roster snapshot: ada is already active.
    match bob.recv().await.event {
        Event::GroupCallState { user, state, .. } => {
            assert_eq!(user.to_string(), "ada@test.example");
            assert_eq!(state, CallState::Active);
        }
        e => panic!("expected roster (ada active), got {e:?}"),
    }
    // ada sees bob join.
    match ada.recv().await.event {
        Event::GroupCallState { user, state, .. } => {
            assert_eq!(user.to_string(), "bob@test.example");
            assert_eq!(state, CallState::Active);
        }
        e => panic!("expected bob active for ada, got {e:?}"),
    }

    // A non-member can't join — uniform NO-SUCH-TARGET.
    let mut eve = ready(&ctx, "eve").await;
    eve.send(&format!("GROUP CALL {gid}"));
    eve.expect_err(ErrCode::NoSuchTarget).await;

    // bob hangs up: his `ended` ack; ada is told he left.
    bob.send(&format!("GROUP HANGUP {gid}"));
    assert!(matches!(
        bob.recv().await.event,
        Event::GroupCallState {
            state: CallState::Ended,
            ..
        }
    ));
    match ada.recv().await.event {
        Event::GroupCallState { user, state, .. } => {
            assert_eq!(user.to_string(), "bob@test.example");
            assert_eq!(state, CallState::Ended);
        }
        e => panic!("expected bob ended for ada, got {e:?}"),
    }

    // Leaving when not in the call is NO-SUCH-TARGET.
    bob.send(&format!("GROUP HANGUP {gid}"));
    bob.expect_err(ErrCode::NoSuchTarget).await;
}

#[tokio::test]
async fn group_call_host_rings_remote_networks_with_a_relay_leg() {
    // §16 M-lk-3b group-call relay star, host side: starting a group call with a
    // remote member tunnels a `GROUP CALL` ring carrying the host's relay leg
    // (a relay token for the host's own room) to that member's network.
    let ctx = ctx(&["#general"]);
    ctx.set_voice_backend(Arc::new(LiveKitBackend::new(
        Arc::new(StubLk),
        "wss://lk.test.example".to_string(),
        "test.example".parse().unwrap(),
        600,
    )));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.set_friend_deliver_sink(tx);
    let mut ada = ready(&ctx, "ada").await;

    // A group spanning networks (ada@test + carol@peer).
    ada.send("GROUP CREATE carol@peer.example");
    let gid = match ada.recv().await.event {
        Event::Group { id, .. } => id.to_string(),
        e => panic!("expected GROUP, got {e:?}"),
    };

    // ada starts the call: her own active ack + CALL-MEDIA, and a ring to peer.
    ada.send(&format!("GROUP CALL {gid}"));
    assert!(matches!(
        ada.recv().await.event,
        Event::GroupCallState {
            state: CallState::Active,
            ..
        }
    ));
    assert!(matches!(ada.recv().await.event, Event::CallMedia { .. }));

    // Skip the GROUP SYNC that group creation tunnels; find the GROUP CALL ring.
    let req = loop {
        let d = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("group-call ring")
            .expect("sink open");
        if d.line.contains("GROUP CALL") {
            break d;
        }
    };
    assert_eq!(req.peer.as_str(), "peer.example");
    let parsed = weft_proto::Request::parse(&req.line).expect("valid GROUP CALL");
    let weft_proto::Command::GroupCall { group, media } = parsed.command else {
        panic!("expected GROUP CALL, got {:?}", req.line);
    };
    assert_eq!(group.to_string(), gid);
    let leg = media.expect("host relay leg");
    assert!(leg.room.starts_with("gcall:"), "{}", leg.room);
    assert_eq!(leg.endpoint.as_deref(), Some("wss://lk.test.example"));
    // The leg's identity is `relay@<remote network>` for our host room.
    assert_eq!(leg.token, format!("jwt:{}:relay@peer.example", leg.room));
}

#[tokio::test]
async fn federated_group_call_bridges_via_a_relay_on_join() {
    // §16 M-lk-3b group-call relay star, spoke side: a federated ring carries the
    // host network's relay leg; when a local member joins, our network mints its
    // own room and spawns a relay bridging it to the host's — so the local member
    // never connects to the host's LiveKit.
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "peer.example", &peer_key.public());
    ctx.set_voice_backend(Arc::new(LiveKitBackend::new(
        Arc::new(StubLk),
        "wss://lk.test.example".to_string(),
        "test.example".parse().unwrap(),
        600,
    )));
    let relay = Arc::new(MockRelay::default());
    ctx.set_voice_relay(relay.clone());

    // A group with our local carol + the remote host member alice@peer.
    let mut carol = ready(&ctx, "carol").await;
    carol.send("GROUP CREATE alice@peer.example");
    let gid = match carol.recv().await.event {
        Event::Group { id, .. } => id.to_string(),
        e => panic!("expected GROUP, got {e:?}"),
    };

    // alice@peer (the host) rings us, carrying peer's relay leg for its room.
    let inner = weft_proto::Request::new(weft_proto::Command::GroupCall {
        group: gid.parse().unwrap(),
        media: Some(weft_proto::CallMediaGrant {
            room: "gcall:HOST".to_string(),
            token: "relay.host".to_string(),
            endpoint: Some("wss://lk.peer.example".to_string()),
        }),
    })
    .serialize()
    .unwrap();
    let bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    bridge.send("FSESSION 1 OPEN alice");
    bridge.send(&format!("FSESSION 1 CMD :{inner}"));

    // carol is rung — the host member shows active.
    match carol.recv().await.event {
        Event::GroupCallState { user, state, .. } => {
            assert_eq!(user.to_string(), "alice@peer.example");
            assert_eq!(state, CallState::Active);
        }
        e => panic!("expected GROUP-CALL ring, got {e:?}"),
    }

    // carol joins → active ack, then a relay is spawned, then her media (OUR room).
    carol.send(&format!("GROUP CALL {gid}"));
    assert!(matches!(
        carol.recv().await.event,
        Event::GroupCallState {
            state: CallState::Active,
            ..
        }
    ));
    let room = match carol.recv().await.event {
        Event::CallMedia { room, endpoint, .. } => {
            assert_eq!(endpoint.as_deref(), Some("wss://lk.test.example"));
            assert!(room.starts_with("gcall:"), "{room}");
            room
        }
        e => panic!("expected carol's CALL-MEDIA, got {e:?}"),
    };

    // The relay bridges our room ↔ the host network's leg.
    let specs = relay.specs.lock().unwrap();
    assert_eq!(specs.len(), 1, "one relay spawned");
    let s = &specs[0];
    assert_eq!(s.peer.as_str(), "peer.example");
    assert_eq!(s.key, room);
    assert_eq!(s.remote_room, "gcall:HOST");
    assert_eq!(s.remote_token, "relay.host");
    assert_eq!(s.remote_url, "wss://lk.peer.example");
    assert_eq!(s.local_room, room);
    assert_eq!(s.local_token, format!("jwt:{room}:relay@peer.example"));
}

#[tokio::test]
async fn federated_group_roster_syncs_across_networks() {
    // Roster mesh: a local join tunnels a GROUP ROSTER to remote member networks;
    // an inbound GROUP ROSTER reaches our local members, and a `reply` one is
    // answered with our own participants (the snapshot for a fresh joiner).
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "peer.example", &peer_key.public());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.set_friend_deliver_sink(tx);
    let mut carol = ready(&ctx, "carol").await;
    carol.send("GROUP CREATE alice@peer.example");
    let gid = match carol.recv().await.event {
        Event::Group { id, .. } => id.to_string(),
        e => panic!("expected GROUP, got {e:?}"),
    };

    // carol joins (no LiveKit backend → signaling only). As the host she rings
    // peer (GROUP CALL) and broadcasts her roster (GROUP ROSTER).
    carol.send(&format!("GROUP CALL {gid}"));
    assert!(matches!(
        carol.recv().await.event,
        Event::GroupCallState {
            state: CallState::Active,
            ..
        }
    ));
    macro_rules! recv_line {
        () => {
            tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("delivery")
                .expect("sink open")
        };
    }
    // Find the GROUP ROSTER among the (GROUP SYNC, GROUP CALL ring, GROUP ROSTER)
    // deliveries.
    let mut sent = recv_line!();
    while !sent.line.contains("GROUP ROSTER") {
        sent = recv_line!();
    }
    assert_eq!(sent.peer.as_str(), "peer.example");
    assert!(sent.line.contains("GROUP ROSTER"), "{}", sent.line);
    assert!(
        sent.line.contains("carol@test.example active"),
        "{}",
        sent.line
    );
    assert!(sent.line.contains("reply=yes"), "{}", sent.line);

    // peer tells us alice@peer joined (reply=yes → we answer with our roster).
    let inner = weft_proto::Request::new(weft_proto::Command::GroupCallRoster {
        group: gid.parse().unwrap(),
        user: "alice@peer.example".parse().unwrap(),
        active: true,
        reply: true,
    })
    .serialize()
    .unwrap();
    let bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    bridge.send("FSESSION 1 OPEN alice");
    bridge.send(&format!("FSESSION 1 CMD :{inner}"));

    // carol's client sees the cross-network member.
    match carol.recv().await.event {
        Event::GroupCallState { user, state, .. } => {
            assert_eq!(user.to_string(), "alice@peer.example");
            assert_eq!(state, CallState::Active);
        }
        e => panic!("expected alice@peer in the roster, got {e:?}"),
    }

    // We replied to peer with our participant (carol), reply=no (no loop).
    let reply = recv_line!();
    assert!(
        reply.line.contains("carol@test.example active"),
        "{}",
        reply.line
    );
    assert!(!reply.line.contains("reply=yes"), "{}", reply.line);
}

#[tokio::test]
async fn group_call_simultaneous_start_yields_to_smaller_network() {
    // Split-brain tiebreak: we (test.example) start a call and are momentarily the
    // host; a competing ring from peer.example — which sorts BEFORE us — makes us
    // yield and bridge our room into peer's (peer becomes the single host).
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "peer.example", &peer_key.public());
    ctx.set_voice_backend(Arc::new(LiveKitBackend::new(
        Arc::new(StubLk),
        "wss://lk.test.example".to_string(),
        "test.example".parse().unwrap(),
        600,
    )));
    let relay = Arc::new(MockRelay::default());
    ctx.set_voice_relay(relay.clone());

    let mut ada = ready(&ctx, "ada").await;
    ada.send("GROUP CREATE carol@peer.example");
    let gid = match ada.recv().await.event {
        Event::Group { id, .. } => id.to_string(),
        e => panic!("expected GROUP, got {e:?}"),
    };

    // ada starts → test.example hosts, ada is a participant. Capture her room.
    ada.send(&format!("GROUP CALL {gid}"));
    assert!(matches!(
        ada.recv().await.event,
        Event::GroupCallState {
            state: CallState::Active,
            ..
        }
    ));
    let ada_room = match ada.recv().await.event {
        Event::CallMedia { room, .. } => room,
        e => panic!("expected ada's CALL-MEDIA, got {e:?}"),
    };

    // peer.example simultaneously rings us with its own relay leg.
    let inner = weft_proto::Request::new(weft_proto::Command::GroupCall {
        group: gid.parse().unwrap(),
        media: Some(weft_proto::CallMediaGrant {
            room: "gcall:PEER".to_string(),
            token: "relay.peer".to_string(),
            endpoint: Some("wss://lk.peer.example".to_string()),
        }),
    })
    .serialize()
    .unwrap();
    let bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    bridge.send("FSESSION 1 OPEN carol");
    bridge.send(&format!("FSESSION 1 CMD :{inner}"));

    // The ring notifies ada locally (sync point — the relay spawns before this).
    match ada.recv().await.event {
        Event::GroupCallState { user, state, .. } => {
            assert_eq!(user.to_string(), "carol@peer.example");
            assert_eq!(state, CallState::Active);
        }
        e => panic!("expected carol@peer active, got {e:?}"),
    }

    // We yielded: a relay now bridges OUR room ↔ peer's (the smaller network wins).
    let specs = relay.specs.lock().unwrap();
    assert_eq!(specs.len(), 1, "one relay spawned on yield");
    let s = &specs[0];
    assert_eq!(s.peer.as_str(), "peer.example");
    assert_eq!(s.key, ada_room);
    assert_eq!(s.remote_room, "gcall:PEER");
    assert_eq!(s.remote_token, "relay.peer");
    assert_eq!(s.local_room, ada_room);
}

#[tokio::test]
async fn cross_network_group_message_home_mints_and_fans_out() {
    // The group's home (creator's network) is the single ULID writer: it mints
    // and fans messages out to every member network; a spoke's relayed post is
    // minted here too. Also covers membership propagation on create.
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "peer.example", &peer_key.public());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.set_friend_deliver_sink(tx);
    let mut carol = ready(&ctx, "carol").await;

    macro_rules! sink {
        () => {
            tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("delivery")
                .expect("sink open")
        };
    }

    // carol@test creates a group with alice@peer → test.example is the home. The
    // membership is synced to peer.
    carol.send("GROUP CREATE alice@peer.example");
    let gid = match carol.recv().await.event {
        Event::Group { id, .. } => id.to_string(),
        e => panic!("expected GROUP, got {e:?}"),
    };
    let synced = sink!();
    assert_eq!(synced.peer.as_str(), "peer.example");
    assert!(synced.line.contains("GROUP SYNC"), "{}", synced.line);
    assert!(
        synced.line.contains("carol@test.example"),
        "{}",
        synced.line
    );
    assert!(
        synced.line.contains("alice@peer.example"),
        "{}",
        synced.line
    );

    // carol posts → home mints, echoes to carol, fans out to peer.
    carol.send(&format!("@l=m MSG {gid} :hello"));
    match carol.recv().await.event {
        Event::Message(m) => {
            assert_eq!(m.body, "hello");
            assert_eq!(m.target.to_string(), gid);
        }
        e => panic!("expected own echo, got {e:?}"),
    }
    let relay = sink!();
    assert_eq!(relay.peer.as_str(), "peer.example");
    assert!(relay.line.contains("GROUP RELAY"), "{}", relay.line);
    assert!(relay.line.contains("id="), "{}", relay.line); // home-minted
    assert!(relay.line.contains("carol@test.example"), "{}", relay.line);
    assert!(relay.line.contains("hello"), "{}", relay.line);

    // A spoke relays alice's post to us (home): @id absent → we mint + deliver.
    let inner = weft_proto::Request::new(weft_proto::Command::GroupRelay {
        group: gid.parse().unwrap(),
        sender: "alice@peer.example".parse().unwrap(),
        msgid: None,
        body: "hi from alice".to_string(),
        meta: weft_proto::MsgMeta::default(),
        echo: None,
    })
    .serialize()
    .unwrap();
    let bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    bridge.send("FSESSION 1 OPEN alice");
    bridge.send(&format!("FSESSION 1 CMD :{inner}"));

    match carol.recv().await.event {
        Event::Message(m) => {
            assert_eq!(m.sender.to_string(), "alice@peer.example");
            assert_eq!(m.body, "hi from alice");
            assert_eq!(m.msgid.origin().as_str(), "test.example"); // minted by the home
        }
        e => panic!("expected alice's message, got {e:?}"),
    }
    // And it was fanned back out to peer (home-minted → @id).
    let relay2 = sink!();
    assert!(relay2.line.contains("GROUP RELAY"), "{}", relay2.line);
    assert!(relay2.line.contains("id="), "{}", relay2.line);
    assert!(relay2.line.contains("hi from alice"), "{}", relay2.line);
}

#[tokio::test]
async fn cross_network_group_membership_changes_propagate() {
    // Add / remove / name changes re-sync the group to remote member networks.
    let ctx = ctx(&["#general"]);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.set_friend_deliver_sink(tx);
    let mut carol = ready(&ctx, "carol").await;

    macro_rules! sync_line {
        () => {{
            loop {
                let d = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                    .await
                    .expect("delivery")
                    .expect("sink open");
                if d.line.contains("GROUP SYNC") {
                    break d;
                }
            }
        }};
    }

    carol.send("GROUP CREATE alice@peer.example");
    let gid = match carol.recv().await.event {
        Event::Group { id, .. } => id.to_string(),
        e => panic!("expected GROUP, got {e:?}"),
    };
    let _created = sync_line!(); // the create sync

    // Add dave@peer → a sync carrying dave (creates the group on peer if new).
    // (We check the tunnelled sync, not carol's own events.)
    carol.send(&format!("GROUP ADD {gid} dave@peer.example"));
    let added = sync_line!();
    assert_eq!(added.peer.as_str(), "peer.example");
    assert!(added.line.contains("dave@peer.example"), "{}", added.line);
    assert!(added.line.contains("alice@peer.example"), "{}", added.line);

    // Rename → a sync carrying the name.
    carol.send(&format!("GROUP NAME {gid} :weekend"));
    let named = sync_line!();
    assert!(named.line.contains("name=weekend"), "{}", named.line);

    // Remove alice → a sync without alice, still delivered to peer (dave remains).
    carol.send(&format!("GROUP REMOVE {gid} alice@peer.example"));
    let removed = sync_line!();
    assert_eq!(removed.peer.as_str(), "peer.example");
    assert!(
        removed.line.contains("dave@peer.example"),
        "{}",
        removed.line
    );
    assert!(
        !removed.line.contains("alice@peer.example"),
        "{}",
        removed.line
    );
}

#[tokio::test]
async fn federated_group_sync_reconciles_and_parts_removed_member() {
    // An inbound GROUP SYNC reconciles membership; a removed local member is told
    // it left (its client drops the group).
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "peer.example", &peer_key.public());
    let mut carol = ready(&ctx, "carol").await;

    const G: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    macro_rules! sync {
        ($members:expr) => {{
            let line = weft_proto::Request::new(weft_proto::Command::GroupSync {
                group: G.parse().unwrap(),
                creator: "alice@peer.example".parse().unwrap(),
                name: None,
                members: $members,
            })
            .serialize()
            .unwrap();
            bridge.send(&format!("FSESSION 1 CMD :{line}"));
        }};
    }
    bridge.send("FSESSION 1 OPEN alice");

    // Initial: carol is a member.
    sync!(vec![
        "alice@peer.example".parse().unwrap(),
        "carol@test.example".parse().unwrap(),
        "dave@peer.example".parse().unwrap(),
    ]);
    assert!(matches!(carol.recv().await.event, Event::Group { .. }));

    // Re-sync WITHOUT carol → she's parted.
    sync!(vec![
        "alice@peer.example".parse().unwrap(),
        "dave@peer.example".parse().unwrap(),
    ]);
    match carol.recv().await.event {
        Event::GroupMember { user, action, .. } => {
            assert_eq!(user.to_string(), "carol@test.example");
            assert_eq!(action, MemberAction::Part);
        }
        e => panic!("expected GROUP-MEMBER part, got {e:?}"),
    }
}

#[tokio::test]
async fn spoke_poster_gets_a_labelled_echo() {
    // A spoke poster's cross-network group message comes back from the home as
    // their own **labelled** message (the §3.5 ack), via the echo-token round trip.
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "peer.example", &peer_key.public());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.set_friend_deliver_sink(tx);
    let mut carol = ready(&ctx, "carol").await;

    // Group home = peer (sync it in).
    const G: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    bridge.send("FSESSION 1 OPEN alice");
    let sync = weft_proto::Request::new(weft_proto::Command::GroupSync {
        group: G.parse().unwrap(),
        creator: "alice@peer.example".parse().unwrap(),
        name: None,
        members: vec![
            "alice@peer.example".parse().unwrap(),
            "carol@test.example".parse().unwrap(),
        ],
    })
    .serialize()
    .unwrap();
    bridge.send(&format!("FSESSION 1 CMD :{sync}"));
    assert!(matches!(carol.recv().await.event, Event::Group { .. }));

    // carol posts with a label → relayed to the home, carrying an echo token.
    carol.send(&format!("@label=post MSG &{G} :hello"));
    let relayed = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("relay")
        .expect("sink open");
    let weft_proto::Command::GroupRelay {
        echo: Some(token),
        msgid: None,
        sender,
        ..
    } = weft_proto::Request::parse(&relayed.line).unwrap().command
    else {
        panic!(
            "expected a spoke relay with an echo token, got {:?}",
            relayed.line
        );
    };
    assert_eq!(sender.to_string(), "carol@test.example");

    // The home mints + echoes it back to us with the SAME token.
    let echoed = weft_proto::Request::new(weft_proto::Command::GroupRelay {
        group: G.parse().unwrap(),
        sender: "carol@test.example".parse().unwrap(),
        msgid: Some("peer.example/01ARZ3NDEKTSV4RRFFQ69G5FB0".parse().unwrap()),
        body: "hello".to_string(),
        meta: weft_proto::MsgMeta::default(),
        echo: Some(token),
    })
    .serialize()
    .unwrap();
    bridge.send(&format!("FSESSION 1 CMD :{echoed}"));

    // carol receives her message WITH the label — the ack correlates.
    let reply = carol.recv().await;
    assert_eq!(reply.label.as_deref(), Some("post"));
    match reply.event {
        Event::Message(m) => assert_eq!(m.body, "hello"),
        e => panic!("expected labelled message, got {e:?}"),
    }
}

#[tokio::test]
async fn spoke_requests_group_backfill_on_history() {
    // A member (spoke) viewing a cross-network group's history asks the home to
    // replay anything it missed while unreachable — carrying its cursor (`None`
    // here, since it has no local messages yet).
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "peer.example", &peer_key.public());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.set_friend_deliver_sink(tx);
    let mut carol = ready(&ctx, "carol").await;

    const G: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    bridge.send("FSESSION 1 OPEN alice");
    let sync = weft_proto::Request::new(weft_proto::Command::GroupSync {
        group: G.parse().unwrap(),
        creator: "alice@peer.example".parse().unwrap(),
        name: None,
        members: vec![
            "alice@peer.example".parse().unwrap(),
            "carol@test.example".parse().unwrap(),
        ],
    })
    .serialize()
    .unwrap();
    bridge.send(&format!("FSESSION 1 CMD :{sync}"));
    assert!(matches!(carol.recv().await.event, Event::Group { .. }));

    // Viewing history triggers the catch-up request to the home.
    carol.send(&format!("HISTORY &{G}"));
    let req = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("backfill request")
        .expect("sink open");
    assert_eq!(req.peer.as_str(), "peer.example");
    let weft_proto::Command::GroupBackfill { group, after } =
        weft_proto::Request::parse(&req.line).unwrap().command
    else {
        panic!("expected GROUP BACKFILL, got {:?}", req.line);
    };
    assert_eq!(group.to_string(), format!("&{G}"));
    assert!(after.is_none(), "no local messages yet ⇒ full replay");
}

#[tokio::test]
async fn home_serves_group_backfill_replaying_missed_messages() {
    // The home replays its group messages after a member's cursor as GROUP RELAY
    // ingests — the recovery path for a member that was down when they were minted.
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "peer.example", &peer_key.public());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.set_friend_deliver_sink(tx);
    let mut carol = ready(&ctx, "carol").await;

    macro_rules! sink_line {
        ($needle:literal) => {{
            loop {
                let d = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                    .await
                    .expect("delivery")
                    .expect("sink open");
                if d.line.contains($needle) {
                    break d;
                }
            }
        }};
    }

    // carol (home) creates a group with a remote member and posts two messages.
    carol.send("GROUP CREATE bob@peer.example");
    let gid = match carol.recv().await.event {
        Event::Group { id, .. } => id.to_string(),
        e => panic!("expected GROUP, got {e:?}"),
    };
    let _ = sink_line!("GROUP SYNC"); // membership propagation

    carol.send(&format!("MSG {gid} :first"));
    let m1 = match carol.recv().await.event {
        Event::Message(m) => m.msgid.to_string(),
        e => panic!("expected echo, got {e:?}"),
    };
    let _ = sink_line!("first"); // fanned out to peer
    carol.send(&format!("MSG {gid} :second"));
    assert!(matches!(carol.recv().await.event, Event::Message(_)));
    let _ = sink_line!("second");

    // The peer, catching bob up, asks for everything after the first message.
    let bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    bridge.send("FSESSION 1 OPEN bob");
    let backfill = weft_proto::Request::new(weft_proto::Command::GroupBackfill {
        group: gid.parse().unwrap(),
        after: Some(m1.parse().unwrap()),
    })
    .serialize()
    .unwrap();
    bridge.send(&format!("FSESSION 1 CMD :{backfill}"));

    // The home replays the second message (only) as a home-minted GROUP RELAY.
    let replay = sink_line!("GROUP RELAY");
    assert_eq!(replay.peer.as_str(), "peer.example");
    let weft_proto::Command::GroupRelay { body, msgid, .. } =
        weft_proto::Request::parse(&replay.line).unwrap().command
    else {
        panic!("expected GROUP RELAY, got {:?}", replay.line);
    };
    assert_eq!(body, "second");
    assert!(msgid.is_some(), "a replay carries the home-minted @id");
}

#[tokio::test]
async fn cross_network_group_attachment_is_mirrored() {
    // §11.8: ingesting a cross-network group message with a foreign attachment
    // requests a mirror pull from the blob's origin network.
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "peer.example", &peer_key.public());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.set_mirror_sink(tx);
    let mut carol = ready(&ctx, "carol").await;

    const G: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    bridge.send("FSESSION 1 OPEN alice");
    let sync = weft_proto::Request::new(weft_proto::Command::GroupSync {
        group: G.parse().unwrap(),
        creator: "alice@peer.example".parse().unwrap(),
        name: None,
        members: vec![
            "alice@peer.example".parse().unwrap(),
            "carol@test.example".parse().unwrap(),
        ],
    })
    .serialize()
    .unwrap();
    bridge.send(&format!("FSESSION 1 CMD :{sync}"));
    assert!(matches!(carol.recv().await.event, Event::Group { .. }));

    // A home-minted message carrying an attachment hosted on a THIRD network.
    let relay = weft_proto::Request::new(weft_proto::Command::GroupRelay {
        group: G.parse().unwrap(),
        sender: "alice@peer.example".parse().unwrap(),
        msgid: Some("peer.example/01ARZ3NDEKTSV4RRFFQ69G5FB0".parse().unwrap()),
        body: "look at this".to_string(),
        meta: weft_proto::MsgMeta {
            attachments: vec!["weft-media://media.example/deadbeef".to_string()],
            ..Default::default()
        },
        echo: None,
    })
    .serialize()
    .unwrap();
    bridge.send(&format!("FSESSION 1 CMD :{relay}"));

    // The blob is pulled from its origin network (media.example), not the peer.
    let req = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("mirror request")
        .expect("sink open");
    assert_eq!(req.peer.as_str(), "media.example");
    assert_eq!(req.hash, "deadbeef");

    // And the message still reaches carol.
    match carol.recv().await.event {
        Event::Message(m) => {
            assert_eq!(m.body, "look at this");
            assert_eq!(
                m.meta.attachments,
                vec!["weft-media://media.example/deadbeef"]
            );
        }
        e => panic!("expected message, got {e:?}"),
    }
}

#[tokio::test]
async fn cross_network_group_edit_home_applies_and_fans_out() {
    // The home applies a group message mutation and fans the minted mutation out
    // to every member network (§11.4 — mutations at the origin).
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "peer.example", &peer_key.public());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.set_friend_deliver_sink(tx);
    let mut carol = ready(&ctx, "carol").await;

    macro_rules! sink_line {
        ($needle:literal) => {{
            loop {
                let d = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                    .await
                    .expect("delivery")
                    .expect("sink open");
                if d.line.contains($needle) {
                    break d;
                }
            }
        }};
    }

    carol.send("GROUP CREATE alice@peer.example");
    let gid = match carol.recv().await.event {
        Event::Group { id, .. } => id.to_string(),
        e => panic!("expected GROUP, got {e:?}"),
    };

    // Post (home mints), capture the msgid.
    carol.send(&format!("MSG {gid} :orig"));
    let mid = match carol.recv().await.event {
        Event::Message(m) => m.msgid.to_string(),
        e => panic!("expected echo, got {e:?}"),
    };

    // Edit → home applies (carol gets EDITED) + fans a GROUP MUT out to peer.
    carol.send(&format!("EDIT {mid} :fixed"));
    match carol.recv().await.event {
        Event::Edited { body, target, .. } => {
            assert_eq!(body, "fixed");
            assert_eq!(target.to_string(), gid);
        }
        e => panic!("expected EDITED echo, got {e:?}"),
    }
    let muts = sink_line!("GROUP MUT");
    assert_eq!(muts.peer.as_str(), "peer.example");
    let weft_proto::Command::GroupMut {
        op,
        arg,
        msgid,
        root,
        ..
    } = weft_proto::Request::parse(&muts.line).unwrap().command
    else {
        panic!("expected GROUP MUT, got {:?}", muts.line);
    };
    assert_eq!(op, "edit");
    assert_eq!(arg, "fixed");
    assert!(msgid.is_some(), "home-minted mutation carries @id");
    assert_eq!(root.to_string(), mid);
}

#[tokio::test]
async fn cross_network_group_mutation_spoke_ingests_and_relays() {
    // Spoke side: a home-minted EDITED is ingested + delivered; a local author's
    // edit is relayed to the home (no @id).
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "peer.example", &peer_key.public());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.set_friend_deliver_sink(tx);
    let mut carol = ready(&ctx, "carol").await;

    const G: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    macro_rules! cmd {
        ($c:expr) => {{
            let line = weft_proto::Request::new($c).serialize().unwrap();
            bridge.send(&format!("FSESSION 1 CMD :{line}"));
        }};
    }
    bridge.send("FSESSION 1 OPEN alice");

    // Sync the group (home = peer, via alice@peer).
    cmd!(weft_proto::Command::GroupSync {
        group: G.parse().unwrap(),
        creator: "alice@peer.example".parse().unwrap(),
        name: None,
        members: vec![
            "alice@peer.example".parse().unwrap(),
            "carol@test.example".parse().unwrap(),
        ],
    });
    assert!(matches!(carol.recv().await.event, Event::Group { .. }));

    // Home minted a message authored by carol (relayed earlier) → ingest.
    const MID: &str = "peer.example/01ARZ3NDEKTSV4RRFFQ69G5FB0";
    cmd!(weft_proto::Command::GroupRelay {
        group: G.parse().unwrap(),
        sender: "carol@test.example".parse().unwrap(),
        msgid: Some(MID.parse().unwrap()),
        body: "orig".to_string(),
        meta: weft_proto::MsgMeta::default(),
        echo: None,
    });
    assert!(matches!(carol.recv().await.event, Event::Message(_)));

    // Home minted an EDIT of it → ingest → carol sees EDITED.
    cmd!(weft_proto::Command::GroupMut {
        group: G.parse().unwrap(),
        sender: "carol@test.example".parse().unwrap(),
        root: MID.parse().unwrap(),
        op: "edit".to_string(),
        arg: "home-fixed".to_string(),
        msgid: Some("peer.example/01ARZ3NDEKTSV4RRFFQ69G5FB4".parse().unwrap()),
    });
    match carol.recv().await.event {
        Event::Edited { body, .. } => assert_eq!(body, "home-fixed"),
        e => panic!("expected ingested EDITED, got {e:?}"),
    }

    // carol (the author) edits it herself → we relay to the home (no @id).
    carol.send(&format!("EDIT {MID} :carol-fixed"));
    let relayed = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("relay")
        .expect("sink open");
    assert_eq!(relayed.peer.as_str(), "peer.example");
    let weft_proto::Command::GroupMut { op, arg, msgid, .. } =
        weft_proto::Request::parse(&relayed.line).unwrap().command
    else {
        panic!("expected GROUP MUT relay, got {:?}", relayed.line);
    };
    assert_eq!(op, "edit");
    assert_eq!(arg, "carol-fixed");
    assert!(msgid.is_none(), "a spoke's relay carries no @id");
}

#[tokio::test]
async fn cross_network_group_message_spoke_ingests() {
    // The receiving side of a foreign-home group: a home-minted message (@id) is
    // ingested and delivered to our local member.
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "peer.example", &peer_key.public());
    let mut carol = ready(&ctx, "carol").await;

    // peer is the home: sync a group whose creator is alice@peer, with carol@test.
    const G: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let sync = weft_proto::Request::new(weft_proto::Command::GroupSync {
        group: G.parse().unwrap(),
        creator: "alice@peer.example".parse().unwrap(),
        name: None,
        members: vec![
            "alice@peer.example".parse().unwrap(),
            "carol@test.example".parse().unwrap(),
        ],
    })
    .serialize()
    .unwrap();
    let bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    bridge.send("FSESSION 1 OPEN alice");
    bridge.send(&format!("FSESSION 1 CMD :{sync}"));

    // carol is told the group exists.
    match carol.recv().await.event {
        Event::Group { id, members, .. } => {
            assert_eq!(id.to_string(), format!("&{G}"));
            assert_eq!(members.len(), 2);
        }
        e => panic!("expected GROUP, got {e:?}"),
    }

    // peer (home) sends a minted message (a threaded reply) → we ingest + deliver
    // to carol, meta intact.
    let relay = weft_proto::Request::new(weft_proto::Command::GroupRelay {
        group: G.parse().unwrap(),
        sender: "alice@peer.example".parse().unwrap(),
        msgid: Some("peer.example/01ARZ3NDEKTSV4RRFFQ69G5FB0".parse().unwrap()),
        body: "minted upstream".to_string(),
        meta: weft_proto::MsgMeta {
            reply_to: Some("peer.example/01ARZ3NDEKTSV4RRFFQ69G5FB2".parse().unwrap()),
            thread: Some("peer.example/01ARZ3NDEKTSV4RRFFQ69G5FB2".parse().unwrap()),
            ..Default::default()
        },
        echo: None,
    })
    .serialize()
    .unwrap();
    bridge.send(&format!("FSESSION 1 CMD :{relay}"));

    match carol.recv().await.event {
        Event::Message(m) => {
            assert_eq!(m.sender.to_string(), "alice@peer.example");
            assert_eq!(m.body, "minted upstream");
            assert_eq!(
                m.msgid.to_string(),
                "peer.example/01ARZ3NDEKTSV4RRFFQ69G5FB0"
            );
            // Reply + thread meta crossed the network boundary.
            assert_eq!(
                m.meta.reply_to.map(|r| r.to_string()).as_deref(),
                Some("peer.example/01ARZ3NDEKTSV4RRFFQ69G5FB2")
            );
            assert!(m.meta.thread.is_some());
        }
        e => panic!("expected ingested message, got {e:?}"),
    }
}

#[tokio::test]
async fn friend_request_to_remote_user_is_tunnelled() {
    // Send side of cross-network friends: a local user friending a user on
    // another network records the edge locally AND hands weftd a delivery to
    // tunnel the command to the peer (§11.10 home-side driver).
    let ctx = ctx(&["#general"]);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.set_friend_deliver_sink(tx);
    let mut ada = ready(&ctx, "ada").await;

    ada.send("FRIEND ADD bob@peer.example");
    // ada's own state records `outgoing` locally.
    match ada.recv().await.event {
        Event::Friend { user, state } => {
            assert_eq!(user.to_string(), "bob@peer.example");
            assert_eq!(state, FriendState::Outgoing);
        }
        e => panic!("expected FRIEND outgoing, got {e:?}"),
    }
    // And the command is handed to weftd's tunnel driver for the peer network.
    let req = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("friend delivery")
        .expect("sink open");
    assert_eq!(req.peer.as_str(), "peer.example");
    assert_eq!(req.from.to_string(), "ada");
    assert_eq!(req.line, "FRIEND ADD bob@peer.example");

    // A purely *local* friend request is NOT tunnelled anywhere.
    ada.send("FRIEND ADD carol@test.example");
    assert!(matches!(ada.recv().await.event, Event::Friend { .. }));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .is_err(),
        "a local friend request must not hit the tunnel sink"
    );
}

#[tokio::test]
async fn threads_list_naming_and_unknown_root() {
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;

    let root = say(&mut ada, "#general", "thread root").await;
    ada.send(&format!("@thread={root} MSG #general :reply one"));
    assert!(matches!(ada.recv().await.event, Event::Message(_)));
    ada.send(&format!("@thread={root} MSG #general :reply two"));
    assert!(matches!(ada.recv().await.event, Event::Message(_)));
    // An unrelated (non-thread) message must not become a thread.
    say(&mut ada, "#general", "unrelated chatter").await;

    // THREADS lists exactly the one thread, two replies, unnamed.
    ada.send("@label=t1 THREADS #general");
    let start = ada.recv().await;
    assert_eq!(start.label.as_deref(), Some("t1"));
    assert!(matches!(start.event, Event::BatchStart { .. }));
    let mut threads = Vec::new();
    loop {
        match ada.recv().await.event {
            Event::Thread {
                root,
                replies,
                name,
                ..
            } => threads.push((root.to_string(), replies, name)),
            Event::BatchEnd { .. } => break,
            _ => {}
        }
    }
    assert_eq!(threads.len(), 1, "one active thread");
    assert_eq!(threads[0].0, root);
    assert_eq!(threads[0].1, 2);
    assert_eq!(threads[0].2, None, "unnamed until set");

    // Naming broadcasts THREAD-NAMED to the channel (ada is a member).
    ada.send(&format!("THREAD NAME #general {root} :Release planning"));
    match ada.recv().await.event {
        Event::ThreadNamed { name, .. } => assert_eq!(name.as_deref(), Some("Release planning")),
        e => panic!("expected THREAD-NAMED, got {e:?}"),
    }

    // The name now shows up in the listing.
    ada.send("THREADS #general");
    assert!(matches!(ada.recv().await.event, Event::BatchStart { .. }));
    match ada.recv().await.event {
        Event::Thread { name, .. } => assert_eq!(name.as_deref(), Some("Release planning")),
        e => panic!("expected THREAD, got {e:?}"),
    }
    assert!(matches!(ada.recv().await.event, Event::BatchEnd { .. }));

    // Clearing the name (no trailing) keeps the thread but drops the label.
    ada.send(&format!("THREAD NAME #general {root}"));
    assert!(matches!(
        ada.recv().await.event,
        Event::ThreadNamed { name: None, .. }
    ));

    // Naming an unknown root is NO-SUCH-TARGET (anti-enumeration, invariant 1).
    ada.send("THREAD NAME #general test.example/01ARZ3NDEKTSV4RRFFQ69G5FAV :nope");
    ada.expect_err(ErrCode::NoSuchTarget).await;
}

#[tokio::test]
async fn custom_emoji_add_list_remove_and_gating() {
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    // ada creates a namespace → she owns it (holds ns-admin there).
    ada.send(&format!("@root={} NS CREATE gaming public", root_key_b64()));
    assert!(matches!(ada.recv().await.event, Event::NsMeta { .. }));

    // Owner adds two emoji.
    ada.send("EMOJI ADD gaming partyblob weft-media://test.example/aaa");
    assert!(matches!(&ada.recv().await.event, Event::Emoji { name, .. } if name == "partyblob"));
    ada.send("EMOJI ADD gaming catjam weft-media://test.example/bbb");
    assert!(matches!(ada.recv().await.event, Event::Emoji { .. }));

    // List → a BATCH of both.
    ada.send("@label=el EMOJI LIST gaming");
    assert!(matches!(ada.recv().await.event, Event::BatchStart { .. }));
    let mut names = Vec::new();
    loop {
        match ada.recv().await.event {
            Event::Emoji { name, .. } => names.push(name),
            Event::BatchEnd { .. } => break,
            _ => {}
        }
    }
    names.sort();
    assert_eq!(names, vec!["catjam".to_string(), "partyblob".to_string()]);

    // Remove one.
    ada.send("EMOJI REMOVE gaming catjam");
    assert!(matches!(ada.recv().await.event, Event::EmojiRemoved { .. }));

    // An invalid shortcode is rejected regardless of authority.
    ada.send("EMOJI ADD gaming bad-name! weft-media://x/y");
    ada.expect_err(ErrCode::Policy).await;

    // A non-admin can't add (ns-admin gate).
    let mut bob = joined(&ctx, "bob", "#general").await;
    bob.send("EMOJI ADD gaming sneaky weft-media://x/y");
    bob.expect_err(ErrCode::CapRequired).await;
}

#[tokio::test]
async fn presence_relays_to_co_members_but_never_invisible() {
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    let mut bob = joined(&ctx, "bob", "#general").await;
    ada.recv().await; // bob's join broadcast

    bob.send("PRESENCE away");
    let reply = ada.recv().await;
    assert!(
        matches!(&reply.event, Event::Presence { user, status, .. }
            if user.to_string() == "bob@test.example" && status.to_string() == "away"),
        "got {reply:?}"
    );

    // §6.1: invisible renders offline — it must NOT be relayed.
    bob.send("PRESENCE invisible");
    bob.send("PING check");
    assert!(matches!(bob.recv().await.event, Event::Pong { .. }));
    ada.send("PING probe");
    assert!(
        matches!(ada.recv().await.event, Event::Pong { .. }),
        "ada must see no PRESENCE for invisible"
    );
}

// ---- M4a: capabilities, channels, invites, view gating ----

/// Authenticate an operator (holds every cap at `*`).
async fn ready_op(ctx: &Arc<ServerCtx>, account: &str) -> Client {
    ready(ctx, account).await
}

#[tokio::test]
async fn grant_lets_a_member_use_an_elevated_cap() {
    let ctx = ctx_ops(&["#general"], &["boss"]);
    // Non-operator ada cannot create channels...
    let mut ada = joined(&ctx, "ada", "#general").await;
    ada.send("@label=c1 CHANNEL CREATE #ada-chan");
    let reply = ada.expect_err(ErrCode::CapRequired).await;
    let Event::Err(err) = &reply.event else {
        unreachable!()
    };
    assert_eq!(err.context.as_deref(), Some("chan-create"));

    // ...until the operator grants chan-create at `*`.
    let mut boss = ready_op(&ctx, "boss").await;
    boss.send("@label=g1 GRANT ada * chan-create");
    let reply = boss.recv().await;
    assert_eq!(reply.label.as_deref(), Some("g1"));
    assert!(matches!(&reply.event, Event::Token { subject, .. } if subject == "ada"));

    // Now ada can create.
    ada.send("@label=c2 CHANNEL CREATE #ada-chan retained:30d");
    let reply = ada.recv().await;
    assert_eq!(reply.label.as_deref(), Some("c2"));
    assert!(matches!(&reply.event, Event::Policy { channel, policy }
            if channel.as_str() == "#ada-chan" && policy.to_string() == "retained:30d"));
    // And join the channel she made.
    ada.send("JOIN #ada-chan");
    assert!(matches!(ada.recv().await.event, Event::Member { .. }));
}

#[tokio::test]
async fn revoke_and_epoch_bump_remove_authority() {
    let ctx = ctx_ops(&["#general"], &["boss"]);
    let mut boss = ready_op(&ctx, "boss").await;
    let mut ada = joined(&ctx, "ada", "#general").await;

    boss.send("GRANT ada * chan-create");
    boss.recv().await; // TOKEN
    ada.send("CHANNEL CREATE #x1");
    assert!(matches!(ada.recv().await.event, Event::Policy { .. }));

    // Revoke it; ada loses the cap.
    boss.send("@label=r1 REVOKE ada * chan-create");
    let reply = boss.recv().await;
    assert_eq!(reply.label.as_deref(), Some("r1"));
    assert!(matches!(&reply.event, Event::Token { .. })); // reflects remaining (none)
    ada.send("CHANNEL CREATE #x2");
    ada.expect_err(ErrCode::CapRequired).await;
}

#[tokio::test]
async fn only_operators_bootstrap_grants() {
    let ctx = ctx_ops(&["#general"], &["boss"]);
    // A plain member cannot grant caps they don't hold grant: for.
    let mut ada = joined(&ctx, "ada", "#general").await;
    ada.send("@label=g GRANT bob * chan-create");
    let reply = ada.expect_err(ErrCode::CapRequired).await;
    let Event::Err(err) = &reply.event else {
        unreachable!()
    };
    assert_eq!(err.context.as_deref(), Some("grant:chan-create"));
}

#[tokio::test]
async fn channel_policy_and_delete_require_caps() {
    let ctx = ctx_ops(&["#general"], &["boss"]);
    let mut boss = ready_op(&ctx, "boss").await;

    // Operator creates and reconfigures a channel.
    boss.send("CHANNEL CREATE #ops");
    boss.recv().await;
    boss.send("@label=p1 CHANNEL POLICY #ops ephemeral");
    let reply = boss.recv().await;
    assert_eq!(reply.label.as_deref(), Some("p1"));
    assert!(
        matches!(&reply.event, Event::Policy { policy, .. } if policy.to_string() == "ephemeral")
    );

    // META view-gated.
    boss.send("@label=m1 CHANNEL META #ops view-gated :yes");
    let reply = boss.recv().await;
    assert!(matches!(&reply.event, Event::Chanmeta { key, .. } if key == "view-gated"));

    // DELETE requires the confirmation to match.
    boss.send("CHANNEL DELETE #ops #wrong");
    boss.expect_err(ErrCode::Policy).await;
    boss.send("@label=d1 CHANNEL DELETE #ops #ops");
    let reply = boss.recv().await;
    assert!(matches!(&reply.event, Event::Chanmeta { key, .. } if key == "deleted"));
    // Gone: joining now is NO-SUCH-TARGET.
    boss.send("JOIN #ops");
    boss.expect_err(ErrCode::NoSuchTarget).await;
}

#[tokio::test]
async fn view_gated_channel_hides_without_the_view_cap() {
    let ctx = ctx_ops(&["#general"], &["boss"]);
    let mut boss = ready_op(&ctx, "boss").await;
    boss.send("CHANNEL CREATE #secret");
    boss.recv().await;
    boss.send("CHANNEL META #secret view-gated :yes");
    boss.recv().await;

    // A plain account can't even tell it exists (invariant 1).
    let mut ada = ready(&ctx, "ada").await;
    ada.send("@label=j JOIN #secret");
    let reply = ada.expect_err(ErrCode::NoSuchTarget).await;
    assert_eq!(reply.label.as_deref(), Some("j"));

    // Grant view → it becomes reachable.
    boss.send("GRANT ada #secret view");
    boss.recv().await;
    ada.send("JOIN #secret");
    assert!(matches!(ada.recv().await.event, Event::Member { .. }));
}

#[tokio::test]
async fn invite_mint_and_redeem_grants_membership() {
    let ctx = ctx_ops(&["#general"], &["boss"]);
    let mut boss = ready_op(&ctx, "boss").await;
    boss.send("CHANNEL CREATE #club");
    boss.recv().await;
    boss.send("CHANNEL META #club view-gated :yes");
    boss.recv().await;

    // Mint a 1-use invite for the gated channel.
    boss.send("@label=i1 INVITE MINT #club max-uses=1");
    let reply = boss.recv().await;
    assert_eq!(reply.label.as_deref(), Some("i1"));
    let Event::Invited {
        invite_id, token, ..
    } = &reply.event
    else {
        panic!("expected INVITED, got {reply:?}");
    };
    assert_eq!(invite_id, token);
    let id = invite_id.clone();

    // Ada can't join the gated channel directly...
    let mut ada = ready(&ctx, "ada").await;
    ada.send("JOIN #club");
    ada.expect_err(ErrCode::NoSuchTarget).await;
    // ...but redeeming the invite grants membership and auto-joins.
    ada.send(&format!("@label=rd INVITE REDEEM {id}"));
    let reply = ada.recv().await;
    assert!(
        matches!(&reply.event, Event::Member { user, .. } if user.account.as_str() == "ada"),
        "redeem should auto-join, got {reply:?}"
    );
    assert!(matches!(ada.recv().await.event, Event::Policy { .. }));

    // Second redeem: counter exhausted → NO-SUCH-TARGET (§2.2).
    let mut bob = ready(&ctx, "bob").await;
    bob.send(&format!("INVITE REDEEM {id}"));
    bob.expect_err(ErrCode::NoSuchTarget).await;
}

#[tokio::test]
async fn invite_link_carries_namespace_for_federation() {
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@root={} NS CREATE gaming public", root_key_b64()));
    ada.recv().await;

    // A namespace-scoped invite's link carries the namespace (§11.10), so a
    // foreign redeemer can auto-federate to it.
    ada.send("INVITE MINT ns:gaming");
    let Event::Invited { link, .. } = ada.recv().await.event else {
        panic!("expected INVITED");
    };
    let link = link.expect("a namespace invite should carry a link");
    assert!(
        link.starts_with("weft://test.example/gaming/i/"),
        "link must carry the namespace: {link}"
    );
}

#[tokio::test]
async fn invite_revoke_kills_the_link() {
    let ctx = ctx_ops(&["#general"], &["boss"]);
    let mut boss = ready_op(&ctx, "boss").await;
    boss.send("CHANNEL CREATE #club");
    boss.recv().await;
    boss.send("INVITE MINT #club");
    let Event::Invited { invite_id, .. } = boss.recv().await.event else {
        panic!()
    };
    boss.send(&format!("@label=rv INVITE REVOKE {invite_id}"));
    let reply = boss.recv().await;
    assert!(matches!(
        &reply.event,
        Event::Invited {
            max_uses: Some(0),
            ..
        }
    ));

    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("INVITE REDEEM {invite_id}"));
    ada.expect_err(ErrCode::NoSuchTarget).await;
}

// ---- M4-5: user-owned namespaces + DISCOVER ----

/// A fresh ed25519 pubkey (b64) to serve as a namespace root key.
fn root_key_b64() -> String {
    Keypair::generate().public().to_b64()
}

#[tokio::test]
async fn any_user_can_create_a_namespace_and_owns_it() {
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    let root = root_key_b64();
    ada.send(&format!("@label=n1;root={root} NS CREATE gaming public"));
    let reply = ada.recv().await;
    assert_eq!(reply.label.as_deref(), Some("n1"));
    let Event::NsMeta {
        name,
        visibility,
        owner,
        ..
    } = &reply.event
    else {
        panic!("expected NS-META, got {reply:?}");
    };
    assert_eq!(name.as_str(), "gaming");
    assert_eq!(visibility.to_string(), "public");
    assert_eq!(owner.as_deref(), Some("ada"));

    // As owner, ada holds every cap in her namespace — she can create a
    // namespaced channel (deferred in M4a, unlocked by ownership).
    ada.send("@label=c1 CHANNEL CREATE #gaming/general");
    let reply = ada.recv().await;
    assert!(
        matches!(&reply.event, Event::Policy { channel, .. } if channel.as_str() == "#gaming/general"),
        "owner should create channels in her ns, got {reply:?}"
    );

    // ...and delegate ns caps to someone else (who must exist — caps key by the
    // target's ULID, §10.4).
    let _bob = ready(&ctx, "bob").await;
    ada.send("@label=d1 NS DELEGATE gaming bob ban,kick");
    assert!(matches!(ada.recv().await.event, Event::Token { .. }));

    // A non-owner cannot create channels in the namespace.
    let mut eve = ready(&ctx, "eve").await;
    eve.send("CHANNEL CREATE #gaming/secret");
    eve.expect_err(ErrCode::CapRequired).await;
}

#[tokio::test]
async fn grant_accepts_a_foreign_subject() {
    let ctx = ctx_ops(&["#general"], &["boss"]);
    let mut boss = ready_op(&ctx, "boss").await;
    // An operator can grant caps to a federated user (`account@network`) — keyed
    // by the network-qualified handle, since H doesn't own her ULID (§10.4). The
    // token mints; enforcement rides the later federation-session work.
    boss.send("@label=g1 GRANT alice@peer.example #general send");
    let reply = boss.recv().await;
    assert!(
        matches!(&reply.event, Event::Token { subject, .. } if subject == "alice@peer.example"),
        "granting to a foreign subject should mint a token, got {reply:?}"
    );
}

#[tokio::test]
async fn grant_to_a_nonexistent_account_is_rejected() {
    let ctx = ctx_ops(&["#general"], &["boss"]);
    let mut boss = ready_op(&ctx, "boss").await;
    // Caps key by ULID, so there's no identity to grant to until the account
    // exists (§10.4) — anti-enumeration NO-SUCH-TARGET, uniform with private.
    boss.send("GRANT ghost #general send");
    boss.expect_err(ErrCode::NoSuchTarget).await;
}

#[tokio::test]
async fn namespace_name_conflicts() {
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@root={} NS CREATE gaming", root_key_b64()));
    assert!(matches!(ada.recv().await.event, Event::NsMeta { .. }));
    let mut bob = ready(&ctx, "bob").await;
    bob.send(&format!("@root={} NS CREATE gaming", root_key_b64()));
    bob.expect_err(ErrCode::Conflict).await;
}

#[tokio::test]
async fn namespace_meta_and_visibility_are_owner_only() {
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!(
        "@root={} NS CREATE gaming unlisted",
        root_key_b64()
    ));
    ada.recv().await;

    ada.send("@label=m1 NS META gaming title :The Gaming Lounge");
    let reply = ada.recv().await;
    assert!(
        matches!(&reply.event, Event::NsMeta { title: Some(t), .. } if t == "The Gaming Lounge")
    );
    ada.send("@label=v1 NS VISIBILITY gaming public");
    assert!(
        matches!(&ada.recv().await.event, Event::NsMeta { visibility, .. } if visibility.to_string() == "public")
    );

    // A non-owner can't administer it.
    let mut eve = ready(&ctx, "eve").await;
    eve.send("NS META gaming title :hijacked");
    eve.expect_err(ErrCode::CapRequired).await;
    // ...and a nonexistent namespace is NO-SUCH-TARGET.
    eve.send("NS META ghost title :x");
    eve.expect_err(ErrCode::NoSuchTarget).await;
}

#[tokio::test]
async fn discover_lists_only_public_namespaces() {
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@root={} NS CREATE alpha public", root_key_b64()));
    ada.recv().await;
    ada.send(&format!(
        "@root={} NS CREATE bravo unlisted",
        root_key_b64()
    ));
    ada.recv().await;
    ada.send(&format!(
        "@root={} NS CREATE charlie public",
        root_key_b64()
    ));
    ada.recv().await;

    let mut eve = ready(&ctx, "eve").await;
    eve.send("@label=disc DISCOVER");
    // Public namespaces only, name-sorted; no BATCH bracket for DISCOVER.
    let mut seen = Vec::new();
    loop {
        let reply = eve.recv().await;
        match reply.event {
            Event::NsMeta {
                name, visibility, ..
            } => {
                assert_eq!(visibility.to_string(), "public");
                seen.push(name.to_string());
            }
            Event::More { .. } => continue,
            other => panic!("unexpected in DISCOVER: {other:?}"),
        }
        if seen.len() == 2 {
            break;
        }
    }
    assert_eq!(seen, vec!["alpha", "charlie"]); // bravo is unlisted
}

#[tokio::test]
async fn namespace_quota_is_enforced_when_open() {
    // Tiny quota via a custom ctx.
    let info = weft_core::ServerInfo {
        network: "test.example".parse().unwrap(),
        motd: None,
        features: Vec::new(),
    };
    let ctx = Arc::new(ServerCtx::new(
        info,
        std::iter::empty(),
        Keypair::generate(),
        true,
        Arc::new(MemoryStore::default()),
        Arc::new(weft_core::MemBlobStore::default()),
        "permanent".parse().unwrap(),
        std::iter::empty::<weft_proto::Account>(),
        true, // open
        1,    // quota of 1
        weft_core::FederationConfig::default(),
    ));
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@root={} NS CREATE first", root_key_b64()));
    assert!(matches!(ada.recv().await.event, Event::NsMeta { .. }));
    ada.send(&format!("@root={} NS CREATE second", root_key_b64()));
    ada.expect_err(ErrCode::Quota).await;
}

#[tokio::test]
async fn ns_create_rejects_a_bad_root_key() {
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    ada.send("@root=not-a-real-key NS CREATE gaming");
    ada.expect_err(ErrCode::Malformed).await;
}

#[tokio::test]
async fn channel_categories_and_ordering() {
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    // Own a namespace, then build a channel layout inside it.
    ada.send(&format!("@root={} NS CREATE team", root_key_b64()));
    ada.recv().await;
    for chan in ["#team/general", "#team/random", "#team/voice"] {
        ada.send(&format!("CHANNEL CREATE {chan}"));
        ada.recv().await; // POLICY
    }
    // Categorize + order: general/random under "text", voice uncategorized.
    ada.send("CHANNEL META #team/general category :text");
    assert!(matches!(&ada.recv().await.event, Event::Chanmeta { key, .. } if key == "category"));
    ada.send("CHANNEL META #team/general position :0");
    ada.recv().await;
    ada.send("CHANNEL META #team/random category :text");
    ada.recv().await;
    ada.send("CHANNEL META #team/random position :1");
    ada.recv().await;
    // Reorder: move random ahead of general.
    ada.send("CHANNEL META #team/random position :-1");
    ada.recv().await;

    // Read the layout back, ordered.
    ada.send("@label=cl CHANNELS team");
    let mut layout = Vec::new();
    while layout.len() < 3 {
        let reply = ada.recv().await;
        assert_eq!(reply.label.as_deref(), Some("cl"));
        // The response leads with the namespace's NS-META (categories, …).
        match reply.event {
            Event::ChannelLayout {
                channel,
                category,
                position,
                ..
            } => layout.push((channel.to_string(), category, position)),
            Event::NsMeta { .. } => {}
            other => panic!("expected CHANNEL-LAYOUT or NS-META, got {other:?}"),
        }
    }
    // voice (uncategorized) first, then text by position: random(-1) before general(0).
    assert_eq!(layout[0].0, "#team/voice");
    assert_eq!(
        layout[1],
        ("#team/random".to_string(), Some("text".to_string()), -1)
    );
    assert_eq!(
        layout[2],
        ("#team/general".to_string(), Some("text".to_string()), 0)
    );

    // Non-owner can set neither (needs pin cap in the ns).
    let mut eve = ready(&ctx, "eve").await;
    eve.send("CHANNEL META #team/general category :hijack");
    eve.expect_err(ErrCode::CapRequired).await;
    // ...but can read a public/unlisted namespace's layout (NS-META, then layout).
    eve.send("CHANNELS team");
    assert!(matches!(eve.recv().await.event, Event::NsMeta { .. }));
    assert!(matches!(
        eve.recv().await.event,
        Event::ChannelLayout { .. }
    ));
}

// ---- M4c: namespace recovery ladder (§2.4, invariant 9) ----

/// Create a namespace owned by `owner`, returning its root Keypair (held
/// client-side) so tests can sign transfer/recovery/cancel statements.
async fn make_namespace(ctx: &Arc<ServerCtx>, owner: &str, name: &str) -> (Client, Keypair) {
    let root = Keypair::generate();
    let mut client = ready(ctx, owner).await;
    client.send(
        &format!("root={} NS CREATE {name} unlisted", root.public().to_b64())
            .replace("root=", "@root="),
    );
    assert!(matches!(client.recv().await.event, Event::NsMeta { .. }));
    (client, root)
}

#[tokio::test]
async fn ns_transfer_is_root_signed_rung_one() {
    let ctx = ctx(&[]);
    let (mut ada, root) = make_namespace(&ctx, "ada", "gaming").await;

    // A forged signature is FORBIDDEN.
    ada.send("@sig=Zm9yZ2Vk NS TRANSFER gaming bob");
    let reply = ada.expect_err(ErrCode::Forbidden).await;
    let Event::Err(err) = &reply.event else {
        unreachable!()
    };
    assert_eq!(err.context.as_deref(), Some("signature"));

    // A real root signature transfers ownership immediately (no delay).
    let sig = weft_crypto::sign_transfer(&root, "gaming", "bob");
    ada.send(&format!(
        "@sig={} NS TRANSFER gaming bob",
        weft_crypto::signature_to_b64(&sig)
    ));
    let reply = ada.recv().await;
    assert!(matches!(&reply.event, Event::NsMeta { owner: Some(o), .. } if o == "bob"));

    // Bob is now the owner: he can administer, ada can't.
    let mut bob = ready(&ctx, "bob").await;
    bob.send("NS META gaming title :Bob's Lounge");
    assert!(matches!(bob.recv().await.event, Event::NsMeta { .. }));
    ada.send("NS META gaming title :ada's");
    ada.expect_err(ErrCode::CapRequired).await;
}

#[tokio::test]
async fn recovery_rung_two_quorum_then_cancel() {
    let ctx = ctx(&[]);
    let (mut ada, root) = make_namespace(&ctx, "ada", "gaming").await;
    // Designate a 2-of-3 quorum.
    let (q1, q2, q3) = (
        Keypair::generate(),
        Keypair::generate(),
        Keypair::generate(),
    );
    let keys = format!(
        "{},{},{}",
        q1.public().to_b64(),
        q2.public().to_b64(),
        q3.public().to_b64()
    );
    ada.send(&format!("NS RECOVERY SET gaming 2 {keys}"));
    let reply = ada.recv().await;
    assert!(matches!(
        &reply.event,
        Event::NsMeta {
            recovery_set: true,
            ..
        }
    ));

    // Two quorum members co-sign a rotation to a new root/owner.
    let new_root = Keypair::generate();
    let record = weft_crypto::RotationRecord {
        namespace: "gaming".into(),
        new_root_key: new_root.public(),
        new_owner: "carol".into(),
    };
    let signed = weft_crypto::SignedRotation {
        record: record.clone(),
        signatures: vec![record.sign(&q1), record.sign(&q2)],
    };
    ada.send(&format!("NS RECOVER gaming {}", signed.to_b64()));
    let reply = ada.recv().await;
    let Event::NsMeta {
        recovery_pending: Some((_, rung)),
        ..
    } = &reply.event
    else {
        panic!("expected recovery=pending, got {reply:?}");
    };
    assert_eq!(*rung, 2, "quorum → rung 2");

    // A second RECOVER while one is pending → CONFLICT.
    ada.send(&format!("NS RECOVER gaming {}", signed.to_b64()));
    ada.expect_err(ErrCode::Conflict).await;

    // The live root cancels it (a live root always wins, §2.4).
    let cancel = weft_crypto::sign_cancel(&root, "gaming");
    ada.send(&format!(
        "@sig={} NS RECOVERY CANCEL gaming",
        weft_crypto::signature_to_b64(&cancel)
    ));
    let reply = ada.recv().await;
    assert!(matches!(
        &reply.event,
        Event::NsMeta {
            recovery_pending: None,
            ..
        }
    ));
}

#[tokio::test]
async fn recovery_rejects_insufficient_or_wrong_signatures() {
    let ctx = ctx(&[]);
    let (mut ada, _root) = make_namespace(&ctx, "ada", "gaming").await;
    let (q1, q2) = (Keypair::generate(), Keypair::generate());
    ada.send(&format!(
        "NS RECOVERY SET gaming 2 {},{}",
        q1.public().to_b64(),
        q2.public().to_b64()
    ));
    ada.recv().await;

    // Only one quorum signature (need 2), and not operator-signed → FORBIDDEN.
    let new_root = Keypair::generate();
    let record = weft_crypto::RotationRecord {
        namespace: "gaming".into(),
        new_root_key: new_root.public(),
        new_owner: "carol".into(),
    };
    let under = weft_crypto::SignedRotation {
        record: record.clone(),
        signatures: vec![record.sign(&q1)],
    };
    ada.send(&format!("NS RECOVER gaming {}", under.to_b64()));
    ada.expect_err(ErrCode::Forbidden).await;

    // A rotation record for a *different* namespace is refused.
    let wrong = weft_crypto::RotationRecord {
        namespace: "other".into(),
        new_root_key: new_root.public(),
        new_owner: "carol".into(),
    };
    let wrong_signed = weft_crypto::SignedRotation {
        record: wrong.clone(),
        signatures: vec![wrong.sign(&q1), wrong.sign(&q2)],
    };
    ada.send(&format!("NS RECOVER gaming {}", wrong_signed.to_b64()));
    ada.expect_err(ErrCode::Forbidden).await;
}

#[tokio::test]
async fn recovery_applies_at_expiry_via_scheduler() {
    use weft_core::{apply_due_recoveries, NamespaceStore};
    // Build a ctx whose store we also hold, to drive the scheduler + inspect.
    let store = Arc::new(MemoryStore::default());
    let info = weft_core::ServerInfo {
        network: "test.example".parse().unwrap(),
        motd: None,
        features: Vec::new(),
    };
    let ctx = Arc::new(ServerCtx::new(
        info,
        std::iter::empty(),
        Keypair::generate(),
        true,
        Arc::clone(&store),
        Arc::new(weft_core::MemBlobStore::default()),
        "permanent".parse().unwrap(),
        std::iter::empty::<weft_proto::Account>(),
        true,
        10,
        weft_core::FederationConfig::default(),
    ));
    let root = Keypair::generate();
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!(
        "@root={} NS CREATE gaming unlisted",
        root.public().to_b64()
    ));
    ada.recv().await;
    let q1 = Keypair::generate();
    ada.send(&format!(
        "NS RECOVERY SET gaming 1 {}",
        q1.public().to_b64()
    ));
    ada.recv().await;

    let new_root = Keypair::generate();
    let record = weft_crypto::RotationRecord {
        namespace: "gaming".into(),
        new_root_key: new_root.public(),
        new_owner: "carol".into(),
    };
    let signed = weft_crypto::SignedRotation {
        record: record.clone(),
        signatures: vec![record.sign(&q1)],
    };
    ada.send(&format!("NS RECOVER gaming {}", signed.to_b64()));
    ada.recv().await; // pending

    let ns_name: weft_proto::NamespaceName = "gaming".parse().unwrap();
    let ns_store: Arc<dyn NamespaceStore> = store;
    // Not due yet (7-day window).
    assert_eq!(apply_due_recoveries(&ns_store, 0).await, 0);
    // Far-future now: the rotation applies.
    assert_eq!(apply_due_recoveries(&ns_store, u64::MAX).await, 1);
    let applied = ns_store.namespace(&ns_name).await.unwrap().unwrap();
    assert_eq!(applied.owner.as_str(), "carol");
    assert_eq!(applied.root_key, new_root.public().to_b64());
    assert!(applied.pending_recovery.is_none());
    // root-history records the rung-2 rotation (not operator-initiated).
    let history = ns_store.root_history(&ns_name).await.unwrap();
    assert_eq!(history.len(), 1);
    assert!(!history[0].operator_initiated);
}

#[tokio::test]
async fn operator_takeover_seizes_the_namespace_immediately() {
    use weft_core::NamespaceStore;
    // §2.4 rung 3, zero delay (Appendix A amendment). The moderation case: the
    // *owner* is the abuse, so the seizure must not sit in a window the owner
    // could veto. What survives is accountability, not delay — the rotation is
    // announced and permanently marked operator-initiated.
    let store = Arc::new(MemoryStore::default());
    let network_key = Keypair::generate();
    let info = weft_core::ServerInfo {
        network: "test.example".parse().unwrap(),
        motd: None,
        features: Vec::new(),
    };
    let ctx = Arc::new(ServerCtx::new(
        info,
        std::iter::empty(),
        Keypair::from_seed_b64(&network_key.seed_b64()).unwrap(),
        true,
        Arc::clone(&store),
        Arc::new(weft_core::MemBlobStore::default()),
        "permanent".parse().unwrap(),
        std::iter::empty::<weft_proto::Account>(),
        true,
        10,
        weft_core::FederationConfig::default(),
    ));

    let root = Keypair::generate();
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!(
        "@root={} NS CREATE gaming unlisted",
        root.public().to_b64()
    ));
    ada.recv().await;

    // The operator signs a rotation with the *network* key — that signature is
    // what makes it rung 3. No recovery set is configured, so rung 2 can't apply.
    let new_root = Keypair::generate();
    let record = weft_crypto::RotationRecord {
        namespace: "gaming".into(),
        new_root_key: new_root.public(),
        new_owner: "moderator".into(),
    };
    let signed = weft_crypto::SignedRotation {
        record: record.clone(),
        signatures: vec![record.sign(&network_key)],
    };
    ada.send(&format!("@label=r NS RECOVER gaming {}", signed.to_b64()));
    let reply = drain_until_label(&mut ada, "r").await;
    assert!(
        matches!(&reply.event, Event::NsMeta { .. }),
        "the takeover announces, got {reply:?}"
    );

    let ns_name: weft_proto::NamespaceName = "gaming".parse().unwrap();
    let ns_store: Arc<dyn NamespaceStore> = store;
    let seized = ns_store.namespace(&ns_name).await.unwrap().unwrap();
    // Applied *now* — not parked as pending for a scheduler tick.
    assert_eq!(seized.owner.as_str(), "moderator");
    assert_eq!(seized.root_key, new_root.public().to_b64());
    assert!(
        seized.pending_recovery.is_none(),
        "a zero-delay rung leaves no window to cancel"
    );
    // ...and there is nothing left for the scheduler to do.
    assert_eq!(
        weft_core::apply_due_recoveries(&ns_store, u64::MAX).await,
        0
    );

    // The permanent audit mark — the property that replaces the delay.
    let history = ns_store.root_history(&ns_name).await.unwrap();
    assert_eq!(history.len(), 1);
    assert!(
        history[0].operator_initiated,
        "a rung-3 seizure is marked operator-initiated forever"
    );
}

#[tokio::test]
async fn a_takeover_still_needs_the_network_key() {
    // The zero delay removes the *window*, never the authorization. A rotation
    // signed by a stranger is refused exactly as before.
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    let root = Keypair::generate();
    ada.send(&format!(
        "@root={} NS CREATE gaming unlisted",
        root.public().to_b64()
    ));
    ada.recv().await;

    let impostor = Keypair::generate();
    let record = weft_crypto::RotationRecord {
        namespace: "gaming".into(),
        new_root_key: Keypair::generate().public(),
        new_owner: "mallory".into(),
    };
    let signed = weft_crypto::SignedRotation {
        record: record.clone(),
        signatures: vec![record.sign(&impostor)],
    };
    ada.send(&format!("@label=x NS RECOVER gaming {}", signed.to_b64()));
    ada.expect_err(ErrCode::Forbidden).await;
}

// ---- §6.7 reporting + retention holds ----

#[tokio::test]
async fn report_flow_ack_queue_resolve_and_confidentiality() {
    let ctx = ctx_ops(&["#general"], &["op"]);
    let mut ada = joined(&ctx, "ada", "#general").await;

    ada.send("MSG #general :something bad");
    let Event::Message(msg) = ada.recv().await.event else {
        panic!("expected MESSAGE echo")
    };
    let mid = msg.msgid.to_string();

    // Reporter files (net scope) and gets a labeled REPORTED ack.
    ada.send(&format!("@label=r1 REPORT {mid} harassment net"));
    let ack = ada.recv().await;
    assert_eq!(ack.label.as_deref(), Some("r1"));
    let Event::Reported { report_id } = ack.event else {
        panic!("expected REPORTED, got {ack:?}")
    };

    // Operator connects afterwards and pulls the queue (§6.7).
    let mut op = ready(&ctx, "op").await;
    op.send("REPORTS LIST *");
    let filed = op.recv().await;
    let Event::ReportFiled {
        report_id: fid,
        reporter,
        state,
        ..
    } = &filed.event
    else {
        panic!("expected REPORT-FILED, got {filed:?}")
    };
    assert_eq!(fid, &report_id);
    // Handlers see the reporter (accountability, §6.7).
    assert_eq!(reporter.as_deref(), Some("ada"));
    assert_eq!(*state, weft_proto::ContentState::Verified);

    // Resolve: the handler's echo is the FULL form; the reporter's push is
    // the MINIMAL form — no handler identity, no note (§6.7 confidentiality).
    op.send(&format!(
        "REPORTS RESOLVE {report_id} user-actioned :banned 7d"
    ));
    let op_echo = op.recv().await;
    let Event::ReportResolved {
        by: Some(by),
        note: Some(note),
        ..
    } = &op_echo.event
    else {
        panic!("expected full REPORT-RESOLVED, got {op_echo:?}")
    };
    assert_eq!(by, "op");
    assert_eq!(note, "banned 7d");

    let ada_push = ada.recv().await;
    let Event::ReportResolved {
        report_id: rid,
        action,
        by,
        note,
    } = &ada_push.event
    else {
        panic!("expected REPORT-RESOLVED push, got {ada_push:?}")
    };
    assert_eq!(rid, &report_id);
    assert_eq!(*action, weft_proto::ResolveAction::UserActioned);
    assert_eq!(*by, None, "reporter must not learn the handler");
    assert_eq!(*note, None, "reporter must not see the resolution note");
}

#[tokio::test]
async fn report_unseen_message_is_no_such_target() {
    // Anti-enumeration (invariant 1): you can only report what you can see.
    let ctx = ctx(&["#general", "#secret"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    let mut bob = joined(&ctx, "bob", "#secret").await;

    bob.send("MSG #secret :hidden");
    let Event::Message(msg) = bob.recv().await.event else {
        panic!()
    };
    let mid = msg.msgid.to_string();

    // ada is not a member of #secret.
    ada.send(&format!("REPORT {mid} spam"));
    ada.expect_err(ErrCode::NoSuchTarget).await;
    // A msgid that never existed is indistinguishable.
    ada.send("REPORT test.example/01ARZ3NDEKTSV4RRFFQ69G5FAV spam");
    ada.expect_err(ErrCode::NoSuchTarget).await;
}

#[tokio::test]
async fn reports_queue_requires_reports_cap() {
    let ctx = ctx(&["#general"]); // no operators
    let mut ada = ready(&ctx, "ada").await;

    // No `reports` cap at `*` → CAP-REQUIRED naming the cap.
    ada.send("REPORTS LIST *");
    let err = ada.expect_err(ErrCode::CapRequired).await;
    let Event::Err(e) = &err.event else { panic!() };
    assert_eq!(e.context.as_deref(), Some("reports"));

    // Resolving an unknown report answers NO-SUCH-TARGET (the fetch fails
    // before the cap check — anti-enumeration).
    ada.send("REPORTS RESOLVE nope dismissed");
    ada.expect_err(ErrCode::NoSuchTarget).await;
}

// ---- §11 federation: bridge sessions (M5b) ----

/// A ctx trusting one peer network with a pinned key, auto-accepting its
/// proposals. Optional operators hold the `netblock` cap at `*`.
fn ctx_bridged(
    channels: &[&str],
    operators: &[&str],
    peer: &str,
    peer_key: &weft_core::PublicKey,
) -> Arc<ServerCtx> {
    let chans: Vec<(&str, &str)> = channels.iter().map(|c| (*c, "retained:90d")).collect();
    let info = ServerInfo {
        network: "test.example".parse().unwrap(),
        motd: None,
        features: Vec::new(),
    };
    let mut peer_keys = std::collections::HashMap::new();
    peer_keys.insert(peer.parse().unwrap(), *peer_key);
    Arc::new(ServerCtx::new(
        info,
        chans
            .iter()
            .map(|(c, p)| (c.parse().unwrap(), p.parse::<RetentionPolicy>().unwrap())),
        Keypair::generate(),
        true,
        Arc::new(MemoryStore::default()),
        Arc::new(weft_core::MemBlobStore::default()),
        "permanent".parse().unwrap(),
        operators.iter().map(|o| o.parse().unwrap()),
        true,
        10,
        weft_core::FederationConfig {
            peer_keys,
            accept_any: false,
            auto_accept: true,
        },
    ))
}

/// An open-federation ctx: no pinned peers, accepts a bridge from any network
/// (trust-on-first-use). Optional operators hold the `netblock` cap.
fn ctx_open_federation(channels: &[&str], operators: &[&str]) -> Arc<ServerCtx> {
    let chans: Vec<(&str, &str)> = channels.iter().map(|c| (*c, "retained:90d")).collect();
    let info = ServerInfo {
        network: "test.example".parse().unwrap(),
        motd: None,
        features: Vec::new(),
    };
    Arc::new(ServerCtx::new(
        info,
        chans
            .iter()
            .map(|(c, p)| (c.parse().unwrap(), p.parse::<RetentionPolicy>().unwrap())),
        Keypair::generate(),
        true,
        Arc::new(MemoryStore::default()),
        Arc::new(weft_core::MemBlobStore::default()),
        "permanent".parse().unwrap(),
        operators.iter().map(|o| o.parse().unwrap()),
        true,
        10,
        weft_core::FederationConfig {
            peer_keys: std::collections::HashMap::new(),
            accept_any: true,
            auto_accept: true,
        },
    ))
}

/// Drive a session to `State::Bridge` as `peer`, proving control of `key`.
async fn bridged_peer(ctx: &Arc<ServerCtx>, peer: &str, key: &Keypair) -> Client {
    let mut c = connect(ctx);
    c.send("HELLO weft/1");
    assert!(matches!(c.recv().await.event, Event::Welcome { .. }));
    c.send(&format!("AUTH BRIDGE {peer} {}", key.public().to_b64()));
    let Event::Challenge { nonce } = c.recv().await.event else {
        panic!("expected CHALLENGE");
    };
    let nonce = weft_crypto::b64::decode(&nonce).unwrap();
    let sig = weft_crypto::sign_challenge(key, &nonce, "test.example");
    c.send(&format!(
        "AUTH PROOF {}",
        weft_crypto::signature_to_b64(&sig)
    ));
    assert!(matches!(c.recv().await.event, Event::Welcome { .. }));
    c
}

/// A v1 manifest for `channels`, signed by the peer key, naming us as peer.
fn peer_manifest(key: &Keypair, channels: &[&str]) -> String {
    weft_core::Manifest {
        peer: "test.example".to_string(),
        version: 1,
        channels: channels.iter().map(|c| c.to_string()).collect(),
        history: "from-epoch".to_string(),
        media: "none".to_string(),
        typing: false,
        voice: false,
        created: 0,
        updated: 0,
    }
    .sign(key)
    .to_b64()
}

/// Propose + auto-ack `channels`; returns after reading the `BRIDGE ACCEPT`.
async fn propose(bridge: &mut Client, key: &Keypair, channels: &[&str]) {
    let chan = channels[0];
    bridge.send(&format!(
        "@manifest={} BRIDGE PROPOSE {chan} test.example",
        peer_manifest(key, channels)
    ));
    let ack = bridge.recv_raw().await;
    assert!(ack.contains("BRIDGE ACCEPT test.example 1"), "{ack}");
}

// §11.10 auto-federation: NS META federation flag + BRIDGE REQUEST offer.

#[tokio::test]
async fn ns_meta_federation_requires_public() {
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!(
        "@root={} NS CREATE gaming unlisted",
        root_key_b64()
    ));
    ada.recv().await;

    // Opening federation on a non-public namespace is refused (§11.10).
    ada.send("NS META gaming federation :open");
    ada.expect_err(ErrCode::Forbidden).await;

    // Public first, then it's allowed.
    ada.send("NS VISIBILITY gaming public");
    ada.recv().await;
    ada.send("NS META gaming federation :open");
    assert!(matches!(ada.recv().await.event, Event::NsMeta { .. }));
}

#[tokio::test]
async fn bridge_request_offers_only_reachable_namespaces() {
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&[], &[], "peer.example", &peer_key.public());

    // Owner makes a public namespace reachable.
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@root={} NS CREATE gaming public", root_key_b64()));
    ada.recv().await;
    ada.send("NS META gaming federation :open");
    ada.recv().await;

    let mut peer = bridged_peer(&ctx, "peer.example", &peer_key).await;

    // Reachable → the peer receives a signed BRIDGE PROPOSE offer.
    peer.send("BRIDGE REQUEST gaming");
    let offer = peer.recv_raw().await;
    assert!(
        offer.contains("BRIDGE PROPOSE"),
        "expected an offer, got {offer}"
    );
    assert!(
        offer.contains("manifest="),
        "offer must carry a manifest: {offer}"
    );

    // Closed / unknown → NO-SUCH-TARGET (uniform, anti-enumeration).
    peer.send("BRIDGE REQUEST nonexistent");
    let miss = peer.recv_raw().await;
    assert!(
        miss.contains("NO-SUCH-TARGET"),
        "expected NO-SUCH-TARGET, got {miss}"
    );
}

#[tokio::test]
async fn federate_hands_request_to_the_dialer() {
    let ctx = ctx(&[]);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.set_auto_bridge_sink(tx);
    let mut ada = ready(&ctx, "ada").await;

    // A valid foreign target is handed to the dialer (async — no client ack).
    ada.send("FEDERATE hda.example/gaming");
    let req = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("timed out waiting for the dialer request")
        .expect("sink closed");
    assert_eq!(req.network.as_str(), "hda.example");
    assert_eq!(req.namespace.to_string(), "gaming");

    // A second request immediately after is throttled (per-account cooldown).
    ada.send("FEDERATE hda.example/other");
    ada.expect_err(ErrCode::Throttled).await;

    // Federating your own network is a no-op (self-check precedes the cooldown).
    ada.send("FEDERATE test.example/gaming");
    ada.expect_err(ErrCode::Unsupported).await;
}

#[tokio::test]
async fn federate_unsupported_when_auto_bridge_off() {
    let ctx = ctx(&[]); // no sink installed → auto-federation is off
    let mut ada = ready(&ctx, "ada").await;
    ada.send("FEDERATE hda.example/gaming");
    ada.expect_err(ErrCode::Unsupported).await;
}

#[tokio::test]
async fn bridge_auth_rejects_unknown_or_mismatched_key() {
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&[], &[], "peer.example", &peer_key.public());
    // Unknown peer network → AUTH-FAILED (no existence oracle).
    let mut c = connect(&ctx);
    c.send("HELLO weft/1");
    c.recv().await;
    c.send(&format!(
        "AUTH BRIDGE stranger.example {}",
        peer_key.public().to_b64()
    ));
    c.expect_err(ErrCode::AuthFailed).await;
    // Known peer but a key that isn't the pinned one → AUTH-FAILED.
    c.send(&format!(
        "AUTH BRIDGE peer.example {}",
        Keypair::generate().public().to_b64()
    ));
    c.expect_err(ErrCode::AuthFailed).await;
}

#[tokio::test]
async fn bridge_ingests_remote_message_with_origin_msgid_intact() {
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "peer.example", &peer_key.public());
    let mut ada = joined(&ctx, "ada", "#general").await;
    let mut bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    propose(&mut bridge, &peer_key, &["#general"]).await;
    // The audience change reaches local members (§6.6 MANIFEST, mandatory).
    assert!(matches!(ada.recv().await.event, Event::Manifest { .. }));

    let mid = "peer.example/01ARZ3NDEKTSV4RRFFQ69G5FAV";
    bridge.send(&format!(
        "@msgid={mid} MESSAGE #general bob@peer.example :hi from afar"
    ));
    let Event::Message(m) = ada.recv().await.event else {
        panic!("expected ingested MESSAGE");
    };
    assert_eq!(m.msgid.to_string(), mid, "origin msgid preserved (§11.4)");
    assert_eq!(m.sender.to_string(), "bob@peer.example");
    assert_eq!(m.body, "hi from afar");
}

#[tokio::test]
async fn bridge_ingest_mirrors_foreign_attachments() {
    // §11.8: a bridged message with a foreign `weft-media://` attachment records
    // the reference locally and hands weftd a mirror pull.
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "peer.example", &peer_key.public());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.set_mirror_sink(tx);
    let mut ada = joined(&ctx, "ada", "#general").await;
    let mut bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    propose(&mut bridge, &peer_key, &["#general"]).await;
    assert!(matches!(ada.recv().await.event, Event::Manifest { .. }));

    let mid = "peer.example/01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let hash = "aa".repeat(32); // 64-hex content hash
    bridge.send(&format!(
        "@msgid={mid};attach.1=weft-media://peer.example/{hash} MESSAGE #general bob@peer.example :"
    ));
    assert!(matches!(ada.recv().await.event, Event::Message(_)));

    // A mirror pull was handed to weftd for the foreign blob.
    let req = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("mirror request")
        .expect("sink open");
    assert_eq!(req.peer.as_str(), "peer.example");
    assert_eq!(req.hash, hash);

    // And the reference was recorded so a local member is gated + can fetch it.
    let scopes = ctx.media_refs.blob_scopes(&hash).await.unwrap();
    assert!(scopes
        .iter()
        .any(|s| matches!(s, weft_store::Scope::Channel(c) if c.as_str() == "#general")));
}

#[tokio::test]
async fn federated_moderator_wields_caps_over_the_bridge() {
    // §11.10 homeserver authority: a federated user granted a cap on H wields it
    // through a bridge-tunnelled FSESSION — she never connects to H (IP
    // non-exposure); F vouches for her by having proven its network key.
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &["boss"], "peer.example", &peer_key.public());

    // H's operator grants `mute` at #general to the foreign user alice@peer.example.
    let mut boss = ready(&ctx, "boss").await;
    boss.send("GRANT alice@peer.example #general mute");
    assert!(matches!(boss.recv().await.event, Event::Token { .. }));

    // F authenticates the bridge, opens a session for alice, tunnels her MUTE.
    let mut bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    bridge.send("FSESSION 1 OPEN alice");
    bridge.send("FSESSION 1 CMD :@label=m MUTE #general bob :spam");

    // The reply tunnels back as `FSESSION 1 REPLY :<MODERATED …>`, attributed to
    // the federated moderator — enforcement hit H's grant store for account@net.
    let raw = bridge.recv_raw().await;
    assert!(raw.starts_with("FSESSION 1 REPLY :"), "{raw}");
    assert!(raw.contains("MODERATED #general bob mute"), "{raw}");
    assert!(raw.contains("by=alice@peer.example"), "{raw}");

    // A federated user WITHOUT the cap is refused — homeserver authority is not a
    // blanket; her power is exactly what H granted account@network.
    bridge.send("FSESSION 2 OPEN mallory");
    bridge.send("FSESSION 2 CMD :@label=x MUTE #general bob");
    let raw = bridge.recv_raw().await;
    assert!(raw.starts_with("FSESSION 2 REPLY :"), "{raw}");
    assert!(raw.contains("CAP-REQUIRED"), "{raw}");
}

#[tokio::test]
async fn federated_friend_request_over_the_tunnel() {
    // Cross-network friends: a user on network F friend-requests a user on
    // network H through the §11.10 tunnel. H records the cross-network edge in
    // its own store and pushes the incoming request to its local user; alice's
    // own resulting state tunnels back to F.
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "peer.example", &peer_key.public());

    // bob is a local (H = test.example) user, online to receive the push.
    let mut bob = ready(&ctx, "bob").await;

    // F authenticates the bridge and tunnels alice's FRIEND ADD bob@test.example.
    let mut bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    bridge.send("FSESSION 1 OPEN alice");
    bridge.send("FSESSION 1 CMD :@label=f FRIEND ADD bob@test.example");

    // alice's own state (outgoing) tunnels back to F as an FSESSION REPLY.
    let raw = bridge.recv_raw().await;
    assert!(raw.starts_with("FSESSION 1 REPLY :"), "{raw}");
    assert!(raw.contains("FRIEND bob@test.example outgoing"), "{raw}");

    // bob (local) is pushed the incoming request from the federated user — the
    // edge crossed the network boundary.
    match bob.recv().await.event {
        Event::Friend { user, state } => {
            assert_eq!(user.to_string(), "alice@peer.example");
            assert_eq!(state, FriendState::Incoming);
        }
        e => panic!("expected FRIEND incoming from federated user, got {e:?}"),
    }
}

#[tokio::test]
async fn federated_admin_delegates_a_cap_over_the_bridge() {
    // §11.10 full authority: a federated admin re-delegates a cap she holds
    // (`grant:mute`) to another user, over the tunnel — enforced against H's
    // grant store as her `account@network` identity.
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &["boss"], "peer.example", &peer_key.public());
    let mut boss = ready(&ctx, "boss").await;
    boss.send("GRANT alice@peer.example #general grant:mute");
    assert!(matches!(boss.recv().await.event, Event::Token { .. }));

    let mut bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    bridge.send("FSESSION 1 OPEN alice");
    bridge.send("FSESSION 1 CMD :@label=g GRANT bob@peer.example #general mute");
    let raw = bridge.recv_raw().await;
    assert!(raw.starts_with("FSESSION 1 REPLY :"), "{raw}");
    assert!(raw.contains("TOKEN"), "{raw}");
    assert!(raw.contains("bob@peer.example"), "{raw}");
}

#[tokio::test]
async fn federated_admin_creates_a_channel_over_the_bridge() {
    // §11.10 full authority: channel administration is a control action, so it
    // tunnels via the session (posting/content would ride the mirror instead).
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &["boss"], "peer.example", &peer_key.public());
    let mut boss = ready(&ctx, "boss").await;
    boss.send("GRANT alice@peer.example * chan-create");
    assert!(matches!(boss.recv().await.event, Event::Token { .. }));

    let mut bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    bridge.send("FSESSION 1 OPEN alice");
    bridge.send("FSESSION 1 CMD :@label=c CHANNEL CREATE #lounge");
    let raw = bridge.recv_raw().await;
    assert!(raw.starts_with("FSESSION 1 REPLY :"), "{raw}");
    assert!(raw.contains("POLICY #lounge"), "{raw}");
}

#[tokio::test]
async fn federated_admin_edits_namespace_meta_over_the_bridge() {
    // §11.10 full authority incl. namespace administration (the ns-admin gate is
    // actor-aware). A federated `ns-admin` holder edits H's namespace config.
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&[], &["boss"], "peer.example", &peer_key.public());
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@root={} NS CREATE gaming public", root_key_b64()));
    ada.recv().await;
    ada.send("GRANT alice@peer.example ns:gaming ns-admin");
    assert!(matches!(ada.recv().await.event, Event::Token { .. }));

    let mut bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    bridge.send("FSESSION 1 OPEN alice");
    bridge.send("FSESSION 1 CMD :@label=n NS META gaming title :Alice's Lounge");
    let raw = bridge.recv_raw().await;
    assert!(raw.starts_with("FSESSION 1 REPLY :"), "{raw}");
    assert!(raw.contains("NS-META") && raw.contains("gaming"), "{raw}");
}

#[tokio::test]
async fn bridge_forwards_local_messages_to_peer() {
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "peer.example", &peer_key.public());
    let mut bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    propose(&mut bridge, &peer_key, &["#general"]).await;
    let ada = joined(&ctx, "ada", "#general").await;
    ada.send("MSG #general :hello peers");
    // The local-origin message is forwarded verbatim over the bridge.
    loop {
        let line = bridge.recv_raw().await;
        if line.contains("MESSAGE #general ada@test.example") {
            assert!(line.contains("hello peers"), "{line}");
            break;
        }
    }
}

#[tokio::test]
async fn home_authoritative_channel_mints_relayed_spoke_post_and_mirrors_it() {
    // §11.13: a spoke relays a member's channel post to the home (`@id` absent);
    // the home is the sole ULID writer — it mints a home-origin msgid, delivers to
    // its local members, and the ordinary event mirror fans the minted message back
    // out to the peer, carrying the `nonce` so the spoke reconciles the optimistic copy.
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "peer.example", &peer_key.public());
    let mut ada = joined(&ctx, "ada", "#general").await;
    let mut bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    propose(&mut bridge, &peer_key, &["#general"]).await;
    assert!(matches!(ada.recv().await.event, Event::Manifest { .. }));

    // A spoke relays alice's post to us (the home): @id absent = mint request.
    let inner = weft_proto::Request::new(weft_proto::Command::ChannelRelay {
        channel: "#general".parse().unwrap(),
        sender: "alice@peer.example".parse().unwrap(),
        msgid: None,
        body: "hi from alice".to_string(),
        meta: weft_proto::MsgMeta {
            nonce: Some("n-alice-1".to_string()),
            ..Default::default()
        },
        echo: None,
    })
    .serialize()
    .unwrap();
    bridge.send("FSESSION 1 OPEN alice");
    bridge.send(&format!("FSESSION 1 CMD :{inner}"));

    // ada (a local home member) sees alice's message, minted by the home.
    let Event::Message(m) = ada.recv().await.event else {
        panic!("expected alice's minted message");
    };
    assert_eq!(m.sender.to_string(), "alice@peer.example");
    assert_eq!(m.body, "hi from alice");
    assert_eq!(m.msgid.origin().as_str(), "test.example"); // home is the origin
    assert_eq!(m.meta.nonce.as_deref(), Some("n-alice-1")); // nonce carried for reconcile

    // And the home-minted message is mirrored back out to the peer, nonce intact.
    loop {
        let line = bridge.recv_raw().await;
        if line.contains("MESSAGE #general alice@peer.example") {
            assert!(line.contains("hi from alice"), "{line}");
            assert!(line.contains("test.example/"), "{line}"); // home-minted origin
            assert!(line.contains("nonce=n-alice-1"), "{line}"); // reconcile token rides the mirror
            break;
        }
    }
}

#[tokio::test]
async fn spoke_relays_channel_post_to_the_home_instead_of_minting() {
    // §11.13: on a network that is NOT the channel's home, a member's post is not
    // minted locally — it is relayed to the home (`CHANNEL RELAY`, `@id` absent) to
    // be minted into the one total order.
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "home.example", &peer_key.public());
    // We are a spoke: #general's home is home.example (as an acked manifest would set).
    ctx.registry
        .set_home("#general".parse().unwrap(), "home.example".parse().unwrap());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.set_friend_deliver_sink(tx);
    let ada = joined(&ctx, "ada", "#general").await;

    ada.send("@l=m MSG #general :hello home");

    let relay = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("delivery")
        .expect("sink open");
    assert_eq!(relay.peer.as_str(), "home.example");
    assert!(
        relay
            .line
            .contains("CHANNEL RELAY #general ada@test.example"),
        "{}",
        relay.line
    );
    assert!(!relay.line.contains("id="), "{}", relay.line); // @id absent = mint request
    assert!(relay.line.contains("hello home"), "{}", relay.line);
}

#[tokio::test]
async fn home_applies_relayed_channel_edit_and_rejects_a_non_author() {
    // §11.13/§11.4: the home applies a spoke member's relayed mutation only after
    // verifying authorship — a different sender's forged edit is dropped.
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "peer.example", &peer_key.public());
    let mut ada = joined(&ctx, "ada", "#general").await;
    let mut bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    propose(&mut bridge, &peer_key, &["#general"]).await;
    assert!(matches!(ada.recv().await.event, Event::Manifest { .. }));

    // A spoke relays alice's post → the home mints it.
    let post = weft_proto::Request::new(weft_proto::Command::ChannelRelay {
        channel: "#general".parse().unwrap(),
        sender: "alice@peer.example".parse().unwrap(),
        msgid: None,
        body: "typo heer".to_string(),
        meta: weft_proto::MsgMeta::default(),
        echo: None,
    })
    .serialize()
    .unwrap();
    bridge.send("FSESSION 1 OPEN alice");
    bridge.send(&format!("FSESSION 1 CMD :{post}"));
    let Event::Message(m) = ada.recv().await.event else {
        panic!("expected alice's minted message");
    };
    let minted = m.msgid.clone();
    assert_eq!(minted.origin().as_str(), "test.example");

    // A NON-author (bob) tries to edit alice's message: the home drops it.
    let forged = weft_proto::Request::new(weft_proto::Command::ChannelMut {
        channel: "#general".parse().unwrap(),
        sender: "bob@peer.example".parse().unwrap(),
        root: minted.clone(),
        op: "edit".to_string(),
        arg: "hijacked".to_string(),
        msgid: None,
    })
    .serialize()
    .unwrap();
    bridge.send(&format!("FSESSION 1 CMD :{forged}"));

    // The author (alice) edits: the home applies it.
    let good = weft_proto::Request::new(weft_proto::Command::ChannelMut {
        channel: "#general".parse().unwrap(),
        sender: "alice@peer.example".parse().unwrap(),
        root: minted.clone(),
        op: "edit".to_string(),
        arg: "typo here".to_string(),
        msgid: None,
    })
    .serialize()
    .unwrap();
    bridge.send(&format!("FSESSION 1 CMD :{good}"));

    // The first EDITED ada sees is alice's — the forged edit never applied.
    let Event::Edited {
        body,
        edit_of,
        user,
        ..
    } = ada.recv().await.event
    else {
        panic!("expected EDITED");
    };
    assert_eq!(body, "typo here");
    assert_eq!(edit_of, minted);
    assert_eq!(user.to_string(), "alice@peer.example");
}

#[tokio::test]
async fn spoke_relays_a_channel_edit_to_the_home() {
    // §11.13: a member editing their own message on a spoke relays a `CHANNEL MUT`
    // (`@id` absent) to the home rather than mutating locally.
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "home.example", &peer_key.public());
    ctx.registry
        .set_home("#general".parse().unwrap(), "home.example".parse().unwrap());
    let mut ada = joined(&ctx, "ada", "#general").await;
    let mut bridge = bridged_peer(&ctx, "home.example", &peer_key).await;
    propose(&mut bridge, &peer_key, &["#general"]).await;
    assert!(matches!(ada.recv().await.event, Event::Manifest { .. }));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.set_friend_deliver_sink(tx);

    // ada's own message exists on the spoke as a home-minted (home-origin) replica.
    let mid = "home.example/01ARZ3NDEKTSV4RRFFQ69G5FAV";
    bridge.send(&format!(
        "@msgid={mid} MESSAGE #general ada@test.example :helo"
    ));
    let Event::Message(m) = ada.recv().await.event else {
        panic!("expected the ingested message");
    };
    assert_eq!(m.msgid.to_string(), mid);

    // ada edits it → we don't mutate locally; we relay to the home.
    ada.send(&format!("@l=e EDIT {mid} :hello"));
    let relay = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("delivery")
        .expect("sink open");
    assert_eq!(relay.peer.as_str(), "home.example");
    assert!(
        relay.line.contains("CHANNEL MUT #general ada@test.example"),
        "{}",
        relay.line
    );
    assert!(relay.line.contains("edit"), "{}", relay.line);
    assert!(relay.line.contains("hello"), "{}", relay.line);
    assert!(!relay.line.contains("id="), "{}", relay.line); // @id absent = apply request
}

#[tokio::test]
async fn spoke_requests_channel_backfill_from_the_home_on_history() {
    // §11.13: a spoke viewing a home-authoritative channel's history asks the home
    // to replay anything it minted while the spoke was unreachable.
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "home.example", &peer_key.public());
    ctx.registry
        .set_home("#general".parse().unwrap(), "home.example".parse().unwrap());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.set_friend_deliver_sink(tx);
    let ada = joined(&ctx, "ada", "#general").await;

    ada.send("HISTORY #general");

    // The catch-up request goes to the home, carrying our (empty) cursor.
    let req = loop {
        let d = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("delivery")
            .expect("sink open");
        if d.line.contains("CHANNEL BACKFILL") {
            break d;
        }
    };
    assert_eq!(req.peer.as_str(), "home.example");
    let weft_proto::Command::ChannelBackfill { channel, .. } =
        weft_proto::Request::parse(&req.line).unwrap().command
    else {
        panic!("expected CHANNEL BACKFILL, got {:?}", req.line);
    };
    assert_eq!(channel.to_string(), "#general");
}

#[tokio::test]
async fn home_serves_channel_backfill_replaying_missed_messages() {
    // §11.13: the home replays its channel's message roots after a spoke's cursor
    // as `CHANNEL RELAY` (`@id` present) ingests — the recovery path for a spoke
    // that was down when they were minted.
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "peer.example", &peer_key.public());
    let mut ada = joined(&ctx, "ada", "#general").await;
    let mut bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    propose(&mut bridge, &peer_key, &["#general"]).await;
    assert!(matches!(ada.recv().await.event, Event::Manifest { .. }));

    // Two messages the home mints (draining ada's echoes ensures they persist).
    ada.send("MSG #general :first");
    assert!(matches!(ada.recv().await.event, Event::Message(_)));
    ada.send("MSG #general :second");
    assert!(matches!(ada.recv().await.event, Event::Message(_)));

    // The spoke asks us (the home) to replay from the start.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.set_friend_deliver_sink(tx);
    let bf = weft_proto::Request::new(weft_proto::Command::ChannelBackfill {
        channel: "#general".parse().unwrap(),
        after: None,
    })
    .serialize()
    .unwrap();
    bridge.send("FSESSION 1 OPEN alice");
    bridge.send(&format!("FSESSION 1 CMD :{bf}"));

    // We replay both messages as home-minted CHANNEL RELAY ingests to the peer.
    let mut bodies = Vec::new();
    while bodies.len() < 2 {
        let d = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("delivery")
            .expect("sink open");
        if let weft_proto::Command::ChannelRelay { msgid, body, .. } =
            weft_proto::Request::parse(&d.line).unwrap().command
        {
            assert_eq!(d.peer.as_str(), "peer.example");
            assert!(msgid.is_some(), "replay carries the home-minted id"); // @id present
            bodies.push(body);
        }
    }
    assert!(bodies.contains(&"first".to_string()), "{bodies:?}");
    assert!(bodies.contains(&"second".to_string()), "{bodies:?}");
}

#[tokio::test]
async fn spoke_provisions_a_replica_for_a_manifested_foreign_channel() {
    // §11.13: the spoke does not seed #gaming/room. When the home's manifest offers
    // it, the spoke provisions a replica homed at the peer — so `is_home` reports
    // the peer and mirrored events land (previously `ingest_bridged` no-op'd).
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "home.example", &peer_key.public());
    let room: weft_proto::ChannelName = "#gaming/room".parse().unwrap();
    assert!(!ctx.registry.exists(&room), "not seeded");

    let mut bridge = bridged_peer(&ctx, "home.example", &peer_key).await;
    propose(&mut bridge, &peer_key, &["#gaming/room"]).await;

    // The replica now exists, homed at the peer (we are a spoke for it).
    assert!(ctx.registry.exists(&room));
    assert_eq!(ctx.registry.home(&room).as_str(), "home.example");
    assert!(!ctx.registry.is_home(&room));

    // A local member can join the provisioned replica, and a home-minted message
    // now lands (it would have been dropped before provisioning).
    let mut ada = joined(&ctx, "ada", "#gaming/room").await;
    let mid = "home.example/01ARZ3NDEKTSV4RRFFQ69G5FAV";
    bridge.send(&format!(
        "@msgid={mid} MESSAGE #gaming/room bob@home.example :hi"
    ));
    let Event::Message(m) = ada.recv().await.event else {
        panic!("expected the ingested message on the replica");
    };
    assert_eq!(m.msgid.to_string(), mid);
    assert_eq!(m.body, "hi");
}

#[tokio::test]
async fn bridge_drops_foreign_origin_events() {
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "peer.example", &peer_key.public());
    let mut ada = joined(&ctx, "ada", "#general").await;
    let mut bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    propose(&mut bridge, &peer_key, &["#general"]).await;
    assert!(matches!(ada.recv().await.event, Event::Manifest { .. }));
    // An event whose origin isn't the authenticated peer is dropped (inv. 2).
    bridge.send(
        "@msgid=other.example/01ARZ3NDEKTSV4RRFFQ69G5FAV MESSAGE #general eve@other.example :spoofed",
    );
    // A legitimate peer message follows; it's the first thing ada sees.
    let mid = "peer.example/01ARZ3NDEKTSV4RRFFQ69G5FB0";
    bridge.send(&format!(
        "@msgid={mid} MESSAGE #general bob@peer.example :real"
    ));
    let Event::Message(m) = ada.recv().await.event else {
        panic!("expected MESSAGE");
    };
    assert_eq!(m.msgid.to_string(), mid, "the spoofed event never arrived");
    assert_eq!(m.body, "real");
}

#[tokio::test]
async fn bridge_gates_ingest_on_acked_manifest() {
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(
        &["#general", "#secret"],
        &[],
        "peer.example",
        &peer_key.public(),
    );
    let mut ada = joined(&ctx, "ada", "#secret").await;
    let mut bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    // Only #general is bridged; #secret is not in the manifest.
    propose(&mut bridge, &peer_key, &["#general"]).await;
    // A remote message aimed at the un-bridged channel must be dropped (inv. 3).
    bridge.send(
        "@msgid=peer.example/01ARZ3NDEKTSV4RRFFQ69G5FAV MESSAGE #secret bob@peer.example :leak",
    );
    // ada's own echo is the next thing she sees — the leak never landed.
    ada.send("MSG #secret :ping");
    let Event::Message(m) = ada.recv().await.event else {
        panic!("expected own echo");
    };
    assert_eq!(m.body, "ping", "un-bridged ingest must not reach members");
}

#[tokio::test]
async fn netblock_add_list_remove_gated_on_cap() {
    let ctx = ctx_ops(&[], &["op"]);
    let mut op = ready(&ctx, "op").await;
    op.send("@label=n1 NETBLOCK ADD evil.example :spam floods");
    let reply = op.recv().await;
    assert_eq!(reply.label.as_deref(), Some("n1"));
    assert!(
        matches!(&reply.event, Event::Netblocked { network, .. } if network.as_str() == "evil.example")
    );

    op.send("NETBLOCK LIST");
    let listed = op.recv().await;
    assert!(
        matches!(&listed.event, Event::Netblocked { network, reason } if network.as_str() == "evil.example" && reason.as_deref() == Some("spam floods"))
    );

    // A non-operator lacks the `netblock` cap (§10.4, `*`-only).
    let mut mallory = ready(&ctx, "mallory").await;
    mallory.send("NETBLOCK ADD good.example");
    let err = mallory.expect_err(ErrCode::CapRequired).await;
    let Event::Err(e) = err.event else { panic!() };
    assert_eq!(e.context.as_deref(), Some("netblock")); // §8 names the cap

    op.send("NETBLOCK REMOVE evil.example");
    assert!(matches!(op.recv().await.event, Event::Netblocked { .. }));
    op.send("NETBLOCK REMOVE evil.example");
    op.expect_err(ErrCode::NoSuchTarget).await;
}

/// §13 M-media-5: MEDIA BLOCK is `media-block`-cap-gated (`*`), flips
/// `is_blob_blocked`, lists, and UNBLOCK reverses it.
#[tokio::test]
async fn media_block_gates_cap_and_flips_the_blocklist() {
    let ctx = ctx_ops(&[], &["op"]);

    // A non-operator lacks the `media-block` cap.
    let mut mallory = ready(&ctx, "mallory").await;
    mallory.send("MEDIA BLOCK deadbeef");
    let err = mallory.expect_err(ErrCode::CapRequired).await;
    let Event::Err(e) = err.event else { panic!() };
    assert_eq!(e.context.as_deref(), Some("media-block"));

    // The operator blocks a hash → the gate flips + a MEDIA-BLOCKED ack.
    let mut op = ready(&ctx, "op").await;
    assert!(!ctx.is_blob_blocked("deadbeef").await);
    op.send("@label=b1 MEDIA BLOCK deadbeef :csam");
    let ack = op.recv().await;
    assert_eq!(ack.label.as_deref(), Some("b1"));
    assert!(matches!(&ack.event, Event::MediaBlocked { hash, reason }
            if hash == "deadbeef" && reason.as_deref() == Some("csam")));
    assert!(ctx.is_blob_blocked("deadbeef").await);

    // MEDIA BLOCKS lists the entry.
    op.send("MEDIA BLOCKS");
    assert!(
        matches!(&op.recv().await.event, Event::MediaBlocked { hash, .. } if hash == "deadbeef")
    );

    // UNBLOCK reverses; a second UNBLOCK is NO-SUCH-TARGET.
    op.send("MEDIA UNBLOCK deadbeef");
    assert!(matches!(op.recv().await.event, Event::MediaBlocked { .. }));
    assert!(!ctx.is_blob_blocked("deadbeef").await);
    op.send("MEDIA UNBLOCK deadbeef");
    op.expect_err(ErrCode::NoSuchTarget).await;
}

// ---- §11 federation: backfill, report-forward, netblock effects (M5c) ----

#[tokio::test]
async fn bridge_backfill_serves_acked_channel_history() {
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "peer.example", &peer_key.public());
    // Local history, drained so it's persisted before the backfill.
    let mut ada = joined(&ctx, "ada", "#general").await;
    ada.send("MSG #general :first");
    assert!(matches!(ada.recv().await.event, Event::Message(_)));
    ada.send("MSG #general :second");
    assert!(matches!(ada.recv().await.event, Event::Message(_)));

    let mut bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    propose(&mut bridge, &peer_key, &["#general"]).await;
    bridge.send("HISTORY #general limit=10");
    assert!(matches!(
        bridge.recv().await.event,
        Event::BatchStart { .. }
    ));
    let Event::Message(m1) = bridge.recv().await.event else {
        panic!("expected first backfilled MESSAGE");
    };
    assert_eq!(m1.body, "first");
    let Event::Message(m2) = bridge.recv().await.event else {
        panic!("expected second backfilled MESSAGE");
    };
    assert_eq!(m2.body, "second");
    let Event::BatchEnd { compacted, .. } = bridge.recv().await.event else {
        panic!("expected BATCH END");
    };
    assert!(
        compacted,
        "backfill serves the compacted materialization (§11.7)"
    );
}

/// §6/§13 a HISTORY page over the stream threshold is offered as a `STREAM
/// ACCEPT <token>` instead of an inline BATCH; the token resolves to the whole
/// serialized batch, which parses back to `BatchStart … messages … BatchEnd`.
#[tokio::test]
async fn large_history_upgrades_to_a_backfill_stream() {
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    // Post one past the threshold so the page must stream.
    let n = weft_proto::HISTORY_STREAM_THRESHOLD + 1;
    for i in 0..n {
        ada.send(&format!("MSG #general :m{i}"));
        assert!(matches!(ada.recv().await.event, Event::Message(_)));
    }

    ada.send("HISTORY #general limit=500");
    let Event::StreamAccept { token } = ada.recv().await.event else {
        panic!("a large page must upgrade to a STREAM ACCEPT");
    };

    // The token yields the serialized batch, one Reply per line.
    let body = ctx
        .take_backfill_token(&token)
        .expect("backfill token resolves to a body");
    let body = String::from_utf8(body).expect("utf-8 batch");
    let events: Vec<Event> = body
        .lines()
        .map(|l| Reply::parse(l).expect("parseable batch line").event)
        .collect();
    assert!(matches!(events.first(), Some(Event::BatchStart { .. })));
    assert!(matches!(events.last(), Some(Event::BatchEnd { .. })));
    let bodies: std::collections::HashSet<&str> = events
        .iter()
        .filter_map(|e| match e {
            Event::Message(m) => Some(m.body.as_str()),
            _ => None,
        })
        .collect();
    for i in 0..n {
        assert!(
            bodies.contains(format!("m{i}").as_str()),
            "m{i} missing from stream"
        );
    }

    // One-time: a second pull of the same token is uniformly "not found".
    assert!(ctx.take_backfill_token(&token).is_none());
}

#[tokio::test]
async fn bridge_backfill_refuses_unbridged_channel() {
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(
        &["#general", "#secret"],
        &[],
        "peer.example",
        &peer_key.public(),
    );
    let mut bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    propose(&mut bridge, &peer_key, &["#general"]).await; // only #general
    bridge.send("HISTORY #secret limit=10");
    // An un-bridged channel yields an empty batch — no history leak (inv. 3).
    assert!(matches!(
        bridge.recv().await.event,
        Event::BatchStart { .. }
    ));
    assert!(matches!(bridge.recv().await.event, Event::BatchEnd { .. }));
}

#[tokio::test]
async fn forwarded_report_files_unverified_stripping_reporter() {
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &["op"], "peer.example", &peer_key.public());
    // A local message that a remote user will report.
    let mut ada = joined(&ctx, "ada", "#general").await;
    ada.send("MSG #general :something reportable");
    let Event::Message(m) = ada.recv().await.event else {
        panic!("expected echo");
    };
    let mid = m.msgid.to_string();

    // An operator is connected to receive the live REPORT-FILED push.
    let mut op = ready(&ctx, "op").await;
    let mut bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    propose(&mut bridge, &peer_key, &["#general"]).await;
    bridge.send(&format!(
        "REPORT-FORWARD rep-remote-1 {mid} harassment :their user complained"
    ));
    let filed = op.recv().await;
    let Event::ReportFiled {
        state,
        reporter,
        scope,
        category,
        ..
    } = filed.event
    else {
        panic!("expected REPORT-FILED, got {filed:?}");
    };
    assert_eq!(state, weft_proto::ContentState::Unverified); // §11.9
    assert_eq!(reporter, None, "reporter identity stripped (invariant 12)");
    assert!(matches!(scope, weft_proto::ReportScope::Net));
    assert_eq!(category, "harassment");
}

#[tokio::test]
async fn netblock_stops_ingestion_from_blocked_peer() {
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &["op"], "peer.example", &peer_key.public());
    let mut ada = joined(&ctx, "ada", "#general").await;
    let mut bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    propose(&mut bridge, &peer_key, &["#general"]).await;
    assert!(matches!(ada.recv().await.event, Event::Manifest { .. }));
    // Before the block, ingestion works.
    bridge.send(
        "@msgid=peer.example/01ARZ3NDEKTSV4RRFFQ69G5FAV MESSAGE #general bob@peer.example :before",
    );
    assert!(matches!(ada.recv().await.event, Event::Message(_)));

    // Operator blocks the peer (invariant 7). The block is committed once the
    // NETBLOCKED ack returns.
    let mut op = ready(&ctx, "op").await;
    op.send("NETBLOCK ADD peer.example :abuse");
    assert!(matches!(op.recv().await.event, Event::Netblocked { .. }));

    // A subsequent event from the now-blocked peer is dropped at ingestion.
    bridge.send(
        "@msgid=peer.example/01ARZ3NDEKTSV4RRFFQ69G5FB0 MESSAGE #general bob@peer.example :after",
    );
    ada.send("MSG #general :ping");
    let Event::Message(m) = ada.recv().await.event else {
        panic!("expected own echo");
    };
    assert_eq!(m.body, "ping", "blocked peer's event must not arrive");
}

// ---- §11 open federation (accept-any) ----

#[tokio::test]
async fn open_federation_accepts_unpinned_peer_and_ingests() {
    let ctx = ctx_open_federation(&["#general"], &[]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    // A network with no pinned key brings its own and bridges (trust-on-first-use).
    let peer_key = Keypair::generate();
    let mut bridge = bridged_peer(&ctx, "newcomer.example", &peer_key).await;
    propose(&mut bridge, &peer_key, &["#general"]).await;
    assert!(matches!(ada.recv().await.event, Event::Manifest { .. }));
    bridge.send(
        "@msgid=newcomer.example/01ARZ3NDEKTSV4RRFFQ69G5FAV MESSAGE #general zoe@newcomer.example :hi",
    );
    let Event::Message(m) = ada.recv().await.event else {
        panic!("expected ingested MESSAGE");
    };
    assert_eq!(m.sender.to_string(), "zoe@newcomer.example");
    assert_eq!(m.body, "hi");
}

#[tokio::test]
async fn open_federation_still_honors_netblock() {
    let ctx = ctx_open_federation(&["#general"], &["op"]);
    let mut op = ready(&ctx, "op").await;
    op.send("NETBLOCK ADD evil.example :known bad");
    assert!(matches!(op.recv().await.event, Event::Netblocked { .. }));
    // Even accept-any refuses a blocked network's bridge (invariant 7).
    let evil_key = Keypair::generate();
    let mut c = connect(&ctx);
    c.send("HELLO weft/1");
    c.recv().await;
    c.send(&format!(
        "AUTH BRIDGE evil.example {}",
        evil_key.public().to_b64()
    ));
    c.expect_err(ErrCode::AuthFailed).await;
}

// ---- §6.7 moderation (M7) ----

#[tokio::test]
async fn mute_denies_send_and_unmute_restores() {
    let ctx = ctx_ops(&["#general"], &["mod"]);
    let mut bob = joined(&ctx, "bob", "#general").await;
    let mut op = ready(&ctx, "mod").await;

    op.send("@label=x MUTE #general bob :spamming");
    let reply = op.recv().await;
    assert!(
        matches!(&reply.event, Event::Moderated { action, .. } if *action == weft_proto::ModAction::Mute),
        "moderator gets a MODERATED echo, got {reply:?}"
    );

    bob.send("MSG #general :hello");
    let Event::Err(e) = bob.expect_err(ErrCode::Forbidden).await.event else {
        panic!()
    };
    assert_eq!(e.context.as_deref(), Some("muted"));

    op.send("UNMUTE #general bob");
    op.recv().await;
    bob.send("MSG #general :hi again");
    assert!(
        matches!(bob.recv().await.event, Event::Message(_)),
        "unmuted → can post"
    );
}

#[tokio::test]
async fn modlist_returns_the_deny_list() {
    let ctx = ctx_ops(&["#general"], &["mod"]);
    let mut op = ready(&ctx, "mod").await;
    op.send("MUTE #general bob :spam");
    op.recv().await;
    op.send("BAN #general eve :raid");
    op.recv().await;

    // The moderator lists the channel deny-list — a BATCH of MODERATED entries.
    op.send("@label=L MODLIST #general");
    assert!(
        matches!(op.recv().await.event, Event::BatchStart { .. }),
        "MODLIST opens a batch"
    );
    let mut got = Vec::new();
    loop {
        match op.recv().await.event {
            Event::Moderated {
                account, action, ..
            } => got.push((account.to_string(), action)),
            Event::BatchEnd { .. } => break,
            other => panic!("unexpected in modlist batch: {other:?}"),
        }
    }
    assert!(
        got.iter()
            .any(|(a, act)| a == "bob" && *act == weft_proto::ModAction::Mute),
        "mute present: {got:?}"
    );
    assert!(
        got.iter()
            .any(|(a, act)| a == "eve" && *act == weft_proto::ModAction::Ban),
        "ban present: {got:?}"
    );

    // A non-moderator cannot read the list.
    let mut ada = ready(&ctx, "ada").await;
    ada.send("MODLIST #general");
    assert!(
        matches!(&ada.recv().await.event, Event::Err(e) if e.code == ErrCode::CapRequired),
        "non-moderator MODLIST is cap-gated"
    );
}

#[tokio::test]
async fn ns_scope_mute_covers_a_namespaced_channel() {
    let ctx = ctx_ops(&["#gaming/general"], &["mod"]);
    let mut bob = joined(&ctx, "bob", "#gaming/general").await;
    let mut op = ready(&ctx, "mod").await;
    // A namespace-wide mute (a namespace moderator) covers the channel.
    op.send("MUTE ns:gaming bob");
    op.recv().await;
    bob.send("MSG #gaming/general :hi");
    let Event::Err(e) = bob.expect_err(ErrCode::Forbidden).await.event else {
        panic!()
    };
    assert_eq!(e.context.as_deref(), Some("muted"));
}

#[tokio::test]
async fn ban_ejects_and_blocks_rejoin() {
    let ctx = ctx_ops(&["#general"], &["mod"]);
    let mut bob = joined(&ctx, "bob", "#general").await;
    let mut op = ready(&ctx, "mod").await;

    op.send("BAN #general bob :raid");
    op.recv().await; // MODERATED
                     // bob is force-parted (kicked out).
    let ev = bob.recv().await;
    assert!(
        matches!(&ev.event, Event::Member { action: MemberAction::Part, user, .. } if user.account.as_str() == "bob"),
        "banned member is ejected, got {ev:?}"
    );
    // …and cannot rejoin.
    bob.send("JOIN #general");
    bob.expect_err(ErrCode::Banned).await;
    // Unban restores access.
    op.send("UNBAN #general bob");
    op.recv().await;
    bob.send("JOIN #general");
    assert!(matches!(bob.recv().await.event, Event::Member { .. }));
}

#[tokio::test]
async fn moderation_requires_the_cap() {
    let ctx = ctx_ops(&["#general"], &["mod"]);
    let mut mallory = joined(&ctx, "mallory", "#general").await;
    mallory.send("MUTE #general bob");
    let Event::Err(e) = mallory.expect_err(ErrCode::CapRequired).await.event else {
        panic!()
    };
    assert_eq!(e.context.as_deref(), Some("mute"));
}

#[tokio::test]
async fn restricted_channel_gates_posting_on_send_cap() {
    // A runtime-created channel lands in the channel store (where the
    // `restricted` flag lives); the real server seeds config channels there too.
    let ctx = ctx_ops(&[], &["mod"]);
    let mut op = ready(&ctx, "mod").await;
    op.send("CHANNEL CREATE #locked");
    op.recv().await; // POLICY (create ack)
    op.send("JOIN #locked");
    op.recv().await; // MEMBER
    op.recv().await; // POLICY
    op.send("CHANNEL META #locked posting :restricted");
    op.recv().await; // CHANMETA

    // A normal member (no send grant) can't post in a restricted channel.
    let mut bob = joined(&ctx, "bob", "#locked").await;
    op.recv().await; // bob's MEMBER join broadcast
    bob.send("MSG #locked :hello");
    let Event::Err(e) = bob.expect_err(ErrCode::CapRequired).await.event else {
        panic!()
    };
    assert_eq!(e.context.as_deref(), Some("send"));

    // The grant path (the "both" story): granting `send` lets them post — and
    // REVOKE would take it away again.
    op.send("GRANT bob #locked send");
    op.recv().await; // TOKEN
    bob.send("MSG #locked :now i can");
    loop {
        if matches!(bob.recv().await.event, Event::Message(ref m) if m.body == "now i can") {
            break;
        }
    }
}

#[tokio::test]
async fn a_frozen_channel_takes_nobody_but_a_moderator() {
    // WC7 room action. A freeze is a blanket lock, unlike `restricted` (which
    // delegates posting to the `send` cap) — so holding `send` is *not* enough
    // to talk through it, but an ns-admin can still post the reason.
    let (ctx, store) = ctx_full_store(&[], true, &["mod"]);
    let mut op = ready(&ctx, "mod").await;
    op.send("CHANNEL CREATE #cooldown");
    op.recv().await; // POLICY
    op.send("JOIN #cooldown");
    op.recv().await; // MEMBER
    op.recv().await; // POLICY

    let mut bob = joined(&ctx, "bob", "#cooldown").await;
    op.recv().await; // bob's join broadcast
                     // Give bob `send`, so the freeze — not a missing cap — is what stops him.
    op.send("GRANT bob #cooldown send");
    op.recv().await; // TOKEN

    let channel: weft_proto::ChannelName = "#cooldown".parse().unwrap();
    store.set_channel_frozen(&channel, true).await.unwrap();

    bob.send("MSG #cooldown :can i talk");
    let Event::Err(e) = bob.expect_err(ErrCode::Forbidden).await.event else {
        panic!()
    };
    assert_eq!(e.context.as_deref(), Some("frozen"));

    // The moderator (operator ⇒ holds ns-admin everywhere) still can.
    op.send("MSG #cooldown :locked while we sort this out");
    loop {
        if matches!(op.recv().await.event, Event::Message(ref m) if m.body.starts_with("locked")) {
            break;
        }
    }

    // Unfreezing restores bob's access — the freeze is reversible and left his
    // grant untouched.
    store.set_channel_frozen(&channel, false).await.unwrap();
    bob.send("MSG #cooldown :thanks");
    loop {
        if matches!(bob.recv().await.event, Event::Message(ref m) if m.body == "thanks") {
            break;
        }
    }
}

#[tokio::test]
async fn a_full_namespace_freeze_admits_only_the_owner() {
    // WC7 **full freeze** — the rung above a channel freeze. It locks every
    // channel in a namespace and, unlike the channel freeze, a delegated
    // `ns-admin` cannot talk through it either: only the namespace *owner* and
    // network operators can. That distinction is the whole point, so it's what
    // this asserts.
    let (ctx, store) = ctx_full_store(&[], true, &[]);
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@root={} NS CREATE gaming public", root_key_b64()));
    assert!(matches!(ada.recv().await.event, Event::NsMeta { .. }));
    ada.send("CHANNEL CREATE #gaming/lobby");
    ada.recv().await; // POLICY
    ada.send("JOIN #gaming/lobby");
    ada.recv().await; // MEMBER
    ada.recv().await; // POLICY

    // bob is a delegated ns-admin — full moderation authority in the namespace.
    let mut bob = joined(&ctx, "bob", "#gaming/lobby").await;
    ada.recv().await; // bob's join broadcast
    ada.send("GRANT bob ns:gaming ns-admin");
    ada.recv().await; // TOKEN

    let ns: weft_proto::NamespaceName = "gaming".parse().unwrap();
    store.set_namespace_frozen(&ns, true).await.unwrap();

    // Even an ns-admin is silenced by a full freeze.
    bob.send("MSG #gaming/lobby :i'm an admin though");
    let Event::Err(e) = bob.expect_err(ErrCode::Forbidden).await.event else {
        panic!()
    };
    assert_eq!(e.context.as_deref(), Some("frozen"));

    // The owner still speaks.
    ada.send("MSG #gaming/lobby :everything is paused");
    loop {
        if matches!(ada.recv().await.event, Event::Message(ref m) if m.body.starts_with("everything"))
        {
            break;
        }
    }

    // Lifting it restores the namespace.
    store.set_namespace_frozen(&ns, false).await.unwrap();
    bob.send("MSG #gaming/lobby :back");
    loop {
        if matches!(bob.recv().await.event, Event::Message(ref m) if m.body == "back") {
            break;
        }
    }
}

// ---- §6.2 NS JOIN (auto-join a namespace's visible channels) ----

#[tokio::test]
async fn ns_join_auto_joins_visible_channels_only() {
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@root={} NS CREATE gaming public", root_key_b64()));
    assert!(matches!(ada.recv().await.event, Event::NsMeta { .. }));
    // Owner creates three channels; one is view-gated (hidden by permissions).
    for c in ["#gaming/general", "#gaming/lounge", "#gaming/secret"] {
        ada.send(&format!("CHANNEL CREATE {c}"));
        assert!(matches!(ada.recv().await.event, Event::Policy { .. }));
    }
    ada.send("CHANNEL META #gaming/secret view-gated :yes");
    assert!(matches!(ada.recv().await.event, Event::Chanmeta { .. }));

    // A regular user joins the namespace → auto-joins the two visible channels.
    let mut bob = ready(&ctx, "bob").await;
    bob.send("NS JOIN gaming");
    let mut joined = std::collections::HashSet::new();
    for _ in 0..4 {
        // Two channels × (MEMBER + POLICY).
        match bob.recv().await.event {
            Event::Member { channel, .. } => {
                joined.insert(channel.to_string());
            }
            Event::Policy { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(joined.contains("#gaming/general"));
    assert!(joined.contains("#gaming/lounge"));
    assert!(
        !joined.contains("#gaming/secret"),
        "a view-gated channel must not be auto-joined"
    );
}

#[tokio::test]
async fn ns_join_with_no_visible_channels_is_no_such_target() {
    let ctx = ctx(&[]);
    let mut bob = ready(&ctx, "bob").await;
    bob.send("@label=j NS JOIN nope");
    let reply = bob.expect_err(ErrCode::NoSuchTarget).await;
    assert_eq!(reply.label.as_deref(), Some("j"));
}

// ---- §6.3 MEMBERS roster snapshot ----

#[tokio::test]
async fn members_returns_the_full_roster() {
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    let _bob = joined(&ctx, "bob", "#general").await;
    ada.recv().await; // bob's MEMBER join broadcast

    ada.send("@label=m MEMBERS #general");
    let start = ada.recv().await;
    assert!(
        matches!(start.event, Event::BatchStart { .. }),
        "got {start:?}"
    );
    assert_eq!(start.label.as_deref(), Some("m"), "batch echoes the label");

    let mut names = std::collections::HashSet::new();
    loop {
        let ev = ada.recv().await;
        match ev.event {
            Event::Member {
                user,
                action: MemberAction::Join,
                count: Some(2),
                ..
            } => {
                names.insert(user.account.as_str().to_string());
            }
            // Each member's dot rides along as a Presence event (§6.1).
            Event::Presence { .. } => {}
            Event::BatchEnd { .. } => break,
            other => panic!("unexpected in roster batch: {other:?}"),
        }
    }
    assert_eq!(
        names,
        ["ada", "bob"].into_iter().map(String::from).collect()
    );
}

#[tokio::test]
async fn members_shows_disconnected_members_offline() {
    // Discord-style: a disconnected member stays in the roster, dot offline.
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    let bob = joined(&ctx, "bob", "#general").await;
    ada.recv().await; // bob's MEMBER join

    drop(bob); // abrupt disconnect
    assert!(
        matches!(
            &ada.recv().await.event,
            Event::Presence { user, status: weft_proto::PresenceStatus::Offline }
                if user.account.as_str() == "bob"
        ),
        "co-member sees bob go offline live"
    );

    ada.send("MEMBERS #general");
    assert!(matches!(ada.recv().await.event, Event::BatchStart { .. }));
    let mut bob_status = None;
    let mut in_roster = false;
    loop {
        match ada.recv().await.event {
            Event::Member { user, .. } if user.account.as_str() == "bob" => in_roster = true,
            Event::Presence { user, status } if user.account.as_str() == "bob" => {
                bob_status = Some(status)
            }
            Event::BatchEnd { .. } => break,
            _ => {}
        }
    }
    assert!(in_roster, "bob remains a roster member after disconnect");
    assert_eq!(
        bob_status,
        Some(weft_proto::PresenceStatus::Offline),
        "bob's dot is offline"
    );
}

#[tokio::test]
async fn members_requires_membership() {
    let ctx = ctx(&["#general"]);
    let mut eve = ready(&ctx, "eve").await; // never joined
    eve.send("@label=m MEMBERS #general");
    // Same as MARK on a channel you're not in: join first (CAP-REQUIRED view).
    let Event::Err(e) = eve.expect_err(ErrCode::CapRequired).await.event else {
        panic!()
    };
    assert_eq!(e.context.as_deref(), Some("view"));
}

// ---- §6.4 PIN / UNPIN / PINS ----

#[tokio::test]
async fn pin_list_and_unpin() {
    let ctx = ctx_ops(&["#general"], &["mod"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    ada.send("MSG #general :pin me");
    let Event::Message(m) = ada.recv().await.event else {
        panic!("expected own echo")
    };
    let msgid = m.msgid.to_string();

    let mut op = joined(&ctx, "mod", "#general").await;
    ada.recv().await; // MEMBER: op joined

    // Operator pins the message.
    op.send(&format!("@label=p PIN {msgid}"));
    let ev = op.recv().await;
    assert!(
        matches!(&ev.event, Event::Pinned { by: Some(a), .. } if a.as_str() == "mod"),
        "got {ev:?}"
    );
    assert!(
        matches!(ada.recv().await.event, Event::Pinned { .. }),
        "ada sees the pin"
    );

    // PINS returns the pinned message as a batch.
    op.send("PINS #general");
    assert!(matches!(op.recv().await.event, Event::BatchStart { .. }));
    let msg = op.recv().await;
    assert!(
        matches!(&msg.event, Event::Message(m) if m.body == "pin me"),
        "got {msg:?}"
    );
    assert!(matches!(op.recv().await.event, Event::BatchEnd { .. }));

    // Unpin removes it.
    op.send(&format!("UNPIN {msgid}"));
    assert!(matches!(op.recv().await.event, Event::Unpinned { .. }));
    ada.recv().await; // ada sees the unpin
    op.send("PINS #general");
    assert!(matches!(op.recv().await.event, Event::BatchStart { .. }));
    assert!(
        matches!(op.recv().await.event, Event::BatchEnd { .. }),
        "no pins left"
    );
}

#[tokio::test]
async fn deleting_a_pinned_message_drops_its_pin() {
    // §6.4 a pin must never outlive its message — otherwise the pins view keeps
    // an entry that resolves to a tombstone. The channel actor is the single
    // writer for the delete, so it clears the pin and announces the UNPINNED.
    let ctx = ctx_ops(&["#general"], &["mod"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    ada.send("MSG #general :pin me");
    let Event::Message(m) = ada.recv().await.event else {
        panic!("expected own echo")
    };
    let msgid = m.msgid.to_string();

    let mut op = joined(&ctx, "mod", "#general").await;
    ada.recv().await; // MEMBER: op joined
    op.send(&format!("PIN {msgid}"));
    assert!(matches!(op.recv().await.event, Event::Pinned { .. }));
    assert!(matches!(ada.recv().await.event, Event::Pinned { .. }));

    // The author deletes it → everyone sees the pin lifted as well.
    ada.send(&format!("DELETE {msgid}"));
    let mut saw_unpinned = false;
    let mut saw_deleted = false;
    for _ in 0..2 {
        match ada.recv().await.event {
            Event::Unpinned { .. } => saw_unpinned = true,
            Event::Deleted { .. } => saw_deleted = true,
            _ => {}
        }
        if saw_unpinned && saw_deleted {
            break;
        }
    }
    assert!(saw_deleted, "the delete still broadcasts");
    assert!(saw_unpinned, "the pin is lifted with the message");

    // ...and the pins list is genuinely empty, not just visually cleared.
    op.send("PINS #general");
    loop {
        match op.recv().await.event {
            Event::BatchStart { .. } => continue,
            Event::BatchEnd { .. } => break,
            Event::Message(m) => panic!("a deleted message is still pinned: {m:?}"),
            _ => continue,
        }
    }
}

#[tokio::test]
async fn pin_requires_the_cap() {
    let ctx = ctx_ops(&["#general"], &["mod"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    ada.send("MSG #general :hi");
    let Event::Message(m) = ada.recv().await.event else {
        panic!()
    };
    let msgid = m.msgid.to_string();
    // A regular member has no `pin` cap — even for her own message.
    ada.send(&format!("@label=p PIN {msgid}"));
    let Event::Err(e) = ada.expect_err(ErrCode::CapRequired).await.event else {
        panic!()
    };
    assert_eq!(e.context.as_deref(), Some("pin"));
}

// ---- §10.4 CAPS query ----

#[tokio::test]
async fn caps_query_reports_effective_caps() {
    let ctx = ctx_ops(&["#general"], &["mod"]);
    let mut ada = joined(&ctx, "ada", "#general").await;

    // An operator holds every capability.
    ada.send("CAPS mod *");
    let Event::Caps { account, caps, .. } = ada.recv().await.event else {
        panic!()
    };
    assert_eq!(account.as_str(), "mod");
    assert!(
        caps.contains("mute") && caps.contains("ban") && caps.contains("ns-admin"),
        "operator holds all: {caps}"
    );

    // A regular member holds no explicit caps (posting is implicit, not a cap).
    ada.send("CAPS ada #general");
    let Event::Caps { caps, .. } = ada.recv().await.event else {
        panic!()
    };
    assert_eq!(caps, "", "regular member: {caps:?}");
}

#[tokio::test]
async fn members_carries_stored_presence() {
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    ada.send("PRESENCE away");
    // Serialize: the PONG proves ada's PRESENCE was processed (FIFO) before we
    // ask for the roster, so the shared presence map is written.
    ada.send("PING sync");
    assert!(matches!(ada.recv().await.event, Event::Pong { .. }));

    let mut bob = joined(&ctx, "bob", "#general").await;
    ada.recv().await; // MEMBER: bob joined

    bob.send("MEMBERS #general");
    let mut ada_status = None;
    loop {
        match bob.recv().await.event {
            Event::BatchEnd { .. } => break,
            Event::Presence { user, status } if user.account.as_str() == "ada" => {
                ada_status = Some(status.to_string());
            }
            _ => {}
        }
    }
    assert_eq!(
        ada_status.as_deref(),
        Some("away"),
        "presence rides with MEMBERS"
    );
}

// ---- §6.3 persistent membership (auto-rejoin on auth) ----

#[tokio::test]
async fn membership_is_restored_on_a_new_session() {
    let ctx = ctx(&["#general"]);
    let _ada = joined(&ctx, "ada", "#general").await; // registers ada + joins #general

    // A fresh session for ada authenticates — the server auto-rejoins her
    // persisted channels, so the client's tiles reappear without re-joining.
    let mut second = helloed(&ctx).await;
    second.send(&format!("@label=a AUTH PASSWORD ada :{PASSWORD}"));
    assert!(matches!(second.recv().await.event, Event::Welcome { .. }));

    let mut rejoined = false;
    for _ in 0..4 {
        match second.recv().await.event {
            Event::Member {
                channel,
                action: MemberAction::Join,
                ..
            } if channel.as_str() == "#general" => {
                rejoined = true;
                break;
            }
            _ => {}
        }
    }
    assert!(rejoined, "the new session is auto-rejoined to #general");
}

#[tokio::test]
async fn parting_stops_auto_rejoin() {
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    ada.send("PART #general");
    assert!(matches!(
        ada.recv().await.event,
        Event::Member {
            action: MemberAction::Part,
            ..
        }
    ));

    // A new session must NOT be auto-rejoined to the parted channel.
    let mut second = helloed(&ctx).await;
    second.send(&format!("@label=a AUTH PASSWORD ada :{PASSWORD}"));
    assert!(matches!(second.recv().await.event, Event::Welcome { .. }));
    second.send("@label=p PING x");
    // Only PONG should arrive — no MEMBER rejoin before it.
    loop {
        match second.recv().await.event {
            Event::Pong { .. } => break,
            Event::Member {
                action: MemberAction::Join,
                ..
            } => {
                panic!("parted channel should not be auto-rejoined")
            }
            _ => {}
        }
    }
}

// ---- §6.5 named roles (capability-token bundles) ----

#[tokio::test]
async fn roles_define_list_and_assign_grants_the_bundle() {
    let ctx = ctx_ops(&["#general"], &["root"]);
    let mut root = ready(&ctx, "root").await;
    let _bob = ready(&ctx, "bob").await;

    // Define a role at the global scope (operator authority) → updated batch.
    root.send("@label=c ROLE CREATE * #e8b93d mute,ban,kick :Moderator");
    assert!(matches!(root.recv().await.event, Event::BatchStart { .. }));
    let ev = root.recv().await;
    let Event::Role {
        name,
        caps,
        color,
        scope,
        ..
    } = &ev.event
    else {
        panic!("expected ROLE, got {ev:?}");
    };
    assert_eq!(name, "Moderator");
    assert_eq!(color, "#e8b93d");
    assert_eq!(scope, "*");
    assert_eq!(caps, "mute,ban,kick");
    assert!(matches!(root.recv().await.event, Event::BatchEnd { .. }));

    // Assign it to bob → grants the bundle (a signed Token).
    root.send("@label=a ROLE ASSIGN * bob :Moderator");
    let ev = root.recv().await;
    assert!(matches!(&ev.event, Event::Token { .. }), "got {ev:?}");

    // bob now effectively holds the role's caps.
    root.send("@label=q CAPS bob *");
    let ev = root.recv().await;
    let Event::Caps { caps, .. } = &ev.event else {
        panic!("expected CAPS, got {ev:?}");
    };
    assert!(
        caps.contains("mute") && caps.contains("ban") && caps.contains("kick"),
        "bob holds the role's caps, got {caps}"
    );
}

#[tokio::test]
async fn role_assigns_to_a_foreign_user() {
    let ctx = ctx_ops(&["#general"], &["root"]);
    let mut root = ready(&ctx, "root").await;
    // Define a role at the global scope.
    root.send("ROLE CREATE * #e8b93d mute,ban :Moderator");
    root.recv().await; // BatchStart
    root.recv().await; // Role
    root.recv().await; // BatchEnd

    // Assign it to a *federated* user (account@network) — membership recorded by
    // the network-qualified handle, caps granted to the foreign subject (§10.4).
    root.send("@label=a ROLE ASSIGN * alice@peer.example :Moderator");
    let reply = root.recv().await;
    assert_eq!(reply.label.as_deref(), Some("a"));
    assert!(
        matches!(&reply.event, Event::Token { subject, .. } if subject == "alice@peer.example"),
        "assigning to a foreign user mints the bundle, got {reply:?}"
    );

    // ROLES-OF reflects the membership (recognition), keyed by account@network.
    root.send("ROLES-OF * alice@peer.example");
    let reply = root.recv().await;
    let Event::RoleMember { account, roles, .. } = &reply.event else {
        panic!("expected ROLE-MEMBER, got {reply:?}");
    };
    assert_eq!(account, "alice@peer.example");
    assert_eq!(roles, "Moderator");
}

#[tokio::test]
async fn renaming_a_role_keeps_its_members_and_caps() {
    let ctx = ctx_ops(&["#general"], &["root"]);
    let mut root = ready(&ctx, "root").await;
    let _bob = ready(&ctx, "bob").await;

    root.send("ROLE CREATE * #e8b93d mute,ban :Moderator");
    root.recv().await; // BatchStart
    root.recv().await; // Role
    root.recv().await; // BatchEnd
    root.send("@label=a ROLE ASSIGN * bob :Moderator");
    root.recv().await; // Token

    // Rename → the ROLES batch comes back under the new name, definition intact.
    root.send("@label=r ROLE RENAME * :Moderator,Head Moderator");
    assert!(matches!(root.recv().await.event, Event::BatchStart { .. }));
    let ev = root.recv().await;
    let Event::Role { name, caps, .. } = &ev.event else {
        panic!("expected ROLE, got {ev:?}");
    };
    assert_eq!(name, "Head Moderator");
    assert_eq!(caps, "mute,ban");
    assert!(matches!(root.recv().await.event, Event::BatchEnd { .. }));

    // Membership followed the rename — a rename must never un-role anyone.
    root.send("ROLES-OF * bob");
    let ev = root.recv().await;
    let Event::RoleMember { roles, .. } = &ev.event else {
        panic!("expected ROLE-MEMBER, got {ev:?}");
    };
    assert_eq!(roles, "Head Moderator");

    // ...and the granted bundle is untouched (authority is caps, not the name).
    root.send("@label=q CAPS bob *");
    let ev = root.recv().await;
    let Event::Caps { caps, .. } = &ev.event else {
        panic!("expected CAPS, got {ev:?}");
    };
    assert!(caps.contains("mute") && caps.contains("ban"), "got {caps}");
}

#[tokio::test]
async fn renaming_onto_an_existing_role_is_refused() {
    let ctx = ctx_ops(&["#general"], &["root"]);
    let mut root = ready(&ctx, "root").await;
    for name in ["Moderator", "Helper"] {
        root.send(&format!("ROLE CREATE * #e8b93d mute :{name}"));
        root.recv().await;
        root.recv().await;
        if name == "Helper" {
            root.recv().await; // second role in the batch
        }
        root.recv().await;
    }
    // Merging two bundles under one name is not a rename.
    root.send("@label=x ROLE RENAME * :Helper,Moderator");
    root.expect_err(ErrCode::Policy).await;

    // An absent source is NO-SUCH-TARGET, same as any other hidden/absent target.
    root.send("@label=y ROLE RENAME * :Ghost,Phantom");
    root.expect_err(ErrCode::NoSuchTarget).await;
}

#[tokio::test]
async fn role_rename_needs_admin_authority() {
    let ctx = ctx_ops(&["#general"], &["root"]);
    let mut root = ready(&ctx, "root").await;
    root.send("ROLE CREATE * #fff send :Member");
    root.recv().await;
    root.recv().await;
    root.recv().await;

    let mut mallory = ready(&ctx, "mallory").await; // no caps
    mallory.send("@label=x ROLE RENAME * :Member,Owner");
    let reply = mallory.expect_err(ErrCode::CapRequired).await;
    let Event::Err(err) = &reply.event else {
        unreachable!()
    };
    assert_eq!(err.context.as_deref(), Some("ns-admin"));
}

#[tokio::test]
async fn role_management_needs_admin_authority() {
    let ctx = ctx_ops(&["#general"], &["root"]);
    let _root = ready(&ctx, "root").await;
    let mut mallory = ready(&ctx, "mallory").await; // no caps

    mallory.send("@label=x ROLE CREATE * #fff send :Sneaky");
    let reply = mallory.expect_err(ErrCode::CapRequired).await;
    let Event::Err(err) = &reply.event else {
        unreachable!()
    };
    assert_eq!(err.context.as_deref(), Some("ns-admin"));
}

async fn drain_until_label(c: &mut Client, label: &str) -> Reply {
    loop {
        let r = c.recv().await;
        if r.label.as_deref() == Some(label) {
            return r;
        }
    }
}

#[tokio::test]
async fn assigning_a_namespace_role_grants_its_channel_permissions() {
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    let _bob = ready(&ctx, "bob").await;
    let root = root_key_b64();

    ada.send(&format!("@label=n;root={root} NS CREATE gaming public"));
    drain_until_label(&mut ada, "n").await;
    ada.send("@label=c CHANNEL CREATE #gaming/stage");
    drain_until_label(&mut ada, "c").await;

    // A namespace role (react) plus a same-named *channel* role (send) — the
    // channel role is the role's per-channel permission.
    ada.send("@label=r1 ROLE CREATE ns:gaming #e8b93d react :Speaker");
    drain_until_label(&mut ada, "r1").await;
    ada.send("@label=r2 ROLE CREATE #gaming/stage #e8b93d send :Speaker");
    drain_until_label(&mut ada, "r2").await;

    // Assigning the namespace role should propagate the channel permission.
    ada.send("@label=a ROLE ASSIGN ns:gaming bob :Speaker");
    drain_until_label(&mut ada, "a").await;

    ada.send("@label=q CAPS bob #gaming/stage");
    let ev = drain_until_label(&mut ada, "q").await;
    let Event::Caps { caps, .. } = &ev.event else {
        panic!("expected CAPS, got {ev:?}");
    };
    assert!(
        caps.contains("send"),
        "bob gains send in the channel via the namespace role, got {caps}"
    );
}

#[tokio::test]
async fn adding_a_channel_permission_propagates_to_existing_holders() {
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    let _bob = ready(&ctx, "bob").await;
    let root = root_key_b64();
    ada.send(&format!("@label=n;root={root} NS CREATE gaming public"));
    drain_until_label(&mut ada, "n").await;
    ada.send("@label=c CHANNEL CREATE #gaming/stage");
    drain_until_label(&mut ada, "c").await;
    ada.send("@label=r1 ROLE CREATE ns:gaming #e8b93d react :Speaker");
    drain_until_label(&mut ada, "r1").await;

    // Assign the role FIRST (bob holds react at ns:gaming, no channel perm yet).
    ada.send("@label=a ROLE ASSIGN ns:gaming bob :Speaker");
    drain_until_label(&mut ada, "a").await;

    // THEN add the channel permission — it must reach bob with no re-assignment.
    ada.send("@label=r2 ROLE CREATE #gaming/stage #e8b93d send :Speaker");
    drain_until_label(&mut ada, "r2").await;

    ada.send("@label=q CAPS bob #gaming/stage");
    let ev = drain_until_label(&mut ada, "q").await;
    let Event::Caps { caps, .. } = &ev.event else {
        panic!("expected CAPS, got {ev:?}");
    };
    assert!(
        caps.contains("send"),
        "an already-assigned holder gains a newly-added channel permission, got {caps}"
    );
}

#[tokio::test]
async fn roles_are_explicit_membership_not_derived() {
    let ctx = ctx_ops(&["#general"], &["root"]);
    let mut root = ready(&ctx, "root").await;
    let _bob = ready(&ctx, "bob").await;

    root.send("@label=c ROLE CREATE * #e8b93d mute,ban :Mod");
    drain_until_label(&mut root, "c").await;

    // bob holds no roles yet, even though the operator implicitly has every cap.
    root.send("@label=q1 ROLES-OF * bob");
    let ev = drain_until_label(&mut root, "q1").await;
    assert!(matches!(&ev.event, Event::RoleMember { roles, .. } if roles.is_empty()));

    // Assign, then it shows; unassign, then it's gone.
    root.send("@label=a ROLE ASSIGN * bob :Mod");
    drain_until_label(&mut root, "a").await;
    root.send("@label=q2 ROLES-OF * bob");
    let ev = drain_until_label(&mut root, "q2").await;
    assert!(
        matches!(&ev.event, Event::RoleMember { roles, .. } if roles == "Mod"),
        "got {ev:?}"
    );

    root.send("@label=u ROLE UNASSIGN * bob :Mod");
    let ev = drain_until_label(&mut root, "u").await; // UNASSIGN → ROLE-MEMBER
    assert!(
        matches!(&ev.event, Event::RoleMember { roles, .. } if roles.is_empty()),
        "got {ev:?}"
    );
}

// ---- §10.3 display profiles (M-prof-3) ----

#[tokio::test]
async fn profile_set_acks_and_broadcasts_to_co_members() {
    let ctx = ctx(&["#general"]);
    let mut bob = joined(&ctx, "bob", "#general").await;
    let mut alice = joined(&ctx, "alice", "#general").await;

    // alice sets her profile (display name with a space, escaped in the tag).
    alice.send("@label=p;display=Ada\\sL.;avatar=b3-ada PROFILE SET");
    let reply = alice.recv().await;
    assert_eq!(reply.label.as_deref(), Some("p"));
    let Event::Profile {
        user,
        display,
        avatar,
    } = &reply.event
    else {
        panic!("expected PROFILE ack, got {reply:?}");
    };
    assert_eq!(user.account.as_str(), "alice");
    assert_eq!(user.network.as_str(), "test.example"); // qualified with our network
    assert_eq!(display.as_deref(), Some("Ada L."));
    assert_eq!(avatar.as_deref(), Some("b3-ada"));

    // bob (a co-member) sees alice's new profile (unlabeled broadcast).
    let reply = loop {
        let r = bob.recv().await;
        if matches!(r.event, Event::Profile { .. }) {
            break r;
        }
    };
    assert!(matches!(
        &reply.event,
        Event::Profile { user, avatar, .. }
            if user.account.as_str() == "alice" && avatar.as_deref() == Some("b3-ada")
    ));
    assert_eq!(reply.label, None); // broadcast copies carry no label (§3.5)
}

#[tokio::test]
async fn profile_partial_update_and_query() {
    let ctx = ctx(&["#general"]);
    let mut alice = joined(&ctx, "alice", "#general").await;

    alice.send("@display=Ada;avatar=b3-1 PROFILE SET");
    assert!(matches!(alice.recv().await.event, Event::Profile { .. }));
    // Partial update: change only the avatar; display is left intact.
    alice.send("@avatar=b3-2 PROFILE SET");
    assert!(matches!(alice.recv().await.event, Event::Profile { .. }));

    // Query it back.
    alice.send("@label=q PROFILES alice bob");
    let reply = alice.recv().await;
    let Event::Profile {
        user,
        display,
        avatar,
    } = &reply.event
    else {
        panic!("expected PROFILE, got {reply:?}");
    };
    assert_eq!(user.account.as_str(), "alice");
    assert_eq!(display.as_deref(), Some("Ada")); // preserved through the avatar-only update
    assert_eq!(avatar.as_deref(), Some("b3-2"));
    // bob has no profile → omitted (not an error).
}

#[tokio::test]
async fn profile_clear_via_empty_tag() {
    let ctx = ctx(&["#general"]);
    let mut alice = joined(&ctx, "alice", "#general").await;

    alice.send("@display=Ada PROFILE SET");
    assert!(matches!(alice.recv().await.event, Event::Profile { .. }));
    // A present-but-empty tag clears the field.
    alice.send("@display= PROFILE SET");
    let reply = alice.recv().await;
    assert!(matches!(
        &reply.event,
        Event::Profile { display, .. } if display.is_none()
    ));
}

// ---- §16 WEFT-RT voice signaling (M-voice-1) ----

/// A stand-in SFU: it authorizes nothing (core already did) — it just mints a
/// token and echoes SDP so the signaling relay is observable without WebRTC.
struct MockVoice;

#[async_trait::async_trait]
impl VoiceBackend for MockVoice {
    async fn join(&self, req: VoiceJoinReq) -> Result<VoiceGrant, VoiceError> {
        Ok(VoiceGrant {
            mode: weft_proto::VoiceTransport::Webrtc,
            token: format!("vtok-{}-{}", req.channel, req.session),
            room: None,
            endpoint: None,
        })
    }
    async fn describe(
        &self,
        _session: u64,
        _channel: &weft_proto::ChannelName,
        sdp: String,
    ) -> Result<String, VoiceError> {
        Ok(format!("answer-to:{sdp}"))
    }
    async fn candidate(
        &self,
        _session: u64,
        _channel: &weft_proto::ChannelName,
        _candidate: String,
    ) -> Result<(), VoiceError> {
        Ok(())
    }
    async fn leave(&self, _session: u64, _channel: &weft_proto::ChannelName) {}
    async fn set_muted(&self, _session: u64, _channel: &weft_proto::ChannelName, _muted: bool) {}
}

fn ctx_voice(channels: &[&str], operators: &[&str]) -> Arc<ServerCtx> {
    let ctx = ctx_ops(channels, operators);
    ctx.set_voice_backend(Arc::new(MockVoice));
    ctx
}

/// Next `VOICE STATE` from a co-member's stream, skipping the MEMBER/PRESENCE
/// lines that interleave when several clients share a channel.
async fn next_voice_state(client: &mut Client) -> Reply {
    loop {
        let reply = client.recv().await;
        if matches!(reply.event, Event::VoiceState { .. }) {
            return reply;
        }
    }
}

/// Create a voice channel `name` via a fresh operator session, which then drops
/// (the channel persists in the registry + store). Returns a voice-enabled ctx.
async fn voice_ctx_with(name: &str) -> Arc<ServerCtx> {
    let ctx = ctx_voice(&[], &["boss"]);
    let mut boss = ready_op(&ctx, "boss").await;
    boss.send(&format!("CHANNEL CREATE {name} voice"));
    assert!(matches!(boss.recv().await.event, Event::Policy { .. }));
    ctx
}

#[tokio::test]
async fn voice_join_without_backend_is_unsupported() {
    // No backend installed → the verb is known but the server has no SFU.
    let ctx = ctx(&["#general"]);
    let mut alice = ready(&ctx, "alice").await;
    alice.send("@label=v VOICE JOIN #general");
    let reply = alice.expect_err(ErrCode::Unsupported).await;
    assert_eq!(reply.label.as_deref(), Some("v"));
}

#[tokio::test]
async fn voice_join_a_text_or_missing_channel_is_no_such_target() {
    // §16 voice-only: a text channel (or a nonexistent one) is not a voice
    // target — both collapse to NO-SUCH-TARGET (invariant 1).
    let ctx = ctx_voice(&["#general"], &[]);
    let mut alice = ready(&ctx, "alice").await;
    alice.send("@label=t VOICE JOIN #general"); // a text channel
    assert_eq!(
        alice
            .expect_err(ErrCode::NoSuchTarget)
            .await
            .label
            .as_deref(),
        Some("t")
    );
    alice.send("@label=m VOICE JOIN #nope"); // nonexistent
    assert_eq!(
        alice
            .expect_err(ErrCode::NoSuchTarget)
            .await
            .label
            .as_deref(),
        Some("m")
    );
}

#[tokio::test]
async fn voice_channel_is_not_text_joinable() {
    // §16 the IRC-protection guarantee: a text JOIN to a voice channel is
    // NO-SUCH-TARGET, so voice channels never surface to text-only (IRC) clients.
    let ctx = voice_ctx_with("#lounge").await;
    let mut alice = ready(&ctx, "alice").await;
    alice.send("@label=j JOIN #lounge");
    assert_eq!(
        alice
            .expect_err(ErrCode::NoSuchTarget)
            .await
            .label
            .as_deref(),
        Some("j")
    );
}

#[tokio::test(start_paused = true)]
async fn a_crashed_voice_client_leaves_the_roster_promptly() {
    // §16 regression: a crashed client sends no FIN over QUIC (it's UDP), so the
    // only signal the server gets is silence. A session *in a voice room* must
    // therefore be reaped on the short voice deadline (~30 s), not the 120 s
    // text one — else the caller haunts every co-member's roster for two minutes.
    let ctx = voice_ctx_with("#lounge").await;

    let mut bob = ready(&ctx, "bob").await;
    bob.send("VOICE JOIN #lounge");
    assert!(matches!(bob.recv().await.event, Event::VoiceOffer { .. }));
    // bob is a *healthy* client: he keeps PINGing, so only alice goes quiet.
    let _bob_alive = bob.keepalive();

    let mut alice = ready(&ctx, "alice").await;
    alice.send("VOICE JOIN #lounge");
    assert!(matches!(alice.recv().await.event, Event::VoiceOffer { .. }));
    let reply = next_voice_state(&mut bob).await; // alice entered
    assert!(
        matches!(&reply.event, Event::VoiceState { action, .. } if *action == VoiceAction::Join)
    );

    // alice "crashes": her client stops speaking entirely but never closes the
    // stream — exactly what a dead QUIC peer looks like from the server. Holding
    // `alice` keeps her sender alive, so this is silence, not a disconnect.
    let started = tokio::time::Instant::now();

    // bob learns she's gone, and well inside the 120 s text idle window.
    let reply = loop {
        let reply = bob.recv_slow().await;
        if matches!(reply.event, Event::VoiceState { .. }) {
            break reply;
        }
    };
    let Event::VoiceState { user, action, .. } = &reply.event else {
        unreachable!()
    };
    assert_eq!(user.account.as_str(), "alice");
    assert_eq!(*action, VoiceAction::Leave);
    let waited = started.elapsed();
    assert!(
        waited < READY_IDLE_SECS,
        "ghost lingered {waited:?} — the voice deadline isn't being applied"
    );
    drop(alice);
}

/// The text-session idle ceiling, as a test-visible bound (see `READY_IDLE`).
const READY_IDLE_SECS: Duration = Duration::from_secs(120);

#[tokio::test]
async fn voice_join_offers_token_and_announces_to_members() {
    let ctx = voice_ctx_with("#lounge").await;

    // bob joins voice first (subscribing to the room).
    let mut bob = ready(&ctx, "bob").await;
    bob.send("VOICE JOIN #lounge");
    assert!(matches!(bob.recv().await.event, Event::VoiceOffer { .. }));

    // alice joins voice → labeled VOICE OFFER with a token, endpoint absent.
    let mut alice = ready(&ctx, "alice").await;
    alice.send("@label=v1 VOICE JOIN #lounge");
    let reply = alice.recv().await;
    assert_eq!(reply.label.as_deref(), Some("v1"));
    let Event::VoiceOffer {
        channel,
        mode,
        token,
        endpoint,
        ..
    } = &reply.event
    else {
        panic!("expected VOICE OFFER, got {reply:?}");
    };
    assert_eq!(channel.as_str(), "#lounge");
    assert_eq!(*mode, weft_proto::VoiceTransport::Webrtc);
    assert!(token.starts_with("vtok-"), "token: {token}");
    assert!(endpoint.is_none());

    // bob (already in the room) sees alice enter voice, not muted (open channel).
    let reply = next_voice_state(&mut bob).await;
    let Event::VoiceState {
        user,
        action,
        muted,
        ..
    } = &reply.event
    else {
        unreachable!()
    };
    assert_eq!(user.account.as_str(), "alice");
    assert_eq!(*action, VoiceAction::Join);
    assert!(!*muted);

    // alice negotiates: her SDP offer gets the SFU's answer back as VOICE DESC
    // (skipping the roster snapshot she also received on join).
    alice.send("@label=v2 VOICE DESC #lounge :v=0\\r\\nmy-offer");
    let reply = drain_until_label(&mut alice, "v2").await;
    let Event::VoiceDesc { sdp, .. } = &reply.event else {
        panic!("expected VOICE DESC answer, got {reply:?}");
    };
    assert_eq!(sdp, "answer-to:v=0\r\nmy-offer");

    // alice leaves → labeled VOICE STATE leave ack; bob sees the leave too.
    alice.send("@label=v3 VOICE LEAVE #lounge");
    let reply = drain_until_label(&mut alice, "v3").await;
    assert!(matches!(
        &reply.event,
        Event::VoiceState { action, user, .. }
            if *action == VoiceAction::Leave && user.account.as_str() == "alice"
    ));
    let reply = next_voice_state(&mut bob).await;
    assert!(
        matches!(&reply.event, Event::VoiceState { action, .. } if *action == VoiceAction::Leave)
    );

    // Leaving again → nothing to leave (uniform NO-SUCH-TARGET).
    alice.send("VOICE LEAVE #lounge");
    alice.expect_err(ErrCode::NoSuchTarget).await;
}

#[tokio::test]
async fn voice_muted_member_joins_but_renders_muted() {
    let ctx = ctx_voice(&[], &["boss"]);
    let mut boss = ready_op(&ctx, "boss").await;
    boss.send("CHANNEL CREATE #lounge voice");
    assert!(matches!(boss.recv().await.event, Event::Policy { .. }));

    // A network-wide mute (M7) removes `speak` but not the join itself.
    boss.send("@label=m MUTE * alice");
    let reply = drain_until_label(&mut boss, "m").await;
    assert!(matches!(reply.event, Event::Moderated { .. }));

    let mut bob = ready(&ctx, "bob").await;
    bob.send("VOICE JOIN #lounge");
    assert!(matches!(bob.recv().await.event, Event::VoiceOffer { .. }));

    let mut alice = ready(&ctx, "alice").await;
    alice.send("VOICE JOIN #lounge");
    assert!(matches!(alice.recv().await.event, Event::VoiceOffer { .. }));

    // bob sees alice join voice, flagged muted (can't speak).
    let reply = next_voice_state(&mut bob).await;
    assert!(matches!(
        &reply.event,
        Event::VoiceState { action, muted, .. } if *action == VoiceAction::Join && *muted
    ));
}

#[tokio::test]
async fn voice_banned_member_cannot_join() {
    let ctx = ctx_voice(&[], &["boss"]);
    let mut boss = ready_op(&ctx, "boss").await;
    boss.send("CHANNEL CREATE #lounge voice");
    assert!(matches!(boss.recv().await.event, Event::Policy { .. }));

    // A `*`-scope ban covers #lounge — she is barred from voice.
    boss.send("@label=b BAN * alice");
    let reply = drain_until_label(&mut boss, "b").await;
    assert!(matches!(reply.event, Event::Moderated { .. }));

    let mut alice = ready(&ctx, "alice").await;
    alice.send("@label=v VOICE JOIN #lounge");
    let reply = alice.expect_err(ErrCode::Forbidden).await;
    assert_eq!(reply.label.as_deref(), Some("v"));
}

#[tokio::test]
async fn voice_join_receives_roster_snapshot() {
    // §16 (M-voice-4) a joiner learns who's already in the room, not just future
    // arrivals — a VOICE STATE snapshot follows the OFFER.
    let ctx = voice_ctx_with("#lounge").await;
    let mut bob = ready(&ctx, "bob").await;
    bob.send("VOICE JOIN #lounge");
    assert!(matches!(bob.recv().await.event, Event::VoiceOffer { .. }));

    let mut alice = ready(&ctx, "alice").await;
    alice.send("@label=j VOICE JOIN #lounge");
    assert!(matches!(
        drain_until_label(&mut alice, "j").await.event,
        Event::VoiceOffer { .. }
    ));
    // The snapshot names the existing member (bob), unlabeled.
    let snap = next_voice_state(&mut alice).await;
    assert!(matches!(
        &snap.event,
        Event::VoiceState { user, action, .. }
            if user.account.as_str() == "bob" && *action == VoiceAction::Join
    ));
    assert_eq!(snap.label, None);
}

#[tokio::test]
async fn voice_mute_silences_live_and_updates_the_room() {
    // §16 (M-voice-4) a moderator's MUTE of a voice participant drops their audio
    // at the SFU and broadcasts a VOICE STATE `update` so the room re-renders.
    let ctx = ctx_voice(&[], &["boss"]);
    let mut boss = ready_op(&ctx, "boss").await;
    boss.send("CHANNEL CREATE #lounge voice");
    assert!(matches!(boss.recv().await.event, Event::Policy { .. }));

    let mut bob = ready(&ctx, "bob").await;
    bob.send("VOICE JOIN #lounge");
    assert!(matches!(bob.recv().await.event, Event::VoiceOffer { .. }));
    let mut alice = ready(&ctx, "alice").await;
    alice.send("VOICE JOIN #lounge");
    assert!(matches!(alice.recv().await.event, Event::VoiceOffer { .. }));

    // boss mutes alice at the channel scope.
    boss.send("@label=m MUTE #lounge alice");
    assert!(matches!(
        drain_until_label(&mut boss, "m").await.event,
        Event::Moderated { .. }
    ));

    // bob (in the room) sees alice's live mute as a VOICE STATE update.
    let upd = loop {
        let r = next_voice_state(&mut bob).await;
        if matches!(&r.event, Event::VoiceState { action, .. } if *action == VoiceAction::Update) {
            break r;
        }
    };
    assert!(matches!(
        &upd.event,
        Event::VoiceState { user, muted, .. }
            if user.account.as_str() == "alice" && *muted
    ));
}

#[tokio::test]
async fn voice_ban_ejects_the_target_from_the_room() {
    // §16 (M-lk-2) a channel-scope BAN removes the target from that channel's
    // voice room (backend peer torn down); co-members see a VOICE STATE leave.
    let ctx = ctx_voice(&[], &["boss"]);
    let mut boss = ready_op(&ctx, "boss").await;
    boss.send("CHANNEL CREATE #lounge voice");
    assert!(matches!(boss.recv().await.event, Event::Policy { .. }));

    let mut bob = ready(&ctx, "bob").await;
    bob.send("VOICE JOIN #lounge");
    assert!(matches!(bob.recv().await.event, Event::VoiceOffer { .. }));
    let mut alice = ready(&ctx, "alice").await;
    alice.send("VOICE JOIN #lounge");
    assert!(matches!(alice.recv().await.event, Event::VoiceOffer { .. }));

    // boss bans alice at the channel scope.
    boss.send("@label=b BAN #lounge alice :raid");
    assert!(matches!(
        drain_until_label(&mut boss, "b").await.event,
        Event::Moderated { .. }
    ));

    // bob (still in the room) sees alice ejected from voice.
    let leave = loop {
        let r = next_voice_state(&mut bob).await;
        if matches!(&r.event, Event::VoiceState { action, .. } if *action == VoiceAction::Leave) {
            break r;
        }
    };
    assert!(matches!(
        &leave.event,
        Event::VoiceState { user, .. } if user.account.as_str() == "alice"
    ));
}

// ---- §16 M-lk-3a: federated voice (VOICE REQUEST → VOICE GRANT gating) ----

/// A stand-in LiveKit admin: mints an opaque token, records nothing.
struct StubLk;

#[async_trait::async_trait]
impl LiveKitAdmin for StubLk {
    fn access_token(&self, req: &LiveKitTokenReq) -> String {
        format!("jwt:{}:{}", req.room, req.identity)
    }
    async fn set_participant_muted(&self, _room: &str, _identity: &str, _muted: bool) {}
    async fn remove_participant(&self, _room: &str, _identity: &str) {}
}

/// An open-federation ctx with a LiveKit voice backend installed.
fn ctx_livekit_federation() -> Arc<ServerCtx> {
    let ctx = ctx_open_federation(&["#lounge"], &[]);
    ctx.set_voice_backend(Arc::new(LiveKitBackend::new(
        Arc::new(StubLk),
        "wss://livekit.test.example".to_string(),
        "test.example".parse().unwrap(),
        600,
    )));
    ctx
}

/// A v1 manifest naming us as peer, with the §16 `voice` flag set as requested.
fn peer_manifest_voice(key: &Keypair, channels: &[&str], voice: bool) -> String {
    weft_core::Manifest {
        peer: "test.example".to_string(),
        version: 1,
        channels: channels.iter().map(|c| c.to_string()).collect(),
        history: "from-epoch".to_string(),
        media: "none".to_string(),
        typing: false,
        voice,
        created: 0,
        updated: 0,
    }
    .sign(key)
    .to_b64()
}

/// Propose + auto-ack `channels` with an explicit `voice` flag (peer → us).
async fn propose_voice(bridge: &mut Client, key: &Keypair, channels: &[&str], voice: bool) {
    let chan = channels[0];
    bridge.send(&format!(
        "@manifest={} BRIDGE PROPOSE {chan} test.example",
        peer_manifest_voice(key, channels, voice)
    ));
    let ack = bridge.recv_raw().await;
    assert!(ack.contains("BRIDGE ACCEPT test.example 1"), "{ack}");
}

#[tokio::test]
async fn voice_request_grants_when_the_channel_is_voice_federated() {
    let key = Keypair::generate();
    let ctx = ctx_livekit_federation();
    let mut bridge = bridged_peer(&ctx, "test.example", &key).await;

    // The bridge is acked with #lounge federating voice (voice=on).
    propose_voice(&mut bridge, &key, &["#lounge"], true).await;

    // The peer asks us to relay #lounge → we answer VOICE GRANT with the LiveKit
    // credentials + a signed relay grant.
    bridge.send("@label=vr VOICE REQUEST * #lounge");
    let reply = drain_until_label(&mut bridge, "vr").await;
    let Event::VoiceGrant {
        channel,
        url,
        room,
        token,
        grant,
        ttl,
    } = &reply.event
    else {
        panic!("expected VOICE GRANT, got {reply:?}");
    };
    assert_eq!(channel.as_str(), "#lounge");
    assert_eq!(url, "wss://livekit.test.example");
    assert_eq!(room, "wv:test.example:#lounge");
    assert!(token.starts_with("jwt:"), "livekit token: {token}");
    assert_eq!(*ttl, 600);

    // The relay grant verifies against our network key, naming the peer grantee.
    let signed = weft_crypto::SignedVoiceRelayGrant::from_b64(grant).expect("decode grant");
    assert!(signed.verify());
    assert_eq!(signed.grant.grantee, "test.example");
    assert_eq!(signed.grant.channel, "#lounge");
}

#[tokio::test]
async fn voice_request_refused_when_voice_not_federated() {
    // §16 invariant 1: a channel bridged with voice=off is indistinguishable from
    // a non-existent one — both are NO-SUCH-TARGET, no VOICE GRANT.
    let key = Keypair::generate();
    let ctx = ctx_livekit_federation();
    let mut bridge = bridged_peer(&ctx, "test.example", &key).await;
    propose_voice(&mut bridge, &key, &["#lounge"], false).await;

    // voice=off → refused.
    bridge.send("@label=a VOICE REQUEST * #lounge");
    let a = drain_until_label(&mut bridge, "a").await;
    assert!(
        matches!(&a.event, Event::Err(e) if e.code == ErrCode::NoSuchTarget),
        "{a:?}"
    );

    // A channel absent from the manifest → the same refusal.
    bridge.send("@label=b VOICE REQUEST * #nope");
    let b = drain_until_label(&mut bridge, "b").await;
    assert!(
        matches!(&b.event, Event::Err(e) if e.code == ErrCode::NoSuchTarget),
        "{b:?}"
    );
}

// ---- §10.5 account verification (email code flow + self-attested birthday) ----

/// A stand-in mailer: records the (address, code) instead of sending SMTP.
#[derive(Default)]
struct MockMailer {
    sent: std::sync::Mutex<Vec<(String, String)>>,
}

#[async_trait::async_trait]
impl Mailer for MockMailer {
    async fn send_code(&self, address: &str, code: &str) {
        self.sent
            .lock()
            .unwrap()
            .push((address.to_string(), code.to_string()));
    }
}

#[tokio::test]
async fn verify_email_code_flow_birthday_and_list() {
    let ctx = ctx(&[]);
    let mailer = Arc::new(MockMailer::default());
    ctx.set_mailer(mailer.clone());
    let mut ada = ready(&ctx, "ada").await;

    // VERIFY EMAIL → a pending claim + a mailed one-time code.
    ada.send("@label=e VERIFY EMAIL ada@example.com");
    let reply = drain_until_label(&mut ada, "e").await;
    assert!(
        matches!(&reply.event,
            Event::Verified { kind, subject, state }
                if kind == "email" && subject == "ada@example.com"
                   && *state == weft_proto::VerifyState::Pending),
        "{reply:?}"
    );
    let (addr, code) = mailer
        .sent
        .lock()
        .unwrap()
        .last()
        .cloned()
        .expect("a code was mailed");
    assert_eq!(addr, "ada@example.com");

    // A wrong code is refused (FORBIDDEN), the claim stays pending.
    ada.send("@label=w VERIFY CONFIRM email 0000000"); // 7 digits ≠ any 6-digit code
    let w = drain_until_label(&mut ada, "w").await;
    assert!(
        matches!(&w.event, Event::Err(e) if e.code == ErrCode::Forbidden),
        "{w:?}"
    );

    // The right code confirms it.
    ada.send(&format!("@label=c VERIFY CONFIRM email {code}"));
    let reply = drain_until_label(&mut ada, "c").await;
    assert!(
        matches!(&reply.event,
            Event::Verified { kind, subject, state }
                if kind == "email" && subject == "ada@example.com"
                   && *state == weft_proto::VerifyState::Confirmed),
        "{reply:?}"
    );

    // The code is single-use: replaying it now fails.
    ada.send(&format!("@label=r VERIFY CONFIRM email {code}"));
    let r = drain_until_label(&mut ada, "r").await;
    assert!(
        matches!(&r.event, Event::Err(e) if e.code == ErrCode::Forbidden),
        "{r:?}"
    );

    // BIRTHDAY is self-attested → confirmed on the spot (no code).
    ada.send("@label=b VERIFY BIRTHDAY 2000-05-15");
    let reply = drain_until_label(&mut ada, "b").await;
    assert!(
        matches!(&reply.event,
            Event::Verified { kind, state, .. }
                if kind == "birthday" && *state == weft_proto::VerifyState::Confirmed),
        "{reply:?}"
    );
    // A malformed birthday is rejected.
    ada.send("@label=bad VERIFY BIRTHDAY not-a-date");
    let bad = drain_until_label(&mut ada, "bad").await;
    assert!(
        matches!(&bad.event, Event::Err(e) if e.code == ErrCode::Malformed),
        "{bad:?}"
    );

    // VERIFY LIST → both claims, both confirmed.
    ada.send("@label=l VERIFY LIST");
    let mut kinds = std::collections::HashSet::new();
    for _ in 0..2 {
        let reply = drain_until_label(&mut ada, "l").await;
        if let Event::Verified { kind, state, .. } = &reply.event {
            assert_eq!(*state, weft_proto::VerifyState::Confirmed);
            kinds.insert(kind.clone());
        }
    }
    assert_eq!(
        kinds,
        ["email".to_string(), "birthday".to_string()]
            .into_iter()
            .collect()
    );
}

// ---- §16 M-lk-3b: the federated-voice relay lifecycle manager ----

/// A stand-in relay driver: records start/stop (and the full spec) instead of
/// running libwebrtc.
#[derive(Default)]
struct MockRelay {
    started: std::sync::Mutex<Vec<(String, String)>>, // (peer, key)
    stopped: std::sync::Mutex<Vec<(String, String)>>,
    specs: std::sync::Mutex<Vec<RelaySpec>>,
}

#[async_trait::async_trait]
impl VoiceRelay for MockRelay {
    async fn start(&self, spec: RelaySpec) {
        self.started
            .lock()
            .unwrap()
            .push((spec.peer.to_string(), spec.key.clone()));
        self.specs.lock().unwrap().push(spec);
    }
    async fn stop(&self, peer: &weft_proto::NetworkName, key: &str) {
        self.stopped
            .lock()
            .unwrap()
            .push((peer.to_string(), key.to_string()));
    }
}

fn relay_spec(peer: &str, key: &str) -> RelaySpec {
    RelaySpec {
        peer: peer.parse().unwrap(),
        key: key.to_string(),
        remote_url: "wss://f".into(),
        remote_room: "wv:fda.example:c".into(),
        remote_token: "rt".into(),
        local_url: "wss://h".into(),
        local_room: "wv:test.example:c".into(),
        local_token: "lt".into(),
    }
}

#[tokio::test]
async fn relay_lifecycle_refcounts_then_drops_by_peer() {
    let ctx = ctx(&[]);
    let relay = Arc::new(MockRelay::default());
    ctx.set_voice_relay(relay.clone());

    let f: weft_proto::NetworkName = "fda.example".parse().unwrap();

    // Two local members of the same foreign channel → the relay starts once.
    ctx.relay_acquire(relay_spec("fda.example", "#lounge"))
        .await;
    ctx.relay_acquire(relay_spec("fda.example", "#lounge"))
        .await;
    assert_eq!(relay.started.lock().unwrap().len(), 1);

    // One leaves → still live (no stop).
    ctx.relay_release(&f, "#lounge").await;
    assert!(relay.stopped.lock().unwrap().is_empty());

    // The last leaves → stop.
    ctx.relay_release(&f, "#lounge").await;
    assert_eq!(
        *relay.stopped.lock().unwrap(),
        vec![("fda.example".to_string(), "#lounge".to_string())]
    );

    // A SEVER/NETBLOCK drops every relay to a peer regardless of refcount, and
    // leaves other peers' relays alone.
    ctx.relay_acquire(relay_spec("fda.example", "#a")).await;
    ctx.relay_acquire(relay_spec("fda.example", "#b")).await;
    ctx.relay_acquire(relay_spec("other.example", "#c")).await;
    ctx.relay_drop_peer(&f).await;

    let stopped = relay.stopped.lock().unwrap();
    assert!(stopped.iter().any(|(p, c)| p == "fda.example" && c == "#a"));
    assert!(stopped.iter().any(|(p, c)| p == "fda.example" && c == "#b"));
    assert!(
        !stopped.iter().any(|(_, c)| c == "#c"),
        "other peer's relay survives: {stopped:?}"
    );
}

#[tokio::test]
async fn an_operator_disconnect_closes_the_session_and_drops_its_presence() {
    // WC7 forced logout. Suspending an account only blocks *new* logins, so the
    // panel also needs to cut the sessions it already has. A cut session must
    // unwind through the ordinary cleanup, so co-members see exactly what any
    // disconnect looks like — the member goes offline (persistent membership is
    // retained, §6.3), never a ghost that stays lit.
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    let mut bob = joined(&ctx, "bob", "#general").await;
    ada.recv().await; // bob's join broadcast

    let account: weft_proto::Account = "bob".parse().unwrap();
    assert_eq!(ctx.disconnect_account(&account).await, 1);

    // bob's stream closes...
    assert!(bob.closed().await);
    // ...and ada sees him go offline.
    let reply = loop {
        let r = ada.recv().await;
        if matches!(r.event, Event::Presence { .. }) {
            break r;
        }
    };
    let Event::Presence { user, status } = &reply.event else {
        unreachable!()
    };
    assert_eq!(user.account.as_str(), "bob");
    assert_eq!(*status, weft_proto::PresenceStatus::Offline);

    // Idempotent: an account with nothing live cuts zero.
    assert_eq!(ctx.disconnect_account(&account).await, 0);
    // ada is untouched — a targeted logout is not a broadcast shutdown.
    ada.send("@label=p PING :still here");
    assert_eq!(ada.recv().await.label.as_deref(), Some("p"));
}
