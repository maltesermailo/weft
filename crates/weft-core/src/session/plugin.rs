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
        // Bridge-verb family first (they are Commands, not Events).
        if let Ok(req) = Request::from_line(line) {
            match req.command {
                Command::RealmRegister { scheme } => {
                    return self
                        .on_realm_register(req.label, key, plugin_id, scheme)
                        .await;
                }
                Command::RealmAssert { realm } => {
                    return self
                        .on_realm_assert(req.label, key, plugin_id, realm)
                        .await;
                }
                Command::RealmWithdraw => {
                    self.state = State::PluginService {
                        key,
                        plugin_id,
                        realm: None,
                    };
                    return Ok(Flow::Continue);
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
            self.send_err(None, ErrCode::Malformed, None, "undecodable PLUGIN-REGISTER")
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

        // Its virtual namespaces just came online — tell their members.
        if !schemes.is_empty() {
            self.push_provider_state(&schemes).await;
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

        self.ctx
            .add_provider_scheme(&plugin_id, scheme.clone(), self.fed_out_tx.clone());
        info!(%plugin_id, %scheme, "provider registered scheme");
        self.push_provider_state(&[scheme]).await;
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
        match self.ctx.namespaces.namespace_by_origin(&uri.to_string()).await {
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

        if self.ctx.registry.create(canonical.clone(), policy).is_none() {
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
                for member in members {
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

        info!(%realm, %plugin_id, "provider data connection bound to realm");
        self.state = State::PluginService {
            key,
            plugin_id,
            realm: Some(realm),
        };
        Ok(Flow::Continue)
    }

    /// §12.1 client `PLUGINS`: serve the action catalog of every registered plugin.
    pub(super) async fn on_plugins(&mut self, label: Option<String>) -> io::Result<Flow> {
        let catalog = match weft_proto::plugin_to_b64(&self.ctx.plugin_catalog()) {
            Ok(catalog) => catalog,
            Err(e) => return self.internal(label, &e).await,
        };

        self.send_event(label, Event::PluginManifest { catalog }).await?;
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
