//! Session FSM + channel actor tests over an in-memory ControlStream —
//! the whole domain layer, no sockets (architecture doc §2).

use std::io;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use weft_core::{
    run_session, Attestation, ControlStream, Keypair, MemoryStore, ServerCtx, ServerInfo,
    VoiceBackend, VoiceError, VoiceGrant, VoiceJoinReq,
};
use weft_proto::RetentionPolicy;
use weft_proto::{ErrCode, Event, MemberAction, Reply, VoiceAction};

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
        Arc::new(weft_core::MemBlobStore::default()),
        "permanent".parse().unwrap(), // §9.5 DM default
        operators.iter().map(|o| o.parse().unwrap()),
        true, // §2.2 namespace creation open
        10,   // quota
        weft_core::FederationConfig::default(),
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
    assert_eq!(req.channel.as_str(), "#general");

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
            token: format!("vtok-{}-{}", req.channel, req.session),
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
        token,
        endpoint,
    } = &reply.event
    else {
        panic!("expected VOICE OFFER, got {reply:?}");
    };
    assert_eq!(channel.as_str(), "#lounge");
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

    // alice negotiates: her SDP offer gets the SFU's answer back as VOICE DESC.
    alice.send("@label=v2 VOICE DESC #lounge :v=0\\r\\nmy-offer");
    let reply = alice.recv().await;
    assert_eq!(reply.label.as_deref(), Some("v2"));
    let Event::VoiceDesc { sdp, .. } = &reply.event else {
        panic!("expected VOICE DESC answer, got {reply:?}");
    };
    assert_eq!(sdp, "answer-to:v=0\r\nmy-offer");

    // alice leaves → labeled VOICE STATE leave ack; bob sees the leave too.
    alice.send("@label=v3 VOICE LEAVE #lounge");
    let reply = alice.recv().await;
    assert_eq!(reply.label.as_deref(), Some("v3"));
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
