//! Group DMs (social layer): `GROUP CREATE/ADD/REMOVE/LEAVE/NAME` + `GROUPS`.
//! Membership lives in the `GroupStore`; messages ride the directory
//! (`Scope::Group`), minted single-writer like DMs. Members are `UserRef`s
//! (federation-able) — but message + membership *delivery* to remote members
//! rides the bridge (deferred; same-network fan-out works now).

use super::*;
use weft_proto::{GroupId, MemberAction, Ulid, UserRef};

impl<S: ControlStream> Session<S> {
    /// Push an event to every *local* member's live sessions, optionally
    /// skipping one account (the actor, who got the direct reply).
    async fn notify_group(&self, members: &[UserRef], event: Event, except: Option<&Account>) {
        for m in members {
            if m.network == self.ctx.info.network && except != Some(&m.account) {
                self.ctx
                    .directory
                    .notify(m.account.clone(), event.clone())
                    .await;
            }
        }
    }

    /// `GROUP CREATE <user@net>…` — open a group DM. The server mints the id,
    /// records the group (creator always a member), and pushes a `GROUP` event
    /// to every member; the creator's copy is the labelled reply.
    pub(super) async fn on_group_create(
        &mut self,
        label: Option<String>,
        members: Vec<UserRef>,
        account: Account,
    ) -> io::Result<Flow> {
        let me = UserRef::new(account.clone(), self.ctx.info.network.clone());
        let mut all = members;
        all.push(me);
        all.sort();
        all.dedup();

        let id = GroupId::new(Ulid::new());
        if let Err(e) = self
            .ctx
            .groups
            .create_group(
                id,
                &UserRef::new(account.clone(), self.ctx.info.network.clone()),
                &all,
                unix_now_ms(),
            )
            .await
        {
            return self.internal(label, &e).await;
        }
        let event = Event::Group {
            id,
            name: None,
            members: all.clone(),
        };
        self.send_event(label, event.clone()).await?;
        self.notify_group(&all, event, Some(&account)).await;
        Ok(Flow::Continue)
    }

    /// `GROUP ADD <&id> <user@net>` — add a member (any member may add). Pushes
    /// a `GROUP-MEMBER … join` to the group and a full `GROUP` to the newcomer.
    pub(super) async fn on_group_add(
        &mut self,
        label: Option<String>,
        group: GroupId,
        user: UserRef,
        account: Account,
    ) -> io::Result<Flow> {
        let me = UserRef::new(account, self.ctx.info.network.clone());
        if !self.member_or_deny(group, &me, &label).await? {
            return Ok(Flow::Continue);
        }
        match self.ctx.groups.add_group_member(group, &user).await {
            Ok(true) => {}
            Ok(false) => return self.no_such_target(label).await, // unknown group
            Err(e) => return self.internal(label, &e).await,
        }
        let members = self.group_members(group).await?;
        // Ack the actor.
        self.send_event(
            label,
            Event::GroupMember {
                group,
                user: user.clone(),
                action: MemberAction::Join,
            },
        )
        .await?;
        // Tell existing members someone joined, and the newcomer the whole group.
        self.notify_group(
            &members,
            Event::GroupMember {
                group,
                user: user.clone(),
                action: MemberAction::Join,
            },
            Some(&user.account).filter(|_| user.network == self.ctx.info.network),
        )
        .await;
        if user.network == self.ctx.info.network {
            let name = self.group_name(group).await;
            self.ctx
                .directory
                .notify(
                    user.account.clone(),
                    Event::Group {
                        id: group,
                        name,
                        members,
                    },
                )
                .await;
        }
        Ok(Flow::Continue)
    }

    /// `GROUP REMOVE <&id> <user@net>` — remove a member. Anyone may remove
    /// themselves; only the creator may remove someone else.
    pub(super) async fn on_group_remove(
        &mut self,
        label: Option<String>,
        group: GroupId,
        user: UserRef,
        account: Account,
    ) -> io::Result<Flow> {
        let me = UserRef::new(account, self.ctx.info.network.clone());
        if !self.member_or_deny(group, &me, &label).await? {
            return Ok(Flow::Continue);
        }
        if user != me {
            let is_creator = matches!(
                self.ctx.groups.group(group).await,
                Ok(Some(rec)) if rec.creator == me
            );
            if !is_creator {
                return self.no_such_target(label).await; // not allowed = hidden
            }
        }
        self.leave_group(label, group, user, me).await
    }

