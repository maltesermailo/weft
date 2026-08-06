//! `weft-matrix.toml` — everything the daemon needs to stand between the two
//! systems. Modeled on `docs/architecture/matrix.md` §16.

use std::path::PathBuf;

use anyhow::Context as _;

#[derive(Debug, serde::Deserialize)]
pub struct Config {
    pub weft: Weft,
    pub matrix: Matrix,
    pub daemon: Daemon,
}

#[derive(Debug, serde::Deserialize)]
pub struct Weft {
    /// weftd's QUIC endpoint, `host:port`.
    pub endpoint: String,
    /// PEM/base64 Ed25519 signing key file — the key pinned in weftd's
    /// `[[plugin.remote]]`. Generated on first run if absent.
    pub key_file: PathBuf,
    /// weftd's HTTP media base, e.g. `https://weft.example` — §13's data plane
    /// (`POST /media`, `GET /media/<hash>`) is HTTP, not the control stream.
    /// Absent ⇒ media is not bridged, and the daemon says so once.
    #[serde(default)]
    pub media_url: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
pub struct Matrix {
    /// The companion homeserver's client-server API base, e.g.
    /// `http://127.0.0.1:6167`.
    pub hs_url: String,
    /// Our token *to* the homeserver (appservice registration `as_token`).
    pub as_token: String,
    /// The homeserver's token to *us* (`hs_token`).
    pub hs_token: String,
    /// Where the homeserver reaches our appservice API, bind address.
    pub listen: String,
    /// The companion homeserver's server name — puppets live under it.
    pub domain: String,
    /// Puppet localpart prefix (registration namespace `@<prefix>.*`).
    #[serde(default = "default_prefix")]
    pub puppet_prefix: String,
    /// The appservice bot's localpart.
    #[serde(default = "default_bot")]
    pub bot: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct Daemon {
    /// The daemon's Postgres store (`matrix_`-prefixed tables — its own
    /// database, or weftd's, without clashing).
    pub database_url: String,
}

fn default_prefix() -> String {
    "weft_".to_string()
}

fn default_bot() -> String {
    "weftbot".to_string()
}

impl Config {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn bot_mxid(&self) -> String {
        format!("@{}:{}", self.matrix.bot, self.matrix.domain)
    }
}

/// The appservice registration the operator installs on the companion
/// homeserver (`weft-matrix generate-registration`).
pub fn registration_yaml(cfg: &Config, url: &str) -> String {
    format!(
        r#"id: weft-matrix
url: {url}
as_token: {as_token}
hs_token: {hs_token}
sender_localpart: {bot}
rate_limited: false
namespaces:
  users:
    - exclusive: true
      regex: "@{prefix}.*:{domain}"
  aliases: []
  rooms: []
"#,
        as_token = cfg.matrix.as_token,
        hs_token = cfg.matrix.hs_token,
        bot = cfg.matrix.bot,
        prefix = cfg.matrix.puppet_prefix,
        domain = regex_escape(&cfg.matrix.domain),
    )
}

/// Escape a literal domain for the registration's regex field.
fn regex_escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                vec![c]
            } else {
                vec!['\\', c]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minimal_config_parses_with_defaults() {
        let cfg: Config = toml::from_str(
            r#"
            [weft]
            endpoint = "127.0.0.1:9000"
            key_file = "adapter.key"

            [matrix]
            hs_url = "http://127.0.0.1:6167"
            as_token = "as"
            hs_token = "hs"
            listen = "127.0.0.1:9010"
            domain = "test.example"

            [daemon]
            database_url = "postgres://weft:weft@localhost/weft"
            "#,
        )
        .unwrap();

        assert_eq!(cfg.matrix.puppet_prefix, "weft_");
        assert_eq!(cfg.bot_mxid(), "@weftbot:test.example");

        let reg = registration_yaml(&cfg, "http://127.0.0.1:9010");
        assert!(reg.contains("regex: \"@weft_.*:test\\.example\""), "{reg}");
        assert!(reg.contains("sender_localpart: weftbot"), "{reg}");
    }
}
