//! Channel actor: one task exclusively owns one channel's member list and
//! mints its msgids — the actor's inbox order IS the channel's total order
//! (spec §9.1, architecture doc §3), which is also why every stored event
//! is appended here and nowhere else. Fan-out via `tokio::broadcast`; a
//! lagging subscriber gets `RecvError::Lagged`, which the session turns
//! into `ERR SLOW` (§9.2).
//!
//! Commands arrive pre-validated by the session (membership, authorship,
//! tombstone checks). Per-sender mpsc ordering makes that sound for a
//! session's *own* actions; cross-session races (e.g. concurrent DELETE
//! and EDIT) are tolerated by materialization, where Delete always wins.

use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::error;
use ulid::Ulid;
use weft_proto::{
    Account, ChannelName, Event, MemberAction, MessageEvent, MsgId, MsgMeta, NetworkName,
    ReactionOp, RetentionPolicy, Target, TypingState, UserRef,
};
use weft_store::{EventKind, EventRecord, EventStore, Scope};

use crate::session::SessionId;

/// Broadcast ring size per channel; beyond this a slow client lags → SLOW.
const BROADCAST_CAPACITY: usize = 512;
/// Inbox bound: publishers await here when the actor is busy (backpressure).
const INBOX_CAPACITY: usize = 256;

/// A broadcast item. `origin` lets each session tell its own events apart:
/// the sender's MESSAGE/EDITED/DELETED/REACTION copy becomes the labeled
/// echo-ack (§9.2); its other own copies are skipped because the session
/// already sent the direct response.
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
    /// §11: subscribe to the broadcast without joining as a member — a bridge
    /// session watches the channel to forward local events, but must not show
    /// up in the member list or count.
    Subscribe {
        reply: oneshot::Sender<broadcast::Receiver<ChannelEvent>>,
    },
    /// §6.3 MEMBERS: snapshot the current roster (deduped accounts).
    Roster {
        reply: oneshot::Sender<Vec<Account>>,
    },
    Publish {
        session: SessionId,
        body: String,
        meta: MsgMeta,
    },
    /// Pre-validated by the session (author, not tombstoned).
    Edit {
        session: SessionId,
        root: MsgId,
        body: String,
    },
    Delete {
        session: SessionId,
        root: MsgId,
    },
    /// Operator delete-any (admin panel): no session, attributed to `by`. The
    /// caller is responsible for the authority check (a `*`/ns/channel cap).
    SystemDelete {
        root: MsgId,
        by: Account,
    },
    React {
        session: SessionId,
        root: MsgId,
        emoji: String,
        add: bool,
    },
    Typing {
        session: SessionId,
        state: TypingState,
    },
    /// §6.3 CHANNEL POLICY: change the live actor's retention + tell members.
    SetPolicy {
        session: SessionId,
        policy: RetentionPolicy,
    },
    /// §6.1 PRESENCE: relayed to co-members, never stored, never bridged.
    Presence {
        session: SessionId,
        status: weft_proto::PresenceStatus,
    },
    /// §11.4 remote ingestion: persist a bridged event under the local
    /// (negotiated) policy **without minting a fresh msgid** — origin msgids
    /// and their ULID order stay intact — and broadcast it to local members.
    /// `origin` is the bridge session's id so its own forwarder skips the
    /// echo (no loop back to the peer it arrived from).
    Ingest {
        origin: SessionId,
        // Boxed: a stored record + wire event dwarf the other variants.
        record: Box<EventRecord>,
        event: Box<Event>,
    },
    /// §11.5 broadcast a notification event to members without storing it.
    Announce {
        origin: SessionId,
        event: Box<Event>,
    },
    /// §6.7 force-remove an account's session(s) (kick/ban). Broadcasts a
    /// `MEMBER part`; the ejected session cleans itself up when it sees its
    /// own part.
    Eject {
        account: Account,
    },
}

/// A broadcast origin no real session ever has (session ids start at 1), so an
/// [`Cmd::Announce`] copy is delivered to every member.
const SENTINEL_ORIGIN: SessionId = SessionId::MAX;

