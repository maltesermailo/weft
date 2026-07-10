//! Built-in ACME (Let's Encrypt) over HTTP-01: obtain + auto-renew the QUIC
//! cert with no front proxy. Validation is served by the well-known HTTP
//! router (`[listen] http` must be reachable by the CA on port 80).
//!
//! The QUIC endpoint boots immediately on a cached (or self-signed placeholder)
//! cert; the real cert is swapped into the shared resolver once issued, and a
//! background loop renews it well before expiry.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, LetsEncrypt,
    NewAccount, NewOrder, OrderStatus,
};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};
use weft_transport::{CertifiedKey, ReloadableCert};

use super::Challenges;
use crate::config::{Acme, Config};

/// Renew this long after issuance (LE certs last 90 days; renew at ~60).
const RENEW_AFTER: Duration = Duration::from_secs(60 * 60 * 24 * 60);
/// Backoff between failed issuance attempts.
const RETRY_AFTER: Duration = Duration::from_secs(300);

pub async fn setup(
    config: &Config,
    challenges: Challenges,
) -> anyhow::Result<(Arc<ReloadableCert>, Option<JoinHandle<()>>)> {
    let acme = config.acme.clone();
    anyhow::ensure!(!acme.domains.is_empty(), "[acme] domains must be non-empty");
    std::fs::create_dir_all(&acme.cache_dir)
        .with_context(|| format!("creating acme cache dir {}", acme.cache_dir.display()))?;

    // Boot on the cached cert if we have one (avoids re-issuing on restart and
    // the LE duplicate-cert rate limit), else a self-signed placeholder.
    let initial = load_cached(&acme).unwrap_or_else(|| {
        super::self_signed(acme.domains.clone()).expect("placeholder self-signed cert")
    });
    let resolver = ReloadableCert::new(initial);

    info!(domains = ?acme.domains, staging = acme.staging, "TLS: built-in ACME");
    let task = tokio::spawn(renew_loop(acme, challenges, Arc::clone(&resolver)));
    Ok((resolver, Some(task)))
}

/// Obtain on boot if needed, then sleep until the next renewal — forever.
async fn renew_loop(acme: Acme, challenges: Challenges, resolver: Arc<ReloadableCert>) {
    loop {
        // If a fresh cached cert exists, wait out its remaining life first.
        if let Some(age) = cert_age(&acme) {
            if age < RENEW_AFTER {
                let wait = RENEW_AFTER - age;
                info!(?wait, "ACME: certificate current, next renewal scheduled");
                tokio::time::sleep(wait).await;
            }
        }
        match obtain(&acme, &challenges).await {
            Ok(key) => {
                resolver.store(key);
                info!("ACME: certificate issued + installed");
                tokio::time::sleep(RENEW_AFTER).await;
            }
            Err(e) => {
                error!("ACME: issuance failed, retrying in 5m: {e:#}");
                tokio::time::sleep(RETRY_AFTER).await;
            }
        }
    }
}

