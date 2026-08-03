//! # weft-plugin — the WEFT plugin host (L3)
//!
//! The weftd-side machinery for the plugin / App-Service system
//! (`docs/architecture/plugin-spec.md`): it authenticates remote plugins, routes
//! the `PLUGIN` verbs, pushes events, and maintains the action catalog + SDUI
//! router. In-process Rhai/wasmtime engines arrive in later milestones
//! (M-plug-10+, M-plug-13) — this crate deliberately carries **no script engine**,
//! so the security-critical server never links one.
//!
//! **M-plug-0 — skeleton only.** [`Host`] is the placeholder that the SDUI/verb
//! codec (M-plug-1, in `weft-proto`) and the remote-transport router (M-plug-2)
//! build on. It holds no state yet.

#![forbid(unsafe_code)]

/// The weftd-side plugin host: owns the action catalog and the plugin-service
/// session router. Empty in M-plug-0; populated from M-plug-1 onward.
#[derive(Debug, Default)]
pub struct Host;

impl Host {
    /// Construct an empty host.
    pub fn new() -> Self {
        Host
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_constructs() {
        let _ = Host::new();
    }
}
