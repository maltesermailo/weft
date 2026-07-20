//! Signed bridge manifests (spec §11.1). A manifest is the authoritative,
//! offline-verifiable statement of *what a network agrees to forward to a
//! peer*: the channel snapshot plus the history / media / typing bounds. It is
//! signed by the **scope authority key** (§11.3): the network signing key for
//! `*`, the namespace root for `ns:<name>`, or a `bridge`-cap holder's key for
//! a single `#channel`. Both sides store the signed blob; forwarding outside
//! the last mutually-acked version is a protocol violation (§11.1, invariant 3).
//!
//! Deterministic-CBOR encode-before-sign, domain-separated by a tag string so a
//! manifest signature can never be replayed as a transfer/rotation (§2.4). No
//! clock here — `created`/`updated` are plain fields the caller stamps; this
//! module only answers "is this signature valid for this manifest?".
//!
//! Layering: like the rest of weft-crypto this is a leaf with no `weft-proto`
//! dependency, so `peer`/`channels`/`history`/`media` travel as plain strings
//! the caller (weft-core) validates against proto's types.

use serde::{Deserialize, Serialize};

use crate::keys::{Keypair, PublicKey, Signature};
use crate::{b64, CryptoError};

const MANIFEST_TAG: &str = "weft-manifest/1";

fn cbor(value: &impl Serialize) -> Vec<u8> {
    let mut bytes = Vec::new();
    ciborium::into_writer(value, &mut bytes).expect("CBOR to Vec cannot fail");
    bytes
}

/// The manifest body (§11.1). `history`/`media` are the wire string forms
/// (`from-epoch`|`full`, `mirror`|`mirror-max:<bytes>`|`none`); `typing`/`voice`
/// are the `yes|no` flags as bools (`voice` = whether voice channels in the
/// snapshot federate, §16). `created`/`updated` are unix-ms timestamps —
/// `created` is the `from-epoch` backfill boundary (§11.7, cheap ULID compare).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub peer: String,
    pub version: u64,
    pub channels: Vec<String>,
    pub history: String,
    pub media: String,
    pub typing: bool,
    pub voice: bool,
    pub created: u64,
    pub updated: u64,
}

impl Manifest {
    /// Canonical bytes the scope authority signs. Field order is fixed and
    /// tag-prefixed; any change to any field changes these bytes.
    fn signing_bytes(&self) -> Vec<u8> {
        cbor(&(
            MANIFEST_TAG,
            &self.peer,
            self.version,
            &self.channels,
            &self.history,
            &self.media,
            self.typing,
            self.voice,
            self.created,
            self.updated,
        ))
    }

    /// Sign this manifest with the scope authority key (§11.3).
    pub fn sign(&self, authority: &Keypair) -> SignedManifest {
        SignedManifest {
            signer: authority.public(),
            signature: authority.sign(&self.signing_bytes()),
            manifest: self.clone(),
        }
    }
}

/// A manifest plus the scope authority's public key and signature. Crosses the
/// bridge as the `manifest=<b64>` tag on `BRIDGE PROPOSE`; both sides persist
/// it so either can prove what version was mutually acked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedManifest {
    pub manifest: Manifest,
    pub signer: PublicKey,
    pub signature: Signature,
}

#[derive(Serialize, Deserialize)]
struct Wire {
    peer: String,
    version: u64,
    channels: Vec<String>,
    history: String,
    media: String,
    typing: bool,
    voice: bool,
    created: u64,
    updated: u64,
    signer: Vec<u8>,
    signature: Vec<u8>,
}

impl SignedManifest {
    /// The public key that signed this manifest. The caller checks it against
    /// the §11.3 scope authority (network key / ns root / `bridge`-cap holder).
    pub fn signer(&self) -> &PublicKey {
        &self.signer
    }

    /// Self-authentication: the embedded signer's signature is valid over the
    /// manifest body. Says nothing about *authority* — the caller must still
    /// confirm `signer()` is entitled to bridge this scope.
    pub fn verify(&self) -> bool {
        self.signer
            .verify(&self.manifest.signing_bytes(), &self.signature)
            .is_ok()
    }

    /// Valid AND signed by exactly `key` — the `*`/`ns:` case where the
    /// verifier already knows which key must have signed (network key / root).
    pub fn signed_by(&self, key: &PublicKey) -> bool {
        self.signer == *key && self.verify()
    }

    pub fn to_b64(&self) -> String {
        let wire = Wire {
            peer: self.manifest.peer.clone(),
            version: self.manifest.version,
            channels: self.manifest.channels.clone(),
            history: self.manifest.history.clone(),
            media: self.manifest.media.clone(),
            typing: self.manifest.typing,
            voice: self.manifest.voice,
            created: self.manifest.created,
            updated: self.manifest.updated,
            signer: self.signer.as_bytes().to_vec(),
            signature: self.signature.to_bytes().to_vec(),
        };
        b64::encode(cbor(&wire))
    }

    pub fn from_b64(s: &str) -> Result<Self, CryptoError> {
        let wire: Wire =
            ciborium::from_reader(b64::decode(s)?.as_slice()).map_err(|_| CryptoError::BadToken)?;
        Ok(SignedManifest {
            manifest: Manifest {
                peer: wire.peer,
                version: wire.version,
                channels: wire.channels,
                history: wire.history,
                media: wire.media,
                typing: wire.typing,
                voice: wire.voice,
                created: wire.created,
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

    fn manifest() -> Manifest {
        Manifest {
            peer: "hda.example".to_string(),
            version: 1,
            channels: vec!["#general".to_string(), "#gaming/lobby".to_string()],
            history: "from-epoch".to_string(),
            media: "mirror-max:1048576".to_string(),
            typing: true,
            voice: true,
            created: 1_700_000_000_000,
            updated: 1_700_000_000_000,
        }
    }

    #[test]
    fn sign_and_verify_round_trip() {
        let authority = Keypair::generate();
        let signed = manifest().sign(&authority);
        assert!(signed.verify());
        assert!(signed.signed_by(&authority.public()));
        assert!(!signed.signed_by(&Keypair::generate().public()));
        assert_eq!(signed.signer(), &authority.public());
    }

    #[test]
    fn tampered_manifest_fails_verification() {
        let authority = Keypair::generate();
        let mut signed = manifest().sign(&authority);
        // Sneak an extra channel into the snapshot after signing.
        signed.manifest.channels.push("#secret".to_string());
        assert!(!signed.verify());
        // Bumping the version after signing also breaks it.
        let mut bumped = manifest().sign(&authority);
        bumped.manifest.version = 2;
        assert!(!bumped.verify());
    }

    #[test]
    fn manifest_signature_is_domain_separated() {
        // A manifest signature must never verify as anything else. We can't
        // cross-check against transfer here without constructing one, but the
        // tag prefix guarantees distinct signing bytes; assert a re-signed
        // body with a different tag-adjacent field diverges.
        let authority = Keypair::generate();
        let signed = manifest().sign(&authority);
        let mut other = manifest();
        other.peer = "peer.example".to_string();
        assert!(authority
            .public()
            .verify(
                &other.sign(&authority).manifest.signing_bytes(),
                &signed.signature
            )
            .is_err());
    }

    #[test]
    fn signed_manifest_round_trips_through_b64() {
        let authority = Keypair::generate();
        let signed = manifest().sign(&authority);
        let restored = SignedManifest::from_b64(&signed.to_b64()).unwrap();
        assert_eq!(restored, signed);
        assert!(restored.signed_by(&authority.public()));
        assert!(SignedManifest::from_b64("!!!").is_err());
    }
}
