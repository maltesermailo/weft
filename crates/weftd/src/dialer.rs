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

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use anyhow::{bail, Context};
use tracing::info;
use weft_core::{Keypair, PublicKey, ServerCtx};
use weft_proto::{Command, Event, NamespaceName, NetworkName, Reply, Request};
use weft_transport::QuicControlStream;

/// An authenticated outbound bridge: the control stream plus the connection it
/// rides (quinn drops streams when the connection is dropped, so hold both).
pub struct BridgeLink {
    pub stream: QuicControlStream,
    /// Kept alive for the lifetime of the stream.
    pub connection: quinn::Connection,
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
) -> anyhow::Result<()> {
    dial_and_run(endpoint, addr, peer, identity, our_network, move |lines| {
        weft_core::run_bridge_client(lines, ctx, peer.clone(), peer_key)
    })
    .await
}

/// Dial + authenticate a bridge, then hand the stream to `run` (the core session
/// runner), holding the connection alive until it returns. Shared by the P1
/// proposer and the §11.10 requester.
async fn dial_and_run<F, Fut>(
    endpoint: &quinn::Endpoint,
    addr: SocketAddr,
    peer: &NetworkName,
    identity: &Keypair,
    our_network: &NetworkName,
    run: F,
) -> anyhow::Result<()>
where
    F: FnOnce(crate::acceptor::QuicLines) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let BridgeLink { stream, connection } =
        dial_bridge(endpoint, addr, peer, identity, our_network).await?;
    run(crate::acceptor::QuicLines(stream)).await;
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
) -> anyhow::Result<()> {
    if !is_dialable(&addr) {
        bail!("refusing to auto-bridge {peer} at non-public address {addr} (SSRF guard, §11.10)");
    }
    run_peer_requester(endpoint, addr, peer, peer_key, ns, identity, our_network, ctx).await
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
) -> anyhow::Result<()> {
    dial_and_run(endpoint, addr, peer, identity, our_network, move |lines| {
        weft_core::run_bridge_requester(lines, ctx, peer.clone(), peer_key, ns)
    })
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

/// Spawn one maintained outbound bridge per configured peer. Each task dials,
/// runs the bridge until it closes, then reconnects after a backoff — until
/// shutdown. Returns the task handles for the caller to track.
pub fn spawn_dialers(
    peers: &[crate::config::Peer],
    identity_seed: String,
    our_network: NetworkName,
    ctx: Arc<ServerCtx>,
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
        )));
    }
    tasks
}

/// Maintain one outbound bridge: (re)dial with a backoff, running the session
/// each time until it drops. Interruptible by the shutdown signal.
async fn dial_loop(
    endpoint_str: String,
    network: NetworkName,
    key: PublicKey,
    seed: String,
    our_network: NetworkName,
    ctx: Arc<ServerCtx>,
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
                if let Err(e) =
                    run_peer_bridge(&client, addr, &network, key, &identity, &our_network, Arc::clone(&ctx))
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
            "127.0.0.1:443",           // loopback
            "10.0.0.5:443",            // RFC-1918
            "192.168.1.1:443",         // RFC-1918
            "172.16.0.1:443",          // RFC-1918
            "169.254.169.254:443",     // cloud metadata (link-local)
            "100.64.0.1:443",          // CGNAT
            "0.0.0.0:443",             // unspecified
            "[::1]:443",               // v6 loopback
            "[fc00::1]:443",           // ULA
            "[fe80::1]:443",           // v6 link-local
            "[::ffff:10.0.0.1]:443",   // v4-mapped private (smuggling)
        ] {
            assert!(!dialable(bad), "{bad} must not be dialable");
        }
    }
}
