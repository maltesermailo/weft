//! Storage row types. Event-sourced (§9.3): edits, deletes, and reactions
//! are rows referencing the original message's msgid — never mutations.

use weft_proto::{Account, ChannelName, MsgId, MsgMeta, Ulid, UserRef};

/// Where events live: a channel, or a same-network DM pair (§9.5).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scope {
    Channel(ChannelName),
    /// Participants in sorted order — `Scope::dm` normalizes, so
    /// (ada, bob) and (bob, ada) are the same conversation.
    Dm(Account, Account),
}

impl Scope {
    pub fn dm(a: Account, b: Account) -> Self {
        if a <= b {
            Scope::Dm(a, b)
        } else {
            Scope::Dm(b, a)
        }
    }

    /// Stable string key: the channel name, or `dm:<a>:<b>`. Used as the
    /// database key and safe because channel names always start with `#`.
    pub fn as_key(&self) -> String {
        match self {
            Scope::Channel(channel) => channel.to_string(),
            Scope::Dm(a, b) => format!("dm:{a}:{b}"),
        }
    }

    /// Inverse of [`Scope::as_key`], for backends rehydrating rows.
    pub fn from_key(key: &str) -> Option<Self> {
        if key.starts_with('#') {
            return key.parse().ok().map(Scope::Channel);
        }
        let (a, b) = key.strip_prefix("dm:")?.split_once(':')?;
        Some(Scope::dm(a.parse().ok()?, b.parse().ok()?))
    }
}

/// What happened. `Message` rows are roots; everything else is a child of
/// its `root` msgid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    Message { body: String, meta: MsgMeta },
    Edit { body: String },
    Delete,
    React { emoji: String, add: bool },
}

/// One stored event. Timestamps live inside the msgid's ULID (§9.6 —
/// server-stamped, single source of truth).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRecord {
    pub scope: Scope,
    /// This event's own id (every event gets one, §9.3).
    pub msgid: MsgId,
    /// The original message this event belongs to; equals `msgid` for
    /// `Message` rows.
    pub root: MsgId,
    pub sender: UserRef,
    pub kind: EventKind,
}

impl EventRecord {
    pub fn at_ms(&self) -> u64 {
        self.msgid.timestamp_ms()
    }

    pub fn is_root(&self) -> bool {
        matches!(self.kind, EventKind::Message { .. })
    }
}

/// A HISTORY window (§6.4): exclusive cursors, newest-anchored — the last
/// `limit` roots strictly between `after` and `before`.
#[derive(Debug, Clone, Copy)]
pub struct Page {
    pub before: Option<Ulid>,
    pub after: Option<Ulid>,
    pub limit: usize,
}

/// A verification claim on an account — the *infrastructure* for
/// email/age/phone verification. `kind` is an open namespace ("email",
/// "age", ...); `subject` is what is being verified (an address, a birth
/// year assertion, ...). A claim starts unverified; a verifier (SMTP flow,
/// ID provider, operator panel — all later work) confirms it. The wire
/// protocol for *proving* a claim is a spec decision (§18) and
/// deliberately not implemented here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verification {
    pub kind: String,
    pub subject: String,
    /// Unix seconds when confirmed; `None` = still pending.
    pub verified_at: Option<u64>,
}
