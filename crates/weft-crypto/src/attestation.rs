//! Device attestations (§10.2): the home network signs
//! `{pubkey, account, network, expiry}`. Remote networks verify against the
//! signing key published at `https://<network>/.well-known/weft`.
//!
//! Encode-before-sign (CLAUDE.md): the signed payload is a CBOR **array**
//! (fields in fixed positional order), so the byte encoding is fully
//! deterministic — no map-ordering questions to get wrong. Rotation is a
//! superseding attestation; revocation happens at the well-known endpoint,
//! not in this structure.

use serde::{Deserialize, Serialize};

use crate::keys::{Keypair, PublicKey, Signature};
use crate::{b64, CryptoError};

/// Format version, first element of the signed payload.
const VERSION: u8 = 1;

/// A signed device attestation. Field types are plain strings — validation
/// against proto's identifier grammar is the caller's job (layering: this
/// crate is a leaf).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attestation {
    pub device: PublicKey,
    pub account: String,
    pub network: String,
    /// Unix seconds. Rotation = superseding attestation, so lifetimes are
    /// deliberately short-ish; the issuer picks the horizon.
    pub expires_at: u64,
    pub signature: Signature,
}

/// Wire/storage form: the payload tuple plus the signature, CBOR-encoded
/// then base64. Byte vectors (not fixed arrays) keep serde simple.
#[derive(Serialize, Deserialize)]
struct WireAttestation(u8, String, Vec<u8>, u64, String, Vec<u8>);

/// The exact bytes that are signed: CBOR of
/// `[version, account, device-key, expires-at, network]`.
fn payload_bytes(device: &PublicKey, account: &str, network: &str, expires_at: u64) -> Vec<u8> {
    let payload = (
        VERSION,
        account,
        device.as_bytes().as_slice(),
        expires_at,
        network,
    );
    let mut bytes = Vec::new();
    ciborium::into_writer(&payload, &mut bytes).expect("CBOR to Vec cannot fail");
    bytes
}

impl Attestation {
    /// Issue: the network signing key attests that `device` belongs to
    /// `account@network` until `expires_at`.
    pub fn sign(
        network_key: &Keypair,
        device: PublicKey,
        account: &str,
        network: &str,
        expires_at: u64,
    ) -> Self {
        let signature = network_key.sign(&payload_bytes(&device, account, network, expires_at));
        Self {
            device,
            account: account.to_string(),
            network: network.to_string(),
            expires_at,
            signature,
        }
    }

    /// Verify signature and expiry against the issuing network's public
    /// key. Backfilled/bridged attestations verify exactly like live ones
    /// (§11.7) — there is only this one path.
    pub fn verify(&self, network_key: &PublicKey, now: u64) -> Result<(), CryptoError> {
        if now >= self.expires_at {
            return Err(CryptoError::Expired);
        }
        network_key.verify(
            &payload_bytes(&self.device, &self.account, &self.network, self.expires_at),
            &self.signature,
        )
    }

    /// Wire form for the `attestation=` tag (§6.1).
    pub fn to_b64(&self) -> String {
        let wire = WireAttestation(
            VERSION,
            self.account.clone(),
            self.device.as_bytes().to_vec(),
            self.expires_at,
            self.network.clone(),
            self.signature.to_bytes().to_vec(),
        );
        let mut bytes = Vec::new();
        ciborium::into_writer(&wire, &mut bytes).expect("CBOR to Vec cannot fail");
        b64::encode(bytes)
    }

    pub fn from_b64(s: &str) -> Result<Self, CryptoError> {
        let bytes = b64::decode(s)?;
        let WireAttestation(version, account, device, expires_at, network, signature) =
            ciborium::from_reader(bytes.as_slice()).map_err(|_| CryptoError::BadAttestation)?;
        if version != VERSION {
            return Err(CryptoError::BadAttestation);
        }
        Ok(Self {
            device: PublicKey::from_bytes(&device)?,
            account,
            network,
            expires_at,
            signature: Signature::from_slice(&signature)
                .map_err(|_| CryptoError::BadSignatureEncoding)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(expires_at: u64) -> (Keypair, Attestation) {
        let network_key = Keypair::generate();
        let device = Keypair::generate().public();
        let attestation = Attestation::sign(&network_key, device, "ada", "hda.example", expires_at);
        (network_key, attestation)
    }

    #[test]
    fn sign_verify_and_wire_round_trip() {
        let (network_key, attestation) = issue(1_000);
        assert!(attestation.verify(&network_key.public(), 999).is_ok());

        let restored = Attestation::from_b64(&attestation.to_b64()).unwrap();
        assert_eq!(restored, attestation);
        assert!(restored.verify(&network_key.public(), 999).is_ok());
    }

    #[test]
    fn expiry_is_enforced() {
        let (network_key, attestation) = issue(1_000);
        assert_eq!(
            attestation.verify(&network_key.public(), 1_000),
            Err(CryptoError::Expired)
        );
    }

    #[test]
    fn tampered_fields_fail_verification() {
        let (network_key, attestation) = issue(1_000);
        let key = network_key.public();

        let mut forged = attestation.clone();
        forged.account = "eve".to_string();
        assert_eq!(forged.verify(&key, 0), Err(CryptoError::BadSignature));

        let mut forged = attestation.clone();
        forged.network = "evil.example".to_string();
        assert_eq!(forged.verify(&key, 0), Err(CryptoError::BadSignature));

        let mut forged = attestation;
        forged.expires_at = u64::MAX; // stretching the lifetime breaks the sig
        assert_eq!(forged.verify(&key, 0), Err(CryptoError::BadSignature));
    }

    #[test]
    fn wrong_issuer_fails_verification() {
        let (_, attestation) = issue(1_000);
        let other = Keypair::generate();
        assert_eq!(
            attestation.verify(&other.public(), 0),
            Err(CryptoError::BadSignature)
        );
    }

    #[test]
    fn garbage_wire_forms_are_rejected() {
        assert!(Attestation::from_b64("not base64 !!!").is_err());
        assert!(Attestation::from_b64(&b64::encode(b"not cbor")).is_err());
    }
}
