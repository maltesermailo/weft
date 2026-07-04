//! Session FSM + channel actor tests over an in-memory ControlStream —
//! the whole domain layer, no sockets (architecture doc §2).

use std::io;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use weft_core::{
    run_session, Attestation, ControlStream, Keypair, MemoryStore, ServerCtx, ServerInfo,
};
use weft_proto::RetentionPolicy;
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
    let info = ServerInfo {
        network: "test.example".parse().unwrap(),
        motd: Some("welcome!".to_string()),
        features: Vec::new(),
    };
    Arc::new(ServerCtx::new(
        info,
        channels
            .iter()
            .map(|(c, p)| (c.parse().unwrap(), p.parse::<RetentionPolicy>().unwrap())),
        Keypair::generate(),
        registration_open,
        Arc::new(MemoryStore::default()),
        "permanent".parse().unwrap(), // §9.5 DM default
        operators.iter().map(|o| o.parse().unwrap()),
        true, // §2.2 namespace creation open
        10,   // quota
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
    // The other device gets the sync copy.
    let sync = ada2.recv().await;
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

    // ...and delegate ns caps to someone else.
    ada.send("@label=d1 NS DELEGATE gaming bob ban,kick");
    assert!(matches!(ada.recv().await.event, Event::Token { .. }));

    // A non-owner cannot create channels in the namespace.
    let mut eve = ready(&ctx, "eve").await;
    eve.send("CHANNEL CREATE #gaming/secret");
    eve.expect_err(ErrCode::CapRequired).await;
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
        "permanent".parse().unwrap(),
        std::iter::empty::<weft_proto::Account>(),
        true, // open
        1,    // quota of 1
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
    for _ in 0..3 {
        let reply = ada.recv().await;
        assert_eq!(reply.label.as_deref(), Some("cl"));
        let Event::ChannelLayout {
            channel,
            category,
            position,
        } = reply.event
        else {
            panic!("expected CHANNEL-LAYOUT");
        };
        layout.push((channel.to_string(), category, position));
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
    // ...but can read a public/unlisted namespace's layout.
    eve.send("CHANNELS team");
    assert!(matches!(
        eve.recv().await.event,
        Event::ChannelLayout { .. }
    ));
}
