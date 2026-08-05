//! weft-matrix — run the daemon, or emit the appservice registration.
//!
//! ```text
//! weft-matrix <config.toml>                          run
//! weft-matrix generate-registration <config.toml>    print registration YAML
//! ```

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use tracing::{error, info, warn};
use weft_matrix::{asapi, bridge::Bridge, config::Config, hs::Hs, store::Store};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let (registration, config_path) = match (args.next().as_deref(), args.next()) {
        (Some("generate-registration"), Some(path)) => (true, PathBuf::from(path)),
        (Some(path), None) => (false, PathBuf::from(path)),
        _ => {
            eprintln!("usage: weft-matrix [generate-registration] <config.toml>");
            std::process::exit(2);
        }
    };

    let cfg = Config::load(&config_path)?;

    if registration {
        let url = format!("http://{}", cfg.matrix.listen);
        print!("{}", weft_matrix::config::registration_yaml(&cfg, &url));
        return Ok(());
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(cfg))
}

async fn run(cfg: Config) -> anyhow::Result<()> {
    let keypair = load_or_generate_key(&cfg.weft.key_file)?;
    info!(
        pubkey = keypair.public().to_b64(),
        "adapter key (pin this in weftd's [[plugin.remote]], scheme \"matrix\")"
    );

    // The appservice HTTP surface, up before anything else: the homeserver
    // retries pushes, so being briefly unreachable loses nothing, but a bound
    // port that never answers would look like success in the logs.
    let (txn_tx, mut txn_rx) = tokio::sync::mpsc::channel(64);
    let listener = tokio::net::TcpListener::bind(&cfg.matrix.listen)
        .await
        .with_context(|| format!("binding AS API on {}", cfg.matrix.listen))?;
    info!(listen = %cfg.matrix.listen, "appservice API up");
    tokio::spawn({
        let router = asapi::router(cfg.matrix.hs_token.clone(), txn_tx);
        async move {
            if let Err(e) = axum::serve(listener, router).await {
                error!("AS API server died: {e}");
            }
        }
    });

    let hs = Hs::new(&cfg.matrix.hs_url, &cfg.matrix.as_token);

    // Reconnect loop: on any session loss, re-dial, re-assert the realm and
    // structure, and re-state membership — the protocol's answer to every gap
    // (bridge-session-protocol §10).
    loop {
        match session(&cfg, &keypair, &hs, &mut txn_rx).await {
            Ok(()) => info!("weftd closed the session; reconnecting"),
            Err(e) => warn!("session failed: {e:#}; reconnecting"),
        }

        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

/// One connected life: register, re-assert what we know, pump until the
/// session dies.
async fn session(
    cfg: &Config,
    keypair: &weft_crypto::Keypair,
    hs: &Hs,
    txns: &mut tokio::sync::mpsc::Receiver<asapi::Txn>,
) -> anyhow::Result<()> {
    // Fresh load per session: the store is the durable truth, and a reconnect
    // is exactly when re-reading it is cheapest and most correct.
    let store = Store::connect(&cfg.daemon.database_url).await?;

    // Keypair is deliberately not Clone (it wraps key material); each session
    // rebuilds it from the seed.
    let keypair = weft_crypto::Keypair::from_seed_b64(&keypair.seed_b64())
        .expect("a round-tripped seed is valid");
    let connected =
        weft_appservice::AppService::builder(cfg.weft.endpoint.clone(), keypair, "matrix")
            .name("Matrix Bridge")
            .scheme("matrix")
            .connect()
            .await?;
    let realm = connected.realm.clone();
    let session = tokio::spawn(connected.session);
    info!(network = realm.network(), "connected to weftd");

    let mut bridge = Bridge {
        realm: realm.clone(),
        hs: hs.clone(),
        store,
        domain: cfg.matrix.domain.clone(),
        puppet_prefix: cfg.matrix.puppet_prefix.clone(),
        bot_localpart: cfg.matrix.bot.clone(),
    };

    reassert(&mut bridge).await?;

    let result = bridge.run(connected.events, txns).await;
    session.abort();
    result
}

/// Re-assert every known space and re-state its membership — the reconnect
/// contract: weftd holds nothing durable for us, so the session starts by
/// stating the world (bridge-session-protocol §10).
async fn reassert(bridge: &mut Bridge) -> anyhow::Result<()> {
    // Realms bind per-space URI domain; the session-level binding is the
    // first space's realm. MVP: one realm per daemon (matrix.org-style
    // multi-realm needs one data connection each — deferred with multi-realm).
    let Some(space) = bridge.store.state.spaces.values().next() else {
        return Ok(()); // nothing provisioned yet — REALM REGISTER only
    };
    let realm_name = weft_matrix::ident::SpaceRef::parse(&space.uri)
        .map(|s| s.realm)
        .context("stored space with an unparsable URI")?;

    bridge
        .realm
        .assert(&format!("matrix://{realm_name}"))
        .await?;

    let uris: Vec<String> = bridge.store.state.spaces.keys().cloned().collect();
    for uri in uris {
        if let Err(e) = bridge.provision(&uri).await {
            warn!(uri, "re-assertion failed: {e:#}");
        }
    }

    // Full-replace resync: everyone we did not just name is dropped weftd-side,
    // which is exactly the drift correction a gap needs.
    bridge.realm.begin_sync().await?;
    let statements: Vec<(String, String)> = bridge
        .store
        .state
        .spaces
        .values()
        .flat_map(|space| {
            space
                .member_rooms
                .keys()
                .map(|user| (space.ns_id.clone(), user.clone()))
        })
        .collect();
    for (ns, user) in statements {
        bridge
            .realm
            .member(&ns, &user, weft_proto::MemberAction::Join)
            .await?;
    }
    let cursor = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().to_string())
        .unwrap_or_else(|_| "0".into());
    bridge.realm.end_sync(&cursor).await?;

    Ok(())
}

fn load_or_generate_key(path: &Path) -> anyhow::Result<weft_crypto::Keypair> {
    if path.exists() {
        let seed = std::fs::read_to_string(path)
            .with_context(|| format!("reading key file {}", path.display()))?;

        weft_crypto::Keypair::from_seed_b64(seed.trim())
            .map_err(|e| anyhow::anyhow!("invalid key file {}: {e}", path.display()))
    } else {
        let keypair = weft_crypto::Keypair::generate();
        std::fs::write(path, keypair.seed_b64() + "\n")
            .with_context(|| format!("writing key file {}", path.display()))?;
        info!(path = %path.display(), "generated adapter signing key");

        Ok(keypair)
    }
}
