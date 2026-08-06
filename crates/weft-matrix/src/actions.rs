//! The management flows (plan slice 11): what an operator or moderator can
//! *do* to a bridged space, as SDUI views rather than wire verbs.
//!
//! Two rules shape all of them:
//!
//! - **The invoker acts, not the service.** Every wire command a flow issues is
//!   attributed (`@as` the invoking WEFT user), so weftd checks *their*
//!   capabilities. A flow that acted as the bridge would be a privilege
//!   escalation with a form in front of it.
//! - **Levels are set here, never on the wire.** weftd has no notion of a
//!   power level (protocol doc §7): the client sends a *number* as a param of
//!   this action, the adapter maps it (`levels.rs`), and the resulting
//!   capabilities go out as an attributed `GRANT`. Translation belongs where
//!   the pinned key is.
//!
//! Declarations live here so the SDK's builder stays a builder; the handlers
//! run against a shared [`Bridge`] because they read its structure maps.

use std::collections::BTreeMap;

use weft_proto::{
    ActionDecl, Component, Container, ContextType, SelectOption, Surface, ToastKind, View,
};

/// The flows this adapter offers, in catalog order.
pub fn declarations() -> Vec<ActionDecl> {
    vec![
        decl(
            "power-levels",
            "Power Levels",
            Surface::Settings,
            ContextType::Namespace,
            Some("Matrix power levels for this space — the roles editor's stand-in (authority=levels)."),
        ),
        decl(
            "invite",
            "Invite to Matrix room",
            Surface::ChannelList,
            ContextType::Channel,
            Some("Invite a Matrix user to this channel's bridged room."),
        ),
        decl(
            "moderate",
            "Moderate on Matrix",
            Surface::ContextMenu,
            ContextType::Member,
            Some("Kick or ban this member in the bridged rooms."),
        ),
        decl(
            "create-room",
            "New bridged room",
            Surface::ChannelList,
            ContextType::Namespace,
            Some("Create a channel that mirrors as a Matrix room."),
        ),
        decl(
            "room-settings",
            "Bridged room",
            Surface::ChannelSettings,
            ContextType::Channel,
            Some("Name and topic of the bridged Matrix room."),
        ),
        decl(
            "bans",
            "Bridged space bans",
            Surface::Admin,
            ContextType::None,
            Some("Spaces this bridge refuses to mirror."),
        ),
    ]
}

fn decl(
    id: &str,
    label: &str,
    surface: Surface,
    context: ContextType,
    description: Option<&str>,
) -> ActionDecl {
    ActionDecl {
        id: id.to_string(),
        label: label.to_string(),
        icon: None,
        surface,
        context,
        description: description.map(str::to_string),
        visibility: None,
        input: Vec::new(),
    }
}

/// The tier picker every level flow uses — three named tiers, because the
/// mapping is three tiers (`levels.rs`); a free-number field would imply a
/// precision the translation does not have.
pub fn tier_options() -> Vec<SelectOption> {
    vec![
        SelectOption {
            value: crate::levels::ADMIN_LEVEL.to_string(),
            label: format!("Admin ({})", crate::levels::ADMIN_LEVEL),
        },
        SelectOption {
            value: crate::levels::MOD_LEVEL.to_string(),
            label: format!("Moderator ({})", crate::levels::MOD_LEVEL),
        },
        SelectOption {
            value: "0".to_string(),
            label: "Member (0)".to_string(),
        },
    ]
}

