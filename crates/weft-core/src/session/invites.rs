//! §6.5 invite handlers: MINT / REVOKE / REDEEM.

use super::*;

impl<S: ControlStream> Session<S> {
    pub(super) async fn on_invite_mint(
        &mut self,
        label: Option<String>,
        scope: String,
        max_uses: Option<u32>,
        expiry: Option<u64>,
        actor: Actor,
    ) -> io::Result<Flow> {
        let Some(token_scope) = TokenScope::parse(&scope) else {
            return self.bad_scope(label).await;
        };
        match self
            .ctx
            .actor_has_cap(&actor, &Capability::Invite, &token_scope, unix_now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "invite").await,
            Err(e) => return self.internal(label, &e).await,
        }
        // The invite grants membership (view+send) at the scope on redeem.
        let caps = vec!["view".to_string(), "send".to_string()];
        let invite_id = format!("i{}", weft_proto::Ulid::new());
        let absolute_expiry = expiry.map(|ttl| unix_now() + ttl);
        let creator = self.actor_ref(&actor);
        if let Err(e) = self
            .ctx
            .invites
            .create_invite(InviteRecord {
                id: invite_id.clone(),
                scope: scope.clone(),
                caps,
                uses_left: max_uses,
                expiry: absolute_expiry,
                creator,
            })
            .await
        {
            return self.internal(label, &e).await;
        }
        // Federation-ready link: carry the namespace (from `ns:<name>` or
        // `#<ns>/<chan>`) so a *foreign* redeemer can auto-federate to it
        // (§11.10). Top-level channels have no namespace and stay the short form.
        let network = &self.ctx.info.network;
        let link = match invite_scope_namespace(&scope) {
            Some(ns) => format!("weft://{network}/{ns}/i/{invite_id}"),
            None => format!("weft://{network}/i/{invite_id}"),
        };
        self.send_event(
            label,
            Event::Invited {
                scope,
                invite_id: invite_id.clone(),
                token: invite_id,
                link: Some(link),
                max_uses,
                expiry: absolute_expiry,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    pub(super) async fn on_invite_revoke(
        &mut self,
        label: Option<String>,
        invite_id: String,
        actor: Actor,
    ) -> io::Result<Flow> {
        let invite = match self.ctx.invites.invite(&invite_id).await {
            Ok(Some(invite)) => invite,
            Ok(None) => return self.no_such_target(label).await,
            Err(e) => return self.internal(label, &e).await,
        };
        let Some(token_scope) = TokenScope::parse(&invite.scope) else {
            return self.no_such_target(label).await;
        };
        match self
            .ctx
            .actor_has_cap(&actor, &Capability::Invite, &token_scope, unix_now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "invite").await,
            Err(e) => return self.internal(label, &e).await,
        }
        if let Err(e) = self.ctx.invites.revoke_invite(&invite_id).await {
            return self.internal(label, &e).await;
        }
        // Confirmation: the invite echoed back closed (max-uses=0, no link).
        self.send_event(
            label,
            Event::Invited {
                scope: invite.scope,
                invite_id: invite_id.clone(),
                token: invite_id,
                link: None,
                max_uses: Some(0),
                expiry: None,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    pub(super) async fn on_invite_revoke_all(
        &mut self,
        label: Option<String>,
        scope: String,
        actor: Actor,
    ) -> io::Result<Flow> {
        let Some(token_scope) = TokenScope::parse(&scope) else {
            return self.bad_scope(label).await;
        };
        // The bulk revoke is a namespace-admin action: the scope must resolve to
        // a namespace (`ns:<name>` or `#<ns>/<chan>`).
        let Some(ns) = invite_scope_namespace(&scope) else {
            return self.bad_scope(label).await;
        };
        let ns = ns.to_string();
        match self
            .ctx
            .actor_has_cap(&actor, &Capability::Invite, &token_scope, unix_now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "invite").await,
            Err(e) => return self.internal(label, &e).await,
        }
        if let Err(e) = self.ctx.invites.revoke_invites_for_namespace(&ns).await {
            return self.internal(label, &e).await;
        }
        // Bulk-close ack: an INVITED marker with a `*` id and max-uses=0 (the
        // client treats max-uses=0 as "closed" and ignores the `*` id, §6.5).
        self.send_event(
            label,
            Event::Invited {
                scope,
                invite_id: "*".to_string(),
                token: "*".to_string(),
                link: None,
                max_uses: Some(0),
                expiry: None,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    pub(super) async fn on_invite_redeem(
        &mut self,
        label: Option<String>,
        invite_id: String,
        account: Account,
    ) -> io::Result<Flow> {
        let outcome = match self.ctx.invites.redeem_invite(&invite_id, unix_now()).await {
            Ok(outcome) => outcome,
            Err(e) => return self.internal(label, &e).await,
        };
        // §6.5/§2.2: dead or exhausted invites are indistinct from absent.
        let invite = match outcome {
            RedeemOutcome::Redeemed(invite) => invite,
            RedeemOutcome::Exhausted | RedeemOutcome::Gone => {
                return self.no_such_target(label).await;
            }
        };
        // Bind the granted membership caps to the redeemer — keyed by ULID, the
        // same identity account_has_cap resolves to (§10.4).
        let epoch = match self.ctx.caps.scope_epoch(&invite.scope).await {
            Ok(epoch) => epoch,
            Err(e) => return self.internal(label, &e).await,
        };
        let redeemer_key = match self.ctx.accounts.account_ulid(&account).await {
            Ok(Some(u)) => u,
            Ok(None) => return self.no_such_target(label).await,
            Err(e) => return self.internal(label, &e).await,
        };
        if let Err(e) = self
            .ctx
            .caps
            .record_grant(&redeemer_key, &invite.scope, &invite.caps, epoch, None)
            .await
        {
            return self.internal(label, &e).await;
        }
        debug!(%account, scope = %invite.scope, "invite redeemed");
        // Channel-scope invites auto-join (§6.5); namespace-scope invites
        // grant membership and return the namespace's NS-META so the client
        // knows what it joined (its channels come via DISCOVER/JOIN).
        match TokenScope::parse(&invite.scope) {
            Some(TokenScope::Channel(chan)) => match chan.parse::<ChannelName>() {
                Ok(channel) => self.on_join(label, channel, None, account).await,
                Err(_) => self.no_such_target(label).await,
            },
            Some(TokenScope::Namespace(ns)) => match ns.parse::<weft_proto::NamespaceName>() {
                Ok(name) => match self.ctx.namespaces.namespace(&name).await {
                    Ok(Some(record)) => {
                        self.send_event(label, Self::ns_meta_event(&record)).await?;
                        Ok(Flow::Continue)
                    }
                    Ok(None) => self.no_such_target(label).await,
                    Err(e) => self.internal(label, &e).await,
                },
                Err(_) => self.no_such_target(label).await,
            },
            _ => self.no_such_target(label).await,
        }
    }

    /// `INVITE LIST <scope>` — the live invites at `scope`, as a `BATCH` of
    /// `INVITE-INFO` (cap `invite`, same as mint/revoke). Powers the Discord-
    /// style invites menu: id, creator, uses left, expiry — each revocable.
    pub(super) async fn on_invite_list(
        &mut self,
        label: Option<String>,
        scope: String,
        actor: Actor,
    ) -> io::Result<Flow> {
        let Some(token_scope) = TokenScope::parse(&scope) else {
            return self.bad_scope(label).await;
        };
        match self
            .ctx
            .actor_has_cap(&actor, &Capability::Invite, &token_scope, unix_now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "invite").await,
            Err(e) => return self.internal(label, &e).await,
        }
        let invites = match self.ctx.invites.invites_for_scope(&scope).await {
            Ok(v) => v,
            Err(e) => return self.internal(label, &e).await,
        };

        self.batches += 1;
        let id = format!("il{}", self.batches);
        self.send_event(label.clone(), Event::BatchStart { id: id.clone() })
            .await?;
        for inv in invites {
            self.send_event(
                None,
                Event::InviteInfo {
                    scope: inv.scope,
                    invite_id: inv.id,
                    creator: inv.creator,
                    uses_left: inv.uses_left,
                    expiry: inv.expiry,
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

    /// The acting user as a `UserRef` (for stamping the invite creator).
    fn actor_ref(&self, actor: &Actor) -> weft_proto::UserRef {
        match actor {
            Actor::Local(a) => weft_proto::UserRef::new(a.clone(), self.ctx.info.network.clone()),
            Actor::Foreign(u) => u.parse().unwrap_or_else(|_| {
                weft_proto::UserRef::new(
                    "unknown".parse().expect("valid account"),
                    self.ctx.info.network.clone(),
                )
            }),
        }
    }
}
