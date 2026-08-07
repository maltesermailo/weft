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
    /// `tracing` log filter (`EnvFilter` syntax): a bare level (`"debug"`) or
    /// per-target directives (`"info,weft_core=debug"`). Default `"info"`.
    /// `RUST_LOG` in the environment overrides this when set.
    pub log: String,
    /// WELCOME trailing text (§3.6).
    pub motd: Option<String>,
    /// The static channel set (JOIN never auto-creates and CHANNEL CREATE
    /// is M4, so channels exist only by being listed here). Entries are a
    /// bare name (`"#general"`, default policy `retained:90d` per §6.3) or
    /// `{ name = "#logs", policy = "ephemeral" }`.
    pub channels: Vec<ChannelConfig>,
    /// §6.1: REGISTER works only when `open`.
    pub registration: Registration,
    /// §6.1 require a contact email at REGISTER (verify-later, §10.5) — which
    /// also enables password reset. Off by default. Turning it on **requires
    /// `[smtp]` to be configured** (a reset code must be deliverable); weftd
    /// refuses to boot otherwise. The WEFT-IRC gateway is exempt (it
    /// auto-registers emailless accounts, which can't password-reset).
    #[serde(default)]
    pub require_email: bool,
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
    /// Foreign-bridge framework (§3): pinned adapter daemons (`[[foreign_bridge]]`).
    #[serde(default)]
    pub foreign_bridge: Vec<ForeignBridge>,
    /// Plugin system (`docs/architecture/plugin-spec.md`): the `[plugin]` section
    /// (`[[plugin.remote]]` App Services). Reserved in M-plug-0; consumed M-plug-2+.
    #[serde(default)]
    pub plugin: Plugin,
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
    /// Max concurrent client sessions across all client transports (QUIC + WS +
    /// IRC combined). A new connection past the cap is refused immediately, not
    /// queued — this bounds total memory/threads (threat-model D-2). Bridge and
    /// data-plane streams are separate and not counted. Default 1024.
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
    /// Server-side link-preview (unfurl) proxy. When enabled, `/unfurl` fetches
    /// a user-supplied URL server-side (SSRF-guarded) and returns its OpenGraph
    /// preview, so clients don't leak the viewer's IP to arbitrary hosts.
    pub unfurl: Unfurl,
    /// §6.7 moderation: a network-wide **support account** that holds a namespace
    /// after an operator "seize to support". Provisioned suspended at boot (it can
    /// never log in), it exists only to own seized communities for moderation.
    /// `None` disables the feature. Example: `support_account = "support"`.
    #[serde(default)]
    pub support_account: Option<String>,
    /// §6.7 path to a `banned-words.toml` (`words = ["...", ...]`) whose entries
    /// are refused (case-insensitive substring) in new **usernames** and
    /// **namespace vanities**. Relative paths resolve against the weftd.toml dir.
    /// `None` = no filter. Default file name: `banned-words.toml`.
    #[serde(default)]
    pub banned_words_file: Option<String>,
}

fn default_max_connections() -> usize {
    1024
}

/// `[unfurl]` — the link-preview proxy (§13 data plane, HTTP surface).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Unfurl {
    /// On by default; set false to refuse all server-side link fetching.
    pub enabled: bool,
}

impl Default for Unfurl {
    fn default() -> Self {
        Self { enabled: true }
    }
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

/// Foreign-bridge framework (`docs/architecture/foreign-bridge-framework.md` §3):
/// a pinned adapter daemon, authorized for one scheme. The adapter proves control
/// of `key` at `AUTH ADAPTER`; `REALM REGISTER`/`ASSERT` for `scheme` then require
/// that same key. Multiple `[[foreign_bridge]]` entries allow several adapters (or
/// one adapter spanning several schemes with repeated entries).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignBridge {
    /// Adapter protocol scheme, e.g. `matrix`.
    pub scheme: String,
    /// Adapter's signing key, base64.
    pub key: String,
}

/// Plugin system (`docs/architecture/plugin-spec.md`). M-plug-0 reserves the
/// `[plugin]` schema slot; the remote-plugin session router that consumes it
/// arrives with M-plug-2.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Plugin {
    /// Remote plugins / App Services — pinned-key external processes (§3.1, §4.2),
    /// mirroring `[[foreign_bridge]]`. `[[plugin.remote]]` entries.
    pub remote: Vec<PluginRemote>,
}

/// A remote plugin / App Service (spec §14): a pinned key the process proves at
/// `AUTH ADAPTER`, plus its id and optional bot + config.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginRemote {
    /// Plugin id — the catalog/route key.
    pub id: String,
    /// Pinned Ed25519 signing key, base64.
    pub key: String,
    /// Optional bot account to provision + attribute (§9).
    #[serde(default)]
    pub bot: Option<String>,
    /// Foreign-URI schemes this provider may provision (§18 capability 6) — e.g.
    /// `["instagram"]` for a bridge-style plugin. Empty = none.
    #[serde(default)]
    pub schemes: Vec<String>,
    /// Config keys delivered to the plugin at connect; secrets as `"env:X"` or
    /// inline (redacted where weftd surfaces them, §14).
    #[serde(default)]
    pub config: std::collections::HashMap<String, String>,
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
    /// Dedicated **admin panel** listener (plaintext); `None` = the admin
    /// merges into the shared `http`/`https` app. Set it to move the panel (and
    /// the `/media` it renders) onto its own port so it can be firewalled off
    /// the public HTTP surface — bind it to a private/VPN interface. Front it
    /// with TLS yourself.
    pub admin: Option<SocketAddr>,
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
            log: "info".to_string(),
            motd: None,
            channels: vec![ChannelConfig::Name("#general".to_string())],
            registration: Registration::Open,
            require_email: false,
            operators: Vec::new(),
            namespaces: Namespaces::default(),
            federation: Federation::default(),
            peers: Vec::new(),
            foreign_bridge: Vec::new(),
            plugin: Plugin::default(),
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
            max_connections: default_max_connections(),
            unfurl: Unfurl::default(),
            support_account: None,
            banned_words_file: None,
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
            admin: None,
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

