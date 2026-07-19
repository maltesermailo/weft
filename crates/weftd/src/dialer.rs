//! Outbound bridge dialer (M5d / auto-federation P1).
//!
//! Connect to a peer network over QUIC, open a control stream, and drive the
//! **client** side of the AUTH BRIDGE handshake (§11.2): prove THIS network's
//! signing key to the peer. On success the peer treats the session as an
//! authenticated bridge (`State::Bridge`), and forwarding/ingestion ride the
//! returned stream (later slices).
//!
//! The inbound acceptor path (weft-core `run_session`) is the mirror of this;
//! here weftd is the initiator, so the handshake is hand-driven with the codec.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context};
use tracing::info;
use weft_core::{BlobHash, Keypair, MirrorRequest, PublicKey, ServerCtx};
use weft_proto::{Command, Event, NamespaceName, NetworkName, Reply, Request};
use weft_transport::QuicControlStream;

/// An authenticated outbound bridge: the control stream plus the connection it
/// rides (quinn drops streams when the connection is dropped, so hold both).
pub struct BridgeLink {
    pub stream: QuicControlStream,
    /// Kept alive for the lifetime of the stream.
    pub connection: quinn::Connection,
}

/// Live authenticated **outbound** bridge connections, keyed by peer network.
/// The mirror consumer (§11.8) opens data-plane streams back over these to pull
/// foreign blobs referenced by ingested messages. Populated by [`dial_and_run`]
/// for as long as a bridge session is up, then removed.
#[derive(Clone, Default)]
pub struct PeerLinks(Arc<Mutex<HashMap<NetworkName, quinn::Connection>>>);

impl PeerLinks {
    pub fn new() -> Self {
        Self::default()
    }

    fn insert(&self, peer: NetworkName, connection: quinn::Connection) {
        self.0.lock().unwrap().insert(peer, connection);
    }

    fn remove(&self, peer: &NetworkName) {
        self.0.lock().unwrap().remove(peer);
    }

    /// The current bridge connection to `peer`, if one is live.
    pub fn get(&self, peer: &NetworkName) -> Option<quinn::Connection> {
        self.0.lock().unwrap().get(peer).cloned()
    }
}

/// Dial `peer`, authenticate, and run the outbound bridge session to completion
/// (§11 M5d): transmit our stored proposal, forward local-origin events, ingest
/// the peer's. `peer_key` is the peer's network key (pinned / well-known), used
/// to verify manifests it sends. Returns when the session closes or errors.
#[allow(clippy::too_many_arguments)]
pub async fn run_peer_bridge(
    endpoint: &quinn::Endpoint,
    addr: SocketAddr,
    peer: &NetworkName,
    peer_key: PublicKey,
    identity: &Keypair,
    our_network: &NetworkName,
    ctx: Arc<ServerCtx>,
    links: PeerLinks,
) -> anyhow::Result<()> {
    dial_and_run(
        endpoint,
        addr,
        peer,
        identity,
        our_network,
        links,
        move |lines| weft_core::run_bridge_client(lines, ctx, peer.clone(), peer_key),
    )
    .await
}

/// Dial + authenticate a bridge, then hand the stream to `run` (the core session
/// runner), holding the connection alive until it returns. Shared by the P1
/// proposer and the §11.10 requester.
#[allow(clippy::too_many_arguments)]
async fn dial_and_run<F, Fut>(
    endpoint: &quinn::Endpoint,
    addr: SocketAddr,
    peer: &NetworkName,
    identity: &Keypair,
    our_network: &NetworkName,
    links: PeerLinks,
    run: F,
) -> anyhow::Result<()>
where
    F: FnOnce(crate::acceptor::QuicLines) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let BridgeLink { stream, connection } =
        dial_bridge(endpoint, addr, peer, identity, our_network).await?;
    // Register the connection so the mirror consumer can pull foreign blobs back
    // over it (§11.8) for as long as this bridge is up.
    links.insert(peer.clone(), connection.clone());
    run(crate::acceptor::QuicLines(stream)).await;
    links.remove(peer);
    drop(connection);
    Ok(())
}