/// One full ACME order: account → authorize (HTTP-01) → finalize → download.
async fn obtain(acme: &Acme, challenges: &Challenges) -> anyhow::Result<Arc<CertifiedKey>> {
    let account = account(acme).await?;

    let identifiers: Vec<Identifier> = acme.domains.iter().cloned().map(Identifier::Dns).collect();
    let mut order = account
        .new_order(&NewOrder {
            identifiers: &identifiers,
        })
        .await
        .context("creating ACME order")?;

    // Publish an HTTP-01 response for each pending authorization.
    let mut tokens = Vec::new();
    for authz in order
        .authorizations()
        .await
        .context("fetching authorizations")?
    {
        if authz.status != AuthorizationStatus::Pending {
            continue;
        }
        let challenge = authz
            .challenges
            .iter()
            .find(|c| c.r#type == ChallengeType::Http01)
            .context("no http-01 challenge offered")?;
        let key_auth = order.key_authorization(challenge);
        challenges
            .write()
            .expect("challenges lock")
            .insert(challenge.token.clone(), key_auth.as_str().to_string());
        tokens.push(challenge.token.clone());
        order
            .set_challenge_ready(&challenge.url)
            .await
            .context("signalling challenge ready")?;
    }

    // Poll until the order is Ready (validated) — or give up.
    let result = poll_ready(&mut order).await;
    // Challenges are one-shot; drop them regardless of outcome.
    {
        let mut map = challenges.write().expect("challenges lock");
        for t in &tokens {
            map.remove(t);
        }
    }
    result?;

    // Finalize with a fresh keypair + CSR, then download the chain.
    let key_pair = rcgen::KeyPair::generate().context("generating cert keypair")?;
    let mut params = rcgen::CertificateParams::new(acme.domains.clone()).context("cert params")?;
    // Clear rcgen's default subject (CN "rcgen self signed cert") — otherwise
    // Let's Encrypt reads that CN as a domain identifier and rejects the order.
    // The SANs alone carry the domains for an ACME CSR.
    params.distinguished_name = rcgen::DistinguishedName::new();
    let csr = params
        .serialize_request(&key_pair)
        .context("building CSR")?;
    order
        .finalize(csr.der().as_ref())
        .await
        .context("finalizing order")?;

    let chain_pem = loop {
        if let Some(pem) = order
            .certificate()
            .await
            .context("downloading certificate")?
        {
            break pem;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    let key_pem = key_pair.serialize_pem();

    // Cache to disk (survives restart) and hand back the parsed key.
    std::fs::write(acme.cache_dir.join("cert.pem"), &chain_pem).context("caching cert")?;
    std::fs::write(acme.cache_dir.join("key.pem"), &key_pem).context("caching key")?;
    super::load_pem(
        &acme.cache_dir.join("cert.pem"),
        &acme.cache_dir.join("key.pem"),
    )
}

async fn poll_ready(order: &mut instant_acme::Order) -> anyhow::Result<()> {
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let state = order.refresh().await.context("polling order")?;
        match state.status {
            OrderStatus::Ready | OrderStatus::Valid => return Ok(()),
            OrderStatus::Invalid => anyhow::bail!("ACME order invalid: {:?}", state.error),
            _ => {}
        }
    }
    anyhow::bail!("ACME order not ready after timeout")
}

/// Restore the ACME account from cache, or register a new one.
async fn account(acme: &Acme) -> anyhow::Result<Account> {
    let creds_path = acme.cache_dir.join("account.json");
    if let Ok(bytes) = std::fs::read(&creds_path) {
        if let Ok(creds) = serde_json::from_slice::<AccountCredentials>(&bytes) {
            return Account::from_credentials(creds)
                .await
                .context("restoring ACME account");
        }
        warn!("ACME: cached account unreadable, registering a new one");
    }
    let contacts: Vec<String> = acme.email.iter().map(|e| format!("mailto:{e}")).collect();
    let contact: Vec<&str> = contacts.iter().map(String::as_str).collect();
    let (account, creds) = Account::create(
        &NewAccount {
            contact: &contact,
            terms_of_service_agreed: true,
            only_return_existing: false,
        },
        directory(acme),
        None,
    )
    .await
    .context("registering ACME account")?;
    std::fs::write(&creds_path, serde_json::to_vec(&creds)?).context("caching ACME account")?;
    Ok(account)
}

fn directory(acme: &Acme) -> &'static str {
    if acme.staging {
        LetsEncrypt::Staging.url()
    } else {
        LetsEncrypt::Production.url()
    }
}

fn load_cached(acme: &Acme) -> Option<Arc<CertifiedKey>> {
    super::load_pem(
        &acme.cache_dir.join("cert.pem"),
        &acme.cache_dir.join("key.pem"),
    )
    .ok()
}

/// Time since the cached cert was last written (its issuance time).
fn cert_age(acme: &Acme) -> Option<Duration> {
    std::fs::metadata(acme.cache_dir.join("cert.pem"))
        .and_then(|m| m.modified())
        .ok()?
        .elapsed()
        .ok()
}
