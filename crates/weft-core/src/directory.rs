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
use tracing::{error, warn};
use ulid::Ulid;
use weft_proto::{
    Account, ChannelName, Event, MessageEvent, MsgId, MsgMeta, NetworkName, ReactionOp,
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
    },
    Deregister {
        account: Account,
        session: SessionId,
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
    /// §6.3: fan a fresh read marker out to the account's *other* sessions.
    MarkSync {
        origin: SessionId,
        account: Account,
        channel: ChannelName,
        msgid: MsgId,
    },
}

impl Directory {
    pub(crate) async fn register(
        &self,
        account: Account,
        session: SessionId,
        queue: mpsc::Sender<DirectEvent>,
    ) {
        let _ = self
            .inbox
            .send(Cmd::Register {
                account,
                session,
                queue,
            })
            .await;
    }

    pub(crate) async fn deregister(&self, account: Account, session: SessionId) {
        let _ = self.inbox.send(Cmd::Deregister { account, session }).await;
    }

    /// False = recipient does not exist (→ NO-SUCH-TARGET).
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
    ) {
        let _ = self
            .inbox
            .send(Cmd::MarkSync {
                origin,
                account,
                channel,
                msgid,
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

struct Actor {
    network: NetworkName,
    dm_policy: RetentionPolicy,
    events: Arc<dyn EventStore>,
    accounts: Arc<dyn AccountStore>,
    sessions: HashMap<Account, Vec<(SessionId, mpsc::Sender<DirectEvent>)>>,
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
            } => {
                self.sessions
                    .entry(account)
                    .or_default()
                    .push((session, queue));
            }
            Cmd::Deregister { account, session } => {
                if let Some(sessions) = self.sessions.get_mut(&account) {
                    sessions.retain(|(id, _)| *id != session);
                    if sessions.is_empty() {
                        self.sessions.remove(&account);
                    }
                }
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
            } => {
                // The marking session already got its labeled echo; this
                // syncs the account's other devices only.
                for (session, queue) in self.sessions.get(&account).into_iter().flatten() {
                    if *session == origin {
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
                }
            }
        }
    }

    /// Deliver to every session of both participants (once, if self-DM).
    fn deliver(&self, a: &Account, b: &Account, origin: SessionId, event: Event) {
        let mut targets: Vec<&(SessionId, mpsc::Sender<DirectEvent>)> =
            self.sessions.get(a).into_iter().flatten().collect();
        if b != a {
            targets.extend(self.sessions.get(b).into_iter().flatten());
        }
        for (_, queue) in targets {
            push(
                queue,
                DirectEvent {
                    origin,
                    event: event.clone(),
                },
            );
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
