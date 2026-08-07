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
    /// The URL the **homeserver** calls us on, as it appears in the appservice
    /// registration. Distinct from `listen`: we bind `0.0.0.0:9010`, but the
    /// homeserver has to dial a name it can resolve (`http://bridge:9010` on a
    /// Compose network). Defaults to `http://<listen>`, which is right only when
    /// both run in one place.
    #[serde(default)]
    pub as_url: Option<String>,
    /// The companion homeserver's server name — puppets live under it.
    pub domain: String,
    /// Puppet localpart prefix (registration namespace `@<prefix>.*`).
    #[serde(default = "default_prefix")]
    pub puppet_prefix: String,
    /// The appservice bot's localpart.
    #[serde(default = "default_bot")]
    pub bot: String,
    /// MXIDs allowed to drive the bot console (`!weft …`). A **config
    /// allowlist**, not a Matrix power level: power in a room says what you may
    /// do to that room, not who may re-point this bridge's state. Empty (the
    /// default) disables the console.
    #[serde(default)]
    pub admins: Vec<String>,
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

    /// The URL to put in the registration — what the homeserver will call.
    pub fn as_url(&self) -> String {
        self.matrix
            .as_url
            .clone()
            .unwrap_or_else(|| format!("http://{}", self.matrix.listen))
    }
}

/// The appservice registration the operator installs on the companion
/// homeserver (`weft-matrix generate-registration`).
pub fn registration_yaml(cfg: &Config, url: &str) -> String {
    let regex = format!(
        "@{}.*:{}",
        cfg.matrix.puppet_prefix,
        regex_escape(&cfg.matrix.domain)
    );

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
      regex: {regex}
  aliases: []
  rooms: []
"#,
        url = yaml_quoted(url),
        as_token = yaml_quoted(&cfg.matrix.as_token),
        hs_token = yaml_quoted(&cfg.matrix.hs_token),
        bot = yaml_quoted(&cfg.matrix.bot),
        regex = yaml_quoted(&regex),
    )
}

/// Wrap a value as a YAML **single-quoted** scalar.
///
/// Single, not double, because of the regex: a domain escaped for the regex
/// engine contains `\.`, which is an *invalid escape* inside a YAML double-quoted
/// scalar — Synapse then refuses to load the registration with
/// `found unknown escape character '.'`. Single quotes disable escape processing
/// entirely; only a literal `'` needs doubling. The other fields are quoted the
/// same way because they are operator-supplied: a token beginning with `*`, `&`
/// or `!` would otherwise be a YAML indicator rather than a string.
fn yaml_quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
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

        // Absent `as_url` ⇒ the bind address, which is only right when the
        // homeserver shares our network namespace.
        assert_eq!(cfg.as_url(), "http://127.0.0.1:9010");

        let reg = registration_yaml(&cfg, &cfg.as_url());
        assert!(reg.contains("sender_localpart: 'weftbot'"), "{reg}");
        assert!(reg.contains("url: 'http://127.0.0.1:9010'"), "{reg}");

        // Single-quoted, NOT double: the regex carries `\.` from escaping the
        // domain, and `\.` inside a YAML double-quoted scalar is an invalid escape
        // — Synapse rejects the whole registration with "found unknown escape
        // character '.'". Guard the class of bug too: we emit no double quotes at
        // all, so no interpolated value can land somewhere escapes are processed.
        assert!(reg.contains(r"regex: '@weft_.*:test\.example'"), "{reg}");
        assert!(!reg.contains('"'), "{reg}");
    }
}
