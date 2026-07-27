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
        pingable: bool,
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
            .set_role(
                &scope,
                &name,
                &color,
                &cap_strings,
                hoist,
                pingable,
                position,
            )
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

    /// §6.5 ROLE UPDATE (scope admin only, v0.13) — edit an existing role by its
    /// stable id: replace color/caps/hoist/pingable/position, and if the label
    /// changed, carry the definition **and every assignment** to the new name
    /// (subsumes the old ROLE RENAME; the id is stable so issued grants — keyed
    /// by the role's caps, not its name — need no migration).
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn on_role_update(
        &mut self,
        label: Option<String>,
        scope: String,
        role: weft_proto::RoleId,
        color: String,
        caps: String,
        hoist: bool,
        pingable: bool,
        position: i32,
        name: String,
        account: Account,
    ) -> io::Result<Flow> {
        let Some(token_scope) = TokenScope::parse(&scope) else {
            return self.bad_scope(label).await;
        };
        // Invariant 4: the cap check precedes any mutation (and the id probe).
        match self
            .ctx
            .account_has_cap(&account, &Capability::NsAdmin, &token_scope, unix_now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "ns-admin").await,
            Err(e) => return self.internal(label, &e).await,
        }
        // Resolve the id → the role's current name.
        let Some((_, old_def)) = self
            .ctx
            .roles
            .role_by_id(&role.to_string())
            .await
            .ok()
            .flatten()
        else {
            return self.no_such_target(label).await;
        };
        let old = old_def.name;
        // A label change carries the definition + assignments to the new name.
        if old != name {
            let roles = match self.ctx.roles.roles(&scope).await {
                Ok(roles) => roles,
                Err(e) => return self.internal(label, &e).await,
            };
            if roles.iter().any(|r| r.name == name) {
                self.send_err(
                    label,
                    ErrCode::Policy,
                    None,
                    "a role with that name already exists",
                )
                .await?;
                return Ok(Flow::Continue);
            }
            if let Err(e) = self.ctx.roles.rename_role(&scope, &old, &name).await {
                return self.internal(label, &e).await;
            }
        }
        // Replace the definition's fields on the (possibly renamed) role.
        let Some(parsed) = parse_caps(&caps) else {
            self.send_err(label, ErrCode::Malformed, None, "unknown capability")
                .await?;
            return Ok(Flow::Continue);
        };
        let cap_strings: Vec<String> = parsed.iter().map(Capability::to_string).collect();
        if let Err(e) = self
            .ctx
            .roles
            .set_role(
                &scope,
                &name,
                &color,
                &cap_strings,
                hoist,
                pingable,
                position,
            )
            .await
        {
            return self.internal(label, &e).await;
        }
        // §6.5 always-propagate a channel role-permission edit to holders.
        if let Some((ns, _)) = scope.strip_prefix('#').and_then(|s| s.split_once('/')) {
            self.propagate_channel_role(ns, &scope, &name, &cap_strings, &account)
                .await?;
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
        // The `@everyone` role is implicit (every member holds it, resolved live
        // in `actor_has_cap`) — assigning it would materialize stale grants.
        if name == crate::context::EVERYONE_ROLE {
            self.send_err(
                label,
                ErrCode::Malformed,
                None,
                "the everyone role is implicit and cannot be assigned",
            )
            .await?;
            return Ok(Flow::Continue);
        }
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
    /// Resolve an account's assigned role **names** at a scope to their stable
    /// role **ids** (v0.13) — the wire form of `ROLE-MEMBER`/`NS-MEMBER-INFO`, so
    /// clients address roles unambiguously (names aren't unique). A name with no
    /// id (a race with a delete) is dropped.
    pub(super) async fn assigned_role_ids(&self, scope: &str, account: &str) -> Vec<String> {
        let names = self
            .ctx
            .roles
            .roles_of(scope, account)
            .await
            .unwrap_or_default();
        let mut ids = Vec::with_capacity(names.len());
        for name in names {
            if let Ok(Some(id)) = self.ctx.roles.role_id(scope, &name).await {
                ids.push(id);
            }
        }
        ids
    }

    pub(super) async fn on_roles_of(
        &mut self,
        label: Option<String>,
        scope: String,
        account: String,
    ) -> io::Result<Flow> {
        let roles = self.assigned_role_ids(&scope, &account).await;
        self.send_event(
            label,
            Event::RoleMember {
                scope,
                account,
                roles: roles.join(","),
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
            // The stable role id (v0.13) — lazily minted, keyed by (scope, name).
            let Ok(Some(rid)) = self.ctx.roles.role_id(&scope, &role.name).await else {
                continue;
            };
            let Ok(role_id) = rid.parse::<weft_proto::RoleId>() else {
                continue;
            };
            self.send_event(
                None,
                Event::Role {
                    scope: scope.clone(),
                    role: role_id,
                    color: role.color,
                    caps: role.caps.join(","),
                    hoist: role.hoist,
                    pingable: role.pingable,
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
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    /// §6.5 GRANTS — list a scope's per-subject grants so the channel-permission
    /// editor can surface individual-member overrides. ns-admin gated (the
    /// roster is sensitive). Role-propagated grants are filtered out: a member
    /// whose channel caps come from a channel role is covered by that role, not
    /// a genuine override, so only members *not* holding a channel-mapped role
    /// appear. Foreign / device-key grants are omitted — overrides target local
    /// accounts. Emits a `gr…` BATCH of `GRANT-INFO`.
    pub(super) async fn on_grants_at(
        &mut self,
        label: Option<String>,
        scope: String,
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

        // Grants at this scope, keyed by resolved store-key (ULID for a local
        // account) → caps.
        let grant_caps: std::collections::HashMap<String, Vec<String>> =
            match self.ctx.caps.grants_at_scope(&scope).await {
                Ok(grants) => grants.into_iter().map(|g| (g.subject, g.caps)).collect(),
                Err(e) => return self.internal(label, &e).await,
            };

        // Only a channel scope carries a namespace + member roster to resolve
        // against; a bare `#chan` or `ns:`/`*` scope yields an empty roster.
        let ns = scope
            .strip_prefix('#')
            .and_then(|s| s.split_once('/'))
            .map(|(ns, _)| ns.to_string());

        let mut overrides: Vec<(Account, String)> = Vec::new();
        if let Some(ns) = ns {
            if ns.parse::<weft_proto::NamespaceId>().is_ok() {
                let ns_scope = format!("ns:{ns}");
                // Members who hold a channel-mapped role already get its caps by
                // propagation — skip them so the list is genuine overrides.
                let mut role_covered = std::collections::HashSet::<String>::new();
                if let Ok(roles) = self.ctx.roles.roles(&scope).await {
                    for role in roles {
                        if role.name == crate::context::EVERYONE_ROLE {
                            continue;
                        }
                        if let Ok(members) =
                            self.ctx.roles.role_members(&ns_scope, &role.name).await
                        {
                            role_covered.extend(members);
                        }
                    }
                }
                if let Ok(members) = self.ctx.memberships.ns_members(&ns).await {
                    for member in members {
                        if role_covered.contains(member.as_str()) {
                            continue;
                        }
                        let Ok(Some(ulid)) = self.ctx.accounts.account_ulid(&member).await else {
                            continue;
                        };
                        if let Some(caps) = grant_caps.get(&ulid) {
                            if !caps.is_empty() {
                                overrides.push((member, caps.join(",")));
                            }
                        }
                    }
                }
            }
        }

        self.batches += 1;
        let id = format!("gr{}", self.batches);
        self.send_event(label.clone(), Event::BatchStart { id: id.clone() })
            .await?;
        for (subject, caps) in overrides {
            self.send_event(
                None,
                Event::GrantInfo {
                    scope: scope.clone(),
                    subject,
                    caps,
                },
            )
            .await?;
        }
        self.send_event(
            label,
            Event::BatchEnd {
                id,
                truncated: false,
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
