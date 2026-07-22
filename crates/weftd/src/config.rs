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
    /// §11.3 **deprecated** — operator status now lives in Postgres, managed
    /// with `weftd admin` (create/grant/revoke/list). Any accounts still listed
    /// here are treated as operators (a compat seed), but prefer the CLI and
    /// remove this list. Operators hold every capability at `*`.
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
    /// §13 media blob storage.
    pub media: Media,
    /// §16 voice SFU (off by default).
    pub voice: Voice,
    /// §10.5 outbound SMTP for account (email) verification. Disabled → the
    /// server records claims and logs the code (dev) but sends no mail.
    pub smtp: Smtp,
}

/// §10.5 SMTP submission for verification emails. weftd connects out to this
/// server (STARTTLS on 587 by default) to deliver one-time codes.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Smtp {
    /// Send verification email (also needs `host`/`from`). Off → log-only.
    pub enabled: bool,
    /// SMTP submission host (e.g. `smtp.example.com`).
    pub host: String,
    /// Submission port (587 STARTTLS by default; 465 = implicit TLS).
    pub port: u16,
    /// Whether the port is implicit-TLS (465) rather than STARTTLS (587).
    pub implicit_tls: bool,
    /// SMTP AUTH username (empty = no auth, e.g. a local relay).
    pub username: String,
    /// SMTP AUTH password. Keep it out of logs.
    pub password: String,
    /// `From:` address on verification mail (e.g. `noreply@example.com`).
    pub from: String,
}

impl Default for Smtp {
    fn default() -> Self {
        Self {
            enabled: false,
            host: String::new(),
            port: 587,
            implicit_tls: false,
            username: String::new(),
            password: String::new(),
            from: String::new(),
        }
    }
}

/// Embedded admin panel toggle. (Standalone `weft-admin` has its own config.)
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Admin {
    pub enabled: bool,
    /// WC3 soft-delete grace window (days). An operator account delete is
    /// *scheduled* this many days out and is recoverable until then; the
    /// maintenance pass finalizes it. Default 7.
    pub delete_grace_days: u64,
}

impl Default for Admin {
    fn default() -> Self {
        Self {
            enabled: false,
            delete_grace_days: 7,
        }
    }
}

/// §13 media (content-addressed blobs). Fetched home-network-only; the data
/// plane rides QUIC + HTTP `/media`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Media {
    /// Filesystem directory for the content-addressed blob store. Unset =
    /// in-memory (ephemeral; pairs with `storage.backend = "memory"`).
    pub dir: Option<PathBuf>,
}

/// §16 voice. Two media planes select via `backend`:
/// - `native` — the embedded WEFT-RT SFU (compiled only with the `voice` build
///   feature; without it, `enabled = true` just logs a warning).
/// - `livekit` — hand `VOICE JOIN` a LiveKit access token for an external,
///   self-hosted LiveKit server (`[voice.livekit]`); needs no build feature.
///
/// `enabled = false` (the default) = a zero-voice, fully-conformant server.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Voice {
    /// Turn voice on (the `native` backend also needs the `voice` build feature).
    pub enabled: bool,
    /// Which media plane to use (`native` | `livekit`).
    pub backend: VoiceBackendKind,
    /// UDP port range the native SFU binds for media (host/srflx candidates).
    /// Open this range to the internet for voice to work behind NAT.
    pub udp_port_min: u16,
    pub udp_port_max: u16,
    /// STUN servers advertised to clients for server-reflexive candidates.
    pub stun: Vec<String>,
    /// LiveKit connection details (used only when `backend = "livekit"`).
    pub livekit: LiveKit,
}

/// The voice media plane. `native` = embedded WEFT-RT SFU; `livekit` = external
/// LiveKit server the client reaches with the SDK using a weftd-minted token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VoiceBackendKind {
    #[default]
    Native,
    Livekit,
}

/// LiveKit deployment (`[voice.livekit]`). weftd signs access tokens with
/// `api_secret` (HS256) — the same secret the operator gives their LiveKit — so
/// weftd + LiveKit share a trust boundary (both run by the operator).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LiveKit {
    /// LiveKit server URL handed to **clients** (`wss://livekit.example`) — the
    /// public address browsers/apps connect to.
    pub url: String,
    /// LiveKit **server-API** URL weftd itself calls for the Room API (mute/
    /// remove) — the internal address (e.g. `http://livekit:7880` in Docker).
    /// Empty → derived from `url` (scheme swapped). Set this when the public and
    /// internal addresses differ (a reverse proxy / container network).
    pub api_url: String,
    /// API key — the JWT `iss`.
    pub api_key: String,
    /// API secret — the HS256 signing key. Keep it out of logs.
    pub api_secret: String,
    /// Access-token lifetime (seconds); the client refreshes via `VOICE JOIN`.
    pub token_ttl_secs: u64,
}

impl Default for LiveKit {
    fn default() -> Self {
        Self {
            url: String::new(),
            api_url: String::new(),
            api_key: String::new(),
            api_secret: String::new(),
            token_ttl_secs: 600,
        }
    }
}

impl Default for Voice {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: VoiceBackendKind::Native,
            udp_port_min: 40000,
            udp_port_max: 40100,
            stun: vec!["stun:stun.l.google.com:19302".to_string()],
            livekit: LiveKit::default(),
        }
    }
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
    Detailed {
        name: String,
        #[serde(default = "default_channel_policy")]
        policy: String,
        /// §16 `"voice"` for a WEFT-RT voice channel; default `"text"`.
        #[serde(default)]
        kind: Option<String>,
    },
}

fn default_channel_policy() -> String {
    "retained:90d".to_string()
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

    /// §16 channel kind: `text` (default) or `voice`.
    pub fn kind(&self) -> &str {
        match self {
            ChannelConfig::Name(_) => "text",
            ChannelConfig::Detailed { kind, .. } => kind.as_deref().unwrap_or("text"),
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
            media: Media::default(),
            voice: Voice::default(),
            smtp: Smtp::default(),
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