    /// `GROUP LEAVE <&id>` — leave a group (remove yourself).
    pub(super) async fn on_group_leave(
        &mut self,
        label: Option<String>,
        group: GroupId,
        account: Account,
    ) -> io::Result<Flow> {
        let me = UserRef::new(account, self.ctx.info.network.clone());
        self.leave_group(label, group, me.clone(), me).await
    }

    /// Shared remove-a-member path: `who` leaves `group` (initiated by `actor`).
    async fn leave_group(
        &mut self,
        label: Option<String>,
        group: GroupId,
        who: UserRef,
        _actor: UserRef,
    ) -> io::Result<Flow> {
        // Capture members *before* the removal so the leaver is told too.
        let members = self.group_members(group).await?;
        match self.ctx.groups.remove_group_member(group, &who).await {
            Ok(true) => {}
            Ok(false) => return self.no_such_target(label).await,
            Err(e) => return self.internal(label, &e).await,
        }
        let event = Event::GroupMember {
            group,
            user: who.clone(),
            action: MemberAction::Part,
        };
        self.send_event(label, event.clone()).await?;
        self.notify_group(&members, event, Some(&who.account)).await;
        Ok(Flow::Continue)
    }

    /// `GROUP NAME <&id> [:name]` — set/clear the group's name (any member).
    pub(super) async fn on_group_name(
        &mut self,
        label: Option<String>,
        group: GroupId,
        name: Option<String>,
        account: Account,
    ) -> io::Result<Flow> {
        let me = UserRef::new(account, self.ctx.info.network.clone());
        if !self.member_or_deny(group, &me, &label).await? {
            return Ok(Flow::Continue);
        }
        match self.ctx.groups.set_group_name(group, name.as_deref()).await {
            Ok(true) => {}
            Ok(false) => return self.no_such_target(label).await,
            Err(e) => return self.internal(label, &e).await,
        }
        let members = self.group_members(group).await?;
        let event = Event::Group {
            id: group,
            name,
            members: members.clone(),
        };
        self.send_event(label, event.clone()).await?;
        self.notify_group(&members, event, Some(&me.account)).await;
        Ok(Flow::Continue)
    }

    /// `GROUPS` — the caller's group DMs, one `GROUP` per group (a `BATCH`).
    pub(super) async fn on_groups(
        &mut self,
        label: Option<String>,
        account: Account,
    ) -> io::Result<Flow> {
        let me = UserRef::new(account, self.ctx.info.network.clone());
        let groups = match self.ctx.groups.groups_for(&me).await {
            Ok(g) => g,
            Err(e) => return self.internal(label, &e).await,
        };

        self.batches += 1;
        let id = format!("gr{}", self.batches);
        self.send_event(label.clone(), Event::BatchStart { id: id.clone() })
            .await?;
        for rec in groups {
            let members = self.group_members(rec.id).await?;
            self.send_event(
                None,
                Event::Group {
                    id: rec.id,
                    name: rec.name,
                    members,
                },
            )
            .await?;
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

    // ---- helpers ----

    /// Membership gate: `true` = a member (proceed); `false` = denied and a
    /// uniform `NO-SUCH-TARGET` has been sent (never leaks membership).
    async fn member_or_deny(
        &mut self,
        group: GroupId,
        me: &UserRef,
        label: &Option<String>,
    ) -> io::Result<bool> {
        match self.ctx.groups.is_group_member(group, me).await {
            Ok(true) => Ok(true),
            Ok(false) => {
                self.no_such_target(label.clone()).await?;
                Ok(false)
            }
            Err(e) => {
                self.internal(label.clone(), &e).await?;
                Ok(false)
            }
        }
    }

    async fn group_members(&mut self, group: GroupId) -> io::Result<Vec<UserRef>> {
        self.ctx
            .groups
            .group_members(group)
            .await
            .or_else(|_| Ok(Vec::new()))
    }

    async fn group_name(&self, group: GroupId) -> Option<String> {
        self.ctx
            .groups
            .group(group)
            .await
            .ok()
            .flatten()
            .and_then(|r| r.name)
    }
}
