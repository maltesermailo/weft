//! # weft-proto — WEFT wire codec (L0)
//!
//! Pure codec for the WEFT control plane, spec v0.10 (`docs/weft-protocol-spec.md`).
//! No I/O, no tokio, no async: this crate is the security-critical parser and
//! must stay fuzzable in isolation (CLAUDE.md layering rules).
//!
//! Layers:
//! - [`Line`]: the raw `@tags VERB params :trailing` grammar (§4),
//!   lenient-in / strict-out.
//! - [`Request`] / [`Reply`]: typed commands (client → server, §6) and
//!   events (server → client, §7), each pairing a payload with its
//!   optional `label` (§3.5).
//! - Scalars: identifiers (§2.3), [`MsgId`] (§5.1), [`RetentionPolicy`]
//!   (§5.2), [`ErrCode`] (§8).
//!
//! Unknown verbs decode to `Command::Unknown` / `Event::Unknown` — never a
//! parse error (§4); neither has a wire form on the way out.

#![forbid(unsafe_code)]

mod command;
mod errcode;
mod error;
mod event;
mod id;
mod line;
mod name;
mod policy;
mod types;

pub use command::{Command, Request};
pub use errcode::ErrCode;
pub use error::{ParseError, SerializeError};
pub use event::{ErrEvent, Event, MessageEvent, Reply};
pub use id::MsgId;
pub use line::{
    escape_tag_value, unescape_tag_value, Line, Tags, MAX_ATTACHMENTS, MAX_EMOJI_BYTES,
    MAX_HISTORY_LIMIT, MAX_LABEL_BYTES, MAX_LINE_BYTES, MAX_PARAMS, MAX_TAGS, MAX_TAG_KEY_BYTES,
    MAX_TAG_VALUE_BYTES,
};
pub use name::{Account, ChannelName, NamespaceName, NetworkName, Target, UserRef};
pub use policy::{RetainedFor, RetentionPolicy, RetentionUnit};
pub use types::{MemberAction, MsgMeta, PresenceStatus, ReactionOp, TypingState, Visibility};

// Re-exported so consumers (weft-core mints ULIDs in channel actors) share
// one ulid version without a direct dependency.
pub use ulid::Ulid;
