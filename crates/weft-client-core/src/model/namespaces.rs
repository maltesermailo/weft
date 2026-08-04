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
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
