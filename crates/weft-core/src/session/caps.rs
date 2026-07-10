//! §6.5 / §10.4 capability handlers: CAPS query, GRANT, REVOKE.

use super::*;

impl<S: ControlStream> Session<S> {
    pub(super) async fn on_caps(
        &mut self,
        label: Option<String>,
        subject: Account,
        scope_str: String,
    ) -> io::Result<Flow> {
        let Some(scope) = TokenScope::parse(&scope_str) else {
            return self.no_such_target(label).await;
        };
        let now = unix_now();
        let mut held = Vec::new();
        for cap in Capability::STANDARD {
            match self.ctx.account_has_cap(&subject, &cap, &scope, now).await {
                Ok(true) => held.push(cap.to_string()),
                Ok(false) => {}
                Err(e) => return self.internal(label, &e).await,
            }
        }
        self.send_event(
            label,
            Event::Caps {
                account: subject,
                scope: scope_str,
                caps: held.join(","),
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    pub(super) async fn on_grant(
        &mut self,
        label: Option<String>,
        subject: String,
        scope: String,
        caps: String,
        expiry: Option<u64>,
        actor: Actor,
    ) -> io::Result<Flow> {
        let Some(token_scope) = TokenScope::parse(&scope) else {
            return self.bad_scope(label).await;
        };
        // A grant at ns: scope needs the namespace to exist; enforcement
        // works through owner/table (the network-key-signed token is a
        // same-network artifact — a root-key-signed chain arrives with
        // federation, M5).
        if let TokenScope::Namespace(ns) = &token_scope {
            if !self.namespace_exists(ns).await {
                return self.no_such_target(label).await;
            }
        }
        let parsed = match parse_caps(&caps) {
            Some(caps) => caps,
            None => {
                self.send_err(label, ErrCode::Malformed, None, "unknown capability")
                    .await?;
                return Ok(Flow::Continue);
            }
        };
        let now = unix_now();
        // Invariant 4: authority checked before any state change.
        for cap in &parsed {
            match self
                .ctx
                .actor_can_grant(&actor, cap, &token_scope, now)
                .await
            {
                Ok(true) => {}
                Ok(false) => return self.cap_required(label, &format!("grant:{cap}")).await,
                Err(e) => return self.internal(label, &e).await,
            }
        }
        let epoch = match self.ctx.caps.scope_epoch(&scope).await {
            Ok(epoch) => epoch,
            Err(e) => return self.internal(label, &e).await,
        };
        let absolute_expiry = expiry.map(|ttl| now + ttl);
        // Resolve to the stable identity (ULID / device key / foreign) — both the
        // grant record and the signed token key by it, never the handle (§10.4).
        let (subj, store_key) = match self.ctx.resolve_subject(&subject).await {
            Ok(Some(v)) => v,
            Ok(None) => return self.no_such_target(label).await,
            Err(e) => return self.internal(label, &e).await,
        };
        let cap_strings: Vec<String> = parsed.iter().map(Capability::to_string).collect();
        if let Err(e) = self
            .ctx
            .caps
            .record_grant(&store_key, &scope, &cap_strings, epoch, absolute_expiry)
            .await
        {
            return self.internal(label, &e).await;
        }
        let token = self.ctx.mint_token(
            subj,
            token_scope,
            parsed,
            epoch,
            absolute_expiry.unwrap_or(u64::MAX),
        );
        self.send_event(
            label,
            Event::Token {
                subject,
                scope,
                token,
                expiry: absolute_expiry,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    pub(super) async fn on_revoke(
        &mut self,
        label: Option<String>,
        subject: String,
        scope: String,
        caps: Option<String>,
        epoch: Option<u64>,
        actor: Actor,
    ) -> io::Result<Flow> {
        let Some(token_scope) = TokenScope::parse(&scope) else {
            return self.bad_scope(label).await;
        };
        // Resolve to the stable identity the grant store keys by (§10.4).
        let (subj, store_key) = match self.ctx.resolve_subject(&subject).await {
            Ok(Some(v)) => v,
            Ok(None) => return self.no_such_target(label).await,
            Err(e) => return self.internal(label, &e).await,
        };
        // The caps we intend to remove (given, or all currently held).
        let grants = match self.ctx.caps.grants_for(&store_key).await {
            Ok(grants) => grants,
            Err(e) => return self.internal(label, &e).await,
        };
        let cap_list: Option<Vec<String>> = caps.as_ref().map(|c| {
            c.split(',')
                .filter(|c| !c.is_empty())
                .map(str::to_string)
                .collect()
        });
        let target: Vec<Capability> = match &cap_list {
            Some(list) => list.iter().filter_map(|c| c.parse().ok()).collect(),
            None => grants
                .iter()
                .filter(|g| g.scope == scope)
                .flat_map(|g| g.caps.iter())
                .filter_map(|c| c.parse().ok())
                .collect(),
        };
        let now = unix_now();
        for cap in &target {
            match self
                .ctx
                .actor_can_grant(&actor, cap, &token_scope, now)
                .await
            {
                Ok(true) => {}
                Ok(false) => return self.cap_required(label, &format!("grant:{cap}")).await,
                Err(e) => return self.internal(label, &e).await,
            }
        }
        if let Err(e) = self
            .ctx
            .caps
            .revoke_grants(&store_key, &scope, cap_list.as_deref())
            .await
        {
            return self.internal(label, &e).await;
        }
        // `epoch` present = bump the scope's revocation epoch, killing every
        // already-issued token there (§10.4).
        let new_epoch = if epoch.is_some() {
            self.ctx.caps.bump_epoch(&scope).await
        } else {
            self.ctx.caps.scope_epoch(&scope).await
        };
        let new_epoch = match new_epoch {
            Ok(epoch) => epoch,
            Err(e) => return self.internal(label, &e).await,
        };
        // Re-mint a token reflecting what remains (empty caps if none).
        let remaining: Vec<Capability> = match self.ctx.caps.grants_for(&store_key).await {
            Ok(grants) => grants
                .into_iter()
                .filter(|g| g.scope == scope)
                .flat_map(|g| g.caps)
                .filter_map(|c| c.parse().ok())
                .collect(),
            Err(e) => return self.internal(label, &e).await,
        };
        let token = self
            .ctx
            .mint_token(subj, token_scope, remaining, new_epoch, u64::MAX);
        self.send_event(
            label,
            Event::Token {
                subject,
                scope,
                token,
                expiry: None,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }
}
