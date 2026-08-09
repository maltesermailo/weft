//! Plugin system (`docs/architecture/plugin-spec.md` §11–§12): the remote-plugin
//! (App Service) session + the client-facing action routing.
//!
//! **M-plug-2 scope:** a plugin authenticates (`AUTH ADAPTER`, §4.2), sends
//! `PLUGIN-REGISTER` (its actions), and weftd serves the catalog (`PLUGINS` →
//! `PLUGIN-MANIFEST`) and routes a client `PLUGIN INVOKE` to the plugin's session,
//! relaying its `PLUGIN-VIEW`/`-RESULT` back. Multi-step flows (`SUBMIT`/`ACTION`)
//! and hooks land in later milestones.

use super::*;

/// The reserved account owning every provider-managed namespace (Phase-0
/// decision, 2026-08-04). Registered suspended; confers no authority (the
/// origin gate strips the owner shortcut, `context.rs`).
const FOREIGN_SENTINEL: &str = "foreign";

/// Reduce a foreign path segment / display name to the namespace/vanity charset
/// (`[a-z0-9-_]{1,64}`), falling back to `"foreign"` when nothing survives.
fn sanitize_vanity(raw: &str) -> String {
    let mut out: String = raw
        .to_ascii_lowercase()
        .chars()
        .filter(|c| matches!(c, 'a'..='z' | '0'..='9' | '-' | '_'))
        .take(58) // leave room for a dedupe suffix
        .collect();

    if out.is_empty() {
        out = "foreign".to_string();
    }

    out
}

/// Build the wire event a provider's ingested command stands for (slice 5).
///
/// The provider mints the ids, so `MSG` and `EDIT` — the two verbs whose row is
/// keyed by its own id — carry `@msgid=<realm>/<ulid>`; `DELETE`/`REACT` name
/// only the root they act on and get a local bookkeeping id in `ingest_record`,
/// the same as on the peer-federation path. A missing or unparsable `@msgid`
/// drops the line rather than inventing one (invariant 2: we never mint for a
/// foreign origin).
fn provider_event(
    sender: UserRef,
    channel: ChannelName,
    cmd: Command,
    line: &Line,
) -> Option<Event> {
    let minted = || {
        line.tags
            .get("msgid")
            .and_then(|id| id.parse::<MsgId>().ok())
    };
    let target = Target::Channel(channel);

    match cmd {
        Command::Msg { body, meta, .. } => Some(Event::Message(Box::new(MessageEvent {
            target,
            sender,
            msgid: minted()?,
            body: body.unwrap_or_default(),
            meta,
            edited: None,
            edited_at: None,
        }))),
        Command::Edit { msgid, body } => Some(Event::Edited {
            target,
            user: sender,
            msgid: minted()?,
            edit_of: msgid,
            body,
        }),
        Command::Delete { msgid } => Some(Event::Deleted {
            target,
            msgid,
            by: Some(sender),
        }),
        Command::React { msgid, emoji } => Some(Event::Reaction {
            target,
            msgid,
            emoji,
            op: weft_proto::ReactionOp::Add,
            by: sender,
        }),
        Command::Unreact { msgid, emoji } => Some(Event::Reaction {
            target,
            msgid,
            emoji,
            op: weft_proto::ReactionOp::Remove,
            by: sender,
        }),
        _ => None,
    }
}

/// The §7a.3 capability profile carried on a namespace assertion — kept together
/// so it travels as one thing rather than two more positional arguments.
struct NsProfile {
    authority: Option<weft_proto::Authority>,
    settings_disabled: Vec<String>,
}

/// A view's `panel_key`, if it declared one (§11.3). Only panels have one —
/// a modal is addressed by its view-id alone.
fn panel_key_of(view: &str) -> Option<String> {
    weft_proto::plugin_from_b64::<weft_proto::View>(view)
        .ok()
        .and_then(|v| v.panel_key)
}

/// Serialize an event with an optional echoed label, for a cross-session relay.
fn relay_line(event: Event, label: Option<String>) -> Result<String, weft_proto::SerializeError> {
    match label {
        Some(label) => Reply::with_label(event, label).serialize(),
        None => Reply::new(event).serialize(),
    }
}

impl<S: ControlStream> Session<S> {
    /// §4.2 provider PROOF verified: enter the provider session bound to
    /// `plugin_id`, realm-unbound. The provider then registers (`PLUGIN-REGISTER`
    /// and/or `REALM REGISTER`); a bridge data connection binds a realm via
    /// `REALM ASSERT`.
    pub(super) async fn welcome_plugin_service(
        &mut self,
        label: Option<String>,
        key: PublicKey,
        plugin_id: String,
    ) -> io::Result<Flow> {
        self.send_event(
            label,
            Event::Welcome {
                network: self.ctx.info.network.clone(),
                features: vec!["plugin".to_string()],
                attestation: None,
                motd: None,
            },
        )
        .await?;

        self.state = State::PluginService {
            key,
            plugin_id,
            realm: None,
        };
        Ok(Flow::Continue)
    }

    /// §12.3/§18 route a line from a provider session. Two families share it:
    /// the **bridge verbs** (Commands: `REALM REGISTER/ASSERT/WITHDRAW`,
    /// `PROVISION-OK|ERR`) and the **plugin events** (`PLUGIN-REGISTER` +
    /// the `PLUGIN-VIEW`/`-PATCH`/`-RESULT` responses weftd relays to clients).
    pub(super) async fn on_plugin_service_line(
        &mut self,
        key: PublicKey,
        plugin_id: String,
        _realm: Option<ForeignUri>,
        line: &Line,
    ) -> io::Result<Flow> {
        // Slice 4: an `@as=<user@realm>` line is **ingestion** — the provider
        // replaying a foreign room's traffic (framework §3.1). Routed before the
        // bridge verbs since it is identified by the tag, not the verb.
        if let Some(as_tag) = line.tags.get("as").filter(|v| !v.is_empty()) {
            let Ok(sender) = as_tag.parse::<UserRef>() else {
                return self.unsupported(None, "@as is not a user@network").await;
            };
            return match Request::from_line(line) {
                Ok(req) => {
                    self.on_provider_acting(&key, sender, req.command, line)
                        .await
                }
                Err(_) => Ok(Flow::Continue), // tolerate noise
            };
        }

        // Bridge-verb family first (they are Commands, not Events).
        if let Ok(req) = Request::from_line(line) {
            match req.command {
                // §13 media upload grant. A provider has no account, so the
                // client path (cap check against `attach`) does not apply — its
                // authority is the pinned key that already lets it speak into
                // its channels, and a blob is worth less than the messages it
                // attaches to. Size/mime are still bounded, and the grant is
                // one-shot.
                Command::StreamOffer { mode, mime, bytes } => {
                    return self
                        .on_provider_stream_offer(req.label, &plugin_id, mode, mime, bytes)
                        .await;
                }
                Command::RealmRegister { scheme } => {
                    return self
                        .on_realm_register(req.label, key, plugin_id, scheme)
                        .await;
                }
                Command::RealmAssert { realm } => {
                    return self.on_realm_assert(req.label, key, plugin_id, realm).await;
                }
                Command::RealmWithdraw => return self.on_realm_withdraw().await,
                // Authority translation: the provider governs its namespaces, so
                // it may set capabilities in them (its power levels, mapped).
                // The ordinary handlers, with the ordinary authority check —
                // `Actor::Provider` is what makes that work.
                // Framework §7a: the adapter answering for a message we handed it.
                // Silence until the deadline is itself an answer — see
                // `ServerCtx::expired_deliveries`.
                Command::Delivered { msgid } => {
                    self.ctx.resolve_delivery(&msgid);
                    return Ok(Flow::Continue);
                }
                // Two shapes, because there are two things that can go undelivered
                // and only one of them has an id here:
                //
                // - **with a msgid** — a message *we* minted and handed over (the
                //   projection path). The author is told about a message that is
                //   already in their history, so it is marked rather than failed.
                // - **with only a `@label`** — a post we *relayed* into a replica,
                //   which we never minted (the realm is the home). Nothing is stored
                //   to mark, so the answer is an `ERR` on the poster's own label:
                //   their client fails the pending echo instead of shimmering until
                //   its deadline.
                Command::Undelivered { msgid, reason } => {
                    match (msgid, req.label.as_deref()) {
                        (Some(msgid), _) => {
                            if let Some((author, channel)) = self.ctx.resolve_delivery(&msgid) {
                                self.report_undelivered(author, channel, msgid, reason)
                                    .await;
                            }
                        }
                        (None, Some(label)) => self.fail_relayed_post(label, reason).await,
                        (None, None) => debug!("UNDELIVERED names neither a msgid nor a label"),
                    }
                    return Ok(Flow::Continue);
                }
                Command::Grant {
                    subject,
                    scope,
                    caps,
                    expiry,
                } => {
                    let actor = Actor::Provider(plugin_id.clone());
                    return self
                        .on_grant(req.label, subject, scope, caps, expiry, actor)
                        .await;
                }
                Command::Revoke {
                    subject,
                    scope,
                    caps,
                    epoch,
                } => {
                    let actor = Actor::Provider(plugin_id.clone());
                    return self
                        .on_revoke(req.label, subject, scope, caps, epoch, actor)
                        .await;
                }
                // Framework §7a.3: a provider whose foreign system really has
                // **roles** (Discord) mirrors them as WEFT roles, so it speaks
                // the ordinary ROLE verbs as `Actor::Provider` — the governing
                // authority of its own namespaces. A levels-based realm (Matrix)
                // uses bare GRANTs instead and advertises `authority=levels`.
                Command::RoleCreate {
                    scope,
                    color,
                    caps,
                    hoist,
                    pingable,
                    position,
                    name,
                } => {
                    let actor = Actor::Provider(plugin_id.clone());
                    return self
                        .on_role_create(
                            req.label, scope, color, caps, hoist, pingable, position, name, actor,
                        )
                        .await;
                }
                // Both resolve the role **id** to its name first, exactly as the
                // client dispatch does — the wire carries ids (v0.13), the
                // handlers key on names.
                Command::RoleAssign {
                    scope,
                    account,
                    role,
                } => {
                    let Some(name) = self.role_name(&role.to_string()).await else {
                        return self.no_such_target(req.label).await;
                    };
                    let actor = Actor::Provider(plugin_id.clone());

                    return self
                        .on_role_assign(req.label, scope, account, name, actor)
                        .await;
                }
                Command::RoleUnassign {
                    scope,
                    account,
                    role,
                } => {
                    let Some(name) = self.role_name(&role.to_string()).await else {
                        return self.no_such_target(req.label).await;
                    };
                    let actor = Actor::Provider(plugin_id.clone());

                    return self
                        .on_role_unassign(req.label, scope, account, name, actor)
                        .await;
                }
                Command::ProvisionOk { job } => return self.on_provision_result(job, true).await,
                Command::ProvisionErr { job } => {
                    return self.on_provision_result(job, false).await;
                }
                _ => {} // not a bridge verb — fall through to the event family
            }
        }

        let event = match Reply::from_line(line) {
            Ok(reply) => reply.event,
            Err(_) => return Ok(Flow::Continue), // tolerate noise on the session
        };

        match event {
            // Framework §7a.0a: a full-replace statement of the realm's state.
            // `SYNC START` opens it, `SYNC END` closes it — anyone not named
            // in between is no longer a member.
            Event::SyncStart => self.on_provider_sync(&key, true).await,
            Event::SyncEnd { .. } => self.on_provider_sync(&key, false).await,
            // Membership: the realm states who belongs to a namespace it governs.
            Event::NsMember {
                namespace,
                user,
                action,
                ..
            } => {
                self.on_provider_ns_member(&key, namespace, user, action)
                    .await
            }
            Event::PluginRegister { registration } => {
                self.on_plugin_register(&key, &plugin_id, &registration)
                    .await
            }
            // Framework §3.1: structure assertions — normal NS-META /
            // CHANNEL-LAYOUT verbs with origin-URI targets (capability 4).
            Event::NsMetaForeign {
                uri,
                id,
                authority,
                settings_disabled,
                visibility,
                title,
                description,
                icon,
            } => {
                self.on_ns_assert(
                    &key,
                    uri,
                    id,
                    NsProfile {
                        authority,
                        settings_disabled,
                    },
                    visibility,
                    title,
                    description,
                    icon,
                )
                .await
            }
            Event::ChannelLayoutForeign {
                uri,
                id,
                position,
                kind,
                vanity,
                category,
            } => {
                self.on_channel_assert(&key, uri, id, position, kind, vanity, category)
                    .await
            }
            // Terminal: relay to the parked client, then drop the pending.
            Event::PluginResult { view_id, result } => {
                if let Some((reply, label)) = self.ctx.complete_invoke(&view_id) {
                    if let Ok(line) = relay_line(Event::PluginResult { view_id, result }, label) {
                        let _ = reply.try_send(line);
                    }
                }
                Ok(Flow::Continue)
            }
            // Non-terminal: relay, keep the pending for the flow's next step.
            Event::PluginView { view_id, view } => {
                if let Some((reply, label)) = self.ctx.peek_invoke(&view_id) {
                    // §11.3: note the panel key while the view goes past, so a
                    // later SUBSCRIBE can register under it — the plugin patches
                    // by key, having no way to know each open copy's view-id.
                    self.ctx.note_panel_key(&view_id, panel_key_of(&view));

                    if let Ok(line) = relay_line(Event::PluginView { view_id, view }, label) {
                        let _ = reply.try_send(line);
                    }
                }
                Ok(Flow::Continue)
            }
            // §11.3 a patch is addressed by view-id **or** panel key, and reaches
            // only views someone is subscribed to — patching a panel the client
            // has closed is a no-op, not a delivery.
            Event::PluginPatch { view_id, patch } => {
                for (reply, _) in self.ctx.patch_targets(&view_id) {
                    let event = Event::PluginPatch {
                        view_id: view_id.clone(),
                        patch: patch.clone(),
                    };
                    // Unsolicited: a push carries no label (§12.4).
                    if let Ok(line) = relay_line(event, None) {
                        let _ = reply.try_send(line);
                    }
                }
                Ok(Flow::Continue)
            }
            _ => Ok(Flow::Continue),
        }
    }

