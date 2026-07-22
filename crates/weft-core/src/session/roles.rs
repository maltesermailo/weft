//! §6.5 role handlers: CREATE / DELETE / ASSIGN / UNASSIGN / ROLES-OF / ROLES.

use super::*;

impl<S: ControlStream> Session<S> {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn on_role_create(
        &mut self,
        label: Option<String>,
        scope: String,
        color: String,
        caps: String,
        hoist: bool,
        position: i32,
        name: String,
        account: Account,
    ) -> io::Result<Flow> {
        let Some(token_scope) = TokenScope::parse(&scope) else {
            return self.bad_scope(label).await;
        };
        if let TokenScope::Namespace(ns) = &token_scope {
            if !self.namespace_exists(ns).await {
                return self.no_such_target(label).await;
            }
        }
        let now = unix_now();
        match self
            .ctx
            .account_has_cap(&account, &Capability::NsAdmin, &token_scope, now)
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "ns-admin").await,
            Err(e) => return self.internal(label, &e).await,
        }
        // The bundle must be real capabilities (strict-out).
        let Some(parsed) = parse_caps(&caps) else {
            self.send_err(label, ErrCode::Malformed, None, "unknown capability")
                .await?;
            return Ok(Flow::Continue);
        };
        let cap_strings: Vec<String> = parsed.iter().map(Capability::to_string).collect();
        if let Err(e) = self
            .ctx
            .roles
            .set_role(&scope, &name, &color, &cap_strings, hoist, position)
            .await
        {
            return self.internal(label, &e).await;
        }
        // §6.5 always-propagate: a *channel* role-permission is granted to
        // everyone who currently holds the same-named namespace role, so the
        // permission applies immediately — no re-assignment needed.
        if let Some((ns, _)) = scope.strip_prefix('#').and_then(|s| s.split_once('/')) {
            self.propagate_channel_role(ns, &scope, &name, &cap_strings, &account)
                .await?;
        }
        self.on_roles_list(label, scope).await
    }

    /// Grant a channel role's caps to every **explicitly assigned** holder of
    /// the same-named namespace role — so editing a channel permission reaches
    /// existing members with no re-assignment (§6.5, "always propagate").
    async fn propagate_channel_role(
        &mut self,
        ns: &str,
        channel_scope: &str,
        role_name: &str,
        caps: &[String],
        actor: &Account,
    ) -> io::Result<()> {
        let ns_scope = format!("ns:{ns}");
        let members = self
            .ctx
            .roles
            .role_members(&ns_scope, role_name)
            .await
            .unwrap_or_default();
        let caps_csv = caps.join(",");
        for member in members {
            self.on_grant(
                None,
                member.to_string(),
                channel_scope.to_string(),
                caps_csv.clone(),
                None,
                Actor::Local(actor.clone()),
            )
            .await?;
        }
        Ok(())
    }

    /// §6.5 ROLE DELETE (scope admin only) → updated `ROLES` batch.
    pub(super) async fn on_role_delete(
        &mut self,
        label: Option<String>,
        scope: String,
        name: String,
        account: Account,
    ) -> io::Result<Flow> {
        let Some(token_scope) = TokenScope::parse(&scope) else {
            return self.bad_scope(label).await;
        };
        let now = unix_now();
        match self
            .ctx
            .account_has_cap(&account, &Capability::NsAdmin, &token_scope, now)
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "ns-admin").await,
            Err(e) => return self.internal(label, &e).await,
        }
        if let Err(e) = self.ctx.roles.delete_role(&scope, &name).await {
            return self.internal(label, &e).await;
        }
        self.on_roles_list(label, scope).await
    }

    /// §6.5 ROLE RENAME (scope admin only) → updated `ROLES` batch.
    ///
    /// Roles are keyed by name, so this is a store migration that carries the
    /// definition *and* every assignment. Issued grants need no migration: a
    /// role's authority is its capability bundle, and that is unchanged.
    pub(super) async fn on_role_rename(
        &mut self,
        label: Option<String>,
        scope: String,
        old: String,
        new: String,
        account: Account,
    ) -> io::Result<Flow> {
        let Some(token_scope) = TokenScope::parse(&scope) else {
            return self.bad_scope(label).await;
        };
        // Invariant 4: the cap check precedes any mutation — and precedes the
        // existence probes below, so they can't be used to enumerate roles.
        match self
            .ctx
            .account_has_cap(&account, &Capability::NsAdmin, &token_scope, unix_now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "ns-admin").await,
            Err(e) => return self.internal(label, &e).await,
        }
        if old == new {
            return self.on_roles_list(label, scope).await;
        }
        let roles = match self.ctx.roles.roles(&scope).await {
            Ok(roles) => roles,
            Err(e) => return self.internal(label, &e).await,
        };
        if !roles.iter().any(|r| r.name == old) {
            return self.no_such_target(label).await;
        }
        // Renaming onto a live role would merge two bundles — refuse.
        if roles.iter().any(|r| r.name == new) {
            self.send_err(
                label,
                ErrCode::Policy,
                None,
                "a role with that name already exists",
            )
            .await?;
            return Ok(Flow::Continue);
        }
        if let Err(e) = self.ctx.roles.rename_role(&scope, &old, &new).await {
            return self.internal(label, &e).await;
        }
        self.on_roles_list(label, scope).await
    }

    /// §6.5 ROLE ASSIGN: grant the role's token bundle to an account. Resolves
    /// the role to its caps and reuses the GRANT path — the authority check
    /// (`account_can_grant`) and token issue are identical, so enforcement
    /// stays purely token-based.
    pub(super) async fn on_role_assign(
        &mut self,
        label: Option<String>,
        scope: String,
        subject: String,
        name: String,
        actor: Actor,
    ) -> io::Result<Flow> {
        let roles = match self.ctx.roles.roles(&scope).await {
            Ok(roles) => roles,
            Err(e) => return self.internal(label, &e).await,
        };
        let Some(role) = roles.into_iter().find(|r| r.name == name) else {
            return self.no_such_target(label).await;
        };
        // Record explicit membership — a role is held because it was assigned,
        // never inferred from caps (§6.5).
        if let Err(e) = self.ctx.roles.assign_role(&scope, &name, &subject).await {
            return self.internal(label, &e).await;
        }
        // Grant the role's own bundle at its scope (the labeled response).
        self.on_grant(
            label,
            subject.to_string(),
            scope.clone(),
            role.caps.join(","),
            None,
            actor.clone(),
        )
        .await?;
        // §6.5 role channel-permissions: assigning a *namespace* role also
        // grants any same-named channel role's caps on every channel in that
        // namespace — so "give role X send in #chan" follows the assignment.
        if let Some(ns) = scope.strip_prefix("ns:") {
            for (cscope, caps) in self.channel_role_caps(ns, &name).await {
                self.on_grant(None, subject.to_string(), cscope, caps, None, actor.clone())
                    .await?;
            }
        }
        Ok(Flow::Continue)
    }

    /// §6.5 ROLE UNASSIGN: drop explicit membership and revoke the role's caps
    /// (its bundle at the scope + any same-named channel roles' caps).
    pub(super) async fn on_role_unassign(
        &mut self,
        label: Option<String>,
        scope: String,
        subject: String,
        name: String,
        actor: Actor,
    ) -> io::Result<Flow> {
        let Some(token_scope) = TokenScope::parse(&scope) else {
            return self.bad_scope(label).await;
        };
        let now = unix_now();
        match self
            .ctx
            .actor_has_cap(&actor, &Capability::NsAdmin, &token_scope, now)
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "ns-admin").await,
            Err(e) => return self.internal(label, &e).await,
        }
        let role = self
            .ctx
            .roles
            .roles(&scope)
            .await
            .unwrap_or_default()
            .into_iter()
            .find(|r| r.name == name);
        if let Err(e) = self.ctx.roles.unassign_role(&scope, &name, &subject).await {
            return self.internal(label, &e).await;
        }
        // Revoke by the same key `on_grant` recorded under (§10.4): the member's
        // ULID (local) or `account@network` (foreign). Falls back to the handle
        // if unresolved (then a harmless no-op).
        let member_key = self
            .ctx
            .resolve_subject(&subject)
            .await
            .ok()
            .flatten()
            .map(|(_, key)| key)
            .unwrap_or_else(|| subject.clone());
        // Revoke the role's own caps, then any channel-role caps in the ns.
        if let Some(role) = role {
            let _ = self
                .ctx
                .caps
                .revoke_grants(&member_key, &scope, Some(&role.caps))
                .await;
        }
        if let Some(ns) = scope.strip_prefix("ns:") {
            for (cscope, caps) in self.channel_role_caps(ns, &name).await {
                let caps: Vec<String> = caps.split(',').map(str::to_string).collect();
                let _ = self
                    .ctx
                    .caps
                    .revoke_grants(&member_key, &cscope, Some(&caps))
                    .await;
            }
        }
        self.on_roles_of(label, scope, subject).await
    }

    /// §6.5 ROLES-OF: the roles an account is explicitly assigned at a scope.
    pub(super) async fn on_roles_of(
        &mut self,
        label: Option<String>,
        scope: String,
        account: String,
    ) -> io::Result<Flow> {
        let names = self
            .ctx
            .roles
            .roles_of(&scope, &account)
            .await
            .unwrap_or_default();
        self.send_event(
            label,
            Event::RoleMember {
                scope,
                account,
                roles: names.join(","),
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    /// `(channel-scope, caps-csv)` for every channel in `ns` that defines a
    /// role named `name` — the role's per-channel permissions (§6.5).
    async fn channel_role_caps(&self, ns: &str, name: &str) -> Vec<(String, String)> {
        let prefix = format!("#{ns}/");
        let channels = self
            .ctx
            .channel_store
            .list_channels()
            .await
            .unwrap_or_default();
        let mut out = Vec::new();
        for (chan, _) in channels {
            if !chan.as_str().starts_with(&prefix) {
                continue;
            }
            let cscope = chan.to_string();
            let croles = self.ctx.roles.roles(&cscope).await.unwrap_or_default();
            if let Some(crole) = croles.into_iter().find(|r| r.name == name) {
                if !crole.caps.is_empty() {
                    out.push((cscope, crole.caps.join(",")));
                }
            }
        }
        out
    }

    /// §6.5 ROLES: the role definitions at a scope, as a `BATCH` of `ROLE`.
    pub(super) async fn on_roles_list(
        &mut self,
        label: Option<String>,
        scope: String,
    ) -> io::Result<Flow> {
        let roles = match self.ctx.roles.roles(&scope).await {
            Ok(roles) => roles,
            Err(e) => return self.internal(label, &e).await,
        };
        self.batches += 1;
        let id = format!("r{}", self.batches);
        self.send_event(label.clone(), Event::BatchStart { id: id.clone() })
            .await?;
        for role in roles {
            self.send_event(
                None,
                Event::Role {
                    scope: scope.clone(),
                    color: role.color,
                    caps: role.caps.join(","),
                    hoist: role.hoist,
                    position: role.position,
                    name: role.name,
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

    /// §6.5 ROLE REORDER (scope admin only) → sets positions, re-emits `ROLES`.
    pub(super) async fn on_roles_reorder(
        &mut self,
        label: Option<String>,
        scope: String,
        order: Vec<String>,
        account: Account,
    ) -> io::Result<Flow> {
        let Some(token_scope) = TokenScope::parse(&scope) else {
            return self.bad_scope(label).await;
        };
        match self
            .ctx
            .account_has_cap(&account, &Capability::NsAdmin, &token_scope, unix_now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "ns-admin").await,
            Err(e) => return self.internal(label, &e).await,
        }
        if let Err(e) = self.ctx.roles.reorder_roles(&scope, &order).await {
            return self.internal(label, &e).await;
        }
        self.on_roles_list(label, scope).await
    }
}