/// §11.10 on-demand auto-bridge (home side): SSRF-guard `addr`, dial `peer`,
/// then **request** its namespace `ns` and auto-accept the offer. `endpoint` is
/// the QUIC client endpoint (verified in prod; tests inject an insecure one).
/// Returns when the bridge session closes, or an error on a refused/failed dial.
#[allow(clippy::too_many_arguments)]
pub async fn auto_bridge(
    endpoint: &quinn::Endpoint,
    addr: SocketAddr,
    peer: &NetworkName,
    peer_key: PublicKey,
    ns: NamespaceName,
    identity: &Keypair,
    our_network: &NetworkName,
    ctx: Arc<ServerCtx>,
    links: PeerLinks,
) -> anyhow::Result<()> {
    if !is_dialable(&addr) {
        bail!("refusing to auto-bridge {peer} at non-public address {addr} (SSRF guard, §11.10)");
    }
    run_peer_requester(
        endpoint,
        addr,
        peer,
        peer_key,
        ns,
        identity,
        our_network,
        ctx,
        links,
    )
    .await
}

/// Dial `peer`, then request its namespace `ns` and auto-accept the offer,
/// running the bridge session to completion. The un-guarded core of
/// [`auto_bridge`] (which adds the SSRF check).
#[allow(clippy::too_many_arguments)]
pub async fn run_peer_requester(
    endpoint: &quinn::Endpoint,
    addr: SocketAddr,
    peer: &NetworkName,
    peer_key: PublicKey,
    ns: NamespaceName,
    identity: &Keypair,
    our_network: &NetworkName,
    ctx: Arc<ServerCtx>,
    links: PeerLinks,
) -> anyhow::Result<()> {
    dial_and_run(
        endpoint,
        addr,
        peer,
        identity,
        our_network,
        links,
        move |lines| weft_core::run_bridge_requester(lines, ctx, peer.clone(), peer_key, ns),
    )
    .await
}

/// Dial `peer` at `addr` over `endpoint` and complete AUTH BRIDGE, proving
/// `identity` (our network key) belongs to `our_network`. `endpoint` is the QUIC
/// **client** endpoint — verified in production; tests inject an insecure one.
///
/// Returns the authenticated link, or an error if the connection or the §11.2
/// handshake fails (a rejected proof surfaces as the peer's `AUTH-FAILED`).
pub async fn dial_bridge(
    endpoint: &quinn::Endpoint,
    addr: SocketAddr,
    peer: &NetworkName,
    identity: &Keypair,
    our_network: &NetworkName,
) -> anyhow::Result<BridgeLink> {
    // SNI = the peer's network name (§3.1: it must match the peer's cert).
    let connection = endpoint
        .connect(addr, peer.as_str())
        .context("QUIC connect config")?
        .await
        .with_context(|| format!("QUIC handshake with {peer} at {addr}"))?;
    let mut stream = QuicControlStream::open(&connection)
        .await
        .context("opening bridge control stream")?;

    // → HELLO weft/1 (§3.3 negotiation); ← WELCOME → the peer is now UNAUTHED.
    send(
        &mut stream,
        Command::Hello {
            version: "weft/1".to_string(),
        },
    )
    .await
    .context("send HELLO")?;
    match recv_event(&mut stream).await? {
        Event::Welcome { .. } => {}
        other => bail!("expected WELCOME after HELLO, got {other:?}"),
    }

    // → AUTH BRIDGE <our-network> <our-pubkey>
    send(
        &mut stream,
        Command::AuthBridge {
            network: our_network.clone(),
            token: identity.public().to_b64(),
        },
    )
    .await
    .context("send AUTH BRIDGE")?;

    // ← CHALLENGE <nonce>
    let nonce = match recv_event(&mut stream).await? {
        Event::Challenge { nonce } => {
            weft_crypto::b64::decode(&nonce).context("decoding challenge nonce")?
        }
        other => bail!("expected CHALLENGE, got {other:?}"),
    };

    // → AUTH PROOF: sign nonce‖peer-network (§6.1 binds the *target* network,
    // so a proof for one peer is dead against another — invariant 5).
    let sig = weft_crypto::sign_challenge(identity, &nonce, peer.as_str());
    send(
        &mut stream,
        Command::AuthProof {
            signature: weft_crypto::signature_to_b64(&sig),
        },
    )
    .await
    .context("send AUTH PROOF")?;

    // ← WELCOME (success) or ERR AUTH-FAILED.
    match recv_event(&mut stream).await? {
        Event::Welcome { network, .. } => {
            info!(%peer, reported = %network, "outbound bridge authenticated");
            Ok(BridgeLink { stream, connection })
        }
        Event::Err(e) => bail!("bridge auth rejected by {peer}: {e:?}"),
        other => bail!("expected WELCOME after PROOF, got {other:?}"),
    }
}