    /// §12.3 a provider's self-description: decode + validate + register its
    /// actions and schemes (§18 capability 6). A failed registration **refuses
    /// the connection with a typed error** (spec §4.2) — a trusted,
    /// operator-installed provider must fail loudly at the handshake, never sit
    /// silently unregistered.
    async fn on_plugin_register(
        &mut self,
        key: &PublicKey,
        plugin_id: &str,
        registration: &str,
    ) -> io::Result<Flow> {
        let Ok(reg) = weft_proto::plugin_from_b64::<weft_proto::Registration>(registration) else {
            warn!(%plugin_id, "refusing provider: undecodable PLUGIN-REGISTER");
            self.send_err(
                None,
                ErrCode::Malformed,
                None,
                "undecodable PLUGIN-REGISTER",
            )
            .await?;
            return Ok(Flow::Close);
        };

        // The registration must self-identify as the authenticated provider (a
        // provider can only speak for its own id).
        if reg.id != plugin_id {
            warn!(%plugin_id, claimed = %reg.id, "refusing provider: id mismatch in PLUGIN-REGISTER");
            self.send_err(
                None,
                ErrCode::Forbidden,
                None,
                "registration id does not match the authenticated provider",
            )
            .await?;
            return Ok(Flow::Close);
        }

        // Every declared scheme must be authorized by the provider's pinned
        // config entry — an unauthorized one fails the whole registration
        // (declaring a scheme you don't own must never silently succeed).
        let mut schemes = Vec::new();

        for s in &reg.schemes {
            let Ok(scheme) = s.parse::<weft_proto::Scheme>() else {
                warn!(%plugin_id, scheme = %s, "refusing provider: malformed scheme");
                self.send_err(None, ErrCode::Malformed, None, "malformed scheme")
                    .await?;
                return Ok(Flow::Close);
            };

            if !self.ctx.scheme_authorized(key, &scheme) {
                warn!(%plugin_id, %scheme, "refusing provider: scheme not authorized");
                self.send_err(None, ErrCode::Forbidden, None, "scheme not authorized")
                    .await?;
                return Ok(Flow::Close);
            }

            // First registrant holds a scheme (3b-c) — a second claimant is a
            // deployment error, refused loudly rather than routed by luck.
            if self.ctx.scheme_held_by_other(&scheme, plugin_id) {
                warn!(%plugin_id, %scheme, "refusing provider: scheme already served");
                self.send_err(None, ErrCode::Conflict, None, "scheme already served")
                    .await?;
                return Ok(Flow::Close);
            }

            schemes.push(scheme);
        }

        // The provider's own WEFT identity, at its request. Provisioned
        // **suspended** — like the §6.7 support account, it exists to be
        // attributed, never to authenticate — and idempotent across reconnects.
        let bot = match reg.bot.as_deref().map(str::parse::<Account>) {
            Some(Ok(bot)) => match self.ctx.accounts.provision_bot(&bot).await {
                Ok(()) => {
                    info!(%plugin_id, %bot, "provider bot account provisioned");
                    Some(bot)
                }
                Err(e) => {
                    warn!(%plugin_id, %bot, "could not provision the bot account: {e}");
                    None
                }
            },
            Some(Err(_)) => {
                warn!(%plugin_id, "invalid bot handle in PLUGIN-REGISTER — ignored");
                None
            }
            None => None,
        };

        self.ctx.register_plugin(
            plugin_id.to_string(),
            crate::context::ProviderRegistration {
                bot,
                out: self.fed_out_tx.clone(),
                name: reg.name,
                icon: reg.icon,
                actions: reg.actions,
                events: self.events_tx.clone(),
            },
            schemes.clone(),
        );
        info!(%plugin_id, "provider registered");

        // Its virtual namespaces just came online — tell their members, and
        // start relaying their channels' local traffic outward (slice 5).
        if !schemes.is_empty() {
            self.push_provider_state(&schemes).await;
            self.sync_provider_forwarders(&schemes).await;
            self.push_projected_structure(&schemes).await?;
        }
        Ok(Flow::Continue)
    }

    /// §3.3 REALM REGISTER: the provider declares a scheme it provisions. The
    /// proven key must be authorized for that scheme; the scheme lands in the
    /// unified provider registry (`NS JOIN <scheme>://…` routes here).
    async fn on_realm_register(
        &mut self,
        label: Option<String>,
        key: PublicKey,
        plugin_id: String,
        scheme: Scheme,
    ) -> io::Result<Flow> {
        if !self.ctx.scheme_authorized(&key, &scheme) {
            return self
                .unsupported(label, "provider key not pinned for that scheme")
                .await;
        }
        if self.ctx.scheme_held_by_other(&scheme, &plugin_id) {
            return self
                .send_err(label, ErrCode::Conflict, None, "scheme already served")
                .await
                .map(|_| Flow::Continue);
        }

        self.ctx.add_provider_scheme(
            &plugin_id,
            scheme.clone(),
            None,
            self.fed_out_tx.clone(),
            self.events_tx.clone(),
        );
        info!(%plugin_id, %scheme, "provider registered scheme");
        self.push_provider_state(std::slice::from_ref(&scheme))
            .await;
        self.sync_provider_forwarders(std::slice::from_ref(&scheme))
            .await;
        self.push_projected_structure(std::slice::from_ref(&scheme))
            .await?;
        self.push_consumed_membership(&[scheme]).await?;
        Ok(Flow::Continue)
    }

    /// Framework §3.3 / capability 4: a provider asserts a **virtual namespace**
    /// (`NS-META <origin-uri> <visibility>`). weftd mints the replica id
    /// (invariant 2), owner = the suspended sentinel (Phase-0 decision — local
    /// owner authority is origin-gated away), and answers with the minted
    /// `NS-META` (id form, `origin=`) so the provider learns its mapping.
    /// Re-asserting an existing origin re-sends the mapping (structural *update*
    /// sync is a later slice).
    #[allow(clippy::too_many_arguments)] // the fields of one asserted namespace
    async fn on_ns_assert(
        &mut self,
        key: &PublicKey,
        uri: ForeignUri,
        id: weft_proto::NamespaceId,
        profile: NsProfile,
        visibility: weft_proto::Visibility,
        title: Option<String>,
        description: Option<String>,
        icon: Option<String>,
    ) -> io::Result<Flow> {
        // A provider may only assert under schemes its pin authorizes.
        if !self.ctx.scheme_authorized(key, uri.scheme()) {
            return self
                .unsupported(None, "provider key not pinned for that scheme")
                .await;
        }

        // Idempotent re-assert: the namespace exists → re-send the mapping.
        match self
            .ctx
            .namespaces
            .namespace_by_origin(&uri.to_string())
            .await
        {
            Ok(Some(mut record)) => {
                // §7a.0e: re-asserting is how a realm **updates** a namespace it
                // governs — local edits are refused precisely so this is the one
                // path, and a realm that renames a space upstream must be able to
                // say so. Absent fields clear, so the assertion is the whole
                // truth rather than a patch.
                for (key, value) in [
                    ("title", title.as_deref()),
                    ("description", description.as_deref()),
                    ("icon", icon.as_deref()),
                ] {
                    let value = value.unwrap_or_default();
                    if let Err(e) = self
                        .ctx
                        .namespaces
                        .set_namespace_meta(&record.name, key, value)
                        .await
                    {
                        return self.internal(None, &e).await;
                    }
                }
                let visibility = match visibility {
                    weft_proto::Visibility::Public => "public",
                    _ => "unlisted", // `private` is unreachable-by-design here
                };
                if let Err(e) = self
                    .ctx
                    .namespaces
                    .set_namespace_visibility(&record.name, visibility)
                    .await
                {
                    return self.internal(None, &e).await;
                }

                record.title = title;
                record.description = description;
                record.icon = icon;
                record.visibility = visibility.to_string();
                record.authority = profile.authority.map(|a| a.to_string());
                record.settings_disabled = profile.settings_disabled;

                self.send_event(None, self.ns_meta_event(&record)).await?;
                return Ok(Flow::Continue);
            }
            Ok(None) => {}
            Err(e) => return self.internal(None, &e).await,
        }

        let Some(owner) = self.ensure_sentinel().await else {
            return self.internal(None, &"sentinel account unavailable").await;
        };

        // Vanity: the URI's last segment, sanitized to the name charset, deduped
        // with a short suffix on conflict (names are per-network unique).
        let base = sanitize_vanity(uri.path().last().map(String::as_str).unwrap_or("foreign"));
        // §7a.0d: the realm minted the id; we pin it, as federation pins a peer's.
        // An id already taken by anything that is not this realm's own replica is
        // a takeover attempt (or a genuine collision) — refuse rather than adopt.
        let ns_id = id.to_string();
        if matches!(
            self.ctx.namespaces.namespace_by_id(&ns_id).await,
            Ok(Some(_))
        ) {
            return self
                .send_err(None, ErrCode::Conflict, Some("id"), "namespace id in use")
                .await
                .map(|_| Flow::Continue);
        }
        let mut record = weft_store::NamespaceRecord {
            id: ns_id,
            // §7a.3: the realm says how its authority should be rendered and
            // which native settings surfaces to hide.
            authority: profile.authority.map(|a| a.to_string()),
            settings_disabled: profile.settings_disabled,
            // A replica cannot itself be projected (it IS a projection).
            bridges: Vec::new(),
            name: base.parse().expect("sanitized vanity is a valid name"),
            owner,
            root_key: String::new(), // never transferable/recoverable locally
            visibility: match visibility {
                weft_proto::Visibility::Public => "public".to_string(),
                // `private` would be unreachable-by-design; clamp to unlisted.
                _ => "unlisted".to_string(),
            },
            title,
            description,
            icon,
            recovery_set: None,
            pending_recovery: None,
            categories: Vec::new(),
            federation: false,
            frozen: false,
            welcome_channel: None,
            origin: Some(uri.to_string()),
        };

        for attempt in 0..3u8 {
            match self.ctx.namespaces.create_namespace(record.clone()).await {
                Ok(true) => {
                    info!(ns = %record.id, origin = %uri, "virtual namespace materialized");
                    self.send_event(None, self.ns_meta_event(&record)).await?;
                    return Ok(Flow::Continue);
                }
                Ok(false) => {
                    // Vanity taken — retry with a short random suffix.
                    let suffix = &weft_proto::Ulid::new().to_string().to_ascii_lowercase()
                        [26 - 4 - attempt as usize..];
                    record.name = format!("{base}-{suffix}")
                        .parse()
                        .expect("suffixed vanity is a valid name");
                }
                Err(e) => return self.internal(None, &e).await,
            }
        }

        self.internal(None, &"could not allocate a namespace name")
            .await
    }

