//! The account directory: one global actor mapping accounts to their live
//! sessions. It owns everything that is account-scoped rather than
//! channel-scoped — DM delivery (§9.5) and MARK read-marker sync (§6.3).
//!
//! DM ordering: a single actor with a single monotonic ULID generator
//! gives a global total order, which trivially contains the required
//! per-(network, pair) order (§9.1). Like channel actors, this is the only
//! place DM msgids are minted.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};
use ulid::Ulid;
use weft_proto::{
    Account, ChannelName, Event, GroupId, MessageEvent, MsgId, MsgMeta, NetworkName, ReactionOp,
    RetentionPolicy, Target, UserRef,
};
use weft_store::{AccountStore, EventKind, EventRecord, EventStore, Scope};

use crate::session::SessionId;

const INBOX_CAPACITY: usize = 256;

/// An account-scoped event delivered straight to a session's queue.
/// `origin` drives the same echo-label rule as channel broadcasts.
#[derive(Debug)]
pub(crate) struct DirectEvent {
    pub origin: SessionId,
    pub event: Event,
}

/// A group DM message mutation (edit / delete / react) — the operation, shared by
/// the same-network and cross-network (federated) paths.
#[derive(Debug, Clone)]
pub(crate) enum GroupMutKind {
    Edit(String),
    Delete,
    React { emoji: String, add: bool },
}

impl GroupMutKind {
    /// The stored `EventKind` for this mutation.
    fn event_kind(&self) -> EventKind {
        match self.clone() {
            GroupMutKind::Edit(body) => EventKind::Edit { body },
            GroupMutKind::Delete => EventKind::Delete,
            GroupMutKind::React { emoji, add } => EventKind::React { emoji, add },
        }
    }

    /// The wire event this mutation produces for a group. `mut_msgid` is the
    /// mutation's own (origin-minted) id; `root` is the message it targets.
    fn to_event(&self, group: GroupId, sender: UserRef, mut_msgid: MsgId, root: MsgId) -> Event {
        match self.clone() {
            GroupMutKind::Edit(body) => Event::Edited {
                target: Target::Group(group),
                user: sender,
                msgid: mut_msgid,
                edit_of: root,
                body,
            },
            GroupMutKind::Delete => Event::Deleted {
                target: Target::Group(group),
                msgid: root,
                by: Some(sender),
            },
            GroupMutKind::React { emoji, add } => Event::Reaction {
                target: Target::Group(group),
                msgid: root,
                emoji,
                op: if add {
                    ReactionOp::Add
                } else {
                    ReactionOp::Remove
                },
                by: sender,
            },
        }
    }
}

/// Cheap handle; held by `ServerCtx`.
#[derive(Debug, Clone)]
pub(crate) struct Directory {
    inbox: mpsc::Sender<Cmd>,
}