/// §11.10 Drain the auto-federation port: for each `FEDERATE` request (handed up
/// from weft-core), resolve the peer, SSRF-guard, dial, and request its
/// namespace. Peer keys + endpoints come from `[[peers]]` pins for now
/// (well-known key fetch for arbitrary domains is a TODO). Runs until shutdown.
pub fn spawn_auto_bridge_consumer(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<weft_core::AutoBridgeRequest>,
    peers: &[crate::config::Peer],
    identity_seed: String,
    our_network: NetworkName,
    ctx: Arc<ServerCtx>,
    links: PeerLinks,
) -> tokio::task::JoinHandle<()> {
    let pinned: std::collections::HashMap<NetworkName, (String, PublicKey)> = peers
        .iter()
        .filter_map(|p| {
            Some((
                p.network.parse().ok()?,
                (p.endpoint.clone(), PublicKey::from_b64(&p.key).ok()?),
            ))
        })
        .collect();
    tokio::spawn(async move {
        let Ok(identity) = Keypair::from_seed_b64(&identity_seed) else {
            return;
        };
        let client = match weft_transport::client_endpoint(weft_transport::ALPN) {
            Ok(e) => e,
            Err(e) => {
                tracing::error!("auto-federation: cannot build client endpoint: {e}");
                return;
            }
        };
        loop {
            let req = tokio::select! {
                r = rx.recv() => match r { Some(r) => r, None => break },
                _ = ctx.shutdown.cancelled() => break,
            };
            let weft_core::AutoBridgeRequest { network, namespace } = req;
            // Resolve the peer's key + dial address: a `[[peers]]` pin, else the
            // §10.2 well-known key fetch (arbitrary public domains).
            let resolved = match pinned.get(&network) {
                Some((endpoint, key)) => tokio::net::lookup_host(endpoint)
                    .await
                    .ok()
                    .and_then(|mut a| a.next())
                    .map(|addr| (*key, addr))
                    .ok_or_else(|| anyhow::anyhow!("cannot resolve pinned endpoint {endpoint}")),
                None => fetch_signing_key(network.as_str()).await,
            };
            let (key, addr) = match resolved {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(%network, "auto-federation: cannot resolve peer: {e}");
                    continue;
                }
            };
            if let Err(e) = auto_bridge(
                &client,
                addr,
                &network,
                key,
                namespace,
                &identity,
                &our_network,
                Arc::clone(&ctx),
                links.clone(),
            )
            .await
            {
                tracing::warn!(%network, "auto-bridge failed: {e}");
            }
        }
    })
}

/// Spawn one maintained outbound bridge per configured peer. Each task dials,
/// runs the bridge until it closes, then reconnects after a backoff — until
/// shutdown. Returns the task handles for the caller to track.
pub fn spawn_dialers(
    peers: &[crate::config::Peer],
    identity_seed: String,
    our_network: NetworkName,
    ctx: Arc<ServerCtx>,
    links: PeerLinks,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut tasks = Vec::new();
    for peer in peers {
        let (Ok(network), Ok(key)) = (
            peer.network.parse::<NetworkName>(),
            PublicKey::from_b64(&peer.key),
        ) else {
            tracing::error!(peer = %peer.network, "invalid [[peers]] network/key; not dialing");
            continue;
        };
        let endpoint = peer.endpoint.clone();
        let ctx = Arc::clone(&ctx);
        let our_network = our_network.clone();
        let seed = identity_seed.clone();
        tasks.push(tokio::spawn(dial_loop(
            endpoint,
            network,
            key,
            seed,
            our_network,
            ctx,
            links.clone(),
        )));
    }
    tasks
}

