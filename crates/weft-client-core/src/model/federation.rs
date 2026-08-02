//! Federation domain (§11) — the operator-facing block-list + peering-manifest
//! live state. The Rust twin of `federationHandlers`: it reshapes NETBLOCKED /
//! NETBLOCK-REMOVED / MANIFEST wire events into diffs the mirror applies onto
//! `store.federation`. Stateless — each event maps directly to a diff. The
//! block-list clear-on-refresh stays TS (a UI operation on the mirror), as do the
//! operator RPC wrappers. `NETBLOCKED` (block, now carrying its reason) → set;
//! `NETBLOCK-REMOVED` (unblock) → drop — the two are now distinct verbs, so a
//! removal no longer re-adds the entry (the former §11.6 "netblock quirk").

use serde::Serialize;

use crate::ClientEvent;

/// A live peering manifest (§11.6), mirroring the TS `ManifestInfo`. The wire
/// event's `voice` flag isn't surfaced here (the panel doesn't show it).
#[derive(Serialize, Clone)]
pub struct ManifestInfo {
    pub peer: String,
    pub version: u64,
    pub state: String,
    pub channels: Vec<String>,
    pub history: String,
    pub media: String,
    pub typing: bool,
}

/// This domain's state diffs — the mirror applies them onto `store.federation`.
/// `NetblockSet`/`NetblockDrop` set/drop `netblocks[network]`; `ManifestSet`/
/// `ManifestDrop` set/drop `manifests[peer]`. The refresh-clear is a TS mirror op.
#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum FederationDiff {
    NetblockSet { network: String, reason: Option<String> },
    NetblockDrop { network: String },
    ManifestSet { manifest: ManifestInfo },
    ManifestDrop { peer: String },
}

/// The federation sub-model — stateless (the mirror holds the maps).
#[derive(Default)]
pub struct Federation;

impl Federation {
    pub fn handle(&mut self, event: &ClientEvent) -> Vec<FederationDiff> {
        match event {
            // §11.6 a blocked network + reason.
            ClientEvent::Netblocked { network, reason } => {
                vec![FederationDiff::NetblockSet { network: network.clone(), reason: reason.clone() }]
            }
            // §11.6 an un-blocked network → drop it (distinct from a block now).
            ClientEvent::NetblockRemoved { network } => {
                vec![FederationDiff::NetblockDrop { network: network.clone() }]
            }
            // §11 a bridge's manifest: `severed`/`removed` drops it, any other state sets it.
            ClientEvent::Manifest { peer, state, .. } if state == "severed" || state == "removed" => {
                vec![FederationDiff::ManifestDrop { peer: peer.clone() }]
            }
            ClientEvent::Manifest { peer, version, state, channels, history, media, typing, .. } => {
                vec![FederationDiff::ManifestSet {
                    manifest: ManifestInfo {
                        peer: peer.clone(),
                        version: *version,
                        state: state.clone(),
                        channels: channels.clone(),
                        history: history.clone(),
                        media: media.clone(),
                        typing: *typing,
                    },
                }]
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(peer: &str, state: &str) -> ClientEvent {
        ClientEvent::Manifest {
            peer: peer.into(),
            version: 3,
            state: state.into(),
            channels: vec!["#n/c".into()],
            history: "recent".into(),
            media: "mirror".into(),
            typing: true,
            voice: false,
        }
    }

    #[test]
    fn netblocked_maps_to_set() {
        let mut f = Federation;
        let d = f.handle(&ClientEvent::Netblocked { network: "evil.example".into(), reason: Some("spam".into()) });
        assert!(matches!(&d[0],
            FederationDiff::NetblockSet { network, reason } if network == "evil.example" && reason.as_deref() == Some("spam")));
    }

    #[test]
    fn netblock_removed_maps_to_drop() {
        let mut f = Federation;
        let d = f.handle(&ClientEvent::NetblockRemoved { network: "evil.example".into() });
        assert!(matches!(&d[0], FederationDiff::NetblockDrop { network } if network == "evil.example"));
    }

    #[test]
    fn manifest_sets_then_severed_drops() {
        let mut f = Federation;
        // An active manifest → set, carrying its fields.
        let d = f.handle(&manifest("peer.net", "active"));
        assert!(matches!(&d[0],
            FederationDiff::ManifestSet { manifest } if manifest.peer == "peer.net" && manifest.channels == ["#n/c"] && manifest.typing));
        // `severed` and `removed` both drop it.
        assert!(matches!(&f.handle(&manifest("peer.net", "severed"))[0], FederationDiff::ManifestDrop { peer } if peer == "peer.net"));
        assert!(matches!(&f.handle(&manifest("peer.net", "removed"))[0], FederationDiff::ManifestDrop { .. }));
    }
}
