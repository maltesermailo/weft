//! QUIC TLS certificate provisioning + hot-reload.
//!
//! One resolver ([`weft_transport::ReloadableCert`]) feeds the QUIC endpoint;
//! an optional background task keeps it fresh. Three sources, one resolver:
//!
//! * `[acme]`  — built-in Let's Encrypt: obtain + renew (HTTP-01). [`acme`]
//! * `[tls]`   — a PEM file, reloaded when its mtime changes (certbot/Caddy).
//! * neither   — a fresh self-signed cert (dev only).

mod acme;

use std::collections::HashMap;
use std::io::BufReader;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use anyhow::Context;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};
use weft_transport::ReloadableCert;

use crate::config::Config;

/// Pending HTTP-01 challenge responses (`token → key-authorization`), served by
/// the well-known router while ACME validates. Empty in non-ACME modes.
pub type Challenges = Arc<RwLock<HashMap<String, String>>>;

/// Build the QUIC cert resolver + any background refresh task.
pub async fn setup(
    config: &Config,
    network: &weft_proto::NetworkName,
    challenges: Challenges,
) -> anyhow::Result<(Arc<ReloadableCert>, Option<JoinHandle<()>>)> {
    if config.acme.enabled {
        return acme::setup(config, challenges).await;
    }
    if let Some(tls) = &config.tls {
        let resolver = ReloadableCert::new(load_pem(&tls.cert, &tls.key)?);
        let task = spawn_file_watch(Arc::clone(&resolver), tls.cert.clone(), tls.key.clone());
        info!(cert = %tls.cert.display(), "TLS: file certificate (hot-reload on change)");
        return Ok((resolver, Some(task)));
    }
    warn!("TLS: self-signed certificate (dev only — clients need allow_insecure)");
    let sans = vec![network.as_str().to_string(), "localhost".to_string()];
    Ok((ReloadableCert::new(self_signed(sans)?), None))
}

/// Parse a PEM cert chain + private key into the resolver's swap unit.
fn load_pem(cert: &Path, key: &Path) -> anyhow::Result<Arc<weft_transport::CertifiedKey>> {
    let mut reader = BufReader::new(
        std::fs::File::open(cert)
            .with_context(|| format!("opening certificate {}", cert.display()))?,
    );
    let chain: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<_, _>>()
        .context("parsing certificate PEM")?;
    anyhow::ensure!(!chain.is_empty(), "no certificates in {}", cert.display());

    let mut reader = BufReader::new(
        std::fs::File::open(key)
            .with_context(|| format!("opening private key {}", key.display()))?,
    );
    let key_der: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut reader)
        .context("parsing private key PEM")?
        .with_context(|| format!("no private key in {}", key.display()))?;

    weft_transport::certified_key(chain, key_der).map_err(Into::into)
}

/// Poll the cert file's mtime and reload on change (renewals apply without a
/// restart). A failed reload logs and keeps the current cert.
fn spawn_file_watch(
    resolver: Arc<ReloadableCert>,
    cert: std::path::PathBuf,
    key: std::path::PathBuf,
) -> JoinHandle<()> {
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    tokio::spawn(async move {
        let mut last: Option<SystemTime> = mtime(&cert);
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let now = mtime(&cert);
            if now == last {
                continue;
            }
            last = now;
            match load_pem(&cert, &key) {
                Ok(ck) => {
                    resolver.store(ck);
                    info!("TLS: certificate reloaded from disk");
                }
                Err(e) => error!("TLS: reload failed, keeping current certificate: {e:#}"),
            }
        }
    })
}

/// A fresh self-signed cert for the given SANs (dev, or an ACME placeholder
/// until the real cert is issued). Unverifiable by design.
fn self_signed(sans: Vec<String>) -> anyhow::Result<Arc<weft_transport::CertifiedKey>> {
    let cert =
        rcgen::generate_simple_self_signed(sans).context("generating self-signed certificate")?;
    weft_transport::certified_key(
        vec![cert.cert.der().clone()],
        PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into()),
    )
    .map_err(Into::into)
}
