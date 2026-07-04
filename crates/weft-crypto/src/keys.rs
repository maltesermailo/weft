//! Ed25519 wrappers (§10.2). One key shape serves both roles: device keys
//! (client-held) and the network signing key (operator-held); what a key
//! *means* is decided by where it is used.

use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};

use crate::{b64, CryptoError};

pub use ed25519_dalek::Signature;

/// A validated Ed25519 public key (32 bytes, a real curve point).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PublicKey([u8; 32]);

impl PublicKey {
    /// Validates the point on construction so later verification can't
    /// fail on key shape.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CryptoError> {
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| CryptoError::BadKey)?;
        VerifyingKey::from_bytes(&bytes).map_err(|_| CryptoError::BadKey)?;
        Ok(Self(bytes))
    }

    pub fn from_b64(s: &str) -> Result<Self, CryptoError> {
        Self::from_bytes(&b64::decode(s)?)
    }

    pub fn to_b64(&self) -> String {
        b64::encode(self.0)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn verify(&self, message: &[u8], signature: &Signature) -> Result<(), CryptoError> {
        let key = VerifyingKey::from_bytes(&self.0).map_err(|_| CryptoError::BadKey)?;
        key.verify(message, signature)
            .map_err(|_| CryptoError::BadSignature)
    }
}

/// A signing keypair (device key or network signing key).
pub struct Keypair(SigningKey);

impl Keypair {
    pub fn generate() -> Self {
        Self(SigningKey::generate(&mut rand::rngs::OsRng))
    }

    /// From a 32-byte seed (how weftd persists the network key).
    pub fn from_seed(seed: &[u8]) -> Result<Self, CryptoError> {
        let seed: [u8; 32] = seed.try_into().map_err(|_| CryptoError::BadKey)?;
        Ok(Self(SigningKey::from_bytes(&seed)))
    }

    pub fn from_seed_b64(s: &str) -> Result<Self, CryptoError> {
        Self::from_seed(&b64::decode(s)?)
    }

    pub fn seed_b64(&self) -> String {
        b64::encode(self.0.to_bytes())
    }

    pub fn public(&self) -> PublicKey {
        PublicKey(self.0.verifying_key().to_bytes())
    }

    pub fn sign(&self, message: &[u8]) -> Signature {
        self.0.sign(message)
    }
}

/// Decode a base64 signature (64 bytes).
pub fn signature_from_b64(s: &str) -> Result<Signature, CryptoError> {
    Signature::from_slice(&b64::decode(s)?).map_err(|_| CryptoError::BadSignatureEncoding)
}

/// Encode a signature for the wire.
pub fn signature_to_b64(signature: &Signature) -> String {
    b64::encode(signature.to_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_through_b64() {
        let keypair = Keypair::generate();
        let public = PublicKey::from_b64(&keypair.public().to_b64()).unwrap();
        assert_eq!(public, keypair.public());
        let restored = Keypair::from_seed_b64(&keypair.seed_b64()).unwrap();
        assert_eq!(restored.public(), keypair.public());
    }

    #[test]
    fn sign_verify_and_reject_tamper() {
        let keypair = Keypair::generate();
        let sig = keypair.sign(b"message");
        assert!(keypair.public().verify(b"message", &sig).is_ok());
        assert_eq!(
            keypair.public().verify(b"messagE", &sig),
            Err(CryptoError::BadSignature)
        );
        // A different key must not verify.
        let other = Keypair::generate();
        assert!(other.public().verify(b"message", &sig).is_err());
    }

    #[test]
    fn rejects_malformed_key_material() {
        assert_eq!(PublicKey::from_b64("!!!"), Err(CryptoError::BadEncoding));
        assert_eq!(PublicKey::from_bytes(&[0u8; 31]), Err(CryptoError::BadKey));
        assert!(signature_from_b64(&crate::b64::encode([0u8; 10])).is_err());
    }
}
