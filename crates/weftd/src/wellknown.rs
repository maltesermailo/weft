//! `/.well-known/weft` (§10.2): the network publishes its signing key so
//! remote networks can verify the attestations it issues. Served over
//! plain HTTP here — a real deployment terminates HTTPS in front (the
//! spec's `https://<network>/...` URL is the *public* address).

use std::sync::Arc;

use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use tokio::net::TcpListener;
use weft_core::ServerCtx;

#[derive(Debug, Clone, Serialize)]
struct WellKnownDoc {
    protocol: &'static str,
    network: String,
    /// Base64 Ed25519 public key — verifies `attestation=` blobs (§6.1).
    #[serde(rename = "signing-key")]
    signing_key: String,
}

pub(crate) fn router(ctx: &ServerCtx) -> Router {
    let doc = WellKnownDoc {
        protocol: weft_core::PROTOCOL_VERSION,
        network: ctx.info.network.to_string(),
        signing_key: ctx.identity_public().to_b64(),
    };
    Router::new().route(
        "/.well-known/weft",
        get(move || {
            let doc = doc.clone();
            async move { Json(doc) }
        }),
    )
}

pub(crate) async fn serve(listener: TcpListener, ctx: Arc<ServerCtx>) {
    if let Err(e) = axum::serve(listener, router(&ctx)).await {
        tracing::error!("well-known server failed: {e}");
    }
}
