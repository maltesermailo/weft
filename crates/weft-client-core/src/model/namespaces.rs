//! Namespaces domain (§2/§6.2) — the namespace **descriptor** (name / title /
//! description / owner / visibility / federation / recovery announcement). The
//! Rust twin of `Server.applyMeta`: it reshapes NS-META into an `ns-descriptor`
//! diff the mirror absorbs. Stateless. The namespace's **categories** ride the
//! separate `cat-list` diff ([`channels`](super::channels)); membership (`joined`),
//! the deletion drop + nav, and the owner auto-join stay TS side-effects.
//!
//! A §6.2 **deletion marker** (owner cleared + `description == "deleted"`) emits
//! nothing — applying its descriptor would resurrect the server the TS deletion
//! branch is dropping.

use serde::Serialize;

use crate::ClientEvent;

/// This domain's state diff — the mirror applies it via `Server.applyMeta`. The
/// wire NS-META carries no `welcome` (the descriptor's `welcome` stays TS-null,
/// matching the current behavior), and categories ride `cat-list`.
#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum NsDiff {
    NsDescriptor {
        id: String,
        name: String,
        title: Option<String>,
        description: Option<String>,
        owner: Option<String>,
        visibility: String,
        federation: bool,
        recovery_eta: Option<u64>,
        recovery_rung: Option<u8>,
        /// Foreign-bridge §7a.2: the provider-managed replica's origin URI
        /// (`matrix://teamnight.app/test`), or `None` for a native namespace.
        /// The UI needs it because a foreign namespace's local vanity name is a
        /// collision-suffixed placeholder (`test-fp1n`) that names nothing a
        /// person recognises — the origin is the only handle that does.
        origin: Option<String>,
        /// §9 liveness: is the provider governing this replica connected?
        /// `None` for a native namespace — nothing governs it, so it is never
        /// offline. `Some(false)` means the namespace exists but cannot serve:
        /// weftd refuses joins and writes into it, so the client badges it and
        /// declines to open it rather than showing a dead room.
        provider_online: Option<bool>,
    },
}

/// The namespaces sub-model — stateless (the mirror holds the `Server` records).
#[derive(Default)]
pub struct Namespaces;

impl Namespaces {
    pub fn handle(&mut self, event: &ClientEvent) -> Vec<NsDiff> {
        let ClientEvent::NsMeta {
            id,
            name,
            visibility,
            owner,
            title,
            description,
            recovery_eta,
            recovery_rung,
            federation,
            origin,
            provider_online,
            ..
        } = event
        else {
            return Vec::new();
        };

        // §6.2 deletion tombstone — don't apply the descriptor (it would resurrect
        // the server the TS deletion branch is dropping).
        if owner.is_none() && description.as_deref() == Some("deleted") {
            return Vec::new();
        }

        vec![NsDiff::NsDescriptor {
            id: id.clone(),
            name: name.clone(),
            title: title.clone(),
            description: description.clone(),
            owner: owner.clone(),
            visibility: visibility.clone(),
            federation: *federation,
            recovery_eta: *recovery_eta,
            recovery_rung: *recovery_rung,
            origin: origin.clone(),
            provider_online: *provider_online,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same event, marked as a provider-managed replica (§7a.2).
    fn foreign_ns_meta(id: &str, origin: &str) -> ClientEvent {
        let mut event = ns_meta(id, Some("alice"), None);
        if let ClientEvent::NsMeta { origin: o, .. } = &mut event {
            *o = Some(origin.into());
        }

        event
    }

    fn ns_meta(id: &str, owner: Option<&str>, description: Option<&str>) -> ClientEvent {
        ClientEvent::NsMeta {
            authority: None,
            settings_disabled: Vec::new(),
            id: id.into(),
            name: "cool-server".into(),
            visibility: "public".into(),
            owner: owner.map(Into::into),
            title: Some("Cool".into()),
            description: description.map(Into::into),
            recovery_set: false,
            recovery_eta: None,
            recovery_rung: None,
            categories: vec!["Text".into()],
            federation: true,
            origin: None,
            provider_online: None,
        }
    }

    #[test]
    fn ns_meta_maps_to_descriptor() {
        let mut ns = Namespaces;
        let d = ns.handle(&ns_meta("01ns", Some("alice"), Some("a cool place")));
        assert!(matches!(&d[0],
            NsDiff::NsDescriptor { id, name, owner, visibility, federation, .. }
            if id == "01ns" && name == "cool-server" && owner.as_deref() == Some("alice")
                && visibility == "public" && *federation));
    }

    #[test]
    fn a_foreign_namespaces_origin_reaches_the_descriptor() {
        // The origin is what the UI can actually show for a bridged namespace:
        // its local vanity is a collision-suffixed placeholder. Dropping it here
        // is why one displayed as `test-fp1n` instead of `teamnight.app/test`.
        let mut ns = Namespaces;
        let d = ns.handle(&foreign_ns_meta("01ns", "matrix://teamnight.app/test"));
        assert!(matches!(&d[0],
            NsDiff::NsDescriptor { origin, .. }
            if origin.as_deref() == Some("matrix://teamnight.app/test")));

        // A native namespace has none — the UI falls back to title/name.
        let d = ns.handle(&ns_meta("01ns", Some("alice"), None));
        assert!(matches!(&d[0], NsDiff::NsDescriptor { origin, .. } if origin.is_none()));
    }

    #[test]
    fn deletion_marker_emits_nothing() {
        let mut ns = Namespaces;
        // Owner cleared + description "deleted" is a tombstone → no descriptor.
        assert!(ns
            .handle(&ns_meta("01ns", None, Some("deleted")))
            .is_empty());
        // But owner-cleared with a *different* description is a normal update.
        assert!(!ns
            .handle(&ns_meta("01ns", None, Some("still here")))
            .is_empty());
    }
}
