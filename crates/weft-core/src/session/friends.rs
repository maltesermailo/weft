//! Social layer: `FRIEND ADD/ACCEPT/REMOVE` + `FRIENDS`. Friendships are
//! **federation-able** — the peer is a full `UserRef`, so a friend may live on
//! another network. Same-network changes are delivered to the peer's live
//! sessions through the directory `notify`; cross-network delivery rides the
//! bridge user-event transport (federation routing, tracked separately) — the
//! store already records cross-network relationships regardless.

use super::*;
use weft_proto::{FriendState, UserRef};
use weft_store::FriendOutcome;

impl<S: ControlStream> Session<S> {
    /// The caller's own fully-qualified identity on this network.
    fn me(&self, account: Account) -> UserRef {
        UserRef::new(account, self.ctx.info.network.clone())
    }

    /// Push a `FRIEND` update to a peer's live sessions when the peer is local.
    /// Cross-network peers are reached over the bridge (deferred routing).
    async fn notify_friend(&self, peer: &UserRef, about: UserRef, state: FriendState) {
        if peer.network == self.ctx.info.network {
            self.ctx
                .directory
                .notify(peer.account.clone(), Event::Friend { user: about, state })
                .await;
        }
    }

    /// `FRIEND ADD <user@net>` — send a request, or accept the peer's pending
    /// one. No existence check: an unknown handle is recorded and delivered if
    /// they ever appear, so the wire never leaks who exists (invariant 1).
    pub(super) async fn on_friend_add(
        &mut self,
        label: Option<String>,
        user: UserRef,
        account: Account,
    ) -> io::Result<Flow> {
        let me = self.me(account);
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
        // Push the corresponding change to the peer (nothing to push on a
        // duplicate request / already-friends).
        match outcome {
            FriendOutcome::Requested => {
                self.notify_friend(&user, me, FriendState::Incoming).await;
            }
            FriendOutcome::Accepted => {
                self.notify_friend(&user, me, FriendState::Friends).await;
            }
            _ => {}
        }
        Ok(Flow::Continue)
    }

    /// `FRIEND ACCEPT <user@net>` — accept a pending incoming request.
    pub(super) async fn on_friend_accept(
        &mut self,
        label: Option<String>,
        user: UserRef,
        account: Account,
    ) -> io::Result<Flow> {
        let me = self.me(account);
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
        self.notify_friend(&user, me, FriendState::Friends).await;
        Ok(Flow::Continue)
    }

    /// `FRIEND REMOVE <user@net>` — unfriend, cancel an outgoing request, or
    /// decline an incoming one (one verb, any state).
    pub(super) async fn on_friend_remove(
        &mut self,
        label: Option<String>,
        user: UserRef,
        account: Account,
    ) -> io::Result<Flow> {
        let me = self.me(account);
        let removed = match self.ctx.friends.friend_remove(&me, &user).await {
            Ok(r) => r,
            Err(e) => return self.internal(label, &e).await,
        };
        if !removed {
            return self.no_such_target(label).await;
        }
        self.send_event(label, Event::FriendRemoved { user: user.clone() })
            .await?;
        // Tell the peer the edge is gone.
        if user.network == self.ctx.info.network {
            self.ctx
                .directory
                .notify(user.account.clone(), Event::FriendRemoved { user: me })
                .await;
        }
        Ok(Flow::Continue)
    }

    /// `FRIENDS` — the caller's friends + pending requests, as a `BATCH` of
    /// `FRIEND` events.
    pub(super) async fn on_friends(
        &mut self,
        label: Option<String>,
        account: Account,
    ) -> io::Result<Flow> {
        let me = self.me(account);
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
