//! Namespace succession & recovery crypto (spec §2.4). Two signed
//! statements, both deterministic-CBOR encode-before-sign and
//! domain-separated by a tag string so a signature for one can never be
//! replayed as the other:
//!
//! - **Transfer** (rung 1): the current root signs "hand `namespace` to
//!   `new_owner`". Verified against the stored root key.
//! - **Rotation** (rungs 2/3): a record naming a *new root key* + new
//!   owner, co-signed by an M-of-N recovery quorum (rung 2) or by the
//!   network/operator key (rung 3).
//!
//! No clock here — delay windows live in the server. This module only
//! answers "are these signatures valid for this statement?".

use serde::{Deserialize, Serialize};

use crate::keys::{Keypair, PublicKey, Signature};
use crate::{b64, CryptoError};

const TRANSFER_TAG: &str = "weft-ns-transfer/1";
const ROTATION_TAG: &str = "weft-ns-rotation/1";
const CANCEL_TAG: &str = "weft-ns-cancel/1";

fn cbor(value: &impl Serialize) -> Vec<u8> {
    let mut bytes = Vec::new();
    ciborium::into_writer(value, &mut bytes).expect("CBOR to Vec cannot fail");
    bytes
}

/// Canonical bytes a root signs to transfer ownership (rung 1).
fn transfer_bytes(namespace: &str, new_owner: &str) -> Vec<u8> {
    cbor(&(TRANSFER_TAG, namespace, new_owner))
}

/// Sign a rung-1 transfer with the current root key.
pub fn sign_transfer(root: &Keypair, namespace: &str, new_owner: &str) -> Signature {
    root.sign(&transfer_bytes(namespace, new_owner))
}

/// Verify a transfer signature against the namespace's current root key.
pub fn verify_transfer(
    root_key: &PublicKey,
    namespace: &str,
    new_owner: &str,
    signature: &Signature,
) -> bool {
    root_key
        .verify(&transfer_bytes(namespace, new_owner), signature)
        .is_ok()
}

/// Canonical bytes the current root signs to veto a pending recovery
/// (NS RECOVERY CANCEL, §2.4 — a live root always wins).
fn cancel_bytes(namespace: &str) -> Vec<u8> {
    cbor(&(CANCEL_TAG, namespace))
}

pub fn sign_cancel(root: &Keypair, namespace: &str) -> Signature {
    root.sign(&cancel_bytes(namespace))
}

pub fn verify_cancel(root_key: &PublicKey, namespace: &str, signature: &Signature) -> bool {
    root_key.verify(&cancel_bytes(namespace), signature).is_ok()
}

/// The statement a recovery rotates *to*: a new root key + new owner for a
/// namespace. Signed by the quorum (rung 2) or operator (rung 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationRecord {
    pub namespace: String,
    pub new_root_key: PublicKey,
    pub new_owner: String,
}

impl RotationRecord {
    fn signing_bytes(&self) -> Vec<u8> {
        cbor(&(
            ROTATION_TAG,
            &self.namespace,
            self.new_root_key.as_bytes().as_slice(),
            &self.new_owner,
        ))
    }

    pub fn sign(&self, signer: &Keypair) -> (PublicKey, Signature) {
        (signer.public(), signer.sign(&self.signing_bytes()))
    }
}

/// A rotation record plus its collected signatures. Submitted via
/// `NS RECOVER`; the server decides the rung by whose signatures verify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedRotation {
    pub record: RotationRecord,
    pub signatures: Vec<(PublicKey, Signature)>,
}

#[derive(Serialize, Deserialize)]
struct Wire {
    namespace: String,
    new_root_key: Vec<u8>,
    new_owner: String,
    /// (pubkey bytes, signature bytes) pairs.
    signatures: Vec<(Vec<u8>, Vec<u8>)>,
}

impl SignedRotation {
    /// Count the *distinct* quorum members with a valid signature. Rung 2
    /// applies iff this reaches `m`. Duplicate signers and non-quorum
    /// signers are ignored, so padding the list can't manufacture a quorum.
    pub fn quorum_signers(&self, quorum: &[PublicKey]) -> usize {
        let bytes = self.record.signing_bytes();
        let mut counted: Vec<&PublicKey> = Vec::new();
        for (signer, sig) in &self.signatures {
            if quorum.contains(signer)
                && !counted.contains(&signer)
                && signer.verify(&bytes, sig).is_ok()
            {
                counted.push(signer);
            }
        }
        counted.len()
    }

    /// Rung 3: does the operator (network key) vouch for this rotation?
    pub fn signed_by(&self, key: &PublicKey) -> bool {
        let bytes = self.record.signing_bytes();
        self.signatures
            .iter()
            .any(|(signer, sig)| signer == key && signer.verify(&bytes, sig).is_ok())
    }

