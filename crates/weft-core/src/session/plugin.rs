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
                Command::Grant {
                    subject,
                    scope,
                    caps,
                    ..
                } => {
                    return self
                        .on_provider_grant(&key, subject, scope, Some(caps), true)
                        .await;
                }
                Command::Revoke {
                    subject,
                    scope,
                    caps,
                    ..
                } => {
                    return self
                        .on_provider_grant(&key, subject, scope, caps, false)
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
                visibility,
                title,
                description,
                icon,
            } => {
                self.on_ns_assert(&key, uri, visibility, title, description, icon)
                    .await
            }
            Event::ChannelLayoutForeign {
                uri,
                position,
                kind,
                vanity,
                category,
            } => {
                self.on_channel_assert(&key, uri, position, kind, vanity, category)
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
                    if let Ok(line) = relay_line(Event::PluginView { view_id, view }, label) {
                        let _ = reply.try_send(line);
                    }
                }
                Ok(Flow::Continue)
            }
            Event::PluginPatch { view_id, patch } => {
                if let Some((reply, label)) = self.ctx.peek_invoke(&view_id) {
                    if let Ok(line) = relay_line(Event::PluginPatch { view_id, patch }, label) {
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

        self.ctx.register_plugin(
            plugin_id.to_string(),
            self.fed_out_tx.clone(),
            reg.name,
            reg.icon,
            reg.actions,
            schemes.clone(),
        );
        info!(%plugin_id, "provider registered");

        // Its virtual namespaces just came online — tell their members, and
        // start relaying their channels' local traffic outward (slice 5).
        if !schemes.is_empty() {
            self.push_provider_state(&schemes).await;
            self.sync_provider_forwarders(&schemes).await;
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

        self.ctx
            .add_provider_scheme(&plugin_id, scheme.clone(), self.fed_out_tx.clone());
        info!(%plugin_id, %scheme, "provider registered scheme");
        self.push_provider_state(std::slice::from_ref(&scheme))
            .await;
        self.sync_provider_forwarders(&[scheme]).await;
        Ok(Flow::Continue)
    }

    /// Framework §3.3 / capability 4: a provider asserts a **virtual namespace**
    /// (`NS-META <origin-uri> <visibility>`). weftd mints the replica id
    /// (invariant 2), owner = the suspended sentinel (Phase-0 decision — local
    /// owner authority is origin-gated away), and answers with the minted
    /// `NS-META` (id form, `origin=`) so the provider learns its mapping.
    /// Re-asserting an existing origin re-sends the mapping (structural *update*
    /// sync is a later slice).
    async fn on_ns_assert(
        &mut self,
        key: &PublicKey,
        uri: ForeignUri,
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
            Ok(Some(record)) => {
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
        let ns_id = weft_proto::Ulid::new().to_string().to_ascii_lowercase();
        let mut record = weft_store::NamespaceRecord {
            id: ns_id,
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
    async fn on_channel_assert(
        &mut self,
        key: &PublicKey,
        uri: ForeignUri,
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
            self.send_event(
                None,
                Event::ChannelLayout {
                    channel,
                    category: rec.category,
                    position: rec.position,
                    kind: rec.kind,
                    vanity: rec.vanity,
                    origin: Some(uri),
                },
            )
            .await?;
            return Ok(Flow::Continue);
        }

        let vanity = if vanity.is_empty() {
            sanitize_vanity(&segment)
        } else {
            sanitize_vanity(&vanity)
        };
        let policy: RetentionPolicy = "retained:90d".parse().expect("valid default");
        let chan_id = weft_proto::Ulid::new().to_string().to_ascii_lowercase();
        let canonical: ChannelName = format!("#{}/{chan_id}", record.id)
            .parse()
            .expect("a minted canonical channel name is valid");

        if self
            .ctx
            .registry
            .create(canonical.clone(), policy)
            .is_none()
        {
            return self.internal(None, &"minted channel collided").await;
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
    /// at which point every member of the provider's namespaces it did not name
    /// is dropped. Only the roles are swapped: here the realm is the one holding
    /// the state and weftd is the one conforming.
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
        let Some(uri) = record
            .origin
            .as_deref()
            .and_then(|o| o.parse::<ForeignUri>().ok())
        else {
            debug!(%ns, "NS-MEMBER for a native namespace — refused");
            return self
                .unsupported(None, "not a provider-managed namespace")
                .await;
        };
        if !self.ctx.scheme_authorized(key, uri.scheme()) {
            return self
                .unsupported(None, "provider key not pinned for that scheme")
                .await;
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

        Ok(Flow::Continue)
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

        match cmd {
            Command::Mute {
                scope,
                account: target,
                reason,
            } => {
                self.on_moderate(None, scope, target, ModKind::Mute, true, reason, actor)
                    .await
            }
            Command::Unmute {
                scope,
                account: target,
            } => {
                self.on_moderate(None, scope, target, ModKind::Mute, false, None, actor)
                    .await
            }
            Command::Ban {
                scope,
                account: target,
                reason,
            } => {
                self.on_moderate(None, scope, target, ModKind::Ban, true, reason, actor)
                    .await
            }
            Command::Unban {
                scope,
                account: target,
            } => {
                self.on_moderate(None, scope, target, ModKind::Ban, false, None, actor)
                    .await
            }
            Command::Kick {
                channel,
                account: target,
                reason,
            } => self.on_kick(None, channel, target, reason, actor).await,

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
    /// rooms it owns — and `@as`/`@msgid` must both name that channel's realm, so
    /// a provider cannot forge a local user or another realm's event.
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
            debug!(%channel, "provider ingest into a native channel — refused");
            return self
                .unsupported(None, "not a provider-managed channel")
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

        // A realm is a network: the sender must live on the one this channel
        // replicates, so a provider can never attribute an event to a local
        // account (or to another realm's user).
        let Ok(realm) = origin.realm().parse::<NetworkName>() else {
            debug!(realm = origin.realm(), "realm is not a usable network name");
            return self.unsupported(None, "realm is not a network name").await;
        };
        if sender.network != realm {
            return self
                .unsupported(None, "@as is not a user of this realm")
                .await;
        }
        // Invariant 7 effect 3, name-keyed: a realm blocked mid-session stops
        // being ingested at once, exactly as a blocked peer does — a bridge is
        // not a way back in for a network an operator has shut out.
        if self
            .ctx
            .netblocks
            .is_netblocked(&realm)
            .await
            .unwrap_or(false)
        {
            debug!(%realm, "ingestion from a netblocked realm — dropped");
            return Ok(Flow::Continue);
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

        handle.ingest(self.id, record, event).await;

        Ok(Flow::Continue)
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
    pub(super) async fn sync_provider_forwarders(&mut self, schemes: &[Scheme]) {
        let Ok(namespaces) = self.ctx.namespaces.namespaces_with_origin().await else {
            return;
        };

        for record in namespaces {
            let serves = record
                .origin
                .as_deref()
                .and_then(|o| o.parse::<ForeignUri>().ok())
                .is_some_and(|uri| schemes.contains(uri.scheme()));

            if !serves {
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
            // TYPING/POLICY are not relayed outward.
            _ => false,
        };

        if forward {
            if let Ok(line) = Reply::new(event.event).serialize() {
                self.stream.send_line(&line).await?;
            }
        }

        Ok(())
    }

    /// **Authority, inbound** (owner directive 2026-08-04): the provider grants or
    /// revokes capabilities inside a namespace it governs, so a moderator on the
    /// foreign side is a moderator here. A Matrix bridge translates its power
    /// levels into WEFT capabilities and sends an ordinary `GRANT`/`REVOKE`;
    /// weftd stays free of any notion of a power level — the adapter owns that
    /// mapping, exactly as it owns the identity mapping (§7a.0).
    ///
    /// **Authority to do it** is the same rule as ingestion: the scope must name a
    /// namespace this provider's key is pinned for. No capability chain is
    /// consulted — for a provider-managed namespace the provider *is* the
    /// governing authority (§7a.3), the way an owner is for a native one.
    async fn on_provider_grant(
        &mut self,
        key: &PublicKey,
        subject: String,
        scope: String,
        caps: Option<String>,
        grant: bool,
    ) -> io::Result<Flow> {
        let Some(TokenScope::Namespace(ns)) = TokenScope::parse(&scope) else {
            return self
                .unsupported(None, "a provider grants at ns: scope")
                .await;
        };
        let record = match self.ctx.namespaces.namespace_by_id(&ns).await {
            Ok(Some(record)) => record,
            Ok(None) => return self.no_such_target(None).await,
            Err(e) => return self.internal(None, &e).await,
        };
        let governs = record
            .origin
            .as_deref()
            .and_then(|o| o.parse::<ForeignUri>().ok())
            .is_some_and(|uri| self.ctx.scheme_authorized(key, uri.scheme()));
        if !governs {
            return self
                .unsupported(None, "not a namespace this provider governs")
                .await;
        }

        // The subject may be foreign (a Matrix user, keyed by `user@realm`) or a
        // local account — `resolve_subject` handles both.
        let store_key = match self.ctx.resolve_subject(&subject).await {
            Ok(Some((_, store_key))) => store_key,
            Ok(None) => return self.no_such_target(None).await,
            Err(e) => return self.internal(None, &e).await,
        };
        let parsed = caps.as_deref().and_then(parse_caps);
        if grant && parsed.is_none() {
            return self.unsupported(None, "unknown capability").await;
        }

        let wrote = if grant {
            let caps: Vec<String> = parsed
                .unwrap_or_default()
                .iter()
                .map(Capability::to_string)
                .collect();
            let epoch = match self.ctx.caps.scope_epoch(&scope).await {
                Ok(epoch) => epoch,
                Err(e) => return self.internal(None, &e).await,
            };

            self.ctx
                .caps
                .record_grant(&store_key, &scope, &caps, epoch, None)
                .await
        } else {
            let caps: Option<Vec<String>> =
                parsed.map(|caps| caps.iter().map(Capability::to_string).collect());

            self.ctx
                .caps
                .revoke_grants(&store_key, &scope, caps.as_deref())
                .await
                .map(|_| ())
        };
        if let Err(e) = wrote {
            return self.internal(None, &e).await;
        }

        Ok(Flow::Continue)
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
    ) {
        let Some(TokenScope::Namespace(ns)) = TokenScope::parse(scope) else {
            return; // only namespace authority maps onto a foreign space
        };
        let Ok(Some(record)) = self.ctx.namespaces.namespace_by_id(&ns).await else {
            return;
        };
        let Some(uri) = record
            .origin
            .as_deref()
            .and_then(|o| o.parse::<ForeignUri>().ok())
        else {
            return; // a native namespace has no provider to tell
        };
        let Some((_, out)) = self.ctx.provider_for_scheme(uri.scheme()) else {
            return;
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

        if let Ok(line) = Request::new(cmd).serialize() {
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

        if let Some(out) = self.ctx.provider_for_realm(peer.network.as_str()).await {
            let mut line = line;
            line.tags.insert(
                "as".to_string(),
                UserRef::new(from.clone(), self.ctx.info.network.clone()).to_string(),
            );
            if let Ok(serialized) = line.serialize() {
                if out.try_send(serialized).is_err() {
                    warn!(%peer, "provider queue full — DM relay dropped");
                }
            }
            return;
        }

        if let Ok(serialized) = line.serialize() {
            self.ctx.request_friend_deliver(crate::FriendDeliver {
                peer: peer.network.clone(),
                from: Some(from.clone()),
                line: serialized,
            });
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
    pub(super) fn relay_provider_mut(
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
            if let Ok(serialized) = line.serialize() {
                if out.try_send(serialized).is_err() {
                    warn!(%user, "provider queue full — mutation relay dropped");
                }
            }
        }
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
    pub(super) fn relay_ns_membership(
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

        let cmd = match action {
            MemberAction::Join => match namespace.to_string().parse() {
                Ok(ns) => Command::NsJoin { ns },
                Err(_) => return,
            },
            MemberAction::Part => Command::NsLeave { ns: namespace },
        };

        if let Ok(mut line) = Request::new(cmd).to_line() {
            line.tags.insert("as".to_string(), user.to_string());
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
        self.ctx
            .add_provider_scheme(&plugin_id, scheme.clone(), self.fed_out_tx.clone());

        info!(%realm, %plugin_id, "provider data connection bound to realm");

        // Bind in place — never reconstruct the session state from copies.
        if let State::PluginService { realm: bound, .. } = &mut self.state {
            *bound = Some(realm);
        }

        self.push_provider_state(std::slice::from_ref(&scheme))
            .await;
        self.sync_provider_forwarders(&[scheme]).await;
        Ok(Flow::Continue)
    }

    /// Why a realm may not be bound (4b) — `None` means it is fine.
    ///
    /// **A realm is a network** (§7a.0), which is what makes replicas behave like
    /// federation — but it also means a realm name lands in the *same namespace*
    /// as real WEFT networks. So a provider must not claim one that is already
    /// spoken for, or it could mint users indistinguishable from that network's
    /// (`alice@hda.example`) and — since DM routing prefers a provider over a
    /// peer — quietly receive mail addressed to them.
    ///
    /// Refused: **our own** network name, any network we hold a **peer record**
    /// for, and any **netblocked** name (invariant 7 is name-keyed, so it has to
    /// bite a realm exactly as it bites a peer — otherwise blocking a network
    /// would be evadable by re-entering as a bridge).
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

        None
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
        let Ok(line) = Request::with_label(cmd, view_id.clone()).serialize() else {
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
