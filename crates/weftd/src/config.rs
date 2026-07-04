//! Server configuration (TOML). Everything has a dev-friendly default so
//! `weftd` with no arguments starts a localhost network.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// This network's DNS name — the identity everything is scoped to.
    pub network: String,
    /// WELCOME trailing text (§3.6).
    pub motd: Option<String>,
    /// The static channel set (JOIN never auto-creates and CHANNEL CREATE
    /// is M4, so channels exist only by being listed here). Entries are a
    /// bare name (`"#general"`, default policy `retained:90d` per §6.3) or
    /// `{ name = "#logs", policy = "ephemeral" }`.
    pub channels: Vec<ChannelConfig>,
    /// §6.1: REGISTER works only when `open`.
    pub registration: Registration,
    /// §9.5: one retention policy for all DMs (default `permanent`).
    pub dm_policy: String,
    pub listen: Listen,
    pub identity: Identity,
    pub storage: Storage,
    /// TLS identity for QUIC. Absent → fresh self-signed (dev only).
    pub tls: Option<Tls>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ChannelConfig {
    Name(String),
    Detailed { name: String, policy: String },
}

impl ChannelConfig {
    pub fn name(&self) -> &str {
        match self {
            ChannelConfig::Name(name) => name,
            ChannelConfig::Detailed { name, .. } => name,
        }
    }

    /// §6.3: CHANNEL CREATE defaults to `retained:90d`.
    pub fn policy(&self) -> &str {
        match self {
            ChannelConfig::Name(_) => "retained:90d",
            ChannelConfig::Detailed { policy, .. } => policy,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Registration {
    #[default]
    Open,
    Closed,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Listen {
    /// QUIC (native transport, ALPN `weft/1`).
    pub quic: SocketAddr,
    /// WebSocket fallback; `None` disables it.
    pub ws: Option<SocketAddr>,
    /// HTTP for `/.well-known/weft` (§10.2); `None` disables it.
    pub http: Option<SocketAddr>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Identity {
    /// Network signing key location (base64 seed, one line). Created on
    /// first boot if missing; `None` = ephemeral key (tests/dev).
    pub key_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tls {
    pub cert: PathBuf,
    pub key: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Storage {
    pub backend: StorageBackend,
    /// PostgreSQL connection URL (required for `backend = "postgres"`).
    pub url: Option<String>,
    /// Retention purge + compaction cadence.
    pub maintenance_interval_secs: u64,
    /// §12.1 `compact-after` audit window.
    pub compact_after_hours: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageBackend {
    /// In-memory: DB-less dev/test; nothing survives a restart.
    #[default]
    Memory,
    Postgres,
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            backend: StorageBackend::Memory,
            url: None,
            maintenance_interval_secs: 300,
            compact_after_hours: 24,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            network: "localhost".to_string(),
            motd: None,
            channels: vec![ChannelConfig::Name("#general".to_string())],
            registration: Registration::Open,
            dm_policy: "permanent".to_string(),
            listen: Listen::default(),
            identity: Identity::default(),
            storage: Storage::default(),
            tls: None,
        }
    }
}

impl Default for Listen {
    fn default() -> Self {
        Self {
            quic: ([127, 0, 0, 1], 4433).into(),
            ws: None,
            http: None,
        }
    }
}

impl Default for Identity {
    fn default() -> Self {
        Self {
            key_file: Some(PathBuf::from("weftd.key")),
        }
    }
}

pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Config> {
    let path = path.as_ref();
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("parsing config {}", path.display()))
}