    /// Framework §3.1: a provider asserts one channel of a virtual namespace
    /// (`CHANNEL-LAYOUT <ns-origin>/<segment> <position>`). The parent namespace
    /// (the URI minus its last segment) must have been asserted first; weftd
    /// mints the channel id and answers with the canonical `CHANNEL-LAYOUT`
    /// (`#<ns-id>/<chan-id>`, `origin=`) — the provider's mapping row.
    #[allow(clippy::too_many_arguments)] // the fields of one asserted channel
    async fn on_channel_assert(
        &mut self,
        key: &PublicKey,
        uri: ForeignUri,
        id: weft_proto::ChannelId,
        position: i64,
        kind: weft_proto::ChannelKind,
        vanity: String,
        category: Option<String>,
    ) -> io::Result<Flow> {
        if !self.ctx.scheme_authorized(key, uri.scheme()) {
            return self
                .unsupported(None, "provider key not pinned for that scheme")
                .await;
        }

        // The parent namespace = the URI minus its last segment.
        let Some(segment) = uri.path().last().cloned() else {
            return self
                .unsupported(None, "a channel origin needs a path segment")
                .await;
        };
        let parent = {
            let full = uri.to_string();
            full[..full.len() - segment.len() - 1].to_string()
        };
        let record = match self.ctx.namespaces.namespace_by_origin(&parent).await {
            Ok(Some(record)) => record,
            Ok(None) => {
                debug!(%uri, "channel assertion before its namespace — dropped");
                return self
                    .unsupported(None, "assert the namespace before its channels")
                    .await;
            }
            Err(e) => return self.internal(None, &e).await,
        };

        // Idempotent re-assert: the channel exists (by origin) → re-send its row.
        let existing = self
            .ctx
            .channel_store
            .channels_in_namespace(&record.id)
            .await
            .ok()
            .and_then(|list| {
                list.into_iter()
                    .find(|(_, rec)| rec.origin.as_deref() == Some(uri.to_string().as_str()))
            });

        if let Some((channel, rec)) = existing {
            // A re-assert is how the realm *restates* a room, so it can carry a new
            // display name, category or position — and the row we hold may predate
            // it. Adopt the change and tell the **members**, not just the provider
            // that asked: the assert used to be answered on this session alone, so a
            // reconnecting adapter corrected weftd's store while every connected
            // client kept the name it had cached (a bare ULID, until the user
            // restarted the client).
            let asserted_vanity = match vanity.is_empty() {
                true => rec.vanity.clone(),
                false => sanitize_vanity(&vanity),
            };
            let changed = asserted_vanity != rec.vanity
                || category != rec.category
                || position != rec.position;

            if changed {
                if let Err(e) = self
                    .ctx
                    .channel_store
                    .upsert_channel(&channel, &asserted_vanity, rec.policy, kind)
                    .await
                {
                    return self.internal(None, &e).await;
                }
                let _ = self
                    .ctx
                    .channel_store
                    .set_channel_layout(&channel, category.as_deref(), position)
                    .await;
            }

            let layout = Event::ChannelLayout {
                channel: channel.clone(),
                category: category.clone().or(rec.category),
                position,
                kind,
                vanity: asserted_vanity,
                origin: Some(uri),
            };

            if changed {
                if let Some(handle) = self.ctx.registry.get(&channel) {
                    handle.announce(layout.clone()).await;
                }
            }

            self.send_event(None, layout).await?;
            return Ok(Flow::Continue);
        }

        let vanity = if vanity.is_empty() {
            sanitize_vanity(&segment)
        } else {
            sanitize_vanity(&vanity)
        };
        let policy: RetentionPolicy = "retained:90d".parse().expect("valid default");
        // §7a.0d: the realm minted the id, we pin it. `registry.create` returning
        // `None` now means the id is already taken — a takeover attempt or a
        // genuine collision, either way refused rather than adopted.
        let chan_id = id.to_string().to_ascii_lowercase();
        let canonical: ChannelName = format!("#{}/{chan_id}", record.id)
            .parse()
            .expect("a canonical channel name from two ULIDs is valid");

        if self
            .ctx
            .registry
            .create(canonical.clone(), policy)
            .is_none()
        {
            return self
                .send_err(None, ErrCode::Conflict, Some("id"), "channel id in use")
                .await
                .map(|_| Flow::Continue);
        }
        if let Err(e) = self
            .ctx
            .channel_store
            .upsert_channel(&canonical, &vanity, policy, kind)
            .await
        {
            return self.internal(None, &e).await;
        }
        if let Err(e) = self
            .ctx
            .channel_store
            .set_channel_origin(&canonical, &uri.to_string())
            .await
        {
            return self.internal(None, &e).await;
        }
        if category.is_some() || position != 0 {
            let _ = self
                .ctx
                .channel_store
                .set_channel_layout(&canonical, category.as_deref(), position)
                .await;
        }

        info!(channel = %canonical, origin = %uri, "virtual channel materialized");

        // Slice 5: relay this fresh channel's local traffic outward too — it was
        // created after the provider's scheme registration, so the initial
        // forwarder sweep couldn't have seen it.
        if let Some(handle) = self.ctx.registry.get(&canonical) {
            if let Some(rx) = handle.subscribe().await {
                let forwarder = spawn_forwarder(canonical.clone(), rx, self.events_tx.clone());
                self.bridged.insert(canonical.clone(), forwarder);
            }
        }
        self.send_event(
            None,
            Event::ChannelLayout {
                channel: canonical,
                category,
                position,
                kind,
                vanity,
                origin: Some(uri),
            },
        )
        .await?;
        Ok(Flow::Continue)
    }

    /// **Membership, inbound — the full-replace window** (framework §7a.0a).
    ///
    /// A realm resyncs by re-stating, using the **same snapshot framing a client
    /// gets on login** (§6.9): `SYNC START` opens the statement, the assertions
    /// and `NS-MEMBER` events in between *are* the state, `SYNC END` closes it —
    /// at which point every **foreign** member of the provider's namespaces it
    /// did not name is dropped. Only the roles are swapped: here the realm is
    /// the one holding the state and weftd is the one conforming.
    ///
    /// The window is scoped to foreign members because that is what a realm can
    /// enumerate — see the prune below.
    ///
    /// Stating the whole set beats diffing against what we believe: the adapter
    /// already has it (a Matrix adapter reads room state), there is no
    /// read-modify-write across the link and so no stale-read race, and replaying
    /// it is idempotent. It is the shape federation already uses for a manifest.
    ///
    /// The `SYNC START` opener is what makes it safe: without it a stray
    /// `SYNC END` would name nobody and delete everyone, so an unopened one is
    /// ignored.
    async fn on_provider_sync(&mut self, key: &PublicKey, begin: bool) -> io::Result<Flow> {
        // The namespaces this provider speaks for — the scope of the statement.
        let governed = self.provider_namespaces(key).await;

        if begin {
            self.ns_replace = Some(NsReplace {
                namespaces: governed,
                named: std::collections::HashSet::new(),
            });
            return Ok(Flow::Continue);
        }

        let Some(statement) = self.ns_replace.take() else {
            debug!("SYNC END without a SYNC START — ignored (it would name nobody)");
            return Ok(Flow::Continue);
        };

        for ns in statement.namespaces {
            let current = self
                .ctx
                .memberships
                .ns_members(&ns)
                .await
                .unwrap_or_default();

            for member in current {
                if statement.named.contains(&(ns.clone(), member.clone())) {
                    continue;
                }
                // A full-replace may only prune inside the set its author can
                // enumerate, and a realm can enumerate its **own** users — it
                // reads foreign room state. Our local accounts are not in that
                // state: they are represented foreign-side by puppets, which an
                // adapter filters out of the roster precisely because their
                // traffic is a relay of ours. So an adapter re-stating a space
                // names the foreign members and *cannot* name the local ones,
                // and pruning what it never had a way to mention turned every
                // reconnect into a silent mass-part of every local member of
                // every bridged namespace.
                //
                // Local membership stays governed: it only becomes true on the
                // realm's `NS-MEMBER … join` in the first place, and the realm
                // can still revoke it by naming the part explicitly. What it
                // can no longer do is revoke it by omission.
                if weft_store::local_member(&member).is_some() {
                    continue;
                }
                if let Err(e) = self.ctx.memberships.clear_ns_membership(&member, &ns).await {
                    error!("full-replace membership prune failed: {e}");
                    continue;
                }

                if let Some(user) = self.member_userref(&member) {
                    self.announce_ns_member(&ns, user, MemberAction::Part).await;
                }
            }
        }

        Ok(Flow::Continue)
    }