enum Cmd {
    Register {
        account: Account,
        session: SessionId,
        queue: mpsc::Sender<DirectEvent>,
        /// Cancelled to force this session's loop to exit (WC7 forced logout).
        close: CancellationToken,
    },
    /// WC7: cut every live session of `account` — the operator-side counterpart
    /// to suspend, which only blocks *new* logins. Replies with how many were
    /// closed. Each session's own loop observes the cancellation and runs its
    /// ordinary `cleanup`, so co-members see a normal disconnect (presence
    /// offline, voice leave) rather than a session that silently vanishes.
    Disconnect {
        account: Account,
        reply: oneshot::Sender<usize>,
    },
    Deregister {
        account: Account,
        session: SessionId,
    },
    IsOnline {
        account: Account,
        reply: oneshot::Sender<bool>,
    },
    /// Pre-validated except recipient existence (the directory owns that
    /// check — anti-enumeration: the reply is a plain bool the session
    /// turns into NO-SUCH-TARGET).
    Dm {
        origin: SessionId,
        from: Account,
        to: Account,
        body: String,
        meta: MsgMeta,
        delivered: oneshot::Sender<bool>,
    },
    /// A group DM message (social layer). The session pre-validates membership
    /// and resolves the local members to fan out to; the directory mints the
    /// ULID (single writer → group total order), persists under `Scope::Group`,
    /// and delivers to every local member's sessions (incl. the sender's echo).
    GroupMsg {
        origin: SessionId,
        from: Account,
        group: GroupId,
        members: Vec<Account>,
        body: String,
        meta: MsgMeta,
    },
    /// The **home** network mints a cross-network group message (§9.1 single
    /// writer): `sender` may be foreign (a spoke relayed the post). Persists +
    /// delivers to local members, and replies with the minted `MessageEvent` so
    /// the caller can fan it out to the other member networks.
    GroupMint {
        origin: SessionId,
        sender: UserRef,
        group: GroupId,
        members: Vec<Account>,
        body: String,
        meta: MsgMeta,
        reply: oneshot::Sender<MessageEvent>,
    },
    /// A **member** network ingests a home-minted group message (origin msgid
    /// intact, invariant 2): persist under `Scope::Group` + deliver to local
    /// members. No minting. `origin` is the spoke poster's session when this is
    /// their own echo (so its labelled ack attaches), else `SessionId::MAX`.
    GroupIngest {
        origin: SessionId,
        sender: UserRef,
        group: GroupId,
        msgid: MsgId,
        body: String,
        meta: MsgMeta,
        members: Vec<Account>,
    },
    /// The **home** mints a group message mutation (edit/delete/react): persist +
    /// deliver to local members, and reply with the wire event + the mutation's
    /// own msgid, for fan-out to other member networks.
    GroupMutate {
        origin: SessionId,
        sender: UserRef,
        group: GroupId,
        root: MsgId,
        kind: GroupMutKind,
        members: Vec<Account>,
        reply: oneshot::Sender<(Event, MsgId)>,
    },
    /// A **member** network ingests a home-minted mutation (origin `msgid` intact).
    GroupMutIngest {
        sender: UserRef,
        group: GroupId,
        root: MsgId,
        msgid: MsgId,
        kind: GroupMutKind,
        members: Vec<Account>,
    },
    /// DM mutations, fully pre-validated by the session (participant,
    /// author, tombstone).
    Edit {
        origin: SessionId,
        from: Account,
        peer: Account,
        root: MsgId,
        body: String,
    },
    Delete {
        origin: SessionId,
        from: Account,
        peer: Account,
        root: MsgId,
    },
    React {
        origin: SessionId,
        from: Account,
        peer: Account,
        root: MsgId,
        emoji: String,
        add: bool,
    },
    /// §6.3: fan a fresh read marker (and the refreshed unread counts) out to
    /// the account's *other* sessions. The marking device already knows it
    /// read, so it is skipped.
    MarkSync {
        origin: SessionId,
        account: Account,
        channel: ChannelName,
        msgid: MsgId,
        unread: u64,
        mentions: u64,
    },
    /// Push an account-addressed event to every live session of `account`
    /// (§6.7 report delivery to a known handler/reporter). Fire-and-forget —
    /// offline accounts fetch via REPORTS LIST on reconnect.
    Notify { account: Account, event: Event },
}

impl Directory {
    pub(crate) async fn register(
        &self,
        account: Account,
        session: SessionId,
        queue: mpsc::Sender<DirectEvent>,
        close: CancellationToken,
    ) {
        let _ = self
            .inbox
            .send(Cmd::Register {
                account,
                session,
                queue,
                close,
            })
            .await;
    }

    /// WC7 forced logout: close every live session of `account`, returning how
    /// many were cut. Idempotent — an account with no sessions returns 0.
    pub(crate) async fn disconnect(&self, account: &Account) -> usize {
        let (reply, rx) = oneshot::channel();
        if self
            .inbox
            .send(Cmd::Disconnect {
                account: account.clone(),
                reply,
            })
            .await
            .is_err()
        {
            return 0;
        }
        rx.await.unwrap_or(0)
    }

    pub(crate) async fn deregister(&self, account: Account, session: SessionId) {
        let _ = self.inbox.send(Cmd::Deregister { account, session }).await;
    }