/// Maintain one outbound bridge: (re)dial with a backoff, running the session
/// each time until it drops. Interruptible by the shutdown signal.
#[allow(clippy::too_many_arguments)]
async fn dial_loop(
    endpoint_str: String,
    network: NetworkName,
    key: PublicKey,
    seed: String,
    our_network: NetworkName,
    ctx: Arc<ServerCtx>,
    links: PeerLinks,
) {
    let Ok(identity) = Keypair::from_seed_b64(&seed) else {
        tracing::error!("invalid network key seed; not dialing");
        return;
    };
    let client = match weft_transport::client_endpoint(weft_transport::ALPN) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!("cannot build outbound QUIC endpoint: {e}");
            return;
        }
    };
    const BACKOFF: std::time::Duration = std::time::Duration::from_secs(5);
    while !ctx.shutdown.is_cancelled() {
        match tokio::net::lookup_host(&endpoint_str)
            .await
            .ok()
            .and_then(|mut a| a.next())
        {
            Some(addr) => {
                if let Err(e) = run_peer_bridge(
                    &client,
                    addr,
                    &network,
                    key,
                    &identity,
                    &our_network,
                    Arc::clone(&ctx),
                    links.clone(),
                )
                .await
                {
                    tracing::debug!(%network, "outbound bridge ended: {e}");
                }
            }
            None => tracing::warn!(endpoint = %endpoint_str, "cannot resolve peer endpoint"),
        }
        tokio::select! {
            _ = tokio::time::sleep(BACKOFF) => {}
            _ = ctx.shutdown.cancelled() => break,
        }
    }
}

/// §11.10 SSRF guard (security invariant 13): only public unicast addresses are
/// dialable for **auto-federation**. A user-supplied network name must never
/// make us reach internal infrastructure — loopback, RFC-1918, CGNAT, link-local,
/// ULA, unspecified, multicast, or the `169.254.169.254` metadata address.
/// (Operator-configured `[[peers]]` are trusted and skip this.)
pub fn is_dialable(addr: &SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_public_v4(v4), // don't let a mapped address smuggle a private v4
            None => is_public_v6(v6),
        },
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    let cgnat = o[0] == 100 && (o[1] & 0xc0) == 0x40; // 100.64.0.0/10
    !(ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || cgnat)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let ula = (ip.segments()[0] & 0xfe00) == 0xfc00; // fc00::/7
    let link_local = (ip.segments()[0] & 0xffc0) == 0xfe80; // fe80::/10
    !(ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() || ula || link_local)
}

/// §11.10 Fetch a foreign network's Ed25519 signing key from its
/// `/.well-known/weft` (§10.2), so we can verify its manifests when it isn't
/// pinned. TLS-verified, timeout- and size-bounded; the resolved host is
/// SSRF-guarded (invariant 13) *before* we connect, and redirects are not
/// followed. Returns the key + the resolved dial address (`<network>:443`).
pub async fn fetch_signing_key(network: &str) -> anyhow::Result<(PublicKey, SocketAddr)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let addr = tokio::net::lookup_host((network, 443u16))
        .await
        .ok()
        .and_then(|mut a| a.next())
        .with_context(|| format!("resolving {network}:443"))?;
    if !is_dialable(&addr) {
        bail!("well-known host {network} ({addr}) is not public (SSRF guard, §11.10)");
    }

    // rustls client config: process-default provider (ring) + Mozilla roots —
    // the same trust anchors as the verified QUIC client.
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = tokio_rustls::rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let server_name = tokio_rustls::rustls::pki_types::ServerName::try_from(network.to_string())
        .context("invalid TLS server name")?;

    const MAX_BODY: usize = 64 * 1024;
    let raw = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let tcp = tokio::net::TcpStream::connect(addr).await?;
        let mut tls = connector.connect(server_name, tcp).await?;
        let req = format!(
            "GET /.well-known/weft HTTP/1.1\r\nHost: {network}\r\nAccept: application/json\r\nUser-Agent: weftd\r\nConnection: close\r\n\r\n"
        );
        tls.write_all(req.as_bytes()).await?;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = tls.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.len() > MAX_BODY {
                bail!("well-known response exceeds {MAX_BODY} bytes");
            }
        }
        Ok::<Vec<u8>, anyhow::Error>(buf)
    })
    .await
    .context("well-known fetch timed out")??;

    if !raw.starts_with(b"HTTP/1.1 200") && !raw.starts_with(b"HTTP/1.0 200") {
        let status: String = raw
            .iter()
            .take_while(|&&b| b != b'\r')
            .map(|&b| b as char)
            .collect();
        bail!("well-known returned {status:?}");
    }
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("malformed HTTP response (no body)")?;
    #[derive(serde::Deserialize)]
    struct Doc {
        #[serde(rename = "signing-key")]
        signing_key: String,
    }
    let doc: Doc =
        serde_json::from_slice(&raw[split + 4..]).context("parsing /.well-known/weft JSON")?;
    let key = PublicKey::from_b64(&doc.signing_key)
        .map_err(|_| anyhow::anyhow!("well-known signing-key is not a valid pubkey"))?;
    Ok((key, addr))
}

