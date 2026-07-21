//! §6.1 account auth handlers: REGISTER + the authed WELCOME.

use super::*;

impl<S: ControlStream> Session<S> {
    /// §6.1 REGISTER: gated on config, password ≥ 12 B, unique name.
    /// Success is also authentication (→ WELCOME → READY).
    pub(super) async fn on_register(
        &mut self,
        label: Option<String>,
        account: Account,
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
        match self.ctx.accounts.register(&account, password).await {
            Ok(crate::accounts::RegisterOutcome::Exists) => {
                self.send_err(label, ErrCode::Conflict, None, "account name is taken")
                    .await?;
                Ok(Flow::Continue)
            }
            Ok(crate::accounts::RegisterOutcome::Created) => {
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
            .register(account.clone(), self.id, self.direct_tx.clone())
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
        // §6.3 restore persistent channel memberships — the client's channels
        // (and namespace tiles) reappear without re-joining.
        match self.ctx.memberships.memberships(&account).await {
            Ok(channels) => {
                for channel in channels {
                    self.join_one(&channel, &account, None).await?;
                }
            }
            Err(e) => error!("membership restore failed: {e}"),
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
