//! Plugin SDUI codec (M-plug-1, `docs/architecture/plugin-spec.md` §10–§11): the
//! typed component catalog, views, patches, terminal results, and action
//! declarations. These are structured trees that the line grammar (§4) can't
//! express, so they ride as **base64-CBOR in a tag** (`@view=<b64>` etc.) — the
//! same pattern signed manifests and capability tokens already use.
//!
//! Pure L0: serde + ciborium are no-I/O. The client renders **only** known
//! component `type`s; an unknown `type`/patch-op decodes to a skip-variant
//! (forward-compatible), never executed (spec §10, invariant 1). Enums use
//! internally-tagged, kebab-case CBOR so the wire matches the spec's `{ "type":
//! "…" }` shape.

use base64::prelude::{Engine as _, BASE64_STANDARD};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::{ParseError, SerializeError};

/// Encode any SDUI payload to the wire form: deterministic CBOR, then base64.
pub fn to_b64<T: Serialize>(value: &T) -> Result<String, SerializeError> {
    let mut bytes = Vec::new();
    ciborium::into_writer(value, &mut bytes).map_err(|_| SerializeError::Unrepresentable("cbor"))?;
    Ok(BASE64_STANDARD.encode(bytes))
}

/// Decode an SDUI payload from a `@key=<b64cbor>` tag value.
pub fn from_b64<T: DeserializeOwned>(s: &str) -> Result<T, ParseError> {
    let bytes = BASE64_STANDARD.decode(s).map_err(|_| invalid("plugin payload", s))?;
    ciborium::from_reader(&bytes[..]).map_err(|_| invalid("plugin payload", s))
}

fn invalid(what: &'static str, value: &str) -> ParseError {
    ParseError::Invalid {
        what,
        value: value.to_string(),
    }
}

// ---- small enums ----

/// Where a declared action appears (spec §13.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Surface {
    ContextMenu,
    Slash,
    Settings,
    Global,
    ServerMenu,
    ChannelList,
}

/// The object an action targets (spec §13.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContextType {
    Message,
    Channel,
    Member,
    User,
    Namespace,
    None,
}

/// A hook's kind (spec §8). Remote plugins register only `Observe` (§8.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HookKind {
    Observe,
    Veto,
}

/// A veto hook's deadline-overrun policy (spec §8.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailPolicy {
    Open,
    Closed,
}

/// A view's container (spec §11). `Custom` is a widget (§3.3): a client-bundle
/// asset ref, not a blocks tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Container {
    Modal,
    Panel,
    Custom,
}

/// A button's visual weight (spec §10.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ButtonStyle {
    Primary,
    Default,
    Danger,
}

/// A toast's severity (spec §11.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToastKind {
    Ok,
    Error,
    Info,
}

// ---- component substructures ----

/// One choice in a `select`/`multiselect`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectOption {
    pub value: String,
    pub label: String,
}

/// One row of a `keyvalue` display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KvRow {
    pub key: String,
    pub value: String,
}

/// A clickable control (spec §10.3), used bare and inside an `action-row`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Button {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<ButtonStyle>,
    /// Confirm-before-fire prompt text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<String>,
}

// ---- the component catalog (spec §10) ----

/// A single SDUI component. Internally tagged (`{ "type": "text", … }`); an
/// unknown `type` on the wire decodes to [`Component::Unknown`] and is skipped by
/// the renderer (forward-compatible, never executed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Component {
    // inputs (§10.1)
    Text {
        id: String,
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        required: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        multiline: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_len: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
    },
    Number {
        id: String,
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        required: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step: Option<f64>,
    },
    Select {
        id: String,
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        required: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
        options: Vec<SelectOption>,
    },
    Multiselect {
        id: String,
        label: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        default: Vec<String>,
        options: Vec<SelectOption>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<u32>,
    },
    Toggle {
        id: String,
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<bool>,
    },
    Date {
        id: String,
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        required: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<String>,
    },
    // display (§10.2)
    Heading {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        level: Option<u8>,
    },
    Markdown {
        text: String,
    },
    Divider,
    Keyvalue {
        rows: Vec<KvRow>,
    },
    Table {
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dense: Option<bool>,
    },
    Image {
        src: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alt: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_height: Option<u32>,
    },
    // controls (§10.3)
    Button(Button),
    ActionRow {
        buttons: Vec<Button>,
    },
    Submit {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        style: Option<ButtonStyle>,
    },
    /// An unknown component type from a newer server — decoded here so the tree
    /// still parses; the client skips it (spec §10, forward-compatible).
    #[serde(other)]
    Unknown,
}

