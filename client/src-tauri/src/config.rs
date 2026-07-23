//! Client configuration file (`client.toml` in the OS app-config dir).
//!
//! Real distribution verifies the server's TLS certificate by default. The
//! only escape hatch — for LAN / self-signed / dev servers — is a config file
//! the user (or an admin) writes:
//!
//! ```toml
//! # ~/Library/Application Support/<identifier>/client.toml   (macOS)
//! # ~/.config/<identifier>/client.toml                        (Linux)
//! # %APPDATA%\<identifier>\client.toml                        (Windows)
//! allow_insecure = true          # accept self-signed / unverified certs
//! default_host   = "127.0.0.1:4433"
//! media_base     = "http://127.0.0.1:8080"   # dev only; see below
//! ```
//!
//! `media_base` is the HTTP origin serving §13 `/media` — weftd's axum listener,
//! which is a *different* port from the QUIC control plane. In a normal
//! deployment the network's DNS name serves it over HTTPS, so the client derives
//! `https://<host>` from the connect host and this stays unset. Set it only when
//! the media endpoint isn't at `https://<host>` (dev, LAN, a reverse proxy on a
//! nonstandard port).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ClientConfig {
    /// Accept self-signed / unverified server certificates. **Off by default** —
    /// only turn this on for a server you control (dev, LAN, self-signed).
    pub allow_insecure: bool,
    /// Optional host to prefill the connect screen.
    pub default_host: Option<String>,
    /// Override the HTTP origin for §13 media (upload + fetch). Unset ⇒ derive
    /// `https://<connect-host>`, which is correct whenever the network's DNS
    /// name fronts weftd's HTTP listener.
    pub media_base: Option<String>,
}

/// `<app-config-dir>/client.toml`, if the platform exposes a config dir.
pub fn path(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .app_config_dir()
        .ok()
        .map(|dir| dir.join("client.toml"))
}

/// Load the config, falling back to secure defaults if it's missing or invalid.
pub fn load(app: &AppHandle) -> ClientConfig {
    let Some(path) = path(app) else {
        return ClientConfig::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(raw) => toml::from_str(&raw).unwrap_or_default(),
        Err(_) => ClientConfig::default(),
    }
}