    /// Does `account` still have at least one live session? Ordered after a
    /// preceding `deregister` (same inbox), so a session can ask "was I the
    /// last?" right after leaving — used to clear presence on true disconnect.
    pub(crate) async fn is_online(&self, account: &Account) -> bool {
        let (reply, rx) = oneshot::channel();
        if self
            .inbox
            .send(Cmd::IsOnline {
                account: account.clone(),
                reply,
            })
            .await
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Push `event` to every live session of `account` (§6.7). Best-effort.
    pub(crate) async fn notify(&self, account: Account, event: Event) {
        let _ = self.inbox.send(Cmd::Notify { account, event }).await;
    }

    /// False = recipient does not exist (→ NO-SUCH-TARGET).
    /// Publish a group DM message. `members` are the local accounts to fan out
    /// to (the session resolved them from the `GroupStore`, including itself).
    pub(crate) async fn group_msg(
        &self,
        origin: SessionId,
        from: Account,
        group: GroupId,
        members: Vec<Account>,
        body: String,
        meta: MsgMeta,
    ) {
        let _ = self
            .inbox
            .send(Cmd::GroupMsg {
                origin,
                from,
                group,
                members,
                body,
                meta,
            })
            .await;
    }

    /// Home-mint a cross-network group message; returns the minted `MessageEvent`
    /// for fan-out (`None` if the directory actor is gone).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn group_mint(
        &self,
        origin: SessionId,
        sender: UserRef,
        group: GroupId,
        members: Vec<Account>,
        body: String,
        meta: MsgMeta,
    ) -> Option<MessageEvent> {
        let (tx, rx) = oneshot::channel();
        self.inbox
            .send(Cmd::GroupMint {
                origin,
                sender,
                group,
                members,
                body,
                meta,
                reply: tx,
            })
            .await
            .ok()?;
        rx.await.ok()
    }

    /// Ingest a home-minted group message on a member network.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn group_ingest(
        &self,
        origin: SessionId,
        sender: UserRef,
        group: GroupId,
        msgid: MsgId,
        body: String,
        meta: MsgMeta,
        members: Vec<Account>,
    ) {
        let _ = self
            .inbox
            .send(Cmd::GroupIngest {
                origin,
                sender,
                group,
                msgid,
                body,
                meta,
                members,
            })
            .await;
    }

    /// Home-mint a group mutation; returns `(wire event, mutation msgid)` for
    /// fan-out.
    pub(crate) async fn group_mutate(
        &self,
        origin: SessionId,
        sender: UserRef,
        group: GroupId,
        root: MsgId,
        kind: GroupMutKind,
        members: Vec<Account>,
    ) -> Option<(Event, MsgId)> {
        let (tx, rx) = oneshot::channel();
        self.inbox
            .send(Cmd::GroupMutate {
                origin,
                sender,
                group,
                root,
                kind,
                members,
                reply: tx,
            })
            .await
            .ok()?;
        rx.await.ok()
    }

    /// Ingest a home-minted group mutation on a member network.
    pub(crate) async fn group_mut_ingest(
        &self,
        sender: UserRef,
        group: GroupId,
        root: MsgId,
        msgid: MsgId,
        kind: GroupMutKind,
        members: Vec<Account>,
    ) {
        let _ = self
            .inbox
            .send(Cmd::GroupMutIngest {
                sender,
                group,
                root,
                msgid,
                kind,
                members,
            })
            .await;
    }

    pub(crate) async fn dm(
        &self,
        origin: SessionId,
        from: Account,
        to: Account,
        body: String,
        meta: MsgMeta,
    ) -> bool {
        let (delivered, done) = oneshot::channel();
        if self
            .inbox
            .send(Cmd::Dm {
                origin,
                from,
                to,
                body,
                meta,
                delivered,
            })
            .await
            .is_err()
        {
            return false;
        }
        done.await.unwrap_or(false)
    }

    pub(crate) async fn edit(
        &self,
        origin: SessionId,
        from: Account,
        peer: Account,
        root: MsgId,
        body: String,
    ) {
        let _ = self
            .inbox
            .send(Cmd::Edit {
                origin,
                from,
                peer,
                root,
                body,
            })
            .await;
    }

    pub(crate) async fn delete(
        &self,
        origin: SessionId,
        from: Account,
        peer: Account,
        root: MsgId,
    ) {
        let _ = self
            .inbox
            .send(Cmd::Delete {
                origin,
                from,
                peer,
                root,
            })
            .await;
    }

    pub(crate) async fn react(
        &self,
        origin: SessionId,
        from: Account,
        peer: Account,
        root: MsgId,
        emoji: String,
        add: bool,
    ) {
        let _ = self
            .inbox
            .send(Cmd::React {
                origin,
                from,
                peer,
                root,
                emoji,
                add,
            })
            .await;
    }

    pub(crate) async fn mark_sync(
        &self,
        origin: SessionId,
        account: Account,
        channel: ChannelName,
        msgid: MsgId,
        unread: u64,
        mentions: u64,
    ) {
        let _ = self
            .inbox
            .send(Cmd::MarkSync {
                origin,
                account,
                channel,
                msgid,
                unread,
                mentions,
            })
            .await;
    }
}

