//! Federation helpers (§11): manifest construction/verification and the
//! forwarding gate (invariant 3). The peering *state machine* and event
//! ingestion live in the session; this module is the pure, testable core —
//! "given these stored manifests, what may cross this bridge?".

use weft_crypto::{Manifest, PublicKey, SignedManifest};
use weft_proto::{ChannelName, HistoryMode, MediaMode, NetworkName};
use weft_store::PeerRecord;

/// Build the manifest body a network signs to bridge `channels` to `peer`
/// (§11.1). `history`/`media` are stored as their wire string forms.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_manifest(
    peer: &NetworkName,
    version: u64,
    channels: &[ChannelName],
    history: HistoryMode,
    media: MediaMode,
    typing: bool,
    voice: bool,
    created_ms: u64,
    updated_ms: u64,
) -> Manifest {
    Manifest {
        peer: peer.to_string(),
        version,
        channels: channels.iter().map(ChannelName::to_string).collect(),
        history: history.to_string(),
        media: media.to_string(),
        typing,
        voice,
        created: created_ms,
        updated: updated_ms,
    }
}

/// Verify a manifest received from a peer (§11.4): it must be signed by the
/// peer's pinned network key and name *us* as its peer. Returns the decoded
/// manifest on success. The §11.3 authority ladder is enforced on the signing
/// side; the wire artifact is uniformly network-key-signed.
pub(crate) fn verify_incoming(
    signed: &SignedManifest,
    peer_key: &PublicKey,
    our_network: &NetworkName,
) -> bool {
    signed.signed_by(peer_key) && signed.manifest.peer == our_network.as_str()
}

/// Channels currently forwardable to `peer` (invariant 3): present in **both**
/// the last mutually-acked snapshot and the current one. A `BRIDGE ADD` not
/// yet re-acked is in the current-but-not-acked set (blocked until re-ack); a
/// `BRIDGE REMOVE` is acked-but-not-current (stopped at once).
pub(crate) fn forwardable_channels(peer: &PeerRecord) -> Vec<String> {
    if peer.severed {
        return Vec::new();
    }
    let Some(acked) = peer.acked_manifest.as_deref() else {
        return Vec::new(); // not live until a mutual ack
    };
    let (Ok(acked), Ok(current)) = (
        SignedManifest::from_b64(acked),
        SignedManifest::from_b64(&peer.manifest),
    ) else {
        return Vec::new();
    };
    acked
        .manifest
        .channels
        .into_iter()
        .filter(|c| current.manifest.channels.contains(c))
        .collect()
}

/// Is `channel` in the mutually-acked, still-current bridge to `peer`?
pub(crate) fn is_forwardable(peer: &PeerRecord, channel: &str) -> bool {
    forwardable_channels(peer).iter().any(|c| c == channel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use weft_crypto::Keypair;

    fn signed(peer: &str, version: u64, channels: &[&str], key: &Keypair) -> String {
        build_manifest(
            &peer.parse().unwrap(),
            version,
            &channels
                .iter()
                .map(|c| c.parse().unwrap())
                .collect::<Vec<_>>(),
            HistoryMode::FromEpoch,
            MediaMode::None,
            false,
            false,
            0,
            0,
        )
        .sign(key)
        .to_b64()
    }

    fn peer_record(manifest: String, acked: Option<String>, severed: bool) -> PeerRecord {
        PeerRecord {
            peer: "peer.example".parse().unwrap(),
            scope: "*".to_string(),
            manifest,
            version: 1,
            acked_manifest: acked,
            severed,
            created_ms: 0,
            updated_ms: 0,
        }
    }

    #[test]
    fn verify_incoming_checks_signer_and_peer() {
        let key = Keypair::generate();
        let blob = signed("us.example", 1, &["#general"], &key);
        let sm = SignedManifest::from_b64(&blob).unwrap();
        assert!(verify_incoming(
            &sm,
            &key.public(),
            &"us.example".parse().unwrap()
        ));
        // Wrong signer or wrong named peer both fail.
        assert!(!verify_incoming(
            &sm,
            &Keypair::generate().public(),
            &"us.example".parse().unwrap()
        ));
        assert!(!verify_incoming(
            &sm,
            &key.public(),
            &"other.example".parse().unwrap()
        ));
    }

    #[test]
    fn gating_needs_acked_and_current_intersection() {
        let key = Keypair::generate();
        // Proposed but never acked → nothing forwards.
        let m1 = signed("us.example", 1, &["#general"], &key);
        let proposed = peer_record(m1.clone(), None, false);
        assert!(!is_forwardable(&proposed, "#general"));

        // Live at v1 (acked == current).
        let live = peer_record(m1.clone(), Some(m1.clone()), false);
        assert!(is_forwardable(&live, "#general"));

        // BRIDGE ADD → current has #new but acked (v1) does not: #new blocked,
        // #general still fine.
        let m2 = signed("us.example", 2, &["#general", "#new"], &key);
        let adding = peer_record(m2.clone(), Some(m1.clone()), false);
        assert!(is_forwardable(&adding, "#general"));
        assert!(!is_forwardable(&adding, "#new"));

        // BRIDGE REMOVE → current drops #general while acked still has it:
        // stopped immediately.
        let m3 = signed("us.example", 3, &[], &key);
        let removing = peer_record(m3, Some(m1.clone()), false);
        assert!(!is_forwardable(&removing, "#general"));

        // Severed → nothing.
        let severed = peer_record(m1.clone(), Some(m1), true);
        assert!(!is_forwardable(&severed, "#general"));
    }
}