/// §11.8 Drain the mirror port: for each foreign attachment weft-core ingested,
/// pull the blob back over the live bridge connection to its origin and store it
/// locally, so members fetch media from *this* server (no connection to the
/// origin, no origin↔member correlation). The pull is a signed `MIRROR` request
/// (this network's key), self-authenticating so the origin need not correlate
/// the data-plane stream with a bridge session. Runs until shutdown.
pub fn spawn_mirror_consumer(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<MirrorRequest>,
    links: PeerLinks,
    identity_seed: String,
    our_network: NetworkName,
    ctx: Arc<ServerCtx>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let Ok(identity) = Keypair::from_seed_b64(&identity_seed) else {
            tracing::error!("mirror: invalid network key seed; not mirroring");
            return;
        };
        loop {
            let req = tokio::select! {
                r = rx.recv() => match r { Some(r) => r, None => break },
                _ = ctx.shutdown.cancelled() => break,
            };
            let MirrorRequest { peer, hash, .. } = req;
            // Already have it? on_ingest recorded the *reference* eagerly; the
            // blob record only exists once bytes are stored, so this is the
            // honest "do we hold the bytes" check (and dedups retries).
            if ctx
                .media_refs
                .blob_meta(&hash)
                .await
                .ok()
                .flatten()
                .is_some()
            {
                continue;
            }
            let Some(connection) = links.get(&peer) else {
                tracing::debug!(%peer, %hash, "mirror: no live bridge to origin; skipping pull");
                continue;
            };
            if let Err(e) =
                pull_blob(&connection, &identity, &our_network, &peer, &hash, &ctx).await
            {
                tracing::warn!(%peer, %hash, "mirror pull failed: {e}");
            }
        }
    })
}

/// One mirror pull: open a data-plane bidi stream to the origin, send a signed
/// `MIRROR <our-network> <hash> <sig>`, verify the returned bytes hash to `hash`
/// (content addressing — the origin can't substitute), and store them locally.
async fn pull_blob(
    connection: &quinn::Connection,
    identity: &Keypair,
    our_network: &NetworkName,
    origin: &NetworkName,
    hash: &str,
    ctx: &Arc<ServerCtx>,
) -> anyhow::Result<()> {
    let sig =
        weft_crypto::sign_mirror_request(identity, hash, our_network.as_str(), origin.as_str());
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .context("opening mirror data stream")?;
    let line = format!(
        "MIRROR {} {} {}\n",
        our_network,
        hash,
        weft_crypto::signature_to_b64(&sig)
    );
    send.write_all(line.as_bytes())
        .await
        .context("sending MIRROR request")?;
    let _ = send.finish();

    // `OK <mime> <len>\n<bytes…>` on success; `ERR <why>` otherwise.
    let resp = recv
        .read_to_end(weft_core::MEDIA_MAX_BYTES as usize + 4096)
        .await
        .context("reading mirror response")?;
    let nl = resp.iter().position(|&b| b == b'\n').unwrap_or(resp.len());
    let header = String::from_utf8_lossy(&resp[..nl]).into_owned();
    let body = resp.get(nl + 1..).unwrap_or(&[]);
    let mut parts = header.split_whitespace();
    match parts.next() {
        Some("OK") => {
            let mime = parts.next().unwrap_or("application/octet-stream");
            // Content addressing: refuse anything that doesn't hash to what we
            // asked for (the origin cannot swap in other bytes, invariant 2).
            if BlobHash::parse(hash).is_none() || weft_core::blob_hash(body).as_str() != hash {
                bail!("mirror bytes do not match requested hash");
            }
            if !crate::media::store_mirrored(ctx, hash, mime, body).await {
                bail!("storing mirrored blob failed");
            }
            tracing::debug!(%origin, %hash, bytes = body.len(), "mirrored foreign blob");
            Ok(())
        }
        _ => bail!("origin refused mirror: {}", header.trim()),
    }
}