// ---- view, patch, result (spec §11) ----

/// An SDUI screen (spec §11): a modal/panel of `blocks`, or a `custom` widget
/// (a client-bundle asset ref + params, §3.3/§11.6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct View {
    pub container: Container,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// A panel's stable push handle (spec §11.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submit_label: Option<String>,
    /// modal/panel content.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<Component>,
    /// `custom` only: the client-bundle asset ref to mount (§11.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub widget: Option<String>,
    /// `custom` only: opaque string params handed to the widget.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<KvRow>,
}

/// A live update to an open panel/widget (spec §11.4). Unknown ops decode to
/// [`PatchOp::Unknown`] and are ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum PatchOp {
    Replace { view: Box<View> },
    Set { component_id: String, props: View },
    Append { container_id: String, blocks: Vec<Component> },
    Remove { component_id: String },
    #[serde(other)]
    Unknown,
}

/// A flow's terminal outcome (spec §11.5). Any real side effect reaches the
/// client through the normal event stream, not here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "kebab-case")]
pub enum ViewResult {
    Toast { kind: ToastKind, text: String },
    Navigate { target: String },
    Close {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Refresh {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
    },
}

// ---- action declaration + catalog (spec §12.5, §13) ----

/// A declared action's client-facing metadata (spec §13). The handlers stay
/// server-side; this is what enters the catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionDecl {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    pub surface: Surface,
    pub context: ContextType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Client-side show/hide predicate (spec §13.3); display-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    /// Inputs collected into a form before invoke (spec §13.4).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input: Vec<Component>,
}

/// One plugin's catalog entry (spec §12.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub plugin_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    pub actions: Vec<ActionDecl>,
}

/// The whole `PLUGIN-MANIFEST` payload: the declared actions of every plugin.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Catalog {
    pub plugins: Vec<CatalogEntry>,
}

/// A hook subscription in a provider's registration (spec §4.2, §8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookDecl {
    pub event: String,
    pub kind: HookKind,
    /// veto only; default open (spec §8.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail: Option<FailPolicy>,
}