/// Cheap handle to a channel actor. Mutations are fire-and-forget: the
/// broadcast copy (echoed with the sender's label) is the ack (§9.2).
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

    /// §6.7 kick/ban: force-remove an account's session(s) from the channel.
    pub async fn eject(&self, account: Account) {
        let _ = self.inbox.send(Cmd::Eject { account }).await;
    }

    /// §11 subscribe a bridge session to the broadcast (no membership).
    pub async fn subscribe(&self) -> Option<broadcast::Receiver<ChannelEvent>> {
        let (reply, ack) = oneshot::channel();
        self.inbox.send(Cmd::Subscribe { reply }).await.ok()?;
        ack.await.ok()
    }

    /// §6.3 MEMBERS: the current roster (deduped accounts). Empty if the actor
    /// is gone.
    pub async fn roster(&self) -> Vec<Account> {
        let (reply, ack) = oneshot::channel();
        if self.inbox.send(Cmd::Roster { reply }).await.is_err() {
            return Vec::new();
        }
        ack.await.unwrap_or_default()
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

    pub async fn edit(&self, session: SessionId, root: MsgId, body: String) {
        let _ = self
            .inbox
            .send(Cmd::Edit {
                session,
                root,
                body,
            })
            .await;
    }

    pub async fn delete(&self, session: SessionId, root: MsgId) {
        let _ = self.inbox.send(Cmd::Delete { session, root }).await;
    }

    /// Operator delete-any (admin panel) — no session; the tombstone is
    /// attributed to `by`. Authority is enforced by the caller.
    pub async fn admin_delete(&self, root: MsgId, by: Account) {
        let _ = self.inbox.send(Cmd::SystemDelete { root, by }).await;
    }

    pub async fn react(&self, session: SessionId, root: MsgId, emoji: String, add: bool) {
        let _ = self
            .inbox
            .send(Cmd::React {
                session,
                root,
                emoji,
                add,
            })
            .await;
    }

    pub async fn typing(&self, session: SessionId, state: TypingState) {
        let _ = self.inbox.send(Cmd::Typing { session, state }).await;
    }

    pub async fn presence(&self, session: SessionId, status: weft_proto::PresenceStatus) {
        let _ = self.inbox.send(Cmd::Presence { session, status }).await;
    }

    pub async fn set_policy(&self, session: SessionId, policy: RetentionPolicy) {
        let _ = self.inbox.send(Cmd::SetPolicy { session, policy }).await;
    }

    /// §11.5/§6.6 broadcast a non-stored notification (e.g. `MANIFEST`) to
    /// members. `SENTINEL_ORIGIN` marks it as no session's own event so every
    /// member — including the acting bridge session — receives a copy.
    pub async fn announce(&self, event: Event) {
        let _ = self
            .inbox
            .send(Cmd::Announce {
                origin: SENTINEL_ORIGIN,
                event: Box::new(event),
            })
            .await;
    }

    /// §11.4 ingest a verified remote event (see [`Cmd::Ingest`]).
    pub async fn ingest(&self, origin: SessionId, record: EventRecord, event: Event) {
        let _ = self
            .inbox
            .send(Cmd::Ingest {
                origin,
                record: Box::new(record),
                event: Box::new(event),
            })
            .await;
    }
}

/// Spawn the actor task. Actors live for the process lifetime (the channel
/// set is static config until CHANNEL CREATE in M4).
pub fn spawn(
    name: ChannelName,
    network: NetworkName,
    policy: RetentionPolicy,
    store: Arc<dyn EventStore>,
    media: Arc<dyn weft_store::MediaStore>,
) -> ChannelHandle {
    let (inbox_tx, inbox) = mpsc::channel(INBOX_CAPACITY);
    let handle = ChannelHandle {
        name: name.clone(),
        inbox: inbox_tx,
    };
    let actor = Actor {
        scope: Scope::Channel(name.clone()),
        name,
        network,
        policy,
        store,
        media,
        members: std::collections::HashMap::new(),
        events: broadcast::Sender::new(BROADCAST_CAPACITY),
        ulids: ulid::Generator::new(),
    };
    tokio::spawn(actor.run(inbox));
    handle
}

struct Actor {
    scope: Scope,
    name: ChannelName,
    network: NetworkName,
    policy: RetentionPolicy,
    store: Arc<dyn EventStore>,
    /// §13 media reference index — refs recorded here, the single-writer point.
    media: Arc<dyn weft_store::MediaStore>,
    members: std::collections::HashMap<SessionId, Account>,
    events: broadcast::Sender<ChannelEvent>,
    ulids: ulid::Generator,
}

impl Actor {
    async fn run(mut self, mut inbox: mpsc::Receiver<Cmd>) {
        while let Some(cmd) = inbox.recv().await {
            self.handle(cmd).await;
        }
    }

