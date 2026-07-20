//! # weft-crypto — WEFT identity primitives (L0)
//!
//! Ed25519 device/network keys (§10.2), signed attestations with
//! deterministic CBOR encode-before-sign (CLAUDE.md convention), the
//! `nonce‖network-name` challenge proof (§6.1), and constant-time password
//! verification (§6.1). Pure logic — no I/O, no tokio, no clocks: `now` is
//! always a parameter, so everything is testable and fuzzable in isolation.
//!
//! Layering note: this crate is a leaf like `weft-proto` and deliberately
//! does not depend on it — accounts/network names appear here as plain
//! strings, validated by the caller (weft-core) against proto's types.

#![forbid(unsafe_code)]

mod attestation;
mod caps;
mod captoken;
mod challenge;
mod keys;
mod manifest;
mod mirror;
mod password;
mod profile;
mod rotation;
mod voice;

pub use attestation::Attestation;
pub use caps::Capability;
pub use captoken::{verify_chain, Grant, Subject, Token, TokenScope, Verified};
pub use challenge::{sign_challenge, verify_challenge, CHALLENGE_NONCE_LEN};
pub use keys::{signature_from_b64, signature_to_b64, Keypair, PublicKey, Signature};
pub use manifest::{Manifest, SignedManifest};
pub use mirror::{sign_mirror_request, verify_mirror_request};
pub use password::PasswordHash;
pub use profile::{Profile, SignedProfile};
pub use rotation::{
    sign_cancel, sign_transfer, verify_cancel, verify_transfer, RotationRecord, SignedRotation,
};
pub use voice::{SignedVoiceRelayGrant, VoiceRelayGrant};

use thiserror::Error;

/// Errors from parsing or verifying identity material.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CryptoError {
    #[error("invalid base64")]
    BadEncoding,

    /// Wrong length or not a valid curve point.
    #[error("invalid Ed25519 key")]
    BadKey,

    #[error("invalid signature encoding")]
    BadSignatureEncoding,

    #[error("signature verification failed")]
    BadSignature,

    #[error("attestation expired")]
    Expired,

    #[error("malformed attestation")]
    BadAttestation,

    #[error("unknown capability")]
    BadCapability,

    #[error("malformed capability token")]
    BadToken,

    /// A token in the chain was issued before its scope's current
    /// revocation epoch (§10.4).
    #[error("capability revoked")]
    Revoked,

    /// The chain does not authorize the requested capability/scope.
    #[error("capability not authorized")]
    Unauthorized,
}

/// Standard base64 for all wire-facing key/signature/attestation material.
pub mod b64 {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;

    use crate::CryptoError;

    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        STANDARD.encode(bytes)
    }

    pub fn decode(s: &str) -> Result<Vec<u8>, CryptoError> {
        STANDARD.decode(s).map_err(|_| CryptoError::BadEncoding)
    }
}