/// The Power Levels view: the current map as a table, plus a set-one form.
/// This is the surface `authority=levels` promises when it hides the native
/// roles editor.
pub fn power_levels_view(space_room: &str, users: &BTreeMap<String, i64>) -> View {
    let mut rows: Vec<Vec<String>> = users
        .iter()
        .map(|(mxid, level)| {
            let tier = match crate::levels::caps_for_level(*level) {
                Some(crate::levels::ADMIN_CAPS) => "admin",
                Some(_) => "moderator",
                None => "member",
            };
            vec![mxid.clone(), level.to_string(), tier.to_string()]
        })
        .collect();
    rows.sort();

    let mut blocks = vec![Component::Heading {
        text: "Power levels".into(),
        level: Some(2),
    }];

    if rows.is_empty() {
        blocks.push(Component::Markdown {
            text: "No elevated users — everyone is a member.".into(),
        });
    } else {
        blocks.push(Component::Table {
            columns: vec!["Matrix user".into(), "Level".into(), "Tier".into()],
            rows,
            dense: Some(true),
        });
    }

    blocks.extend([
        Component::Divider,
        Component::Text {
            id: "mxid".into(),
            label: "Matrix user".into(),
            required: Some(true),
            default: None,
            placeholder: Some("@alice:matrix.org".into()),
            multiline: None,
            max_len: Some(255),
            pattern: None,
        },
        Component::Select {
            id: "level".into(),
            label: "Tier".into(),
            required: Some(true),
            default: Some(crate::levels::MOD_LEVEL.to_string()),
            options: tier_options(),
        },
        Component::Markdown {
            text: format!(
                "Applies in every room of `{space_room}`. The tier maps to WEFT \
                 capabilities, and takes effect only if your own grants allow it."
            ),
        },
    ]);

    View {
        container: Container::Modal,
        title: Some("Power Levels".into()),
        panel_key: None,
        submit_label: Some("Set level".into()),
        blocks,
        widget: None,
        params: Vec::new(),
    }
}

/// The create-room view. Two shapes, because the two namespace kinds are
/// genuinely different objects: in a **projected** namespace the WEFT channel
/// is the real thing (and only a `permanent` one mirrors, §3), while in a
/// **consumed** space the Matrix room is, and WEFT gets a replica of it.
pub fn create_room_view(projected: bool) -> View {
    let mut blocks = vec![Component::Text {
        id: "name".into(),
        label: "Room name".into(),
        required: Some(true),
        default: None,
        placeholder: Some("announcements".into()),
        multiline: None,
        max_len: Some(64),
        pattern: None,
    }];

    blocks.push(Component::Markdown {
        text: if projected {
            "Creates a WEFT channel with **permanent** retention — the policy              Matrix projection requires — and mirrors it as a room."
                .into()
        } else {
            "Creates the room on Matrix; WEFT receives it as a channel of this              bridged space."
                .into()
        },
    });

    View {
        container: Container::Modal,
        title: Some("New bridged room".into()),
        panel_key: None,
        submit_label: Some("Create".into()),
        blocks,
        widget: None,
        params: Vec::new(),
    }
}

pub fn invite_view(room: &str) -> View {
    View {
        container: Container::Modal,
        title: Some("Invite to the bridged room".into()),
        panel_key: None,
        submit_label: Some("Invite".into()),
        blocks: vec![
            Component::Text {
                id: "mxid".into(),
                label: "Matrix user".into(),
                required: Some(true),
                default: None,
                placeholder: Some("@alice:matrix.org".into()),
                multiline: None,
                max_len: Some(255),
                pattern: None,
            },
            Component::Markdown {
                text: format!("They will be invited to `{room}`."),
            },
        ],
        widget: None,
        params: Vec::new(),
    }
}

pub fn moderate_view(member: &str) -> View {
    View {
        container: Container::Modal,
        title: Some(format!("Moderate {member}")),
        panel_key: None,
        submit_label: None,
        blocks: vec![
            Component::Text {
                id: "reason".into(),
                label: "Reason".into(),
                required: None,
                default: None,
                placeholder: Some("optional".into()),
                multiline: None,
                max_len: Some(200),
                pattern: None,
            },
            Component::Markdown {
                text: "Checked against **your** WEFT capabilities — a refusal is \
                       reverted on the Matrix side."
                    .into(),
            },
            Component::ActionRow {
                buttons: vec![
                    weft_proto::Button {
                        id: "kick".into(),
                        label: "Kick".into(),
                        style: None,
                        confirm: None,
                    },
                    weft_proto::Button {
                        id: "ban".into(),
                        label: "Ban".into(),
                        style: Some(weft_proto::ButtonStyle::Danger),
                        confirm: Some(format!("Ban {member} from the bridged rooms?")),
                    },
                ],
            },
        ],
        widget: None,
        params: Vec::new(),
    }
}

pub fn room_settings_view(room: &str, name: &str, topic: &str) -> View {
    View {
        container: Container::Modal,
        title: Some("Bridged room settings".into()),
        panel_key: None,
        submit_label: Some("Save".into()),
        blocks: vec![
            Component::Text {
                id: "name".into(),
                label: "Room name".into(),
                required: None,
                default: Some(name.to_string()),
                placeholder: None,
                multiline: None,
                max_len: Some(120),
                pattern: None,
            },
            Component::Text {
                id: "topic".into(),
                label: "Topic".into(),
                required: None,
                default: Some(topic.to_string()),
                placeholder: None,
                multiline: Some(true),
                max_len: Some(500),
                pattern: None,
            },
            Component::Markdown {
                text: format!("Applies to `{room}` on the companion homeserver."),
            },
        ],
        widget: None,
        params: Vec::new(),
    }
}

