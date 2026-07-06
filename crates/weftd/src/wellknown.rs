//! `/.well-known/weft` (§10.2): the network publishes its signing key so
//! remote networks can verify the attestations it issues. Served over
//! plain HTTP here — a real deployment terminates HTTPS in front (the
//! spec's `https://<network>/...` URL is the *public* address).

use std::sync::Arc;

use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use weft_core::ServerCtx;

use crate::tls::Challenges;

#[derive(Debug, Clone, Serialize)]
struct WellKnownDoc {
    protocol: &'static str,
    network: String,
    /// Base64 Ed25519 public key — verifies `attestation=` blobs (§6.1).
    #[serde(rename = "signing-key")]
    signing_key: String,
}

pub(crate) fn router(ctx: &ServerCtx, challenges: Challenges) -> Router {
    let doc = WellKnownDoc {
        protocol: weft_core::PROTOCOL_VERSION,
        network: ctx.info.network.to_string(),
        signing_key: ctx.identity_public().to_b64(),
    };
    Router::new()
        .route(
            "/.well-known/weft",
            get(move || {
                let doc = doc.clone();
                async move { Json(doc) }
            }),
        )
        // ACME HTTP-01 validation (built-in Let's Encrypt). Empty unless ACME is
        // running, in which case the challenge task fills `challenges`.
        .route(
            "/.well-known/acme-challenge/:token",
            get(move |axum::extract::Path(token): axum::extract::Path<String>| {
                let challenges = Arc::clone(&challenges);
                async move {
                    challenges
                        .read()
                        .expect("challenges lock")
                        .get(&token)
                        .cloned()
                        .ok_or(axum::http::StatusCode::NOT_FOUND)
                }
            }),
        )
}