    pub fn to_b64(&self) -> String {
        let wire = Wire {
            namespace: self.record.namespace.clone(),
            new_root_key: self.record.new_root_key.as_bytes().to_vec(),
            new_owner: self.record.new_owner.clone(),
            signatures: self
                .signatures
                .iter()
                .map(|(k, s)| (k.as_bytes().to_vec(), s.to_bytes().to_vec()))
                .collect(),
        };
        b64::encode(cbor(&wire))
    }

    pub fn from_b64(s: &str) -> Result<Self, CryptoError> {
        let wire: Wire =
            ciborium::from_reader(b64::decode(s)?.as_slice()).map_err(|_| CryptoError::BadToken)?;
        let signatures = wire
            .signatures
            .into_iter()
            .map(|(k, s)| {
                Ok((
                    PublicKey::from_bytes(&k)?,
                    Signature::from_slice(&s).map_err(|_| CryptoError::BadSignatureEncoding)?,
                ))
            })
            .collect::<Result<Vec<_>, CryptoError>>()?;
        Ok(SignedRotation {
            record: RotationRecord {
                namespace: wire.namespace,
                new_root_key: PublicKey::from_bytes(&wire.new_root_key)?,
                new_owner: wire.new_owner,
            },
            signatures,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rotation(new_root: &Keypair) -> RotationRecord {
        RotationRecord {
            namespace: "gaming".to_string(),
            new_root_key: new_root.public(),
            new_owner: "carol".to_string(),
        }
    }

    #[test]
    fn transfer_round_trips_and_is_domain_separated() {
        let root = Keypair::generate();
        let sig = sign_transfer(&root, "gaming", "bob");
        assert!(verify_transfer(&root.public(), "gaming", "bob", &sig));
        // Wrong namespace/owner, or a different key, all fail.
        assert!(!verify_transfer(&root.public(), "gaming", "eve", &sig));
        assert!(!verify_transfer(&root.public(), "other", "bob", &sig));
        assert!(!verify_transfer(
            &Keypair::generate().public(),
            "gaming",
            "bob",
            &sig
        ));
    }

    #[test]
    fn quorum_needs_m_distinct_valid_members() {
        let (a, b, c, outsider) = (
            Keypair::generate(),
            Keypair::generate(),
            Keypair::generate(),
            Keypair::generate(),
        );
        let quorum = vec![a.public(), b.public(), c.public()];
        let new_root = Keypair::generate();
        let record = rotation(&new_root);

        // Two quorum members sign → 2 distinct signers.
        let signed = SignedRotation {
            record: record.clone(),
            signatures: vec![record.sign(&a), record.sign(&b)],
        };
        assert_eq!(signed.quorum_signers(&quorum), 2);

        // Padding with an outsider and a duplicate can't inflate the count.
        let padded = SignedRotation {
            record: record.clone(),
            signatures: vec![record.sign(&a), record.sign(&a), record.sign(&outsider)],
        };
        assert_eq!(padded.quorum_signers(&quorum), 1);
    }

    #[test]
    fn tampered_rotation_signature_is_invalid() {
        let a = Keypair::generate();
        let quorum = vec![a.public()];
        let new_root = Keypair::generate();
        let mut signed = SignedRotation {
            record: rotation(&new_root),
            signatures: vec![rotation(&new_root).sign(&a)],
        };
        // Change the target owner after signing → signature no longer valid.
        signed.record.new_owner = "attacker".to_string();
        assert_eq!(signed.quorum_signers(&quorum), 0);
    }

    #[test]
    fn operator_single_signature_for_rung_three() {
        let operator = Keypair::generate();
        let new_root = Keypair::generate();
        let record = rotation(&new_root);
        let signed = SignedRotation {
            record: record.clone(),
            signatures: vec![record.sign(&operator)],
        };
        assert!(signed.signed_by(&operator.public()));
        assert!(!signed.signed_by(&Keypair::generate().public()));
    }

    #[test]
    fn signed_rotation_round_trips_through_b64() {
        let a = Keypair::generate();
        let new_root = Keypair::generate();
        let record = rotation(&new_root);
        let signed = SignedRotation {
            record: record.clone(),
            signatures: vec![record.sign(&a)],
        };
        let restored = SignedRotation::from_b64(&signed.to_b64()).unwrap();
        assert_eq!(restored, signed);
        assert_eq!(restored.quorum_signers(&[a.public()]), 1);
        assert!(SignedRotation::from_b64("!!!").is_err());
    }
}