    async fn handle(&mut self, cmd: Cmd) {
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
                // Only announce a *new* member — a second device/session for an
                // account already present (e.g. auto-rejoin on reconnect while
                // another device is online) must not broadcast a fresh join.
                let account_new = !self.members.values().any(|a| *a == account);
                self.members.insert(session, account);
                let count = self.distinct_members();
                if account_new {
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
            Cmd::Subscribe { reply } => {
                let _ = reply.send(self.events.subscribe());
            }
            Cmd::Roster { reply } => {
                // Dedup: one account may hold several sessions/devices.
                let mut seen = std::collections::HashSet::new();
                let roster = self
                    .members
                    .values()
                    .filter(|account| seen.insert((*account).clone()))
                    .cloned()
                    .collect();
                let _ = reply.send(roster);
            }
            Cmd::Part { session } => {
                if let Some(account) = self.members.remove(&session) {
                    // Only announce a part when the account has no *other*
                    // session left — one device leaving while another stays
                    // online is not a departure.
                    let account_gone = !self.members.values().any(|a| *a == account);
                    if account_gone {
                        let user = self.user(&account);
                        let count = self.distinct_members();
                        self.broadcast(
                            session,
                            member_event(&self.name, user, MemberAction::Part, count),
                        );
                    }
                }
            }
            Cmd::Publish {
                session,
                body,
                meta,
            } => {
                let Some(sender) = self.member(session) else {
                    return; // raced with a part; session-side checks already answered
                };
                let msgid = self.mint();
                let record = EventRecord {
                    scope: self.scope.clone(),
                    msgid: msgid.clone(),
                    root: msgid.clone(),
                    sender: sender.clone(),
                    kind: EventKind::Message {
                        body: body.clone(),
                        meta: meta.clone(),
                    },
                };
                self.persist(record).await;
                self.record_media_refs(&msgid, &meta).await;
                self.broadcast(
                    session,
                    Event::Message(Box::new(MessageEvent {
                        target: Target::Channel(self.name.clone()),
                        sender,
                        msgid,
                        body,
                        meta,
                        edited: None,
                        edited_at: None,
                    })),
                );
            }
            Cmd::Edit {
                session,
                root,
                body,
            } => {
                let Some(user) = self.member(session) else {
                    return;
                };
                let msgid = self.mint();
                self.persist(EventRecord {
                    scope: self.scope.clone(),
                    msgid: msgid.clone(),
                    root: root.clone(),
                    sender: user.clone(),
                    kind: EventKind::Edit { body: body.clone() },
                })
                .await;
                self.broadcast(
                    session,
                    Event::Edited {
                        target: Target::Channel(self.name.clone()),
                        user,
                        msgid,
                        edit_of: root,
                        body,
                    },
                );
            }
            Cmd::Delete { session, root } => {
                let Some(user) = self.member(session) else {
                    return;
                };
                let msgid = self.mint();
                self.persist(EventRecord {
                    scope: self.scope.clone(),
                    msgid,
                    root: root.clone(),
                    sender: user.clone(),
                    kind: EventKind::Delete,
                })
                .await;
                // §13 the deleted message's blob refs drop → refcount may hit 0.
                let _ = self.media.drop_refs(&root).await;
                self.broadcast(
                    session,
                    Event::Deleted {
                        target: Target::Channel(self.name.clone()),
                        msgid: root,
                        by: Some(user),
                    },
                );
            }
            Cmd::SystemDelete { root, by } => {
                // Admin delete-any: attributed to the moderator, no member
                // check. SENTINEL origin so every member sees the tombstone.
                let user = self.user(&by);
                let msgid = self.mint();
                self.persist(EventRecord {
                    scope: self.scope.clone(),
                    msgid,
                    root: root.clone(),
                    sender: user.clone(),
                    kind: EventKind::Delete,
                })
                .await;
                self.broadcast(
                    SENTINEL_ORIGIN,
                    Event::Deleted {
                        target: Target::Channel(self.name.clone()),
                        msgid: root,
                        by: Some(user),
                    },
                );
            }
            Cmd::React {
                session,
                root,
                emoji,
                add,
            } => {
                let Some(user) = self.member(session) else {
                    return;
                };
                let msgid = self.mint();
                self.persist(EventRecord {
                    scope: self.scope.clone(),
                    msgid,
                    root: root.clone(),
                    sender: user.clone(),
                    kind: EventKind::React {
                        emoji: emoji.clone(),
                        add,
                    },
                })
                .await;
                self.broadcast(
                    session,
                    Event::Reaction {
                        target: Target::Channel(self.name.clone()),
                        msgid: root,
                        emoji,
                        op: if add {
                            ReactionOp::Add
                        } else {
                            ReactionOp::Remove
                        },
                        by: user,
                    },
                );
            }
            Cmd::Typing { session, state } => {
                if let Some(user) = self.member(session) {
                    // Relay only — never stored (§6.3).
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
            Cmd::Presence { session, status } => {
                if let Some(user) = self.member(session) {
                    self.broadcast(session, Event::Presence { user, status });
                }
            }
            Cmd::Ingest {
                origin,
                record,
                event,
            } => {
                // Persist under the local (negotiated/strictest) policy, then
                // fan out to local members. The msgid inside `record`/`event`
                // is the remote origin's — never re-minted (§11.4, invariant 2).
                self.persist(*record).await;
                self.broadcast(origin, *event);
            }
            Cmd::Announce { origin, event } => self.broadcast(origin, *event),
            Cmd::Eject { account } => {
                let sessions: Vec<SessionId> = self
                    .members
                    .iter()
                    .filter(|(_, a)| **a == account)
                    .map(|(session, _)| *session)
                    .collect();
                if sessions.is_empty() {
                    return;
                }
                let user = self.user(&account);
                for session in sessions {
                    self.members.remove(&session);
                }
                let count = self.members.len() as u64;
                // SENTINEL origin so every member — including the ejected
                // session and the acting moderator — receives the part.
                self.broadcast(
                    SENTINEL_ORIGIN,
                    member_event(&self.name, user, MemberAction::Part, count),
                );
            }
            Cmd::SetPolicy { session, policy } => {
                self.policy = policy;
                // Members learn the new retention (§5.2: policy visible);
                // the actor's own sender skips this copy (POLICY isn't an
                // echo type), so the acting session gets its labeled ack
                // from the session layer instead.
                self.broadcast(
                    session,
                    Event::Policy {
                        channel: self.name.clone(),
                        policy,
                    },
                );
            }
        }
    }

    fn member(&self, session: SessionId) -> Option<UserRef> {
        self.members.get(&session).map(|a| self.user(a))
    }

    fn user(&self, account: &Account) -> UserRef {
        UserRef::new(account.clone(), self.network.clone())
    }

    /// Distinct-account member count — one account may hold several sessions
    /// (multi-device), so member counts dedupe by account.
    fn distinct_members(&self) -> u64 {
        self.members
            .values()
            .collect::<std::collections::HashSet<_>>()
            .len() as u64
    }

    /// Monotonic within the actor = per-channel total order. The generator
    /// only fails when >2^80 IDs land in one ms; fall back to a fresh
    /// random ULID rather than dropping traffic.
    fn mint(&mut self) -> MsgId {
        let ulid = self.ulids.generate().unwrap_or_else(|_| Ulid::new());
        MsgId::new(self.network.clone(), ulid)
    }

    /// §13 record the blob references a posted message carries (M-media-1).
    /// Only well-formed **same-network** `weft-media://` attachments are tracked
    /// (foreign media = M-media-3 mirroring); malformed refs are ignored.
    async fn record_media_refs(&self, msgid: &MsgId, meta: &MsgMeta) {
        if meta.attachments.is_empty() {
            return;
        }
        let net = self.network.to_string();
        let mut hashes: Vec<String> = Vec::new();
        for uri in &meta.attachments {
            let Some((origin, hash)) = crate::media::parse_media_uri(uri) else {
                continue;
            };
            if origin != net.as_str() {
                continue;
            }
            hashes.push(hash.to_string());
            // §13 also reference the server-generated thumbnail so it shares the
            // parent's membership gating + refcount lifetime (M-media-1b).
            if let Ok(Some(rec)) = self.media.blob_meta(hash).await {
                if let Some(thumb) = rec.thumb {
                    hashes.push(thumb);
                }
            }
        }
        if !hashes.is_empty() {
            let _ = self.media.add_refs(&self.scope, msgid, &hashes).await;
        }
    }

    /// §5.2 `ephemeral` = relay only; everything else is stored. A storage
    /// failure degrades to relay-only delivery (live members still get the
    /// event) rather than dropping traffic — logged, never silent.
    async fn persist(&self, record: EventRecord) {
        if self.policy == RetentionPolicy::Ephemeral {
            return;
        }
        if let Err(e) = self.store.append(record).await {
            error!(channel = %self.name, "event not persisted: {e}");
        }
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
        display: None, // display profiles land with identity profiles
        count: Some(count),
    }
}