    /// The namespace ids a provider governs — every `origin`-marked namespace
    /// whose scheme its key is pinned for.
    async fn provider_namespaces(&self, key: &PublicKey) -> Vec<String> {
        self.ctx
            .namespaces
            .namespaces_with_origin()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|record| {
                record
                    .origin
                    .as_deref()
                    .and_then(|o| o.parse::<ForeignUri>().ok())
                    .is_some_and(|uri| self.ctx.scheme_authorized(key, uri.scheme()))
            })
            .map(|record| record.id)
            .collect()
    }

    /// **Membership, inbound**: the realm states who is a member of a namespace
    /// it governs (`NS-MEMBER <ns> <user> join|part`).
    ///
    /// This is the authoritative direction. A bridge behaves as a federation
    /// peer: weftd *asks* by relaying `NS JOIN`/`NS LEAVE`
    /// ([`Self::relay_ns_membership`]) and the realm *answers* with this event —
    /// for its own users and for ours alike, once the foreign side has them.
    /// weftd never asserts membership of a foreign space itself.
    ///
    /// The membership row is keyed by the member key (`user@realm` for a foreign
    /// member, the bare account for a local one), so it survives restarts and
    /// feeds every derived roster and member count.
    async fn on_provider_ns_member(
        &mut self,
        key: &PublicKey,
        namespace: weft_proto::NamespaceId,
        user: UserRef,
        action: MemberAction,
    ) -> io::Result<Flow> {
        let ns = namespace.to_string();
        let record = match self.ctx.namespaces.namespace_by_id(&ns).await {
            Ok(Some(record)) => record,
            Ok(None) => return Ok(Flow::Continue),
            Err(e) => return self.internal(None, &e).await,
        };
        match record
            .origin
            .as_deref()
            .and_then(|o| o.parse::<ForeignUri>().ok())
        {
            // A replica: the realm is the membership authority outright (§6).
            Some(uri) => {
                if !self.ctx.scheme_authorized(key, uri.scheme()) {
                    return self
                        .unsupported(None, "provider key not pinned for that scheme")
                        .await;
                }
            }
            // Native: only a **projected** namespace accepts statements, and
            // only for **foreign** members — the §8 mapping of Matrix users
            // joining the projected rooms. Local users join natively; a
            // provider stating a local membership here would be forging an
            // action weftd itself owns.
            None => {
                let projected = record.bridges.iter().any(|b| {
                    b.parse::<Scheme>()
                        .is_ok_and(|b| self.ctx.scheme_authorized(key, &b))
                });

                if !projected {
                    debug!(%ns, "NS-MEMBER for an unprojected native namespace — refused");
                    return self
                        .unsupported(None, "not a provider-managed namespace")
                        .await;
                }
                if user.network == self.ctx.info.network {
                    return self
                        .unsupported(None, "locals join natively — statement refused")
                        .await;
                }
            }
        }

        // Our own users are keyed by their bare account, a foreign member by the
        // full `user@realm` handle (`weft_store::local_member` is the inverse).
        let member = if user.network == self.ctx.info.network {
            user.account.to_string()
        } else {
            user.to_string()
        };
        // Inside a full-replace window, note who was named — anyone absent at
        // `SYNC END` is dropped.
        if let Some(statement) = &mut self.ns_replace {
            if statement.namespaces.contains(&ns) {
                statement.named.insert((ns.clone(), member.clone()));
            }
        }

        let joining = matches!(action, MemberAction::Join);
        let wrote = if joining {
            self.ctx
                .memberships
                .set_ns_membership(&member, &ns, unix_now() as i64)
                .await
        } else {
            self.ctx.memberships.clear_ns_membership(&member, &ns).await
        };
        if let Err(e) = wrote {
            error!("provider membership write failed: {e}");
            return Ok(Flow::Continue);
        }

        self.announce_ns_member(&ns, user, action).await;

        // A local account whose membership just became true may be connected
        // *right now* — this statement arrived on our session, not theirs, and
        // subscriptions live on theirs. Nudge it to re-derive, or the row exists
        // while nothing is joined and its next HISTORY answers CAP-REQUIRED.
        if joining {
            if let Some(account) = weft_store::local_member(&member) {
                let channels = self
                    .ctx
                    .channel_store
                    .channels_in_namespace(&ns)
                    .await
                    .unwrap_or_default();

                self.ctx
                    .directory
                    .attach(
                        account,
                        channels.into_iter().map(|(name, _)| name).collect(),
                    )
                    .await;
            }
        }

        Ok(Flow::Continue)
    }

    /// Tell an author their message never reached the realm.
    ///
    /// Delivered to the account, not one session, so it lands wherever they have
    /// the client open — including a different device from the one that posted.
    pub(super) async fn report_undelivered(
        &self,
        author: Account,
        channel: ChannelName,
        msgid: MsgId,
        reason: Option<String>,
    ) {
        warn!(%msgid, %channel, "message not delivered to the realm: {reason:?}");

        self.ctx
            .directory
            .notify(
                author,
                Event::Undelivered {
                    channel,
                    msgid,
                    reason,
                },
            )
            .await;
    }

    /// Tell **local** members that a namespace's roster changed, so live rosters
    /// and counts follow. Channels are not joinable, so this is a roster notice
    /// per channel of the namespace — local delivery only, nothing is asserted
    /// outward.
    async fn announce_ns_member(&self, ns: &str, user: UserRef, action: MemberAction) {
        let count = self
            .ctx
            .memberships
            .ns_members(ns)
            .await
            .map(|m| m.len() as u64)
            .ok();
        let channels = self
            .ctx
            .channel_store
            .channels_in_namespace(ns)
            .await
            .unwrap_or_default();

        for (channel, _) in channels {
            if let Some(handle) = self.ctx.registry.get(&channel) {
                handle
                    .announce(Event::Member {
                        channel: channel.clone(),
                        user: user.clone(),
                        action,
                        display: None,
                        count,
                    })
                    .await;
            }
        }
    }

    /// A stored member key back to the user it names: a bare account is one of
    /// ours, anything else is the foreign `user@realm` handle.
    /// The wire target naming a DM peer: bare `@ada` on our own network, the
    /// qualified `@alice@matrix.org` otherwise (§9.5 + framework §7a.0).
    pub(super) fn dm_target(&self, peer: &UserRef) -> Target {
        Target::User {
            account: peer.account.clone(),
            network: (peer.network != self.ctx.info.network).then(|| peer.network.clone()),
        }
    }

    pub(super) fn member_userref(&self, member: &str) -> Option<UserRef> {
        match weft_store::local_member(member) {
            Some(account) => Some(UserRef::new(account, self.ctx.info.network.clone())),
            None => member.parse().ok(),
        }
    }

    /// A line the provider sends **on behalf of** one of its users (`@as`). Two
    /// families: the message plane, which is *ingestion* (the foreign room's
    /// traffic replayed here), and **moderation**, which is a foreign moderator
    /// exercising authority.
    ///
    /// Moderation goes through the ordinary actor-aware handler as
    /// `Actor::Foreign`, so it is enforced against the very grants the provider
    /// issued (see [`Self::on_provider_grant`]) — a foreign user with no grant is
    /// refused exactly like a local one. That is what makes "a Matrix moderator
    /// is a moderator here" real rather than decorative, and it needs no
    /// provider-specific authority path.
    async fn on_provider_acting(
        &mut self,
        key: &PublicKey,
        sender: UserRef,
        cmd: Command,
        line: &Line,
    ) -> io::Result<Flow> {
        let actor = Actor::Foreign(sender.to_string());
        // §3.5: an attributed act may carry a label, and weftd echoes it on the
        // direct response — including `ERR`. That is what makes §10's *revert*
        // possible: a refused act is correlated back to the foreign-side state
        // change the adapter must undo. Absent ⇒ fire-and-forget, as before.
        let label = line.tags.get("label").cloned();

        match cmd {
            Command::Mute {
                scope,
                account: target,
                reason,
            } => {
                self.on_moderate(label, scope, target, ModKind::Mute, true, reason, actor)
                    .await
            }
            Command::Unmute {
                scope,
                account: target,
            } => {
                self.on_moderate(label, scope, target, ModKind::Mute, false, None, actor)
                    .await
            }
            Command::Ban {
                scope,
                account: target,
                reason,
            } => {
                self.on_moderate(label, scope, target, ModKind::Ban, true, reason, actor)
                    .await
            }
            Command::Unban {
                scope,
                account: target,
            } => {
                self.on_moderate(label, scope, target, ModKind::Ban, false, None, actor)
                    .await
            }
            Command::Kick {
                channel,
                account: target,
                reason,
            } => self.on_kick(label, channel, target, reason, actor).await,

            // §15 ephemera: a foreign user is typing in a replica. Never
            // stored, so it takes the announce seam rather than the ingest
            // path — and it needs `@as` because the wire's `TYPING` names no
            // user (a client's own session identifies them).
            Command::Typing { channel, state } => {
                self.on_provider_typing(key, sender, channel, state).await
            }
            // §6.1 the other ephemeron: one of the realm's users changed status.
            // Global per user in every system that has it (Matrix included), so
            // unlike TYPING it names no channel — weftd fans it out to the replica
            // channels that user is actually in, exactly as it does for a local
            // session's own `PRESENCE`.
            Command::Presence { status } => self.on_provider_presence(key, sender, status).await,
            // §10 (matrix.md): a foreign moderator's PL change arrives as an
            // attributed GRANT/REVOKE and succeeds **iff WEFT granted that
            // user** `grant:<cap>` — the ordinary handlers with the ordinary
            // authority check. No side-channel authority: being a Matrix admin
            // confers exactly what some WEFT grant gave their account.
            Command::Grant {
                subject,
                scope,
                caps,
                expiry,
            } => {
                self.on_grant(label, subject, scope, caps, expiry, actor)
                    .await
            }
            Command::Revoke {
                subject,
                scope,
                caps,
                epoch,
            } => {
                self.on_revoke(label, subject, scope, caps, epoch, actor)
                    .await
            }

            cmd => self.on_provider_ingest(key, sender, cmd, line).await,
        }
    }

    /// Slice 4 — **provider ingestion** (framework §3.1): the provider replays a
    /// foreign room's traffic as ordinary verbs with `@as=<user@realm>`,
    /// addressing the replica by the **canonical channel name it learned** from
    /// the `CHANNEL-LAYOUT` mapping reply (§3.3) — so this is an ordinary `MSG`,
    /// no URI-target parsing needed.
    ///
    /// **A realm is a network.** The provider names its users by their WEFT
    /// handle on the realm (`alice=bob@matrix.org`) and mints their msgids under
    /// it (`@msgid=matrix.org/<ulid>`), so a replica is indistinguishable from a
    /// federated peer's channel: the adapter owns the foreign→WEFT mapping (only
    /// it knows its escaping rules), and weftd ingests exactly as it does for a
    /// peer network — same `ingest_record`, same origin authority (invariant 2),
    /// same one-hop forwarding.
    ///
    /// **Authority:** the target channel must be an `origin`-marked replica whose
    /// scheme this provider's key is pinned for — a provider can only speak into
    /// rooms it owns. `@msgid` must name that channel's realm; `@as` must be
    /// **foreign** (any non-local, non-peer network — rooms are cross-realm),
    /// with one bounded exception for the §8 return path. Both amendments
    /// 2026-08-05; the gates below carry the details.
    async fn on_provider_ingest(
        &mut self,
        key: &PublicKey,
        sender: UserRef,
        cmd: Command,
        line: &Line,
    ) -> io::Result<Flow> {
        // Every ingestable verb names its channel differently: MSG by target,
        // the mutations by the msgid's channel.
        // A DM has no channel: it is a 1:1 conversation with one of our users,
        // stored in the ordinary `Scope::Dm` keyed by the sender's member key.
        if let Command::Msg {
            target: Target::User { account, network },
            body,
            meta,
        } = &cmd
        {
            if network
                .as_ref()
                .map_or(true, |net| *net == self.ctx.info.network)
            {
                let Some(msgid) = line
                    .tags
                    .get("msgid")
                    .and_then(|id| id.parse::<MsgId>().ok())
                else {
                    debug!("provider DM without a minted msgid — dropped");
                    return Ok(Flow::Continue);
                };
                if msgid.origin().as_str() != sender.network.as_str() {
                    debug!("provider DM minted outside its realm — dropped");
                    return Ok(Flow::Continue);
                }

                return self
                    .on_provider_dm(&sender, account.clone(), msgid, body.clone(), meta.clone())
                    .await;
            }

            debug!("provider ingest of a DM to another network — dropped");
            return Ok(Flow::Continue);
        }

        let channel = match &cmd {
            Command::Msg {
                target: Target::Channel(channel),
                ..
            } => channel.clone(),
            Command::Edit { msgid, .. }
            | Command::Delete { msgid }
            | Command::React { msgid, .. }
            | Command::Unreact { msgid, .. } => match self.channel_of_msgid(msgid).await {
                Some(channel) => channel,
                None => {
                    debug!("provider ingest for an unknown msgid — dropped");
                    return Ok(Flow::Continue);
                }
            },
            _ => {
                debug!("unsupported provider-ingest verb — dropped");
                return Ok(Flow::Continue);
            }
        };

        // The channel must be a replica this provider may speak for.
        let record = match self.ctx.channel_store.channel(&channel).await {
            Ok(Some(record)) => record,
            _ => {
                debug!(%channel, "provider ingest for an unknown channel — dropped");
                return Ok(Flow::Continue);
            }
        };
        let origin = record
            .origin
            .as_deref()
            .and_then(|o| o.parse::<ForeignUri>().ok());
        let Some(origin) = origin else {
            // Not a replica: a **native** channel accepts foreign traffic only
            // through the outbound-projection door (matrix.md §17.1).
            return self
                .on_projected_ingest(key, sender, cmd, line, channel, record)
                .await;
        };
        if !self.ctx.scheme_authorized(key, origin.scheme()) {
            return self
                .unsupported(None, "provider key not pinned for that scheme")
                .await;
        }

        let Some(handle) = self.ctx.registry.get(&channel) else {
            debug!(%channel, "provider ingest for a channel with no live actor — dropped");
            return Ok(Flow::Continue);
        };

        // A bridge label is resolved up front because it serves twice below: it
        // authorizes a local-sender `MSG` (the realm's answer to a post we
        // relayed) and it routes the copy to the session waiting on that label.
        let relay_echo = line
            .tags
            .get("label")
            .and_then(|l| self.ctx.take_group_echo(l));

        // The sender must be **foreign** — not ours, and not a peer network's
        // (owner decision 2026-08-05, amending the protocol doc's §5). It need
        // not live on the bound realm itself: foreign systems are cross-realm —
        // a Matrix room homed on matrix.org has members from kde.org — and the
        // trust root here is the provider's pinned key + the channel's scheme,
        // not the sender's domain. Forgery protection keeps its teeth: a local
        // account or a real WEFT peer's user still cannot be attributed, since
        // those identities are anchored by *our* auth and *their* signing keys
        // respectively, never by a bridge.
        let Ok(realm) = origin.realm().parse::<NetworkName>() else {
            debug!(realm = origin.realm(), "realm is not a usable network name");
            return self.unsupported(None, "realm is not a network name").await;
        };
        if sender.network == self.ctx.info.network {
            // A provider's **own bot** is its WEFT identity, provisioned at its
            // request and login-disabled — attributing a line to it is the
            // service speaking as itself, not forging a user.
            let own_bot = self
                .plugin_id()
                .and_then(|id| self.ctx.provider_bot(id))
                .is_some_and(|bot| bot == sender.account);

            // The return half of §8's outbound relay: weftd itself asked the
            // provider to perform this local user's mutation foreign-side
            // (`relay_provider_mut`), and the provider confirming it IS the
            // flow completing — without this arm the flip side can never
            // close, because the puppet's echo always maps back to a local
            // account. Bounded hard: only the mutation verbs, and only on a
            // root the realm itself minted — the exact class weftd relays.
            // Touching a *local-origin* root stays a forgery and is refused.
            let confirms_relay = match &cmd {
                Command::Edit { msgid, .. }
                | Command::Delete { msgid }
                | Command::React { msgid, .. }
                | Command::Unreact { msgid, .. } => msgid.origin().as_str() == origin.realm(),

                // The realm is the source of truth in its own channels, so a
                // local user's post is relayed out and minted *there*; this is
                // that answer coming back. Authorized by the bridge label we
                // issued for that very post — an unlabelled `MSG` attributed to
                // a local account is still a forgery.
                Command::Msg { .. } => relay_echo.is_some(),

                _ => false,
            };

            if !own_bot && !confirms_relay {
                return self
                    .unsupported(None, "@as cannot name a local account")
                    .await;
            }
        }
        if let Ok(Some(_)) = self.ctx.peers.peer(&sender.network).await {
            return self
                .unsupported(None, "@as cannot name a peer network's user")
                .await;
        }

        // Invariant 7 effect 3, name-keyed: a realm blocked mid-session stops
        // being ingested at once, exactly as a blocked peer does — a bridge is
        // not a way back in for a network an operator has shut out. The check
        // covers both the channel's realm and the sender's own network, so
        // blocking a homeserver silences its users everywhere.
        for blocked in [&realm, &sender.network] {
            if self
                .ctx
                .netblocks
                .is_netblocked(blocked)
                .await
                .unwrap_or(false)
            {
                debug!(%blocked, "ingestion touching a netblocked network — dropped");
                return Ok(Flow::Continue);
            }
        }

        // The provider minted these, exactly as a peer network does, so they take
        // the ordinary federated ingest path. `ingest_record` re-checks that every
        // msgid originates on `realm` (invariant 2) and mints only the local
        // bookkeeping ids that a delete/react row needs. Passing our own session
        // id makes the loop guard structural — our forwarder skips the event it
        // just ingested.
        let Some(event) = provider_event(sender, channel, cmd, line) else {
            return Ok(Flow::Continue);
        };
        let Some((_, record)) = super::federation::ingest_record(&realm, &event) else {
            debug!("provider ingest with a msgid outside its realm — dropped");
            return Ok(Flow::Continue);
        };

        // A labelled copy is the realm's minted answer to a local user's post, so
        // it is ingested **as that session's own**: its queued label attaches and
        // the client reconciles the message it is waiting for. Same mechanism the
        // peer-spoke and group paths use. Anything else fans out normally as
        // someone else's message.
        handle
            .ingest(relay_echo.unwrap_or(self.id), record, event)
            .await;

        Ok(Flow::Continue)
    }

    /// May this provider's key speak for `channel`?
    ///
    /// Foreign traffic has **two doors**, and every attributed line has to accept
    /// both or it silently works in one direction only: a **replica** channel, whose
    /// `origin` names a scheme the key is pinned for, and a **native** channel whose
    /// namespace opted into projection with `bridge:<scheme>` (matrix.md §17.1). The
    /// ingest path already branches on exactly this; the ephemera below need the same
    /// answer without the branch, so it lives here once.
    async fn may_speak_for(&self, key: &PublicKey, channel: &ChannelName) -> bool {
        let Ok(Some(record)) = self.ctx.channel_store.channel(channel).await else {
            return false;
        };

        if let Some(origin) = record
            .origin
            .as_deref()
            .and_then(|o| o.parse::<ForeignUri>().ok())
        {
            return self.ctx.scheme_authorized(key, origin.scheme());
        }

        let Some(ns_id) = channel.namespace() else {
            return false; // a top-level channel is never bridged
        };
        let Ok(Some(ns)) = self.ctx.namespaces.namespace_by_id(ns_id).await else {
            return false;
        };

        ns.bridges.iter().any(|b| {
            b.parse::<Scheme>()
                .is_ok_and(|b| self.ctx.scheme_authorized(key, &b))
        })
    }

    /// A **relayed** post the realm refused: answer the waiting session on the
    /// label it queued, so the author learns at once instead of waiting out their
    /// client's send deadline.
    ///
    /// The bridge label is the only handle either side has on such a post — weftd
    /// minted nothing — so it carries the routing: which session is waiting, and
    /// which channel's pending label answers.
    async fn fail_relayed_post(&mut self, label: &str, reason: Option<String>) {
        let Some((session, channel)) = self.ctx.take_group_echo_failure(label) else {
            debug!(label, "UNDELIVERED for an unknown or expired bridge label");
            return;
        };

        self.ctx
            .directory
            .fail_relay(
                session,
                channel,
                reason.unwrap_or_else(|| "the bridge could not deliver it".to_string()),
            )
            .await;
    }

    /// §15 a realm's user is typing in one of its channels.
    ///
    /// Bounded like every other attributed line: the channel must be one this
    /// provider may speak for (replica *or* projected — see [`Self::may_speak_for`]),
    /// and the sender must be foreign — a local "is typing" from a bridge would be a
    /// small forgery, but a forgery. Ephemeral, so it is announced, never ingested.
    async fn on_provider_typing(
        &mut self,
        key: &PublicKey,
        sender: UserRef,
        channel: ChannelName,
        state: weft_proto::TypingState,
    ) -> io::Result<Flow> {
        if sender.network == self.ctx.info.network {
            return self
                .unsupported(None, "@as cannot name a local account")
                .await;
        }

        if !self.may_speak_for(key, &channel).await {
            return Ok(Flow::Continue);
        }

        if let Some(handle) = self.ctx.registry.get(&channel) {
            handle
                .announce(Event::Typing {
                    channel,
                    user: sender,
                    state,
                })
                .await;
        }

        Ok(Flow::Continue)
    }

    /// §6.1 a realm's user changed presence.
    ///
    /// Presence is per-*user* on the wire (`PRESENCE <status>` names nobody — a
    /// client's session identifies them), so the fan-out is ours to do: we remember
    /// the status against the qualified user and announce it into the replica
    /// channels of the namespaces they belong to. Bounded the same way typing is —
    /// only namespaces whose scheme this provider's key is pinned for.
    ///
    /// `invisible` is stored but not announced, as for a local session: relaying it
    /// would reveal the hiding.
    async fn on_provider_presence(
        &mut self,
        key: &PublicKey,
        sender: UserRef,
        status: weft_proto::PresenceStatus,
    ) -> io::Result<Flow> {
        if sender.network == self.ctx.info.network {
            return self
                .unsupported(None, "@as cannot name a local account")
                .await;
        }

        // Both doors again (see `may_speak_for`): the realm's own **replicas**, and
        // **native** namespaces that opted into projection. Checking only the first
        // meant a Matrix user in a projected room had no presence at all.
        let replicas = self
            .ctx
            .namespaces
            .namespaces_with_origin()
            .await
            .unwrap_or_default();
        let projected = self
            .ctx
            .namespaces
            .namespaces_bridged()
            .await
            .unwrap_or_default();

        let mut announce_in = Vec::new();
        for record in replicas.into_iter().chain(projected) {
            let scheme_of_origin = record
                .origin
                .as_deref()
                .and_then(|o| o.parse::<ForeignUri>().ok());
            let authorized = match scheme_of_origin {
                Some(uri) => self.ctx.scheme_authorized(key, uri.scheme()),
                None => record.bridges.iter().any(|b| {
                    b.parse::<Scheme>()
                        .is_ok_and(|b| self.ctx.scheme_authorized(key, &b))
                }),
            };
            if !authorized {
                continue;
            }

            // Membership is namespace-level (§5.3), so a member of the namespace is
            // a member of its channels — no per-channel roster to consult.
            let member = weft_store::member_key(&sender, &self.ctx.info.network);
            match self.ctx.memberships.is_ns_member(&member, &record.id).await {
                Ok(true) => {}
                _ => continue,
            }

            if let Ok(channels) = self
                .ctx
                .channel_store
                .channels_in_namespace(&record.id)
                .await
            {
                announce_in.extend(channels.into_iter().map(|(channel, _)| channel));
            }
        }

        if announce_in.is_empty() {
            return Ok(Flow::Continue); // a user we share nothing with
        }

        {
            let mut map = self.ctx.presence.lock().expect("presence lock");
            map.insert(sender.clone(), status);
        }

        if status != weft_proto::PresenceStatus::Invisible {
            for channel in announce_in {
                if let Some(handle) = self.ctx.registry.get(&channel) {
                    handle
                        .announce(Event::Presence {
                            user: sender.clone(),
                            status,
                        })
                        .await;
                }
            }
        }

        Ok(Flow::Continue)
    }

    /// Outbound projection, the return path (owner decision 2026-08-06): a
    /// foreign user's traffic entering a **native** channel whose namespace
    /// opted in via `bridge:<scheme>` — the flag is the authorization anchor.
    ///
    /// The differences from replica ingestion are the point:
    /// - **The home mints.** A carried `@msgid` is refused outright — a
    ///   foreign-minted id on a native channel would break home authority
    ///   (invariant 2 read in this direction).
    /// - **No local `@as` at all.** Local users act natively here; there is no
    ///   relay to confirm, so a local sender is always a forgery.
    /// - **The ack is the echo** (§3.5): the minted event returns on this same
    ///   session carrying the injection's label (`on_provider_event`), which is
    ///   how the adapter learns the minted id.
    async fn on_projected_ingest(
        &mut self,
        key: &PublicKey,
        sender: UserRef,
        cmd: Command,
        line: &Line,
        channel: ChannelName,
        _record: weft_store::ChannelRecord,
    ) -> io::Result<Flow> {
        // The namespace rides the canonical channel name (`#<ns-id>/<chan-id>`);
        // a top-level network channel has none and is never projectable.
        let Some(ns_id) = channel.namespace().map(str::to_string) else {
            return self
                .unsupported(None, "not a provider-managed channel")
                .await;
        };
        let ns = match self.ctx.namespaces.namespace_by_id(&ns_id).await {
            Ok(Some(ns)) => ns,
            _ => {
                debug!(%channel, "projected ingest for a channel with no namespace — dropped");
                return Ok(Flow::Continue);
            }
        };

        let projected = ns.bridges.iter().any(|b| {
            b.parse::<Scheme>()
                .is_ok_and(|b| self.ctx.scheme_authorized(key, &b))
        });
        if !projected {
            return self
                .unsupported(None, "not a provider-managed channel")
                .await;
        }

        if sender.network == self.ctx.info.network {
            return self
                .unsupported(None, "@as cannot name a local account")
                .await;
        }
        if let Ok(Some(_)) = self.ctx.peers.peer(&sender.network).await {
            return self
                .unsupported(None, "@as cannot name a peer network's user")
                .await;
        }
        if self
            .ctx
            .netblocks
            .is_netblocked(&sender.network)
            .await
            .unwrap_or(false)
        {
            debug!(network = %sender.network, "projected ingest from a netblocked network — dropped");
            return Ok(Flow::Continue);
        }

        if line.tags.contains_key("msgid") {
            return self.unsupported(None, "the home mints — drop @msgid").await;
        }

        let Some(handle) = self.ctx.registry.get(&channel) else {
            debug!(%channel, "projected ingest for a channel with no live actor — dropped");
            return Ok(Flow::Continue);
        };

        match cmd {
            Command::Msg { body, meta, .. } => {
                // The echo returns to this session keyed on its bound realm —
                // the injection's label rides it back as the ack (§3.5).
                let echo = match (&self.state, line.tags.get("label")) {
                    (
                        State::PluginService {
                            realm: Some(realm), ..
                        },
                        Some(label),
                    ) => realm.realm().parse().ok().map(|net| (label.clone(), net)),
                    _ => None,
                };

                handle
                    .relay_publish(sender, body.unwrap_or_default(), meta, echo)
                    .await;
            }
            Command::Edit { msgid, body } => {
                // §11.4 authored-by, exactly as the home re-checks a spoke's
                // relay: EDIT requires the sender to own the target.
                match self.ctx.events.find_root(msgid.ulid()).await {
                    Ok(Some(target)) if target.sender == sender => {}
                    Ok(_) => return Ok(Flow::Continue),
                    Err(e) => return self.internal(None, &e).await,
                }
                handle
                    .relay_mutate(sender, msgid, "edit".into(), body)
                    .await;
            }
            Command::Delete { msgid } => {
                match self.ctx.events.find_root(msgid.ulid()).await {
                    Ok(Some(target)) if target.sender == sender => {}
                    // Not the author: a foreign **moderator** may still delete
                    // iff WEFT granted them `delete-any` (§10 — a Matrix mod's
                    // redaction has exactly the power some grant gave them).
                    Ok(Some(_)) => {
                        let allowed = self
                            .ctx
                            .actor_has_cap(
                                &Actor::Foreign(sender.to_string()),
                                &weft_crypto::Capability::DeleteAny,
                                &TokenScope::Channel(channel.to_string()),
                                unix_now(),
                            )
                            .await
                            .unwrap_or(false);
                        if !allowed {
                            return Ok(Flow::Continue);
                        }
                    }
                    Ok(None) => return Ok(Flow::Continue),
                    Err(e) => return self.internal(None, &e).await,
                }
                handle
                    .relay_mutate(sender, msgid, "delete".into(), String::new())
                    .await;
            }
            Command::React { msgid, emoji } => {
                handle
                    .relay_mutate(sender, msgid, "react-add".into(), emoji)
                    .await;
            }
            Command::Unreact { msgid, emoji } => {
                handle
                    .relay_mutate(sender, msgid, "react-remove".into(), emoji)
                    .await;
            }
            _ => {
                debug!("unsupported projected-ingest verb — dropped");
            }
        }

        Ok(Flow::Continue)
    }

    /// §13 `STREAM OFFER` from a provider: mint the one-shot upload grant it
    /// posts the bytes with (`POST /media?t=…`).
    ///
    /// Attributed to the provider's **bot** when it has one, so the blob's
    /// uploader is a real identity; otherwise to the realm's own name, which is
    /// enough for the grant's bookkeeping (the fetch path is content-addressed
    /// and does not consult it).
    async fn on_provider_stream_offer(
        &mut self,
        label: Option<String>,
        plugin_id: &str,
        mode: weft_proto::StreamMode,
        mime: String,
        bytes: u64,
    ) -> io::Result<Flow> {
        if mode != weft_proto::StreamMode::Media {
            return self
                .unsupported(label, "a provider offers media only")
                .await;
        }
        if bytes == 0 || bytes > crate::MEDIA_MAX_BYTES {
            self.send_err(label, ErrCode::TooLarge, None, "blob size out of range")
                .await?;
            return Ok(Flow::Continue);
        }

        let uploader = self.ctx.provider_bot(plugin_id).unwrap_or_else(|| {
            plugin_id
                .parse()
                .unwrap_or_else(|_| "bridge".parse().expect("a valid fallback account"))
        });
        let token = self.ctx.mint_upload_token(uploader, mime, bytes);

        self.send_event(label, Event::StreamAccept { token })
            .await?;
        Ok(Flow::Continue)
    }

    /// This session's provider id, if it is a provider session.
    fn plugin_id(&self) -> Option<&str> {
        match &self.state {
            State::PluginService { plugin_id, .. } => Some(plugin_id),
            _ => None,
        }
    }

    /// The channel a stored msgid belongs to — the mutation verbs name their
    /// target by msgid, not channel (slice 4).
    async fn channel_of_msgid(&self, msgid: &MsgId) -> Option<ChannelName> {
        match self.ctx.events.find_root(msgid.ulid()).await {
            Ok(Some(record)) => match record.scope {
                Scope::Channel(channel) => Some(channel),
                _ => None,
            },
            _ => None,
        }
    }

    /// §3.1 REALM WITHDRAW: the provider says the bound realm is **gone** (deleted
    /// upstream) — distinct from a disconnect (which is only *offline*). weftd
    /// withdraws the realm's virtual namespaces cleanly: the full deletion
    /// cascade, with the tombstone pushed to every member. Unbound (no prior
    /// `REALM ASSERT`) ⇒ nothing to withdraw; the binding clears either way.
    async fn on_realm_withdraw(&mut self) -> io::Result<Flow> {
        // Take the bound realm straight off the live session state — `take`
        // clears the binding in place. Never *construct* provider state from
        // copies: if the session isn't a provider session (unreachable via the
        // on_line intercept, but load-bearing if dispatch ever changes), this
        // must be a no-op, not a state overwrite.
        let realm = match &mut self.state {
            State::PluginService { realm, .. } => realm.take(),
            _ => return Ok(Flow::Continue),
        };

        if let Some(realm) = realm {
            let prefix = format!("{realm}/");
            let namespaces = self
                .ctx
                .namespaces
                .namespaces_with_origin()
                .await
                .unwrap_or_default();

            for record in namespaces {
                let in_realm = record
                    .origin
                    .as_deref()
                    .is_some_and(|o| o == realm.to_string() || o.starts_with(&prefix));

                if !in_realm {
                    continue;
                }

                match self.delete_namespace_cascade(&record).await {
                    Ok(members) => {
                        let tombstone = Self::deletion_tombstone(&record);

                        // Local sessions only — a bridged member has none (4c).
                        for member in members.iter().filter_map(|m| weft_store::local_member(m)) {
                            self.ctx.directory.notify(member, tombstone.clone()).await;
                        }
                        info!(ns = %record.id, %realm, "virtual namespace withdrawn");
                    }
                    Err(e) => error!("withdraw cascade failed for {}: {e}", record.id),
                }
            }
        }

        Ok(Flow::Continue) // the binding was already cleared by `take`
    }

    /// Slice 5 — **outbound relay**: subscribe this provider session to every
    /// replica channel under `schemes`, so a local user's post/edit/delete/react
    /// is forwarded to the provider to puppet into the foreign system
    /// (framework §3.2). Idempotent; called whenever the provider's scheme set
    /// grows (`PLUGIN-REGISTER` / `REALM REGISTER` / `REALM ASSERT`).
    /// Outbound projection: describe every projected namespace serving these
    /// schemes to the provider — `NS-META` (carrying `bridges=`), then each
    /// channel's `CHANNEL-LAYOUT` + `POLICY`. The same events the provider
    /// itself speaks inbound for a replica, with the roles swapped: here weftd
    /// holds the structure and the adapter conforms (it needs the policy to
    /// apply the projection rules — `permanent`-only, no e2ee, no voice).
    ///
    /// Runs at registration/`REALM ASSERT`, like the forwarder sweep — a flag
    /// flipped mid-session is picked up on reconnect (§10's recovery story).
    pub(super) async fn push_projected_structure(&mut self, schemes: &[Scheme]) -> io::Result<()> {
        let namespaces = self
            .ctx
            .namespaces
            .namespaces_bridged()
            .await
            .unwrap_or_default();

        for record in namespaces {
            let serves = record
                .bridges
                .iter()
                .any(|b| b.parse::<Scheme>().is_ok_and(|b| schemes.contains(&b)));
            if !serves {
                continue;
            }

            self.send_event(None, self.ns_meta_event(&record)).await?;

            let Ok(channels) = self
                .ctx
                .channel_store
                .channels_in_namespace(&record.id)
                .await
            else {
                continue;
            };

            for (channel, chan) in channels {
                self.send_event(
                    None,
                    Event::ChannelLayout {
                        channel: channel.clone(),
                        category: chan.category.clone(),
                        position: chan.position,
                        kind: chan.kind,
                        vanity: chan.vanity.clone(),
                        origin: None,
                    },
                )
                .await?;
                self.send_event(
                    None,
                    Event::Policy {
                        channel,
                        policy: chan.policy,
                    },
                )
                .await?;
            }
        }

        Ok(())
    }

    /// The mirror of the realm's full-replace window, in the direction where
    /// **weftd** holds the state: our local membership of every namespace this
    /// provider's realms govern, stated on registration.
    ///
    /// A provider's pushes are live-only, so an `NS LEAVE` applied while the
    /// adapter was down never reaches it and the foreign side keeps a member we
    /// no longer have. The adapter cannot ask — it holds a key, not an account,
    /// so the cap-gated `NS INFO MEMBERS` is closed to it — and it cannot infer a
    /// *departure* from anything pushed incrementally, since an absent name and
    /// an unchanged one look identical. Only a complete set lets it reconcile by
    /// difference, which is why this is framed as the same `ni…` roster BATCH the
    /// verb produces: `BATCH END` is the signal that the set is whole and whoever
    /// is missing may be dropped foreign-side.
    ///
    /// Local members only — the realm is authoritative for its own users and
    /// already knows them (they are what it re-states back to us).
    ///
    /// **One** batch covers every governed namespace, and the rows name their own
    /// namespace, because the interesting case is a namespace with *no* local
    /// members left: per-namespace batches would frame that as a batch containing
    /// nothing that says which namespace it was about, so the one namespace that
    /// most needs reconciling is the one the adapter could not identify. Spanning
    /// them makes `BATCH END` mean "this is the whole of what I hold for your
    /// schemes", and an absent namespace is then honestly empty.
    pub(super) async fn push_consumed_membership(&mut self, schemes: &[Scheme]) -> io::Result<()> {
        let namespaces = self
            .ctx
            .namespaces
            .namespaces_with_origin()
            .await
            .unwrap_or_default();

        let governed: Vec<_> = namespaces
            .into_iter()
            .filter(|record| {
                record
                    .origin
                    .as_deref()
                    .and_then(|o| o.parse::<ForeignUri>().ok())
                    .is_some_and(|uri| schemes.contains(uri.scheme()))
            })
            .collect();

        if governed.is_empty() {
            return Ok(());
        }

        self.batches += 1;
        let id = format!("ni{}", self.batches);
        self.send_event(None, Event::BatchStart { id: id.clone() })
            .await?;

        for record in governed {
            let Ok(namespace) = record.id.parse::<weft_proto::NamespaceId>() else {
                continue;
            };
            let Ok(members) = self.ctx.memberships.ns_members_joined(&record.id).await else {
                continue;
            };

            for (member, joined_ms) in members {
                let Some(account) = weft_store::local_member(&member) else {
                    continue;
                };

                self.send_event(
                    None,
                    Event::NsMemberInfo {
                        namespace,
                        user: UserRef::new(account, self.ctx.info.network.clone()),
                        joined_ms: joined_ms.max(0) as u64,
                        roles: Vec::new(),
                    },
                )
                .await?;
            }
        }

        self.send_event(
            None,
            Event::BatchEnd {
                id,
                truncated: false,
            },
        )
        .await?;

        Ok(())
    }

    pub(super) async fn sync_provider_forwarders(&mut self, schemes: &[Scheme]) {
        // Two families feed a provider: the replicas of its own realms, and —
        // outbound projection (matrix.md §17.1) — native namespaces whose
        // `bridge:<scheme>` opt-in names one of its schemes.
        let mut namespaces = self
            .ctx
            .namespaces
            .namespaces_with_origin()
            .await
            .unwrap_or_default();
        namespaces.extend(
            self.ctx
                .namespaces
                .namespaces_bridged()
                .await
                .unwrap_or_default(),
        );

        for record in namespaces {
            let replica = record
                .origin
                .as_deref()
                .and_then(|o| o.parse::<ForeignUri>().ok())
                .is_some_and(|uri| schemes.contains(uri.scheme()));
            let projected = record
                .bridges
                .iter()
                .any(|b| b.parse::<Scheme>().is_ok_and(|b| schemes.contains(&b)));

            if !replica && !projected {
                continue;
            }

            let Ok(channels) = self
                .ctx
                .channel_store
                .channels_in_namespace(&record.id)
                .await
            else {
                continue;
            };

            for (channel, _) in channels {
                if self.bridged.contains_key(&channel) {
                    continue;
                }
                let Some(handle) = self.ctx.registry.get(&channel) else {
                    continue;
                };
                let Some(rx) = handle.subscribe().await else {
                    continue;
                };

                let forwarder = spawn_forwarder(channel.clone(), rx, self.events_tx.clone());
                self.bridged.insert(channel, forwarder);
            }
        }
    }

    /// Slice 5: a replica channel's event reached this provider session — forward
    /// the **local-origin** ones so the provider can puppet them outward.
    ///
    /// **Loop guard:** the same one-hop rule as a peer bridge (§11.4) — only
    /// events *this* network minted cross the link. A replica is multi-origin
    /// (our members post under our origin, the provider's users under their
    /// realm), so an event we ingested is never sent back to the provider that
    /// produced it — which would ping-pong into the foreign system.
    pub(super) async fn on_provider_event(&mut self, event: SessionEvent) -> io::Result<()> {
        // A channel created in a projected namespace after our startup sweep:
        // start watching it, so its traffic mirrors without a reconnect.
        if let SessionEvent::Attach { channel, ready } = event {
            if !self.bridged.contains_key(&channel) {
                if let Some(handle) = self.ctx.registry.get(&channel) {
                    if let Some(rx) = handle.subscribe().await {
                        let forwarder =
                            spawn_forwarder(channel.clone(), rx, self.events_tx.clone());
                        self.bridged.insert(channel, forwarder);
                    }
                }
            }

            // Confirm either way — already-attached is just as ready, and the
            // creator is waiting on this before it acks.
            if let Some(ready) = ready {
                let _ = ready.send(());
            }
            return Ok(());
        }

        let SessionEvent::Channel { event, .. } = event else {
            return Ok(()); // Lagged: the provider re-syncs via HISTORY
        };

        let ours = |id: &MsgId| id.origin().as_str() == self.ctx.network_name();
        let forward = match &event.event {
            // System lines (join/part notices) are local channel noise.
            Event::Message(m) if m.meta.system.is_some() => false,
            Event::Message(m) => ours(&m.msgid),
            Event::Edited { msgid, .. }
            | Event::Deleted { msgid, .. }
            | Event::Reaction { msgid, .. } => ours(msgid),
            // MEMBER carries no msgid, so membership uses the same rule against
            // the *user*: our own members' joins/parts go out (the provider
            // mirrors them into the foreign room), a bridged member's do not —
            // that one is the echo of an ingested JOIN/PART.
            Event::Member { user, .. } => user.network.as_str() == self.ctx.network_name(),
            // §15 typing is bridged both ways; like MEMBER it has no msgid, so
            // the same rule applies against the *user* — ours goes out, a
            // bridged user's does not (that one is the echo of an ingest).
            Event::Typing { user, .. } => user.network.as_str() == self.ctx.network_name(),
            // §6.1 presence, same rule again: our members' status goes out for the
            // adapter to set on their puppet, a bridged user's does not — that one
            // is the echo of what the realm just told us. `invisible` never reaches
            // here (it is stored and not announced), so nothing leaks a hidden user.
            Event::Presence { user, .. } => user.network.as_str() == self.ctx.network_name(),
            // POLICY is not relayed outward.
            _ => false,
        };

        if forward {
            // §3.5 on the projection path: an event minted from this session's
            // own injection carries the injection's label back — the sender's
            // echo is the ack, and it is how the daemon learns the minted id.
            // Keyed on the bound realm, exactly like the peer forwarder (§11.14).
            let label = match (&event.echo, &self.state) {
                (
                    Some((l, net)),
                    State::PluginService {
                        realm: Some(realm), ..
                    },
                ) if net.as_str() == realm.realm() => Some(l.clone()),
                _ => None,
            };

            // A local message handed to a provider is *in flight*: stored and acked
            // here, but whether the realm has it is unanswered until the adapter
            // says so.
            //
            // NOT an injected echo, though. A message the provider itself injected
            // (a Matrix user's, minted here for the projection return path) is
            // home-minted, so it looks local by msgid — but it already came *from*
            // the realm. The adapter recognises its own label, links the id and
            // returns, so no ack was ever coming, and awaiting one reported a
            // Matrix user's message as undelivered to Matrix.
            //
            // Messages only: an edit or reaction that never lands is a lesser wrong
            // than a message the author believes was delivered.
            if label.is_none() {
                if let Event::Message(m) = &event.event {
                    if let (None, weft_proto::Target::Channel(channel)) =
                        (m.meta.system.as_ref(), &m.target)
                    {
                        self.ctx.await_delivery(
                            m.msgid.clone(),
                            m.sender.account.clone(),
                            channel.clone(),
                            unix_now() * 1000,
                        );
                    }
                }
            }

            // The acting **local** user's ULID rides as `ulid=` (owner
            // directive 2026-08-06) — the adapter keys puppets by it, and
            // names are mutable vanity labels. Cached per session: one store
            // hit per distinct author, not per message.
            let actor = match &event.event {
                Event::Message(m) => Some(&m.sender),
                Event::Edited { user, .. } => Some(user),
                Event::Reaction { by, .. } => Some(by),
                Event::Deleted { by: Some(by), .. } => Some(by),
                Event::Member { user, .. } => Some(user),
                // §15/§6.1 the daemon needs the ULID to pick the right puppet, and
                // typing and presence are the ephemeral events that cross.
                Event::Typing { user, .. } => Some(user),
                Event::Presence { user, .. } => Some(user),
                _ => None,
            };
            let ulid = match actor {
                Some(user) if user.network == self.ctx.info.network => {
                    self.cached_account_ulid(&user.account).await
                }
                _ => None,
            };

            if let Ok(mut line) = Reply::new(event.event).to_line() {
                if let Some(l) = label {
                    line.tags.insert("label".to_string(), l);
                }
                if let Some(ulid) = ulid {
                    line.tags.insert("ulid".to_string(), ulid);
                }
                if let Ok(serialized) = line.serialize() {
                    self.stream.send_line(&serialized).await?;
                }
            }
        }

        Ok(())
    }

    /// [`Self::on_provider_event`]'s ULID lookup, memoized for the session —
    /// authors repeat, and the map only ever holds this provider's audience.
    async fn cached_account_ulid(&mut self, account: &Account) -> Option<String> {
        if let Some(hit) = self.provider_ulids.get(account) {
            return Some(hit.clone());
        }

        let ulid = self
            .ctx
            .accounts
            .account_ulid(account)
            .await
            .ok()
            .flatten()?;
        self.provider_ulids.insert(account.clone(), ulid.clone());

        Some(ulid)
    }

    /// **Authority, outbound**: a local `GRANT`/`REVOKE` inside a replica
    /// namespace is relayed to the provider, which raises or lowers the
    /// corresponding foreign power level — so promoting someone to moderator here
    /// makes them one on the Matrix side too. The inverse of
    /// [`Self::on_provider_grant`]; together they make authority bidirectional.
    pub(super) async fn relay_provider_grant(
        &self,
        scope: &str,
        subject: &str,
        caps: Option<String>,
        grant: bool,
        actor: &Actor,
    ) {
        if matches!(actor, Actor::Provider(_)) {
            return; // the provider told *us* — echoing it back would loop
        }

        let Some(TokenScope::Namespace(ns)) = TokenScope::parse(scope) else {
            return; // only namespace authority maps onto a foreign space
        };
        let Ok(Some(record)) = self.ctx.namespaces.namespace_by_id(&ns).await else {
            return;
        };
        // A replica's realm scheme, or a projected namespace's flagged scheme
        // — either way the grant maps onto a foreign power level (§10).
        let scheme = match record
            .origin
            .as_deref()
            .and_then(|o| o.parse::<ForeignUri>().ok())
        {
            Some(uri) => Some(uri.scheme().clone()),
            None => record.bridges.iter().find_map(|b| b.parse::<Scheme>().ok()),
        };
        let Some((_, out)) = scheme
            .as_ref()
            .and_then(|s| self.ctx.provider_for_scheme(s))
        else {
            return; // native + unprojected, or the provider is offline
        };

        let cmd = if grant {
            Command::Grant {
                subject: subject.to_string(),
                scope: scope.to_string(),
                caps: caps.unwrap_or_default(),
                expiry: None,
            }
        } else {
            Command::Revoke {
                subject: subject.to_string(),
                scope: scope.to_string(),
                caps,
                epoch: None,
            }
        };

        // A **local** subject rides with `ulid=` like every other local-actor
        // relay: the adapter addresses them by their ULID-keyed puppet, and
        // without the id a grant for a user it has not seen post yet could
        // never be applied at all.
        let subject_ulid = match subject.parse::<Account>() {
            Ok(account) => self
                .ctx
                .accounts
                .account_ulid(&account)
                .await
                .ok()
                .flatten(),
            Err(_) => None, // a foreign handle addresses its own MXID
        };

        if let Ok(mut line) = Request::new(cmd).to_line() {
            if let Some(ulid) = subject_ulid {
                line.tags.insert("ulid".to_string(), ulid);
            }
            let Ok(line) = line.serialize() else {
                return;
            };
            if out.try_send(line).is_err() {
                warn!(%subject, "provider queue full — authority relay dropped");
            }
        }
    }

    /// Slice 4d, inbound: a bridged user DMs one of ours.
    ///
    /// The provider replays it as `@as=<their user> MSG @<our account>`, and it
    /// lands in the ordinary `Scope::Dm` — a bridged conversation is a
    /// first-class DM, not a second table. The realm minted the message, so the
    /// msgid keeps its origin (invariant 2); only the local delivery is ours.
    ///
    /// The `@as` sender was already checked to live on this provider's realm, so
    /// it cannot forge a DM from a local account.
    async fn on_provider_dm(
        &mut self,
        sender: &UserRef,
        to: Account,
        msgid: MsgId,
        body: Option<String>,
        meta: MsgMeta,
    ) -> io::Result<Flow> {
        self.ctx
            .directory
            .ingest_dm(sender.clone(), to, msgid, body.unwrap_or_default(), meta)
            .await;

        Ok(Flow::Continue)
    }

    /// §11.7 for bridges: a local client scrolled past what we hold of a replica
    /// channel, so ask the **realm** for that window.
    ///
    /// The same shape peer federation uses (`on_backfill_demand`): demand-driven,
    /// never an eager pull of a whole foreign scrollback, and deduped per
    /// `(channel, before)` so repeated scrolls over one window ask once. The
    /// realm answers by replaying the window as ordinary `@as` ingestion, which
    /// is already origin-checked — so there is no separate backfill ingress and
    /// no way for a provider to smuggle events in under cover of an answer.
    pub(super) async fn request_provider_backfill(
        &mut self,
        channel: &ChannelName,
        before: Option<&MsgId>,
    ) {
        let window = (channel.clone(), before.map(|m| m.to_string()));
        if !self.backfilled.insert(window) {
            return; // already asked for this window
        }

        let origin = match self.ctx.channel_store.channel(channel).await {
            Ok(Some(record)) => record.origin,
            _ => return,
        };
        let Some(uri) = origin.as_deref().and_then(|o| o.parse::<ForeignUri>().ok()) else {
            return; // a native channel has no realm to ask
        };
        let Some((_, out)) = self.ctx.provider_for_scheme(uri.scheme()) else {
            return; // offline; the client just sees the short page
        };

        let cmd = Command::History {
            target: Target::Channel(channel.clone()),
            before: before.cloned(),
            after: None,
            limit: Some(weft_proto::MAX_HISTORY_LIMIT),
            thread: None,
        };
        if let Ok(line) = Request::new(cmd).serialize() {
            let _ = out.try_send(line);
        }
    }

    /// Slice 4d: carry a DM to a peer on **another network**.
    ///
    /// The conversation is stored and echoed locally either way — a bridged DM is
    /// an ordinary `Scope::Dm` keyed by the peer's member key, not a second table
    /// — but a foreign peer can only be reached over their network's link, so the
    /// copy goes there too:
    ///
    /// - a **bridged** peer (`alice@matrix.org`, a realm we hold a provider for)
    ///   gets it down the provider's writer as `@as=<our user> MSG @<peer>`;
    /// - a **federated** peer (another WEFT network) rides the ordinary peer
    ///   bridge, the same route friends and group DMs already use.
    ///
    /// A local peer needs neither — `deliver_dm` already reached their sessions.
    pub(super) async fn relay_foreign_dm(
        &self,
        from: &Account,
        peer: &UserRef,
        body: String,
        meta: MsgMeta,
    ) {
        if peer.network == self.ctx.info.network {
            debug!(%peer, "DM stays local — no relay");
            return;
        }

        let cmd = Command::Msg {
            target: Target::User {
                account: peer.account.clone(),
                network: Some(peer.network.clone()),
            },
            body: Some(body),
            meta,
        };
        let Ok(line) = Request::new(cmd).to_line() else {
            return;
        };

        // A realm we bridge is served by its provider; anything else is a peer
        // WEFT network and takes the federation path.
        if self
            .ctx
            .netblocks
            .is_netblocked(&peer.network)
            .await
            .unwrap_or(false)
        {
            return; // blocked either way — bridged realm or peer network
        }

        if let Some(out) = self.ctx.provider_for_realm(peer.network.as_str()) {
            info!(%peer, %from, "relaying a DM to the realm's provider");
            let mut line = line;
            line.tags.insert(
                "as".to_string(),
                UserRef::new(from.clone(), self.ctx.info.network.clone()).to_string(),
            );
            match line.serialize() {
                Ok(serialized) => {
                    debug!(%peer, line = %serialized, "DM line handed to the provider");

                    if out.try_send(serialized).is_err() {
                        warn!(%peer, "provider queue full — DM relay dropped");
                    }
                }
                Err(e) => warn!(%peer, "DM line would not serialize: {e}"),
            }
            return;
        }

        // Not a bridged realm, so it must be a WEFT network: the social path either
        // rides an existing peer bridge or dials one (§11.10). If *that* finds no
        // route either, say so — a DM stored and echoed locally with nowhere to go is
        // the "sent, but nobody got it" case, and it should at least be in the log.
        if let Ok(serialized) = line.serialize() {
            let routed = self.ctx.request_friend_deliver(crate::FriendDeliver {
                peer: peer.network.clone(),
                from: Some(from.clone()),
                line: serialized,
            });

            if !routed {
                warn!(
                    %peer,
                    "DM has no route: no provider asserted that realm and no peer bridge \
                     could take it — it is stored locally only"
                );
            }
        }
    }

    /// §11.4 relay a mutation of a **provider-minted** message back to its origin.
    ///
    /// The foreign side is authoritative for its own messages, so we never mint
    /// the `EDITED`/`DELETED`/`REACTION` ourselves — that would be authoring an
    /// event under someone else's origin. Instead we ask the provider to perform
    /// it (a redaction, a reaction) and the *resulting* foreign event arrives back
    /// through ordinary ingestion, which is what local members then see.
    ///
    /// The wire form is the ordinary verb carrying `@as=<the acting local user>` —
    /// the mirror image of ingestion (§3.1), where `@as` names a foreign user
    /// acting on our side. In both directions it reads "on behalf of".
    pub(super) async fn relay_provider_mut(
        &self,
        origin: &str,
        user: &UserRef,
        root: MsgId,
        op: &str,
        arg: String,
    ) {
        let Ok(uri) = origin.parse::<ForeignUri>() else {
            return;
        };
        let Some((_, out)) = self.ctx.provider_for_scheme(uri.scheme()) else {
            return; // liveness is gated upstream; a race here just drops it
        };
        let ulid = self.actor_ulid(user).await;

        let cmd = match op {
            "edit" => Command::Edit {
                msgid: root,
                body: arg,
            },
            "delete" => Command::Delete { msgid: root },
            "react-add" => Command::React {
                msgid: root,
                emoji: arg,
            },
            "react-remove" => Command::Unreact {
                msgid: root,
                emoji: arg,
            },
            _ => return,
        };

        if let Ok(mut line) = Request::new(cmd).to_line() {
            line.tags.insert("as".to_string(), user.to_string());
            if let Some(ulid) = ulid {
                line.tags.insert("ulid".to_string(), ulid);
            }
            if let Ok(serialized) = line.serialize() {
                if out.try_send(serialized).is_err() {
                    warn!(%user, "provider queue full — mutation relay dropped");
                }
            }
        }
    }

    /// The acting **local** user's account ULID, for the `ulid=` tag on
    /// provider-bound relays (owner directive 2026-08-06): account names are
    /// mutable vanity labels, so an adapter that keyed puppets by name would
    /// orphan them on a rename. Foreign actors have no local ULID — `None`.
    pub(super) async fn actor_ulid(&self, user: &UserRef) -> Option<String> {
        if user.network != self.ctx.info.network {
            return None;
        }

        self.ctx
            .accounts
            .account_ulid(&user.account)
            .await
            .ok()
            .flatten()
    }

    /// Whether a channel/namespace `origin` names a replica whose provider is
    /// currently offline (owner directive 2026-08-04). `false` for a native
    /// object — it has no provider to be offline.
    ///
    /// **The foreign side is authoritative for its own spaces**, so while the
    /// only route to it is down we accept no writes into it at all: not posts,
    /// not edits, deletes or reactions. Taking one would leave local members
    /// looking at state the foreign room never agreed to, with nothing to
    /// reconcile against when the provider returns.
    pub(super) fn origin_offline(&self, origin: Option<&str>) -> bool {
        let Some(origin) = origin else {
            return false;
        };

        !origin
            .parse::<ForeignUri>()
            .is_ok_and(|uri| self.ctx.scheme_online(uri.scheme()))
    }

    /// Slice 5: a local user joined or left a replica namespace — **relay the
    /// request to the realm**, which is its authority.
    ///
    /// A bridge behaves as a federation peer, so the directions are the ones
    /// federation uses: we send the realm a **command** (`NS JOIN`/`NS LEAVE`
    /// carrying `@as=<the local user>` — "this user of ours asks to join"), and
    /// the realm answers with the **`NS-MEMBER` event** that states who is a
    /// member ([`Self::on_provider_ns_member`]). weftd never asserts membership
    /// of a foreign space; only the realm may.
    ///
    /// Membership is namespace-level — channels are not joinable — so this names
    /// only the namespace. Putting the user into the foreign rooms it maps
    /// (including ones created later) is the adapter's job.
    pub(super) async fn relay_ns_membership(
        &self,
        origin: Option<&str>,
        namespace: weft_proto::NamespaceId,
        user: &UserRef,
        action: MemberAction,
    ) {
        let Some(uri) = origin.and_then(|o| o.parse::<ForeignUri>().ok()) else {
            return; // a native namespace has no realm to ask
        };
        let Some((_, out)) = self.ctx.provider_for_scheme(uri.scheme()) else {
            return; // provider offline; it re-reads membership when it returns
        };
        let ulid = self.actor_ulid(user).await;

        let cmd = match action {
            MemberAction::Join => match namespace.to_string().parse() {
                Ok(ns) => Command::NsJoin { ns },
                Err(_) => return,
            },
            MemberAction::Part => Command::NsLeave { ns: namespace },
        };

        if let Ok(mut line) = Request::new(cmd).to_line() {
            line.tags.insert("as".to_string(), user.to_string());
            if let Some(ulid) = ulid {
                line.tags.insert("ulid".to_string(), ulid);
            }
            if let Ok(serialized) = line.serialize() {
                if out.try_send(serialized).is_err() {
                    warn!(%user, "provider queue full — membership relay dropped");
                }
            }
        }
    }

    /// Owner directive 2026-08-04: push the provider-state transition (`NS-META`
    /// with `provider=online|offline`) to every member of every virtual namespace
    /// under `schemes` — the client's live "bridge online/offline" indicator.
    /// Reads the registry's *current* state, so call it **after** the
    /// register/unregister that changed it.
    pub(super) async fn push_provider_state(&mut self, schemes: &[Scheme]) {
        let Ok(namespaces) = self.ctx.namespaces.namespaces_with_origin().await else {
            return;
        };

        for record in namespaces {
            let scheme_matches = record
                .origin
                .as_deref()
                .and_then(|o| o.parse::<ForeignUri>().ok())
                .is_some_and(|uri| schemes.contains(uri.scheme()));

            if !scheme_matches {
                continue;
            }

            let event = self.ns_meta_event(&record);
            if let Ok(members) = self.ctx.memberships.ns_members(&record.id).await {
                // Local sessions only — a bridged member has none (4c).
                for member in members.iter().filter_map(|m| weft_store::local_member(m)) {
                    self.ctx.directory.notify(member, event.clone()).await;
                }
            }
        }
    }

    /// Ensure the reserved, suspended **sentinel account** that owns every
    /// provider-managed namespace (Phase-0 decision: it exists so records have a
    /// valid owner; authority never derives from it — the origin gate strips the
    /// owner shortcut). Registration is idempotent; the password is random and
    /// never used (the account is suspended).
    async fn ensure_sentinel(&self) -> Option<Account> {
        let account: Account = FOREIGN_SENTINEL.parse().expect("sentinel name is valid");
        let secret = weft_crypto::b64::encode(rand::random::<[u8; 32]>());

        match self.ctx.accounts.register(&account, &secret).await {
            Ok(crate::accounts::RegisterOutcome::Created) => {
                let _ = self.ctx.accounts.set_suspended(&account, true).await;
            }
            Ok(crate::accounts::RegisterOutcome::Exists) => {}
            Err(e) => {
                warn!("sentinel account registration failed: {e}");
                return None;
            }
        }

        Some(account)
    }

    /// §3.1 REALM ASSERT: bind this provider connection to a single realm — the
    /// bridge data-connection handshake. The proven key must be authorized for
    /// the realm's scheme. Netblock gating arrives with the NETBLOCK slice.
    async fn on_realm_assert(
        &mut self,
        label: Option<String>,
        key: PublicKey,
        plugin_id: String,
        realm: ForeignUri,
    ) -> io::Result<Flow> {
        if !self.ctx.scheme_authorized(&key, realm.scheme()) {
            return self
                .unsupported(label, "provider key not pinned for that scheme")
                .await;
        }
        if self.ctx.scheme_held_by_other(realm.scheme(), &plugin_id) {
            return self
                .send_err(label, ErrCode::Conflict, None, "scheme already served")
                .await
                .map(|_| Flow::Continue);
        }
        if let Some(refusal) = self.realm_refusal(realm.realm()).await {
            return self
                .send_err(label, ErrCode::Forbidden, Some(refusal), "realm refused")
                .await
                .map(|_| Flow::Continue);
        }

        // A bound data connection is definitionally *serving* its scheme: it
        // joins the provider registry (liveness + PROVISION routing) like a
        // REALM REGISTER would, and its namespaces come online.
        let scheme = realm.scheme().clone();
        self.ctx.add_provider_scheme(
            &plugin_id,
            scheme.clone(),
            Some(realm.realm()),
            self.fed_out_tx.clone(),
            self.events_tx.clone(),
        );

        info!(%realm, %plugin_id, "provider data connection bound to realm");

        // Bind in place — never reconstruct the session state from copies.
        if let State::PluginService { realm: bound, .. } = &mut self.state {
            *bound = Some(realm);
        }

        self.push_provider_state(std::slice::from_ref(&scheme))
            .await;
        self.sync_provider_forwarders(std::slice::from_ref(&scheme))
            .await;
        self.push_projected_structure(std::slice::from_ref(&scheme))
            .await?;
        self.push_consumed_membership(&[scheme]).await?;
        Ok(Flow::Continue)
    }

    /// A role id → its name (the handlers key on names, the wire on ids).
    async fn role_name(&self, role_id: &str) -> Option<String> {
        self.ctx
            .roles
            .role_by_id(role_id)
            .await
            .ok()
            .flatten()
            .map(|(_, def)| def.name)
    }

    /// Why a realm may not be bound (4b) — `None` means it is fine.
    ///
    /// **A realm is a network** (§7a.0), which is what makes replicas behave like
    /// federation — but it also means a realm name lands in the *same identity
    /// space* as real WEFT networks. A realm called `weft.example` mints
    /// `alice@weft.example`, which is the very `UserRef` that network's own user
    /// has: same grant subject, same member key, same DM scope — and since DM
    /// routing prefers a provider over a peer, the realm would quietly receive
    /// mail addressed to them. Worse for **our own** name: `member_key` collapses
    /// a user on our network to their bare account, so a realm `test.example`
    /// would let a provider act as the local account `ada`.
    ///
    /// **The arbiter is the domain owner**, not our peer table: whoever controls
    /// `weft.example` chooses whether it runs a WEFT server or something a bridge
    /// reaches, and a domain publishing `/.well-known/weft` has chosen WEFT. Only
    /// a *positive* answer refuses — an unreachable domain, or a realm that is no
    /// domain at all (a Discord guild id), still binds (see [`crate::NetworkProbe`]).
    ///
    /// The local checks stay as a fast path and cover what the probe cannot: our
    /// own name, a peer we already hold a record for (authoritative regardless of
    /// what DNS says today), and any **netblocked** name — invariant 7 is
    /// name-keyed, so it must bite a realm exactly as it bites a peer, or
    /// blocking a network would be evadable by re-entering as a bridge.
    async fn realm_refusal(&self, realm: &str) -> Option<&'static str> {
        if realm == self.ctx.network_name() {
            return Some("own-network");
        }

        let Ok(name) = realm.parse::<NetworkName>() else {
            return Some("not-a-network-name"); // it could never mint valid users
        };
        if matches!(self.ctx.peers.peer(&name).await, Ok(Some(_))) {
            return Some("peer-network");
        }
        if self
            .ctx
            .netblocks
            .is_netblocked(&name)
            .await
            .unwrap_or(false)
        {
            return Some("netblocked");
        }
        if self.ctx.host_runs_weft(realm).await {
            return Some("domain-runs-weft");
        }

        None
    }

    /// M-plug-3: drive a flow already in progress — `PLUGIN SUBMIT` (a form step),
    /// `PLUGIN ACTION` (a control click), `SUBSCRIBE`/`UNSUBSCRIBE` (does anyone
    /// still have this panel open), `CLOSE` (dismissed).
    ///
    /// All five are the same routing problem, so they share it: find the flow by
    /// view-id, check it is the caller's, and hand the step to the plugin that
    /// owns it. The view-id carries the plugin (`<plugin>:<seq>`), so no extra
    /// bookkeeping is needed to know where a step goes.
    ///
    /// **Ownership matters here.** A view-id is guessable — it is a plugin name
    /// and a counter — so without a check any session could drive, read, or
    /// dismiss another user's dialog. The parked writer *is* the requester's, so
    /// comparing channels answers "is this yours" without new state. A view that
    /// is not yours is refused exactly as one that does not exist (invariant 1):
    /// same code, no branch that reveals which.
    pub(super) async fn on_plugin_step(
        &mut self,
        label: Option<String>,
        view_id: String,
        cmd: Command,
        terminal: bool,
    ) -> io::Result<Flow> {
        let Some((reply, _)) = self.ctx.peek_invoke(&view_id) else {
            return self.no_such_target(label).await;
        };
        if !reply.same_channel(&self.fed_out_tx) {
            return self.no_such_target(label).await;
        }
        let Some(out) = view_id
            .split_once(':')
            .and_then(|(plugin, _)| self.ctx.plugin_out(plugin))
        else {
            return self.no_such_target(label).await; // the plugin went away
        };

        // The plugin's next response should ack *this* step.
        self.ctx.relabel_invoke(&view_id, label.clone());

        // §11.3 panel liveness. The plugin is routed the step either way, so it
        // learns to stop pushing from the UNSUBSCRIBE itself.
        match &cmd {
            Command::PluginSubscribe { .. } => self.ctx.set_subscribed(&view_id, true),
            Command::PluginUnsubscribe { .. } => self.ctx.set_subscribed(&view_id, false),
            _ => {}
        }

        let Ok(line) = Request::with_label(cmd, view_id.clone()).serialize() else {
            return self.no_such_target(label).await;
        };
        if out.try_send(line).is_err() {
            self.ctx.complete_invoke(&view_id); // gone — nothing will answer
            return self.no_such_target(label).await;
        }

        // A dismissed view frees its parking: nothing more will arrive for it,
        // and leaving it would pin the requester's writer for the session's life.
        if terminal {
            self.ctx.complete_invoke(&view_id);
        }

        Ok(Flow::Continue)
    }

    /// §12.1 client `PLUGINS`: serve the action catalog of every registered plugin.
    pub(super) async fn on_plugins(&mut self, label: Option<String>) -> io::Result<Flow> {
        let catalog = match weft_proto::plugin_to_b64(&self.ctx.plugin_catalog()) {
            Ok(catalog) => catalog,
            Err(e) => return self.internal(label, &e).await,
        };

        self.send_event(label, Event::PluginManifest { catalog })
            .await?;
        Ok(Flow::Continue)
    }

    /// §12.1 client `PLUGIN INVOKE`: route to the plugin owning `action`, park the
    /// request keyed by a minted view-id, and push the invoke to the plugin's
    /// session; its `PLUGIN-VIEW`/`-RESULT` completes the request asynchronously.
    /// An unknown plugin/action → `NO-SUCH-TARGET` (invariant 10, anti-enumeration).
    pub(super) async fn on_plugin_invoke(
        &mut self,
        label: Option<String>,
        account: Account,
        plugin: String,
        action: String,
        ctx_ref: Option<String>,
        params: Option<String>,
    ) -> io::Result<Flow> {
        let Some(out) = self.ctx.plugin_out_for(&plugin, &action) else {
            return self.no_such_target(label).await;
        };

        // Correlate the whole flow by a minted view-id (§11.1), carried to the
        // plugin as the invoke's label and echoed on its responses.
        let view_id = self.ctx.mint_view_id(&plugin);
        let cmd = Command::PluginInvoke {
            plugin,
            action,
            ctx_ref,
            params,
        };
        // The invoker rides as `as=`/`ulid=` (slice 11): a management action's
        // handler must know **who** is asking to attribute the resulting wire
        // commands — anonymous invokes would force every provider to invent a
        // side-channel identity, or worse, act as itself.
        let invoker = UserRef::new(account.clone(), self.ctx.info.network.clone());
        let ulid = self.cached_account_ulid(&account).await;
        let Ok(mut line) = Request::with_label(cmd, view_id.clone()).to_line() else {
            return self.no_such_target(label).await;
        };
        line.tags.insert("as".to_string(), invoker.to_string());
        if let Some(ulid) = ulid {
            line.tags.insert("ulid".to_string(), ulid);
        }
        let Ok(line) = line.serialize() else {
            return self.no_such_target(label).await;
        };

        // Park before pushing so a fast reply can't race ahead of the pending.
        self.ctx
            .park_invoke(view_id.clone(), self.fed_out_tx.clone(), label.clone());

        if out.try_send(line).is_err() {
            self.ctx.complete_invoke(&view_id); // roll back — the plugin is gone
            return self.no_such_target(label).await;
        }

        Ok(Flow::Continue) // the reply arrives asynchronously on the plugin's response
    }
}