pub(crate) fn spawn(
    network: NetworkName,
    dm_policy: RetentionPolicy,
    events: Arc<dyn EventStore>,
    accounts: Arc<dyn AccountStore>,
) -> Directory {
    let (inbox_tx, inbox) = mpsc::channel(INBOX_CAPACITY);
    let actor = Actor {
        network,
        dm_policy,
        events,
        accounts,
        sessions: HashMap::new(),
        ulids: ulid::Generator::new(),
    };
    tokio::spawn(actor.run(inbox));
    Directory { inbox: inbox_tx }
}

/// One live session of an account: its direct-event queue plus the token that
/// forces it to close (WC7).
struct SessionEntry {
    id: SessionId,
    queue: mpsc::Sender<DirectEvent>,
    close: CancellationToken,
}

struct Actor {
    network: NetworkName,
    dm_policy: RetentionPolicy,
    events: Arc<dyn EventStore>,
    accounts: Arc<dyn AccountStore>,
    sessions: HashMap<Account, Vec<SessionEntry>>,
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
            Cmd::Register {
                account,
                session,
                queue,
                close,
            } => {
                self.sessions
                    .entry(account)
                    .or_default()
                    .push(SessionEntry {
                        id: session,
                        queue,
                        close,
                    });
            }
            Cmd::Deregister { account, session } => {
                if let Some(sessions) = self.sessions.get_mut(&account) {
                    sessions.retain(|s| s.id != session);
                    if sessions.is_empty() {
                        self.sessions.remove(&account);
                    }
                }
            }
            Cmd::Disconnect { account, reply } => {
                // Cancel only — each session deregisters itself as it unwinds
                // through `cleanup`, so co-members still see the parts/leaves.
                let n = self
                    .sessions
                    .get(&account)
                    .map(|sessions| {
                        sessions.iter().for_each(|s| s.close.cancel());
                        sessions.len()
                    })
                    .unwrap_or(0);
                let _ = reply.send(n);
            }
            Cmd::IsOnline { account, reply } => {
                let _ = reply.send(self.sessions.contains_key(&account));
            }
            Cmd::Dm {
                origin,
                from,
                to,
                body,
                meta,
                delivered,
            } => {
                // Existence check lives here so unknown recipients get the
                // same NO-SUCH-TARGET as everything hidden (§2.2).
                let exists = match self.accounts.password_phc(&to).await {
                    Ok(record) => record.is_some(),
                    Err(e) => {
                        error!("account lookup failed: {e}");
                        false
                    }
                };
                if !exists {
                    let _ = delivered.send(false);
                    return;
                }
                let msgid = self.mint();
                let record = EventRecord {
                    scope: Scope::dm(from.clone(), to.clone()),
                    msgid: msgid.clone(),
                    root: msgid.clone(),
                    sender: self.user(&from),
                    kind: EventKind::Message {
                        body: body.clone(),
                        meta: meta.clone(),
                    },
                };
                self.persist(record).await;
                let event = Event::Message(Box::new(MessageEvent {
                    target: Target::User(to.clone()),
                    sender: self.user(&from),
                    msgid,
                    body,
                    meta,
                    edited: None,
                    edited_at: None,
                }));
                self.deliver(&from, &to, origin, event);
                let _ = delivered.send(true);
            }
            Cmd::GroupMsg {
                origin,
                from,
                group,
                members,
                body,
                meta,
            } => {
                let msgid = self.mint();
                let record = EventRecord {
                    scope: Scope::Group(group),
                    msgid: msgid.clone(),
                    root: msgid.clone(),
                    sender: self.user(&from),
                    kind: EventKind::Message {
                        body: body.clone(),
                        meta: meta.clone(),
                    },
                };
                self.persist(record).await;
                let event = Event::Message(Box::new(MessageEvent {
                    target: Target::Group(group),
                    sender: self.user(&from),
                    msgid,
                    body,
                    meta,
                    edited: None,
                    edited_at: None,
                }));
                self.deliver_many(&members, origin, event);
            }
            Cmd::GroupMint {
                origin,
                sender,
                group,
                members,
                body,
                meta,
                reply,
            } => {
                let msgid = self.mint();
                let msg = MessageEvent {
                    target: Target::Group(group),
                    sender: sender.clone(),
                    msgid: msgid.clone(),
                    body: body.clone(),
                    meta: meta.clone(),
                    edited: None,
                    edited_at: None,
                };
                self.persist(EventRecord {
                    scope: Scope::Group(group),
                    msgid: msgid.clone(),
                    root: msgid,
                    sender,
                    kind: EventKind::Message { body, meta },
                })
                .await;
                self.deliver_many(&members, origin, Event::Message(Box::new(msg.clone())));
                let _ = reply.send(msg);
            }
            Cmd::GroupIngest {
                origin,
                sender,
                group,
                msgid,
                body,
                meta,
                members,
            } => {
                self.persist(EventRecord {
                    scope: Scope::Group(group),
                    msgid: msgid.clone(),
                    root: msgid.clone(),
                    sender: sender.clone(),
                    kind: EventKind::Message {
                        body: body.clone(),
                        meta: meta.clone(),
                    },
                })
                .await;
                let event = Event::Message(Box::new(MessageEvent {
                    target: Target::Group(group),
                    sender,
                    msgid,
                    body,
                    meta,
                    edited: None,
                    edited_at: None,
                }));
                // `origin` = the spoke poster's session for their own echo (its
                // pending label attaches), else `SessionId::MAX` (a fresh ingest,
                // no session skipped/labelled).
                self.deliver_many(&members, origin, event);
            }
            Cmd::GroupMutate {
                origin,
                sender,
                group,
                root,
                kind,
                members,
                reply,
            } => {
                let mut_msgid = self.mint();
                self.persist(EventRecord {
                    scope: Scope::Group(group),
                    msgid: mut_msgid.clone(),
                    root: root.clone(),
                    sender: sender.clone(),
                    kind: kind.event_kind(),
                })
                .await;
                let event = kind.to_event(group, sender, mut_msgid.clone(), root);
                self.deliver_many(&members, origin, event.clone());
                let _ = reply.send((event, mut_msgid));
            }
            Cmd::GroupMutIngest {
                sender,
                group,
                root,
                msgid,
                kind,
                members,
            } => {
                self.persist(EventRecord {
                    scope: Scope::Group(group),
                    msgid: msgid.clone(),
                    root: root.clone(),
                    sender: sender.clone(),
                    kind: kind.event_kind(),
                })
                .await;
                let event = kind.to_event(group, sender, msgid, root);
                self.deliver_many(&members, u64::MAX, event);
            }
            Cmd::Edit {
                origin,
                from,
                peer,
                root,
                body,
            } => {
                let msgid = self.mint();
                self.persist(EventRecord {
                    scope: Scope::dm(from.clone(), peer.clone()),
                    msgid: msgid.clone(),
                    root: root.clone(),
                    sender: self.user(&from),
                    kind: EventKind::Edit { body: body.clone() },
                })
                .await;
                let event = Event::Edited {
                    target: Target::User(peer.clone()),
                    user: self.user(&from),
                    msgid,
                    edit_of: root,
                    body,
                };
                self.deliver(&from, &peer, origin, event);
            }
            Cmd::Delete {
                origin,
                from,
                peer,
                root,
            } => {
                let msgid = self.mint();
                self.persist(EventRecord {
                    scope: Scope::dm(from.clone(), peer.clone()),
                    msgid,
                    root: root.clone(),
                    sender: self.user(&from),
                    kind: EventKind::Delete,
                })
                .await;
                let event = Event::Deleted {
                    target: Target::User(peer.clone()),
                    msgid: root,
                    by: Some(self.user(&from)),
                };
                self.deliver(&from, &peer, origin, event);
            }
            Cmd::React {
                origin,
                from,
                peer,
                root,
                emoji,
                add,
            } => {
                let msgid = self.mint();
                self.persist(EventRecord {
                    scope: Scope::dm(from.clone(), peer.clone()),
                    msgid,
                    root: root.clone(),
                    sender: self.user(&from),
                    kind: EventKind::React {
                        emoji: emoji.clone(),
                        add,
                    },
                })
                .await;
                let event = Event::Reaction {
                    target: Target::User(peer.clone()),
                    msgid: root,
                    emoji,
                    op: if add {
                        ReactionOp::Add
                    } else {
                        ReactionOp::Remove
                    },
                    by: self.user(&from),
                };
                self.deliver(&from, &peer, origin, event);
            }
            Cmd::MarkSync {
                origin,
                account,
                channel,
                msgid,
                unread,
                mentions,
            } => {
                // The marking session already got its labeled echo; this
                // syncs the account's other devices only — both the new marker
                // and the refreshed unread counts, so their badges update.
                for entry in self.sessions.get(&account).into_iter().flatten() {
                    let queue = &entry.queue;
                    if entry.id == origin {
                        continue;
                    }
                    push(
                        queue,
                        DirectEvent {
                            origin,
                            event: Event::Marked {
                                channel: channel.clone(),
                                msgid: msgid.clone(),
                            },
                        },
                    );
                    push(
                        queue,
                        DirectEvent {
                            origin,
                            event: Event::UnreadCounts {
                                channel: channel.clone(),
                                unread,
                                mentions,
                            },
                        },
                    );
                }
            }
            Cmd::Notify { account, event } => {
                // origin 0 is never a real session id (they start at 1), so
                // on_direct delivers this as a plain, unlabeled event.
                for entry in self.sessions.get(&account).into_iter().flatten() {
                    let queue = &entry.queue;
                    push(
                        queue,
                        DirectEvent {
                            origin: 0,
                            event: event.clone(),
                        },
                    );
                }
            }
        }
    }

    /// Deliver to every session of both participants (once, if self-DM).
    fn deliver(&self, a: &Account, b: &Account, origin: SessionId, event: Event) {
        let mut targets: Vec<&SessionEntry> = self.sessions.get(a).into_iter().flatten().collect();
        if b != a {
            targets.extend(self.sessions.get(b).into_iter().flatten());
        }
        for entry in targets {
            let queue = &entry.queue;
            push(
                queue,
                DirectEvent {
                    origin,
                    event: event.clone(),
                },
            );
        }
    }

    /// Fan an event out to every live session of each account in `members`
    /// (group DM delivery). `origin` drives the echo-label rule as usual.
    fn deliver_many(&self, members: &[Account], origin: SessionId, event: Event) {
        for account in members {
            for entry in self.sessions.get(account).into_iter().flatten() {
                push(
                    &entry.queue,
                    DirectEvent {
                        origin,
                        event: event.clone(),
                    },
                );
            }
        }
    }

    fn user(&self, account: &Account) -> UserRef {
        UserRef::new(account.clone(), self.network.clone())
    }

    fn mint(&mut self) -> MsgId {
        let ulid = self.ulids.generate().unwrap_or_else(|_| Ulid::new());
        MsgId::new(self.network.clone(), ulid)
    }

    async fn persist(&self, record: EventRecord) {
        if self.dm_policy == RetentionPolicy::Ephemeral {
            return;
        }
        if let Err(e) = self.events.append(record).await {
            error!("DM event not persisted: {e}");
        }
    }
}

/// Non-blocking delivery: a session drowning in direct events loses some
/// (it can HISTORY-resync) rather than stalling every DM on the network
/// behind one slow client — the directory is global, unlike channel
/// actors, so it must never block on a single receiver.
fn push(queue: &mpsc::Sender<DirectEvent>, event: DirectEvent) {
    if let Err(e) = queue.try_send(event) {
        warn!("direct event dropped for a slow session: {e}");
    }
}