/// §11.7 Drain the backfill port: for each `STREAM ACCEPT` a peer offered in
/// answer to our federated HISTORY, open a data-plane stream on the live bridge
/// to that peer, pull the serialized batch (`BACKFILL <token>`), and feed each
/// line back through `ctx.ingest_bridged` — origin-authority-checked and
/// manifest-gated exactly like a live bridge event (invariants 2, 3). Runs until
/// shutdown.
pub fn spawn_backfill_consumer(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<weft_core::BackfillPull>,
    links: PeerLinks,
    ctx: Arc<ServerCtx>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let req = tokio::select! {
                r = rx.recv() => match r { Some(r) => r, None => break },
                _ = ctx.shutdown.cancelled() => break,
            };
            let weft_core::BackfillPull { peer, token } = req;
            let Some(connection) = links.get(&peer) else {
                tracing::debug!(%peer, "backfill: no live bridge to peer; dropping pull");
                continue;
            };
            if let Err(e) = pull_backfill(&connection, &peer, &token, &ctx).await {
                tracing::warn!(%peer, "backfill pull failed: {e}");
            }
        }
    })
}

/// One backfill pull: open a data-plane bidi stream to `peer`, send `BACKFILL
/// <token>`, and ingest each serialized line it streams back.
async fn pull_backfill(
    connection: &quinn::Connection,
    peer: &NetworkName,
    token: &str,
    ctx: &Arc<ServerCtx>,
) -> anyhow::Result<()> {
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .context("opening backfill data stream")?;
    send.write_all(format!("BACKFILL {token}\n").as_bytes())
        .await
        .context("sending BACKFILL request")?;
    let _ = send.finish();

    // `OK <len>\n<serialized Reply lines…>` on success; `ERR <why>` otherwise.
    // The batch is capped at MAX_HISTORY_LIMIT events, so it fits the media
    // ceiling comfortably.
    let resp = recv
        .read_to_end(weft_core::MEDIA_MAX_BYTES as usize + 4096)
        .await
        .context("reading backfill response")?;
    let nl = resp.iter().position(|&b| b == b'\n').unwrap_or(resp.len());
    let header = String::from_utf8_lossy(&resp[..nl]).into_owned();
    if !header.starts_with("OK ") {
        bail!("peer refused backfill: {}", header.trim());
    }
    let body = resp.get(nl + 1..).unwrap_or(&[]);
    let mut count = 0usize;
    for raw in body.split(|&b| b == b'\n') {
        let line = String::from_utf8_lossy(raw);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(parsed) = weft_proto::Line::parse(line) {
            ctx.ingest_bridged(peer, &parsed).await;
            count += 1;
        }
    }
    tracing::debug!(%peer, lines = count, "ingested backfill stream");
    Ok(())
}

async fn send(stream: &mut QuicControlStream, cmd: Command) -> anyhow::Result<()> {
    let line = Request::new(cmd).serialize().context("serialize command")?;
    stream.send_line(&line).await.context("send line")
}

async fn recv_event(stream: &mut QuicControlStream) -> anyhow::Result<Event> {
    let line = stream
        .recv_line()
        .await
        .context("reading from bridge stream")?
        .context("peer closed the bridge stream")?;
    Ok(Reply::parse(&line)
        .with_context(|| format!("parsing bridge event {line:?}"))?
        .event)
}

#[cfg(test)]
mod tests {
    use super::is_dialable;

    fn dialable(s: &str) -> bool {
        is_dialable(&s.parse().unwrap())
    }

    #[test]
    fn ssrf_guard_rejects_internal_targets() {
        // Public → allowed.
        assert!(dialable("93.184.216.34:443"));
        assert!(dialable("[2606:2800:220:1:248:1893:25c8:1946]:443"));
        // Internal / special → refused (invariant 13).
        for bad in [
            "127.0.0.1:443",         // loopback
            "10.0.0.5:443",          // RFC-1918
            "192.168.1.1:443",       // RFC-1918
            "172.16.0.1:443",        // RFC-1918
            "169.254.169.254:443",   // cloud metadata (link-local)
            "100.64.0.1:443",        // CGNAT
            "0.0.0.0:443",           // unspecified
            "[::1]:443",             // v6 loopback
            "[fc00::1]:443",         // ULA
            "[fe80::1]:443",         // v6 link-local
            "[::ffff:10.0.0.1]:443", // v4-mapped private (smuggling)
        ] {
            assert!(!dialable(bad), "{bad} must not be dialable");
        }
    }

    #[tokio::test]
    async fn well_known_fetch_refuses_private_host() {
        // `localhost` resolves to a loopback address — the fetch must bail on the
        // SSRF guard before opening any connection (invariant 13).
        let err = super::fetch_signing_key("localhost").await.unwrap_err();
        assert!(err.to_string().contains("SSRF guard"), "{err}");
    }
}
