//! §11.8 media-mirroring pull authorization. When a network fetches a bridged
//! blob over a bridge data-plane stream, it signs the request with its **network
//! key**; the origin verifies against the peer key it already holds. This proves
//! the puller is the bridge peer (not a random client on the wire), so the pull
//! can't be used to bypass the origin's per-blob membership gating.
//!
//! The `origin` network is bound in so a request signed for one origin can't be
//! replayed against another (cf. the §6.1 challenge, invariant 5).

use crate::keys::{Keypair, PublicKey, Signature};

const DOMAIN: &[u8] = b"weft-mirror/1";

/// Domain-separated, unambiguous message: `weft-mirror/1 \0 hash \0 requester \0 origin`.
fn message(hash: &str, requester: &str, origin: &str) -> Vec<u8> {
    let mut msg = Vec::from(DOMAIN);
    for part in [hash, requester, origin] {
        msg.push(0);
        msg.extend_from_slice(part.as_bytes());
    }
    msg
}

/// Requester side: sign a pull of blob `hash` from `origin`, as `requester`.
pub fn sign_mirror_request(key: &Keypair, hash: &str, requester: &str, origin: &str) -> Signature {
    key.sign(&message(hash, requester, origin))
}

/// Origin side: verify a pull against the requester's network public key.
pub fn verify_mirror_request(
    requester_key: &PublicKey,
    hash: &str,
    requester: &str,
    origin: &str,
    signature: &Signature,
) -> bool {
    requester_key
        .verify(&message(hash, requester, origin), signature)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirror_request_round_trips() {
        let net2 = Keypair::generate();
        let sig = sign_mirror_request(&net2, "b3hash", "net2.example", "net1.example");
        assert!(verify_mirror_request(
            &net2.public(),
            "b3hash",
            "net2.example",
            "net1.example",
            &sig
        ));
    }

    #[test]
    fn wrong_hash_origin_or_requester_is_rejected() {
        let net2 = Keypair::generate();
        let sig = sign_mirror_request(&net2, "b3hash", "net2.example", "net1.example");
        // Tampered hash, origin, or requester → invalid.
        assert!(!verify_mirror_request(
            &net2.public(),
            "other",
            "net2.example",
            "net1.example",
            &sig
        ));
        assert!(!verify_mirror_request(
            &net2.public(),
            "b3hash",
            "net2.example",
            "evil.example",
            &sig
        ));
        assert!(!verify_mirror_request(
            &net2.public(),
            "b3hash",
            "imposter.example",
            "net1.example",
            &sig
        ));
        // A different key (not the claimed peer) → invalid.
        assert!(!verify_mirror_request(
            &Keypair::generate().public(),
            "b3hash",
            "net2.example",
            "net1.example",
            &sig
        ));
    }
}
