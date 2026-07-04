//! Session FSM + channel actor tests over an in-memory ControlStream —
//! the whole domain layer, no sockets (architecture doc §2).

use std::io;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use weft_core::{run_session, Attestation, ControlStream, Keypair, ServerCtx, ServerInfo};
use weft_proto::{ErrCode, Event, MemberAction, Reply};

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
        let raw = self.recv_raw().await;
        Reply::parse(&raw).expect("server sent an unparseable line")
    }

    async fn expect_err(&mut self, code: ErrCode) -> Reply {
        let reply = self.recv().await;
        match &reply.event {
            Event::Err(err) if err.code == code => reply,
            other => panic!("expected ERR {code}, got {other:?}"),
        }
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
    ctx_with(channels, true)
}

fn ctx_with(channels: &[&str], registration_open: bool) -> Arc<ServerCtx> {
    let info = ServerInfo {
        network: "test.example".parse().unwrap(),
        motd: Some("welcome!".to_string()),
        features: Vec::new(),
    };
    Arc::new(ServerCtx::new(
        info,
        channels.iter().map(|c| c.parse().unwrap()),
        Keypair::generate(),
        registration_open,
    ))
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
        matches!(&reply.event, Event::Policy { policy, .. } if policy.to_string() == "ephemeral")
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

    client.send("@label=e1 MSG @bob :hi"); // DMs are M3
    assert_eq!(
        client
            .expect_err(ErrCode::Unsupported)
            .await
            .label
            .as_deref(),
        Some("e1")
    );
    client.send("MSG #general :"); // §6.4: empty body needs attachments
    client.expect_err(ErrCode::Policy).await;
    client.send("@attach.1=blob MSG #general :look"); // media is M6
    client.expect_err(ErrCode::Unsupported).await;
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
async fn disconnect_broadcasts_part_to_members() {
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    let bob = joined(&ctx, "bob", "#general").await;
    ada.recv().await; // bob's join broadcast

    drop(bob); // connection drops without QUIT
    let reply = ada.recv().await;
    assert!(matches!(
        &reply.event,
        Event::Member { user, action: MemberAction::Part, .. }
            if user.to_string() == "bob@test.example"
    ));
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
