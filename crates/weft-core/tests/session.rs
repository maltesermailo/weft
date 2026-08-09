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
use weft_proto::{
    CallState, ChannelName, ErrCode, Event, FriendState, MemberAction, Reply, VoiceAction,
};
use weft_store::{AccountStore, ChannelStore, NamespaceStore, NetblockStore, PeerStore};

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

/// The verb of a wire line, past any `@tags` prefix (§4).
fn verb_of(raw: &str) -> &str {
    let rest = match raw.strip_prefix('@') {
        Some(tagged) => tagged.split_once(' ').map(|(_, rest)| rest).unwrap_or(""),
        None => raw,
    };

    rest.split(' ').next().unwrap_or_default()
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

    /// The next line from the server, verbatim.
    async fn recv_raw_any(&mut self) -> String {
        tokio::time::timeout(Duration::from_secs(5), self.from_server.recv())
            .await
            .expect("timed out waiting for a server line")
            .expect("server closed the stream")
    }

    /// The next line that is not a §6.1 presence flip.
    ///
    /// Every member's connect and disconnect broadcasts one, and a provider session
    /// is subscribed to all of it — so a test reading a *verb* off that stream would
    /// otherwise have to know which roster dots happened to move first. Tests about
    /// presence itself read `recv_raw_any` (or the typed `recv`, which is unfiltered).
    async fn recv_raw(&mut self) -> String {
        loop {
            let raw = self.recv_raw_any().await;

            if verb_of(&raw) != "PRESENCE" {
                return raw;
            }
        }
    }

    /// The next typed reply, skipping §6.1 presence flips — the same roster noise
    /// `recv_raw` filters, for the same reason. Tests *about* presence use
    /// [`Client::recv_any`].
    async fn recv(&mut self) -> Reply {
        loop {
            let reply = self.recv_any().await;

            if !matches!(reply.event, Event::Presence { .. }) {
                return reply;
            }
        }
    }

    async fn recv_any(&mut self) -> Reply {
        loop {
            let raw = self.recv_raw_any().await;
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

    /// Like [`Client::recv_raw_any`], but tolerant of a long wait — same reason as
    /// [`Client::recv_slow`]: under `start_paused` the short deadline would be the
    /// next timer to fire and would trip before the server timer under test.
    async fn recv_raw_slow(&mut self) -> String {
        tokio::time::timeout(Duration::from_secs(600), self.from_server.recv())
            .await
            .expect("timed out waiting for a server line")
            .expect("server closed the stream")
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

    /// v0.13 helper: create a `public` namespace with vanity `vanity`, returning
    /// its minted ULID id (the token every ns/channel/scope now addresses by).
    /// Consumes the `NS-META` reply.
    async fn create_ns(&mut self, vanity: &str) -> String {
        self.send(&format!(
            "@root={} NS CREATE {vanity} public",
            root_key_b64()
        ));
        match self.recv().await.event {
            Event::NsMeta { id, .. } => id.to_string(),
            other => panic!("expected NS-META creating {vanity}, got {other:?}"),
        }
    }

    /// v0.13 helper: create a channel with desired display name `vanity` in
    /// namespace `ns_id`, returning its canonical `#<ns-id>/<chan-id>` wire name
    /// (both segments minted ULIDs). Consumes the `POLICY` reply.
    async fn create_channel(&mut self, ns_id: &str, vanity: &str) -> ChannelName {
        self.send(&format!("CHANNEL CREATE #{ns_id}/{vanity}"));
        // A namespaced create ack leads with a CHANNEL-LAYOUT (carrying the vanity
        // so the creator's client shows it, not the ULID), then the POLICY.
        match self.recv().await.event {
            Event::ChannelLayout { .. } => {}
            other => panic!("expected CHANNEL-LAYOUT creating {vanity}, got {other:?}"),
        }
        match self.recv().await.event {
            Event::Policy { channel, .. } => channel,
            other => panic!("expected POLICY creating {vanity}, got {other:?}"),
        }
    }

    /// v0.13 helper: resolve a channel by its display `vanity` within namespace
    /// `ns_id` to its canonical `#<ns-id>/<chan-id>` wire name, by reading the
    /// `CHANNELS` layout. Used to grab the auto-seeded `general` channel that
    /// `NS CREATE` now provisions (rather than minting a second one, which would
    /// collide on the vanity). Call it while `general` is the only channel so the
    /// labeled layout response leaves nothing unread.
    async fn channel_by_vanity(&mut self, ns_id: &str, vanity: &str) -> ChannelName {
        self.send(&format!("@label=cbv CHANNELS {ns_id}"));
        loop {
            let reply = self.recv().await;
            if reply.label.as_deref() != Some("cbv") {
                continue;
            }
            match reply.event {
                Event::ChannelLayout {
                    channel, vanity: v, ..
                } if v == vanity => return channel,
                Event::ChannelLayout { .. } | Event::NsMeta { .. } => {}
                other => panic!("expected CHANNEL-LAYOUT for {vanity}, got {other:?}"),
            }
        }
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

/// A context with a §6.7 banned-word filter, for the register/ns-create tests.
fn ctx_banned(substrings: &[&str], regexes: &[&str]) -> Arc<ServerCtx> {
    let store = Arc::new(MemoryStore::default());
    let info = ServerInfo {
        network: "test.example".parse().unwrap(),
        motd: None,
        features: Vec::new(),
    };
    Arc::new(
        ServerCtx::new(
            info,
            std::iter::empty::<(weft_proto::ChannelName, RetentionPolicy)>(),
            Keypair::generate(),
            true,
            store,
            Arc::new(weft_core::MemBlobStore::default()),
            "permanent".parse().unwrap(),
            std::iter::empty::<weft_proto::Account>(),
            true,
            10,
            weft_core::FederationConfig::default(),
        )
        .with_banned_words(
            substrings.iter().map(|w| w.to_string()).collect(),
            regexes.iter().map(|w| w.to_string()).collect(),
        ),
    )
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

/// Drain an `NS JOIN` response (MEMBER/POLICY per visible channel) up to the
/// trailing `NS-MEMBER … join`, returning its `count=` (v0.12).
async fn drain_until_ns_member(client: &mut Client) -> Option<u64> {
    loop {
        match client.recv().await.event {
            Event::Member { .. } | Event::Policy { .. } => {}
            Event::NsMember {
                action: MemberAction::Join,
                count,
                ..
            } => return count,
            other => panic!("unexpected before NS-MEMBER: {other:?}"),
        }
    }
}

/// The `context` of an `ERR` reply — which specific rule refused.
fn err_context(reply: &Reply) -> Option<String> {
    match &reply.event {
        Event::Err(err) => err.context.clone(),
        other => panic!("not an ERR: {other:?}"),
    }
}

/// Send-and-collect a `MEMBERS` roster: skip anything before the batch, then
/// gather the member account names until `BATCH END`.
/// The next event, past the local-membership statement a provider is sent when it
/// registers or asserts a realm (the `ni…` BATCH — see `push_consumed_membership`).
/// A test asserting on the answer to its *own* command shouldn't have to know that
/// weftd also states the world on connect.
async fn recv_past_membership_statement(client: &mut Client) -> Reply {
    loop {
        let reply = client.recv().await;
        let skip = match &reply.event {
            Event::BatchStart { id } | Event::BatchEnd { id, .. } => id.starts_with("ni"),
            Event::NsMemberInfo { .. } => true,
            _ => false,
        };

        if !skip {
            return reply;
        }
    }
}

async fn roster_names(client: &mut Client) -> std::collections::HashSet<String> {
    loop {
        if matches!(client.recv().await.event, Event::BatchStart { .. }) {
            break;
        }
    }
    let mut names = std::collections::HashSet::new();
    loop {
        match client.recv_any().await.event {
            Event::Member { user, .. } => {
                names.insert(user.account.as_str().to_string());
            }
            Event::Presence { .. } => {}
            Event::BatchEnd { .. } => break,
            other => panic!("unexpected in roster batch: {other:?}"),
        }
    }
    names
}

/// Collect a `HISTORY` batch's message bodies, oldest first.
async fn history_bodies(client: &mut Client) -> Vec<String> {
    loop {
        if matches!(client.recv().await.event, Event::BatchStart { .. }) {
            break;
        }
    }
    let mut out = Vec::new();
    loop {
        match client.recv().await.event {
            Event::Message(m) => out.push(m.body.clone()),
            Event::BatchEnd { .. } => break,
            other => panic!("unexpected in history batch: {other:?}"),
        }
    }
    out
}

/// Send-and-collect a `GRANTS` batch → the `(subject, caps)` member overrides.
async fn grant_infos(client: &mut Client) -> Vec<(String, String)> {
    loop {
        if matches!(client.recv().await.event, Event::BatchStart { .. }) {
            break;
        }
    }
    let mut out = Vec::new();
    loop {
        match client.recv().await.event {
            Event::GrantInfo { subject, caps, .. } => {
                out.push((subject.as_str().to_string(), caps))
            }
            Event::BatchEnd { .. } => break,
            other => panic!("unexpected in grants batch: {other:?}"),
        }
    }
    out
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
    let reply = ada.recv_any().await;
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
    let Event::BatchEnd { id, truncated } = &end.event else {
        panic!("expected BATCH END, got {end:?}");
    };
    assert_eq!(id, &batch_id);
    // The wire form off the live path is always the materialized view (v0.12
    // Part 4.1) — no `compacted` flag to carry.
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
    assert_eq!(req.from.as_ref().unwrap().to_string(), "ada");
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

    // F authenticates the bridge and runs alice's CALL as `@as=alice` (§11.14).
    let mut bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    bridge.send("@as=alice;label=c CALL bob@test.example");

    // alice's own ringing state comes back as an ordinary event over the bridge.
    let raw = bridge.recv_raw().await;
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
    bridge.send(&with_as("alice", &inner));

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

    // Skip the GROUP-ROSTER that group creation fans out; find the GROUP CALL ring.
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
    bridge.send(&with_as("alice", &inner));

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
    // Find the GROUP ROSTER among the (GROUP-ROSTER, GROUP CALL ring, GROUP ROSTER)
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
    bridge.send(&with_as("alice", &inner));

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
    bridge.send(&with_as("carol", &inner));

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
    assert!(synced.line.contains("GROUP-ROSTER"), "{}", synced.line);
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
    // §11.14 the fan-out to a member network is a home-minted MESSAGE event.
    let relay = sink!();
    assert_eq!(relay.peer.as_str(), "peer.example");
    assert!(relay.from.is_none(), "an event, not an @as command"); // no attribution
    assert!(relay.line.contains("MESSAGE"), "{}", relay.line);
    assert!(relay.line.contains("msgid="), "{}", relay.line); // home-minted
    assert!(relay.line.contains("carol@test.example"), "{}", relay.line);
    assert!(relay.line.contains("hello"), "{}", relay.line);

    // A spoke relays alice's post to us (home) as an `@as` MSG → we mint + deliver.
    let bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    bridge.send(&format!("@as=alice MSG {gid} :hi from alice"));

    match carol.recv().await.event {
        Event::Message(m) => {
            assert_eq!(m.sender.to_string(), "alice@peer.example");
            assert_eq!(m.body, "hi from alice");
            assert_eq!(m.msgid.origin().as_str(), "test.example"); // minted by the home
        }
        e => panic!("expected alice's message, got {e:?}"),
    }
    // And it was fanned back out to peer as a home-minted MESSAGE event.
    let relay2 = sink!();
    assert!(relay2.line.contains("MESSAGE"), "{}", relay2.line);
    assert!(relay2.line.contains("msgid="), "{}", relay2.line);
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
                if d.line.contains("GROUP-ROSTER") {
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
    // An inbound GROUP-ROSTER reconciles membership; a removed local member is told
    // it left (its client drops the group).
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "peer.example", &peer_key.public());
    let mut carol = ready(&ctx, "carol").await;

    const G: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    macro_rules! sync {
        ($members:expr) => {{
            let line = weft_proto::Reply::new(weft_proto::Event::GroupRoster {
                group: G.parse().unwrap(),
                creator: "alice@peer.example".parse().unwrap(),
                name: None,
                members: $members,
            })
            .to_line()
            .unwrap()
            .serialize()
            .unwrap();
            bridge.send(&line);
        }};
    }

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
    bridge.send(&roster_line(
        G,
        "alice@peer.example",
        None,
        &["alice@peer.example", "carol@test.example"],
    ));
    assert!(matches!(carol.recv().await.event, Event::Group { .. }));

    // carol posts with a label → relayed to the home as an `@as` MSG carrying a
    // bridge label `B-…` (§11.14).
    carol.send(&format!("@label=post MSG &{G} :hello"));
    let relayed = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("relay")
        .expect("sink open");
    let parsed = weft_proto::Request::parse(&relayed.line).unwrap();
    let token = parsed.label.clone().expect("bridge label");
    let weft_proto::Command::Msg { target, .. } = parsed.command else {
        panic!("expected a spoke @as MSG relay, got {:?}", relayed.line);
    };
    assert_eq!(target.to_string(), format!("&{G}"));
    assert_eq!(relayed.from.as_ref().unwrap().to_string(), "carol");
    assert!(token.starts_with("B-peer.example-"), "{token}");

    // The home mints + fans it back to us as a MESSAGE event with the SAME label.
    bridge.send(&format!(
        "@msgid=peer.example/01ARZ3NDEKTSV4RRFFQ69G5FB0;label={token} MESSAGE &{G} carol@test.example :hello"
    ));

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
    bridge.send(&roster_line(
        G,
        "alice@peer.example",
        None,
        &["alice@peer.example", "carol@test.example"],
    ));
    assert!(matches!(carol.recv().await.event, Event::Group { .. }));

    // Viewing history triggers the catch-up request to the home.
    carol.send(&format!("HISTORY &{G}"));
    let req = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("backfill request")
        .expect("sink open");
    assert_eq!(req.peer.as_str(), "peer.example");
    let weft_proto::Command::History { target, after, .. } =
        weft_proto::Request::parse(&req.line).unwrap().command
    else {
        panic!("expected HISTORY, got {:?}", req.line);
    };
    assert_eq!(target.to_string(), format!("&{G}"));
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
    let _ = sink_line!("GROUP-ROSTER"); // membership propagation

    carol.send(&format!("MSG {gid} :first"));
    let m1 = match carol.recv().await.event {
        Event::Message(m) => m.msgid.to_string(),
        e => panic!("expected echo, got {e:?}"),
    };
    let _ = sink_line!("first"); // fanned out to peer
    carol.send(&format!("MSG {gid} :second"));
    assert!(matches!(carol.recv().await.event, Event::Message(_)));
    let _ = sink_line!("second");

    // The peer, catching bob up, asks for everything after the first message
    // as an `@as HISTORY &group after=<m1>`.
    let bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    let backfill = weft_proto::Request::new(weft_proto::Command::History {
        target: gid.parse().unwrap(),
        before: None,
        after: Some(m1.parse().unwrap()),
        limit: None,
        thread: None,
    })
    .serialize()
    .unwrap();
    bridge.send(&format!("@as=bob {backfill}"));

    // The home replays the second message (only) as a home-minted MESSAGE event.
    let replay = sink_line!("MESSAGE");
    assert_eq!(replay.peer.as_str(), "peer.example");
    assert!(replay.from.is_none(), "an event, not an @as command");
    let weft_proto::Event::Message(m) = weft_proto::Reply::parse(&replay.line).unwrap().event
    else {
        panic!("expected a replayed MESSAGE, got {:?}", replay.line);
    };
    assert_eq!(m.body, "second");
    assert_eq!(m.msgid.origin().as_str(), "test.example"); // home-minted
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
    bridge.send(&roster_line(
        G,
        "alice@peer.example",
        None,
        &["alice@peer.example", "carol@test.example"],
    ));
    assert!(matches!(carol.recv().await.event, Event::Group { .. }));

    // A home-minted MESSAGE event carrying an attachment hosted on a THIRD network.
    let relay = weft_proto::Reply::new(weft_proto::Event::Message(Box::new(
        weft_proto::MessageEvent {
            target: format!("&{G}").parse().unwrap(),
            sender: "alice@peer.example".parse().unwrap(),
            msgid: "peer.example/01ARZ3NDEKTSV4RRFFQ69G5FB0".parse().unwrap(),
            body: "look at this".to_string(),
            meta: weft_proto::MsgMeta {
                attachments: vec!["weft-media://media.example/deadbeef".to_string()],
                ..Default::default()
            },
            edited: None,
            edited_at: None,
        },
    )))
    .to_line()
    .unwrap()
    .serialize()
    .unwrap();
    bridge.send(&relay);

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

    // Edit → home applies (carol gets EDITED) + fans an EDITED event out to peer.
    carol.send(&format!("EDIT {mid} :fixed"));
    match carol.recv().await.event {
        Event::Edited { body, target, .. } => {
            assert_eq!(body, "fixed");
            assert_eq!(target.to_string(), gid);
        }
        e => panic!("expected EDITED echo, got {e:?}"),
    }
    let muts = sink_line!("EDITED");
    assert_eq!(muts.peer.as_str(), "peer.example");
    assert!(muts.from.is_none(), "an event, not an @as command");
    let weft_proto::Event::Edited {
        body,
        edit_of,
        target,
        ..
    } = weft_proto::Reply::parse(&muts.line).unwrap().event
    else {
        panic!("expected EDITED, got {:?}", muts.line);
    };
    assert_eq!(body, "fixed");
    assert_eq!(edit_of.to_string(), mid);
    assert_eq!(target.to_string(), gid);
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

    // Sync the group (home = peer, creator alice@peer) via a GROUP-ROSTER event.
    bridge.send(&roster_line(
        G,
        "alice@peer.example",
        None,
        &["alice@peer.example", "carol@test.example"],
    ));
    assert!(matches!(carol.recv().await.event, Event::Group { .. }));

    // Home minted a message authored by carol (relayed earlier) → ingest it as a
    // home-minted MESSAGE event.
    const MID: &str = "peer.example/01ARZ3NDEKTSV4RRFFQ69G5FB0";
    bridge.send(&format!(
        "@msgid={MID} MESSAGE &{G} carol@test.example :orig"
    ));
    assert!(matches!(carol.recv().await.event, Event::Message(_)));

    // Home minted an EDIT of it → ingest it as an EDITED event → carol sees EDITED.
    let edited = weft_proto::Reply::new(weft_proto::Event::Edited {
        target: format!("&{G}").parse().unwrap(),
        user: "carol@test.example".parse().unwrap(),
        msgid: "peer.example/01ARZ3NDEKTSV4RRFFQ69G5FB4".parse().unwrap(),
        edit_of: MID.parse().unwrap(),
        body: "home-fixed".to_string(),
    })
    .to_line()
    .unwrap()
    .serialize()
    .unwrap();
    bridge.send(&edited);
    match carol.recv().await.event {
        Event::Edited { body, .. } => assert_eq!(body, "home-fixed"),
        e => panic!("expected ingested EDITED, got {e:?}"),
    }

    // carol (the author) edits it herself → we relay to the home as an `@as EDIT`.
    carol.send(&format!("EDIT {MID} :carol-fixed"));
    let relayed = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("relay")
        .expect("sink open");
    assert_eq!(relayed.peer.as_str(), "peer.example");
    assert_eq!(relayed.from.as_ref().unwrap().to_string(), "carol");
    let weft_proto::Command::Edit { msgid, body } =
        weft_proto::Request::parse(&relayed.line).unwrap().command
    else {
        panic!("expected @as EDIT relay, got {:?}", relayed.line);
    };
    assert_eq!(msgid.to_string(), MID);
    assert_eq!(body, "carol-fixed");
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
    let bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    bridge.send(&roster_line(
        G,
        "alice@peer.example",
        None,
        &["alice@peer.example", "carol@test.example"],
    ));

    // carol is told the group exists.
    match carol.recv().await.event {
        Event::Group { id, members, .. } => {
            assert_eq!(id.to_string(), format!("&{G}"));
            assert_eq!(members.len(), 2);
        }
        e => panic!("expected GROUP, got {e:?}"),
    }

    // peer (home) sends a minted MESSAGE event (a threaded reply) → we ingest +
    // deliver to carol, meta intact.
    let relay = weft_proto::Reply::new(weft_proto::Event::Message(Box::new(
        weft_proto::MessageEvent {
            target: format!("&{G}").parse().unwrap(),
            sender: "alice@peer.example".parse().unwrap(),
            msgid: "peer.example/01ARZ3NDEKTSV4RRFFQ69G5FB0".parse().unwrap(),
            body: "minted upstream".to_string(),
            meta: weft_proto::MsgMeta {
                reply_to: Some("peer.example/01ARZ3NDEKTSV4RRFFQ69G5FB2".parse().unwrap()),
                thread: Some("peer.example/01ARZ3NDEKTSV4RRFFQ69G5FB2".parse().unwrap()),
                ..Default::default()
            },
            edited: None,
            edited_at: None,
        },
    )))
    .to_line()
    .unwrap()
    .serialize()
    .unwrap();
    bridge.send(&relay);

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
    assert_eq!(req.from.as_ref().unwrap().to_string(), "ada");
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
    let ns_id = ada.create_ns("gaming").await;

    // Owner adds two emoji.
    ada.send(&format!(
        "EMOJI ADD {ns_id} partyblob weft-media://test.example/aaa"
    ));
    assert!(matches!(&ada.recv().await.event, Event::Emoji { name, .. } if name == "partyblob"));
    ada.send(&format!(
        "EMOJI ADD {ns_id} catjam weft-media://test.example/bbb"
    ));
    assert!(matches!(ada.recv().await.event, Event::Emoji { .. }));

    // List → a BATCH of both.
    ada.send(&format!("@label=el EMOJI LIST {ns_id}"));
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
    ada.send(&format!("EMOJI REMOVE {ns_id} catjam"));
    assert!(matches!(ada.recv().await.event, Event::EmojiRemoved { .. }));

    // An invalid shortcode is rejected regardless of authority.
    ada.send(&format!("EMOJI ADD {ns_id} bad-name! weft-media://x/y"));
    ada.expect_err(ErrCode::Policy).await;

    // A non-admin can't add (ns-admin gate).
    let mut bob = joined(&ctx, "bob", "#general").await;
    bob.send(&format!("EMOJI ADD {ns_id} sneaky weft-media://x/y"));
    bob.expect_err(ErrCode::CapRequired).await;
}

#[tokio::test]
async fn presence_relays_to_co_members_but_never_invisible() {
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    let mut bob = joined(&ctx, "bob", "#general").await;
    ada.recv().await; // bob's join broadcast

    bob.send("PRESENCE away");
    let reply = ada.recv_any().await;
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

    // Now ada can create. The server mints the channel's canonical `#<chan-id>`
    // wire name (v0.13); "ada-chan" is just the desired vanity.
    ada.send("@label=c2 CHANNEL CREATE #ada-chan retained:30d");
    let reply = ada.recv().await;
    assert_eq!(reply.label.as_deref(), Some("c2"));
    let Event::Policy { channel, policy } = &reply.event else {
        panic!("expected POLICY, got {reply:?}");
    };
    assert_eq!(policy.to_string(), "retained:30d");
    let ada_chan = channel.clone();
    // And join the channel she made.
    ada.send(&format!("JOIN {ada_chan}"));
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

    // Operator creates and reconfigures a channel. The server mints its
    // canonical `#<chan-id>` (v0.13) — capture it for the later verbs.
    boss.send("CHANNEL CREATE #ops");
    let Event::Policy { channel: ops, .. } = boss.recv().await.event else {
        panic!("expected POLICY");
    };
    boss.send(&format!("@label=p1 CHANNEL POLICY {ops} ephemeral"));
    let reply = boss.recv().await;
    assert_eq!(reply.label.as_deref(), Some("p1"));
    assert!(
        matches!(&reply.event, Event::Policy { policy, .. } if policy.to_string() == "ephemeral")
    );

    // META view-gated.
    boss.send(&format!("@label=m1 CHANNEL META {ops} view-gated :yes"));
    let reply = boss.recv().await;
    assert!(matches!(&reply.event, Event::Chanmeta { key, .. } if key == "view-gated"));

    // DELETE requires the confirmation to match.
    boss.send(&format!("CHANNEL DELETE {ops} #wrong"));
    boss.expect_err(ErrCode::Policy).await;
    boss.send(&format!("@label=d1 CHANNEL DELETE {ops} {ops}"));
    let reply = boss.recv().await;
    assert!(matches!(&reply.event, Event::Chanmeta { key, .. } if key == "deleted"));
    // Gone: joining now is NO-SUCH-TARGET.
    boss.send(&format!("JOIN {ops}"));
    boss.expect_err(ErrCode::NoSuchTarget).await;
}

#[tokio::test]
async fn view_gated_channel_hides_without_the_view_cap() {
    let ctx = ctx_ops(&["#general"], &["boss"]);
    let mut boss = ready_op(&ctx, "boss").await;
    boss.send("CHANNEL CREATE #secret");
    let Event::Policy {
        channel: secret, ..
    } = boss.recv().await.event
    else {
        panic!("expected POLICY");
    };
    boss.send(&format!("CHANNEL META {secret} view-gated :yes"));
    boss.recv().await;

    // A plain account can't even tell it exists (invariant 1).
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@label=j JOIN {secret}"));
    let reply = ada.expect_err(ErrCode::NoSuchTarget).await;
    assert_eq!(reply.label.as_deref(), Some("j"));

    // Grant view → it becomes reachable.
    boss.send(&format!("GRANT ada {secret} view"));
    boss.recv().await;
    ada.send(&format!("JOIN {secret}"));
    assert!(matches!(ada.recv().await.event, Event::Member { .. }));
}

#[tokio::test]
async fn invite_mint_and_redeem_grants_membership() {
    let ctx = ctx_ops(&["#general"], &["boss"]);
    let mut boss = ready_op(&ctx, "boss").await;
    boss.send("CHANNEL CREATE #club");
    let Event::Policy { channel: club, .. } = boss.recv().await.event else {
        panic!("expected POLICY");
    };
    boss.send(&format!("CHANNEL META {club} view-gated :yes"));
    boss.recv().await;

    // Mint a 1-use invite for the gated channel.
    boss.send(&format!("@label=i1 INVITE MINT {club} max-uses=1"));
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
    ada.send(&format!("JOIN {club}"));
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
    let ns_id = ada.create_ns("gaming").await;

    // A namespace-scoped invite's link carries the namespace (§11.10), so a
    // foreign redeemer can auto-federate to it. The scope is `ns:<id>`, so the
    // link carries the ns id.
    ada.send(&format!("INVITE MINT ns:{ns_id}"));
    let Event::Invited { link, .. } = ada.recv().await.event else {
        panic!("expected INVITED");
    };
    let link = link.expect("a namespace invite should carry a link");
    assert!(
        link.starts_with(&format!("weft://test.example/{ns_id}/i/")),
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

/// §11.14 attribute a serialized command line to a foreign account (`@as=<acct>`,
/// merged into the tag group), as the dialer does on a real bridge.
fn with_as(account: &str, line: &str) -> String {
    let mut l = weft_proto::Line::parse(line).unwrap();
    l.tags.insert("as".to_string(), account.to_string());
    l.serialize().unwrap()
}

/// §11.12 a serialized `GROUP-ROSTER` event line — the down-leg the home fans out
/// to a member network to keep the group's membership authoritative.
fn roster_line(group: &str, creator: &str, name: Option<&str>, members: &[&str]) -> String {
    weft_proto::Reply::new(weft_proto::Event::GroupRoster {
        group: group.parse().unwrap(),
        creator: creator.parse().unwrap(),
        name: name.map(str::to_string),
        members: members.iter().map(|m| m.parse().unwrap()).collect(),
    })
    .to_line()
    .unwrap()
    .serialize()
    .unwrap()
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
        id,
        vanity,
        visibility,
        owner,
        ..
    } = &reply.event
    else {
        panic!("expected NS-META, got {reply:?}");
    };
    let ns_id = id.to_string();
    assert_eq!(vanity.as_str(), "gaming");
    assert_eq!(visibility.to_string(), "public");
    assert_eq!(owner.as_deref(), Some("ada"));

    // As owner, ada holds every cap in her namespace — she can create a
    // namespaced channel (deferred in M4a, unlocked by ownership). The wire name
    // is the minted `#<ns-id>/<chan-id>`; she sent the desired vanity "chat"
    // ("general" is already taken by the channel NS CREATE auto-seeds).
    ada.send(&format!("@label=c1 CHANNEL CREATE #{ns_id}/chat"));
    assert!(matches!(
        ada.recv().await.event,
        Event::ChannelLayout { .. }
    )); // vanity
    let reply = ada.recv().await;
    assert!(
        matches!(&reply.event, Event::Policy { channel, .. } if channel.namespace() == Some(ns_id.as_str())),
        "owner should create channels in her ns, got {reply:?}"
    );

    // ...and delegate ns caps to someone else (who must exist — caps key by the
    // target's ULID, §10.4). Delegation addresses the namespace by id.
    let _bob = ready(&ctx, "bob").await;
    ada.send(&format!("@label=d1 NS DELEGATE {ns_id} bob ban,kick"));
    assert!(matches!(ada.recv().await.event, Event::Token { .. }));

    // A non-owner cannot create channels in the namespace.
    let mut eve = ready(&ctx, "eve").await;
    eve.send(&format!("CHANNEL CREATE #{ns_id}/secret"));
    eve.expect_err(ErrCode::CapRequired).await;
}

#[tokio::test]
async fn ns_create_records_membership_so_it_survives_a_reconnect() {
    // Regression: creating a namespace must persist the creator's membership, so a
    // fresh login (SYNC — the reconnect path) restores it to the rail. Previously
    // the creator wasn't recorded as a member and the server only reappeared after
    // opening Discover.
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    let ns_id = ada.create_ns("gaming").await;

    // Simulate the reconnect: a fresh SYNC skeleton must report the membership
    // (NS-MEMBER) and its auto-seeded #general (CHANNEL-LAYOUT).
    ada.send("@label=s SYNC preview=0");
    let mut saw_member = false;
    let mut saw_general = false;
    loop {
        match ada.recv().await.event {
            Event::NsMember {
                namespace,
                action: MemberAction::Join,
                ..
            } if namespace.to_string() == ns_id => saw_member = true,
            Event::ChannelLayout { vanity, .. } if vanity == "general" => saw_general = true,
            Event::SyncEnd { .. } => break,
            _ => {}
        }
    }
    assert!(
        saw_member,
        "SYNC restores the created namespace's membership"
    );
    assert!(saw_general, "…and its seeded #general channel");
}

#[tokio::test]
async fn ns_create_is_blocked_by_an_admin_vanity_lock() {
    // §2.3: an operator reserves a vanity in the web admin panel (store-direct);
    // NS CREATE of that name is then refused until the lock is lifted.
    let (ctx, store) = ctx_full_store(&[], true, &[]);
    let mut ada = ready(&ctx, "ada").await;
    let vanity: weft_proto::NamespaceName = "reserved".parse().unwrap();
    assert!(store.set_vanity_locked(&vanity, true).await.unwrap());

    let root = root_key_b64();
    ada.send(&format!("@root={root} NS CREATE reserved public"));
    ada.expect_err(ErrCode::Conflict).await;

    // Lifting the reservation lets the name be registered.
    assert!(store.set_vanity_locked(&vanity, false).await.unwrap());
    ada.send(&format!("@root={root} NS CREATE reserved public"));
    assert!(matches!(ada.recv().await.event, Event::NsMeta { .. }));
}

#[tokio::test]
async fn ns_create_rejects_a_ulid_shaped_vanity() {
    // §2.3: a vanity can't masquerade as an id. NS JOIN resolves ids first, so a
    // ULID-shaped vanity could never be reached by name AND could impersonate
    // another server's id in a link/UI — refuse it at creation. This keeps the
    // id-space and vanity-space provably disjoint.
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    let root = root_key_b64();
    // A well-formed lowercase ULID as the vanity is refused…
    ada.send(&format!(
        "@root={root} NS CREATE 01arz3ndektsv4rrffq69g5fav public"
    ));
    ada.expect_err(ErrCode::Policy).await;
    // …while an ordinary human vanity of the same length is fine (not a ULID).
    ada.send(&format!(
        "@root={root} NS CREATE my-cool-gaming-server-name public"
    ));
    assert!(matches!(ada.recv().await.event, Event::NsMeta { .. }));
}

#[tokio::test]
async fn banned_words_block_usernames_and_ns_vanities() {
    // §6.7 both filter categories apply to new usernames + namespace vanities:
    // `words_substring` (case-insensitive substring) and `words_regex` (case-
    // insensitive regex). Clean names pass.
    let ctx = ctx_banned(&["admin"], &[r"^mod[-_]?\d+$", r"gr[i1]ef"]);

    let mut c = connect(&ctx);
    c.send("HELLO weft/1");
    assert!(matches!(c.recv().await.event, Event::Welcome { .. }));
    // Substring: "superADMIN" contains "admin".
    c.send(&format!("@label=r REGISTER superADMIN :{PASSWORD}"));
    c.expect_err(ErrCode::Policy).await;
    // Regex: "mod-7" matches `^mod[-_]?\d+$`.
    c.send(&format!("@label=r2 REGISTER mod-7 :{PASSWORD}"));
    c.expect_err(ErrCode::Policy).await;
    // Regex with leetspeak class: "gr1efer" matches `gr[i1]ef`.
    c.send(&format!("@label=r3 REGISTER gr1efer :{PASSWORD}"));
    c.expect_err(ErrCode::Policy).await;
    // A clean name registers.
    c.send(&format!("REGISTER alice :{PASSWORD}"));
    assert!(matches!(c.recv().await.event, Event::Welcome { .. }));

    // NS CREATE honors the same filter.
    let root = root_key_b64();
    c.send(&format!("@root={root} NS CREATE mod_42 public"));
    c.expect_err(ErrCode::Policy).await;
    c.send(&format!("@root={root} NS CREATE friendly-place public"));
    assert!(matches!(c.recv().await.event, Event::NsMeta { .. }));
}

#[tokio::test]
async fn ns_delete_cascades_channels() {
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    let root = root_key_b64();
    ada.send(&format!("@root={root} NS CREATE gaming public"));
    let Event::NsMeta { id, .. } = ada.recv().await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();
    ada.send(&format!("@label=c1 CHANNEL CREATE #{ns_id}/chat"));
    assert!(matches!(
        ada.recv().await.event,
        Event::ChannelLayout { .. }
    )); // vanity
    let Event::Policy { channel, .. } = ada.recv().await.event else {
        panic!("expected POLICY");
    };
    assert_eq!(channel.namespace(), Some(ns_id.as_str()));

    assert!(
        ctx.registry.exists(&channel),
        "channel should be live after create"
    );

    // Deleting the namespace must tear its channels down with it. Leaving the
    // actor + store row orphaned is the bug: the channel stays live (writable)
    // and advertised, and a same-name namespace later inherits the ghost.
    ada.send(&format!("@label=del NS DELETE {ns_id} {ns_id}"));
    let reply = ada.recv().await;
    assert!(
        matches!(&reply.event, Event::NsMeta { description, .. } if description.as_deref() == Some("deleted")),
        "expected NS-META deleted, got {reply:?}"
    );

    assert!(
        !ctx.registry.exists(&channel),
        "NS DELETE must cascade-remove its channels (actor still live = still writable)"
    );
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
    let Event::NsMeta { id, .. } = ada.recv().await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();

    ada.send(&format!(
        "@label=m1 NS META {ns_id} title :The Gaming Lounge"
    ));
    let reply = ada.recv().await;
    assert!(
        matches!(&reply.event, Event::NsMeta { title: Some(t), .. } if t == "The Gaming Lounge")
    );
    ada.send(&format!("@label=v1 NS VISIBILITY {ns_id} public"));
    assert!(
        matches!(&ada.recv().await.event, Event::NsMeta { visibility, .. } if visibility.to_string() == "public")
    );

    // A non-owner can't administer it.
    let mut eve = ready(&ctx, "eve").await;
    eve.send(&format!("NS META {ns_id} title :hijacked"));
    eve.expect_err(ErrCode::CapRequired).await;
    // ...and a nonexistent namespace is NO-SUCH-TARGET.
    let ghost = weft_proto::Ulid::new().to_string().to_ascii_lowercase();
    eve.send(&format!("NS META {ghost} title :x"));
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
                vanity, visibility, ..
            } => {
                assert_eq!(visibility.to_string(), "public");
                seen.push(vanity.to_string());
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
    let Event::NsMeta { id, .. } = ada.recv().await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();
    // `general` is auto-seeded by NS CREATE; grab its canonical handle rather than
    // minting a second one. Each CHANNEL CREATE mints a canonical
    // `#<ns-id>/<chan-id>`; keep the handles.
    let general = ada.channel_by_vanity(&ns_id, "general").await;
    let random = ada.create_channel(&ns_id, "random").await;
    let voice = ada.create_channel(&ns_id, "voice").await;
    // Categorize + order: general/random under "text", voice uncategorized.
    ada.send(&format!("CHANNEL META {general} category :text"));
    assert!(matches!(&ada.recv().await.event, Event::Chanmeta { key, .. } if key == "category"));
    ada.send(&format!("CHANNEL META {general} position :0"));
    ada.recv().await;
    ada.send(&format!("CHANNEL META {random} category :text"));
    ada.recv().await;
    ada.send(&format!("CHANNEL META {random} position :1"));
    ada.recv().await;
    // Reorder: move random ahead of general.
    ada.send(&format!("CHANNEL META {random} position :-1"));
    ada.recv().await;

    // Read the layout back, ordered.
    ada.send(&format!("@label=cl CHANNELS {ns_id}"));
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
    assert_eq!(layout[0].0, voice.to_string());
    assert_eq!(
        layout[1],
        (random.to_string(), Some("text".to_string()), -1)
    );
    assert_eq!(
        layout[2],
        (general.to_string(), Some("text".to_string()), 0)
    );

    // Non-owner can set neither (needs pin cap in the ns).
    let mut eve = ready(&ctx, "eve").await;
    eve.send(&format!("CHANNEL META {general} category :hijack"));
    eve.expect_err(ErrCode::CapRequired).await;
    // ...but can read a public/unlisted namespace's layout (NS-META, then layout).
    eve.send(&format!("CHANNELS {ns_id}"));
    assert!(matches!(eve.recv().await.event, Event::NsMeta { .. }));
    assert!(matches!(
        eve.recv().await.event,
        Event::ChannelLayout { .. }
    ));
}

// ---- M4c: namespace recovery ladder (§2.4, invariant 9) ----

/// Create a namespace owned by `owner`, returning its root Keypair (held
/// client-side) so tests can sign transfer/recovery/cancel statements.
async fn make_namespace(
    ctx: &Arc<ServerCtx>,
    owner: &str,
    name: &str,
) -> (Client, Keypair, String) {
    let root = Keypair::generate();
    let mut client = ready(ctx, owner).await;
    client.send(
        &format!("root={} NS CREATE {name} unlisted", root.public().to_b64())
            .replace("root=", "@root="),
    );
    let Event::NsMeta { id, .. } = client.recv().await.event else {
        panic!("expected NS-META");
    };
    // The rotation/transfer/cancel signatures still cover the vanity `name`
    // (the server verifies against `record.name`); commands address by this id.
    (client, root, id.to_string())
}

#[tokio::test]
async fn ns_transfer_is_root_signed_rung_one() {
    let ctx = ctx(&[]);
    let (mut ada, root, ns_id) = make_namespace(&ctx, "ada", "gaming").await;

    // A forged signature is FORBIDDEN.
    ada.send(&format!("@sig=Zm9yZ2Vk NS TRANSFER {ns_id} bob"));
    let reply = ada.expect_err(ErrCode::Forbidden).await;
    let Event::Err(err) = &reply.event else {
        unreachable!()
    };
    assert_eq!(err.context.as_deref(), Some("signature"));

    // A real root signature transfers ownership immediately (no delay). The
    // signature still covers the vanity name; the command addresses by id.
    let sig = weft_crypto::sign_transfer(&root, "gaming", "bob");
    ada.send(&format!(
        "@sig={} NS TRANSFER {ns_id} bob",
        weft_crypto::signature_to_b64(&sig)
    ));
    let reply = ada.recv().await;
    assert!(matches!(&reply.event, Event::NsMeta { owner: Some(o), .. } if o == "bob"));

    // Bob is now the owner: he can administer, ada can't.
    let mut bob = ready(&ctx, "bob").await;
    bob.send(&format!("NS META {ns_id} title :Bob's Lounge"));
    assert!(matches!(bob.recv().await.event, Event::NsMeta { .. }));
    ada.send(&format!("NS META {ns_id} title :ada's"));
    ada.expect_err(ErrCode::CapRequired).await;
}

#[tokio::test]
async fn recovery_rung_two_quorum_then_cancel() {
    let ctx = ctx(&[]);
    let (mut ada, root, ns_id) = make_namespace(&ctx, "ada", "gaming").await;
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
    ada.send(&format!("NS RECOVERY SET {ns_id} 2 {keys}"));
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
    ada.send(&format!("NS RECOVER {ns_id} {}", signed.to_b64()));
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
    ada.send(&format!("NS RECOVER {ns_id} {}", signed.to_b64()));
    ada.expect_err(ErrCode::Conflict).await;

    // The live root cancels it (a live root always wins, §2.4). The cancel
    // signature covers the vanity name; the command addresses by id.
    let cancel = weft_crypto::sign_cancel(&root, "gaming");
    ada.send(&format!(
        "@sig={} NS RECOVERY CANCEL {ns_id}",
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
    let (mut ada, _root, ns_id) = make_namespace(&ctx, "ada", "gaming").await;
    let (q1, q2) = (Keypair::generate(), Keypair::generate());
    ada.send(&format!(
        "NS RECOVERY SET {ns_id} 2 {},{}",
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
    ada.send(&format!("NS RECOVER {ns_id} {}", under.to_b64()));
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
    ada.send(&format!("NS RECOVER {ns_id} {}", wrong_signed.to_b64()));
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
    let Event::NsMeta { id, .. } = ada.recv().await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();
    let q1 = Keypair::generate();
    ada.send(&format!(
        "NS RECOVERY SET {ns_id} 1 {}",
        q1.public().to_b64()
    ));
    ada.recv().await;

    let new_root = Keypair::generate();
    // The rotation record still names the vanity (server verifies against it).
    let record = weft_crypto::RotationRecord {
        namespace: "gaming".into(),
        new_root_key: new_root.public(),
        new_owner: "carol".into(),
    };
    let signed = weft_crypto::SignedRotation {
        record: record.clone(),
        signatures: vec![record.sign(&q1)],
    };
    ada.send(&format!("NS RECOVER {ns_id} {}", signed.to_b64()));
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
    let Event::NsMeta { id, .. } = ada.recv().await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();

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
    ada.send(&format!("@label=r NS RECOVER {ns_id} {}", signed.to_b64()));
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
    let Event::NsMeta { id, .. } = ada.recv().await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();

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
    ada.send(&format!("@label=x NS RECOVER {ns_id} {}", signed.to_b64()));
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

// ---- foreign-bridge framework (§3): adapter auth + REALM context ----

/// A ctx pinning one foreign-bridge adapter key, authorized for `scheme`
/// (`[[foreign_bridge]]`).
fn ctx_adapter(scheme: &str, adapter_key: &weft_core::PublicKey) -> Arc<ServerCtx> {
    let info = ServerInfo {
        network: "test.example".parse().unwrap(),
        motd: None,
        features: Vec::new(),
    };
    let chans: Vec<(&str, &str)> = Vec::new();
    Arc::new(
        ServerCtx::new(
            info,
            chans
                .iter()
                .map(|(c, p)| (c.parse().unwrap(), p.parse::<RetentionPolicy>().unwrap())),
            Keypair::generate(),
            true,
            Arc::new(MemoryStore::default()),
            Arc::new(weft_core::MemBlobStore::default()),
            "permanent".parse().unwrap(),
            std::iter::empty::<weft_proto::Account>(),
            true,
            10,
            weft_core::FederationConfig::default(),
        )
        .with_foreign_adapters(vec![(scheme.parse().unwrap(), *adapter_key)]),
    )
}

/// Drive a session to `State::ForeignBridge`, proving control of the adapter
/// `key`; consumes the WELCOME.
async fn adapter_session(ctx: &Arc<ServerCtx>, key: &Keypair) -> Client {
    let mut c = connect(ctx);
    c.send("HELLO weft/1");
    assert!(matches!(c.recv().await.event, Event::Welcome { .. }));

    c.send(&format!("AUTH ADAPTER {}", key.public().to_b64()));
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

#[tokio::test]
async fn foreign_bridge_adapter_authenticates() {
    let key = Keypair::generate();
    let ctx = ctx_adapter("matrix", &key.public());

    let mut c = connect(&ctx);
    c.send("HELLO weft/1");
    assert!(matches!(c.recv().await.event, Event::Welcome { .. }));

    c.send(&format!("AUTH ADAPTER {}", key.public().to_b64()));
    let Event::Challenge { nonce } = c.recv().await.event else {
        panic!("expected CHALLENGE");
    };
    let nonce = weft_crypto::b64::decode(&nonce).unwrap();
    let sig = weft_crypto::sign_challenge(&key, &nonce, "test.example");
    c.send(&format!(
        "AUTH PROOF {}",
        weft_crypto::signature_to_b64(&sig)
    ));

    let Event::Welcome { features, .. } = c.recv().await.event else {
        panic!("expected WELCOME after adapter PROOF");
    };
    // Unified provider session (plugin-spec §18): a bridge adapter is a provider.
    assert!(features.iter().any(|f| f == "plugin"));
}

#[tokio::test]
async fn foreign_bridge_unpinned_key_auth_fails() {
    let pinned = Keypair::generate();
    let ctx = ctx_adapter("matrix", &pinned.public());

    // A different key, not pinned in `[[foreign_bridge]]`, gets the uniform
    // AUTH-FAILED — no adapter-existence oracle (invariant 1 discipline).
    let stranger = Keypair::generate();
    let mut c = connect(&ctx);
    c.send("HELLO weft/1");
    assert!(matches!(c.recv().await.event, Event::Welcome { .. }));

    c.send(&format!("AUTH ADAPTER {}", stranger.public().to_b64()));
    c.expect_err(ErrCode::AuthFailed).await;
}

// ---- plugin system (plugin-spec.md §12): remote plugin register + invoke ----

/// A ctx pinning one remote plugin (`[[plugin.remote]]`) by id + key, open to
/// client registration.
fn ctx_plugin_schemes(
    plugin_id: &str,
    plugin_key: &weft_core::PublicKey,
    schemes: Vec<weft_proto::Scheme>,
) -> Arc<ServerCtx> {
    let info = ServerInfo {
        network: "test.example".parse().unwrap(),
        motd: None,
        features: Vec::new(),
    };
    let chans: Vec<(&str, &str)> = Vec::new();
    Arc::new(
        ServerCtx::new(
            info,
            chans
                .iter()
                .map(|(c, p)| (c.parse().unwrap(), p.parse::<RetentionPolicy>().unwrap())),
            Keypair::generate(),
            true,
            Arc::new(MemoryStore::default()),
            Arc::new(weft_core::MemBlobStore::default()),
            "permanent".parse().unwrap(),
            std::iter::empty::<weft_proto::Account>(),
            true,
            10,
            weft_core::FederationConfig::default(),
        )
        .with_remote_plugins(vec![(plugin_id.to_string(), *plugin_key, schemes)]),
    )
}

/// A ctx pinning one remote plugin with no scheme authorization.
fn ctx_plugin(plugin_id: &str, plugin_key: &weft_core::PublicKey) -> Arc<ServerCtx> {
    ctx_plugin_schemes(plugin_id, plugin_key, Vec::new())
}

/// Drive a session to `State::PluginService`, proving control of the plugin `key`;
/// consumes the WELCOME.
async fn plugin_session(ctx: &Arc<ServerCtx>, key: &Keypair) -> Client {
    let mut c = connect(ctx);
    c.send("HELLO weft/1");
    assert!(matches!(c.recv().await.event, Event::Welcome { .. }));

    c.send(&format!("AUTH ADAPTER {}", key.public().to_b64()));
    let Event::Challenge { nonce } = c.recv().await.event else {
        panic!("expected CHALLENGE");
    };
    let nonce = weft_crypto::b64::decode(&nonce).unwrap();
    let sig = weft_crypto::sign_challenge(key, &nonce, "test.example");
    c.send(&format!(
        "AUTH PROOF {}",
        weft_crypto::signature_to_b64(&sig)
    ));

    let Event::Welcome { features, .. } = c.recv().await.event else {
        panic!("expected WELCOME after plugin PROOF");
    };
    assert!(features.iter().any(|f| f == "plugin"));
    c
}

/// Poll the catalog until `plugin/action` appears (the register is on a *different*
/// session than the querying client, so this is the cross-session barrier).
async fn wait_for_action(client: &mut Client, plugin: &str, action: &str) {
    for _ in 0..50 {
        client.send("PLUGINS");
        let Event::PluginManifest { catalog } = client.recv().await.event else {
            panic!("expected PLUGIN-MANIFEST");
        };
        let cat: weft_proto::Catalog = weft_proto::plugin_from_b64(&catalog).unwrap();
        if cat
            .plugins
            .iter()
            .any(|p| p.plugin_id == plugin && p.actions.iter().any(|a| a.id == action))
        {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("plugin '{plugin}' action '{action}' never registered");
}

#[tokio::test]
async fn plugin_register_and_invoke() {
    let key = Keypair::generate();
    let ctx = ctx_plugin("modq", &key.public());

    // The plugin authenticates and registers a `global` action.
    let mut plugin = plugin_session(&ctx, &key).await;
    let reg = weft_proto::Registration {
        api: 1,
        id: "modq".into(),
        name: "Mod Queue".into(),
        icon: None,
        actions: vec![weft_proto::ActionDecl {
            id: "open".into(),
            label: "Open".into(),
            icon: None,
            surface: weft_proto::Surface::Global,
            context: weft_proto::ContextType::None,
            description: None,
            visibility: None,
            input: vec![],
        }],
        hooks: vec![],
        bot: None,
        schemes: vec![],
    };
    plugin.send(&format!(
        "PLUGIN-REGISTER :{}",
        weft_proto::plugin_to_b64(&reg).unwrap()
    ));

    // A client sees the action in the catalog (barrier: register is processed).
    let mut client = ready(&ctx, "ada").await;
    wait_for_action(&mut client, "modq", "open").await;

    // The client invokes it; weftd routes the invoke to the plugin's session.
    client.send("@label=i1 PLUGIN INVOKE modq open");
    let routed = plugin.recv_raw().await;
    let req = weft_proto::Request::parse(&routed).expect("routed invoke parses");
    let weft_proto::Command::PluginInvoke { action, .. } = req.command else {
        panic!("plugin expected a routed PLUGIN INVOKE, got {routed}");
    };
    assert_eq!(action, "open");
    let view_id = req.label.expect("invoke carries the view-id as its label");

    // The plugin returns a terminal toast; weftd relays it to the client with the
    // client's original label.
    let result = weft_proto::plugin_to_b64(&weft_proto::ViewResult::Toast {
        kind: weft_proto::ToastKind::Ok,
        text: "done".into(),
    })
    .unwrap();
    plugin.send(&format!("PLUGIN-RESULT {view_id} :{result}"));

    let reply = client.recv().await;
    assert_eq!(reply.label.as_deref(), Some("i1"));
    let Event::PluginResult {
        view_id: vid,
        result: got,
    } = reply.event
    else {
        panic!("client expected the relayed PLUGIN-RESULT, got {reply:?}");
    };
    assert_eq!(vid, view_id);
    let decoded: weft_proto::ViewResult = weft_proto::plugin_from_b64(&got).unwrap();
    assert!(matches!(decoded, weft_proto::ViewResult::Toast { .. }));
}

#[tokio::test]
async fn plugin_scheme_registration_routes_provision() {
    // §18 capability 6 (the "Instagram bridge is just a plugin" case): a remote
    // plugin declares `schemes` in its Registration; a client's NS JOIN for that
    // scheme routes a PROVISION push to the plugin's session.
    let key = Keypair::generate();
    let ctx = ctx_plugin_schemes("insta", &key.public(), vec!["instagram".parse().unwrap()]);

    let mut plugin = plugin_session(&ctx, &key).await;
    let reg = weft_proto::Registration {
        api: 1,
        id: "insta".into(),
        name: "Instagram Bridge".into(),
        icon: None,
        actions: vec![],
        hooks: vec![],
        bot: None,
        schemes: vec!["instagram".into()],
    };
    plugin.send(&format!(
        "PLUGIN-REGISTER :{}",
        weft_proto::plugin_to_b64(&reg).unwrap()
    ));

    // Barrier: poll until the registration landed (it's on another session).
    let mut client = ready(&ctx, "ada").await;
    for _ in 0..50 {
        client.send("PLUGINS");
        let Event::PluginManifest { catalog } = client.recv().await.event else {
            panic!("expected PLUGIN-MANIFEST");
        };
        let cat: weft_proto::Catalog = weft_proto::plugin_from_b64(&catalog).unwrap();
        if cat.plugins.iter().any(|p| p.plugin_id == "insta") {
            break;
        }
        tokio::task::yield_now().await;
    }

    client.send("@label=j1 NS JOIN instagram://acme-corp");
    let routed = plugin.recv_raw().await;
    let reply = weft_proto::Reply::parse(&routed).expect("PROVISION parses");
    let Event::Provision { uri, job } = reply.event else {
        panic!("plugin expected PROVISION, got {routed}");
    };
    assert_eq!(uri.to_string(), "instagram://acme-corp");

    // The plugin reports failure → the parked join completes NO-SUCH-TARGET.
    plugin.send(&format!("PROVISION-ERR {job}"));
    let reply = client.expect_err(ErrCode::NoSuchTarget).await;
    assert_eq!(reply.label.as_deref(), Some("j1"));
}

#[tokio::test]
async fn plugin_unauthorized_scheme_is_refused() {
    // A provider declaring a scheme its pin does not authorize must fail the
    // whole registration — refused LOUDLY with a typed error + close (spec
    // §4.2), never silently tolerated; nothing lands in the registry.
    let key = Keypair::generate();
    let ctx = ctx_plugin("modq", &key.public()); // no schemes authorized

    let mut plugin = plugin_session(&ctx, &key).await;
    let reg = weft_proto::Registration {
        api: 1,
        id: "modq".into(),
        name: "Sneaky".into(),
        icon: None,
        actions: vec![],
        hooks: vec![],
        bot: None,
        schemes: vec!["instagram".into()],
    };
    plugin.send(&format!(
        "PLUGIN-REGISTER :{}",
        weft_proto::plugin_to_b64(&reg).unwrap()
    ));
    plugin.expect_err(ErrCode::Forbidden).await;
    assert!(plugin.closed().await);

    // Nothing registered, so the scheme routes nowhere: a foreign join answers
    // NO-SUCH-TARGET immediately.
    let mut client = ready(&ctx, "ada").await;
    client.send("@label=j1 NS JOIN instagram://acme-corp");
    let reply = client.expect_err(ErrCode::NoSuchTarget).await;
    assert_eq!(reply.label.as_deref(), Some("j1"));
}

#[tokio::test]
async fn foreign_ns_join_succeeds_via_assertion() {
    // The slice-3 vertical (framework §3.3): NS JOIN <uri> → PROVISION → the
    // provider asserts structure with NORMAL verbs (NS-META / CHANNEL-LAYOUT,
    // URI targets) → PROVISION-OK → the parked join completes with the minted,
    // origin-badged replica. Then the known-local branch + the authority gate.
    let key = Keypair::generate();
    let ctx = ctx_plugin_schemes("insta", &key.public(), vec!["instagram".parse().unwrap()]);

    let mut plugin = plugin_session(&ctx, &key).await;
    let reg = weft_proto::Registration {
        api: 1,
        id: "insta".into(),
        name: "Instagram Bridge".into(),
        icon: None,
        actions: vec![],
        hooks: vec![],
        bot: None,
        schemes: vec!["instagram".into()],
    };
    plugin.send(&format!(
        "PLUGIN-REGISTER :{}",
        weft_proto::plugin_to_b64(&reg).unwrap()
    ));

    // Barrier: the registration landed (poll the catalog for the plugin entry).
    let mut ada = ready(&ctx, "ada").await;
    for _ in 0..50 {
        ada.send("PLUGINS");
        let Event::PluginManifest { catalog } = ada.recv().await.event else {
            panic!("expected PLUGIN-MANIFEST");
        };
        let cat: weft_proto::Catalog = weft_proto::plugin_from_b64(&catalog).unwrap();
        if cat.plugins.iter().any(|p| p.plugin_id == "insta") {
            break;
        }
        tokio::task::yield_now().await;
    }

    // First contact: the join parks; the provider gets the PROVISION push.
    ada.send("@label=j1 NS JOIN instagram://acme-corp/club");
    let routed = weft_proto::Reply::parse(&plugin.recv_raw().await).unwrap();
    let Event::Provision { uri, job } = routed.event else {
        panic!("expected PROVISION");
    };
    assert_eq!(uri.to_string(), "instagram://acme-corp/club");

    // The provider asserts the space with NORMAL verbs on URI targets, learning
    // its minted mapping from each reply.
    plugin.send(&format!(
        "@title=Club;id={} NS-META instagram://acme-corp/club public",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::NsMeta {
        id, origin, title, ..
    } = plugin.recv().await.event
    else {
        panic!("expected the minted NS-META mapping");
    };
    let ns_id = id.to_string();
    assert_eq!(
        origin.map(|o| o.to_string()).as_deref(),
        Some("instagram://acme-corp/club")
    );
    assert_eq!(title.as_deref(), Some("Club"));

    plugin.send(&format!(
        "@vanity=general;id={} CHANNEL-LAYOUT instagram://acme-corp/club/general 0",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::ChannelLayout {
        channel,
        origin,
        vanity,
        ..
    } = plugin.recv().await.event
    else {
        panic!("expected the minted CHANNEL-LAYOUT mapping");
    };
    assert!(channel.as_str().starts_with(&format!("#{ns_id}/")));
    assert_eq!(
        origin.map(|o| o.to_string()).as_deref(),
        Some("instagram://acme-corp/club/general")
    );
    assert_eq!(vanity, "general");

    // The provider completes the job → the parked join acks: NS-META +
    // CHANNEL-LAYOUT (both badged) + the labeled NS-MEMBER join.
    plugin.send(&format!("PROVISION-OK {job}"));
    let Event::NsMeta { origin, .. } = ada.recv().await.event else {
        panic!("ada expected NS-META");
    };
    assert!(origin.is_some(), "the replica is badged");
    assert!(matches!(
        ada.recv().await.event,
        Event::ChannelLayout { .. }
    ));
    let reply = ada.recv().await;
    assert_eq!(reply.label.as_deref(), Some("j1"));
    let Event::NsMember {
        action: MemberAction::Join,
        count,
        ..
    } = reply.event
    else {
        panic!("expected the labeled NS-MEMBER join ack, got {reply:?}");
    };
    assert_eq!(count, Some(1));

    // Known-local branch: a second joiner takes the ordinary NS JOIN path
    // (channel subscription burst + NS-MEMBER), no provisioning round-trip.
    let mut bob = ready(&ctx, "bob").await;
    bob.send("@label=j2 NS JOIN instagram://acme-corp/club");
    let count = drain_until_ns_member(&mut bob).await;
    assert_eq!(count, Some(2));

    // The authority gate: nobody local governs a provider-managed namespace —
    // the sentinel owner confers nothing and members hold no ns-admin.
    ada.send(&format!("@label=m1 NS META {ns_id} title :Hax"));
    let reply = ada.expect_err(ErrCode::CapRequired).await;
    assert_eq!(reply.label.as_deref(), Some("m1"));
}

/// Like [`ctx_plugin_schemes`], with operator accounts and/or extra plugin pins.
fn ctx_plugin_full(
    plugins: Vec<(&str, weft_core::PublicKey, Vec<weft_proto::Scheme>)>,
    operators: &[&str],
) -> Arc<ServerCtx> {
    ctx_plugin_store(plugins, operators).0
}

/// [`ctx_plugin_full`] keeping the store, for tests that seed peer/netblock rows
/// the wire has no path to.
fn ctx_plugin_store(
    plugins: Vec<(&str, weft_core::PublicKey, Vec<weft_proto::Scheme>)>,
    operators: &[&str],
) -> (Arc<ServerCtx>, Arc<MemoryStore>) {
    let info = ServerInfo {
        network: "test.example".parse().unwrap(),
        motd: None,
        features: Vec::new(),
    };
    let chans: Vec<(&str, &str)> = Vec::new();
    let store = Arc::new(MemoryStore::default());
    let ctx = Arc::new(
        ServerCtx::new(
            info,
            chans
                .iter()
                .map(|(c, p)| (c.parse().unwrap(), p.parse::<RetentionPolicy>().unwrap())),
            Keypair::generate(),
            true,
            store.clone(),
            Arc::new(weft_core::MemBlobStore::default()),
            "permanent".parse().unwrap(),
            operators.iter().map(|o| o.parse().unwrap()),
            true,
            10,
            weft_core::FederationConfig::default(),
        )
        .with_remote_plugins(
            plugins
                .into_iter()
                .map(|(id, k, s)| (id.to_string(), k, s))
                .collect(),
        ),
    );

    (ctx, store)
}

#[tokio::test]
async fn a_cross_realm_sender_ingests_but_local_and_peer_users_are_refused() {
    // Owner decision 2026-08-05 (amends the protocol doc's §5): foreign systems
    // are cross-realm — a Matrix room homed on matrix.org has members from
    // kde.org — so `@as` must be *foreign*, not necessarily a user of the bound
    // realm. What must stay impossible is attributing to an identity anchored
    // elsewhere: a local account (our auth) or a WEFT peer's user (its keys).
    let key = Keypair::generate();
    let (ctx, store) = ctx_plugin_store(
        vec![("mx", key.public(), vec!["matrix".parse().unwrap()])],
        &[],
    );

    // peer.example is a real federation peer — its identities are its own.
    store
        .upsert_peer(weft_store::PeerRecord {
            peer: "peer.example".parse().unwrap(),
            scope: "*".into(),
            manifest: "m".into(),
            version: 1,
            acked_manifest: None,
            severed: false,
            created_ms: 0,
            updated_ms: 0,
        })
        .await
        .unwrap();

    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT matrix://matrix.org");
    plugin.send(&format!(
        "@title=Space;id={} NS-META matrix://matrix.org/space public",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::NsMeta { id, .. } = plugin.recv().await.event else {
        panic!("expected the minted NS-META");
    };
    let ns_id = id.to_string();
    plugin.send(&format!(
        "@vanity=general;id={} CHANNEL-LAYOUT matrix://matrix.org/space/general 0",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::ChannelLayout { channel, .. } = plugin.recv().await.event else {
        panic!("expected the minted CHANNEL-LAYOUT");
    };

    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@label=j1 NS JOIN {ns_id}"));
    drain_until_ns_member(&mut ada).await;
    assert!(matches!(
        weft_proto::Request::parse(&plugin.recv_raw().await)
            .unwrap()
            .command,
        weft_proto::Command::NsJoin { .. }
    ));

    // A third-server user posts into the matrix.org-homed room. The msgid is
    // still minted under the **channel's** realm — the room's home is the
    // authority for its event ids, whoever the author's homeserver is.
    let posted = format!("matrix.org/{}", ulid::Ulid::new());
    plugin.send(&format!(
        "@as=carol@kde.org;msgid={posted} MSG {channel} :hello from kde"
    ));
    let Event::Message(m) = ada.recv().await.event else {
        panic!("ada expected the cross-realm MESSAGE");
    };
    assert_eq!(m.sender.to_string(), "carol@kde.org");
    assert_eq!(m.msgid.to_string(), posted);

    // A local account stays unforgeable…
    plugin.send(&format!(
        "@as=ada@test.example;msgid=matrix.org/{} MSG {channel} :forged",
        ulid::Ulid::new()
    ));
    plugin.expect_err(ErrCode::Unsupported).await;

    // …and so does a federation peer's user.
    plugin.send(&format!(
        "@as=eve@peer.example;msgid=matrix.org/{} MSG {channel} :forged",
        ulid::Ulid::new()
    ));
    plugin.expect_err(ErrCode::Unsupported).await;

    // §8's return path: ada reacts to the realm-minted message; weftd relays
    // the request to the provider rather than minting anything…
    let root = m.msgid.clone();
    ada.send(&format!("@label=r1 REACT {root} wave"));
    let raw = plugin.recv_raw().await;
    let relayed = weft_proto::Request::parse(&raw).unwrap();
    assert!(matches!(relayed.command, weft_proto::Command::React { .. }));
    assert!(
        raw.contains("ulid="),
        "the mutation relay carries @ulid: {raw}"
    );

    // …the provider performs it foreign-side and **confirms it back through
    // ingestion, attributed to ada** — the one shape of local `@as` that is
    // the flow completing rather than a forgery. Without this the flip side
    // never closes: the puppet's echo always maps back to a local account.
    plugin.send(&format!("@as=ada@test.example REACT {root} wave"));
    let Event::Reaction { by, op, .. } = ada.recv().await.event else {
        panic!("ada expected her own confirmed REACTION");
    };
    assert_eq!(op, weft_proto::ReactionOp::Add);
    assert_eq!(by.to_string(), "ada@test.example");
}

#[tokio::test]
async fn provider_ingests_foreign_messages() {
    // Slice 4: the provider replays a foreign room's traffic as ordinary verbs
    // with a `<scheme>://` channel target + `@as=<foreign identity>`; weftd mints
    // the WEFT-side event (home-authoritative) and stamps `foreign=` for display.
    let key = Keypair::generate();
    let ctx = ctx_plugin_full(
        vec![("insta", key.public(), vec!["instagram".parse().unwrap()])],
        &[],
    );

    let mut plugin = plugin_session(&ctx, &key).await;
    // Binding the realm registers the scheme → its namespaces are online (3b-b).
    plugin.send("REALM ASSERT instagram://acme-corp");
    plugin.send(&format!(
        "@title=Club;id={} NS-META instagram://acme-corp/club public",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::NsMeta { id, .. } = plugin.recv().await.event else {
        panic!("expected the minted NS-META");
    };
    let ns_id = id.to_string();
    plugin.send(&format!(
        "@vanity=general;id={} CHANNEL-LAYOUT instagram://acme-corp/club/general 0",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::ChannelLayout { channel, .. } = plugin.recv().await.event else {
        panic!("expected the minted CHANNEL-LAYOUT");
    };

    // A local member joins the namespace — which auto-subscribes her to the
    // replica's visible channels (v0.12 derived membership).
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@label=j1 NS JOIN {ns_id}"));
    drain_until_ns_member(&mut ada).await;
    // Her join is relayed to the realm as a request (slice 5) — consume it so
    // the assertions below read the provider's stream from a known point.
    assert!(matches!(
        weft_proto::Request::parse(&plugin.recv_raw().await)
            .unwrap()
            .command,
        weft_proto::Command::NsJoin { .. }
    ));

    // The provider ingests a foreign post, addressing the replica by the
    // canonical name it learned from the CHANNEL-LAYOUT mapping reply. **A realm
    // is a network**: the provider names its users on the realm and mints their
    // msgids under it, exactly as a peer WEFT network does. `alice=bob` is a
    // Matrix localpart that survives verbatim — a lossy mapping would collide it
    // with `@alicebob`, merging two people into one identity.
    let posted = format!("acme-corp/{}", ulid::Ulid::new());
    plugin.send(&format!(
        "@as=alice=bob@acme-corp;msgid={posted} MSG {channel} :hi from insta"
    ));

    let Event::Message(m) = ada.recv().await.event else {
        panic!("ada expected the ingested MESSAGE");
    };
    // The bridge minted it, so the realm is the origin (invariant 2) — the
    // replica is indistinguishable from a federated peer's channel.
    assert_eq!(m.msgid.to_string(), posted);
    assert_eq!(m.body, "hi from insta");
    assert_eq!(m.sender.to_string(), "alice=bob@acme-corp");

    // The mutation verbs: the provider names the target by msgid (not channel).
    // EDIT carries its own minted id; DELETE/REACT name only the root they act on.
    let root = m.msgid.clone();
    plugin.send(&format!(
        "@as=alice=bob@acme-corp;msgid=acme-corp/{} EDIT {root} :fixed",
        ulid::Ulid::new()
    ));
    let Event::Edited { body, user, .. } = ada.recv().await.event else {
        panic!("ada expected the ingested EDITED");
    };
    assert_eq!(body, "fixed");
    assert_eq!(user.to_string(), "alice=bob@acme-corp");

    plugin.send(&format!("@as=bob@acme-corp REACT {root} thumbsup"));
    let Event::Reaction { by, op, .. } = ada.recv().await.event else {
        panic!("ada expected the ingested REACTION");
    };
    assert_eq!(op, weft_proto::ReactionOp::Add);
    assert_eq!(by.to_string(), "bob@acme-corp");

    plugin.send(&format!("@as=alice=bob@acme-corp DELETE {root}"));
    let Event::Deleted { msgid, by, .. } = ada.recv().await.event else {
        panic!("ada expected the ingested DELETED");
    };
    assert_eq!(msgid, root);
    assert_eq!(
        by.map(|u| u.to_string()).as_deref(),
        Some("alice=bob@acme-corp")
    );

    // A provider may not attribute an event to a user outside its own realm —
    // that is how it would otherwise forge a post by a local account.
    plugin.send(&format!("@as=ada@test.example MSG {channel} :forged"));
    plugin.expect_err(ErrCode::Unsupported).await;

    // 4c: the realm states that one of its users is a member — it is the
    // authority for its own space, so this is an NS-MEMBER *event*, not a
    // request. Membership persists under the foreign member key and shows in the
    // derived roster + member count.
    plugin.send(&format!("NS-MEMBER {ns_id} carol@acme-corp join"));
    let Event::Member {
        user,
        action,
        count,
        ..
    } = ada.recv().await.event
    else {
        panic!("ada expected the ingested MEMBER join");
    };
    assert_eq!(action, MemberAction::Join);
    assert_eq!(user.to_string(), "carol@acme-corp");
    assert_eq!(count, Some(2)); // ada + carol

    // The roster (MEMBERS) lists the bridged member alongside the local one.
    ada.send(&format!("@label=mem MEMBERS {channel}"));
    let roster = roster_names(&mut ada).await;
    assert!(roster.contains("ada"), "{roster:?}");
    assert!(roster.contains("carol"), "{roster:?}");

    // …and a part statement clears it.
    plugin.send(&format!("NS-MEMBER {ns_id} carol@acme-corp part"));
    let Event::Member { action, count, .. } = ada.recv().await.event else {
        panic!("ada expected the ingested MEMBER part");
    };
    assert_eq!(action, MemberAction::Part);
    assert_eq!(count, Some(1));

    // An ingest for a channel we don't mirror is dropped silently (a provider
    // replaying an unmirrored room is normal, and no user awaits a reply). The
    // FIFO barrier: an unauthorized REALM REGISTER right after must be the next
    // — and only — line we read.
    plugin.send(&format!(
        "@as=bob@acme-corp;msgid=acme-corp/{} MSG #{ns_id}/01bx5zzkbkactav9wevgemmvr0 :dropped",
        ulid::Ulid::new()
    ));
    plugin.send("@label=probe REALM REGISTER discord");
    let reply = plugin.expect_err(ErrCode::Unsupported).await;
    assert_eq!(reply.label.as_deref(), Some("probe"));
}

#[tokio::test]
async fn no_wire_authority_over_a_replica_not_even_for_an_operator() {
    // Owner directive 2026-08-04: operator/admin authority lives in a **separate
    // permission table** and acts only through the web admin panel. `*` no longer
    // confers power inside a namespace — only `ns-admin` does — and that holds
    // for a provider-managed namespace too. (This replaces the old "operator
    // escape hatch", which granted operators every cap on a replica.)
    let key = Keypair::generate();
    let ctx = ctx_plugin_full(
        vec![("insta", key.public(), vec!["instagram".parse().unwrap()])],
        &["op"],
    );

    let mut plugin = plugin_session(&ctx, &key).await;
    // Bound to its realm: that binding is what its authority is scoped by.
    plugin.send("REALM ASSERT instagram://acme-corp");
    plugin.send(&format!(
        "@title=Club;id={} NS-META instagram://acme-corp/club public",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::NsMeta { id, .. } = plugin.recv().await.event else {
        panic!("expected the minted NS-META");
    };
    let ns_id = id.to_string();

    // A plain member holds nothing here — the replica's sentinel owner confers
    // no authority, so the owner shortcut never fires.
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@label=d1 NS DELETE {ns_id} {ns_id}"));
    ada.expect_err(ErrCode::CapRequired).await;

    // …and neither does an operator, over the wire. Deleting an orphaned replica
    // is the admin panel's job (`DELETE /api/v1/namespaces/:name`, store-direct
    // under `AdminScope::Destroy`), which is the same out-of-band path used for
    // every other cross-namespace intervention.
    let mut op = ready(&ctx, "op").await;
    op.send(&format!("@label=d2 NS DELETE {ns_id} {ns_id}"));
    op.expect_err(ErrCode::CapRequired).await;

    // The realm may appoint a local admin, and *that* authority is real.
    plugin.send(&format!("GRANT ada ns:{ns_id} ns-admin"));
    let ack = weft_proto::Reply::parse(&plugin.recv_raw().await).unwrap();
    assert!(
        matches!(ack.event, Event::Token { .. }),
        "the realm may appoint an admin, got {ack:?}"
    );

    ada.send(&format!("@label=d3 NS DELETE {ns_id} {ns_id}"));
    let reply = ada.recv().await;
    assert_eq!(reply.label.as_deref(), Some("d3"));
    let Event::NsMeta {
        owner, description, ..
    } = reply.event
    else {
        panic!("expected the deletion tombstone, got {reply:?}");
    };
    assert!(owner.is_none());
    assert_eq!(description.as_deref(), Some("deleted"));
}

#[tokio::test]
async fn realm_withdraw_tombstones_namespaces() {
    // 3b-b (framework §3.1): WITHDRAW = the realm is GONE upstream — weftd
    // deletes its virtual namespaces and pushes the tombstone to members.
    // (Distinct from a disconnect, which is only *offline*.)
    let key = Keypair::generate();
    let ctx = ctx_plugin_full(
        vec![("insta", key.public(), vec!["instagram".parse().unwrap()])],
        &[],
    );

    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT instagram://acme-corp");
    plugin.send(&format!(
        "@title=Club;id={} NS-META instagram://acme-corp/club public",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::NsMeta { id, .. } = plugin.recv().await.event else {
        panic!("expected the minted NS-META");
    };
    let ns_id = id.to_string();

    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@label=j1 NS JOIN {ns_id}"));
    assert!(matches!(ada.recv().await.event, Event::NsMember { .. }));

    plugin.send("REALM WITHDRAW");
    let Event::NsMeta {
        owner, description, ..
    } = ada.recv().await.event
    else {
        panic!("ada expected the withdrawal tombstone");
    };
    assert!(owner.is_none());
    assert_eq!(description.as_deref(), Some("deleted"));

    // The namespace is gone (not merely offline): joining by id is absent.
    let mut bob = ready(&ctx, "bob").await;
    bob.send(&format!("@label=j2 NS JOIN {ns_id}"));
    bob.expect_err(ErrCode::NoSuchTarget).await;
}

#[tokio::test]
async fn duplicate_scheme_claim_is_refused() {
    // 3b-c: the first registrant holds a scheme; a second claimant (a
    // deployment error) is refused loudly — never routed by HashMap luck.
    let key1 = Keypair::generate();
    let key2 = Keypair::generate();
    let scheme: weft_proto::Scheme = "instagram".parse().unwrap();
    let ctx = ctx_plugin_full(
        vec![
            ("insta-a", key1.public(), vec![scheme.clone()]),
            ("insta-b", key2.public(), vec![scheme]),
        ],
        &[],
    );

    let reg = |id: &str| weft_proto::Registration {
        api: 1,
        id: id.into(),
        name: id.into(),
        icon: None,
        actions: vec![],
        hooks: vec![],
        bot: None,
        schemes: vec!["instagram".into()],
    };
    let first = plugin_session(&ctx, &key1).await;
    first.send(&format!(
        "PLUGIN-REGISTER :{}",
        weft_proto::plugin_to_b64(&reg("insta-a")).unwrap()
    ));

    let mut second = plugin_session(&ctx, &key2).await;
    // Barrier: the first registration is on another session — poll until its
    // scheme claim would collide, by retrying the second registration? No —
    // registration is one-shot. Instead poll the catalog via a client.
    let mut probe = ready(&ctx, "probe").await;
    for _ in 0..50 {
        probe.send("PLUGINS");
        let Event::PluginManifest { catalog } = probe.recv().await.event else {
            panic!("expected PLUGIN-MANIFEST");
        };
        let cat: weft_proto::Catalog = weft_proto::plugin_from_b64(&catalog).unwrap();
        if cat.plugins.iter().any(|p| p.plugin_id == "insta-a") {
            break;
        }
        tokio::task::yield_now().await;
    }

    second.send(&format!(
        "PLUGIN-REGISTER :{}",
        weft_proto::plugin_to_b64(&reg("insta-b")).unwrap()
    ));
    second.expect_err(ErrCode::Conflict).await;
    assert!(second.closed().await);
}

#[tokio::test]
async fn local_posts_relay_outward_without_looping() {
    // Slice 5: a local user's traffic in a replica channel is forwarded to the
    // provider (to puppet into the foreign system), while an event the provider
    // itself ingested is NEVER sent back — the `foreign=` tag is the loop guard
    // (msgid origin can't distinguish them: a replica is home-authoritative).
    let key = Keypair::generate();
    let ctx = ctx_plugin_full(
        vec![("insta", key.public(), vec!["instagram".parse().unwrap()])],
        &[],
    );

    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT instagram://acme-corp");
    plugin.send(&format!(
        "@title=Club;id={} NS-META instagram://acme-corp/club public",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::NsMeta { id, .. } = plugin.recv().await.event else {
        panic!("expected the minted NS-META");
    };
    let ns_id = id.to_string();
    plugin.send(&format!(
        "@vanity=general;id={} CHANNEL-LAYOUT instagram://acme-corp/club/general 0",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::ChannelLayout { channel, .. } = plugin.recv().await.event else {
        panic!("expected the minted CHANNEL-LAYOUT");
    };

    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@label=j1 NS JOIN {ns_id}"));
    drain_until_ns_member(&mut ada).await;

    // A LOCAL member's namespace JOIN is relayed to the realm as a **request**:
    // a bridge behaves as a federation peer, so we send the command and the realm
    // answers with the authoritative NS-MEMBER. Membership is namespace-level —
    // channels are not joinable — so it names only the namespace; putting her
    // into the foreign rooms is the adapter's job.
    let raw = plugin.recv_raw().await;
    let line = weft_proto::Line::parse(&raw).unwrap();
    assert_eq!(
        line.tags.get("as").map(String::as_str),
        Some("ada@test.example")
    );
    let weft_proto::Command::NsJoin { ns } = weft_proto::Request::from_line(&line).unwrap().command
    else {
        panic!("provider expected the relayed NS JOIN, got {raw}");
    };
    assert_eq!(ns.as_str(), ns_id.to_lowercase());

    // A LOCAL post is relayed outward to the provider as a **request** — the realm
    // is the source of truth in its own channels, so weftd mints nothing and does
    // not echo. It carries the poster's identity and a bridge label.
    ada.send(&format!("@label=m1 MSG {channel} :hello matrix"));
    let raw = plugin.recv_raw().await;
    let line = weft_proto::Line::parse(&raw).unwrap();
    assert_eq!(
        line.tags.get("as").map(String::as_str),
        Some("ada@test.example")
    );
    let bridge_label = line
        .tags
        .get("label")
        .expect("the relayed post carries a bridge label")
        .clone();
    let weft_proto::Command::Msg { body, .. } =
        weft_proto::Request::from_line(&line).unwrap().command
    else {
        panic!("provider expected the relayed MSG request, got {raw}");
    };
    assert_eq!(body.as_deref(), Some("hello matrix"));

    // The realm mints the id and hands the message back, quoting the label. THAT
    // is what ada finally sees — her own message, with a foreign-origin msgid.
    let root: weft_proto::MsgId = format!("acme-corp/{}", ulid::Ulid::new()).parse().unwrap();
    plugin.send(&format!(
        "@as=ada@test.example;msgid={root};label={bridge_label} MSG {channel} :hello matrix"
    ));
    let reply = ada.recv().await;
    // It carries **her own** label (`m1`, not the bridge label): that is the ack her
    // client reconciles its greyed optimistic echo against — without it she would
    // see her own message arrive as a stranger's.
    assert_eq!(reply.label.as_deref(), Some("m1"));
    let Event::Message(m) = reply.event else {
        panic!("ada expected her post back, minted by the realm");
    };
    assert_eq!(m.msgid, root);
    assert_eq!(m.sender.to_string(), "ada@test.example");

    // A local EDIT + REACT relay outward too — the root is foreign-origin now, so
    // these take the same ask-the-provider path as any bridged message.
    ada.send(&format!("@label=e1 EDIT {root} :hello again"));
    assert!(matches!(
        weft_proto::Request::from_line(&weft_proto::Line::parse(&plugin.recv_raw().await).unwrap())
            .unwrap()
            .command,
        weft_proto::Command::Edit { .. }
    ));
    ada.send(&format!("@label=r1 REACT {root} wave"));
    assert!(matches!(
        weft_proto::Request::from_line(&weft_proto::Line::parse(&plugin.recv_raw().await).unwrap())
            .unwrap()
            .command,
        weft_proto::Command::React { .. }
    ));

    // THE LOOP GUARD: the provider ingests a foreign post; the resulting event
    // must NOT come back to it. The next line it reads is the local delete that
    // follows — proving the ingested one was never relayed.
    plugin.send(&format!(
        "@as=alice@acme-corp;msgid=acme-corp/{} MSG {channel} :from matrix",
        ulid::Ulid::new()
    ));
    ada.send(&format!("@label=d1 DELETE {root}"));

    let raw = plugin.recv_raw().await;
    let line = weft_proto::Line::parse(&raw).unwrap();
    let weft_proto::Command::Delete { msgid } =
        weft_proto::Request::from_line(&line).unwrap().command
    else {
        panic!("provider expected the local DELETE next — an ingested event looped back! {raw}");
    };
    assert_eq!(msgid, root);

    // …and the same for a local LEAVE. The realm's own membership statement is
    // not echoed back to it: the NS-MEMBER below produces no outward line, so the
    // next thing the provider reads is ada's leave request.
    plugin.send(&format!("NS-MEMBER {ns_id} carol@acme-corp join"));
    ada.send(&format!("@label=p1 NS LEAVE {ns_id}"));

    let raw = plugin.recv_raw().await;
    let line = weft_proto::Line::parse(&raw).unwrap();
    assert_eq!(
        line.tags.get("as").map(String::as_str),
        Some("ada@test.example")
    );
    assert!(
        matches!(
            weft_proto::Request::from_line(&line).unwrap().command,
            weft_proto::Command::NsLeave { .. }
        ),
        "expected the local leave next — the realm's NS-MEMBER echoed back! {raw}"
    );
}

#[tokio::test]
async fn local_mutations_of_a_bridged_message_relay_to_the_provider() {
    // Owner directive 2026-08-04: a WEFT user must be able to react to (and a
    // moderator to delete) a *Matrix-originated* message. The foreign side owns
    // those events, so we never mint them here — we ask the provider to perform
    // them, and its resulting event arrives back through ordinary ingestion.
    let key = Keypair::generate();
    let ctx = ctx_plugin_full(
        vec![("insta", key.public(), vec!["instagram".parse().unwrap()])],
        &["root"],
    );

    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT instagram://acme-corp");
    plugin.send(&format!(
        "@title=Club;id={} NS-META instagram://acme-corp/club public",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::NsMeta { id, .. } = plugin.recv().await.event else {
        panic!("expected the minted NS-META");
    };
    let ns_id = id.to_string();
    plugin.send(&format!(
        "@vanity=general;id={} CHANNEL-LAYOUT instagram://acme-corp/club/general 0",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::ChannelLayout { channel, .. } = plugin.recv().await.event else {
        panic!("expected the minted CHANNEL-LAYOUT");
    };

    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@label=j1 NS JOIN {ns_id}"));
    drain_until_ns_member(&mut ada).await;
    // Her join relays outward as a request (slice 5) — consume it.
    assert!(matches!(
        weft_proto::Request::parse(&plugin.recv_raw().await)
            .unwrap()
            .command,
        weft_proto::Command::NsJoin { .. }
    ));

    // The provider ingests a message of its own.
    let posted = format!("acme-corp/{}", ulid::Ulid::new());
    plugin.send(&format!(
        "@as=alice@acme-corp;msgid={posted} MSG {channel} :from insta"
    ));
    let Event::Message(m) = ada.recv().await.event else {
        panic!("ada expected the ingested MESSAGE");
    };
    let root = m.msgid.clone();
    assert_eq!(root.origin().as_str(), "acme-corp");

    // Ada reacts to it. Nothing is minted locally — the provider is asked to do
    // it, carrying `@as` naming *her* (the mirror image of ingestion's `@as`).
    ada.send(&format!("@label=r1 REACT {root} wave"));
    let relayed = weft_proto::Request::parse(&plugin.recv_raw().await).unwrap();
    let weft_proto::Command::React { msgid, emoji } = relayed.command else {
        panic!("provider expected the relayed REACT");
    };
    assert_eq!(msgid, root);
    assert_eq!(emoji, "wave");

    // Ada is not the author, so EDIT is still refused on authorship — the relay
    // route does not weaken the ordinary checks.
    ada.send(&format!("@label=e1 EDIT {root} :not mine"));
    let reply = ada.expect_err(ErrCode::CapRequired).await;
    assert_eq!(reply.label.as_deref(), Some("e1"));

    // A moderator's DELETE *is* relayed — decision 20-H: the adapter's bot
    // performs the redaction foreign-side. The realm grants that authority:
    // being a network operator confers nothing inside a namespace (operators act
    // through the web admin panel, never as wire capability).
    let mut root_op = ready(&ctx, "root").await;
    plugin.send(&format!("GRANT root ns:{ns_id} delete-any"));
    root_op.send(&format!("@label=j2 NS JOIN {ns_id}"));
    drain_until_ns_member(&mut root_op).await;
    // Skip the GRANT's Token ack, then take the join request.
    loop {
        let raw = plugin.recv_raw().await;
        if matches!(
            weft_proto::Request::parse(&raw).map(|r| r.command),
            Ok(weft_proto::Command::NsJoin { .. })
        ) {
            break;
        }
    }

    root_op.send(&format!("@label=d1 DELETE {root}"));
    let relayed = weft_proto::Request::parse(&plugin.recv_raw().await).unwrap();
    let weft_proto::Command::Delete { msgid } = relayed.command else {
        panic!("provider expected the relayed DELETE");
    };
    assert_eq!(msgid, root);
}

#[tokio::test]
async fn weftd_states_its_local_membership_when_a_realm_connects() {
    // The other half of the reconcile: weftd applies `NS LEAVE` whether or not the
    // adapter is connected, and its pushes are live-only, so a leave during
    // downtime never reaches the realm and the foreign side keeps a member we no
    // longer have. The adapter cannot ask (it holds a key, not an account, so the
    // cap-gated NS INFO MEMBERS is closed to it), so weftd states the set on
    // connect — one batch spanning every governed namespace, rows naming their own
    // namespace, so an emptied roster is still identifiable.
    let key = Keypair::generate();
    let ctx = ctx_plugin_full(
        vec![("insta", key.public(), vec!["instagram".parse().unwrap()])],
        &[],
    );

    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT instagram://acme-corp");
    plugin.send(&format!(
        "@title=Club;id={} NS-META instagram://acme-corp/club public",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::NsMeta { id, .. } = plugin.recv().await.event else {
        panic!("expected the minted NS-META");
    };
    let ns_id = id.to_string();

    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@label=j1 NS JOIN {ns_id}"));
    drain_until_ns_member(&mut ada).await;
    assert!(matches!(
        weft_proto::Request::parse(&plugin.recv_raw().await)
            .unwrap()
            .command,
        weft_proto::Command::NsJoin { .. }
    ));

    // A fresh session for the same realm — the reconnect. Its REALM ASSERT must be
    // answered with the local membership we hold.
    let mut reconnected = plugin_session(&ctx, &key).await;
    reconnected.send("REALM ASSERT instagram://acme-corp");

    let mut stated = std::collections::HashSet::new();
    let mut batched = false;
    for _ in 0..40 {
        match reconnected.recv().await.event {
            Event::BatchStart { id } if id.starts_with("ni") => batched = true,
            Event::NsMemberInfo {
                namespace, user, ..
            } => {
                stated.insert((namespace.to_string(), user.account.as_str().to_string()));
            }
            Event::BatchEnd { id, .. } if id.starts_with("ni") => break,
            _ => {}
        }
    }

    assert!(
        batched,
        "the statement must be framed as an `ni…` roster batch"
    );
    assert!(
        stated.contains(&(ns_id.clone(), "ada".to_string())),
        "weftd must state the local members it holds: {stated:?}"
    );
}

#[tokio::test]
async fn a_realm_resyncs_membership_by_restating_it() {
    // Framework §7a.0a: a realm corrects drift by re-stating its whole
    // membership inside the ordinary SYNC snapshot framing (§6.9) — the same one
    // a client gets on login, with the roles swapped: here the realm holds the
    // state and weftd conforms. Anyone not named by `SYNC END` is dropped.
    let key = Keypair::generate();
    let ctx = ctx_plugin_full(
        vec![("insta", key.public(), vec!["instagram".parse().unwrap()])],
        &[],
    );

    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT instagram://acme-corp");
    plugin.send(&format!(
        "@title=Club;id={} NS-META instagram://acme-corp/club public",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::NsMeta { id, .. } = plugin.recv().await.event else {
        panic!("expected the minted NS-META");
    };
    let ns_id = id.to_string();
    plugin.send(&format!(
        "@vanity=general;id={} CHANNEL-LAYOUT instagram://acme-corp/club/general 0",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::ChannelLayout { channel, .. } = plugin.recv().await.event else {
        panic!("expected the minted CHANNEL-LAYOUT");
    };

    // Two foreign members, stated live.
    plugin.send(&format!("NS-MEMBER {ns_id} carol@acme-corp join"));
    plugin.send(&format!("NS-MEMBER {ns_id} dave@acme-corp join"));

    // A local member joins too, so the resync is proven not to wipe our own
    // users just because it is the realm speaking.
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@label=j1 NS JOIN {ns_id}"));
    drain_until_ns_member(&mut ada).await;
    assert!(matches!(
        weft_proto::Request::parse(&plugin.recv_raw().await)
            .unwrap()
            .command,
        weft_proto::Command::NsJoin { .. }
    ));

    ada.send(&format!("@label=mem1 MEMBERS {channel}"));
    let roster = roster_names(&mut ada).await;
    assert!(roster.contains("carol"), "{roster:?}");
    assert!(roster.contains("dave"), "{roster:?}");
    assert!(roster.contains("ada"), "{roster:?}");

    // Now the realm re-states: dave left while we weren't looking, and it never
    // names him. Ada is named because the realm accepted her join.
    plugin.send("SYNC START");
    plugin.send(&format!("NS-MEMBER {ns_id} carol@acme-corp join"));
    plugin.send(&format!("NS-MEMBER {ns_id} ada@test.example join"));
    plugin.send("@cursor=c1 SYNC END");

    // Dave is gone; the others survive. The MEMBERS reply is the FIFO barrier —
    // it is answered after the whole statement was processed.
    ada.send(&format!("@label=mem2 MEMBERS {channel}"));
    let roster = roster_names(&mut ada).await;
    assert!(
        !roster.contains("dave"),
        "dave should be pruned: {roster:?}"
    );
    assert!(roster.contains("carol"), "{roster:?}");
    assert!(
        roster.contains("ada"),
        "a local member must survive: {roster:?}"
    );

    // A stray `SYNC END` names nobody — it must be ignored, not obeyed, or it
    // would wipe the namespace.
    plugin.send("@cursor=c2 SYNC END");
    ada.send(&format!("@label=mem3 MEMBERS {channel}"));
    let roster = roster_names(&mut ada).await;
    assert!(
        roster.contains("carol"),
        "unopened SYNC END wiped it: {roster:?}"
    );
    assert!(roster.contains("ada"), "{roster:?}");

    // And the case a real adapter actually produces: it re-states the space from
    // foreign room state, which lists the *foreign* members only — our local
    // accounts appear there as puppets, which an adapter filters out because
    // their traffic is a relay of ours. So ada is not named at all, and must
    // still survive: a full-replace prunes only what its author could enumerate.
    // Pruning by omission here parted every local member of every bridged
    // namespace on every bridge reconnect.
    plugin.send("SYNC START");
    plugin.send(&format!("NS-MEMBER {ns_id} carol@acme-corp join"));
    plugin.send("@cursor=c3 SYNC END");

    ada.send(&format!("@label=mem4 MEMBERS {channel}"));
    let roster = roster_names(&mut ada).await;
    assert!(
        roster.contains("ada"),
        "an unnamed local member must survive a realm resync: {roster:?}"
    );
    assert!(roster.contains("carol"), "{roster:?}");
}

#[tokio::test]
async fn dm_with_a_bridged_user_flows_both_ways() {
    // Slice 4d: a WEFT user can DM a bridged (Matrix) user and be DMed back. The
    // conversation is an ordinary `Scope::Dm` keyed by member keys — first-class,
    // not a second table — with the outbound copy carried to the provider.
    let key = Keypair::generate();
    let ctx = ctx_plugin_full(
        vec![("insta", key.public(), vec!["instagram".parse().unwrap()])],
        &[],
    );

    // The realm has to be reachable for a DM to route to it, which is what the
    // namespace assertion establishes.
    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT instagram://acme-corp");
    plugin.send(&format!(
        "@title=Club;id={} NS-META instagram://acme-corp/club public",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    assert!(matches!(plugin.recv().await.event, Event::NsMeta { .. }));

    let mut ada = ready(&ctx, "ada").await;

    // OUTBOUND: ada DMs a bridged user. She gets her own echo locally…
    ada.send("@label=d1 MSG @alice@acme-corp :hey from weft");
    let reply = ada.recv().await;
    assert_eq!(reply.label.as_deref(), Some("d1"));
    let Event::Message(m) = reply.event else {
        panic!("ada expected her own DM echo");
    };
    let outbound = m.msgid.clone();
    assert_eq!(m.body, "hey from weft");
    assert_eq!(
        m.target.to_string(),
        "@alice@acme-corp",
        "the echo names the qualified peer"
    );

    // …and the provider gets the copy to carry into the foreign system, acting
    // on her behalf.
    let raw = plugin.recv_raw().await;
    let line = weft_proto::Line::parse(&raw).unwrap();
    assert_eq!(
        line.tags.get("as").map(String::as_str),
        Some("ada@test.example")
    );
    let weft_proto::Command::Msg { target, body, .. } =
        weft_proto::Request::from_line(&line).unwrap().command
    else {
        panic!("provider expected the relayed DM, got {raw}");
    };
    assert_eq!(target.to_string(), "@alice@acme-corp");
    assert_eq!(body.as_deref(), Some("hey from weft"));

    // INBOUND: alice replies through the bridge. The realm minted it, so the
    // msgid keeps its origin (invariant 2). Stamped a millisecond after the
    // outbound one: ULIDs are only ordered to the millisecond, so two ids minted
    // by independent generators inside the same tick would sort at random.
    let minted = format!(
        "acme-corp/{}",
        ulid::Ulid::from_parts(outbound.timestamp_ms() + 1, 0)
    );
    plugin.send(&format!(
        "@as=alice@acme-corp;msgid={minted} MSG @ada :hey back"
    ));

    let Event::Message(m) = ada.recv().await.event else {
        panic!("ada expected the inbound bridged DM");
    };
    assert_eq!(m.body, "hey back");
    assert_eq!(m.sender.to_string(), "alice@acme-corp");
    assert_eq!(m.msgid.to_string(), minted);
    assert_eq!(m.target.to_string(), "@alice@acme-corp");

    // Both directions are one conversation: HISTORY on the qualified peer serves
    // it, so the DM is genuinely first-class storage.
    ada.send("@label=h1 HISTORY @alice@acme-corp");
    let bodies = history_bodies(&mut ada).await;
    assert_eq!(bodies, vec!["hey from weft", "hey back"], "{bodies:?}");
}

#[tokio::test]
async fn scrolling_past_a_replicas_history_asks_the_realm() {
    // §11.7 for bridges (owner directive 2026-08-04: "bridges should do the same
    // as federation"). Federation stores what it ingests and pulls *deeper*
    // scrollback on demand; a replica now does the same, asking its realm rather
    // than serving a short page and stopping there.
    let key = Keypair::generate();
    let ctx = ctx_plugin_full(
        vec![("insta", key.public(), vec!["instagram".parse().unwrap()])],
        &[],
    );

    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT instagram://acme-corp");
    plugin.send(&format!(
        "@title=Club;id={} NS-META instagram://acme-corp/club public",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::NsMeta { id, .. } = plugin.recv().await.event else {
        panic!("expected the minted NS-META");
    };
    let ns_id = id.to_string();
    plugin.send(&format!(
        "@vanity=general;id={} CHANNEL-LAYOUT instagram://acme-corp/club/general 0",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::ChannelLayout { channel, .. } = plugin.recv().await.event else {
        panic!("expected the minted CHANNEL-LAYOUT");
    };

    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@label=j1 NS JOIN {ns_id}"));
    drain_until_ns_member(&mut ada).await;
    assert!(matches!(
        weft_proto::Request::parse(&plugin.recv_raw().await)
            .unwrap()
            .command,
        weft_proto::Command::NsJoin { .. }
    ));

    // The channel holds one ingested message — less than the page asked for, so
    // the client has run out of local scrollback.
    let seed = format!("acme-corp/{}", ulid::Ulid::new());
    plugin.send(&format!(
        "@as=alice@acme-corp;msgid={seed} MSG {channel} :the only one we hold"
    ));
    assert!(matches!(ada.recv().await.event, Event::Message(_)));

    ada.send(&format!("@label=h1 HISTORY {channel} limit=50"));
    let bodies = history_bodies(&mut ada).await;
    assert_eq!(bodies, vec!["the only one we hold"]);

    // …so the realm is asked for the deeper window.
    let raw = plugin.recv_raw().await;
    let weft_proto::Command::History { target, .. } =
        weft_proto::Request::parse(&raw).unwrap().command
    else {
        panic!("provider expected the backfill HISTORY, got {raw}");
    };
    assert_eq!(target.to_string(), channel.to_string());

    // The realm answers by replaying the window as ordinary ingestion — there is
    // no separate backfill ingress to secure.
    let older = format!(
        "acme-corp/{}",
        ulid::Ulid::from_parts(
            seed.parse::<weft_proto::MsgId>().unwrap().timestamp_ms() - 1,
            0
        )
    );
    plugin.send(&format!(
        "@as=alice@acme-corp;msgid={older} MSG {channel} :from before"
    ));
    assert!(matches!(ada.recv().await.event, Event::Message(_)));

    ada.send(&format!("@label=h2 HISTORY {channel} limit=50"));
    let bodies = history_bodies(&mut ada).await;
    assert_eq!(bodies, vec!["from before", "the only one we hold"]);

    // The same window is not asked for twice — a repeated scroll is deduped, so
    // the next line the realm reads is a fresh request, not a duplicate.
    ada.send(&format!("@label=h3 HISTORY {channel} limit=50"));
    let _ = history_bodies(&mut ada).await;
    ada.send(&format!(
        "@label=h4 HISTORY {channel} limit=50 before={seed}"
    ));
    let _ = history_bodies(&mut ada).await;

    let raw = plugin.recv_raw().await;
    let weft_proto::Command::History { before, .. } =
        weft_proto::Request::parse(&raw).unwrap().command
    else {
        panic!("provider expected the next backfill window, got {raw}");
    };
    assert_eq!(
        before.map(|m| m.to_string()),
        Some(seed),
        "the un-deduped window is the new one"
    );
}

#[tokio::test]
async fn a_realm_may_not_shadow_a_network_we_already_know() {
    // 4b: "a realm is a network" (§7a.0) is what makes replicas behave like
    // federation — but it puts realm names in the *same namespace* as real WEFT
    // networks. A provider claiming one that is already spoken for could mint
    // users indistinguishable from that network's, and since DM routing prefers
    // a provider over a peer, quietly receive mail addressed to them.
    let key = Keypair::generate();
    let (ctx, store) = ctx_plugin_store(
        vec![("insta", key.public(), vec!["instagram".parse().unwrap()])],
        &[],
    );

    // A peer network we federate with, and a network an operator has blocked.
    store
        .upsert_peer(weft_store::PeerRecord {
            peer: "weft.example".parse().unwrap(),
            scope: "*".into(),
            manifest: String::new(),
            version: 1,
            acked_manifest: None,
            severed: false,
            created_ms: 0,
            updated_ms: 0,
        })
        .await
        .unwrap();
    store
        .add_netblock(weft_store::NetblockRecord {
            network: "evil.example".parse().unwrap(),
            reason: None,
            added_ms: 0,
            actor: "root".into(),
        })
        .await
        .unwrap();

    for (realm, why) in [
        ("test.example", "our own network"),
        ("weft.example", "a peer we federate with"),
        ("evil.example", "a netblocked network"),
    ] {
        let mut plugin = plugin_session(&ctx, &key).await;
        plugin.send(&format!("@label=r1 REALM ASSERT instagram://{realm}"));
        let reply = plugin.expect_err(ErrCode::Forbidden).await;
        assert_eq!(reply.label.as_deref(), Some("r1"), "{why}");
    }

    // An unclaimed realm binds normally — the guard is narrow.
    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT instagram://acme-corp");
    plugin.send(&format!(
        "@title=Club;id={} NS-META instagram://acme-corp/club public",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    assert!(matches!(plugin.recv().await.event, Event::NsMeta { .. }));
}

/// A `NetworkProbe` that answers for exactly the domains it was told about —
/// standing in for the `/.well-known/weft` fetch weftd does.
struct StubProbe(Vec<String>);

#[async_trait::async_trait]
impl weft_core::NetworkProbe for StubProbe {
    async fn is_weft_network(&self, host: &str) -> bool {
        self.0.iter().any(|h| h == host)
    }
}

#[tokio::test]
async fn the_domain_owner_decides_whether_a_realm_may_be_claimed() {
    // Owner directive 2026-08-04: "network should be domain validated. The person
    // who owns the domain chooses. They can either have a matrix server or a WEFT
    // server." Our peer table is local bookkeeping; the arbiter is the domain.
    let key = Keypair::generate();
    let (ctx, _store) = ctx_plugin_store(
        vec![("insta", key.public(), vec!["instagram".parse().unwrap()])],
        &[],
    );
    // `weft.example` runs a WEFT server; nothing else does. Note we hold **no**
    // peer record for it — the local checks would let it through.
    ctx.set_network_probe(Arc::new(StubProbe(vec!["weft.example".to_string()])));

    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("@label=r1 REALM ASSERT instagram://weft.example");
    let reply = plugin.expect_err(ErrCode::Forbidden).await;
    assert_eq!(reply.label.as_deref(), Some("r1"));

    // A domain whose owner did *not* choose WEFT is bridgeable — that is the
    // normal case, and the whole point of asking the domain rather than guessing.
    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT instagram://matrix.example");
    plugin.send(&format!(
        "@title=Club;id={} NS-META instagram://matrix.example/club public",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    assert!(matches!(plugin.recv().await.event, Event::NsMeta { .. }));

    // …and so is a realm that is no domain at all (a Discord guild id): the probe
    // can only answer *positively*, so an inconclusive one must never lock a
    // legitimate bridge out.
    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT instagram://123456789");
    plugin.send(&format!(
        "@title=Guild;id={} NS-META instagram://123456789/general public",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    assert!(matches!(
        recv_past_membership_statement(&mut plugin).await.event,
        Event::NsMeta { .. }
    ));
}

#[tokio::test]
async fn netblocking_a_realm_stops_its_traffic_mid_session() {
    // 4b + invariant 7 (name-keyed): blocking a network must bite a **realm**
    // exactly as it bites a peer, and take effect at once — otherwise a network
    // an operator shut out could re-enter as a bridge, or simply keep talking on
    // an already-bound session.
    let key = Keypair::generate();
    let (ctx, store) = ctx_plugin_store(
        vec![("insta", key.public(), vec!["instagram".parse().unwrap()])],
        &[],
    );

    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT instagram://acme-corp");
    plugin.send(&format!(
        "@title=Club;id={} NS-META instagram://acme-corp/club public",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::NsMeta { id, .. } = plugin.recv().await.event else {
        panic!("expected the minted NS-META");
    };
    let ns_id = id.to_string();
    plugin.send(&format!(
        "@vanity=general;id={} CHANNEL-LAYOUT instagram://acme-corp/club/general 0",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::ChannelLayout { channel, .. } = plugin.recv().await.event else {
        panic!("expected the minted CHANNEL-LAYOUT");
    };

    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@label=j1 NS JOIN {ns_id}"));
    drain_until_ns_member(&mut ada).await;
    assert!(matches!(
        weft_proto::Request::parse(&plugin.recv_raw().await)
            .unwrap()
            .command,
        weft_proto::Command::NsJoin { .. }
    ));

    // Ingestion works before the block.
    let before = format!("acme-corp/{}", ulid::Ulid::new());
    plugin.send(&format!(
        "@as=alice@acme-corp;msgid={before} MSG {channel} :before the block"
    ));
    let Event::Message(m) = ada.recv().await.event else {
        panic!("ada expected the pre-block message");
    };
    assert_eq!(m.body, "before the block");

    // The operator blocks the realm on an already-bound session.
    store
        .add_netblock(weft_store::NetblockRecord {
            network: "acme-corp".parse().unwrap(),
            reason: None,
            added_ms: 0,
            actor: "root".into(),
        })
        .await
        .unwrap();

    // Its traffic stops at once. The FIFO barrier: an unauthorized REALM REGISTER
    // right after must be the next — and only — thing we hear about.
    let after = format!("acme-corp/{}", ulid::Ulid::new());
    plugin.send(&format!(
        "@as=alice@acme-corp;msgid={after} MSG {channel} :after the block"
    ));
    ada.send("@label=p1 PING probe");
    assert!(matches!(
        ada.recv().await.event,
        Event::Pong { token: Some(t) } if t == "probe"
    ));

    // …and a fresh bind of the same realm is refused, so the block cannot be
    // shrugged off by reconnecting.
    let mut again = plugin_session(&ctx, &key).await;
    again.send("@label=r1 REALM ASSERT instagram://acme-corp");
    let reply = again.expect_err(ErrCode::Forbidden).await;
    assert_eq!(reply.label.as_deref(), Some("r1"));
}

#[tokio::test]
async fn the_realm_mints_its_ids_and_weftd_pins_them() {
    // §7a.0d (owner directive 2026-08-04: "fix the minting that the bridge mints
    // everything… mirror federation as much as possible"). Federation pins a
    // peer's ULIDs — `provision_replica` takes the manifest's channel name
    // verbatim — and never re-mints. A bridge is no different: the realm supplies
    // the ids, we pin them, so they survive our store and cost no round-trip.
    let key = Keypair::generate();
    let (ctx, _store) = ctx_plugin_store(
        vec![("insta", key.public(), vec!["instagram".parse().unwrap()])],
        &[],
    );

    let ns_id = ulid::Ulid::new().to_string().to_lowercase();
    let chan_id = ulid::Ulid::new().to_string().to_lowercase();

    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT instagram://acme-corp");
    plugin.send(&format!(
        "@title=Club;id={ns_id} NS-META instagram://acme-corp/club public"
    ));
    let Event::NsMeta { id, .. } = plugin.recv().await.event else {
        panic!("expected the NS-META mapping");
    };
    assert_eq!(id.to_string(), ns_id, "weftd must pin, not re-mint");

    plugin.send(&format!(
        "@vanity=general;id={chan_id} CHANNEL-LAYOUT instagram://acme-corp/club/general 0"
    ));
    let Event::ChannelLayout { channel, .. } = plugin.recv().await.event else {
        panic!("expected the CHANNEL-LAYOUT mapping");
    };
    assert_eq!(
        channel.to_string(),
        format!("#{ns_id}/{chan_id}"),
        "the canonical name is built from the realm's own ids"
    );

    // …so the adapter can address the channel it just asserted **without** having
    // waited for the mapping — the point of minting locally.
    let posted = format!("acme-corp/{}", ulid::Ulid::new());
    plugin.send(&format!(
        "@as=alice@acme-corp;msgid={posted} MSG #{ns_id}/{chan_id} :addressed by an id I minted"
    ));

    // An id already in use is refused rather than adopted — otherwise a provider
    // could assert a native namespace's ULID and take it over.
    plugin.send(&format!(
        "@title=Takeover;id={ns_id} NS-META instagram://acme-corp/other public"
    ));
    let reply = plugin.expect_err(ErrCode::Conflict).await;
    assert_eq!(err_context(&reply).as_deref(), Some("id"));
}

#[tokio::test]
async fn a_provider_mirrors_its_own_roles() {
    // Owner directive 2026-08-04: "For Discord the bridge should mirror roles.
    // For Matrix we implement custom power levels." So a realm whose foreign
    // system really has roles speaks the ordinary ROLE verbs as
    // `Actor::Provider` — the governing authority of its own namespaces —
    // while a levels-based realm uses bare GRANTs and `authority=levels`.
    let key = Keypair::generate();
    let (ctx, _store) = ctx_plugin_store(
        vec![("insta", key.public(), vec!["instagram".parse().unwrap()])],
        &[],
    );

    let ns_id = ulid::Ulid::new().to_string().to_lowercase();
    let chan_id = ulid::Ulid::new().to_string().to_lowercase();

    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT instagram://acme-corp");
    plugin.send(&format!(
        "@title=Club;id={ns_id} NS-META instagram://acme-corp/club public"
    ));
    assert!(matches!(plugin.recv().await.event, Event::NsMeta { .. }));
    plugin.send(&format!(
        "@vanity=general;id={chan_id} CHANNEL-LAYOUT instagram://acme-corp/club/general 0"
    ));
    assert!(matches!(
        plugin.recv().await.event,
        Event::ChannelLayout { .. }
    ));
    let channel = format!("#{ns_id}/{chan_id}");

    // The realm mirrors one of its own roles, with no local account behind it.
    plugin.send(&format!(
        "@label=rc ROLE CREATE ns:{ns_id} #5865f2 mute,ban :Moderator"
    ));
    let role = loop {
        match plugin.recv().await.event {
            Event::Role {
                role, name, caps, ..
            } => {
                assert_eq!(name, "Moderator");
                assert_eq!(caps, "mute,ban");
                break role;
            }
            Event::BatchStart { .. } | Event::BatchEnd { .. } => {}
            other => panic!("unexpected while awaiting ROLE: {other:?}"),
        }
    };

    // …and wears it on one of its own users. The role materializes into grants,
    // so the authority is real, not decorative.
    plugin.send(&format!(
        "@label=ra ROLE ASSIGN ns:{ns_id} carol@acme-corp {role}"
    ));
    loop {
        match plugin.recv().await.event {
            // Assignment materializes into grants — the Token is that ack.
            Event::Token { .. } => break,
            Event::BatchStart { .. } | Event::BatchEnd { .. } | Event::Role { .. } => {}
            other => panic!("unexpected while awaiting the grant ack: {other:?}"),
        }
    }

    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@label=j1 NS JOIN {ns_id}"));
    drain_until_ns_member(&mut ada).await;
    // Skip the role-batch acks still queued for the provider, then its join
    // request.
    loop {
        let raw = plugin.recv_raw().await;
        if matches!(
            weft_proto::Request::parse(&raw).map(|r| r.command),
            Ok(weft_proto::Command::NsJoin { .. })
        ) {
            break;
        }
    }

    plugin.send(&format!("@as=carol@acme-corp MUTE {channel} ada :spam"));
    let Event::Moderated { account, by, .. } = weft_proto::Reply::parse(&plugin.recv_raw().await)
        .unwrap()
        .event
    else {
        panic!("the role-wearing foreign moderator expected the MODERATED ack");
    };
    assert_eq!(account.to_string(), "ada");
    assert_eq!(by.as_deref(), Some("carol@acme-corp"));

    // A provider's authority stops at its own realms: it may not touch a scope
    // outside them, so network-wide roles are refused.
    plugin.send("@label=rx ROLE CREATE * #5865f2 mute :Global");
    let reply = plugin.expect_err(ErrCode::CapRequired).await;
    assert_eq!(reply.label.as_deref(), Some("rx"));
}

#[tokio::test]
async fn a_realm_declares_how_its_authority_should_be_rendered() {
    // §7a.3 (slice 8): a realm supplies a **capability profile** — how the client
    // should render its authority, and which native settings surfaces to hide.
    // Matrix sends `authority=levels` and disables the roles editor, because the
    // caps→levels direction is lossy: Matrix-side editing belongs to the
    // adapter's own Power Levels surface, not to a WEFT roles screen.
    let key = Keypair::generate();
    let (ctx, _store) = ctx_plugin_store(
        vec![("insta", key.public(), vec!["instagram".parse().unwrap()])],
        &[],
    );

    let ns_id = ulid::Ulid::new().to_string().to_lowercase();
    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT instagram://acme-corp");
    plugin.send(&format!(
        "@title=Club;id={ns_id};authority=levels;settings=roles,permissions          NS-META instagram://acme-corp/club public"
    ));
    let Event::NsMeta {
        authority,
        settings_disabled,
        ..
    } = plugin.recv().await.event
    else {
        panic!("expected the NS-META mapping");
    };
    assert_eq!(authority, Some(weft_proto::Authority::Levels));
    assert_eq!(settings_disabled, vec!["roles", "permissions"]);

    // It is stored, not just echoed — a member joining later sees it, which is
    // what actually gates their client's UI.
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@label=j1 NS JOIN {ns_id}"));
    drain_until_ns_member(&mut ada).await;

    ada.send("@label=d1 DISCOVER");
    let Event::NsMeta {
        authority,
        settings_disabled,
        ..
    } = ada.recv().await.event
    else {
        panic!("expected the replica in DISCOVER");
    };
    assert_eq!(authority, Some(weft_proto::Authority::Levels));
    assert!(settings_disabled.iter().any(|k| k == "roles"));

    // (A native namespace carries no profile at all — absent means the default,
    // roles authority with every surface enabled. Every other test in this suite
    // exercises that path, so nothing changes for ordinary servers.)
}

#[tokio::test]
async fn a_replicas_structure_belongs_to_its_realm() {
    // §7a.0e (owner directive 2026-08-04: every verb should route through the
    // bridge, so a local edit should fail anyway). Editing a replica here would
    // diverge with nothing to reconcile against, and silently — a re-assert
    // would not correct it. So local edits are refused, and re-asserting is how
    // the realm updates. The two are a pair: without the second, the first would
    // freeze a replica's metadata forever.
    let key = Keypair::generate();
    let (ctx, _store) = ctx_plugin_store(
        vec![("insta", key.public(), vec!["instagram".parse().unwrap()])],
        &["root"],
    );

    let ns_id = ulid::Ulid::new().to_string().to_lowercase();
    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT instagram://acme-corp");
    plugin.send(&format!(
        "@title=Club;id={ns_id} NS-META instagram://acme-corp/club public"
    ));
    let Event::NsMeta { title, .. } = plugin.recv().await.event else {
        panic!("expected the NS-META mapping");
    };
    assert_eq!(title.as_deref(), Some("Club"));

    // The realm appoints a local admin — real authority, granted by the only
    // party that governs a replica. (A network operator would hold nothing here:
    // operator power lives in a separate table and acts through the web admin
    // panel, never as wire capability inside a namespace.)
    let mut root = ready(&ctx, "root").await;
    plugin.send(&format!("GRANT root ns:{ns_id} ns-admin,chan-create"));
    let ack = weft_proto::Reply::parse(&plugin.recv_raw().await).unwrap();
    assert!(matches!(ack.event, Event::Token { .. }), "got {ack:?}");

    // …and even *they* may not edit its structure: that belongs to the realm, and
    // a local edit would diverge with nothing to reconcile against.
    root.send(&format!("@label=m1 NS META {ns_id} title :Renamed By Hand"));
    let reply = root.expect_err(ErrCode::Forbidden).await;
    assert_eq!(err_context(&reply).as_deref(), Some("provider-managed"));

    root.send(&format!("@label=v1 NS VISIBILITY {ns_id} unlisted"));
    let reply = root.expect_err(ErrCode::Forbidden).await;
    assert_eq!(err_context(&reply).as_deref(), Some("provider-managed"));

    root.send(&format!("@label=c1 CHANNEL CREATE #{ns_id}/handmade"));
    let reply = root.expect_err(ErrCode::Forbidden).await;
    assert_eq!(err_context(&reply).as_deref(), Some("provider-managed"));

    // …but the realm renames it by re-asserting, which keeps both sides equal.
    plugin.send(&format!(
        "@title=RenamedUpstream;id={ns_id} NS-META instagram://acme-corp/club public"
    ));
    let Event::NsMeta { title, .. } = plugin.recv().await.event else {
        panic!("expected the updated NS-META");
    };
    assert_eq!(title.as_deref(), Some("RenamedUpstream"));

    // Deleting still works — that is the whole point of the operator hatch.
    // NS DELETE takes the id twice as its confirmation.
    root.send(&format!("@label=d1 NS DELETE {ns_id} {ns_id}"));
    let reply = root.recv().await;
    assert_eq!(reply.label.as_deref(), Some("d1"));
    assert!(
        matches!(reply.event, Event::NsMeta { .. }),
        "an orphaned replica must stay deletable, got {reply:?}"
    );
}

#[tokio::test]
async fn a_multi_step_flow_routes_and_stays_the_callers_own() {
    // M-plug-3: a flow is more than one round trip — the plugin answers an invoke
    // with a *view*, the client submits it, and the plugin answers again. weftd
    // routes each step by view-id and re-points the echo label so every step acks
    // itself rather than the invoke that opened it.
    let key = Keypair::generate();
    let ctx = ctx_plugin("modq", &key.public());

    let mut plugin = plugin_session(&ctx, &key).await;
    let reg = weft_proto::Registration {
        api: 1,
        id: "modq".into(),
        name: "Mod Queue".into(),
        icon: None,
        actions: vec![weft_proto::ActionDecl {
            id: "open".into(),
            label: "Open".into(),
            icon: None,
            surface: weft_proto::Surface::Global,
            context: weft_proto::ContextType::None,
            description: None,
            visibility: None,
            input: vec![],
        }],
        hooks: vec![],
        bot: None,
        schemes: vec![],
    };
    plugin.send(&format!(
        "PLUGIN-REGISTER :{}",
        weft_proto::plugin_to_b64(&reg).unwrap()
    ));

    let mut ada = ready(&ctx, "ada").await;
    wait_for_action(&mut ada, "modq", "open").await;

    ada.send("@label=i1 PLUGIN INVOKE modq open");
    let req = weft_proto::Request::parse(&plugin.recv_raw().await).unwrap();
    let view_id = req.label.expect("the view-id rides as the label");

    // Step 1: the plugin shows a form, acked with the invoke's label.
    let view = weft_proto::plugin_to_b64(&weft_proto::View {
        container: weft_proto::Container::Modal,
        title: Some("Ban".into()),
        panel_key: None,
        submit_label: None,
        blocks: vec![],
        widget: None,
        params: vec![],
    })
    .unwrap();
    plugin.send(&format!("PLUGIN-VIEW {view_id} :{view}"));
    let reply = ada.recv().await;
    assert_eq!(reply.label.as_deref(), Some("i1"));
    assert!(matches!(reply.event, Event::PluginView { .. }));

    // Step 2: ada submits it. weftd routes the step to the plugin, still keyed by
    // view-id, and the plugin's next answer acks *this* label — not `i1`.
    ada.send(&format!("@label=s1 PLUGIN SUBMIT {view_id}"));
    let step = weft_proto::Request::parse(&plugin.recv_raw().await).unwrap();
    assert!(matches!(
        step.command,
        weft_proto::Command::PluginSubmit { .. }
    ));
    assert_eq!(step.label.as_deref(), Some(view_id.as_str()));

    let result = weft_proto::plugin_to_b64(&weft_proto::ViewResult::Toast {
        kind: weft_proto::ToastKind::Ok,
        text: "banned".into(),
    })
    .unwrap();
    plugin.send(&format!("PLUGIN-RESULT {view_id} :{result}"));
    let reply = ada.recv().await;
    assert_eq!(
        reply.label.as_deref(),
        Some("s1"),
        "each step acks itself, not the invoke that opened the flow"
    );

    // …and once the plugin answers terminally, the flow is over: its parking is
    // freed, so a further step finds nothing rather than lingering forever.
    ada.send(&format!("@label=s3 PLUGIN SUBMIT {view_id}"));
    ada.expect_err(ErrCode::NoSuchTarget).await;
}

#[tokio::test]
async fn the_admin_panel_drives_a_flow_but_cannot_touch_a_sessions() {
    // plugin-spec §22: the panel is HTTP request/response with no session, so
    // its steps go through `admin_plugin_invoke`/`admin_plugin_step` — parked
    // by view-id like any flow, awaited inline. Ownership is the invariant
    // under test: a step re-parks the view's reply slot, so the panel must be
    // refused a session-owned view (it would steal the session's answer).
    let key = Keypair::generate();
    let ctx = ctx_plugin("modq", &key.public());

    let mut plugin = plugin_session(&ctx, &key).await;
    let reg = weft_proto::Registration {
        api: 1,
        id: "modq".into(),
        name: "Mod Queue".into(),
        icon: None,
        actions: vec![weft_proto::ActionDecl {
            id: "bans".into(),
            label: "Bridged spaces".into(),
            icon: None,
            surface: weft_proto::Surface::Admin,
            context: weft_proto::ContextType::None,
            description: None,
            visibility: None,
            input: vec![],
        }],
        hooks: vec![],
        bot: None,
        schemes: vec![],
    };
    plugin.send(&format!(
        "PLUGIN-REGISTER :{}",
        weft_proto::plugin_to_b64(&reg).unwrap()
    ));

    // Invoke: the panel waits, so the answer must come from a concurrent task.
    let invoke = tokio::spawn({
        let ctx = Arc::clone(&ctx);
        async move { ctx.admin_plugin_invoke("modq", "bans", None, None).await }
    });
    let req = weft_proto::Request::parse(&plugin.recv_raw().await).unwrap();
    let view_id = req.label.expect("the view-id rides as the label");

    let view = weft_proto::plugin_to_b64(&weft_proto::View {
        container: weft_proto::Container::Panel,
        title: Some("Bridged spaces".into()),
        panel_key: None,
        submit_label: None,
        blocks: vec![],
        widget: None,
        params: vec![],
    })
    .unwrap();
    plugin.send(&format!("PLUGIN-VIEW {view_id} :{view}"));

    let (got_id, answer) = invoke.await.unwrap().expect("the invoke answers");
    assert_eq!(got_id, view_id);
    assert!(answer.contains("PLUGIN-VIEW"));

    // A step drives the same flow; the plugin sees a PLUGIN ACTION labeled by
    // the view-id, and its terminal result resolves the step.
    let step = tokio::spawn({
        let ctx = Arc::clone(&ctx);
        let view_id = view_id.clone();
        async move {
            ctx.admin_plugin_step(&view_id, Some("ban".into()), None)
                .await
        }
    });
    let req = weft_proto::Request::parse(&plugin.recv_raw().await).unwrap();
    assert!(matches!(
        req.command,
        weft_proto::Command::PluginAction { .. }
    ));
    assert_eq!(req.label.as_deref(), Some(view_id.as_str()));

    let result = weft_proto::plugin_to_b64(&weft_proto::ViewResult::Toast {
        kind: weft_proto::ToastKind::Ok,
        text: "banned".into(),
    })
    .unwrap();
    plugin.send(&format!("PLUGIN-RESULT {view_id} :{result}"));
    let answer = step.await.unwrap().expect("the step answers");
    assert!(answer.contains("PLUGIN-RESULT"));

    // The result was terminal — the flow is gone, so a further step is refused
    // immediately (no send, no timeout) rather than hanging on a dead view.
    assert!(ctx.admin_plugin_step(&view_id, None, None).await.is_none());

    // Ownership: ada opens her own flow; the panel must not be able to step
    // (and thereby re-park) it — and ada's flow keeps working afterwards.
    let mut ada = ready(&ctx, "ada").await;
    wait_for_action(&mut ada, "modq", "bans").await;
    ada.send("@label=i1 PLUGIN INVOKE modq bans");
    let req = weft_proto::Request::parse(&plugin.recv_raw().await).unwrap();
    let ada_view = req.label.expect("ada's view-id");
    plugin.send(&format!("PLUGIN-VIEW {ada_view} :{view}"));
    assert!(matches!(ada.recv().await.event, Event::PluginView { .. }));

    assert!(
        ctx.admin_plugin_step(&ada_view, None, None).await.is_none(),
        "the panel must be refused a session-owned view"
    );

    ada.send(&format!("@label=s1 PLUGIN SUBMIT {ada_view}"));
    let req = weft_proto::Request::parse(&plugin.recv_raw().await).unwrap();
    assert!(
        matches!(req.command, weft_proto::Command::PluginSubmit { .. }),
        "ada's flow is untouched"
    );

    // Close: a fresh panel flow, dismissed — the plugin is told, and the flow
    // is freed.
    let invoke = tokio::spawn({
        let ctx = Arc::clone(&ctx);
        async move { ctx.admin_plugin_invoke("modq", "bans", None, None).await }
    });
    let req = weft_proto::Request::parse(&plugin.recv_raw().await).unwrap();
    let view_id2 = req.label.expect("the view-id rides as the label");
    plugin.send(&format!("PLUGIN-VIEW {view_id2} :{view}"));
    invoke.await.unwrap().expect("the invoke answers");

    ctx.admin_plugin_close(&view_id2);
    let req = weft_proto::Request::parse(&plugin.recv_raw().await).unwrap();
    assert!(matches!(
        req.command,
        weft_proto::Command::PluginClose { .. }
    ));
    assert!(ctx.admin_plugin_step(&view_id2, None, None).await.is_none());
}

#[tokio::test]
async fn a_live_panel_is_patched_by_key_and_only_while_watched() {
    // §11.3 (slice 9): a panel is persistent and the plugin pushes to it
    // unsolicited. It cannot know each open copy's view-id, so it patches by the
    // `panel_key` it chose — and weftd resolves that to whoever is actually
    // subscribed. A closed panel is a no-op, not a delivery to a client that
    // isn't showing it.
    let key = Keypair::generate();
    let ctx = ctx_plugin("modq", &key.public());

    let mut plugin = plugin_session(&ctx, &key).await;
    let reg = weft_proto::Registration {
        api: 1,
        id: "modq".into(),
        name: "Mod Queue".into(),
        icon: None,
        actions: vec![weft_proto::ActionDecl {
            id: "queue".into(),
            label: "Queue".into(),
            icon: None,
            surface: weft_proto::Surface::Settings,
            context: weft_proto::ContextType::Namespace,
            description: None,
            visibility: None,
            input: vec![],
        }],
        hooks: vec![],
        bot: None,
        schemes: vec![],
    };
    plugin.send(&format!(
        "PLUGIN-REGISTER :{}",
        weft_proto::plugin_to_b64(&reg).unwrap()
    ));

    // Two clients open the same panel — same key, different view-ids.
    let mut ada = ready(&ctx, "ada").await;
    let mut bob = ready(&ctx, "bob").await;
    wait_for_action(&mut ada, "modq", "queue").await;
    wait_for_action(&mut bob, "modq", "queue").await;

    let panel = weft_proto::plugin_to_b64(&weft_proto::View {
        container: weft_proto::Container::Panel,
        title: Some("Reports".into()),
        panel_key: Some("reports".into()),
        submit_label: None,
        blocks: vec![],
        widget: None,
        params: vec![],
    })
    .unwrap();

    let mut views = Vec::new();
    for client in [&mut ada, &mut bob] {
        client.send("@label=i1 PLUGIN INVOKE modq queue");
        let view_id = weft_proto::Request::parse(&plugin.recv_raw().await)
            .unwrap()
            .label
            .expect("the view-id rides as the label");
        plugin.send(&format!("PLUGIN-VIEW {view_id} :{panel}"));
        assert!(matches!(
            client.recv().await.event,
            Event::PluginView { .. }
        ));

        client.send(&format!("@label=sub PLUGIN SUBSCRIBE {view_id}"));
        assert!(matches!(
            weft_proto::Request::parse(&plugin.recv_raw().await)
                .unwrap()
                .command,
            weft_proto::Command::PluginSubscribe { .. }
        ));
        views.push(view_id);
    }

    // One patch addressed to the KEY reaches both open copies, unlabelled — a
    // push is unsolicited, so it acks nothing (§12.4).
    let patch = weft_proto::plugin_to_b64(&vec![weft_proto::PatchOp::Remove {
        component_id: "row-1".into(),
    }])
    .unwrap();
    plugin.send(&format!("PLUGIN-PATCH reports :{patch}"));

    for client in [&mut ada, &mut bob] {
        let reply = client.recv().await;
        assert_eq!(reply.label, None, "an unsolicited push carries no label");
        assert!(matches!(reply.event, Event::PluginPatch { .. }));
    }

    // Bob closes his panel. A patch to the same key now reaches only ada — a
    // closed key is a no-op for the client that closed it.
    bob.send(&format!("@label=un PLUGIN UNSUBSCRIBE {}", views[1]));
    assert!(matches!(
        weft_proto::Request::parse(&plugin.recv_raw().await)
            .unwrap()
            .command,
        weft_proto::Command::PluginUnsubscribe { .. }
    ));

    plugin.send(&format!("PLUGIN-PATCH reports :{patch}"));
    assert!(matches!(ada.recv().await.event, Event::PluginPatch { .. }));

    // The FIFO barrier: bob's next line is his own PONG, not a stray patch.
    bob.send("@label=p PING probe");
    assert!(matches!(
        bob.recv().await.event,
        Event::Pong { token: Some(t) } if t == "probe"
    ));
}

#[tokio::test]
async fn a_flow_cannot_be_driven_by_anyone_but_its_caller() {
    // A view-id is a plugin name and a counter, so it is guessable. Without an
    // ownership check any session could drive, read or dismiss another user's
    // dialog. A view that is not yours is refused exactly as one that does not
    // exist (invariant 1) — same code, no branch revealing which.
    let key = Keypair::generate();
    let ctx = ctx_plugin("modq", &key.public());

    let mut plugin = plugin_session(&ctx, &key).await;
    let reg = weft_proto::Registration {
        api: 1,
        id: "modq".into(),
        name: "Mod Queue".into(),
        icon: None,
        actions: vec![weft_proto::ActionDecl {
            id: "open".into(),
            label: "Open".into(),
            icon: None,
            surface: weft_proto::Surface::Global,
            context: weft_proto::ContextType::None,
            description: None,
            visibility: None,
            input: vec![],
        }],
        hooks: vec![],
        bot: None,
        schemes: vec![],
    };
    plugin.send(&format!(
        "PLUGIN-REGISTER :{}",
        weft_proto::plugin_to_b64(&reg).unwrap()
    ));

    let mut ada = ready(&ctx, "ada").await;
    wait_for_action(&mut ada, "modq", "open").await;
    ada.send("@label=i1 PLUGIN INVOKE modq open");
    let view_id = weft_proto::Request::parse(&plugin.recv_raw().await)
        .unwrap()
        .label
        .expect("the view-id rides as the label");

    let mut mallory = ready(&ctx, "mallory").await;
    for (label, line) in [
        ("x1", format!("PLUGIN SUBMIT {view_id}")),
        ("x2", format!("PLUGIN ACTION {view_id} confirm")),
        ("x3", format!("PLUGIN SUBSCRIBE {view_id}")),
        ("x4", format!("PLUGIN CLOSE {view_id}")),
    ] {
        mallory.send(&format!("@label={label} {line}"));
        let reply = mallory.expect_err(ErrCode::NoSuchTarget).await;
        assert_eq!(reply.label.as_deref(), Some(label), "{line}");
    }

    // The owner's flow is untouched by the attempts: her next step still routes.
    ada.send(&format!("@label=s1 PLUGIN SUBMIT {view_id}"));
    let step = weft_proto::Request::parse(&plugin.recv_raw().await).unwrap();
    assert_eq!(step.label.as_deref(), Some(view_id.as_str()));

    // CLOSE is terminal for the owner too — a dismissed view stops existing,
    // rather than pinning her writer for the life of the session.
    ada.send(&format!("@label=c1 PLUGIN CLOSE {view_id}"));
    let closed = weft_proto::Request::parse(&plugin.recv_raw().await).unwrap();
    assert!(matches!(
        closed.command,
        weft_proto::Command::PluginClose { .. }
    ));

    ada.send(&format!("@label=s2 PLUGIN SUBMIT {view_id}"));
    ada.expect_err(ErrCode::NoSuchTarget).await;
}

#[tokio::test]
async fn a_control_link_provider_governs_what_it_registered() {
    // A provider's authority is bounded by the schemes it **registered**, not by
    // whichever realm it happens to have bound. A control link (`REALM REGISTER`,
    // no `REALM ASSERT`) serves its schemes just the same, so it must be able to
    // govern its namespaces — and a provider serving several schemes must not be
    // limited to one of them.
    let key = Keypair::generate();
    let (ctx, _store) = ctx_plugin_store(
        vec![(
            "multi",
            key.public(),
            vec!["instagram".parse().unwrap(), "discord".parse().unwrap()],
        )],
        &[],
    );

    // One session binds a realm and asserts a namespace under it…
    let ns_id = ulid::Ulid::new().to_string().to_lowercase();
    let mut binder = plugin_session(&ctx, &key).await;
    binder.send("REALM ASSERT discord://guild-1");
    binder.send(&format!(
        "@title=Guild;id={ns_id} NS-META discord://guild-1/space public"
    ));
    assert!(matches!(binder.recv().await.event, Event::NsMeta { .. }));

    // …and a *separate* control link, which never bound any realm, governs it
    // too. Before the fix this session carried a placeholder scheme and could
    // do nothing — and worse, a namespace whose origin scheme really was the
    // placeholder would have been governed by any unbound provider.
    let mut control = plugin_session(&ctx, &key).await;
    control.send("REALM REGISTER discord");
    let mut ada = ready(&ctx, "ada").await;
    let _ = &mut ada;

    control.send(&format!("@label=g1 GRANT ada ns:{ns_id} mute"));
    let reply = recv_past_membership_statement(&mut control).await;
    assert!(
        matches!(reply.event, Event::Token { .. }),
        "a registered scheme is authority enough, got {reply:?}"
    );

    // Its reach still stops at what it registered: a namespace of some other
    // scheme is not its to govern.
    let mut other = plugin_session(&ctx, &key).await;
    other.send("REALM ASSERT instagram://acme-corp");
    other.send(&format!(
        "@title=Club;id={} NS-META instagram://acme-corp/club public",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    assert!(matches!(other.recv().await.event, Event::NsMeta { .. }));
}

#[tokio::test]
async fn authority_translates_both_ways_with_the_provider() {
    // Owner directive 2026-08-04: a WEFT user must be able to be made a mod on a
    // Matrix space and vice versa, so the bridge translates power levels. weftd
    // stays free of any notion of a level — it speaks capabilities in both
    // directions and the adapter owns the mapping.
    let key = Keypair::generate();
    let ctx = ctx_plugin_full(
        vec![("insta", key.public(), vec!["instagram".parse().unwrap()])],
        &["root"],
    );

    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT instagram://acme-corp");
    plugin.send(&format!(
        "@title=Club;id={} NS-META instagram://acme-corp/club public",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::NsMeta { id, .. } = plugin.recv().await.event else {
        panic!("expected the minted NS-META");
    };
    let ns_id = id.to_string();
    plugin.send(&format!(
        "@vanity=general;id={} CHANNEL-LAYOUT instagram://acme-corp/club/general 0",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::ChannelLayout { channel, .. } = plugin.recv().await.event else {
        panic!("expected the minted CHANNEL-LAYOUT");
    };

    // INBOUND — a moderator on the foreign side becomes one here. The provider
    // maps its power level to WEFT capabilities and grants them; the subject is
    // a *foreign* user, keyed by `user@realm`.
    plugin.send(&format!("GRANT carol@acme-corp ns:{ns_id} mute,ban"));

    // …and it is real authority, not decoration: carol can mute, which needs the
    // `mute` cap at a scope covering the channel.
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@label=j1 NS JOIN {ns_id}"));
    drain_until_ns_member(&mut ada).await;
    // The inbound GRANT went through the *ordinary* handler, so it acked with a
    // Token like any other; skip it, then take the join request.
    loop {
        let raw = plugin.recv_raw().await;
        if matches!(
            weft_proto::Request::parse(&raw).map(|r| r.command),
            Ok(weft_proto::Command::NsJoin { .. })
        ) {
            break;
        }
    }

    // Carol mutes ada. `MODERATED` acks the moderator, so the proof that the
    // authority is real is the *effect*: ada can no longer post.
    plugin.send(&format!("@as=carol@acme-corp MUTE {channel} ada :spam"));
    let Event::Moderated { account, by, .. } = weft_proto::Reply::parse(&plugin.recv_raw().await)
        .unwrap()
        .event
    else {
        panic!("the foreign moderator expected the MODERATED ack");
    };
    assert_eq!(account.to_string(), "ada");
    assert_eq!(by.as_deref(), Some("carol@acme-corp"));

    ada.send(&format!("@label=m1 MSG {channel} :am i muted"));
    let reply = ada.expect_err(ErrCode::Forbidden).await;
    assert_eq!(reply.label.as_deref(), Some("m1"));
    assert_eq!(err_context(&reply).as_deref(), Some("muted"));

    // And it is the *grant* that confers it, not merely being foreign: a foreign
    // user the provider never granted anything is refused like anyone else.
    plugin.send(&format!("@as=dave@acme-corp MUTE {channel} ada :no rights"));
    plugin.expect_err(ErrCode::CapRequired).await;

    // OUTBOUND — promoting a WEFT user here raises their foreign power level.
    // The realm grants the local admin their authority first: being a network
    // operator confers nothing inside a namespace (operators act through the web
    // admin panel), so a replica's local admins are the realm's to appoint.
    let mut root_op = ready(&ctx, "root").await;
    plugin.send(&format!(
        "GRANT root ns:{ns_id} ns-admin,grant:mute,grant:ban"
    ));
    // The Token acks the *granting* session — read it so the grant is applied
    // before root acts on it.
    loop {
        match weft_proto::Reply::parse(&plugin.recv_raw().await).map(|r| r.event) {
            Ok(Event::Token { .. }) => break,
            _ => continue,
        }
    }

    root_op.send(&format!("@label=g1 GRANT ada ns:{ns_id} mute"));
    let relayed = weft_proto::Request::parse(&plugin.recv_raw().await).unwrap();
    let weft_proto::Command::Grant {
        subject,
        scope,
        caps,
        ..
    } = relayed.command
    else {
        panic!("provider expected the relayed GRANT");
    };
    assert_eq!(subject, "ada");
    assert_eq!(scope, format!("ns:{ns_id}"));
    assert_eq!(caps, "mute");

    // …and demoting relays too.
    root_op.send(&format!("@label=r1 REVOKE ada ns:{ns_id} caps=mute"));
    let relayed = weft_proto::Request::parse(&plugin.recv_raw().await).unwrap();
    let weft_proto::Command::Revoke { subject, caps, .. } = relayed.command else {
        panic!("provider expected the relayed REVOKE");
    };
    assert_eq!(subject, "ada");
    assert_eq!(caps.as_deref(), Some("mute"));

    // A **role** relays too, because a role is a labelled bundle that
    // materializes into grants (`ROLE ASSIGN` → `on_grant`) — promoting someone
    // to Moderator here must raise their level there. Only `@everyone` doesn't,
    // since it is resolved live and never becomes a grant.
    root_op.send(&format!(
        "@label=rc ROLE CREATE ns:{ns_id} #e8b93d mute,ban :Moderator"
    ));
    let role = loop {
        match root_op.recv().await.event {
            Event::Role { role, .. } => break role,
            // The GRANT/REVOKE above each acked with a re-minted Token.
            Event::BatchStart { .. } | Event::BatchEnd { .. } | Event::Token { .. } => {}
            other => panic!("unexpected while awaiting ROLE: {other:?}"),
        }
    };
    root_op.send(&format!("@label=ra ROLE ASSIGN ns:{ns_id} ada {role}"));

    let relayed = weft_proto::Request::parse(&plugin.recv_raw().await).unwrap();
    let weft_proto::Command::Grant { subject, caps, .. } = relayed.command else {
        panic!("provider expected the role's caps relayed as a GRANT");
    };
    assert_eq!(subject, "ada");
    assert_eq!(caps, "mute,ban");
}

#[tokio::test]
async fn provider_offline_gates_virtual_namespace() {
    // Owner directive 2026-08-04: a virtual namespace is online only while its
    // provider is — offline ⇒ undiscoverable + unjoinable; members get live
    // provider=offline/online NS-META pushes.
    let key = Keypair::generate();
    let ctx = ctx_plugin_schemes("insta", &key.public(), vec!["instagram".parse().unwrap()]);

    let register = |c: &Client| {
        let reg = weft_proto::Registration {
            api: 1,
            id: "insta".into(),
            name: "Instagram Bridge".into(),
            icon: None,
            actions: vec![],
            hooks: vec![],
            bot: None,
            schemes: vec!["instagram".into()],
        };
        c.send(&format!(
            "PLUGIN-REGISTER :{}",
            weft_proto::plugin_to_b64(&reg).unwrap()
        ));
    };

    // Provider online: assert a public virtual namespace directly (capability 4 —
    // no provisioning flow needed), then a member joins it.
    let mut plugin = plugin_session(&ctx, &key).await;
    register(&plugin);
    plugin.send(&format!(
        "@title=Club;id={} NS-META instagram://acme-corp/club public",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::NsMeta {
        id,
        provider_online,
        ..
    } = plugin.recv().await.event
    else {
        panic!("expected the minted NS-META mapping");
    };
    assert_eq!(provider_online, Some(true));
    let ns_id = id.to_string();
    plugin.send(&format!(
        "@vanity=general;id={} CHANNEL-LAYOUT instagram://acme-corp/club/general 0",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::ChannelLayout { channel, .. } = plugin.recv().await.event else {
        panic!("expected the minted CHANNEL-LAYOUT");
    };

    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@label=j1 NS JOIN {ns_id}"));
    drain_until_ns_member(&mut ada).await;

    // Online: she can post into the replica. The realm is the source of truth
    // there, so the post is relayed out and comes back with the id IT minted —
    // that returning copy, not a local echo, is what she sees.
    ada.send(&format!("@label=m0 MSG {channel} :while online"));
    // Her relayed NS JOIN is still queued ahead of it on this session.
    let bridge_label = loop {
        let line = weft_proto::Line::parse(&plugin.recv_raw().await).unwrap();

        if let Ok(weft_proto::Command::Msg { .. }) =
            weft_proto::Request::from_line(&line).map(|r| r.command)
        {
            break line.tags.get("label").expect("a bridge label").clone();
        }
    };
    let root: weft_proto::MsgId = format!("acme-corp/{}", ulid::Ulid::new()).parse().unwrap();
    plugin.send(&format!(
        "@as=ada@test.example;msgid={root};label={bridge_label} MSG {channel} :while online"
    ));
    let Event::Message(posted) = ada.recv().await.event else {
        panic!("expected her post back, minted by the realm");
    };
    assert_eq!(posted.msgid, root);

    // Online: DISCOVER lists the public replica.
    ada.send("DISCOVER");
    let Event::NsMeta { origin, .. } = ada.recv().await.event else {
        panic!("expected the replica in DISCOVER while online");
    };
    assert!(origin.is_some());

    // The provider dies → the member gets the live offline indicator.
    drop(plugin);
    let Event::NsMeta {
        provider_online, ..
    } = ada.recv().await.event
    else {
        panic!("ada expected the provider-offline NS-META push");
    };
    assert_eq!(provider_online, Some(false));

    // 5b (owner decision 2026-08-04): **posting is refused while the provider is
    // offline** rather than accepted-and-dropped. Accepting it would split-brain
    // the room — local members would see a message the foreign side never gets,
    // with no route out and nothing to reconcile against later.
    ada.send(&format!("@label=m1 MSG {channel} :into the void"));
    let reply = ada.expect_err(ErrCode::Policy).await;
    assert_eq!(reply.label.as_deref(), Some("m1"));
    assert_eq!(err_context(&reply).as_deref(), Some("provider-offline"));

    // The same for every mutation — the foreign side is authoritative for its
    // own rooms, so we take no write we cannot deliver to it. Her own message,
    // so authorship is not what refuses these.
    for (label, cmd) in [
        ("e1", format!("EDIT {root} :rewritten")),
        ("r1", format!("REACT {root} wave")),
        ("u1", format!("UNREACT {root} wave")),
        ("d1", format!("DELETE {root}")),
    ] {
        ada.send(&format!("@label={label} {cmd}"));
        let reply = ada.expect_err(ErrCode::Policy).await;
        assert_eq!(reply.label.as_deref(), Some(label), "{cmd}");
        // The context proves it is the bridge gate refusing, not some other
        // POLICY rule that happens to share the code.
        assert_eq!(
            err_context(&reply).as_deref(),
            Some("provider-offline"),
            "{cmd}"
        );
    }

    // Offline: unjoinable (uniform NO-SUCH-TARGET) and undiscoverable (a DISCOVER
    // yields nothing; the FIFO PING proves the silence).
    let mut bob = ready(&ctx, "bob").await;
    bob.send(&format!("@label=j2 NS JOIN {ns_id}"));
    let reply = bob.expect_err(ErrCode::NoSuchTarget).await;
    assert_eq!(reply.label.as_deref(), Some("j2"));
    bob.send("DISCOVER");
    bob.send("PING probe");
    assert!(matches!(
        bob.recv().await.event,
        Event::Pong { token: Some(t) } if t == "probe"
    ));

    // The provider reconnects + re-registers → members get the online push and
    // the namespace is joinable + discoverable again.
    let plugin2 = plugin_session(&ctx, &key).await;
    register(&plugin2);
    let Event::NsMeta {
        provider_online, ..
    } = ada.recv().await.event
    else {
        panic!("ada expected the provider-online NS-META push");
    };
    assert_eq!(provider_online, Some(true));
    bob.send(&format!("@label=j3 NS JOIN {ns_id}"));
    loop {
        let reply = bob.recv().await;
        match reply.event {
            Event::NsMember {
                action: MemberAction::Join,
                ..
            } => {
                assert_eq!(reply.label.as_deref(), Some("j3"));
                break;
            }
            // The bulk channel subscription burst precedes the ns-level ack.
            Event::Member { .. } | Event::Policy { .. } => {}
            other => panic!("unexpected before NS-MEMBER: {other:?}"),
        }
    }
}

#[tokio::test]
async fn provider_death_fails_parked_requests_loudly() {
    // A provider that dies with work in flight must FAIL its parked clients —
    // silence is never the failure mode (§3.5). A parked foreign join answers
    // NO-SUCH-TARGET; a parked invocation answers INTERNAL (spec §16).
    let key = Keypair::generate();
    let ctx = ctx_plugin_schemes("insta", &key.public(), vec!["instagram".parse().unwrap()]);

    let mut plugin = plugin_session(&ctx, &key).await;
    let reg = weft_proto::Registration {
        api: 1,
        id: "insta".into(),
        name: "Instagram Bridge".into(),
        icon: None,
        actions: vec![weft_proto::ActionDecl {
            id: "open".into(),
            label: "Open".into(),
            icon: None,
            surface: weft_proto::Surface::Global,
            context: weft_proto::ContextType::None,
            description: None,
            visibility: None,
            input: vec![],
        }],
        hooks: vec![],
        bot: None,
        schemes: vec!["instagram".into()],
    };
    plugin.send(&format!(
        "PLUGIN-REGISTER :{}",
        weft_proto::plugin_to_b64(&reg).unwrap()
    ));

    let mut joiner = ready(&ctx, "ada").await;
    wait_for_action(&mut joiner, "insta", "open").await;
    let mut invoker = ready(&ctx, "bob").await;

    // Park a provision and an invocation on the provider.
    joiner.send("@label=j1 NS JOIN instagram://acme-corp");
    assert!(matches!(
        weft_proto::Reply::parse(&plugin.recv_raw().await)
            .unwrap()
            .event,
        Event::Provision { .. }
    ));
    invoker.send("@label=i1 PLUGIN INVOKE insta open");
    plugin.recv_raw().await; // the routed invoke arrived — both are now parked

    // The provider dies. Both parked clients get loud, labeled completions.
    drop(plugin);
    let reply = joiner.expect_err(ErrCode::NoSuchTarget).await;
    assert_eq!(reply.label.as_deref(), Some("j1"));
    let reply = invoker.expect_err(ErrCode::Internal).await;
    assert_eq!(reply.label.as_deref(), Some("i1"));
}

#[tokio::test]
async fn plugin_invoke_unknown_action_is_no_such_target() {
    let key = Keypair::generate();
    let ctx = ctx_plugin("modq", &key.public());
    let _plugin = plugin_session(&ctx, &key).await; // registered nothing

    let mut client = ready(&ctx, "ada").await;
    client.send("@label=i1 PLUGIN INVOKE modq nope");
    let reply = client.expect_err(ErrCode::NoSuchTarget).await;
    assert_eq!(reply.label.as_deref(), Some("i1"));
}

#[tokio::test]
async fn foreign_bridge_realm_scheme_authorization() {
    let key = Keypair::generate();
    let ctx = ctx_adapter("matrix", &key.public());
    let mut c = adapter_session(&ctx, &key).await;

    // An authorized REALM ASSERT for the adapter's scheme binds the connection
    // *silently*; a REALM ASSERT for a scheme the key is not pinned for is
    // refused. Because a successful bind emits nothing, the single ERR we read
    // must carry the *second* command's label — which also proves the first was
    // silent.
    c.send("@label=ok REALM ASSERT matrix://matrix.org");
    c.send("@label=bad REALM ASSERT discord://evil.example");
    let reply = c.expect_err(ErrCode::Unsupported).await;
    assert_eq!(reply.label.as_deref(), Some("bad"));
}

#[tokio::test]
async fn foreign_bridge_ns_join_provisions_then_errs() {
    let key = Keypair::generate();
    let ctx = ctx_adapter("matrix", &key.public());

    // The adapter authenticates and registers its scheme on the control link.
    let mut adapter = adapter_session(&ctx, &key).await;
    adapter.send("REALM REGISTER matrix");
    // REALM REGISTER is silent, so probe with an *unauthorized* register whose ERR
    // proves (FIFO) the `matrix` registration was already processed.
    adapter.send("@label=probe REALM REGISTER discord");
    let probe = adapter.expect_err(ErrCode::Unsupported).await;
    assert_eq!(probe.label.as_deref(), Some("probe"));

    // A client joins a foreign namespace → weftd provisions via the adapter.
    let mut client = ready(&ctx, "ada").await;
    client.send("@label=j1 NS JOIN matrix://matrix.org/gaming");

    // The adapter receives PROVISION with a correlation job on its control link.
    let Event::Provision { uri, job } = adapter.recv().await.event else {
        panic!("adapter expected a PROVISION push");
    };
    assert_eq!(uri.to_string(), "matrix://matrix.org/gaming");

    // The adapter reports the space is unreachable.
    adapter.send(&format!("PROVISION-ERR {job}"));

    // The parked NS JOIN completes as the uniform NO-SUCH-TARGET (invariant 4),
    // echoing the join's label.
    let reply = client.expect_err(ErrCode::NoSuchTarget).await;
    assert_eq!(reply.label.as_deref(), Some("j1"));
}

#[tokio::test]
async fn foreign_bridge_ns_join_no_adapter_is_no_such_target() {
    // The adapter is pinned in config but never connected/registered, so no
    // control link exists for `matrix` — a foreign NS JOIN is indistinguishable
    // from a nonexistent local namespace (invariant 1/4).
    let key = Keypair::generate();
    let ctx = ctx_adapter("matrix", &key.public());

    let mut client = ready(&ctx, "ada").await;
    client.send("@label=j1 NS JOIN matrix://matrix.org/gaming");
    let reply = client.expect_err(ErrCode::NoSuchTarget).await;
    assert_eq!(reply.label.as_deref(), Some("j1"));
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
async fn ns_meta_federation_opt_in_on_any_visibility() {
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!(
        "@root={} NS CREATE gaming unlisted",
        root_key_b64()
    ));
    let Event::NsMeta { id, .. } = ada.recv().await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();

    // §11.10: federation is an explicit opt-in for *any* visibility (an invite,
    // not public visibility, gates reachability for a non-public namespace).
    // It is off by default, so opening it flips the flag and re-emits NS-META.
    ada.send(&format!("NS META {ns_id} federation :open"));
    let ev = ada.recv().await;
    let Event::NsMeta {
        federation,
        visibility,
        ..
    } = &ev.event
    else {
        panic!("expected NS-META, got {ev:?}");
    };
    assert!(
        *federation,
        "federation is now open on an unlisted namespace"
    );
    assert_eq!(visibility.as_str(), "unlisted");
}

#[tokio::test]
async fn a_projected_namespace_bridges_both_directions_and_the_home_mints() {
    // Outbound projection, the return path (owner decision 2026-08-06): a
    // native namespace flagged `bridge:matrix` accepts the matrix provider's
    // foreign traffic — the home mints (no @msgid), the injection's labeled
    // echo is the ack (§3.5), and local-origin traffic flows out to the
    // provider like a replica's.
    let key = Keypair::generate();
    let ctx = ctx_plugin_full(
        vec![("mx", key.public(), vec!["matrix".parse().unwrap()])],
        &[],
    );

    // ada builds a native, public, projected namespace.
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@root={} NS CREATE gaming public", root_key_b64()));
    let Event::NsMeta { id, .. } = ada.recv().await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();
    let channel = ada.channel_by_vanity(&ns_id, "general").await;
    ada.send(&format!("NS META {ns_id} bridge:matrix :open"));
    ada.recv().await;
    ada.send(&format!("@label=j1 NS JOIN {ns_id}"));
    drain_until_ns_member(&mut ada).await;

    // The provider connects *after* the flag is set (live flag-flip attach is
    // a reconnect concern — §10's recovery story) and binds its realm. The
    // bind pushes the projected **structure**: NS-META (with `bridges=`), then
    // each channel's CHANNEL-LAYOUT + POLICY — the adapter needs the policy to
    // apply the §3 projection rules.
    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT matrix://matrix.org");
    let pushed_meta = loop {
        if let Ok(reply) = weft_proto::Reply::parse(&plugin.recv_raw().await) {
            if let Event::NsMeta { id, bridges, .. } = reply.event {
                break (id.to_string(), bridges);
            }
        }
    };
    assert_eq!(pushed_meta.0, ns_id);
    assert_eq!(pushed_meta.1.len(), 1, "bridges= rides the pushed meta");
    let layout = loop {
        if let Ok(reply) = weft_proto::Reply::parse(&plugin.recv_raw().await) {
            if let Event::ChannelLayout { channel, .. } = reply.event {
                break channel;
            }
        }
    };
    assert_eq!(layout, channel);
    let policy = loop {
        if let Ok(reply) = weft_proto::Reply::parse(&plugin.recv_raw().await) {
            if let Event::Policy { policy, .. } = reply.event {
                break policy;
            }
        }
    };
    assert_eq!(policy.to_string(), "retained:90d"); // the ns-create default seed

    // §8 in the outbound sense: the provider states a **foreign** member of
    // the projected namespace (a Matrix user joined the projected room)…
    plugin.send(&format!("NS-MEMBER {ns_id} carol@kde.org join"));
    let Event::Member { user, action, .. } = ada.recv().await.event else {
        panic!("ada expected carol's MEMBER join");
    };
    assert_eq!(user.to_string(), "carol@kde.org");
    assert_eq!(action, MemberAction::Join);

    // …but never a local one: locals join natively, and a provider claiming
    // otherwise is forging an action weftd itself owns.
    plugin.send(&format!("NS-MEMBER {ns_id} bob@test.example join"));
    plugin.expect_err(ErrCode::Unsupported).await;

    // WEFT → provider: a local member's post crosses like a replica's — and
    // carries her ULID (`ulid=`), the stable identity puppets key on.
    ada.send(&format!("@label=m1 MSG {channel} :hello matrix"));
    let (out, raw) = loop {
        let raw = plugin.recv_raw().await;
        if let Ok(reply) = weft_proto::Reply::parse(&raw) {
            if let Event::Message(m) = reply.event {
                break (m, raw);
            }
        }
    };
    assert_eq!(out.body, "hello matrix");
    assert_eq!(out.sender.to_string(), "ada@test.example");
    assert!(
        raw.contains("ulid="),
        "the event copy is ULID-stamped: {raw}"
    );
    ada.recv().await; // her own echo-ack

    // Provider → WEFT: a foreign user's post, no @msgid — the home mints…
    plugin.send(&format!(
        "@as=carol@kde.org;label=inj1 MSG {channel} :hi from matrix"
    ));
    let Event::Message(m) = ada.recv().await.event else {
        panic!("ada expected the projected MESSAGE");
    };
    assert_eq!(m.body, "hi from matrix");
    assert_eq!(m.sender.to_string(), "carol@kde.org");
    assert_eq!(
        m.msgid.origin().as_str(),
        "test.example",
        "the home mints on a projected channel"
    );

    // …and the provider's own copy carries the injection label — the §3.5
    // echo-ack, which is how the daemon learns the minted id.
    let echo = loop {
        let raw = plugin.recv_raw().await;
        if raw.contains("MESSAGE") && raw.contains("hi from matrix") {
            break raw;
        }
    };
    assert!(echo.contains("label=inj1"), "the echo is the ack: {echo}");

    // A carried @msgid is refused outright: foreign-minted ids on a native
    // channel would break home authority.
    plugin.send(&format!(
        "@as=carol@kde.org;msgid=matrix.org/{} MSG {channel} :minted elsewhere",
        ulid::Ulid::new()
    ));
    plugin.expect_err(ErrCode::Unsupported).await;

    // A local account stays a forgery — there is no relay to confirm here.
    plugin.send(&format!("@as=ada@test.example MSG {channel} :forged"));
    plugin.expect_err(ErrCode::Unsupported).await;

    // An *unflagged* namespace's channel refuses the whole path.
    let mut bob = ready(&ctx, "bob").await;
    bob.send(&format!("@root={} NS CREATE quiet public", root_key_b64()));
    let Event::NsMeta { id, .. } = bob.recv().await.event else {
        panic!("expected NS-META");
    };
    let quiet = bob.channel_by_vanity(&id.to_string(), "general").await;
    plugin.send(&format!("@as=carol@kde.org MSG {quiet} :not projected"));
    plugin.expect_err(ErrCode::Unsupported).await;

    // The foreign author reacts to the home-minted message by its real id —
    // ada sees it, and the provider gets the home-minted copy back too (the
    // ordinary local-origin forwarding; the daemon's own-echo handling drops it).
    plugin.send(&format!("@as=carol@kde.org REACT {} wave", m.msgid));
    let Event::Reaction { by, op, .. } = ada.recv().await.event else {
        panic!("ada expected the projected REACTION");
    };
    assert_eq!(op, weft_proto::ReactionOp::Add);
    assert_eq!(by.to_string(), "carol@kde.org");
}

#[tokio::test]
async fn a_provider_bot_is_a_kind_of_account_not_a_suspended_one() {
    // Owner directive 2026-08-06: bots are native. A provider's bot is a real
    // account that cannot authenticate — it acts through the provider, and later
    // through an API token — and it is **not** suspended, so a moderator can
    // still suspend a misbehaving one and see the difference.
    let key = Keypair::generate();
    let (ctx, store) = ctx_plugin_store(
        vec![("mx", key.public(), vec!["matrix".parse().unwrap()])],
        &[],
    );

    let plugin = plugin_session(&ctx, &key).await;
    let reg = weft_proto::Registration {
        api: 1,
        id: "mx".into(),
        name: "Matrix Bridge".into(),
        icon: None,
        actions: vec![],
        hooks: vec![],
        bot: Some("matrixbot".into()),
        schemes: vec!["matrix".parse().unwrap()],
    };
    plugin.send(&format!(
        "PLUGIN-REGISTER :{}",
        weft_proto::plugin_to_b64(&reg).unwrap()
    ));

    // Give the registration a moment to land, then read the account's state.
    let bot: weft_proto::Account = "matrixbot".parse().unwrap();
    let mut provisioned = false;
    for _ in 0..50 {
        if store.account_ulid(&bot).await.unwrap().is_some() {
            provisioned = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(provisioned, "the bot account was provisioned");
    assert!(store.is_bot(&bot).await.unwrap(), "marked as a bot");
    assert!(
        !store.is_suspended(&bot).await.unwrap(),
        "and NOT suspended — that is a moderation state, not a kind"
    );

    // It cannot sign in: uniform AUTH-FAILED, indistinguishable from bad
    // credentials (whether a handle is a bot is not probeable).
    let mut client = connect(&ctx);
    client.send("HELLO weft/1");
    assert!(matches!(client.recv().await.event, Event::Welcome { .. }));
    client.send("@label=a1 AUTH PASSWORD matrixbot :hunter2");
    let reply = client.expect_err(ErrCode::AuthFailed).await;
    assert_eq!(reply.label.as_deref(), Some("a1"));

    // …and the provider may still attribute lines to it (its own identity).
    assert_eq!(
        ctx.provider_bot("mx").map(|b| b.to_string()).as_deref(),
        Some("matrixbot")
    );
}

#[tokio::test]
async fn typing_crosses_a_replica_both_ways() {
    // §15: typing is bridged when the bridge asks for it. Ephemeral, so it is
    // announced rather than ingested — and attributed, since the wire's TYPING
    // names no user (a client's own session identifies them).
    let key = Keypair::generate();
    let ctx = ctx_plugin_full(
        vec![("mx", key.public(), vec!["matrix".parse().unwrap()])],
        &[],
    );

    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT matrix://matrix.org");
    plugin.send(&format!(
        "@title=Space;id={} NS-META matrix://matrix.org/space public",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::NsMeta { id, .. } = plugin.recv().await.event else {
        panic!("expected the minted NS-META");
    };
    let ns_id = id.to_string();
    plugin.send(&format!(
        "@vanity=general;id={} CHANNEL-LAYOUT matrix://matrix.org/space/general 0",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::ChannelLayout { channel, .. } = plugin.recv().await.event else {
        panic!("expected the minted CHANNEL-LAYOUT");
    };

    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@label=j1 NS JOIN {ns_id}"));
    drain_until_ns_member(&mut ada).await;
    assert!(matches!(
        weft_proto::Request::parse(&plugin.recv_raw().await)
            .unwrap()
            .command,
        weft_proto::Command::NsJoin { .. }
    ));

    // Realm → us: a foreign user is typing. Ada sees it attributed to them.
    plugin.send(&format!("@as=carol@kde.org TYPING {channel} start"));
    let Event::Typing { user, state, .. } = ada.recv().await.event else {
        panic!("ada expected the foreign TYPING");
    };
    assert_eq!(user.to_string(), "carol@kde.org");
    assert_eq!(state, weft_proto::TypingState::Start);

    // A *local* sender would be a forgery, small but still a forgery.
    plugin.send(&format!("@as=ada@test.example TYPING {channel} start"));
    plugin.expect_err(ErrCode::Unsupported).await;

    // Us → realm: ada's typing crosses, carrying her identity.
    ada.send(&format!("JOIN {channel}"));
    loop {
        if matches!(ada.recv().await.event, Event::Policy { .. }) {
            break;
        }
    }
    ada.send(&format!("TYPING {channel} start"));
    let relayed = loop {
        let raw = plugin.recv_raw().await;
        if raw.contains("TYPING") {
            break raw;
        }
    };
    // TYPING names its user in the event itself (no `@as` — that is a command
    // tag), and carries her ULID so the adapter can pick the right puppet.
    assert!(relayed.contains("ada@test.example"), "{relayed}");
    assert!(
        relayed.contains("ulid="),
        "puppets key on the ULID: {relayed}"
    );
}

#[tokio::test]
async fn presence_crosses_a_replica_both_ways() {
    // §6.1 owner directive 2026-08-09: a realm's users' presence is mirrored here
    // and ours is mirrored there. Unlike TYPING, presence names no channel in any
    // system that has it — it is per-user and global — so weftd does the fan-out
    // into the channels the user actually shares with us.
    let key = Keypair::generate();
    let ctx = ctx_plugin_full(
        vec![("mx", key.public(), vec!["matrix".parse().unwrap()])],
        &[],
    );

    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT matrix://matrix.org");
    plugin.send(&format!(
        "@title=Space;id={} NS-META matrix://matrix.org/space public",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::NsMeta { id, .. } = plugin.recv().await.event else {
        panic!("expected the minted NS-META");
    };
    let ns_id = id.to_string();
    plugin.send(&format!(
        "@vanity=general;id={} CHANNEL-LAYOUT matrix://matrix.org/space/general 0",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::ChannelLayout { channel, .. } = plugin.recv().await.event else {
        panic!("expected the minted CHANNEL-LAYOUT");
    };

    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@label=j1 NS JOIN {ns_id}"));
    drain_until_ns_member(&mut ada).await;

    // Consume her relayed join, so the reads below start from a known point.
    assert!(matches!(
        weft_proto::Request::parse(&plugin.recv_raw().await)
            .unwrap()
            .command,
        weft_proto::Command::NsJoin { .. }
    ));

    // The realm's user must be a member for us to share a channel with them —
    // presence is not a way to learn about strangers.
    plugin.send(&format!("NS-MEMBER {ns_id} carol@matrix.org join"));

    // Realm → us: carol goes away. Ada sees it, attributed, in the shared channel.
    plugin.send("@as=carol@matrix.org PRESENCE away");
    let reply = loop {
        let reply = ada.recv_any().await;

        if matches!(reply.event, Event::Presence { .. }) {
            break reply;
        }
    };
    let Event::Presence { user, status } = reply.event else {
        unreachable!("filtered above")
    };
    assert_eq!(user.to_string(), "carol@matrix.org");
    assert_eq!(status, weft_proto::PresenceStatus::Away);

    // …and it rides the roster, which used to read every bridged member offline
    // because there was no session here to read a dot from.
    ada.send(&format!("@label=m1 MEMBERS {channel}"));
    let mut carol_status = None;
    loop {
        match ada.recv_any().await.event {
            Event::BatchEnd { .. } => break,
            Event::Presence { user, status } if user.to_string() == "carol@matrix.org" => {
                carol_status = Some(status);
            }
            _ => {}
        }
    }
    assert_eq!(carol_status, Some(weft_proto::PresenceStatus::Away));

    // A *local* sender is a forgery, exactly as for TYPING.
    plugin.send("@as=ada@test.example PRESENCE away");
    plugin.expect_err(ErrCode::Unsupported).await;

    // Us → realm: ada's own status crosses so the adapter can set it on her
    // puppet, carrying the ULID it keys puppets by.
    ada.send(&format!("JOIN {channel}"));
    loop {
        if matches!(ada.recv().await.event, Event::Policy { .. }) {
            break;
        }
    }
    ada.send("PRESENCE dnd");
    let relayed = loop {
        let raw = plugin.recv_raw_any().await;

        if verb_of(&raw) == "PRESENCE" && raw.contains("ada@test.example") {
            break raw;
        }
    };
    assert!(relayed.contains("dnd"), "{relayed}");
    assert!(
        relayed.contains("ulid="),
        "puppets key on the ULID: {relayed}"
    );

    // Invisible is stored and NOT announced — bridging it would reveal the
    // hiding, which is the one thing it exists to prevent. TYPING is the barrier:
    // it crosses, and she sends it *after*, so seeing it first proves no PRESENCE
    // was on the way (the session writes its lines in order).
    ada.send("PRESENCE invisible");
    ada.send(&format!("TYPING {channel} start"));
    let leaked = loop {
        let raw = plugin.recv_raw_any().await;

        match verb_of(&raw) {
            "PRESENCE" => break Some(raw),
            "TYPING" => break None,
            _ => {}
        }
    };
    assert!(
        leaked.is_none(),
        "invisible leaked to the realm: {leaked:?}"
    );
}

#[tokio::test]
async fn ephemera_and_dms_work_on_a_projection_only_bridge() {
    // Regression, reported 2026-08-09: typing from Element showed nothing, and DMs
    // did not work. All three bugs were the *same* shape — foreign traffic has two
    // doors (a consumed **replica**, and a **projected** native namespace), and each
    // of these only knew the first:
    //
    //   - typing authorized via the channel's `origin` scheme → a projected channel
    //     has no origin, so an attributed TYPING was dropped;
    //   - presence enumerated `namespaces_with_origin()` only → same blind spot;
    //   - the DM route resolved "who serves this realm" by scanning replica origins
    //     → on a projection-only bridge that answered "nobody".
    //
    // So this is a projection-only setup: no replica namespace exists anywhere.
    let key = Keypair::generate();
    let ctx = ctx_plugin_full(
        vec![("mx", key.public(), vec!["matrix".parse().unwrap()])],
        &[],
    );

    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@root={} NS CREATE gaming public", root_key_b64()));
    let Event::NsMeta { id, .. } = ada.recv().await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();
    let channel = ada.channel_by_vanity(&ns_id, "general").await;
    ada.send(&format!("NS META {ns_id} bridge:matrix :open"));
    ada.recv().await;
    ada.send(&format!("@label=j1 NS JOIN {ns_id}"));
    drain_until_ns_member(&mut ada).await;

    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT matrix://matrix.example");
    plugin.send(&format!("NS-MEMBER {ns_id} carol@matrix.example join"));

    // Typing from the realm's user reaches ada in the projected channel.
    plugin.send(&format!("@as=carol@matrix.example TYPING {channel} start"));
    // Past carol's membership statement, which arrives first.
    let user = loop {
        match ada.recv().await.event {
            Event::Typing { user, .. } => break user,
            _ => continue,
        }
    };
    assert_eq!(user.to_string(), "carol@matrix.example");

    // …and so does their presence.
    plugin.send("@as=carol@matrix.example PRESENCE away");
    let reply = loop {
        let reply = ada.recv_any().await;

        if matches!(reply.event, Event::Presence { .. }) {
            break reply;
        }
    };
    let Event::Presence { user, status } = reply.event else {
        unreachable!("filtered above")
    };
    assert_eq!(user.to_string(), "carol@matrix.example");
    assert_eq!(status, weft_proto::PresenceStatus::Away);

    // A DM to one of the realm's users routes to the provider that asserted that
    // realm — which is now how the route is resolved, rather than by inferring it
    // from a replica namespace that a projection-only bridge does not have.
    ada.send("@label=d1 MSG @carol@matrix.example :hey");
    let relayed = loop {
        let raw = plugin.recv_raw().await;

        if verb_of(&raw) == "MSG" && raw.contains("carol@matrix.example") {
            break raw;
        }
    };
    assert!(
        relayed.contains("as=ada@test.example"),
        "the DM carries its sender: {relayed}"
    );
}

#[tokio::test]
async fn a_refused_relay_answers_the_poster_at_once() {
    // Owner directive 2026-08-09: `UNDELIVERED` takes a **label**, so a relayed post
    // the realm refuses is reported rather than waited out. There is no msgid to name
    // — the realm is the home of a replica channel, so weftd minted nothing — and the
    // bridge label is the only handle either side has on that post.
    let key = Keypair::generate();
    let ctx = ctx_plugin_full(
        vec![("insta", key.public(), vec!["instagram".parse().unwrap()])],
        &[],
    );

    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT instagram://acme-corp");
    plugin.send(&format!(
        "@title=Club;id={} NS-META instagram://acme-corp/club public",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::NsMeta { id, .. } = plugin.recv().await.event else {
        panic!("expected the minted NS-META");
    };
    let ns_id = id.to_string();
    plugin.send(&format!(
        "@vanity=general;id={} CHANNEL-LAYOUT instagram://acme-corp/club/general 0",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::ChannelLayout { channel, .. } = plugin.recv().await.event else {
        panic!("expected the minted CHANNEL-LAYOUT");
    };

    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@label=j1 NS JOIN {ns_id}"));
    drain_until_ns_member(&mut ada).await;

    // Her post is relayed under a bridge label…
    ada.send(&format!("@label=m1 MSG {channel} :will not land"));
    let bridge_label = loop {
        let line = weft_proto::Line::parse(&plugin.recv_raw().await).unwrap();

        if let Ok(weft_proto::Command::Msg { .. }) =
            weft_proto::Request::from_line(&line).map(|r| r.command)
        {
            break line.tags.get("label").expect("a bridge label").clone();
        }
    };

    // …and the realm answers that it could not deliver it. No msgid: there is none.
    plugin.send(&format!(
        "@label={bridge_label} UNDELIVERED :no Matrix room is mapped for that channel"
    ));

    // She hears it on **her own** label, so the client fails the pending echo it is
    // holding rather than shimmering until its send deadline.
    let reply = ada.expect_err(ErrCode::Policy).await;
    assert_eq!(reply.label.as_deref(), Some("m1"));
    assert_eq!(err_context(&reply).as_deref(), Some("not-delivered"));
    let Event::Err(err) = &reply.event else {
        unreachable!("expect_err")
    };
    assert!(
        err.text.contains("no Matrix room is mapped"),
        "the realm's reason survives: {}",
        err.text
    );

    // An expired or unknown label must not fail a message it does not own — the
    // token is the authorization here. Barriered by a *real* failure of the next
    // post: if the bogus line had consumed m2's queued label, the error below would
    // carry the wrong reason (or no label at all).
    ada.send(&format!("@label=m2 MSG {channel} :the next one"));
    let second_label = loop {
        let line = weft_proto::Line::parse(&plugin.recv_raw().await).unwrap();

        if let Ok(weft_proto::Command::Msg { .. }) =
            weft_proto::Request::from_line(&line).map(|r| r.command)
        {
            break line.tags.get("label").expect("a bridge label").clone();
        }
    };
    plugin.send("@label=B-instagram-nonexistent UNDELIVERED :not a label we issued");
    plugin.send(&format!(
        "@label={second_label} UNDELIVERED :the real reason"
    ));

    let reply = ada.expect_err(ErrCode::Policy).await;
    assert_eq!(reply.label.as_deref(), Some("m2"));
    let Event::Err(err) = &reply.event else {
        unreachable!("expect_err")
    };
    assert!(
        err.text.contains("the real reason"),
        "an unknown label failed a message it does not own: {}",
        err.text
    );
}

#[tokio::test(start_paused = true)]
async fn a_silent_provider_is_probed_and_then_taken_offline() {
    // Owner directive 2026-08-09: liveness must not be *inferred* from traffic. A
    // bridge is legitimately quiet whenever its realm is, but its namespaces are
    // advertised as online purely because this session exists — so an adapter that
    // is gone or wedged behind an open socket makes weftd claim what it cannot
    // support. weftd asks, and a provider that answers nothing is taken offline.
    let key = Keypair::generate();
    let ctx = ctx_plugin_full(
        vec![("insta", key.public(), vec!["instagram".parse().unwrap()])],
        &[],
    );

    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT instagram://acme-corp");
    plugin.send(&format!(
        "@title=Club;id={} NS-META instagram://acme-corp/club public",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::NsMeta { id, .. } = plugin.recv().await.event else {
        panic!("expected the minted NS-META");
    };
    let ns_id = id.to_string();

    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@label=j1 NS JOIN {ns_id}"));
    drain_until_ns_member(&mut ada).await;

    // Quiet for the probe interval: weftd asks rather than assuming.
    let probe = loop {
        let raw = plugin.recv_raw_slow().await;

        if verb_of(&raw) == "PING" {
            break raw;
        }
    };
    assert!(probe.contains("liveness"), "{probe}");

    // It answers nothing at all. Past the grace window the session is closed and
    // the namespace goes offline for its members — the same push a disconnect
    // produces, because it *is* the disconnect path.
    let Event::NsMeta {
        provider_online, ..
    } = ada.recv_slow().await.event
    else {
        panic!("ada expected the provider-offline NS-META push");
    };
    assert_eq!(provider_online, Some(false));
    assert!(
        plugin.closed().await,
        "the unanswering session was not closed"
    );
}

#[tokio::test]
async fn a_re_asserted_layout_reaches_the_members() {
    // Reported 2026-08-09: after the adapter reconnected, the channel still showed a
    // bare ULID until the *client* was restarted. A re-assert is how a realm restates
    // a room, and it was answered on the provider's own session only — so weftd's
    // store was corrected while every connected client kept what it had cached.
    let key = Keypair::generate();
    let ctx = ctx_plugin_full(
        vec![("insta", key.public(), vec!["instagram".parse().unwrap()])],
        &[],
    );

    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT instagram://acme-corp");
    plugin.send(&format!(
        "@title=Club;id={} NS-META instagram://acme-corp/club public",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::NsMeta { id, .. } = plugin.recv().await.event else {
        panic!("expected the minted NS-META");
    };
    let ns_id = id.to_string();
    let chan_id = ulid::Ulid::new().to_string().to_lowercase();
    plugin.send(&format!(
        "@id={chan_id};vanity=old-name CHANNEL-LAYOUT instagram://acme-corp/club/general 0"
    ));
    let Event::ChannelLayout { channel, .. } = plugin.recv().await.event else {
        panic!("expected the minted CHANNEL-LAYOUT");
    };

    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@label=j1 NS JOIN {ns_id}"));
    drain_until_ns_member(&mut ada).await;

    // The adapter reconnects and restates the room, now naming it.
    plugin.send(&format!(
        "@id={chan_id};vanity=general CHANNEL-LAYOUT instagram://acme-corp/club/general 0"
    ));

    let layout = loop {
        match ada.recv().await.event {
            Event::ChannelLayout {
                channel: c, vanity, ..
            } if c == channel => break vanity,
            _ => continue,
        }
    };
    assert_eq!(layout, "general", "the member never heard the new name");
}

#[tokio::test]
async fn a_dm_to_an_offline_realm_is_refused_like_a_post() {
    // Owner directive 2026-08-09: a bridged domain is routed like a federated one —
    // weftd knows the realm by name — and when its bridge is down the DM must fail
    // the same way a post into one of its channels does. It used to be stored and
    // echoed locally instead, which looks exactly like a delivered message.
    let key = Keypair::generate();
    let ctx = ctx_plugin_full(
        vec![("insta", key.public(), vec!["instagram".parse().unwrap()])],
        &[],
    );

    // A replica namespace is what makes the realm *known* after its provider goes:
    // the origin URI survives the disconnect, so "bridged but down" stays
    // distinguishable from "never heard of".
    let plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT instagram://acme-corp");
    plugin.send(&format!(
        "@title=Club;id={} NS-META instagram://acme-corp/club public",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let mut plugin = plugin;
    let Event::NsMeta { .. } = plugin.recv().await.event else {
        panic!("expected the minted NS-META");
    };

    let mut ada = ready(&ctx, "ada").await;

    // While it is connected the DM is relayed.
    ada.send("@label=d1 MSG @carol@acme-corp :hello");
    let relayed = loop {
        let raw = plugin.recv_raw().await;

        if verb_of(&raw) == "MSG" {
            break raw;
        }
    };
    assert!(relayed.contains("as=ada@test.example"), "{relayed}");
    // Mandatory, not decorative: an adapter keys puppets by the stable id and drops
    // a relay without one, so a DM missing it is written here and discarded there.
    assert!(
        relayed.contains("ulid="),
        "the DM must carry the actor's ULID: {relayed}"
    );

    // The provider goes. The next DM is refused rather than filed: same code and
    // context as posting into one of that realm's channels.
    drop(plugin);
    let refused = loop {
        ada.send("@label=d2 MSG @carol@acme-corp :are you there");
        let reply = ada.recv().await;

        match reply.event {
            Event::Err(_) => break reply,
            // The provider-offline NS-META push may arrive first.
            _ => continue,
        }
    };
    assert_eq!(refused.label.as_deref(), Some("d2"));
    assert_eq!(err_context(&refused).as_deref(), Some("provider-offline"));
}

#[tokio::test]
async fn an_ns_meta_change_reaches_a_projecting_provider() {
    // A provider is not an ns member, so the ordinary fan-out never reaches it
    // — yet NS-META is exactly what describes its structure (Space name,
    // category sub-spaces). Without this push a category added in a client
    // would never appear on the foreign side.
    let key = Keypair::generate();
    let ctx = ctx_plugin_full(
        vec![("mx", key.public(), vec!["matrix".parse().unwrap()])],
        &[],
    );

    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@root={} NS CREATE gaming public", root_key_b64()));
    let Event::NsMeta { id, .. } = ada.recv().await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();
    ada.send(&format!("NS META {ns_id} bridge:matrix :open"));
    ada.recv().await;

    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT matrix://matrix.org");
    let mut settled = false;
    while !settled {
        if let Ok(reply) = weft_proto::Reply::parse(&plugin.recv_raw().await) {
            settled = matches!(reply.event, Event::Policy { .. });
        }
    }

    // The category list is namespace metadata; the provider must hear it.
    ada.send(&format!("NS META {ns_id} categories :Text,Voice"));
    let pushed = loop {
        if let Ok(reply) = weft_proto::Reply::parse(&plugin.recv_raw().await) {
            if let Event::NsMeta { categories, .. } = reply.event {
                break categories;
            }
        }
    };
    assert_eq!(pushed, ["Text", "Voice"]);

    // An **unprojected** namespace's meta stays local: nothing to describe.
    ada.send(&format!("NS META {ns_id} bridge:matrix :closed"));
    ada.recv().await;
    let mut bob = ready(&ctx, "bob").await;
    bob.send(&format!("@root={} NS CREATE quiet public", root_key_b64()));
    let Event::NsMeta { id, .. } = bob.recv().await.event else {
        panic!("expected NS-META");
    };
    bob.send(&format!("NS META {id} categories :Private"));
    bob.recv().await;
    // Drain what the flag-close pushed, then assert nothing else arrives.
    let quiet = tokio::time::timeout(Duration::from_millis(300), async {
        loop {
            let raw = plugin.recv_raw().await;
            if raw.contains("Private") {
                return raw;
            }
        }
    })
    .await;
    assert!(
        quiet.is_err(),
        "an unprojected namespace must not be described: {quiet:?}"
    );
}

#[tokio::test]
async fn a_channel_created_in_a_projected_namespace_reaches_the_provider_live() {
    // The create-room flow's weftd half: a channel created *after* the
    // provider's startup structure push must reach it immediately — structure
    // **and** traffic. Without both, a create-room button appears to do
    // nothing until the bridge reconnects.
    let key = Keypair::generate();
    let ctx = ctx_plugin_full(
        vec![("mx", key.public(), vec!["matrix".parse().unwrap()])],
        &[],
    );

    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@root={} NS CREATE gaming public", root_key_b64()));
    let Event::NsMeta { id, .. } = ada.recv().await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();
    ada.send(&format!("NS META {ns_id} bridge:matrix :open"));
    ada.recv().await;
    ada.send(&format!("@label=j1 NS JOIN {ns_id}"));
    drain_until_ns_member(&mut ada).await;

    // The provider is connected and has already taken its structure push.
    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT matrix://matrix.org");
    let mut seen_policy = false;
    while !seen_policy {
        if let Ok(reply) = weft_proto::Reply::parse(&plugin.recv_raw().await) {
            seen_policy = matches!(reply.event, Event::Policy { .. });
        }
    }

    // Now ada creates a channel — with `permanent` retention, exactly as the
    // create-room flow does, since nothing else projects (matrix.md §3). Its
    // layout + policy must arrive unprompted.
    ada.send(&format!(
        "@label=c1 CHANNEL CREATE #{ns_id}/announcements permanent"
    ));
    let channel = loop {
        match ada.recv().await.event {
            Event::Policy { channel, .. } => break channel,
            _ => continue,
        }
    };
    let mut layout = None;
    let mut policy = None;
    while layout.is_none() || policy.is_none() {
        if let Ok(reply) = weft_proto::Reply::parse(&plugin.recv_raw().await) {
            match reply.event {
                Event::ChannelLayout {
                    channel, vanity, ..
                } => layout = Some((channel, vanity)),
                Event::Policy { policy: p, .. } => policy = Some(p),
                _ => {}
            }
        }
    }
    let (pushed, vanity) = layout.unwrap();
    assert_eq!(pushed, channel);
    assert_eq!(vanity, "announcements");
    assert_eq!(policy.unwrap().to_string(), "permanent");

    // …and its traffic mirrors without a reconnect: the provider was attached
    // to the new channel, not merely told about it.
    // Creating a channel does not join it (v0.12 derived membership covers the
    // roster, not the session's subscription).
    ada.send(&format!("@label=jn JOIN {channel}"));
    loop {
        if ada.recv().await.label.as_deref() == Some("jn") {
            break;
        }
    }

    ada.send(&format!("@label=m1 MSG {channel} :first post"));
    let mirrored = loop {
        if let Ok(reply) = weft_proto::Reply::parse(&plugin.recv_raw().await) {
            if let Event::Message(m) = reply.event {
                break m;
            }
        }
    };
    assert_eq!(mirrored.body, "first post");
    assert_eq!(mirrored.sender.to_string(), "ada@test.example");
}

#[tokio::test]
async fn foreign_moderators_wield_exactly_their_granted_authority() {
    // §10 (slice 11): a Matrix moderator's act arrives as an attributed
    // command and succeeds iff WEFT granted *that user* the authority — a
    // foreign admin has exactly the power some grant gave their handle,
    // nothing structural.
    let key = Keypair::generate();
    let ctx = ctx_plugin_full(
        vec![("mx", key.public(), vec!["matrix".parse().unwrap()])],
        &[],
    );

    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@root={} NS CREATE gaming public", root_key_b64()));
    let Event::NsMeta { id, .. } = ada.recv().await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();
    let channel = ada.channel_by_vanity(&ns_id, "general").await;
    ada.send(&format!("NS META {ns_id} bridge:matrix :open"));
    ada.recv().await;
    ada.send(&format!("@label=j1 NS JOIN {ns_id}"));
    drain_until_ns_member(&mut ada).await;

    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT matrix://matrix.org");

    // ada posts; carol (a foreign non-moderator) tries to delete it.
    ada.send(&format!("@label=m1 MSG {channel} :try to remove this"));
    let posted = loop {
        if let Ok(reply) = weft_proto::Reply::parse(&plugin.recv_raw().await) {
            if let Event::Message(m) = reply.event {
                break m.msgid;
            }
        }
    };
    ada.recv().await; // her echo-ack

    // No grant → the non-author delete drops silently (nothing reaches ada).
    plugin.send(&format!("@as=carol@kde.org DELETE {posted}"));

    // …and an ungranted GRANT attempt is refused with the ordinary error.
    plugin.send(&format!(
        "@as=carol@kde.org GRANT bob@kde.org ns:{ns_id} mute"
    ));
    plugin.expect_err(ErrCode::CapRequired).await;

    // ada (the owner) makes carol a moderator: delete-any + grant:mute.
    ada.send(&format!(
        "@label=g1 GRANT carol@kde.org ns:{ns_id} delete-any,grant:mute"
    ));
    loop {
        let reply = ada.recv().await;
        if reply.label.as_deref() == Some("g1") {
            break;
        }
    }
    // The grant relays outward so the provider can raise carol's level (§10).
    loop {
        let raw = plugin.recv_raw().await;
        if let Ok(req) = weft_proto::Request::parse(&raw) {
            if matches!(req.command, weft_proto::Command::Grant { .. }) {
                break;
            }
        }
    }

    // Now the same two acts succeed: the moderator delete lands as a
    // tombstone ada sees…
    plugin.send(&format!("@as=carol@kde.org DELETE {posted}"));
    let deleted = loop {
        match ada.recv().await.event {
            Event::Deleted { msgid, by, .. } => break (msgid, by),
            _ => continue,
        }
    };
    assert_eq!(deleted.0, posted);
    assert_eq!(
        deleted.1.map(|u| u.to_string()).as_deref(),
        Some("carol@kde.org")
    );

    // …and the granted `grant:mute` lets carol promote bob to muter.
    plugin.send(&format!(
        "@as=carol@kde.org GRANT bob@kde.org ns:{ns_id} mute"
    ));
    let raw = plugin.recv_raw().await;
    assert!(
        raw.contains("TOKEN") || !raw.contains("ERR"),
        "the granted authority must be honored: {raw}"
    );
}

#[tokio::test]
async fn ns_meta_bridge_flag_requires_public_and_closes_with_visibility() {
    // Outbound projection (matrix.md §17.1): `NS META <ns> bridge:<scheme>
    // :open|closed`. The flag is ns-admin consent to mirror a native namespace
    // into a foreign system — and the return-path authorization anchor — so
    // it demands `public` and must never outlive that visibility.
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@root={} NS CREATE gaming public", root_key_b64()));
    let Event::NsMeta { id, .. } = ada.recv().await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();

    ada.send(&format!("NS META {ns_id} bridge:matrix :open"));
    let Event::NsMeta { bridges, .. } = ada.recv().await.event else {
        panic!("expected NS-META");
    };
    assert_eq!(
        bridges.iter().map(|b| b.to_string()).collect::<Vec<_>>(),
        ["matrix"],
        "the projection opt-in rides the meta event"
    );

    // A second scheme accumulates; closing one leaves the other.
    ada.send(&format!("NS META {ns_id} bridge:discord :open"));
    ada.recv().await;
    ada.send(&format!("NS META {ns_id} bridge:matrix :closed"));
    let Event::NsMeta { bridges, .. } = ada.recv().await.event else {
        panic!("expected NS-META");
    };
    assert_eq!(
        bridges.iter().map(|b| b.to_string()).collect::<Vec<_>>(),
        ["discord"]
    );

    // Leaving `public` closes every projection — the flag would otherwise
    // leak what the visibility now hides.
    ada.send(&format!("@label=v1 NS VISIBILITY {ns_id} unlisted"));
    let Event::NsMeta { bridges, .. } = ada.recv().await.event else {
        panic!("expected NS-META");
    };
    assert!(bridges.is_empty(), "projection must not outlive public");

    // …and opening on a non-public namespace is refused outright.
    ada.send(&format!("@label=b1 NS META {ns_id} bridge:matrix :open"));
    let reply = ada.expect_err(ErrCode::Forbidden).await;
    assert_eq!(err_context(&reply).as_deref(), Some("visibility"));
}

#[tokio::test]
async fn bridge_request_offers_only_reachable_namespaces() {
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&[], &[], "peer.example", &peer_key.public());

    // Owner makes a public namespace reachable.
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@root={} NS CREATE gaming public", root_key_b64()));
    let Event::NsMeta { id, .. } = ada.recv().await.event else {
        panic!("expected NS-META");
    };
    let gaming_id = id.to_string();
    ada.send(&format!("NS META {gaming_id} federation :open"));
    ada.recv().await;

    let mut peer = bridged_peer(&ctx, "peer.example", &peer_key).await;

    // Reachable → the peer receives a signed BRIDGE PROPOSE offer. The peer
    // addresses our namespace by its human vanity (it typed `ournet/gaming`).
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

    // §11.10 invite path: an *unlisted* namespace with federation open is
    // reachable only to a peer presenting a valid invite for it.
    ada.send(&format!(
        "@root={} NS CREATE secret unlisted",
        root_key_b64()
    ));
    let Event::NsMeta { id, .. } = ada.recv().await.event else {
        panic!("expected NS-META");
    };
    let secret_id = id.to_string();
    ada.send(&format!("NS META {secret_id} federation :open"));
    ada.recv().await;
    ada.send(&format!("INVITE MINT ns:{secret_id}"));
    let Event::Invited { invite_id, .. } = ada.recv().await.event else {
        panic!("expected INVITED");
    };

    // No invite → indistinguishable from absent (NO-SUCH-TARGET, invariant 1).
    peer.send("BRIDGE REQUEST secret");
    assert!(peer.recv_raw().await.contains("NO-SUCH-TARGET"));

    // A bogus invite → same uniform refusal.
    peer.send("@invite=inv_bogus BRIDGE REQUEST secret");
    assert!(peer.recv_raw().await.contains("NO-SUCH-TARGET"));

    // A valid invite for this namespace → the peer gets the signed offer.
    peer.send(&format!("@invite={invite_id} BRIDGE REQUEST secret"));
    let offer = peer.recv_raw().await;
    assert!(
        offer.contains("BRIDGE PROPOSE") && offer.contains("manifest="),
        "a valid invite unlocks the non-public namespace, got {offer}"
    );

    // An invite for a *different* namespace must not unlock `secret`.
    ada.send(&format!("INVITE MINT ns:{gaming_id}"));
    let Event::Invited {
        invite_id: other, ..
    } = ada.recv().await.event
    else {
        panic!("expected INVITED");
    };
    peer.send(&format!("@invite={other} BRIDGE REQUEST secret"));
    assert!(peer.recv_raw().await.contains("NO-SUCH-TARGET"));
}

#[tokio::test]
async fn federate_hands_request_to_the_dialer() {
    let ctx = ctx(&[]);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.set_auto_bridge_sink(tx);
    let mut ada = ready(&ctx, "ada").await;

    // A valid foreign target is handed to the dialer (async — no client ack);
    // an `@invite=` is threaded through verbatim for the non-public path.
    ada.send("@invite=inv_xyz FEDERATE weft.example/gaming");
    let req = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("timed out waiting for the dialer request")
        .expect("sink closed");
    assert_eq!(req.network.as_str(), "weft.example");
    assert_eq!(req.namespace.to_string(), "gaming");
    assert_eq!(req.invite.as_deref(), Some("inv_xyz"));

    // A second request immediately after is throttled (per-account cooldown).
    ada.send("FEDERATE weft.example/other");
    ada.expect_err(ErrCode::Throttled).await;

    // Federating your own network is a no-op (self-check precedes the cooldown).
    ada.send("FEDERATE test.example/gaming");
    ada.expect_err(ErrCode::Unsupported).await;
}

#[tokio::test]
async fn federate_unsupported_when_auto_bridge_off() {
    let ctx = ctx(&[]); // no sink installed → auto-federation is off
    let mut ada = ready(&ctx, "ada").await;
    ada.send("FEDERATE weft.example/gaming");
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
    // through a bridge `@as` command — she never connects to H (IP
    // non-exposure); F vouches for her by having proven its network key.
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &["boss"], "peer.example", &peer_key.public());

    // H's operator grants `mute` at #general to the foreign user alice@peer.example.
    let mut boss = ready(&ctx, "boss").await;
    boss.send("GRANT alice@peer.example #general mute");
    assert!(matches!(boss.recv().await.event, Event::Token { .. }));

    // F authenticates the bridge and runs alice's MUTE as `@as=alice` (§11.14).
    let mut bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    bridge.send("@as=alice;label=m MUTE #general bob :spam");

    // The reply comes back as an ordinary event over the bridge, attributed to
    // the federated moderator — enforcement hit H's grant store for account@net.
    let raw = bridge.recv_raw().await;
    assert!(raw.contains("MODERATED #general bob mute"), "{raw}");
    assert!(raw.contains("by=alice@peer.example"), "{raw}");

    // A federated user WITHOUT the cap is refused — homeserver authority is not a
    // blanket; her power is exactly what H granted account@network.
    bridge.send("@as=mallory;label=x MUTE #general bob");
    let raw = bridge.recv_raw().await;
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
    bridge.send("@as=alice;label=f FRIEND ADD bob@test.example");

    // alice's own state (outgoing) comes back as an ordinary event over the bridge.
    let raw = bridge.recv_raw().await;
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
    bridge.send("@as=alice;label=g GRANT bob@peer.example #general mute");
    let raw = bridge.recv_raw().await;
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
    // The server mints the channel's canonical `#<chan-id>` (v0.13), so the
    // POLICY echo carries the id, not the "lounge" vanity.
    bridge.send("@as=alice;label=c CHANNEL CREATE #lounge");
    let raw = bridge.recv_raw().await;
    assert!(raw.contains("POLICY") && raw.contains("label=c"), "{raw}");
}

#[tokio::test]
async fn federated_admin_edits_namespace_meta_over_the_bridge() {
    // §11.10 full authority incl. namespace administration (the ns-admin gate is
    // actor-aware). A federated `ns-admin` holder edits H's namespace config.
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&[], &["boss"], "peer.example", &peer_key.public());
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@root={} NS CREATE gaming public", root_key_b64()));
    let Event::NsMeta { id, .. } = ada.recv().await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();
    ada.send(&format!("GRANT alice@peer.example ns:{ns_id} ns-admin"));
    assert!(matches!(ada.recv().await.event, Event::Token { .. }));

    let mut bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    bridge.send(&format!(
        "@as=alice;label=n NS META {ns_id} title :Alice's Lounge"
    ));
    let raw = bridge.recv_raw().await;
    // The NS-META event still carries the "gaming" vanity for display.
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

    // A spoke relays alice's post to us (the home) as an `@as` MSG carrying the
    // bridge label her spoke is waiting on (§11.14; no `@id` = a mint request).
    bridge.send("@as=alice;label=e-alice-1 MSG #general :hi from alice");

    // ada (a local home member) sees alice's message, minted by the home — and
    // *without* the label (only the poster's network's copy carries it).
    let Event::Message(m) = ada.recv().await.event else {
        panic!("expected alice's minted message");
    };
    assert_eq!(m.sender.to_string(), "alice@peer.example");
    assert_eq!(m.body, "hi from alice");
    assert_eq!(m.msgid.origin().as_str(), "test.example"); // home is the origin

    // The home-minted message is mirrored back out to the poster's network
    // (peer.example) carrying the bridge label as `@label` — so its spoke can pair
    // it with the waiting session. No other recipient's copy carries it.
    loop {
        let line = bridge.recv_raw().await;
        if line.contains("MESSAGE #general alice@peer.example") {
            assert!(line.contains("hi from alice"), "{line}");
            assert!(line.contains("test.example/"), "{line}"); // home-minted origin
            assert!(line.contains("label=e-alice-1"), "{line}"); // label rides only to the origin network
            break;
        }
    }
}

#[tokio::test]
async fn spoke_delivers_home_minted_post_as_the_posters_labelled_echo() {
    // §11.13: a spoke poster's message comes back over the mirror carrying the
    // transient echo (as `nonce=`); the spoke pairs it with the waiting session and
    // delivers it as that session's own **labelled** message — the §3.5 ack, by
    // label, exactly like a local send. No user ever sees the echo.
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

    // ada posts with a label → the spoke relays it to the home as an `@as` MSG
    // carrying a bridge label `B-…` it is waiting on (§11.14).
    ada.send("@label=post MSG #general :hello");
    let relayed = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("relay")
        .expect("sink open");
    let req = weft_proto::Request::parse(&relayed.line).unwrap();
    let token = req.label.clone().expect("bridge label");
    let weft_proto::Command::Msg { target, .. } = req.command else {
        panic!("expected a spoke @as MSG relay, got {:?}", relayed.line);
    };
    assert_eq!(target.to_string(), "#general");
    assert_eq!(relayed.from.as_ref().unwrap().to_string(), "ada"); // the dialer attributes it @as=ada
    assert!(token.starts_with("B-home.example-"), "{token}");

    // The home mints it and mirrors it back to us carrying the same bridge label.
    let mid = "home.example/01ARZ3NDEKTSV4RRFFQ69G5FB0";
    bridge.send(&format!(
        "@msgid={mid};label={token} MESSAGE #general ada@test.example :hello"
    ));

    // ada receives her message WITH the label — reconciled as her own send.
    let reply = ada.recv().await;
    assert_eq!(reply.label.as_deref(), Some("post"));
    match reply.event {
        Event::Message(m) => {
            assert_eq!(m.body, "hello");
            assert_eq!(m.msgid.to_string(), mid);
        }
        e => panic!("expected labelled message, got {e:?}"),
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
    assert!(relay.line.contains("MSG #general"), "{}", relay.line);
    assert_eq!(relay.from.as_ref().unwrap().to_string(), "ada"); // the dialer attributes it @as=ada
    assert!(!relay.line.contains("msgid="), "{}", relay.line); // no @id = a mint request
    assert!(relay.line.contains("hello home"), "{}", relay.line);
}

#[tokio::test]
async fn home_applies_relayed_channel_edit_and_rejects_a_non_author() {
    // §11.14/§11.4: the home applies a spoke member's relayed mutation only after
    // verifying authorship — a different sender's forged edit is dropped.
    let peer_key = Keypair::generate();
    let ctx = ctx_bridged(&["#general"], &[], "peer.example", &peer_key.public());
    let mut ada = joined(&ctx, "ada", "#general").await;
    let mut bridge = bridged_peer(&ctx, "peer.example", &peer_key).await;
    propose(&mut bridge, &peer_key, &["#general"]).await;
    assert!(matches!(ada.recv().await.event, Event::Manifest { .. }));

    // A spoke relays alice's post (`@as` MSG) → the home mints it.
    bridge.send("@as=alice MSG #general :typo heer");
    let Event::Message(m) = ada.recv().await.event else {
        panic!("expected alice's minted message");
    };
    let minted = m.msgid.clone();
    assert_eq!(minted.origin().as_str(), "test.example");

    // A NON-author (bob) tries to edit alice's message: the home drops it.
    bridge.send(&format!("@as=bob EDIT {minted} :hijacked"));

    // The author (alice) edits: the home applies it.
    bridge.send(&format!("@as=alice EDIT {minted} :typo here"));

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
    // §11.14: a member editing their own message on a spoke relays an ordinary
    // `@as EDIT <msgid>` to the home rather than mutating locally.
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
        relay.line.contains(&format!("EDIT {mid}")),
        "{}",
        relay.line
    );
    assert_eq!(relay.from.as_ref().unwrap().to_string(), "ada"); // the dialer attributes it @as=ada
    assert!(relay.line.contains("hello"), "{}", relay.line);
    assert!(!relay.line.contains("id="), "{}", relay.line); // @id absent = apply request
}

#[tokio::test]
async fn spoke_requests_channel_backfill_from_the_home_on_history() {
    // §11.14: a spoke viewing a home-authoritative channel's history asks the home
    // to replay anything it minted while the spoke was unreachable — an ordinary
    // `@as HISTORY` (the dialer adds `@as`; here we capture the pre-dialer line).
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
        if d.line.contains("HISTORY") {
            break d;
        }
    };
    assert_eq!(req.peer.as_str(), "home.example");
    assert_eq!(req.from.as_ref().unwrap().to_string(), "ada"); // the dialer attributes it @as=ada
    let weft_proto::Command::History { target, .. } =
        weft_proto::Request::parse(&req.line).unwrap().command
    else {
        panic!("expected HISTORY, got {:?}", req.line);
    };
    assert_eq!(target.to_string(), "#general");
}

#[tokio::test]
async fn home_serves_channel_backfill_replaying_missed_messages() {
    // §11.14: the home replays its channel's message roots after a spoke's cursor
    // as `MESSAGE` events over the same bridge (the down-leg of `@as HISTORY`) —
    // the recovery path for a spoke that was down when they were minted.
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

    // Drain the two live mirror copies the bridge already received as they minted.
    for _ in 0..2 {
        loop {
            if bridge.recv_raw().await.contains("MESSAGE #general") {
                break;
            }
        }
    }

    // The spoke asks us (the home) to replay from the start (@as HISTORY).
    bridge.send("@as=alice HISTORY #general");

    // We replay both messages as home-minted MESSAGE events over the bridge.
    let mut bodies = Vec::new();
    while bodies.len() < 2 {
        let line = bridge.recv_raw().await;
        if line.contains("MESSAGE #general") {
            assert!(line.contains("test.example/"), "home-minted origin: {line}");
            if line.contains(":first") {
                bodies.push("first".to_string());
            } else if line.contains(":second") {
                bodies.push("second".to_string());
            }
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
    // The ADD echo now carries the reason (no LIST round-trip needed).
    assert!(matches!(&reply.event,
        Event::Netblocked { network, reason }
        if network.as_str() == "evil.example" && reason.as_deref() == Some("spam floods")));

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

    // REMOVE now echoes a distinct NETBLOCK-REMOVED (not a re-adding NETBLOCKED).
    op.send("NETBLOCK REMOVE evil.example");
    assert!(matches!(op.recv().await.event,
        Event::NetblockRemoved { network } if network.as_str() == "evil.example"));
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
    // Backfill serves the materialized view (§11.7); the wire form is always
    // materialized off the live path (v0.12 Part 4.1), so BATCH END is bare.
    assert!(matches!(bridge.recv().await.event, Event::BatchEnd { .. }));
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
    // The namespace *owner* is its moderator (operators no longer hold implicit
    // per-namespace authority). A namespace-wide mute covers every channel.
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    let root = root_key_b64();
    ada.send(&format!("@label=n;root={root} NS CREATE gaming public"));
    let Event::NsMeta { id, .. } = drain_until_label(&mut ada, "n").await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();
    ada.send(&format!("@label=c CHANNEL CREATE #{ns_id}/chat"));
    let Event::Policy { channel: chan, .. } = drain_until_label(&mut ada, "c").await.event else {
        panic!("expected POLICY");
    };

    // Joining the namespaced channel makes bob an ns member (drains MEMBER+POLICY).
    let mut bob = joined(&ctx, "bob", chan.as_str()).await;

    ada.send(&format!("@label=m MUTE ns:{ns_id} bob"));
    drain_until_label(&mut ada, "m").await;

    bob.send(&format!("MSG {chan} :hi"));
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
    let Event::Policy {
        channel: locked, ..
    } = op.recv().await.event
    else {
        panic!("expected POLICY");
    };
    op.send(&format!("JOIN {locked}"));
    op.recv().await; // MEMBER
    op.recv().await; // POLICY
    op.send(&format!("CHANNEL META {locked} posting :restricted"));
    op.recv().await; // CHANMETA

    // A normal member (no send grant) can't post in a restricted channel.
    let mut bob = joined(&ctx, "bob", locked.as_str()).await;
    bob.recv().await; // join-time CHANMETA posting:restricted (initial-state push)
    op.recv().await; // bob's MEMBER join broadcast
    bob.send(&format!("MSG {locked} :hello"));
    let Event::Err(e) = bob.expect_err(ErrCode::CapRequired).await.event else {
        panic!()
    };
    assert_eq!(e.context.as_deref(), Some("send"));

    // The grant path (the "both" story): granting `send` lets them post — and
    // REVOKE would take it away again.
    op.send(&format!("GRANT bob {locked} send"));
    op.recv().await; // TOKEN
    bob.send(&format!("MSG {locked} :now i can"));
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
    let Event::Policy {
        channel: cooldown, ..
    } = op.recv().await.event
    else {
        panic!("expected POLICY");
    };
    op.send(&format!("JOIN {cooldown}"));
    op.recv().await; // MEMBER
    op.recv().await; // POLICY

    let mut bob = joined(&ctx, "bob", cooldown.as_str()).await;
    op.recv().await; // bob's join broadcast
                     // Give bob `send`, so the freeze — not a missing cap — is what stops him.
    op.send(&format!("GRANT bob {cooldown} send"));
    op.recv().await; // TOKEN

    store.set_channel_frozen(&cooldown, true).await.unwrap();

    bob.send(&format!("MSG {cooldown} :can i talk"));
    let Event::Err(e) = bob.expect_err(ErrCode::Forbidden).await.event else {
        panic!()
    };
    assert_eq!(e.context.as_deref(), Some("frozen"));

    // The moderator (operator ⇒ holds ns-admin everywhere) still can.
    op.send(&format!("MSG {cooldown} :locked while we sort this out"));
    loop {
        if matches!(op.recv().await.event, Event::Message(ref m) if m.body.starts_with("locked")) {
            break;
        }
    }

    // Unfreezing restores bob's access — the freeze is reversible and left his
    // grant untouched.
    store.set_channel_frozen(&cooldown, false).await.unwrap();
    bob.send(&format!("MSG {cooldown} :thanks"));
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
    let Event::NsMeta { id, .. } = ada.recv().await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();
    ada.send(&format!("CHANNEL CREATE #{ns_id}/lobby"));
    assert!(matches!(
        ada.recv().await.event,
        Event::ChannelLayout { .. }
    )); // vanity
    let Event::Policy { channel: lobby, .. } = ada.recv().await.event else {
        panic!("expected POLICY");
    };
    ada.send(&format!("JOIN {lobby}"));
    ada.recv().await; // MEMBER
    ada.recv().await; // POLICY

    // bob is a delegated ns-admin — full moderation authority in the namespace.
    let mut bob = joined(&ctx, "bob", lobby.as_str()).await;
    ada.recv().await; // bob's join broadcast
    ada.send(&format!("GRANT bob ns:{ns_id} ns-admin"));
    ada.recv().await; // TOKEN

    // The freeze setter keys the namespace by its vanity name.
    let ns: weft_proto::NamespaceName = "gaming".parse().unwrap();
    store.set_namespace_frozen(&ns, true).await.unwrap();

    // Even an ns-admin is silenced by a full freeze.
    bob.send(&format!("MSG {lobby} :i'm an admin though"));
    let Event::Err(e) = bob.expect_err(ErrCode::Forbidden).await.event else {
        panic!()
    };
    assert_eq!(e.context.as_deref(), Some("frozen"));

    // The owner still speaks.
    ada.send(&format!("MSG {lobby} :everything is paused"));
    loop {
        if matches!(ada.recv().await.event, Event::Message(ref m) if m.body.starts_with("everything"))
        {
            break;
        }
    }

    // Lifting it restores the namespace.
    store.set_namespace_frozen(&ns, false).await.unwrap();
    bob.send(&format!("MSG {lobby} :back"));
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
    let Event::NsMeta { id, .. } = ada.recv().await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();
    // `general` is auto-seeded by NS CREATE (grab its handle); the owner adds two
    // more, one of them view-gated (hidden by permissions).
    let general = ada.channel_by_vanity(&ns_id, "general").await;
    let lounge = ada.create_channel(&ns_id, "lounge").await;
    let secret = ada.create_channel(&ns_id, "secret").await;
    ada.send(&format!("CHANNEL META {secret} view-gated :yes"));
    assert!(matches!(ada.recv().await.event, Event::Chanmeta { .. }));

    // A regular user joins the namespace → auto-joins the two visible channels.
    let mut bob = ready(&ctx, "bob").await;
    bob.send(&format!("NS JOIN {ns_id}"));
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
    assert!(joined.contains(general.as_str()));
    assert!(joined.contains(lounge.as_str()));
    assert!(
        !joined.contains(secret.as_str()),
        "a view-gated channel must not be auto-joined"
    );
}

#[tokio::test]
async fn part_hides_a_namespaced_channel_and_ns_leave_drops_membership() {
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@root={} NS CREATE gaming public", root_key_b64()));
    let Event::NsMeta { id, .. } = ada.recv().await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();
    let general = ada.create_channel(&ns_id, "chat").await;
    let clips = ada.create_channel(&ns_id, "clips").await;
    // Ada joins her own namespace so she can query rosters.
    ada.send(&format!("NS JOIN {ns_id}"));
    drain_until_ns_member(&mut ada).await;

    // Bob joins → NS-MEMBER carries the derived member count (ada + bob).
    let mut bob = ready(&ctx, "bob").await;
    bob.send(&format!("NS JOIN {ns_id}"));
    assert_eq!(drain_until_ns_member(&mut bob).await, Some(2));

    // Both are derived-in every channel with zero per-channel joins.
    ada.send(&format!("MEMBERS {clips}"));
    assert!(roster_names(&mut ada).await.contains("bob"));

    // Bob PARTs one channel → hidden. It drops him from that channel's derived
    // roster only; the other channel still shows him (hide is per-channel).
    bob.send(&format!("PART {clips}"));
    assert!(matches!(
        bob.recv().await.event,
        Event::Member {
            action: MemberAction::Part,
            ..
        }
    ));
    ada.send(&format!("MEMBERS {clips}"));
    let clips_roster = roster_names(&mut ada).await;
    assert!(
        !clips_roster.contains("bob"),
        "a hidden channel drops the hider"
    );
    assert!(clips_roster.contains("ada"));
    ada.send(&format!("MEMBERS {general}"));
    assert!(
        roster_names(&mut ada).await.contains("bob"),
        "hide is per-channel, not per-namespace"
    );

    // NS LEAVE drops membership entirely: NS-MEMBER part + gone from every
    // channel's derived roster.
    bob.send(&format!("@label=l NS LEAVE {ns_id}"));
    let reply = bob.recv().await;
    assert!(matches!(
        reply.event,
        Event::NsMember {
            action: MemberAction::Part,
            ..
        }
    ));
    assert_eq!(reply.label.as_deref(), Some("l"));
    ada.send(&format!("MEMBERS {general}"));
    assert!(
        !roster_names(&mut ada).await.contains("bob"),
        "NS LEAVE removes the account from all derived rosters"
    );
}

#[tokio::test]
async fn ns_join_accepts_a_vanity_name() {
    // §2.2: NS JOIN takes the id *or* the vanity name (so an unlisted namespace
    // stays joinable by exact name); the server resolves either to the id.
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    // `unlisted` — not surfaced by DISCOVER, so a joiner only has the name.
    let root = root_key_b64();
    ada.send(&format!("@label=n;root={root} NS CREATE gaming unlisted"));
    let Event::NsMeta { id, .. } = drain_until_label(&mut ada, "n").await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();

    // Bob joins by the vanity name; the NS-MEMBER echo carries the resolved id.
    let mut bob = ready(&ctx, "bob").await;
    bob.send("@label=j NS JOIN gaming");
    loop {
        let reply = bob.recv().await;
        match reply.event {
            Event::Member { .. } | Event::Policy { .. } => {}
            Event::NsMember {
                namespace, action, ..
            } => {
                assert_eq!(action, MemberAction::Join);
                assert_eq!(namespace.to_string(), ns_id, "vanity resolved to the id");
                assert_eq!(reply.label.as_deref(), Some("j"));
                break;
            }
            other => panic!("unexpected before NS-MEMBER: {other:?}"),
        }
    }

    // A bogus vanity is NO-SUCH-TARGET, exactly like a bogus id (anti-enum).
    bob.send("@label=x NS JOIN not-a-real-server");
    bob.expect_err(ErrCode::NoSuchTarget).await;
}

#[tokio::test]
async fn owner_cannot_leave_their_namespace() {
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    let root = root_key_b64();
    ada.send(&format!("@label=n;root={root} NS CREATE gaming public"));
    let Event::NsMeta { id, .. } = drain_until_label(&mut ada, "n").await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();
    ada.send(&format!("@label=c CHANNEL CREATE #{ns_id}/chat"));
    let Event::Policy { channel: chan, .. } = drain_until_label(&mut ada, "c").await.event else {
        panic!("expected POLICY");
    };
    // Join makes the owner a namespace member; leaving would then orphan it.
    ada.send(&format!("@label=j JOIN {chan}"));
    drain_until_label(&mut ada, "j").await;

    ada.send(&format!("@label=l NS LEAVE {ns_id}"));
    let reply = drain_until_label(&mut ada, "l").await;
    assert!(
        matches!(&reply.event, Event::Err(err) if err.code == ErrCode::Policy),
        "the owner can't leave their own namespace, got {reply:?}"
    );
}

#[tokio::test]
async fn ns_welcome_channel_greets_new_members() {
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    let bob = ready(&ctx, "bob").await;
    let root = root_key_b64();
    ada.send(&format!("@label=n;root={root} NS CREATE gaming public"));
    let Event::NsMeta { id, .. } = drain_until_label(&mut ada, "n").await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();
    ada.send(&format!("@label=c1 CHANNEL CREATE #{ns_id}/general"));
    drain_until_label(&mut ada, "c1").await;
    ada.send(&format!("@label=c2 CHANNEL CREATE #{ns_id}/welcome"));
    let Event::Policy {
        channel: welcome, ..
    } = drain_until_label(&mut ada, "c2").await.event
    else {
        panic!("expected POLICY");
    };
    ada.send(&format!("@label=w NS META {ns_id} welcome :{welcome}"));
    drain_until_label(&mut ada, "w").await;
    // ada watches the welcome channel so she receives the greeting broadcast.
    ada.send(&format!("@label=jw JOIN {welcome}"));
    drain_until_label(&mut ada, "jw").await;

    // bob joins the namespace → a "welcome" system line lands in the welcome channel.
    bob.send(&format!("@label=j NS JOIN {ns_id}"));

    // recv() skips system messages, so read raw and match bob's welcome line
    // (ada's own first join fired one too — that's expected).
    loop {
        let raw = ada.recv_raw().await;
        let reply = Reply::parse(&raw).expect("parseable");
        if let Event::Message(m) = &reply.event {
            if m.meta.system.as_deref() == Some("welcome") && m.sender.account.as_str() == "bob" {
                assert!(
                    matches!(&m.target, weft_proto::Target::Channel(c) if c.as_str() == welcome.as_str()),
                    "welcome posts to the designated channel, got {:?}",
                    m.target
                );
                break;
            }
        }
    }
}

#[tokio::test]
async fn sync_fresh_skeleton_then_delta_catches_up() {
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@root={} NS CREATE gaming public", root_key_b64()));
    let Event::NsMeta { id, .. } = ada.recv().await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();
    let general = ada.create_channel(&ns_id, "chat").await;
    ada.send(&format!("NS JOIN {ns_id}"));
    drain_until_ns_member(&mut ada).await;

    // Fresh SYNC → skeleton (NS-META + CHANNEL-LAYOUT + POLICY) + a cursor.
    ada.send("@label=s SYNC preview=0");
    let mut saw_layout = false;
    let mut saw_policy = false;
    let cursor = loop {
        let ev = ada.recv().await;
        match ev.event {
            Event::NsMeta { .. } => {}
            // v0.13: SYNC now replays ns membership so channel-less servers show.
            Event::NsMember { .. } => {}
            Event::ChannelLayout { channel, .. } => {
                saw_layout |= channel.as_str() == general.as_str();
            }
            Event::Policy { .. } => saw_policy = true,
            Event::Marked { .. } | Event::UnreadCounts { .. } => {}
            Event::SyncEnd { cursor } => {
                assert_eq!(ev.label.as_deref(), Some("s"), "SYNC END echoes the label");
                break cursor;
            }
            other => panic!("unexpected in skeleton: {other:?}"),
        }
    };
    assert!(saw_layout, "skeleton carries the channel layout");
    assert!(saw_policy, "skeleton carries the channel policy");

    // Post a message after the cursor, drain its own echo.
    ada.send(&format!("MSG {general} :hello"));
    loop {
        if let Event::Message(m) = ada.recv().await.event {
            assert_eq!(m.body, "hello");
            break;
        }
    }

    // Delta SYNC since=<cursor> re-delivers the message (materialized upsert).
    ada.send(&format!("SYNC since={cursor} preview=0"));
    let mut got = false;
    loop {
        match ada.recv().await.event {
            Event::Message(m) => got |= m.body == "hello",
            Event::Reactions { .. } | Event::Deleted { .. } => {}
            Event::SyncEnd { .. } => break,
            other => panic!("unexpected in delta: {other:?}"),
        }
    }
    assert!(got, "delta delivers a message posted after the cursor");
}

#[tokio::test]
async fn sync_delta_catches_an_edit_of_an_old_message() {
    // Acceptance #5: an edit of a message OLDER than the cursor is delivered by
    // `SYNC since=` (re-materialized with edited=), which ULID paging misses.
    let ctx = ctx(&["#general"]);
    let mut ada = joined(&ctx, "ada", "#general").await;
    ada.send("@label=m MSG #general :original");
    let msgid = loop {
        if let Event::Message(m) = ada.recv().await.event {
            break m.msgid.to_string();
        }
    };

    // Snapshot the cursor AFTER the message exists.
    ada.send("SYNC preview=0");
    let cursor = loop {
        if let Event::SyncEnd { cursor } = ada.recv().await.event {
            break cursor;
        }
    };

    // Edit the (now "old") message; drain the live EDITED.
    ada.send(&format!("EDIT {msgid} :fixed"));
    loop {
        if matches!(ada.recv().await.event, Event::Edited { .. }) {
            break;
        }
    }

    // The delta re-materializes it: final body + edited count.
    ada.send(&format!("SYNC since={cursor} preview=0"));
    let mut edited = None;
    loop {
        match ada.recv().await.event {
            Event::Message(m) if m.msgid.to_string() == msgid => edited = Some((m.body, m.edited)),
            Event::SyncEnd { .. } => break,
            _ => {}
        }
    }
    let (body, edited) = edited.expect("delta re-serves the edited message");
    assert_eq!(body, "fixed");
    assert_eq!(edited, Some(1));
}

#[tokio::test]
async fn sync_delta_catches_up_a_dm() {
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    let mut bob = ready(&ctx, "bob").await;

    // Bob snapshots a cursor before any DM exists.
    bob.send("SYNC preview=0");
    let c0 = loop {
        if let Event::SyncEnd { cursor } = bob.recv().await.event {
            break cursor;
        }
    };

    // Ada DMs bob; drain both echoes.
    ada.send("MSG @bob :hey");
    loop {
        if matches!(ada.recv().await.event, Event::Message(_)) {
            break;
        }
    }
    loop {
        if let Event::Message(m) = bob.recv().await.event {
            assert_eq!(m.body, "hey");
            break;
        }
    }

    // Bob's delta includes the DM scope, so a reconnect catches it up.
    bob.send(&format!("SYNC since={c0} preview=0"));
    let mut got = false;
    loop {
        match bob.recv().await.event {
            Event::Message(m) => got |= m.body == "hey",
            Event::SyncEnd { .. } => break,
            _ => {}
        }
    }
    assert!(
        got,
        "the SYNC delta catches up a DM received after the cursor"
    );
}

#[tokio::test]
async fn channel_create_pushes_layout_to_online_ns_members() {
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@root={} NS CREATE gaming public", root_key_b64()));
    let Event::NsMeta { id, .. } = ada.recv().await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();
    ada.send(&format!("CHANNEL CREATE #{ns_id}/chat"));
    assert!(matches!(
        ada.recv().await.event,
        Event::ChannelLayout { .. }
    )); // vanity
    assert!(matches!(ada.recv().await.event, Event::Policy { .. }));

    // Bob joins the namespace (an online member).
    let mut bob = ready(&ctx, "bob").await;
    bob.send(&format!("NS JOIN {ns_id}"));
    drain_until_ns_member(&mut bob).await;

    // Ada creates a NEW channel → bob receives its layout + policy live, with no
    // reconnect (acceptance #1: derived membership makes it his immediately).
    ada.send(&format!("CHANNEL CREATE #{ns_id}/clips"));
    assert!(matches!(
        ada.recv().await.event,
        Event::ChannelLayout { .. }
    )); // vanity
    let Event::Policy { channel: clips, .. } = ada.recv().await.event else {
        panic!("expected POLICY");
    };
    let mut layout = false;
    let mut policy = false;
    for _ in 0..2 {
        match bob.recv().await.event {
            Event::ChannelLayout { channel, .. } => layout |= channel.as_str() == clips.as_str(),
            Event::Policy { channel, .. } => policy |= channel.as_str() == clips.as_str(),
            other => panic!("unexpected push: {other:?}"),
        }
    }
    assert!(
        layout,
        "a new channel's layout is pushed to online ns members"
    );
    assert!(policy, "…and its policy");
}

#[tokio::test]
async fn sync_delta_catches_a_channel_metadata_change() {
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@root={} NS CREATE gaming public", root_key_b64()));
    let Event::NsMeta { id, .. } = ada.recv().await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();
    let general = ada.create_channel(&ns_id, "chat").await;
    ada.send(&format!("NS JOIN {ns_id}"));
    drain_until_ns_member(&mut ada).await;

    // Snapshot a cursor, then re-category the channel (a layout change).
    ada.send("SYNC preview=0");
    let c0 = loop {
        if let Event::SyncEnd { cursor } = ada.recv().await.event {
            break cursor;
        }
    };
    ada.send(&format!("CHANNEL META {general} category :Voice"));

    // The delta re-serves the channel's layout + policy. A *labeled* SYNC lets
    // us ignore the live CHANNEL-LAYOUT broadcast from the change above — only
    // the delta's own rows echo the label.
    ada.send(&format!("@label=s2 SYNC since={c0} preview=0"));
    let mut layout = false;
    let mut policy = false;
    loop {
        let ev = ada.recv().await;
        if ev.label.as_deref() != Some("s2") {
            continue;
        }
        match ev.event {
            Event::ChannelLayout {
                channel, category, ..
            } if channel.as_str() == general.as_str() => {
                layout = category.as_deref() == Some("Voice");
            }
            Event::Policy { channel, .. } if channel.as_str() == general.as_str() => policy = true,
            Event::SyncEnd { .. } => break,
            _ => {}
        }
    }
    assert!(
        layout,
        "delta re-serves the changed channel's layout (new category)"
    );
    assert!(policy, "…and its policy");
}

#[tokio::test]
async fn ns_join_with_no_visible_channels_is_no_such_target() {
    let ctx = ctx(&[]);
    let mut bob = ready(&ctx, "bob").await;
    // A well-formed but nonexistent ns id → uniform NO-SUCH-TARGET (invariant 1).
    let ghost = weft_proto::Ulid::new().to_string().to_ascii_lowercase();
    bob.send(&format!("@label=j NS JOIN {ghost}"));
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
        let ev = ada.recv_any().await;
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
            &ada.recv_any().await.event,
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
        match ada.recv_any().await.event {
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
        match bob.recv_any().await.event {
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
        role,
        ..
    } = &ev.event
    else {
        panic!("expected ROLE, got {ev:?}");
    };
    assert_eq!(name, "Moderator");
    assert_eq!(color, "#e8b93d");
    assert_eq!(scope, "*");
    assert_eq!(caps, "mute,ban,kick");
    let role_id = role.to_string();
    assert!(matches!(root.recv().await.event, Event::BatchEnd { .. }));

    // Assign it to bob (by the minted role id) → grants the bundle (a Token).
    root.send(&format!("@label=a ROLE ASSIGN * bob {role_id}"));
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
    let role_id = role_id_named(&mut root, "Moderator").await;

    // Assign it to a *federated* user (account@network) — membership recorded by
    // the network-qualified handle, caps granted to the foreign subject (§10.4).
    root.send(&format!(
        "@label=a ROLE ASSIGN * alice@peer.example {role_id}"
    ));
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
    // v0.13: ROLE-MEMBER carries the role **id** (names aren't unique).
    assert_eq!(roles, &role_id);
}

#[tokio::test]
async fn renaming_a_role_keeps_its_members_and_caps() {
    let ctx = ctx_ops(&["#general"], &["root"]);
    let mut root = ready(&ctx, "root").await;
    let _bob = ready(&ctx, "bob").await;

    root.send("ROLE CREATE * #e8b93d mute,ban :Moderator");
    let role_id = role_id_named(&mut root, "Moderator").await;
    root.send(&format!("@label=a ROLE ASSIGN * bob {role_id}"));
    root.recv().await; // Token

    // Rename (v0.13: folded into ROLE UPDATE, addressed by the role id) → the
    // ROLES batch comes back under the new name, definition intact.
    root.send(&format!(
        "@label=r ROLE UPDATE * {role_id} #e8b93d mute,ban :Head Moderator"
    ));
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
    // The id is stable across a rename, so membership still reports it.
    assert_eq!(roles, &role_id);

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
    root.send("ROLE CREATE * #e8b93d mute :Moderator");
    role_id_named(&mut root, "Moderator").await; // drain the batch
    root.send("ROLE CREATE * #e8b93d mute :Helper");
    let helper_id = role_id_named(&mut root, "Helper").await;

    // Merging two bundles under one name is not a rename (renaming Helper onto the
    // existing "Moderator" via ROLE UPDATE).
    root.send(&format!(
        "@label=x ROLE UPDATE * {helper_id} #e8b93d mute :Moderator"
    ));
    root.expect_err(ErrCode::Policy).await;

    // An absent source id is NO-SUCH-TARGET, same as any other hidden/absent target.
    let ghost = weft_proto::Ulid::new().to_string().to_ascii_lowercase();
    root.send(&format!(
        "@label=y ROLE UPDATE * {ghost} #e8b93d mute :Phantom"
    ));
    root.expect_err(ErrCode::NoSuchTarget).await;
}

#[tokio::test]
async fn role_rename_needs_admin_authority() {
    let ctx = ctx_ops(&["#general"], &["root"]);
    let mut root = ready(&ctx, "root").await;
    root.send("ROLE CREATE * #fff send :Member");
    let role_id = role_id_named(&mut root, "Member").await;

    let mut mallory = ready(&ctx, "mallory").await; // no caps
    mallory.send(&format!(
        "@label=x ROLE UPDATE * {role_id} #fff send :Owner"
    ));
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

/// v0.13: drain a `ROLES` reply batch (the ack for ROLE CREATE/UPDATE) and return
/// the server-minted ULID id of the role whose display name is `name`. Stops at
/// the batch's `BATCH END`, so it works for both labeled and unlabeled batches.
async fn role_id_named(c: &mut Client, name: &str) -> String {
    let mut found: Option<String> = None;
    loop {
        let r = c.recv().await;
        match &r.event {
            Event::Role { role, name: n, .. } if n == name => found = Some(role.to_string()),
            Event::BatchEnd { .. } => {
                return found.unwrap_or_else(|| panic!("no role named {name} in the batch"));
            }
            _ => {}
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
    let Event::NsMeta { id, .. } = drain_until_label(&mut ada, "n").await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();
    ada.send(&format!("@label=c CHANNEL CREATE #{ns_id}/stage"));
    let Event::Policy { channel: stage, .. } = drain_until_label(&mut ada, "c").await.event else {
        panic!("expected POLICY");
    };

    // A namespace role (react) plus a same-named *channel* role (send) — the
    // channel role is the role's per-channel permission.
    ada.send(&format!(
        "@label=r1 ROLE CREATE ns:{ns_id} #e8b93d react :Speaker"
    ));
    let speaker = role_id_named(&mut ada, "Speaker").await;
    ada.send(&format!(
        "@label=r2 ROLE CREATE {stage} #e8b93d send :Speaker"
    ));
    drain_until_label(&mut ada, "r2").await;

    // Assigning the namespace role should propagate the channel permission.
    ada.send(&format!("@label=a ROLE ASSIGN ns:{ns_id} bob {speaker}"));
    drain_until_label(&mut ada, "a").await;

    ada.send(&format!("@label=q CAPS bob {stage}"));
    let ev = drain_until_label(&mut ada, "q").await;
    let Event::Caps { caps, .. } = &ev.event else {
        panic!("expected CAPS, got {ev:?}");
    };
    assert!(
        caps.contains("send"),
        "bob gains send in the channel via the namespace role, got {caps}"
    );
}

/// Collect an `NS INFO MEMBERS` batch: skip to BATCH START, gather each
/// `NS-MEMBER-INFO` as `(account, joined_ms, roles)`, stop at BATCH END.
async fn ns_member_info(client: &mut Client) -> Vec<(String, u64, Vec<String>)> {
    loop {
        if matches!(client.recv().await.event, Event::BatchStart { .. }) {
            break;
        }
    }
    let mut out = Vec::new();
    loop {
        match client.recv().await.event {
            Event::NsMemberInfo {
                user,
                joined_ms,
                roles,
                ..
            } => out.push((user.account.as_str().to_string(), joined_ms, roles)),
            Event::BatchEnd { .. } => break,
            other => panic!("unexpected in NS INFO MEMBERS batch: {other:?}"),
        }
    }
    out
}

#[tokio::test]
async fn ns_info_members_lists_the_roster_with_roles_and_join_times() {
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    let root = root_key_b64();

    // Ada owns gaming (holds ns-admin implicitly) and joins it. NS JOIN needs a
    // visible channel to land on, so give it one first.
    ada.send(&format!("@label=n;root={root} NS CREATE gaming public"));
    let Event::NsMeta { id, .. } = drain_until_label(&mut ada, "n").await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();
    ada.send(&format!("@label=c CHANNEL CREATE #{ns_id}/general"));
    drain_until_label(&mut ada, "c").await;
    ada.send(&format!("@label=j NS JOIN {ns_id}"));
    drain_until_label(&mut ada, "j").await;

    // Bob joins as a plain member.
    let mut bob = ready(&ctx, "bob").await;
    bob.send(&format!("@label=jb NS JOIN {ns_id}"));
    drain_until_label(&mut bob, "jb").await;

    // A moderator role, assigned to bob.
    ada.send(&format!(
        "@label=r ROLE CREATE ns:{ns_id} #e8654f mute,kick :Moderator"
    ));
    let mod_id = role_id_named(&mut ada, "Moderator").await;
    ada.send(&format!("@label=a ROLE ASSIGN ns:{ns_id} bob {mod_id}"));
    drain_until_label(&mut ada, "a").await;

    // Owner queries the roster.
    ada.send(&format!("@label=i NS INFO MEMBERS {ns_id}"));
    let roster = ns_member_info(&mut ada).await;

    let ada_row = roster
        .iter()
        .find(|(n, ..)| n == "ada")
        .expect("ada present");
    let bob_row = roster
        .iter()
        .find(|(n, ..)| n == "bob")
        .expect("bob present");
    // Join times were stamped on NS JOIN (non-zero unix-ms).
    assert!(ada_row.1 > 0 && bob_row.1 > 0, "join times recorded");
    // Assigned roles are reported by **id** (v0.13); the owner holds caps
    // implicitly, not via an assignment, so it lists no roles.
    assert_eq!(bob_row.2, vec![mod_id.clone()]);
    assert!(ada_row.2.is_empty(), "owner has no *assigned* roles");
}

#[tokio::test]
async fn ns_info_members_requires_a_moderation_cap() {
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    let root = root_key_b64();
    ada.send(&format!("@label=n;root={root} NS CREATE gaming public"));
    let Event::NsMeta { id, .. } = drain_until_label(&mut ada, "n").await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();
    ada.send(&format!("@label=c CHANNEL CREATE #{ns_id}/general"));
    drain_until_label(&mut ada, "c").await;

    // A plain member (no moderation cap) is refused the roster.
    let mut carol = ready(&ctx, "carol").await;
    carol.send(&format!("@label=jc NS JOIN {ns_id}"));
    drain_until_label(&mut carol, "jc").await;
    carol.send(&format!("@label=i NS INFO MEMBERS {ns_id}"));
    assert_eq!(
        carol
            .expect_err(ErrCode::CapRequired)
            .await
            .label
            .as_deref(),
        Some("i")
    );
}

#[tokio::test]
async fn adding_a_channel_permission_propagates_to_existing_holders() {
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    let _bob = ready(&ctx, "bob").await;
    let root = root_key_b64();
    ada.send(&format!("@label=n;root={root} NS CREATE gaming public"));
    let Event::NsMeta { id, .. } = drain_until_label(&mut ada, "n").await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();
    ada.send(&format!("@label=c CHANNEL CREATE #{ns_id}/stage"));
    let Event::Policy { channel: stage, .. } = drain_until_label(&mut ada, "c").await.event else {
        panic!("expected POLICY");
    };
    ada.send(&format!(
        "@label=r1 ROLE CREATE ns:{ns_id} #e8b93d react :Speaker"
    ));
    let speaker = role_id_named(&mut ada, "Speaker").await;

    // Assign the role FIRST (bob holds react at ns:<id>, no channel perm yet).
    ada.send(&format!("@label=a ROLE ASSIGN ns:{ns_id} bob {speaker}"));
    drain_until_label(&mut ada, "a").await;

    // THEN add the channel permission — it must reach bob with no re-assignment.
    ada.send(&format!(
        "@label=r2 ROLE CREATE {stage} #e8b93d send :Speaker"
    ));
    drain_until_label(&mut ada, "r2").await;

    ada.send(&format!("@label=q CAPS bob {stage}"));
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
async fn everyone_role_grants_baseline_caps_to_members() {
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    let mut bob = ready(&ctx, "bob").await;
    let root = root_key_b64();

    ada.send(&format!("@label=n;root={root} NS CREATE gaming public"));
    let Event::NsMeta { id, .. } = drain_until_label(&mut ada, "n").await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();
    ada.send(&format!("@label=c CHANNEL CREATE #{ns_id}/general"));
    drain_until_label(&mut ada, "c").await;
    // A fresh namespace seeds @everyone with `send,invite`; narrow it to just
    // `send` so we can exercise the baseline gate on `invite`.
    ada.send(&format!(
        "@label=r0 ROLE CREATE ns:{ns_id} #99aab5 send :everyone"
    ));
    drain_until_label(&mut ada, "r0").await;

    // bob joins the namespace → becomes a member (implicitly holds @everyone).
    bob.send(&format!("@label=j NS JOIN {ns_id}"));
    drain_until_label(&mut bob, "j").await;

    // @everyone lacks `invite` → bob can't mint an invite.
    bob.send(&format!("@label=e1 INVITE MINT ns:{ns_id}"));
    let e1 = drain_until_label(&mut bob, "e1").await;
    assert!(
        matches!(&e1.event, Event::Err(err) if err.code == ErrCode::CapRequired),
        "member without @everyone caps is denied, got {e1:?}"
    );

    // Owner sets the implicit @everyone role's caps to include `invite`.
    ada.send(&format!(
        "@label=r ROLE CREATE ns:{ns_id} #99aab5 send,invite :everyone"
    ));
    drain_until_label(&mut ada, "r").await;

    // Now bob — a member, with no role *assignment* — gains the baseline cap.
    bob.send(&format!("@label=e2 INVITE MINT ns:{ns_id}"));
    let e2 = drain_until_label(&mut bob, "e2").await;
    assert!(
        matches!(e2.event, Event::Invited { .. }),
        "member gains @everyone's invite cap with no assignment, got {e2:?}"
    );

    // A non-member gets nothing from @everyone.
    let mut carol = ready(&ctx, "carol").await;
    carol.send(&format!("@label=e3 INVITE MINT ns:{ns_id}"));
    let e3 = drain_until_label(&mut carol, "e3").await;
    assert!(
        matches!(&e3.event, Event::Err(err) if err.code == ErrCode::CapRequired),
        "a non-member is unaffected by @everyone, got {e3:?}"
    );
}

#[tokio::test]
async fn channel_everyone_role_grants_a_per_channel_baseline() {
    // The channel-permission editor's @everyone target: a channel-scoped
    // `everyone` role is honored as a per-channel baseline, distinct from the
    // namespace-wide @everyone.
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    let mut bob = ready(&ctx, "bob").await;
    let root = root_key_b64();

    ada.send(&format!("@label=n;root={root} NS CREATE gaming public"));
    let Event::NsMeta { id, .. } = drain_until_label(&mut ada, "n").await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();
    ada.send(&format!("@label=c CHANNEL CREATE #{ns_id}/chat"));
    let Event::Policy {
        channel: general, ..
    } = drain_until_label(&mut ada, "c").await.event
    else {
        panic!("expected POLICY");
    };
    ada.send(&format!(
        "@label=r CHANNEL META {general} posting :restricted"
    ));
    drain_until_label(&mut ada, "r").await;
    // A fresh namespace seeds @everyone with `send`; strip it so the restricted
    // channel actually gates and we can isolate the *channel*-level baseline.
    ada.send(&format!(
        "@label=r0 ROLE CREATE ns:{ns_id} #99aab5 invite :everyone"
    ));
    drain_until_label(&mut ada, "r0").await;

    bob.send(&format!("@label=j NS JOIN {ns_id}"));
    drain_until_label(&mut bob, "j").await;
    bob.send(&format!("@label=jc JOIN {general}"));
    drain_until_label(&mut bob, "jc").await;

    // Restricted + no send anywhere → posting is denied.
    bob.send(&format!("@label=m1 MSG {general} :hello"));
    let m1 = drain_until_label(&mut bob, "m1").await;
    assert!(
        matches!(&m1.event, Event::Err(err) if err.code == ErrCode::CapRequired),
        "member without a channel baseline is denied, got {m1:?}"
    );

    // Owner sets the *channel's* @everyone role to include `send`.
    ada.send(&format!(
        "@label=e ROLE CREATE {general} #99aab5 send :everyone"
    ));
    drain_until_label(&mut ada, "e").await;

    // Now bob — a member with no assignment or grant — can post, purely from
    // the channel-scoped baseline.
    bob.send(&format!("MSG {general} :now i can"));
    loop {
        if matches!(bob.recv().await.event, Event::Message(ref m) if m.body == "now i can") {
            break;
        }
    }
}

#[tokio::test]
async fn grants_lists_member_overrides_but_not_role_holders() {
    // The channel-permission editor's member list: GRANTS surfaces individual
    // overrides, filtering out members whose channel caps come from a role.
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    let mut bob = ready(&ctx, "bob").await;
    let mut carol = ready(&ctx, "carol").await;
    let root = root_key_b64();

    ada.send(&format!("@label=n;root={root} NS CREATE gaming public"));
    let Event::NsMeta { id, .. } = drain_until_label(&mut ada, "n").await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();
    ada.send(&format!("@label=c CHANNEL CREATE #{ns_id}/chat"));
    let Event::Policy {
        channel: general, ..
    } = drain_until_label(&mut ada, "c").await.event
    else {
        panic!("expected POLICY");
    };

    bob.send(&format!("@label=jb NS JOIN {ns_id}"));
    drain_until_label(&mut bob, "jb").await;
    carol.send(&format!("@label=jc NS JOIN {ns_id}"));
    drain_until_label(&mut carol, "jc").await;

    // carol holds an ns role that then gets a channel override → she's covered
    // by the role (propagation), not a genuine individual override.
    ada.send(&format!(
        "@label=r1 ROLE CREATE ns:{ns_id} #e8b93d send :speaker"
    ));
    let speaker = role_id_named(&mut ada, "speaker").await;
    ada.send(&format!("@label=a ROLE ASSIGN ns:{ns_id} carol {speaker}"));
    drain_until_label(&mut ada, "a").await;
    ada.send(&format!(
        "@label=r2 ROLE CREATE {general} #e8b93d send :speaker"
    ));
    drain_until_label(&mut ada, "r2").await;

    // bob gets an individual channel override (holds no role).
    ada.send(&format!("@label=g GRANT bob {general} send"));
    drain_until_label(&mut ada, "g").await;

    // GRANTS lists bob (genuine override) but not carol (role-covered).
    ada.send(&format!("@label=q GRANTS {general}"));
    let list = grant_infos(&mut ada).await;
    assert_eq!(
        list.len(),
        1,
        "only bob is a genuine override, got {list:?}"
    );
    assert_eq!(list[0].0, "bob");
    assert!(
        list[0].1.contains("send"),
        "bob's override caps, got {list:?}"
    );

    // The roster is ns-admin gated — a normal member can't enumerate it.
    bob.send(&format!("@label=deny GRANTS {general}"));
    let deny = drain_until_label(&mut bob, "deny").await;
    assert!(
        matches!(&deny.event, Event::Err(err) if err.code == ErrCode::CapRequired),
        "grants roster is ns-admin gated, got {deny:?}"
    );
}

#[tokio::test]
async fn delete_any_lets_a_moderator_remove_another_members_message() {
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    let mut bob = ready(&ctx, "bob").await;
    let root = root_key_b64();

    ada.send(&format!("@label=n;root={root} NS CREATE gaming public"));
    let Event::NsMeta { id, .. } = drain_until_label(&mut ada, "n").await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();
    ada.send(&format!("@label=c CHANNEL CREATE #{ns_id}/chat"));
    let Event::Policy {
        channel: general, ..
    } = drain_until_label(&mut ada, "c").await.event
    else {
        panic!("expected POLICY");
    };
    ada.send(&format!("@label=ja JOIN {general}"));
    drain_until_label(&mut ada, "ja").await;

    bob.send(&format!("@label=j NS JOIN {ns_id}"));
    drain_until_label(&mut bob, "j").await;
    bob.send(&format!("@label=jc JOIN {general}"));
    drain_until_label(&mut bob, "jc").await;

    // bob posts; his own echo carries the msgid.
    bob.send(&format!("@label=m MSG {general} :hi"));
    let echo = drain_until_label(&mut bob, "m").await;
    let Event::Message(m) = echo.event else {
        panic!("expected message echo, got {echo:?}");
    };
    let msgid = m.msgid.to_string();

    // The owner holds every cap (incl. delete-any) → removes bob's message.
    ada.send(&format!("@label=d DELETE {msgid}"));
    let d = drain_until_label(&mut ada, "d").await;
    assert!(
        matches!(d.event, Event::Deleted { .. }),
        "owner deletes another member's message via delete-any, got {d:?}"
    );

    // A plain member (no delete-any) cannot delete someone else's message.
    bob.send(&format!("@label=m2 MSG {general} :hi again"));
    let echo2 = drain_until_label(&mut bob, "m2").await;
    let Event::Message(m2) = echo2.event else {
        panic!("expected message echo, got {echo2:?}");
    };
    let mid2 = m2.msgid.to_string();

    let mut carol = ready(&ctx, "carol").await;
    carol.send(&format!("@label=jn NS JOIN {ns_id}"));
    drain_until_label(&mut carol, "jn").await;
    carol.send(&format!("@label=jcc JOIN {general}"));
    drain_until_label(&mut carol, "jcc").await;
    carol.send(&format!("@label=dn DELETE {mid2}"));
    let dn = drain_until_label(&mut carol, "dn").await;
    assert!(
        matches!(&dn.event, Event::Err(err) if err.code == ErrCode::CapRequired),
        "a member without delete-any is denied, got {dn:?}"
    );
}

#[tokio::test]
async fn server_nicknames_are_cap_gated() {
    let ctx = ctx(&[]);
    let mut ada = ready(&ctx, "ada").await;
    let mut bob = ready(&ctx, "bob").await;
    let root = root_key_b64();
    ada.send(&format!("@label=n;root={root} NS CREATE gaming public"));
    let Event::NsMeta { id, .. } = drain_until_label(&mut ada, "n").await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();
    ada.send(&format!("@label=c CHANNEL CREATE #{ns_id}/general"));
    drain_until_label(&mut ada, "c").await;
    bob.send(&format!("@label=j NS JOIN {ns_id}"));
    drain_until_label(&mut bob, "j").await;

    // No `nick` cap → bob can't even set his own nickname.
    bob.send(&format!("@label=e1 NICK ns:{ns_id} bob :Cool Bob"));
    let e1 = drain_until_label(&mut bob, "e1").await;
    assert!(
        matches!(&e1.event, Event::Err(err) if err.code == ErrCode::CapRequired),
        "own nick needs the `nick` cap, got {e1:?}"
    );

    // Give @everyone the `nick` cap → bob can set his own.
    ada.send(&format!(
        "@label=r ROLE CREATE ns:{ns_id} #99aab5 nick :everyone"
    ));
    drain_until_label(&mut ada, "r").await;
    bob.send(&format!("@label=e2 NICK ns:{ns_id} bob :Cool Bob"));
    let e2 = drain_until_label(&mut bob, "e2").await;
    assert!(
        matches!(&e2.event, Event::Nick { nick, .. } if nick == "Cool Bob"),
        "with `nick`, own nickname is set, got {e2:?}"
    );

    // `nick` does NOT let bob rename another member (that needs `manage-nicks`).
    bob.send(&format!("@label=e3 NICK ns:{ns_id} ada :Boss"));
    let e3 = drain_until_label(&mut bob, "e3").await;
    assert!(
        matches!(&e3.event, Event::Err(err) if err.code == ErrCode::CapRequired),
        "renaming others needs `manage-nicks`, got {e3:?}"
    );

    // The owner can rename anyone.
    ada.send(&format!("@label=e4 NICK ns:{ns_id} bob :Renamed"));
    let e4 = drain_until_label(&mut ada, "e4").await;
    assert!(
        matches!(&e4.event, Event::Nick { nick, .. } if nick == "Renamed"),
        "owner renames any member, got {e4:?}"
    );
}

#[tokio::test]
async fn roles_are_explicit_membership_not_derived() {
    let ctx = ctx_ops(&["#general"], &["root"]);
    let mut root = ready(&ctx, "root").await;
    let _bob = ready(&ctx, "bob").await;

    root.send("@label=c ROLE CREATE * #e8b93d mute,ban :Mod");
    let mod_id = role_id_named(&mut root, "Mod").await;

    // bob holds no roles yet, even though the operator implicitly has every cap.
    root.send("@label=q1 ROLES-OF * bob");
    let ev = drain_until_label(&mut root, "q1").await;
    assert!(matches!(&ev.event, Event::RoleMember { roles, .. } if roles.is_empty()));

    // Assign, then it shows; unassign, then it's gone.
    root.send(&format!("@label=a ROLE ASSIGN * bob {mod_id}"));
    drain_until_label(&mut root, "a").await;
    root.send("@label=q2 ROLES-OF * bob");
    let ev = drain_until_label(&mut root, "q2").await;
    assert!(
        matches!(&ev.event, Event::RoleMember { roles, .. } if roles == &mod_id),
        "got {ev:?}"
    );

    root.send(&format!("@label=u ROLE UNASSIGN * bob {mod_id}"));
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
        ..
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
        ..
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

/// Create a voice channel with desired vanity `name` via a fresh operator session,
/// which then drops (the channel persists in the registry + store). Returns a
/// voice-enabled ctx and the channel's minted canonical `#<chan-id>` (v0.13).
async fn voice_ctx_with(name: &str) -> (Arc<ServerCtx>, ChannelName) {
    let ctx = ctx_voice(&[], &["boss"]);
    let mut boss = ready_op(&ctx, "boss").await;
    boss.send(&format!("CHANNEL CREATE {name} voice"));
    let Event::Policy { channel, .. } = boss.recv().await.event else {
        panic!("expected POLICY");
    };
    (ctx, channel)
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
    let (ctx, lounge) = voice_ctx_with("#lounge").await;
    let mut alice = ready(&ctx, "alice").await;
    alice.send(&format!("@label=j JOIN {lounge}"));
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
    let (ctx, lounge) = voice_ctx_with("#lounge").await;

    let mut bob = ready(&ctx, "bob").await;
    bob.send(&format!("VOICE JOIN {lounge}"));
    assert!(matches!(bob.recv().await.event, Event::VoiceOffer { .. }));
    // bob is a *healthy* client: he keeps PINGing, so only alice goes quiet.
    let _bob_alive = bob.keepalive();

    let mut alice = ready(&ctx, "alice").await;
    alice.send(&format!("VOICE JOIN {lounge}"));
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
    let (ctx, lounge) = voice_ctx_with("#lounge").await;

    // bob joins voice first (subscribing to the room).
    let mut bob = ready(&ctx, "bob").await;
    bob.send(&format!("VOICE JOIN {lounge}"));
    assert!(matches!(bob.recv().await.event, Event::VoiceOffer { .. }));

    // alice joins voice → labeled VOICE OFFER with a token, endpoint absent.
    let mut alice = ready(&ctx, "alice").await;
    alice.send(&format!("@label=v1 VOICE JOIN {lounge}"));
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
    assert_eq!(channel.as_str(), lounge.as_str());
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
    alice.send(&format!("@label=v2 VOICE DESC {lounge} :v=0\\r\\nmy-offer"));
    let reply = drain_until_label(&mut alice, "v2").await;
    let Event::VoiceDesc { sdp, .. } = &reply.event else {
        panic!("expected VOICE DESC answer, got {reply:?}");
    };
    assert_eq!(sdp, "answer-to:v=0\r\nmy-offer");

    // alice leaves → labeled VOICE STATE leave ack; bob sees the leave too.
    alice.send(&format!("@label=v3 VOICE LEAVE {lounge}"));
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
    alice.send(&format!("VOICE LEAVE {lounge}"));
    alice.expect_err(ErrCode::NoSuchTarget).await;
}

#[tokio::test]
async fn voice_muted_member_joins_but_renders_muted() {
    let ctx = ctx_voice(&[], &["boss"]);
    let mut boss = ready_op(&ctx, "boss").await;
    boss.send("CHANNEL CREATE #lounge voice");
    let Event::Policy {
        channel: lounge, ..
    } = boss.recv().await.event
    else {
        panic!("expected POLICY");
    };

    // A network-wide mute (M7) removes `speak` but not the join itself.
    boss.send("@label=m MUTE * alice");
    let reply = drain_until_label(&mut boss, "m").await;
    assert!(matches!(reply.event, Event::Moderated { .. }));

    let mut bob = ready(&ctx, "bob").await;
    bob.send(&format!("VOICE JOIN {lounge}"));
    assert!(matches!(bob.recv().await.event, Event::VoiceOffer { .. }));

    let mut alice = ready(&ctx, "alice").await;
    alice.send(&format!("VOICE JOIN {lounge}"));
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
    let Event::Policy {
        channel: lounge, ..
    } = boss.recv().await.event
    else {
        panic!("expected POLICY");
    };

    // A `*`-scope ban covers the voice channel — she is barred from voice.
    boss.send("@label=b BAN * alice");
    let reply = drain_until_label(&mut boss, "b").await;
    assert!(matches!(reply.event, Event::Moderated { .. }));

    let mut alice = ready(&ctx, "alice").await;
    alice.send(&format!("@label=v VOICE JOIN {lounge}"));
    let reply = alice.expect_err(ErrCode::Forbidden).await;
    assert_eq!(reply.label.as_deref(), Some("v"));
}

#[tokio::test]
async fn voice_join_receives_roster_snapshot() {
    // §16 (M-voice-4) a joiner learns who's already in the room, not just future
    // arrivals — a VOICE STATE snapshot follows the OFFER.
    let (ctx, lounge) = voice_ctx_with("#lounge").await;
    let mut bob = ready(&ctx, "bob").await;
    bob.send(&format!("VOICE JOIN {lounge}"));
    assert!(matches!(bob.recv().await.event, Event::VoiceOffer { .. }));

    let mut alice = ready(&ctx, "alice").await;
    alice.send(&format!("@label=j VOICE JOIN {lounge}"));
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
    let Event::Policy {
        channel: lounge, ..
    } = boss.recv().await.event
    else {
        panic!("expected POLICY");
    };

    let mut bob = ready(&ctx, "bob").await;
    bob.send(&format!("VOICE JOIN {lounge}"));
    assert!(matches!(bob.recv().await.event, Event::VoiceOffer { .. }));
    let mut alice = ready(&ctx, "alice").await;
    alice.send(&format!("VOICE JOIN {lounge}"));
    assert!(matches!(alice.recv().await.event, Event::VoiceOffer { .. }));

    // boss mutes alice at the channel scope.
    boss.send(&format!("@label=m MUTE {lounge} alice"));
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
    let Event::Policy {
        channel: lounge, ..
    } = boss.recv().await.event
    else {
        panic!("expected POLICY");
    };

    let mut bob = ready(&ctx, "bob").await;
    bob.send(&format!("VOICE JOIN {lounge}"));
    assert!(matches!(bob.recv().await.event, Event::VoiceOffer { .. }));
    let mut alice = ready(&ctx, "alice").await;
    alice.send(&format!("VOICE JOIN {lounge}"));
    assert!(matches!(alice.recv().await.event, Event::VoiceOffer { .. }));

    // boss bans alice at the channel scope.
    boss.send(&format!("@label=b BAN {lounge} alice :raid"));
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
    async fn send_code(&self, address: &str, code: &str, _purpose: &str) {
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

// ---- §6.1 email-at-registration + password reset ----

/// A context that requires an email at REGISTER, with a mock mailer installed.
fn ctx_require_email() -> (Arc<ServerCtx>, Arc<MockMailer>) {
    let store = Arc::new(MemoryStore::default());
    let info = ServerInfo {
        network: "test.example".parse().unwrap(),
        motd: None,
        features: Vec::new(),
    };
    let ctx = Arc::new(
        ServerCtx::new(
            info,
            std::iter::empty::<(weft_proto::ChannelName, RetentionPolicy)>(),
            Keypair::generate(),
            true,
            store,
            Arc::new(weft_core::MemBlobStore::default()),
            "permanent".parse().unwrap(),
            std::iter::empty::<weft_proto::Account>(),
            true,
            10,
            weft_core::FederationConfig::default(),
        )
        .with_require_email(true),
    );
    let mailer = Arc::new(MockMailer::default());
    ctx.set_mailer(mailer.clone());
    (ctx, mailer)
}

#[tokio::test]
async fn register_requires_email_when_configured() {
    let (ctx, _mailer) = ctx_require_email();
    let mut ada = connect(&ctx);
    ada.send("HELLO weft/1");
    assert!(matches!(ada.recv().await.event, Event::Welcome { .. }));

    // No email → POLICY (the network requires one).
    ada.send(&format!("@label=n REGISTER ada :{PASSWORD}"));
    let n = drain_until_label(&mut ada, "n").await;
    assert!(
        matches!(&n.event, Event::Err(e) if e.code == ErrCode::Policy),
        "{n:?}"
    );

    // With an email → WELCOME, and the email is recorded as a pending claim.
    ada.send(&format!(
        "@label=r REGISTER ada ada@example.com :{PASSWORD}"
    ));
    assert!(matches!(
        drain_until_label(&mut ada, "r").await.event,
        Event::Welcome { .. }
    ));
    ada.send("@label=v VERIFY LIST");
    let claim = drain_until_label(&mut ada, "v").await;
    assert!(
        matches!(&claim.event,
            Event::Verified { kind, subject, state }
                if kind == "email" && subject == "ada@example.com"
                   && *state == weft_proto::VerifyState::Pending),
        "email recorded pending (verify-later): {claim:?}"
    );
}

#[tokio::test]
async fn register_rejects_duplicate_and_malformed_email() {
    let (ctx, _mailer) = ctx_require_email();
    let mut ada = connect(&ctx);
    ada.send("HELLO weft/1");
    assert!(matches!(ada.recv().await.event, Event::Welcome { .. }));
    ada.send(&format!(
        "@label=a REGISTER ada ada@example.com :{PASSWORD}"
    ));
    assert!(matches!(
        drain_until_label(&mut ada, "a").await.event,
        Event::Welcome { .. }
    ));

    // A malformed email → MALFORMED.
    let mut bob = connect(&ctx);
    bob.send("HELLO weft/1");
    assert!(matches!(bob.recv().await.event, Event::Welcome { .. }));
    bob.send(&format!("@label=m REGISTER bob not-an-email :{PASSWORD}"));
    let m = drain_until_label(&mut bob, "m").await;
    assert!(
        matches!(&m.event, Event::Err(e) if e.code == ErrCode::Malformed),
        "{m:?}"
    );

    // A duplicate email (case-insensitive) → CONFLICT.
    bob.send(&format!(
        "@label=d REGISTER bob ADA@example.com :{PASSWORD}"
    ));
    let d = drain_until_label(&mut bob, "d").await;
    assert!(
        matches!(&d.event, Event::Err(e) if e.code == ErrCode::Conflict),
        "{d:?}"
    );
}

#[tokio::test]
async fn password_reset_full_flow() {
    let (ctx, mailer) = ctx_require_email();
    // Register with an email.
    let mut ada = connect(&ctx);
    ada.send("HELLO weft/1");
    assert!(matches!(ada.recv().await.event, Event::Welcome { .. }));
    ada.send(&format!(
        "@label=r REGISTER ada ada@example.com :{PASSWORD}"
    ));
    assert!(matches!(
        drain_until_label(&mut ada, "r").await.event,
        Event::Welcome { .. }
    ));
    drop(ada); // reset runs unauthed, on a fresh connection

    // RESET REQUEST → uniform RESET-SENT + a mailed code.
    let mut c = connect(&ctx);
    c.send("HELLO weft/1");
    assert!(matches!(c.recv().await.event, Event::Welcome { .. }));
    c.send("@label=q RESET REQUEST ada@example.com");
    assert!(matches!(
        drain_until_label(&mut c, "q").await.event,
        Event::ResetSent { .. }
    ));
    let (_addr, code) = mailer
        .sent
        .lock()
        .unwrap()
        .last()
        .cloned()
        .expect("a reset code was mailed");

    // A wrong code → FORBIDDEN.
    c.send("@label=w RESET CONFIRM ada@example.com 000000 :brand-new-password");
    let w = drain_until_label(&mut c, "w").await;
    assert!(
        matches!(&w.event, Event::Err(e) if e.code == ErrCode::Forbidden),
        "{w:?}"
    );

    // The right code sets the new password → RESET-DONE.
    let new_password = "brand-new-password-9";
    c.send(&format!(
        "@label=d RESET CONFIRM ada@example.com {code} :{new_password}"
    ));
    assert!(matches!(
        drain_until_label(&mut c, "d").await.event,
        Event::ResetDone { .. }
    ));

    // Reset does NOT authenticate: the old password no longer works, the new one
    // does. Verify by logging in fresh with each.
    let mut old = connect(&ctx);
    old.send("HELLO weft/1");
    assert!(matches!(old.recv().await.event, Event::Welcome { .. }));
    old.send(&format!("@label=o AUTH PASSWORD ada :{PASSWORD}"));
    let o = drain_until_label(&mut old, "o").await;
    assert!(
        matches!(&o.event, Event::Err(e) if e.code == ErrCode::AuthFailed),
        "old password rejected: {o:?}"
    );

    let mut fresh = connect(&ctx);
    fresh.send("HELLO weft/1");
    assert!(matches!(fresh.recv().await.event, Event::Welcome { .. }));
    fresh.send(&format!("@label=f AUTH PASSWORD ada :{new_password}"));
    assert!(
        matches!(
            drain_until_label(&mut fresh, "f").await.event,
            Event::Welcome { .. }
        ),
        "new password authenticates"
    );
}

#[tokio::test]
async fn password_reset_unknown_email_is_uniform() {
    let (ctx, mailer) = ctx_require_email();
    let mut c = connect(&ctx);
    c.send("HELLO weft/1");
    assert!(matches!(c.recv().await.event, Event::Welcome { .. }));

    // REQUEST for an unregistered email → the same RESET-SENT, no code mailed.
    c.send("@label=q RESET REQUEST ghost@example.com");
    assert!(matches!(
        drain_until_label(&mut c, "q").await.event,
        Event::ResetSent { .. }
    ));
    assert!(
        mailer.sent.lock().unwrap().is_empty(),
        "no code mailed for an unknown email"
    );

    // CONFIRM against an unknown email → the SAME bad-code error as a wrong code.
    c.send("@label=x RESET CONFIRM ghost@example.com 123456 :some-new-password");
    let x = drain_until_label(&mut c, "x").await;
    assert!(
        matches!(&x.event, Event::Err(e) if e.code == ErrCode::Forbidden),
        "{x:?}"
    );
}

#[tokio::test]
async fn login_accepts_email_or_account_name() {
    // §6.1: AUTH PASSWORD resolves the identifier as an account name OR a
    // registered email, so a name can change later without breaking sign-in.
    let (ctx, _mailer) = ctx_require_email();

    // Register ada with an email.
    let mut ada = connect(&ctx);
    ada.send("HELLO weft/1");
    assert!(matches!(ada.recv().await.event, Event::Welcome { .. }));
    ada.send(&format!(
        "@label=r REGISTER ada ada@example.com :{PASSWORD}"
    ));
    assert!(matches!(
        drain_until_label(&mut ada, "r").await.event,
        Event::Welcome { .. }
    ));
    drop(ada);

    // Log in by email → WELCOME.
    let mut by_email = connect(&ctx);
    by_email.send("HELLO weft/1");
    assert!(matches!(by_email.recv().await.event, Event::Welcome { .. }));
    by_email.send(&format!(
        "@label=e AUTH PASSWORD ada@example.com :{PASSWORD}"
    ));
    assert!(
        matches!(
            drain_until_label(&mut by_email, "e").await.event,
            Event::Welcome { .. }
        ),
        "email identifier authenticates"
    );

    // Log in by account name → WELCOME (unchanged behavior).
    let mut by_name = connect(&ctx);
    by_name.send("HELLO weft/1");
    assert!(matches!(by_name.recv().await.event, Event::Welcome { .. }));
    by_name.send(&format!("@label=n AUTH PASSWORD ada :{PASSWORD}"));
    assert!(matches!(
        drain_until_label(&mut by_name, "n").await.event,
        Event::Welcome { .. }
    ));

    // An unregistered email → the uniform AUTH-FAILED (invariant 5): no oracle
    // distinguishes "no such email" from "wrong password".
    let mut ghost = connect(&ctx);
    ghost.send("HELLO weft/1");
    assert!(matches!(ghost.recv().await.event, Event::Welcome { .. }));
    ghost.send(&format!(
        "@label=g AUTH PASSWORD ghost@example.com :{PASSWORD}"
    ));
    let g = drain_until_label(&mut ghost, "g").await;
    assert!(
        matches!(&g.event, Event::Err(e) if e.code == ErrCode::AuthFailed),
        "unknown email is a uniform AUTH-FAILED: {g:?}"
    );
}

#[tokio::test]
async fn welcome_advertises_email_required() {
    // §3.6: a `require_email` network flags `features=email-required` in the
    // negotiation WELCOME, so a client can shape its REGISTER form up front.
    let (ctx, _mailer) = ctx_require_email();
    let mut c = connect(&ctx);
    c.send("HELLO weft/1");
    let welcome = c.recv().await;
    let Event::Welcome { features, .. } = &welcome.event else {
        panic!("expected WELCOME, got {welcome:?}");
    };
    assert!(
        features.iter().any(|f| f == "email-required"),
        "features advertise email-required: {features:?}"
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
        let r = ada.recv_any().await;
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

#[tokio::test]
async fn developer_mode_names_the_branch_that_refused() {
    // The §8 codes are uniform by design, which is what makes a bare toast
    // undiagnosable: `CAP-REQUIRED` on a channel could be any of a dozen
    // branches. Developer mode says which one — verb + the helper that refused.
    //
    // `#[track_caller]` is a no-op on `async fn`, so file:line is not available
    // without a macro at every emit site; verb + helper pins it just as well.
    let dev = ctx(&["#general"]);
    dev.set_developer(true);

    // MEMBERS on a channel we exist-but-are-not-joined-to → `not_member_cap`.
    let mut ada = ready(&dev, "ada").await;
    ada.send("@label=m1 MEMBERS #general");
    let reply = ada.recv().await;
    let Event::Err(err) = reply.event else {
        panic!("expected ERR, got {:?}", reply.event);
    };
    assert_eq!(err.code, ErrCode::CapRequired);
    assert!(
        err.text.contains("Members") && err.text.contains("not_member_cap"),
        "developer mode must name the verb and the helper: {}",
        err.text
    );

    // Off by default, the text is the uniform one — annotating it would leak
    // exactly what invariant 1 buys (absent vs hidden vs not-a-member).
    let plain = ctx(&["#general"]);
    let mut bob = ready(&plain, "bob").await;
    bob.send("@label=m2 MEMBERS #general");
    let Event::Err(err) = bob.recv().await.event else {
        panic!("expected ERR");
    };
    assert_eq!(err.text, "join the channel first", "no annotation when off");
}

#[tokio::test]
async fn a_realm_confirmed_rejoin_subscribes_the_live_session() {
    // Rejoining a bridged namespace without reconnecting: the realm's
    // `NS-MEMBER … join` arrives on the *provider's* session, but subscriptions
    // live on the user's. Without a nudge the membership row exists while nothing
    // is joined, so HISTORY/MEMBERS answer CAP-REQUIRED until the next login —
    // the namespace opens and its history never loads.
    let key = Keypair::generate();
    let ctx = ctx_plugin_full(
        vec![("insta", key.public(), vec!["instagram".parse().unwrap()])],
        &[],
    );

    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT instagram://acme-corp");
    plugin.send(&format!(
        "@title=Club;id={} NS-META instagram://acme-corp/club public",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::NsMeta { id, .. } = plugin.recv().await.event else {
        panic!("expected the minted NS-META");
    };
    let ns_id = id.to_string();
    plugin.send(&format!(
        "@vanity=general;id={} CHANNEL-LAYOUT instagram://acme-corp/club/general 0",
        ulid::Ulid::new().to_string().to_lowercase()
    ));
    let Event::ChannelLayout { channel, .. } = plugin.recv().await.event else {
        panic!("expected the minted CHANNEL-LAYOUT");
    };

    // Ada is connected *before* she is a member — the rejoin case, where no
    // fresh AUTH re-derives her channel set.
    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@label=h0 HISTORY {channel}"));
    let Event::Err(err) = ada.recv().await.event else {
        panic!("a non-member's HISTORY is refused");
    };
    assert_eq!(err.code, ErrCode::CapRequired);

    // The realm now confirms her membership on its own session.
    plugin.send(&format!("NS-MEMBER {ns_id} ada@test.example join"));

    // …and her live session must have been subscribed, so HISTORY is served.
    for _ in 0..40 {
        ada.send(&format!("@label=h1 HISTORY {channel}"));
        match ada.recv().await.event {
            Event::BatchStart { .. } => return, // served — the nudge worked
            Event::Err(_) => tokio::task::yield_now().await,
            _ => {}
        }
    }
    panic!("HISTORY still refused: the realm-confirmed join never subscribed the session");
}

#[tokio::test]
async fn a_message_the_realm_never_confirms_is_reported_undelivered() {
    // The window that made this necessary: the bridge is gone but weftd has not
    // noticed yet, so `can_post` still allows the message. It is stored, echoed
    // (weftd's echo acks local storage, which is all it can honestly promise) —
    // and silently never reaches Matrix. The author was left believing it landed.
    //
    // Now the provider must answer, and silence past the grace window is itself an
    // answer: the author gets `UNDELIVERED` for that msgid.
    //
    // Scoped to a **projected** namespace, which is where weftd mints at all. In a
    // *replica* the realm is the source of truth, so weftd relays the post instead
    // of storing one — there is no locally-minted message to report on.
    let key = Keypair::generate();
    let ctx = ctx_plugin_full(
        vec![("insta", key.public(), vec!["instagram".parse().unwrap()])],
        &[],
    );

    let mut ada = ready(&ctx, "ada").await;
    ada.send(&format!("@root={} NS CREATE gaming public", root_key_b64()));
    let Event::NsMeta { id, .. } = ada.recv().await.event else {
        panic!("expected NS-META");
    };
    let ns_id = id.to_string();
    let channel = ada.channel_by_vanity(&ns_id, "general").await;
    ada.send(&format!("NS META {ns_id} bridge:instagram :open"));
    ada.recv().await;
    ada.send(&format!("@label=j1 NS JOIN {ns_id}"));
    drain_until_ns_member(&mut ada).await;

    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM ASSERT instagram://acme-corp");

    // She posts. weftd stores + echoes it — that ack is honest about storage.
    ada.send(&format!("@label=m1 MSG {channel} :did this reach Matrix?"));
    let reply = ada.recv().await;
    assert_eq!(reply.label.as_deref(), Some("m1"));
    let Event::Message(posted) = reply.event else {
        panic!("expected her own echo");
    };

    // The provider was handed it (it is subscribed) and says nothing at all —
    // which is what a dead adapter looks like from here. Past the deadline, the
    // author is told.
    // Well past any grace window, without coupling the test to its exact length.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 60_000;
    ctx.sweep_undelivered(now_ms).await;

    let Event::Undelivered {
        msgid, channel: ch, ..
    } = ada.recv().await.event
    else {
        panic!("ada expected UNDELIVERED for the message the realm never confirmed");
    };
    assert_eq!(msgid, posted.msgid);
    assert_eq!(ch.to_string(), channel.to_string());

    // And the positive half: a message the provider *acks* is never reported. A
    // false alarm would be worse than the silence it replaces — every delivered
    // message would arrive with a scary marker.
    ada.send(&format!("@label=m2 MSG {channel} :this one lands"));
    let Event::Message(second) = ada.recv().await.event else {
        panic!("expected her own echo");
    };
    // Wait until the provider has actually been handed it. Her echo does not
    // imply that: the echo and the forward run on different tasks, so acking a
    // msgid weftd had not yet marked in-flight resolved nothing — and the sweep
    // then reported an acked message.
    loop {
        let raw = plugin.recv_raw().await;

        if matches!(weft_proto::Reply::parse(&raw).map(|r| r.event),
            Ok(Event::Message(m)) if m.msgid == second.msgid)
        {
            break;
        }
    }

    plugin.send(&format!("DELIVERED {}", second.msgid));
    // Barrier: a re-assert on the SAME session re-pushes the projected structure,
    // and the session reads its lines in order — so that push proves DELIVERED was
    // processed first. Without it the sweep below races the ack and the test passes
    // for the wrong reason.
    plugin.send("REALM ASSERT instagram://acme-corp");
    // Raw lines: this session also carries the forwarded MSG and the membership
    // statement, so match on the text rather than a typed event.
    let mut barriered = false;
    for _ in 0..40 {
        if plugin.recv_raw().await.contains("NS-META") {
            barriered = true;
            break;
        }
    }
    assert!(
        barriered,
        "the re-assert must answer, proving DELIVERED landed first"
    );

    ctx.sweep_undelivered(now_ms + 120_000).await;

    // MEMBERS is the FIFO barrier on ada's side: anything the sweep pushed would
    // arrive before the batch it answers with.
    ada.send(&format!("@label=m3 MEMBERS {channel}"));
    loop {
        match ada.recv().await.event {
            Event::Undelivered { msgid, .. } => {
                assert_ne!(msgid, second.msgid, "an acked message must not be reported");
            }
            Event::BatchStart { .. } => break,
            _ => {}
        }
    }
}

#[tokio::test]
async fn enabling_projection_states_the_namespaces_existing_channels() {
    // `NS-META` describes the namespace and its categories, not its channels. So
    // switching projection on produced a Space and its sub-spaces on the foreign
    // side and no rooms at all — the chats were simply never mentioned.
    //
    // Making a channel `retained` and back to `permanent` worked around it by
    // producing a policy change, which is not a fix: an already-permanent channel
    // in an already-projected namespace must be projected because it *exists*, not
    // because it changed.
    let key = Keypair::generate();
    let ctx = ctx_plugin_full(
        vec![("insta", key.public(), vec!["instagram".parse().unwrap()])],
        &[],
    );

    // The provider must be registered for the scheme, or there is nobody to state
    // the structure to.
    let mut plugin = plugin_session(&ctx, &key).await;
    plugin.send("REALM REGISTER instagram");

    let mut ada = ready(&ctx, "ada").await;
    let ns_id = ada.create_ns("gaming").await;
    // `NS CREATE` already seeds `#general`, so use a distinct vanity.
    let channel = ada.create_channel(&ns_id, "chat").await;
    // Only `permanent` projects (locked decision 2), so make it eligible *before*
    // projection is switched on — that is the case that was broken.
    ada.send(&format!("@label=p1 CHANNEL POLICY {channel} permanent"));

    ada.send(&format!("@label=b1 NS META {ns_id} bridge:instagram :open"));

    // The provider should hear the namespace *and* its channel.
    let mut saw_layout = false;
    let mut saw_policy = false;
    for _ in 0..60 {
        let line = plugin.recv_raw().await;
        if line.contains("CHANNEL-LAYOUT") && line.contains(channel.as_str()) {
            saw_layout = true;
        }
        if line.contains("POLICY") && line.contains(channel.as_str()) {
            saw_policy = true;
        }
        if saw_layout && saw_policy {
            break;
        }
    }

    assert!(
        saw_layout && saw_policy,
        "enabling projection must state the existing channel (layout {saw_layout}, policy {saw_policy})"
    );
}
