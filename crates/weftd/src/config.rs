//! Server configuration (TOML). Everything has a dev-friendly default so
//! `weftd` with no arguments starts a localhost network.

use std::net::SocketAddr;
use std::path::Path;

use anyhow::Context;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// This network's DNS name — the identity everything is scoped to.
    pub network: String,
    /// WELCOME trailing text (§3.6).
    pub motd: Option<String>,
    /// The static channel set (M1: JOIN never auto-creates and CHANNEL
    /// CREATE is M4, so channels exist only by being listed here).
    pub channels: Vec<String>,
    pub listen: Listen,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Listen {
    /// QUIC (native transport, ALPN `weft/1`).
    pub quic: SocketAddr,
    /// WebSocket fallback; `None` disables it.
    pub ws: Option<SocketAddr>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            network: "localhost".to_string(),
            motd: None,
            channels: vec!["#general".to_string()],
            listen: Listen::default(),
        }
    }
}

impl Default for Listen {
    fn default() -> Self {
        Self {
            quic: ([127, 0, 0, 1], 4433).into(),
            ws: None,
        }
    }
}

pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Config> {
    let path = path.as_ref();
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("parsing config {}", path.display()))
}
