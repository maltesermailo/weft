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
    /// §11.3: operator accounts hold every capability at `*` (the network
    /// key's authority) — they bootstrap the grant chain.
    pub operators: Vec<String>,
    /// §9.5: one retention policy for all DMs (default `permanent`).
    pub dm_policy: String,
    /// §2.2 namespace creation policy.
    pub namespaces: Namespaces,
    /// §11 federation policy (inbound bridge behavior).
    pub federation: Federation,
    /// §11.2 pinned peers weftd dials outbound (`[[peers]]`).
    #[serde(default)]
    pub peers: Vec<Peer>,
    pub listen: Listen,
    pub identity: Identity,
    pub storage: Storage,
    /// TLS identity for QUIC. Absent → fresh self-signed (dev only). A file
    /// cert is hot-reloaded when it changes on disk (renewals apply without a
    /// restart) — pair it with a front proxy / certbot that renews the file.
    pub tls: Option<Tls>,
    /// Built-in ACME (Let's Encrypt). When enabled, weftd obtains + renews its
    /// own certificate and uses it for QUIC — no front proxy needed. Takes
    /// precedence over `[tls]`.
    pub acme: Acme,
    /// Operator web admin panel. When enabled, weftd mounts the `weft-admin`
    /// API on the HTTP listener (`/admin/api/*`); operators are `[operators]`.
    pub admin: Admin,
}

/// Embedded admin panel toggle. (Standalone `weft-admin` has its own config.)
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Admin {
    pub enabled: bool,
}

/// §10.2 built-in ACME. Validates over HTTP-01, so the HTTP listener
/// (`[listen] http`) must be reachable by the CA on port 80.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Acme {
    pub enabled: bool,
    /// Certificate domains (SANs). The first is the primary.
    pub domains: Vec<String>,
    /// Contact email for the ACME account (recommended).
    pub email: Option<String>,
    /// Use Let's Encrypt's staging endpoint (untrusted certs, high rate
    /// limits) while testing.
    pub staging: bool,
    /// Directory caching the ACME account key + issued cert/key.
    pub cache_dir: PathBuf,
}

impl Default for Acme {
    fn default() -> Self {
        Self {
            enabled: false,
            domains: Vec::new(),
            email: None,
            staging: false,
            cache_dir: PathBuf::from("acme"),
        }
    }
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

/// §2.2 namespace creation: `open` (any account, up to `quota`) or `gated`
/// (needs the `ns-create` cap).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Namespaces {
    pub creation: NsCreation,
    pub quota: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NsCreation {
    #[default]
    Open,
    Gated,
}

impl Default for Namespaces {
    fn default() -> Self {
        Self {
            creation: NsCreation::Open,
            quota: 10, // §2.2 default quota
        }
    }
}

/// §11 federation policy. Controls how this network treats *inbound* bridge
/// sessions; outbound dialing is driven by `[[peers]]`. By default a network
/// bridges with nobody; `accept_any` opens it to any peer (trust-on-first-use,
/// §11.2), and `auto_accept` skips the manual `BRIDGE ACCEPT` step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Federation {
    /// Accept a bridge from any non-blocked network, trusting the key it
    /// proves control of. `NETBLOCK` remains the escape hatch.
    pub accept_any: bool,
    /// Auto-accept incoming `BRIDGE PROPOSE` instead of requiring an operator
    /// `BRIDGE ACCEPT`.
    pub auto_accept: bool,
    /// §11.10 on-demand outbound bridging when a user references a foreign
    /// namespace. `off` = only manual/pinned peering; `open` = any member may
    /// trigger an auto-bridge to any non-blocked, SSRF-safe network.
    pub auto_bridge: AutoBridge,
}

/// §11.10 outbound auto-federation policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AutoBridge {
    #[default]
    Off,
    Open,
}

/// §11.2 A pinned peer network weftd dials outbound (M5d). Its `key` is pinned:
/// the peer must prove control of it, and it verifies the peer's manifests.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Peer {
    /// Peer network name (DNS), e.g. `hda.example`.
    pub network: String,
    /// `host:port` to dial over QUIC (UDP).
    pub endpoint: String,
    /// Peer's network signing key, base64.
    pub key: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Listen {
    /// QUIC (native transport, ALPN `weft/1`).
    pub quic: SocketAddr,
    /// WebSocket fallback; `None` disables it.
    pub ws: Option<SocketAddr>,
    /// HTTP for `/.well-known/weft` (§10.2) + the ACME HTTP-01 challenge;
    /// `None` disables it. Plaintext — front it or use `https` for the admin.
    pub http: Option<SocketAddr>,
    /// HTTPS (TLS-terminated) for the well-known + admin panel, using the same
    /// cert as QUIC (ACME / file / self-signed); `None` disables it. This is how
    /// the admin panel is served securely without a front proxy.
    pub https: Option<SocketAddr>,
    /// WEFT-IRC gateway (§17); `None` disables it. Conventionally :6667
    /// (plaintext) or :6697 (TLS — TLS termination is the operator's).
    pub irc: Option<SocketAddr>,
    /// Serve the browser client (P3 web embed) + a same-origin `/ws` WebSocket
    /// on the existing `http`/`https` listener. The SPA itself is only present
    /// when built with `--features web-ui`; without it, only `/ws` mounts.
    pub web: bool,
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
            operators: Vec::new(),
            namespaces: Namespaces::default(),
            federation: Federation::default(),
            peers: Vec::new(),
            dm_policy: "permanent".to_string(),
            listen: Listen::default(),
            identity: Identity::default(),
            storage: Storage::default(),
            tls: None,
            acme: Acme::default(),
            admin: Admin::default(),
        }
    }
}

impl Default for Listen {
    fn default() -> Self {
        Self {
            quic: ([127, 0, 0, 1], 4433).into(),
            ws: None,
            http: None,
            https: None,
            irc: None,
            web: false,
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

#[cfg(test)]
mod tests {
    /// The shipped example config must always parse against the live schema
    /// (`deny_unknown_fields` makes any drift a hard failure).
    #[test]
    fn example_config_parses() {
        let raw = include_str!("../../../weftd.example.toml");
        toml::from_str::<super::Config>(raw).expect("weftd.example.toml must parse");
    }
}
