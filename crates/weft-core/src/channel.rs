//! Channel actor: one task exclusively owns one channel's member list and
//! mints its msgids — the actor's inbox order IS the channel's total order
//! (spec §9.1, architecture doc §3). Fan-out via `tokio::broadcast`; a
//! lagging subscriber gets `RecvError::Lagged`, which the session turns
//! into `ERR SLOW` (§9.2) — the actor never buffers per-client.

use tokio::sync::{broadcast, mpsc, oneshot};
use ulid::Ulid;
use weft_proto::{
    Account, ChannelName, Event, MemberAction, MessageEvent, MsgId, MsgMeta, NetworkName,
    RetentionPolicy, Target, TypingState, UserRef,
};

use crate::session::SessionId;

/// Broadcast ring size per channel; beyond this a slow client lags → SLOW.
const BROADCAST_CAPACITY: usize = 512;
/// Inbox bound: publishers await here when the actor is busy (backpressure).
const INBOX_CAPACITY: usize = 256;

/// A broadcast item. `origin` lets each session tell its own events apart:
/// the sender's MESSAGE copy becomes the labeled echo-ack (§9.2); its other
/// own copies (MEMBER/TYPING) are skipped because the session already sent
/// the direct response.
#[derive(Debug, Clone)]
pub struct ChannelEvent {
    pub origin: SessionId,
    pub event: Event,
}

/// What a joiner gets back; the session builds the §6.3 JOIN response
/// (`MEMBER` + `POLICY` + `count=`) from it.
#[derive(Debug)]
pub struct JoinAck {
    pub events: broadcast::Receiver<ChannelEvent>,
    pub count: u64,
    pub policy: RetentionPolicy,
}

enum Cmd {
    Join {
        session: SessionId,
        account: Account,
        reply: oneshot::Sender<JoinAck>,
    },
    Part {
        session: SessionId,
    },
    Publish {
        session: SessionId,
        body: String,
        meta: MsgMeta,
    },
    Typing {
        session: SessionId,
        state: TypingState,
    },
}

/// Cheap handle to a channel actor. All methods are fire-and-forget except
/// `join`; results flow back through the broadcast stream.
#[derive(Debug, Clone)]
pub struct ChannelHandle {
    pub name: ChannelName,
    inbox: mpsc::Sender<Cmd>,
}

impl ChannelHandle {
    pub async fn join(&self, session: SessionId, account: Account) -> Option<JoinAck> {
        let (reply, ack) = oneshot::channel();
        self.inbox
            .send(Cmd::Join {
                session,
                account,
                reply,
            })
            .await
            .ok()?;
        ack.await.ok()
    }

    pub async fn part(&self, session: SessionId) {
        let _ = self.inbox.send(Cmd::Part { session }).await;
    }

    pub async fn publish(&self, session: SessionId, body: String, meta: MsgMeta) {
        let _ = self
            .inbox
            .send(Cmd::Publish {
                session,
                body,
                meta,
            })
            .await;
    }

    pub async fn typing(&self, session: SessionId, state: TypingState) {
        let _ = self.inbox.send(Cmd::Typing { session, state }).await;
    }
}

/// Spawn the actor task. M1 actors live for the process lifetime (the
/// channel set is static config; lazy spawn/park comes with dynamic
/// channels in M4).
pub fn spawn(name: ChannelName, network: NetworkName) -> ChannelHandle {
    let (inbox_tx, inbox) = mpsc::channel(INBOX_CAPACITY);
    let handle = ChannelHandle {
        name: name.clone(),
        inbox: inbox_tx,
    };
    let actor = Actor {
        name,
        network,
        // M1 is relay-only (§5.2 `ephemeral`); persistence lands in M3.
        policy: RetentionPolicy::Ephemeral,
        members: std::collections::HashMap::new(),
        events: broadcast::Sender::new(BROADCAST_CAPACITY),
        ulids: ulid::Generator::new(),
    };
    tokio::spawn(actor.run(inbox));
    handle
}

struct Actor {
    name: ChannelName,
    network: NetworkName,
    policy: RetentionPolicy,
    members: std::collections::HashMap<SessionId, Account>,
    events: broadcast::Sender<ChannelEvent>,
    ulids: ulid::Generator,
}

impl Actor {
    async fn run(mut self, mut inbox: mpsc::Receiver<Cmd>) {
        while let Some(cmd) = inbox.recv().await {
            self.handle(cmd);
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Join {
                session,
                account,
                reply,
            } => {
                // Subscribe before broadcasting so the joiner's receiver
                // sees everything from its own join onward.
                let events = self.events.subscribe();
                let user = self.user(&account);
                let fresh = self.members.insert(session, account).is_none();
                let count = self.members.len() as u64;
                if fresh {
                    self.broadcast(
                        session,
                        member_event(&self.name, user, MemberAction::Join, count),
                    );
                }
                let _ = reply.send(JoinAck {
                    events,
                    count,
                    policy: self.policy,
                });
            }
            Cmd::Part { session } => {
                if let Some(account) = self.members.remove(&session) {
                    let user = self.user(&account);
                    let count = self.members.len() as u64;
                    self.broadcast(
                        session,
                        member_event(&self.name, user, MemberAction::Part, count),
                    );
                }
            }
            Cmd::Publish {
                session,
                body,
                meta,
            } => {
                let Some(account) = self.members.get(&session) else {
                    return; // raced with a part; session-side checks already answered
                };
                let sender = UserRef::new(account.clone(), self.network.clone());
                // Monotonic within the actor = per-channel total order. The
                // generator only fails when >2^80 IDs land in one ms; fall
                // back to a fresh random ULID rather than dropping traffic.
                let ulid = self.ulids.generate().unwrap_or_else(|_| Ulid::new());
                let msgid = MsgId::new(self.network.clone(), ulid);
                self.broadcast(
                    session,
                    Event::Message(Box::new(MessageEvent {
                        target: Target::Channel(self.name.clone()),
                        sender,
                        msgid,
                        body,
                        meta,
                    })),
                );
            }
            Cmd::Typing { session, state } => {
                if let Some(account) = self.members.get(&session) {
                    let user = UserRef::new(account.clone(), self.network.clone());
                    self.broadcast(
                        session,
                        Event::Typing {
                            channel: self.name.clone(),
                            user,
                            state,
                        },
                    );
                }
            }
        }
    }

    fn user(&self, account: &Account) -> UserRef {
        UserRef::new(account.clone(), self.network.clone())
    }

    fn broadcast(&self, origin: SessionId, event: Event) {
        // Err = no subscribers right now; nothing to deliver, not a fault.
        let _ = self.events.send(ChannelEvent { origin, event });
    }
}

fn member_event(channel: &ChannelName, user: UserRef, action: MemberAction, count: u64) -> Event {
    Event::Member {
        channel: channel.clone(),
        user,
        action,
        display: None, // display profiles land with identity (M2)
        count: Some(count),
    }
}
