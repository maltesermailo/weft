//! Signed display-identity profiles (spec §10.3). A profile is the account's
//! **display name + avatar**, signed by the **home network key** so any remote
//! that already trusts that key (via `/.well-known/weft` or bridge peering) can
//! verify a federated user's profile offline. The avatar is bound by its
//! **BLAKE3 hash**, so a mirrored avatar blob (§11.8) cannot be substituted —
//! the receiver fetches the blob by hash and content-addressing does the rest.
//!
//! Deterministic-CBOR encode-before-sign, domain-separated by a tag string so a
//! profile signature can never be replayed as a manifest / transfer / mirror.
//! Modeled on `manifest.rs`; a leaf with no `weft-proto` dependency, so `account`
//! and `avatar` travel as plain strings the caller (weft-core) validates.

use serde::{Deserialize, Serialize};

use crate::keys::{Keypair, PublicKey, Signature};
use crate::{b64, CryptoError};

const PROFILE_TAG: &str = "weft-profile/1";

fn cbor(value: &impl Serialize) -> Vec<u8> {
    let mut bytes = Vec::new();
    ciborium::into_writer(value, &mut bytes).expect("CBOR to Vec cannot fail");
    bytes
}

/// The profile body (§10.3). `account` is the canonical `user@network`;
/// `display` is the optional nick (≤128 B, validated by the caller); `avatar` is
/// the optional avatar blob's **BLAKE3 hash** (empty = none); `updated` is a
/// unix-ms timestamp the caller stamps — last-writer-wins across devices, and
/// the monotonic guard against a stale profile replacing a newer one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub account: String,
    pub display: Option<String>,
    pub avatar: Option<String>,
    pub updated: u64,
}

impl Profile {
    /// Canonical bytes the home network key signs. Field order is fixed and
    /// tag-prefixed; any change to any field changes these bytes. `None` fields
    /// sign as empty strings so the encoding stays total.
    fn signing_bytes(&self) -> Vec<u8> {
        cbor(&(
            PROFILE_TAG,
            &self.account,
            self.display.as_deref().unwrap_or(""),
            self.avatar.as_deref().unwrap_or(""),
            self.updated,
        ))
    }

    /// Sign this profile with the home network key (§10.3 / §11.3).
    pub fn sign(&self, network: &Keypair) -> SignedProfile {
        SignedProfile {
            signer: network.public(),
            signature: network.sign(&self.signing_bytes()),
            profile: self.clone(),
        }
    }
}

/// A profile plus the home network's public key and signature. Crosses the
/// bridge attached to a member; both sides persist it so a remote can prove the
/// profile it shows was vouched for by the account's home network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedProfile {
    pub profile: Profile,
    pub signer: PublicKey,
    pub signature: Signature,
}

#[derive(Serialize, Deserialize)]
struct Wire {
    account: String,
    display: String,
    avatar: String,
    updated: u64,
    signer: Vec<u8>,
    signature: Vec<u8>,
}

impl SignedProfile {
    /// The public key that signed this profile — checked by the caller against
    /// the account's home network signing key.
    pub fn signer(&self) -> &PublicKey {
        &self.signer
    }

    /// Self-authentication: the embedded signer's signature is valid over the
    /// profile body. Says nothing about *authority* — the caller must confirm
    /// `signer()` is the home network's key.
    pub fn verify(&self) -> bool {
        self.signer
            .verify(&self.profile.signing_bytes(), &self.signature)
            .is_ok()
    }

    /// Valid AND signed by exactly `key` — the federated case where the verifier
    /// knows which network key must have signed (the account's home network).
    pub fn signed_by(&self, key: &PublicKey) -> bool {
        self.signer == *key && self.verify()
    }

    pub fn to_b64(&self) -> String {
        let wire = Wire {
            account: self.profile.account.clone(),
            display: self.profile.display.clone().unwrap_or_default(),
            avatar: self.profile.avatar.clone().unwrap_or_default(),
            updated: self.profile.updated,
            signer: self.signer.as_bytes().to_vec(),
            signature: self.signature.to_bytes().to_vec(),
        };
        b64::encode(cbor(&wire))
    }

    pub fn from_b64(s: &str) -> Result<Self, CryptoError> {
        let wire: Wire =
            ciborium::from_reader(b64::decode(s)?.as_slice()).map_err(|_| CryptoError::BadToken)?;
        let empty_to_none = |s: String| (!s.is_empty()).then_some(s);
        Ok(SignedProfile {
            profile: Profile {
                account: wire.account,
                display: empty_to_none(wire.display),
                avatar: empty_to_none(wire.avatar),
                updated: wire.updated,
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

    fn profile() -> Profile {
        Profile {
            account: "ada@hda.example".to_string(),
            display: Some("Ada L.".to_string()),
            avatar: Some("b3-abc123".to_string()),
            updated: 1_700_000_000_000,
        }
    }

    #[test]
    fn sign_and_verify_round_trip() {
        let network = Keypair::generate();
        let signed = profile().sign(&network);
        assert!(signed.verify());
        assert!(signed.signed_by(&network.public()));
        assert!(!signed.signed_by(&Keypair::generate().public()));
        assert_eq!(signed.signer(), &network.public());
    }

    #[test]
    fn tampered_profile_fails_verification() {
        let network = Keypair::generate();
        // Swapping the avatar hash after signing breaks it (anti-substitution).
        let mut signed = profile().sign(&network);
        signed.profile.avatar = Some("b3-evil".to_string());
        assert!(!signed.verify());
        // So does changing the display name.
        let mut renamed = profile().sign(&network);
        renamed.profile.display = Some("Impostor".to_string());
        assert!(!renamed.verify());
    }

    #[test]
    fn empty_fields_round_trip_as_none() {
        let network = Keypair::generate();
        let bare = Profile {
            account: "bob@hda.example".to_string(),
            display: None,
            avatar: None,
            updated: 42,
        };
        let signed = bare.sign(&network);
        assert!(signed.verify());
        let restored = SignedProfile::from_b64(&signed.to_b64()).unwrap();
        assert_eq!(restored, signed);
        assert_eq!(restored.profile.display, None);
        assert_eq!(restored.profile.avatar, None);
    }

    #[test]
    fn signed_profile_round_trips_through_b64() {
        let network = Keypair::generate();
        let signed = profile().sign(&network);
        let restored = SignedProfile::from_b64(&signed.to_b64()).unwrap();
        assert_eq!(restored, signed);
        assert!(restored.signed_by(&network.public()));
        assert!(SignedProfile::from_b64("!!!").is_err());
    }

    #[test]
    fn profile_signature_is_domain_separated() {
        // A different account under the same key must not verify with this sig.
        let network = Keypair::generate();
        let signed = profile().sign(&network);
        let mut other = profile();
        other.account = "eve@hda.example".to_string();
        assert!(network
            .public()
            .verify(
                &other.sign(&network).profile.signing_bytes(),
                &signed.signature
            )
            .is_err());
    }
}
