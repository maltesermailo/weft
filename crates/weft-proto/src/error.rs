//! Typed codec errors. Every parse failure is a distinct, testable variant;
//! the session layer (M1) decides which `ERR` code each one maps to.

use thiserror::Error;

use crate::line;

/// Failures of the lenient inbound parser (spec §4).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseError {
    #[error("line exceeds {max} bytes (got {len})", max = line::MAX_LINE_BYTES)]
    LineTooLong { len: usize },

    #[error("empty line (no verb)")]
    EmptyLine,

    #[error("CR/LF inside line body")]
    EmbeddedLineBreak,

    #[error("invalid tag key {key:?}")]
    BadTagKey { key: String },

    #[error("tag {key:?} value exceeds {max} bytes", max = line::MAX_TAG_VALUE_BYTES)]
    TagValueTooLong { key: String },

    /// §4: a dangling backslash at the end of a tag value is an error
    /// (unknown escapes merely drop the backslash).
    #[error("dangling escape at end of tag value")]
    DanglingEscape,

    #[error("more than {max} tags", max = line::MAX_TAGS)]
    TooManyTags,

    #[error("invalid verb {verb:?}")]
    BadVerb { verb: String },

    #[error("more than {max} middle params", max = line::MAX_PARAMS)]
    TooManyParams,

    #[error("{verb}: missing {what}")]
    MissingParam {
        verb: &'static str,
        what: &'static str,
    },

    #[error("{verb}: bad {what}: {value:?}")]
    BadParam {
        verb: &'static str,
        what: &'static str,
        value: String,
    },

    /// Malformed identifier or scalar (account, channel, msgid, policy, ...).
    #[error("invalid {what}: {value:?}")]
    Invalid { what: &'static str, value: String },

    #[error("label exceeds {max} bytes", max = line::MAX_LABEL_BYTES)]
    LabelTooLong,

    #[error("more than {max} attachments", max = line::MAX_ATTACHMENTS)]
    TooManyAttachments,
}

/// Failures of the strict outbound serializer (spec §4: serializers MUST
/// refuse to emit anything their own parser rejects).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SerializeError {
    #[error("serialized line would exceed {max} bytes", max = line::MAX_LINE_BYTES)]
    LineTooLong,

    #[error("invalid verb {verb:?}")]
    BadVerb { verb: String },

    #[error("invalid tag key {key:?}")]
    BadTagKey { key: String },

    #[error("tag {key:?} value exceeds {max} bytes", max = line::MAX_TAG_VALUE_BYTES)]
    TagValueTooLong { key: String },

    #[error("more than {max} tags", max = line::MAX_TAGS)]
    TooManyTags,

    #[error("more than {max} middle params", max = line::MAX_PARAMS)]
    TooManyParams,

    #[error("invalid middle param {param:?}: {reason}")]
    BadParam { param: String, reason: &'static str },

    #[error("CR/LF in trailing")]
    BadTrailing,

    #[error("label exceeds {max} bytes", max = line::MAX_LABEL_BYTES)]
    LabelTooLong,

    #[error("more than {max} attachments", max = line::MAX_ATTACHMENTS)]
    TooManyAttachments,

    /// Value has no legal wire form (e.g. `Command::Unknown`).
    #[error("cannot serialize {0}")]
    Unrepresentable(&'static str),
}