/// A remote provider's self-description, sent at connect over `PLUGIN-REGISTER`
/// (spec §4.2): its plugin-API version, identity, declared actions, and hook
/// subscriptions. weftd validates it exactly as it validates an in-process
/// `register()` pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Registration {
    pub api: u16,
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ActionDecl>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<HookDecl>,
    /// Foreign-URI schemes this provider handles (spec §18 capability 6): a
    /// `NS JOIN <scheme>://…` for an unknown space routes a `PROVISION` push
    /// here. Must be authorized by the provider's pinned config entry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schemes: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T>(value: &T)
    where
        T: Serialize + DeserializeOwned + std::fmt::Debug + PartialEq,
    {
        let b64 = to_b64(value).expect("encode");
        let back: T = from_b64(&b64).expect("decode");
        assert_eq!(&back, value);
    }

    #[test]
    fn components_round_trip() {
        round_trip(&Component::Text {
            id: "name".into(),
            label: "Name".into(),
            required: Some(true),
            default: None,
            placeholder: Some("…".into()),
            multiline: None,
            max_len: Some(64),
            pattern: None,
        });
        round_trip(&Component::Select {
            id: "lang".into(),
            label: "Language".into(),
            required: Some(true),
            default: Some("en".into()),
            options: vec![SelectOption { value: "en".into(), label: "English".into() }],
        });
        round_trip(&Component::Divider);
        round_trip(&Component::Table {
            columns: vec!["Role".into(), "Members".into()],
            rows: vec![vec!["admin".into(), "3".into()]],
            dense: Some(true),
        });
        round_trip(&Component::Button(Button {
            id: "save".into(),
            label: "Save".into(),
            style: Some(ButtonStyle::Primary),
            confirm: None,
        }));
        round_trip(&Component::ActionRow {
            buttons: vec![Button { id: "ok".into(), label: "OK".into(), style: None, confirm: None }],
        });
    }

    #[test]
    fn view_modal_and_widget_round_trip() {
        round_trip(&View {
            container: Container::Modal,
            title: Some("Translate".into()),
            panel_key: None,
            submit_label: None,
            blocks: vec![Component::Heading { text: "Result".into(), level: None }, Component::Markdown { text: "hi".into() }],
            widget: None,
            params: vec![],
        });
        round_trip(&View {
            container: Container::Custom,
            title: Some("Roles".into()),
            panel_key: None,
            submit_label: None,
            blocks: vec![],
            widget: Some("role-editor".into()),
            params: vec![KvRow { key: "ns".into(), value: "ns:01h".into() }],
        });
    }

    #[test]
    fn patch_result_catalog_round_trip() {
        round_trip(&PatchOp::Set {
            component_id: "bar".into(),
            props: View {
                container: Container::Panel,
                title: None,
                panel_key: Some("modq".into()),
                submit_label: None,
                blocks: vec![Component::Divider],
                widget: None,
                params: vec![],
            },
        });
        round_trip(&ViewResult::Toast { kind: ToastKind::Error, text: "nope".into() });
        round_trip(&ViewResult::Close { reason: Some("reloaded".into()) });
        // A PLUGIN-PATCH carries a Vec of ops.
        round_trip(&vec![
            PatchOp::Remove { component_id: "x".into() },
            PatchOp::Append { container_id: "list".into(), blocks: vec![Component::Divider] },
        ]);
        round_trip(&Catalog {
            plugins: vec![CatalogEntry {
                plugin_id: "translate".into(),
                name: "Translate".into(),
                icon: None,
                actions: vec![ActionDecl {
                    id: "translate".into(),
                    label: "Translate".into(),
                    icon: Some("🌐".into()),
                    surface: Surface::ContextMenu,
                    context: ContextType::Message,
                    description: None,
                    visibility: None,
                    input: vec![Component::Select {
                        id: "lang".into(),
                        label: "Language".into(),
                        required: Some(true),
                        default: None,
                        options: vec![SelectOption { value: "en".into(), label: "English".into() }],
                    }],
                }],
            }],
        });
    }

    #[test]
    fn registration_round_trips() {
        round_trip(&Registration {
            api: 1,
            id: "automod".into(),
            name: "Automod".into(),
            icon: None,
            actions: vec![],
            hooks: vec![HookDecl {
                event: "message.posted".into(),
                kind: HookKind::Veto,
                fail: Some(FailPolicy::Open),
            }],
            schemes: vec!["instagram".into()],
        });
    }

    #[test]
    fn unknown_component_decodes_to_skip() {
        // A newer server sends a component type we don't know: it must still
        // decode (as Unknown), never error — forward-compatibility (spec §10).
        #[derive(Serialize)]
        #[serde(tag = "type", rename_all = "kebab-case")]
        enum Future {
            ColorPicker { id: String },
        }
        let b64 = to_b64(&Future::ColorPicker { id: "c".into() }).unwrap();
        let decoded: Component = from_b64(&b64).unwrap();
        assert_eq!(decoded, Component::Unknown);
    }

    #[test]
    fn garbage_b64_is_a_parse_error() {
        assert!(from_b64::<View>("not valid base64!!!").is_err());
        assert!(from_b64::<View>(&BASE64_STANDARD.encode([0xff, 0x00, 0x99])).is_err());
    }
}
