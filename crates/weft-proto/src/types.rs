//! Small wire enums shared by commands and events, plus the message
//! metadata tags (`fmt=`, `reply-to=`, `thread=`, `attach.N=`) common to
//! `MSG` (§6.4) and `MESSAGE` (§7).

use std::fmt;
use std::str::FromStr;

use crate::error::{ParseError, SerializeError};
use crate::id::MsgId;
use crate::line::{Tags, MAX_ATTACHMENTS};

macro_rules! wire_enum {
    ($(#[$doc:meta])* $name:ident, $what:literal, { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub fn as_str(&self) -> &'static str {
                match self {
                    $($name::$variant => $text),+
                }
            }
        }

        impl FromStr for $name {
            type Err = ParseError;

            fn from_str(s: &str) -> Result<Self, ParseError> {
                match s.to_ascii_lowercase().as_str() {
                    $($text => Ok($name::$variant),)+
                    _ => Err(ParseError::Invalid { what: $what, value: s.to_string() }),
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

wire_enum!(
    /// `PRESENCE` states (§6.1). `invisible` renders offline; presence is
    /// same-network only and never bridged.
    PresenceStatus, "presence status", {
        Online => "online",
        Away => "away",
        Dnd => "dnd",
        Invisible => "invisible",
    }
);

wire_enum!(
    /// `TYPING <#chan> <start|stop>` (§6.3).
    TypingState, "typing state", {
        Start => "start",
        Stop => "stop",
    }
);

wire_enum!(
    /// `MEMBER <#chan> <user@net> <join|part>` (§7).
    MemberAction, "member action", {
        Join => "join",
        Part => "part",
    }
);

wire_enum!(
    /// `REACTION ... op=add|remove` (§7) — live reaction increments.
    ReactionOp, "reaction op", {
        Add => "add",
        Remove => "remove",
    }
);

wire_enum!(
    /// Namespace visibility tier (§2.2). `Private` is indistinguishable
    /// from nonexistent to non-members (anti-enumeration).
    Visibility, "visibility", {
        Public => "public",
        Unlisted => "unlisted",
        Private => "private",
    }
);

wire_enum!(
    /// `REPORT ... <scope>` routing target (§6.7). `Ns` reaches namespace
    /// moderators; `Net` reaches the network operator. `csam`/`illegal`
    /// categories are always *also* routed to `Net` regardless of this.
    ReportScope, "report scope", {
        Ns => "ns",
        Net => "net",
    }
);

wire_enum!(
    /// `REPORTS LIST <scope> [status=...]` filter (§6.7).
    ReportStatus, "report status", {
        Open => "open",
        Resolved => "resolved",
    }
);

wire_enum!(
    /// `REPORTS RESOLVE <id> <action>` (§6.7). `Escalated` re-routes an
    /// ns-scope report up to net scope.
    ResolveAction, "resolve action", {
        Dismissed => "dismissed",
        ContentRemoved => "content-removed",
        UserActioned => "user-actioned",
        Escalated => "escalated",
    }
);

wire_enum!(
    /// Honest content state marked on a filed report (§6.7). `Verified` =
    /// the server still holds the event (a retention hold is placed);
    /// `Unverified` = expired/ephemeral, nothing confirms it;
    /// `ReporterAttested` = e2ee, only reporter-provided plaintext.
    ContentState, "content state", {
        Verified => "verified",
        Unverified => "unverified",
        ReporterAttested => "reporter-attested",
    }
);

wire_enum!(
    /// Manifest `history` bound (§11.1, §11.7). `FromEpoch` = serve nothing
    /// before the manifest's `created` ULID timestamp; `Full` = no lower
    /// bound (a §11.7 amendment that requires a version bump + re-ack).
    HistoryMode, "history mode", {
        FromEpoch => "from-epoch",
        Full => "full",
    }
);

wire_enum!(
    /// `MANIFEST <peer> <version> <state>` transition kind (spec extension,
    /// §11.5/§6.6 — the event's payload was left "as v0.8"; resolved here).
    /// Broadcast to affected members on every manifest change.
    BridgeState, "bridge state", {
        Live => "live",
        Added => "added",
        Removed => "removed",
        Severed => "severed",
    }
);

/// Manifest `media` bound (§11.1, §11.8): mirror all referenced blobs, mirror
/// up to a per-blob byte cap, or mirror nothing (referenced media renders
/// unavailable-by-policy). The `mirror-max:<bytes>` arm carries a parameter,
/// so this can't use the plain [`wire_enum!`] macro.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaMode {
    Mirror,
    MirrorMax(u64),
    None,
}

impl fmt::Display for MediaMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MediaMode::Mirror => f.write_str("mirror"),
            MediaMode::MirrorMax(bytes) => write!(f, "mirror-max:{bytes}"),
            MediaMode::None => f.write_str("none"),
        }
    }
}