/// The admin-panel ban list (weftd tells us once; we store and enforce — §11).
pub fn bans_view(banned: &[String]) -> View {
    let blocks = if banned.is_empty() {
        vec![Component::Markdown {
            text: "No spaces are banned from bridging.".into(),
        }]
    } else {
        vec![Component::Table {
            columns: vec!["Namespace".into()],
            rows: banned.iter().map(|ns| vec![ns.clone()]).collect(),
            dense: Some(true),
        }]
    };

    View {
        container: Container::Panel,
        title: Some("Bridged space bans".into()),
        panel_key: Some("matrix:bans".into()),
        submit_label: None,
        blocks,
        widget: None,
        params: Vec::new(),
    }
}

/// A step's string value, or `""` — the SDUI values map is untyped by design.
pub fn value<'a>(values: &'a BTreeMap<String, serde_json::Value>, id: &str) -> &'a str {
    values.get(id).and_then(|v| v.as_str()).unwrap_or_default()
}

/// A refusal toast, so a flow that cannot proceed says why.
pub fn refusal(text: &str) -> (ToastKind, String) {
    (ToastKind::Error, text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_flow_is_declared_on_a_surface_that_can_show_it() {
        let decls = declarations();
        assert!(decls.iter().any(|d| d.id == "power-levels"
            && d.surface == Surface::Settings
            && d.context == ContextType::Namespace));
        // The admin ban list belongs in the panel, not a client (§22).
        assert!(decls
            .iter()
            .any(|d| d.id == "bans" && d.surface == Surface::Admin));
        // Per-channel configuration is a channel-settings **page**, not a
        // button in the channel list (owner directive 2026-08-06).
        assert!(decls.iter().any(|d| d.id == "room-settings"
            && d.surface == Surface::ChannelSettings
            && d.context == ContextType::Channel));
        assert!(
            decls.iter().all(|d| d.description.is_some()),
            "a management action must explain itself"
        );
    }

    #[test]
    fn the_create_room_view_states_which_side_owns_the_room() {
        // The two namespace kinds create genuinely different objects, and the
        // view says which — a silent difference here would surprise the user
        // when the retention policy or the source of truth differs.
        let projected = create_room_view(true);
        let consumed = create_room_view(false);

        let text = |v: &View| {
            v.blocks
                .iter()
                .filter_map(|b| match b {
                    Component::Markdown { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<String>()
        };
        assert!(text(&projected).contains("permanent"));
        assert!(text(&consumed).contains("on Matrix"));
        // Both ask for exactly one thing: the name.
        for v in [&projected, &consumed] {
            assert_eq!(
                v.blocks
                    .iter()
                    .filter(|b| matches!(b, Component::Text { .. }))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn the_levels_view_shows_the_map_and_offers_only_mapped_tiers() {
        let users: BTreeMap<String, i64> = [("@a:x".into(), 90), ("@b:x".into(), 50)]
            .into_iter()
            .collect();
        let view = power_levels_view("!space:x", &users);

        let Some(Component::Table { rows, .. }) = view
            .blocks
            .iter()
            .find(|b| matches!(b, Component::Table { .. }))
        else {
            panic!("the current map is shown");
        };
        assert_eq!(rows[0], ["@a:x", "90", "admin"]);
        assert_eq!(rows[1], ["@b:x", "50", "moderator"]);

        // Only the three mapped tiers: a free number would imply a precision
        // the capability mapping does not have.
        let Some(Component::Select { options, .. }) = view
            .blocks
            .iter()
            .find(|b| matches!(b, Component::Select { .. }))
        else {
            panic!("a tier picker is offered");
        };
        assert_eq!(options.len(), 3);
    }

    #[test]
    fn an_empty_map_still_renders() {
        let view = power_levels_view("!space:x", &BTreeMap::new());
        assert!(view
            .blocks
            .iter()
            .any(|b| matches!(b, Component::Markdown { .. })));
        assert!(
            !view
                .blocks
                .iter()
                .any(|b| matches!(b, Component::Table { .. })),
            "no table when there is nothing in it"
        );
    }
}
