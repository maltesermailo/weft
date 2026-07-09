//! §6.7 moderation handlers: MUTE/UNMUTE/BAN/UNBAN/KICK + REPORT/REPORTS.
//! Split out of the session engine; methods are `pub(super)` so the
//! dispatch in the parent module can route to them.

use super::*;

impl<S: ControlStream> Session<S> {
    pub(super) async fn on_report(
        &mut self,
        label: Option<String>,
        msgid: MsgId,
        category: String,
        scope: ReportScope,
        note: Option<String>,
        account: Account,
    ) -> io::Result<Flow> {
        // §6.7 rate limit — per account, rolling hour.
        let now_ms = unix_now_ms();
        match self
            .ctx
            .reports
            .reports_by_since(&account, now_ms.saturating_sub(REPORT_RATE_WINDOW_MS))
            .await
        {
            Ok(count) if count >= REPORT_RATE_LIMIT => {
                let mut err = ErrEvent::new(ErrCode::Throttled, "report rate limit");
                err.retry_after = Some(REPORT_RATE_WINDOW_MS / 1000);
                return self
                    .send_event(label, Event::Err(err))
                    .await
                    .map(|_| Flow::Continue);
            }
            Ok(_) => {}
            Err(e) => return self.internal(label, &e).await,
        }

        // Resolve the reported message. Anything not found or not visible to
        // the reporter answers NO-SUCH-TARGET (invariant 1: you can only
        // report what you can see).
        let root = match self.ctx.events.find_root(msgid.ulid()).await {
            Ok(Some(root)) => root,
            Ok(None) => return self.no_such_target(label).await,
            Err(e) => return self.internal(label, &e).await,
        };
        match &root.scope {
            Scope::Channel(channel) => {
                if !self.joined.contains_key(channel)
                    || self.view_gated_denied(channel, &account).await
                {
                    return self.no_such_target(label).await;
                }
            }
            Scope::Dm(a, b) => {
                if account != *a && account != *b {
                    return self.no_such_target(label).await;
                }
            }
        }

        // Routing (§6.7): ns → the channel's namespace owner (or the operator
        // for a top-level channel / DM); net → the operator. `csam`/`illegal`
        // always ALSO reach the operator, who is the legally accountable party.
        let ns = match &root.scope {
            Scope::Channel(c) => channel_namespace(c),
            Scope::Dm(..) => None,
        };
        let mut queue_scopes: Vec<String> = Vec::new();
        match scope {
            ReportScope::Net => queue_scopes.push("*".into()),
            ReportScope::Ns => match &ns {
                Some(name) => queue_scopes.push(format!("ns:{name}")),
                None => queue_scopes.push("*".into()),
            },
        }
        if matches!(category.as_str(), "csam" | "illegal") && !queue_scopes.iter().any(|q| q == "*")
        {
            queue_scopes.push("*".into());
        }

        let state = self.content_state(&root.scope).await;
        let report_id = Ulid::new().to_string();
        let record = ReportRecord {
            id: report_id.clone(),
            msgid: msgid.clone(),
            scope: root.scope.clone(),
            category: category.clone(),
            state,
            reporter: account.clone(),
            note,
            queue_scopes: queue_scopes.clone(),
            status: ReportStatus::Open,
            filed_at_ms: now_ms,
            held_roots: vec![],
            resolution: None,
            holds_released: false,
        };
        if let Err(e) = self.ctx.reports.file_report(record).await {
            return self.internal(label, &e).await;
        }

        // Live push to each queue's default handlers (§6.7). The reporter's
        // identity travels to handlers (accountability), never to the
        // reported party (invariant 12: they receive nothing).
        for queue in &queue_scopes {
            let filed = Event::ReportFiled {
                report_id: report_id.clone(),
                msgid: msgid.clone(),
                category: category.clone(),
                state,
                scope: if queue == "*" {
                    ReportScope::Net
                } else {
                    ReportScope::Ns
                },
                reporter: Some(account.to_string()),
            };
            self.notify_queue_handlers(queue, filed).await;
        }

        self.send_event(label, Event::Reported { report_id })
            .await?;
        Ok(Flow::Continue)
    }

