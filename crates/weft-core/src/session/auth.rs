//! §6.1 account auth handlers: REGISTER + the authed WELCOME.

use super::*;

impl<S: ControlStream> Session<S> {
    /// §6.1 REGISTER: gated on config, password ≥ 12 B, unique name, and — when
    /// the network sets `require_email` — a valid, unused contact email (stored
    /// as a pending §10.5 claim: the account works immediately, verification is
    /// a later step). The email also powers password reset, so it must be unique
    /// whenever supplied. Success is also authentication (→ WELCOME → READY).
    pub(super) async fn on_register(
        &mut self,
        label: Option<String>,
        account: Account,
        email: Option<&str>,
        password: &str,
    ) -> io::Result<Flow> {
        if !self.ctx.registration_open {
            self.send_err(
                label,
                ErrCode::Forbidden,
                None,
                "registration is closed on this network",
            )
            .await?;
            return Ok(Flow::Continue);
        }
        if password.len() < 12 {
            self.send_err(
                label,
                ErrCode::Policy,
                None,
                "password must be at least 12 bytes",
            )
            .await?;
            return Ok(Flow::Continue);
        }
        // §6.7 banned-word filter: refuse a username containing a barred word.
        if self.ctx.name_is_banned(account.as_str()) {
            self.send_err(label, ErrCode::Policy, None, "that name isn't allowed")
                .await?;
            return Ok(Flow::Continue);
        }

        // §6.1 email policy. Gateways (WEFT-IRC) auto-register emailless and are
        // exempt; a native client on a `require_email` network must supply one.
        match email {
            Some(email) => {
                if !crate::session::verify::is_plausible_email(email) {
                    self.send_err(label, ErrCode::Malformed, None, "invalid email address")
                        .await?;
                    return Ok(Flow::Continue);
                }
                // Uniqueness: reset resolves an email → one account, so a reused
                // address is a CONFLICT (same code as a taken name).
                match self.ctx.accounts.account_by_email(email).await {
                    Ok(Some(_)) => {
                        self.send_err(
                            label,
                            ErrCode::Conflict,
                            None,
                            "that email is already registered",
                        )
                        .await?;
                        return Ok(Flow::Continue);
                    }
                    Ok(None) => {}
                    Err(e) => return self.internal(label, &e).await,
                }
            }
            None if self.ctx.require_email && !self.gateway => {
                self.send_err(
                    label,
                    ErrCode::Policy,
                    None,
                    "an email address is required to register on this network",
                )
                .await?;
                return Ok(Flow::Continue);
            }
            None => {}
        }

        match self.ctx.accounts.register(&account, password).await {
            Ok(crate::accounts::RegisterOutcome::Exists) => {
                self.send_err(label, ErrCode::Conflict, None, "account name is taken")
                    .await?;
                Ok(Flow::Continue)
            }
            Ok(crate::accounts::RegisterOutcome::Created) => {
                // Record the contact email as a pending claim (verify-later): the
                // account is usable now; VERIFY EMAIL confirms it whenever.
                if let Some(email) = email {
                    if let Err(e) = self
                        .ctx
                        .accounts
                        .upsert_verification(&account, "email", email)
                        .await
                    {
                        // The account exists; a failed claim write shouldn't undo
                        // it. Log and continue — the user can VERIFY EMAIL later.
                        error!(%account, "recording register email claim failed: {e}");
                    }
                }

                self.welcome_authed(label, account, None).await
            }
            Err(e) => self.internal(label, &e).await,
        }
    }

    /// Successful auth: WELCOME (with `attestation=` for key auth, §6.1)
    /// and the READY transition.
    pub(super) async fn welcome_authed(
        &mut self,
        label: Option<String>,
        account: Account,
        attestation: Option<String>,
    ) -> io::Result<Flow> {
        // WC7: a suspended account can't authenticate. Uniform AUTH-FAILED (it
        // looks exactly like bad credentials — anti-enumeration, §6.1). This is
        // the single chokepoint every AUTH method routes through.
        if self
            .ctx
            .accounts
            .is_suspended(&account)
            .await
            .unwrap_or(false)
        {
            return self.auth_failed(label).await;
        }
        // A **bot** never authenticates on a client session (owner directive
        // 2026-08-06): it acts through the provider that registered it, and
        // later through an API token. Same uniform failure — whether a handle
        // is a bot is not something an unauthenticated caller may probe.
        if self.ctx.accounts.is_bot(&account).await.unwrap_or(false) {
            return self.auth_failed(label).await;
        }
        let welcome = Event::Welcome {
            network: self.ctx.info.network.clone(),
            features: Vec::new(),
            attestation,
            motd: None,
        };
        self.send_event(label, welcome).await?;
        // §13 hand the client a per-session media fetch bearer (used on
        // `/media/<hash>?t=…` URLs; membership is re-checked per fetch).
        let media_token = self.ctx.mint_media_bearer(account.clone());
        self.send_event(None, Event::MediaToken { token: media_token })
            .await?;
        // Join the account directory (DM delivery, MARK sync)...
        self.ctx
            .directory
            .register(
                account.clone(),
                self.id,
                self.direct_tx.clone(),
                self.close.clone(),
            )
            .await;
        // ...and restore read state (§9.7: MARKED snapshot after auth).
        match self.ctx.accounts.marks(&account).await {
            Ok(marks) => {
                for (target, msgid) in marks {
                    if let Ok(channel) = target.parse::<ChannelName>() {
                        self.send_event(
                            None,
                            Event::Marked {
                                channel: channel.clone(),
                                msgid: msgid.clone(),
                            },
                        )
                        .await?;
                        // §6.3 unread snapshot: authoritative counts since the
                        // marker, so the client's badges survive reconnect.
                        if let Ok((unread, mentions)) = self
                            .ctx
                            .events
                            .unread_counts(&Scope::Channel(channel.clone()), &account, msgid.ulid())
                            .await
                        {
                            self.send_event(
                                None,
                                Event::UnreadCounts {
                                    channel,
                                    unread,
                                    mentions,
                                },
                            )
                            .await?;
                        }
                    }
                }
            }
            Err(e) => error!("marks snapshot failed: {e}"),
        }
        self.registered = Some(account.clone());
        // §6.3 restore the **derived** channel set (v0.12, Part 1.1) — top-level
        // memberships plus every visible, non-hidden channel of each namespace
        // the account belongs to. Channels + namespace tiles reappear without
        // re-joining. (Task 17's SYNC will replace this server-push with a
        // client-pulled skeleton; until then this keeps reconnect working.)
        for channel in self.derived_channels(&account).await {
            self.join_one(&channel, &account, None).await?;
        }
        self.state = State::Ready { account };
        Ok(Flow::Continue)
    }

    /// The single failure surface for every credential problem — unknown
    /// account, wrong password, bad proof, unknown device, missing
    /// challenge. One code, one text (§8: AUTH-FAILED is uniform).
    pub(super) async fn auth_failed(&mut self, label: Option<String>) -> io::Result<Flow> {
        self.send_err(label, ErrCode::AuthFailed, None, "authentication failed")
            .await?;
        Ok(Flow::Continue)
    }
}
