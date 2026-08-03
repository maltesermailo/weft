//! # weft-appservice — the WEFT App-Service SDK
//!
//! The base for building a `remote` plugin (an **App Service**, the Matrix-style
//! external process) against a WEFT server — and the foundation the Matrix bridge
//! is built on (`docs/architecture/plugin-spec.md` §3.5). It handles the pinned-key
//! `AUTH ADAPTER` handshake, registration, the dispatch loop, and the `Ctx`
//! act-as-service / SDUI surface, so an author writes only handlers + logic.
//!
//! This is a **client library** (a sibling of `weft-tui`): it depends on the wire
//! codec + transport + crypto, never on `weft-core`/`weftd`/`weft-store`. An App
//! Service is an external process, not the server.
//!
//! **M-plug-0 — skeleton.** The [`AppService::builder`] shape exists; the wire
//! registration (`.action`/`.hook`, M-plug-1) and the connection + dispatch loop
//! (`.run`, M-plug-2) are stubbed. The `bridge` feature (realm/provisioning
//! helpers) arrives with M-plug-11.

#![forbid(unsafe_code)]

use weft_crypto::Keypair;

/// Entry point: `AppService::builder(...)` starts configuring a service.
pub struct AppService;

impl AppService {
    /// Start configuring an App Service that authenticates to `endpoint` as
    /// plugin `id`, proving control of `keypair` (its pinned key, §4.2).
    pub fn builder(
        endpoint: impl Into<String>,
        keypair: Keypair,
        id: impl Into<String>,
    ) -> AppServiceBuilder {
        AppServiceBuilder {
            endpoint: endpoint.into(),
            keypair,
            id: id.into(),
            bot: None,
        }
    }
}

/// Fluent configuration for an App Service. Handler registration
/// (`.action`/`.hook`) lands with the M-plug-1 wire types.
pub struct AppServiceBuilder {
    endpoint: String,
    keypair: Keypair,
    id: String,
    /// Optional bot account to request weftd provision (§9); `[[plugin.remote]]`
    /// must authorize it.
    bot: Option<String>,
}

impl AppServiceBuilder {
    /// Request that weftd provision + attribute a bot account for this service.
    pub fn bot(mut self, account: impl Into<String>) -> Self {
        self.bot = Some(account.into());
        self
    }

    /// Connect, run the `AUTH ADAPTER` handshake, send registrations, and pump the
    /// dispatch loop until shutdown. **Stub in M-plug-0** — the transport +
    /// dispatch land in M-plug-2. The carried config is referenced here so the
    /// skeleton reflects what the real connection consumes.
    pub async fn run(self) -> anyhow::Result<()> {
        let _bot = self.bot.as_deref();
        let _key = self.keypair.public();
        anyhow::bail!(
            "weft-appservice: run() for '{}' → {} lands in M-plug-2 (remote transport)",
            self.id,
            self.endpoint,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_shapes_up() {
        let svc = AppService::builder("weft.example:9000", Keypair::generate(), "welcome-bot")
            .bot("welcome");

        assert_eq!(svc.endpoint, "weft.example:9000");
        assert_eq!(svc.id, "welcome-bot");
        assert_eq!(svc.bot.as_deref(), Some("welcome"));
    }
}
