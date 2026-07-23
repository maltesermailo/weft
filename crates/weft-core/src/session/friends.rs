//! Social layer: `FRIEND ADD/ACCEPT/REMOVE` + `FRIENDS`. Friendships are
//! **federation-able** — the peer is a full `UserRef`, so a friend may live on
//! another network. Same-network changes are delivered to the peer's live
//! sessions through the directory `notify`; cross-network delivery rides the
//! bridge user-event transport (federation routing, tracked separately) — the
//! store already records cross-network relationships regardless.

use super::*;
use crate::FriendDeliver;
use weft_proto::{FriendState, UserRef};
use weft_store::FriendOutcome;

impl<S: ControlStream> Session<S> {
    /// Forward a friend command to the target's network over a §11.10 tunnel,
    /// but only when a **local** caller targets a **remote** user — a federated
    /// (already-received) action must never re-forward, or it would loop. weftd's
    /// tunnel driver reuses/establishes the bridge and opens a tunnel as `me`.
    fn deliver_if_remote(&self, me: &UserRef, target: &UserRef, line: String) {
        let local = &self.ctx.info.network;
        if &me.network == local && &target.network != local && !line.is_empty() {
            self.ctx.request_friend_deliver(FriendDeliver {
                peer: target.network.clone(),
                from: me.account.clone(),
                line,
            });
        }
    }

    /// Push a `FRIEND` update to a peer's live sessions when the peer is local.
    /// A **cross-network** peer is reached over the bridge — home forwards the
    /// change into a §11.10 tunnel to the peer's network, whose own session runs
    /// the matching handler there (send-side routing tracked separately).
    async fn notify_friend(&self, peer: &UserRef, about: UserRef, state: FriendState) {
        if peer.network == self.ctx.info.network {
            self.ctx
                .directory
                .notify(peer.account.clone(), Event::Friend { user: about, state })
                .await;
        }
    }

    /// `FRIEND ADD <user@net>` — send a request, or accept the peer's pending
    /// one. `me` is the caller's fully-qualified identity: for a local session
    /// it is `account@thisnet`; for a **federated (tunnelled) session** it is
    /// the foreign `account@peer` the peer vouched for — so the same handler
    /// serves both directions of a cross-network friendship. No existence
    /// check: an unknown handle is recorded and delivered if they ever appear,
    /// so the wire never leaks who exists (invariant 1).
    pub(super) async fn on_friend_add(
        &mut self,
        label: Option<String>,
        user: UserRef,
        me: UserRef,
    ) -> io::Result<Flow> {
        if user == me {
            return self.no_such_target(label).await; // can't befriend yourself
        }
        let outcome = match self
            .ctx
            .friends
            .friend_request(&me, &user, unix_now_ms())
            .await
        {
            Ok(o) => o,
            Err(e) => return self.internal(label, &e).await,
        };
        // Tell the caller their resulting state.
        let my_state = match outcome {
            FriendOutcome::Accepted | FriendOutcome::AlreadyFriends => FriendState::Friends,
            FriendOutcome::Requested | FriendOutcome::AlreadyPending => FriendState::Outgoing,
        };
        self.send_event(
            label,
            Event::Friend {
                user: user.clone(),
                state: my_state,
            },
        )
        .await?;
        // Push the corresponding change to the other side — a local peer via the
        // directory, a remote one via the tunnel (nothing on a duplicate).
        let line = format!("FRIEND ADD {user}");
        match outcome {
            FriendOutcome::Requested => {
                self.notify_friend(&user, me.clone(), FriendState::Incoming)
                    .await;
                self.deliver_if_remote(&me, &user, line);
            }
            FriendOutcome::Accepted => {
                self.notify_friend(&user, me.clone(), FriendState::Friends)
                    .await;
                self.deliver_if_remote(&me, &user, line);
            }
            _ => {}
        }
        Ok(Flow::Continue)
    }

    /// `FRIEND ACCEPT <user@net>` — accept a pending incoming request. `me` is
    /// the caller's full identity (local or federated — see [`on_friend_add`]).
    pub(super) async fn on_friend_accept(
        &mut self,
        label: Option<String>,
        user: UserRef,
        me: UserRef,
    ) -> io::Result<Flow> {
        let accepted = match self
            .ctx
            .friends
            .friend_accept(&me, &user, unix_now_ms())
            .await
        {
            Ok(a) => a,
            Err(e) => return self.internal(label, &e).await,
        };
        if !accepted {
            // No such pending request — uniform not-found (no state leak).
            return self.no_such_target(label).await;
        }
        self.send_event(
            label,
            Event::Friend {
                user: user.clone(),
                state: FriendState::Friends,
            },
        )
        .await?;
        self.notify_friend(&user, me.clone(), FriendState::Friends)
            .await;
        self.deliver_if_remote(&me, &user, format!("FRIEND ACCEPT {user}"));
        Ok(Flow::Continue)
    }

    /// `FRIEND REMOVE <user@net>` — unfriend, cancel an outgoing request, or
    /// decline an incoming one (one verb, any state).
    pub(super) async fn on_friend_remove(
        &mut self,
        label: Option<String>,
        user: UserRef,
        me: UserRef,
    ) -> io::Result<Flow> {
        let removed = match self.ctx.friends.friend_remove(&me, &user).await {
            Ok(r) => r,
            Err(e) => return self.internal(label, &e).await,
        };
        if !removed {
            return self.no_such_target(label).await;
        }
        self.send_event(label, Event::FriendRemoved { user: user.clone() })
            .await?;
        // Tell the other side the edge is gone — local peer via the directory,
        // remote peer via the tunnel.
        if user.network == self.ctx.info.network {
            self.ctx
                .directory
                .notify(user.account.clone(), Event::FriendRemoved { user: me })
                .await;
        } else {
            self.deliver_if_remote(&me, &user, format!("FRIEND REMOVE {user}"));
        }
        Ok(Flow::Continue)
    }

    /// `FRIENDS` — the caller's friends + pending requests, as a `BATCH` of
    /// `FRIEND` events.
    pub(super) async fn on_friends(
        &mut self,
        label: Option<String>,
        me: UserRef,
    ) -> io::Result<Flow> {
        let list = match self.ctx.friends.friends(&me).await {
            Ok(l) => l,
            Err(e) => return self.internal(label, &e).await,
        };

        self.batches += 1;
        let id = format!("fr{}", self.batches);
        self.send_event(label.clone(), Event::BatchStart { id: id.clone() })
            .await?;
        for (user, state) in list {
            self.send_event(None, Event::Friend { user, state }).await?;
        }
        self.send_event(
            label,
            Event::BatchEnd {
                id,
                truncated: false,
                compacted: false,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }
}
