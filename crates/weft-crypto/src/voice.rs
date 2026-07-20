//! Signed voice-relay grants (§16 federated voice). When a home network H asks a
//! foreign network F to relay one of F's voice channels (a cascaded-SFU bridge),
//! F answers with a `SignedVoiceRelayGrant`: F's network-key-signed statement
//! that H may relay `channel` (as LiveKit room `room`) until `expiry`. Like the
//! bridge manifest it is offline-verifiable against F's network key, so the
//! WEFT-level authorization is durable + auditable — distinct from the ephemeral
//! LiveKit access JWT that carries the actual media credential (the two-token
//! model: this grant authorizes, the JWT admits to the media room).
//!
//! Deterministic-CBOR encode-before-sign, domain-separated by a tag so a
//! voice-relay signature can never be replayed as a manifest / transfer /
//! rotation, and vice-versa.

use serde::{Deserialize, Serialize};

use crate::keys::{Keypair, PublicKey, Signature};
use crate::{b64, CryptoError};

const VOICE_RELAY_TAG: &str = "weft-voice-relay/1";

fn cbor(value: &impl Serialize) -> Vec<u8> {
    let mut bytes = Vec::new();
    ciborium::into_writer(value, &mut bytes).expect("CBOR to Vec cannot fail");
    bytes
}

/// The grant body. `issuer` is the network authorizing (and signing) the relay;
/// `grantee` is the network allowed to relay `channel`; `room` is the LiveKit
/// room id the relay will join on the issuer's LiveKit; `expiry` is unix-ms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceRelayGrant {
    pub issuer: String,
    pub grantee: String,
    pub channel: String,
    pub room: String,
    pub expiry: u64,
}

impl VoiceRelayGrant {
    /// Canonical bytes the issuer network key signs. Field order is fixed and
    /// tag-prefixed; any change to any field changes these bytes.
    fn signing_bytes(&self) -> Vec<u8> {
        cbor(&(
            VOICE_RELAY_TAG,
            &self.issuer,
            &self.grantee,
            &self.channel,
            &self.room,
            self.expiry,
        ))
    }

    /// Sign with the issuer network's signing key (the §11.3 `*` authority).
    pub fn sign(&self, authority: &Keypair) -> SignedVoiceRelayGrant {
        SignedVoiceRelayGrant {
            signer: authority.public(),
            signature: authority.sign(&self.signing_bytes()),
            grant: self.clone(),
        }
    }
}

/// A grant plus the issuer's public key + signature. Crosses the bridge as the
/// `grant=<b64>` tag on `VOICE GRANT`; the grantee persists it as durable proof
/// of what the issuer authorized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedVoiceRelayGrant {
    pub grant: VoiceRelayGrant,
    pub signer: PublicKey,
    pub signature: Signature,
}

#[derive(Serialize, Deserialize)]
struct Wire {
    issuer: String,
    grantee: String,
    channel: String,
    room: String,
    expiry: u64,
    signer: Vec<u8>,
    signature: Vec<u8>,
}

impl SignedVoiceRelayGrant {
    /// The public key that signed this grant — the caller checks it against the
    /// issuer network's known key (§11.3 authority for `*`).
    pub fn signer(&self) -> &PublicKey {
        &self.signer
    }

    /// Self-authentication: the embedded signer's signature is valid over the
    /// grant body. Says nothing about authority — the caller confirms the signer
    /// is the issuer network's key.
    pub fn verify(&self) -> bool {
        self.signer
            .verify(&self.grant.signing_bytes(), &self.signature)
            .is_ok()
    }

    /// Valid AND signed by exactly `key` (the issuer network's key).
    pub fn signed_by(&self, key: &PublicKey) -> bool {
        self.signer == *key && self.verify()
    }

    /// Valid, signed by `key`, and not yet expired at `now` (unix-ms).
    pub fn valid_at(&self, key: &PublicKey, now: u64) -> bool {
        self.signed_by(key) && now < self.grant.expiry
    }

    pub fn to_b64(&self) -> String {
        let wire = Wire {
            issuer: self.grant.issuer.clone(),
            grantee: self.grant.grantee.clone(),
            channel: self.grant.channel.clone(),
            room: self.grant.room.clone(),
            expiry: self.grant.expiry,
            signer: self.signer.as_bytes().to_vec(),
            signature: self.signature.to_bytes().to_vec(),
        };
        b64::encode(cbor(&wire))
    }

    pub fn from_b64(s: &str) -> Result<Self, CryptoError> {
        let wire: Wire =
            ciborium::from_reader(b64::decode(s)?.as_slice()).map_err(|_| CryptoError::BadToken)?;
        Ok(SignedVoiceRelayGrant {
            grant: VoiceRelayGrant {
                issuer: wire.issuer,
                grantee: wire.grantee,
                channel: wire.channel,
                room: wire.room,
                expiry: wire.expiry,
            },
            signer: PublicKey::from_bytes(&wire.signer)?,
            signature: Signature::from_slice(&wire.signature)
                .map_err(|_| CryptoError::BadSignatureEncoding)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant() -> VoiceRelayGrant {
        VoiceRelayGrant {
            issuer: "fda.example".to_string(),
            grantee: "hda.example".to_string(),
            channel: "#lounge".to_string(),
            room: "wv:fda.example:#lounge".to_string(),
            expiry: 1_700_000_600_000,
        }
    }

    #[test]
    fn sign_and_verify_round_trip() {
        let issuer = Keypair::generate();
        let signed = grant().sign(&issuer);
        assert!(signed.verify());
        assert!(signed.signed_by(&issuer.public()));
        assert!(!signed.signed_by(&Keypair::generate().public()));
        assert_eq!(signed.signer(), &issuer.public());
    }

    #[test]
    fn valid_at_honours_expiry() {
        let issuer = Keypair::generate();
        let signed = grant().sign(&issuer);
        assert!(signed.valid_at(&issuer.public(), 1_700_000_000_000));
        // At/after expiry it is no longer valid.
        assert!(!signed.valid_at(&issuer.public(), 1_700_000_600_000));
        assert!(!signed.valid_at(&issuer.public(), 1_700_000_600_001));
        // A different key never validates it.
        assert!(!signed.valid_at(&Keypair::generate().public(), 1_700_000_000_000));
    }

    #[test]
    fn tampered_grant_fails_verification() {
        let issuer = Keypair::generate();
        let mut signed = grant().sign(&issuer);
        // Swap the grantee after signing — a different network can't ride it.
        signed.grant.grantee = "evil.example".to_string();
        assert!(!signed.verify());
    }

    #[test]
    fn round_trips_through_b64() {
        let issuer = Keypair::generate();
        let signed = grant().sign(&issuer);
        let restored = SignedVoiceRelayGrant::from_b64(&signed.to_b64()).unwrap();
        assert_eq!(restored, signed);
        assert!(restored.signed_by(&issuer.public()));
        assert!(SignedVoiceRelayGrant::from_b64("!!!").is_err());
    }
}
