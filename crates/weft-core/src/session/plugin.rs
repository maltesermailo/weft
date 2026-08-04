//! Plugin system (`docs/architecture/plugin-spec.md` §11–§12): the remote-plugin
//! (App Service) session + the client-facing action routing.
//!
//! **M-plug-2 scope:** a plugin authenticates (`AUTH ADAPTER`, §4.2), sends
//! `PLUGIN-REGISTER` (its actions), and weftd serves the catalog (`PLUGINS` →
//! `PLUGIN-MANIFEST`) and routes a client `PLUGIN INVOKE` to the plugin's session,
//! relaying its `PLUGIN-VIEW`/`-RESULT` back. Multi-step flows (`SUBMIT`/`ACTION`)
//! and hooks land in later milestones.

use super::*;

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
            schemes,
        );
        info!(%plugin_id, "provider registered");
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
        Ok(Flow::Continue)
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