/// Substitute every `${VAR}` in the config text from the environment.
///
/// So a secret can live in one place instead of being hand-copied into every
/// file that needs it: the Docker deployment keeps the Postgres password in
/// Compose's `.env` and writes `${POSTGRES_PASSWORD}` here. An unset or empty
/// variable is a hard error, never an empty string — booting with a silently
/// passwordless connection string is worse than not booting.
fn expand_env(raw: &str, origin: &Path) -> anyhow::Result<String> {
    let mut out = String::with_capacity(raw.len());

    for (index, line) in raw.split_inclusive('\n').enumerate() {
        // A whole-line comment is prose, not configuration. Expanding it would let
        // a sentence that merely *mentions* `${SOMETHING}` — like the ones
        // documenting this feature — refuse the boot.
        if line.trim_start().starts_with('#') {
            out.push_str(line);
            continue;
        }

        expand_line(line, origin, index + 1, &mut out)?;
    }

    Ok(out)
}

/// One line's worth of substitution, appended to `out`.
fn expand_line(
    line: &str,
    origin: &Path,
    line_number: usize,
    out: &mut String,
) -> anyhow::Result<()> {
    let mut rest = line;

    while let Some(open) = rest.find("${") {
        out.push_str(&rest[..open]);
        rest = &rest[open..];

        // Only a well-formed `${IDENTIFIER}` is a reference; a `${` with no closing
        // brace, or a name no environment variable could have, is literal text.
        let name = rest
            .strip_prefix("${")
            .and_then(|r| r.split_once('}'))
            .map(|(name, _)| name)
            .filter(|name| is_env_name(name));

        let Some(name) = name else {
            out.push_str("${");
            rest = &rest[2..];
            continue;
        };

        let value = std::env::var(name)
            .ok()
            .filter(|v| !v.is_empty())
            .with_context(|| {
                format!(
                    "{}:{line_number} references `${{{name}}}`, which is unset or empty \
                     in the environment",
                    origin.display()
                )
            })?;
        // Substituted text is never re-scanned: a password that happens to contain
        // `${` is data, not another reference.
        out.push_str(&value);

        rest = &rest[name.len() + 3..];
    }

    out.push_str(rest);

    Ok(())
}

/// A POSIX-shaped environment-variable name.
fn is_env_name(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Config> {
    let path = path.as_ref();
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    let raw = expand_env(&raw, path)?;

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

    /// `${VAR}` expansion is what lets the Docker deployment keep the Postgres
    /// password in `.env` alone instead of hand-copied into every file wanting it.
    #[test]
    fn env_references_expand_and_missing_ones_are_refused() {
        let path = std::path::Path::new("test.toml");

        std::env::set_var("WEFT_TEST_PG_PASSWORD", "s3cr${et}");
        let expanded = super::expand_env(
            r#"url = "postgres://weft:${WEFT_TEST_PG_PASSWORD}@postgres/weft""#,
            path,
        )
        .expect("a set variable expands");
        // The substituted value is not re-scanned: the `${et}` inside it is data.
        assert_eq!(
            expanded,
            r#"url = "postgres://weft:s3cr${et}@postgres/weft""#
        );

        // An unset — or empty — variable must never become an empty password.
        std::env::set_var("WEFT_TEST_PG_BLANK", "");
        for raw in ["${WEFT_TEST_PG_ABSENT}", "${WEFT_TEST_PG_BLANK}"] {
            let err = super::expand_env(raw, path).expect_err("must refuse");
            assert!(format!("{err:#}").contains("unset or empty"), "{err:#}");
        }

        // A whole-line comment is exempt entirely. Not hypothetical: the comment
        // documenting this feature in deploy/weftd/weft-matrix.toml said `${VAR}`,
        // a perfectly good variable name, and weftd refused to start because of a
        // sentence. `${PATH}` is the sharp case — it is always set, so without the
        // exemption a comment would be silently rewritten rather than error.
        for comment in ["# use ${SECRET} here", "  # see ${VAR} above", "# ${PATH}"] {
            assert_eq!(super::expand_env(comment, path).unwrap(), comment);
        }

        // Outside a comment, a malformed reference is still literal text.
        for literal in ["a = \"${...}\"", "a = \"bare ${ is fine\"", "${no-dash}"] {
            assert_eq!(super::expand_env(literal, path).unwrap(), literal);
        }
    }

    /// M-plug-0: the `[[plugin.remote]]` schema slot parses (an operator can
    /// declare a remote plugin before the M-plug-2 router consumes it).
    #[test]
    fn plugin_remote_config_parses() {
        let raw = r#"
            network = "weft.example"
            [[plugin.remote]]
            id = "jira-bot"
            key = "Zm9vYmFy"
            bot = "jira"
            [plugin.remote.config]
            api_key = "env:JIRA_TOKEN"
        "#;
        let cfg = toml::from_str::<super::Config>(raw).expect("plugin.remote must parse");
        let remote = &cfg.plugin.remote[0];
        assert_eq!(remote.id, "jira-bot");
        assert_eq!(remote.bot.as_deref(), Some("jira"));
        assert_eq!(
            remote.config.get("api_key").map(String::as_str),
            Some("env:JIRA_TOKEN")
        );
    }
}