impl FromStr for MediaMode {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        let lower = s.to_ascii_lowercase();
        match lower.as_str() {
            "mirror" => Ok(MediaMode::Mirror),
            "none" => Ok(MediaMode::None),
            other => match other.strip_prefix("mirror-max:") {
                Some(bytes) => {
                    bytes
                        .parse()
                        .map(MediaMode::MirrorMax)
                        .map_err(|_| ParseError::Invalid {
                            what: "media mode",
                            value: s.to_string(),
                        })
                }
                None => Err(ParseError::Invalid {
                    what: "media mode",
                    value: s.to_string(),
                }),
            },
        }
    }
}

/// §6.7 normative report categories; extensible with an `x-` prefix.
const REPORT_CATEGORIES: &[&str] = &[
    "spam",
    "harassment",
    "violence",
    "sexual",
    "csam",
    "illegal",
    "self-harm",
    "other",
];

/// A report category is valid iff it is in the normative set or carries the
/// `x-` extension prefix (§6.7). No spaces (it is a middle param).
pub fn report_category_ok(category: &str) -> bool {
    !category.contains(' ') && (REPORT_CATEGORIES.contains(&category) || category.starts_with("x-"))
}

/// Metadata tags shared by `MSG` and `MESSAGE`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MsgMeta {
    /// `fmt=md` — body format hint (§9.4).
    pub fmt: Option<String>,
    /// `reply-to=<msgid>` (§9.3).
    pub reply_to: Option<MsgId>,
    /// `thread=<msgid>` — threads are views, not channels (§9.3).
    pub thread: Option<MsgId>,
    /// `attach.1=`..`attach.10=` media references, in index order (§6.4).
    pub attachments: Vec<String>,
}

impl MsgMeta {
    pub(crate) fn from_tags(tags: &Tags) -> Result<Self, ParseError> {
        let msgid_tag = |key: &str| tags.get(key).map(|v| v.parse::<MsgId>()).transpose();
        let mut attachments: Vec<(usize, String)> = Vec::new();
        for (key, value) in tags {
            // Non-numeric `attach.*` suffixes are treated as unknown tags.
            if let Some(n) = key
                .strip_prefix("attach.")
                .and_then(|s| s.parse::<usize>().ok())
            {
                if n == 0 || n > MAX_ATTACHMENTS {
                    return Err(ParseError::TooManyAttachments);
                }
                if !value.is_empty() {
                    attachments.push((n, value.clone()));
                }
            }
        }
        attachments.sort_by_key(|(n, _)| *n); // BTreeMap yields "attach.10" before "attach.2"
        Ok(MsgMeta {
            fmt: tags.get("fmt").filter(|v| !v.is_empty()).cloned(),
            reply_to: msgid_tag("reply-to")?,
            thread: msgid_tag("thread")?,
            attachments: attachments.into_iter().map(|(_, v)| v).collect(),
        })
    }

    pub(crate) fn write_tags(&self, tags: &mut Tags) -> Result<(), SerializeError> {
        if self.attachments.len() > MAX_ATTACHMENTS {
            return Err(SerializeError::TooManyAttachments);
        }
        if let Some(fmt) = &self.fmt {
            if fmt.is_empty() {
                return Err(SerializeError::Unrepresentable("empty fmt"));
            }
            tags.insert("fmt".to_string(), fmt.clone());
        }
        if let Some(reply_to) = &self.reply_to {
            tags.insert("reply-to".to_string(), reply_to.to_string());
        }
        if let Some(thread) = &self.thread {
            tags.insert("thread".to_string(), thread.to_string());
        }
        for (i, attachment) in self.attachments.iter().enumerate() {
            if attachment.is_empty() {
                return Err(SerializeError::Unrepresentable("empty attachment"));
            }
            tags.insert(format!("attach.{}", i + 1), attachment.clone());
        }
        Ok(())
    }
}