    pub(super) async fn on_reports_list(
        &mut self,
        label: Option<String>,
        scope: String,
        status: Option<ReportStatus>,
        cursor: Option<String>,
        actor: Actor,
    ) -> io::Result<Flow> {
        const PAGE: usize = 50;
        let Some(token_scope) = TokenScope::parse(&scope) else {
            return self.bad_scope(label).await;
        };
        match self
            .ctx
            .actor_has_cap(&actor, &Capability::Reports, &token_scope, unix_now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "reports").await,
            Err(e) => return self.internal(label, &e).await,
        }
        let page = match self
            .ctx
            .reports
            .list_reports(&scope, status, cursor.as_deref(), PAGE)
            .await
        {
            Ok(page) => page,
            Err(e) => return self.internal(label, &e).await,
        };
        let next_cursor = (page.len() == PAGE)
            .then(|| page.last().map(|r| r.id.clone()))
            .flatten();
        let is_net = scope == "*";
        for report in &page {
            self.send_event(
                label.clone(),
                Event::ReportFiled {
                    report_id: report.id.clone(),
                    msgid: report.msgid.clone(),
                    category: report.category.clone(),
                    state: report.state,
                    scope: if is_net {
                        ReportScope::Net
                    } else {
                        ReportScope::Ns
                    },
                    reporter: Some(report.reporter.to_string()),
                },
            )
            .await?;
        }
        if let Some(cursor) = next_cursor {
            self.send_event(label, Event::More { cursor }).await?;
        }
        Ok(Flow::Continue)
    }

    pub(super) async fn on_reports_resolve(
        &mut self,
        label: Option<String>,
        report_id: String,
        action: ResolveAction,
        note: Option<String>,
        actor: Actor,
    ) -> io::Result<Flow> {
        let report = match self.ctx.reports.report(&report_id).await {
            Ok(Some(report)) => report,
            Ok(None) => return self.no_such_target(label).await,
            Err(e) => return self.internal(label, &e).await,
        };
        // Invariant 4: authority before any state change. The resolver must
        // hold `reports` at one of the report's queue scopes.
        let now = unix_now();
        let mut authorized = false;
        for queue in &report.queue_scopes {
            let Some(qscope) = TokenScope::parse(queue) else {
                continue;
            };
            match self
                .ctx
                .actor_has_cap(&actor, &Capability::Reports, &qscope, now)
                .await
            {
                Ok(true) => {
                    authorized = true;
                    break;
                }
                Ok(false) => {}
                Err(e) => return self.internal(label, &e).await,
            }
        }
        if !authorized {
            return self.cap_required(label, "reports").await;
        }

        // ESCALATE re-routes an ns report up to net, leaving it open and its
        // holds intact (§6.7); it is not a resolution.
        if action == ResolveAction::Escalated {
            match self.ctx.reports.escalate_report(&report_id).await {
                Ok(true) => {}
                Ok(false) => return self.no_such_target(label).await,
                Err(e) => return self.internal(label, &e).await,
            }
            self.notify_queue_handlers(
                "*",
                Event::ReportFiled {
                    report_id: report.id.clone(),
                    msgid: report.msgid.clone(),
                    category: report.category.clone(),
                    state: report.state,
                    scope: ReportScope::Net,
                    reporter: Some(report.reporter.to_string()),
                },
            )
            .await;
            return self
                .send_event(
                    label,
                    Event::ReportResolved {
                        report_id,
                        action,
                        by: Some(actor.to_string()),
                        note,
                    },
                )
                .await
                .map(|_| Flow::Continue);
        }

        let now_ms = unix_now_ms();
        let resolution = ReportResolution {
            action,
            note: note.clone(),
            resolved_by: actor.to_string(),
            at_ms: now_ms,
            hold_release_at: now_ms + REPORT_HOLD_GRACE_MS,
        };
        match self
            .ctx
            .reports
            .resolve_report(&report_id, resolution)
            .await
        {
            Ok(true) => {}
            // Already resolved / gone — indistinct (anti-enumeration).
            Ok(false) => return self.no_such_target(label).await,
            Err(e) => return self.internal(label, &e).await,
        }

        // The reporter gets the MINIMAL form — no handler identity, no note
        // (§6.7 confidentiality; invariant 12 protects the reported party,
        // this clause protects the handler toward the reporter).
        self.ctx
            .directory
            .notify(
                report.reporter.clone(),
                Event::ReportResolved {
                    report_id: report_id.clone(),
                    action,
                    by: None,
                    note: None,
                },
            )
            .await;
        // The resolver's echo carries the full form.
        self.send_event(
            label,
            Event::ReportResolved {
                report_id,
                action,
                by: Some(actor.to_string()),
                note,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    /// `MUTE`/`UNMUTE`/`BAN`/`UNBAN` (§6.7): cap-check the moderator, record or
    /// clear the deny, eject on a fresh channel-scope ban, and echo `MODERATED`.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn on_moderate(
        &mut self,
        label: Option<String>,
        scope: String,
        target: Account,
        kind: ModKind,
        add: bool,
        reason: Option<String>,
        actor: Actor,
    ) -> io::Result<Flow> {
        let Some(tscope) = TokenScope::parse(&scope) else {
            return self.no_such_target(label).await;
        };
        let (cap, cap_str) = match kind {
            ModKind::Mute => (Capability::Mute, "mute"),
            ModKind::Ban => (Capability::Ban, "ban"),
        };
        match self
            .ctx
            .actor_has_cap(&actor, &cap, &tscope, unix_now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, cap_str).await,
            Err(e) => return self.internal(label, &e).await,
        }
        let outcome = if add {
            self.ctx
                .moderation
                .set_moderation(ModRecord {
                    scope: scope.clone(),
                    account: target.clone(),
                    kind,
                    actor: actor.to_string(),
                    reason: reason.clone(),
                    at_ms: unix_now_ms(),
                })
                .await
        } else {
            self.ctx
                .moderation
                .clear_moderation(&scope, &target, kind)
                .await
                .map(|_| ())
        };
        if let Err(e) = outcome {
            return self.internal(label, &e).await;
        }
        // A fresh channel-scope ban force-parts the target.
        if add && kind == ModKind::Ban {
            if let Ok(channel) = scope.parse::<ChannelName>() {
                if let Some(handle) = self.ctx.registry.get(&channel) {
                    handle.eject(target.clone()).await;
                }
            }
        }
        let action = match (kind, add) {
            (ModKind::Mute, true) => ModAction::Mute,
            (ModKind::Mute, false) => ModAction::Unmute,
            (ModKind::Ban, true) => ModAction::Ban,
            (ModKind::Ban, false) => ModAction::Unban,
        };
        self.send_event(
            label,
            Event::Moderated {
                scope,
                account: target,
                action,
                by: Some(actor.to_string()),
                reason,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    /// `KICK` (§6.7): cap-check `kick`, force-part the target (no persistent
    /// state — they may rejoin), echo `MODERATED`.
    pub(super) async fn on_kick(
        &mut self,
        label: Option<String>,
        channel: ChannelName,
        target: Account,
        reason: Option<String>,
        actor: Actor,
    ) -> io::Result<Flow> {
        let scope = TokenScope::Channel(channel.to_string());
        match self
            .ctx
            .actor_has_cap(&actor, &Capability::Kick, &scope, unix_now())
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.cap_required(label, "kick").await,
            Err(e) => return self.internal(label, &e).await,
        }
        let Some(handle) = self.ctx.registry.get(&channel) else {
            return self.no_such_target(label).await;
        };
        handle.eject(target.clone()).await;
        self.send_event(
            label,
            Event::Moderated {
                scope: channel.to_string(),
                account: target,
                action: ModAction::Kick,
                by: Some(actor.to_string()),
                reason,
            },
        )
        .await?;
        Ok(Flow::Continue)
    }
}
