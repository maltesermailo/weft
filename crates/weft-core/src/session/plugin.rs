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
    /// §4.2 remote-plugin PROOF verified: enter the plugin-service session bound to
    /// `plugin_id`. The plugin then sends `PLUGIN-REGISTER`.
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

        self.state = State::PluginService { key, plugin_id };
        Ok(Flow::Continue)
    }

    /// §12.3 route a line from a remote-plugin session: its `PLUGIN-REGISTER`
    /// self-description, and the `PLUGIN-VIEW`/`-PATCH`/`-RESULT` responses that
    /// weftd relays back to the client that invoked.
    pub(super) async fn on_plugin_service_line(
        &mut self,
        _key: PublicKey,
        plugin_id: String,
        line: &Line,
    ) -> io::Result<Flow> {
        let event = match Reply::from_line(line) {
            Ok(reply) => reply.event,
            Err(_) => return Ok(Flow::Continue), // tolerate noise on the session
        };

        match event {
            Event::PluginRegister { registration } => {
                self.on_plugin_register(&plugin_id, &registration)
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

    /// §12.3 a plugin's self-description: decode + validate + register its actions.
    fn on_plugin_register(&mut self, plugin_id: &str, registration: &str) -> io::Result<Flow> {
        let Ok(reg) = weft_proto::plugin_from_b64::<weft_proto::Registration>(registration) else {
            debug!(%plugin_id, "malformed PLUGIN-REGISTER");
            return Ok(Flow::Continue);
        };

        // The registration must self-identify as the authenticated plugin (a plugin
        // can only speak for its own id).
        if reg.id != plugin_id {
            debug!(%plugin_id, claimed = %reg.id, "plugin id mismatch in PLUGIN-REGISTER");
            return Ok(Flow::Continue);
        }

        self.ctx.register_plugin(
            plugin_id.to_string(),
            self.fed_out_tx.clone(),
            reg.name,
            reg.icon,
            reg.actions,
        );
        info!(%plugin_id, "remote plugin registered");
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
