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
//! ```

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
