//! AUTH KEY challenge proof (§6.1): the client signs `nonce ‖ network-name`.
//! Binding the network name into the signed message is what prevents a
//! MITM'd or malicious network from replaying the proof against the user's
//! account on a *different* network (security invariant 5).

use crate::keys::{Keypair, PublicKey, Signature};

/// §6.1: challenges are 32-byte nonces.
pub const CHALLENGE_NONCE_LEN: usize = 32;

fn message(nonce: &[u8], network: &str) -> Vec<u8> {
    let mut msg = Vec::with_capacity(nonce.len() + network.len());
    msg.extend_from_slice(nonce);
    msg.extend_from_slice(network.as_bytes());
    msg
}

/// Client side: prove possession of the device key for this network.
pub fn sign_challenge(device: &Keypair, nonce: &[u8], network: &str) -> Signature {
    device.sign(&message(nonce, network))
}

/// Server side: check the proof against the claimed device key.
pub fn verify_challenge(
    device: &PublicKey,
    nonce: &[u8],
    network: &str,
    signature: &Signature,
) -> bool {
    device.verify(&message(nonce, network), signature).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proof_round_trips() {
        let device = Keypair::generate();
        let nonce = [7u8; CHALLENGE_NONCE_LEN];
        let sig = sign_challenge(&device, &nonce, "hda.example");
        assert!(verify_challenge(
            &device.public(),
            &nonce,
            "hda.example",
            &sig
        ));
    }

    #[test]
    fn cross_network_replay_is_rejected() {
        // Invariant 5: a proof minted for one network is dead on another.
        let device = Keypair::generate();
        let nonce = [7u8; CHALLENGE_NONCE_LEN];
        let sig = sign_challenge(&device, &nonce, "evil.example");
        assert!(!verify_challenge(
            &device.public(),
            &nonce,
            "hda.example",
            &sig
        ));
    }

    #[test]
    fn stale_nonce_is_rejected() {
        let device = Keypair::generate();
        let sig = sign_challenge(&device, &[1u8; 32], "hda.example");
        assert!(!verify_challenge(
            &device.public(),
            &[2u8; 32],
            "hda.example",
            &sig
        ));
    }
}
