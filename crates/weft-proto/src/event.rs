//! Server → client events (§7). Unknown events decode
//! to [`Event::Unknown`] and MUST be ignored by clients (§7).

use crate::errcode::ErrCode;
use crate::error::{ParseError, SerializeError};
use crate::id::MsgId;
use crate::line::{label_from_tags, write_label, Args, Line, Tags};
use crate::name::{Account, ChannelName, NamespaceName, NetworkName, Target, UserRef};
use crate::policy::RetentionPolicy;
use crate::types::{
    BridgeState, ContentState, HistoryMode, MediaMode, MemberAction, ModAction, MsgMeta,
    PresenceStatus, ReactionOp, ReportScope, ResolveAction, TypingState, Visibility,
};

/// An event plus its optional `label` echo (§3.5). Only direct responses
/// carry a label; broadcast copies never do — that distinction is the
/// session layer's job, the codec just (de)serializes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub label: Option<String>,
    pub event: Event,
}

impl Reply {
    pub fn new(event: Event) -> Self {
        Self { label: None, event }
    }

    pub fn with_label(event: Event, label: impl Into<String>) -> Self {
        Self {
            label: Some(label.into()),
            event,
        }
    }

    pub fn parse(input: &str) -> Result<Self, ParseError> {
        Self::from_line(&Line::parse(input)?)
    }

    pub fn from_line(line: &Line) -> Result<Self, ParseError> {
        Ok(Reply {
            label: label_from_tags(&line.tags)?,
            event: Event::from_line(line)?,
        })
    }

    pub fn to_line(&self) -> Result<Line, SerializeError> {
        let mut line = self.event.to_line()?;
        write_label(&mut line.tags, self.label.as_deref())?;
        Ok(line)
    }

    pub fn serialize(&self) -> Result<String, SerializeError> {
        self.to_line()?.serialize()
    }
}

/// `MESSAGE <#chan|@user> <user@net> :body` — the echo copy (with the
/// sender's label) is the delivery ack (§3.5, §9.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageEvent {
    pub target: Target,
    pub sender: UserRef,
    /// Assigned by the origin channel actor — always present on events.
    pub msgid: MsgId,
    pub body: String,
    pub meta: MsgMeta,
    /// Batch form only (§12.1): number of edits collapsed into `body`.
    pub edited: Option<u64>,
    /// Batch form only: unix ms of the final edit (`edited-at=`).
    pub edited_at: Option<u64>,
}

/// `ERR <CODE> [context] :text` (§8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrEvent {
    pub code: ErrCode,
    /// E.g. the missing capability for `CAP-REQUIRED`.
    pub context: Option<String>,
    pub text: String,
    /// `retry-after=` seconds (THROTTLED).
    pub retry_after: Option<u64>,
    /// `max=` limit (QUOTA / TOO-LARGE).
    pub max: Option<u64>,
}

impl ErrEvent {
    /// Plain error with no context or limit tags.
    pub fn new(code: ErrCode, text: impl Into<String>) -> Self {
        Self {
            code,
            context: None,
            text: text.into(),
            retry_after: None,
            max: None,
        }
    }
}

/// The event set through M3 (§7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// `WELCOME <network> [:motd]` with `features=`/`attestation=` tags (§3.6).
    Welcome {
        network: NetworkName,
        features: Vec<String>,
        attestation: Option<String>,
        motd: Option<String>,
    },
    /// `CHALLENGE <b64-nonce>` (§6.1).
    Challenge {
        nonce: String,
    },
    /// Boxed: much larger than every other variant.
    Message(Box<MessageEvent>),
    /// `MEMBER <#chan> <user@net> <join|part>` with optional `display=` and
    /// `count=` (member count after the change; §6.3: JOIN responds with
    /// `MEMBER` + `POLICY` + `count=`).
    Member {
        channel: ChannelName,
        user: UserRef,
        action: MemberAction,
        display: Option<String>,
        count: Option<u64>,
    },
    /// `TYPING <#chan> <user@net> <start|stop>` — never stored.
    Typing {
        channel: ChannelName,
        user: UserRef,
        state: TypingState,
    },
    /// `MARKED <#chan> <msgid>` — read-marker sync across own devices.
    Marked {
        channel: ChannelName,
        msgid: MsgId,
    },
    /// `PRESENCE <user@net> <status>` — never bridged.
    Presence {
        user: UserRef,
        status: PresenceStatus,
    },
    /// `POLICY <#chan> <policy>` — sent on join and on policy change (§5.2).
    Policy {
        channel: ChannelName,
        policy: RetentionPolicy,
    },
    /// `PONG [token]`.
    Pong {
        token: Option<String>,
    },
    /// `EDITED <target> <user@net> :new` — live only, never in batches
    /// (§7, §12.1). Carries its own `msgid=` plus `edit-of=`.
    Edited {
        target: Target,
        user: UserRef,
        msgid: MsgId,
        edit_of: MsgId,
        body: String,
    },
    /// `DELETED <target> <msgid>` — the tombstone (§7); `by=` optional.
    Deleted {
        target: Target,
        msgid: MsgId,
        by: Option<UserRef>,
    },
    /// `REACTION <target> <msgid> <emoji>` with `op=`, `by=` — live only.
    Reaction {
        target: Target,
        msgid: MsgId,
        emoji: String,
        op: ReactionOp,
        by: UserRef,
    },
    /// `REACTIONS <target> <msgid> <emoji> <count>` — batch summary form
    /// (§12.1); `by=` lists the first ≤20 actors, count is authoritative.
    Reactions {
        target: Target,
        msgid: MsgId,
        emoji: String,
        count: u64,
        by: Vec<UserRef>,
    },
    /// `BATCH START` with `id=` — opens a HISTORY page (§7).
    BatchStart {
        id: String,
    },
    /// `BATCH END` with `id=` + `truncated`/`compacted` flags. `truncated`
    /// marks retention gaps — silence about gaps is forbidden (§6.4).
    BatchEnd {
        id: String,
        truncated: bool,
        compacted: bool,
    },
    /// `TOKEN <subject> <scope>` with the minted capability token in the
    /// `token=` tag (§6.5, §10.4). Response to GRANT and to refresh.
    Token {
        subject: String,
        scope: String,
        token: String,
        expiry: Option<u64>,
    },
    /// `INVITED <scope> <invite-id> :<link>` — the shareable invite (§6.5);
    /// the unbound token rides the `token=` tag, `weft://<net>/i/<b64>` link
    /// in the trailing.
    Invited {
        scope: String,
        invite_id: String,
        token: String,
        link: Option<String>,
        max_uses: Option<u32>,
        expiry: Option<u64>,
    },
    /// `CHANMETA <#chan> <key> :<value>` (§7) — channel metadata change.
    Chanmeta {
        channel: ChannelName,
        key: String,
        value: String,
    },
    /// `NS-META <ns> <visibility>` with optional `owner=`/`title=`/
    /// `description=`/`icon=` tags, and the §2.4 recovery announcement
    /// fields: `recovery-set=yes` (a quorum exists), `recovery=pending`
    /// with `recovery-eta=<unix-ms>` + `recovery-rung=2|3` during a window.
    NsMeta {
        name: NamespaceName,
        visibility: Visibility,
        owner: Option<String>,
        title: Option<String>,
        description: Option<String>,
        icon: Option<String>,
        recovery_set: bool,
        /// `Some((eta_ms, rung))` while a recovery is pending (§2.4).
        recovery_pending: Option<(u64, u8)>,
    },
    /// `MORE <cursor>` — pagination continuation (DISCOVER, §6.2).
    More {
        cursor: String,
    },
    /// `CHANNEL-LAYOUT <#chan> <position>` with optional `category=` — one
    /// per channel in a namespace's layout (spec extension).
    ChannelLayout {
        channel: ChannelName,
        category: Option<String>,
        position: i64,
    },
    /// `REPORTED <report-id>` — ack to the reporter (§7); carries `label=`.
    Reported {
        report_id: String,
    },
    /// `REPORT-FILED <report-id> <msgid> <category>` with `state=`, `scope=`
    /// and optional `reporter=` (per config anonymization, §6.7) — delivered
    /// to `reports` cap holders and paged by `REPORTS LIST`.
    ReportFiled {
        report_id: String,
        msgid: MsgId,
        category: String,
        state: ContentState,
        scope: ReportScope,
        reporter: Option<String>,
    },
    /// `REPORT-RESOLVED <report-id> <action>` (§7). Handlers get the full
    /// form (optional `by=` handler + `note=`); the reporter gets the
    /// minimal form (neither) — reporter never learns handler identity.
    ReportResolved {
        report_id: String,
        action: ResolveAction,
        by: Option<String>,
        note: Option<String>,
    },
    /// `MANIFEST <peer> <version> <state>` with `channels=`/`history=`/
    /// `media=`/`typing=` tags — broadcast to affected members on any manifest
    /// change (§6.6, §11.5). The event payload was left "as v0.8" in the spec;
    /// resolved here (§6.6 amendment). `channels` lists the acked snapshot for
    /// `live`/`added`, or the affected channel for `removed`.
    Manifest {
        peer: NetworkName,
        version: u64,
        state: BridgeState,
        channels: Vec<ChannelName>,
        history: HistoryMode,
        media: MediaMode,
        typing: bool,
    },
    /// `NETBLOCKED <network> [:reason]` — sent to bridge owners when a manifest
    /// is severed by a NETBLOCK (§11.6). Reason is included per the network's
    /// `blocklist_visibility` config.
    Netblocked {
        network: NetworkName,
        reason: Option<String>,
    },
    /// `MODERATED <scope> <account> <action>` with `by=`/`reason=` tags (§6.7)
    /// — a moderation state change (mute/ban/kick), broadcast to a channel's
    /// members and echoed to the acting moderator.
    Moderated {
        scope: String,
        account: Account,
        action: ModAction,
        by: Option<Account>,
        reason: Option<String>,
    },
    Err(ErrEvent),
    /// Any event outside the known set — MUST be ignored by clients.
    Unknown {
        verb: String,
    },
}

/// Optional numeric tag (`retry-after=`, `max=`).
fn u64_tag(line: &Line, key: &'static str, verb: &'static str) -> Result<Option<u64>, ParseError> {
    line.tags
        .get(key)
        .map(|value| {
            value.parse().map_err(|_| ParseError::BadParam {
                verb,
                what: key,
                value: value.clone(),
            })
        })
        .transpose()
}

impl Event {
    pub fn from_line(line: &Line) -> Result<Self, ParseError> {
        match line.verb.as_str() {
            "WELCOME" => {
                let mut args = Args::new(line, "WELCOME");
                let features = line
                    .tags
                    .get("features")
                    .map(|v| {
                        v.split(',')
                            .filter(|f| !f.is_empty())
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(Event::Welcome {
                    network: args.req("network")?.parse()?,
                    features,
                    attestation: line
                        .tags
                        .get("attestation")
                        .filter(|v| !v.is_empty())
                        .cloned(),
                    motd: args.trailing_opt(),
                })
            }
            "CHALLENGE" => {
                let mut args = Args::new(line, "CHALLENGE");
                Ok(Event::Challenge {
                    nonce: args.req("nonce")?.to_string(),
                })
            }
            "MESSAGE" => {
                let mut args = Args::new(line, "MESSAGE");
                let msgid = line
                    .tags
                    .get("msgid")
                    .ok_or(ParseError::MissingParam {
                        verb: "MESSAGE",
                        what: "msgid tag",
                    })?
                    .parse()?;
                Ok(Event::Message(Box::new(MessageEvent {
                    target: args.req("target")?.parse()?,
                    sender: args.req("sender")?.parse()?,
                    msgid,
                    body: args.trailing_req("body")?.to_string(),
                    meta: MsgMeta::from_tags(&line.tags)?,
                    edited: u64_tag(line, "edited", "MESSAGE")?,
                    edited_at: u64_tag(line, "edited-at", "MESSAGE")?,
                })))
            }
            "MEMBER" => {
                let mut args = Args::new(line, "MEMBER");
                Ok(Event::Member {
                    channel: args.req("channel")?.parse()?,
                    user: args.req("user")?.parse()?,
                    action: args.req("action")?.parse()?,
                    display: line.tags.get("display").filter(|v| !v.is_empty()).cloned(),
                    count: u64_tag(line, "count", "MEMBER")?,
                })
            }
            "TYPING" => {
                let mut args = Args::new(line, "TYPING");
                Ok(Event::Typing {
                    channel: args.req("channel")?.parse()?,
                    user: args.req("user")?.parse()?,
                    state: args.req("state")?.parse()?,
                })
            }
            "MARKED" => {
                let mut args = Args::new(line, "MARKED");
                Ok(Event::Marked {
                    channel: args.req("channel")?.parse()?,
                    msgid: args.req("msgid")?.parse()?,
                })
            }
            "PRESENCE" => {
                let mut args = Args::new(line, "PRESENCE");
                Ok(Event::Presence {
                    user: args.req("user")?.parse()?,
                    status: args.req("status")?.parse()?,
                })
            }
            "POLICY" => {
                let mut args = Args::new(line, "POLICY");
                Ok(Event::Policy {
                    channel: args.req("channel")?.parse()?,
                    policy: args.req("policy")?.parse()?,
                })
            }
            "PONG" => {
                let mut args = Args::new(line, "PONG");
                Ok(Event::Pong {
                    token: args.opt().map(str::to_string),
                })
            }
            "EDITED" => {
                let mut args = Args::new(line, "EDITED");
                let tag_msgid = |key: &'static str| -> Result<MsgId, ParseError> {
                    line.tags
                        .get(key)
                        .ok_or(ParseError::MissingParam {
                            verb: "EDITED",
                            what: key,
                        })?
                        .parse()
                };
                Ok(Event::Edited {
                    target: args.req("target")?.parse()?,
                    user: args.req("user")?.parse()?,
                    msgid: tag_msgid("msgid")?,
                    edit_of: tag_msgid("edit-of")?,
                    body: args.trailing_req("new body")?.to_string(),
                })
            }
            "DELETED" => {
                let mut args = Args::new(line, "DELETED");
                Ok(Event::Deleted {
                    target: args.req("target")?.parse()?,
                    msgid: args.req("msgid")?.parse()?,
                    by: line.tags.get("by").map(|v| v.parse()).transpose()?,
                })
            }
            "REACTION" => {
                let mut args = Args::new(line, "REACTION");
                Ok(Event::Reaction {
                    target: args.req("target")?.parse()?,
                    msgid: args.req("msgid")?.parse()?,
                    emoji: args.req("emoji")?.to_string(),
                    op: line
                        .tags
                        .get("op")
                        .ok_or(ParseError::MissingParam {
                            verb: "REACTION",
                            what: "op tag",
                        })?
                        .parse()?,
                    by: line
                        .tags
                        .get("by")
                        .ok_or(ParseError::MissingParam {
                            verb: "REACTION",
                            what: "by tag",
                        })?
                        .parse()?,
                })
            }
            "REACTIONS" => {
                let mut args = Args::new(line, "REACTIONS");
                let target = args.req("target")?.parse()?;
                let msgid = args.req("msgid")?.parse()?;
                let emoji = args.req("emoji")?.to_string();
                let count = args.req("count")?;
                let count = count.parse().map_err(|_| ParseError::BadParam {
                    verb: "REACTIONS",
                    what: "count",
                    value: count.to_string(),
                })?;
                let by = line
                    .tags
                    .get("by")
                    .map(|v| {
                        v.split(',')
                            .filter(|a| !a.is_empty())
                            .map(str::parse)
                            .collect::<Result<Vec<UserRef>, _>>()
                    })
                    .transpose()?
                    .unwrap_or_default();
                Ok(Event::Reactions {
                    target,
                    msgid,
                    emoji,
                    count,
                    by,
                })
            }
            "BATCH" => {
                let mut args = Args::new(line, "BATCH");
                let sub = args.req("START|END")?.to_ascii_uppercase();
                let id = line
                    .tags
                    .get("id")
                    .filter(|v| !v.is_empty())
                    .cloned()
                    .ok_or(ParseError::MissingParam {
                        verb: "BATCH",
                        what: "id tag",
                    })?;
                match sub.as_str() {
                    "START" => Ok(Event::BatchStart { id }),
                    "END" => Ok(Event::BatchEnd {
                        id,
                        truncated: line.tags.contains_key("truncated"),
                        compacted: line.tags.contains_key("compacted"),
                    }),
                    _ => Err(ParseError::BadParam {
                        verb: "BATCH",
                        what: "subcommand",
                        value: sub,
                    }),
                }
            }
            "TOKEN" => {
                let mut args = Args::new(line, "TOKEN");
                let subject = args.req("subject")?.to_string();
                let scope = args.req("scope")?.to_string();
                let token = line
                    .tags
                    .get("token")
                    .filter(|v| !v.is_empty())
                    .cloned()
                    .ok_or(ParseError::MissingParam {
                        verb: "TOKEN",
                        what: "token tag",
                    })?;
                Ok(Event::Token {
                    subject,
                    scope,
                    token,
                    expiry: u64_tag(line, "expiry", "TOKEN")?,
                })
            }
            "INVITED" => {
                let mut args = Args::new(line, "INVITED");
                let scope = args.req("scope")?.to_string();
                let invite_id = args.req("invite-id")?.to_string();
                let token = line
                    .tags
                    .get("token")
                    .filter(|v| !v.is_empty())
                    .cloned()
                    .ok_or(ParseError::MissingParam {
                        verb: "INVITED",
                        what: "token tag",
                    })?;
                Ok(Event::Invited {
                    scope,
                    invite_id,
                    token,
                    link: args.trailing_opt(),
                    max_uses: line
                        .tags
                        .get("max-uses")
                        .map(|v| {
                            v.parse().map_err(|_| ParseError::BadParam {
                                verb: "INVITED",
                                what: "max-uses",
                                value: v.clone(),
                            })
                        })
                        .transpose()?,
                    expiry: u64_tag(line, "expiry", "INVITED")?,
                })
            }
            "CHANMETA" => {
                let mut args = Args::new(line, "CHANMETA");
                Ok(Event::Chanmeta {
                    channel: args.req("channel")?.parse()?,
                    key: args.req("key")?.to_string(),
                    value: args.trailing_req("value")?.to_string(),
                })
            }
            "NS-META" => {
                let mut args = Args::new(line, "NS-META");
                let tag = |k: &str| line.tags.get(k).filter(|v| !v.is_empty()).cloned();
                let recovery_pending =
                    if line.tags.get("recovery").map(String::as_str) == Some("pending") {
                        let eta = u64_tag(line, "recovery-eta", "NS-META")?.unwrap_or(0);
                        let rung = u64_tag(line, "recovery-rung", "NS-META")?.unwrap_or(0) as u8;
                        Some((eta, rung))
                    } else {
                        None
                    };
                Ok(Event::NsMeta {
                    name: args.req("name")?.parse()?,
                    visibility: args.req("visibility")?.parse()?,
                    owner: tag("owner"),
                    title: tag("title"),
                    description: tag("description"),
                    icon: tag("icon"),
                    recovery_set: line.tags.get("recovery-set").map(String::as_str) == Some("yes"),
                    recovery_pending,
                })
            }
            "MORE" => {
                let mut args = Args::new(line, "MORE");
                Ok(Event::More {
                    cursor: args.req("cursor")?.to_string(),
                })
            }
            "CHANNEL-LAYOUT" => {
                let mut args = Args::new(line, "CHANNEL-LAYOUT");
                let channel = args.req("channel")?.parse()?;
                let position = args.req("position")?;
                let position = position.parse().map_err(|_| ParseError::BadParam {
                    verb: "CHANNEL-LAYOUT",
                    what: "position",
                    value: position.to_string(),
                })?;
                Ok(Event::ChannelLayout {
                    channel,
                    category: line.tags.get("category").filter(|v| !v.is_empty()).cloned(),
                    position,
                })
            }
            "REPORTED" => {
                let mut args = Args::new(line, "REPORTED");
                Ok(Event::Reported {
                    report_id: args.req("report-id")?.to_string(),
                })
            }
            "REPORT-FILED" => {
                let mut args = Args::new(line, "REPORT-FILED");
                let report_id = args.req("report-id")?.to_string();
                let msgid = args.req("msgid")?.parse()?;
                let category = args.req("category")?.to_string();
                let state = line
                    .tags
                    .get("state")
                    .ok_or(ParseError::MissingParam {
                        verb: "REPORT-FILED",
                        what: "state tag",
                    })?
                    .parse()?;
                let scope = line
                    .tags
                    .get("scope")
                    .ok_or(ParseError::MissingParam {
                        verb: "REPORT-FILED",
                        what: "scope tag",
                    })?
                    .parse()?;
                Ok(Event::ReportFiled {
                    report_id,
                    msgid,
                    category,
                    state,
                    scope,
                    reporter: line.tags.get("reporter").filter(|v| !v.is_empty()).cloned(),
                })
            }
            "REPORT-RESOLVED" => {
                let mut args = Args::new(line, "REPORT-RESOLVED");
                Ok(Event::ReportResolved {
                    report_id: args.req("report-id")?.to_string(),
                    action: args.req("action")?.parse()?,
                    by: line.tags.get("by").filter(|v| !v.is_empty()).cloned(),
                    note: line.tags.get("note").filter(|v| !v.is_empty()).cloned(),
                })
            }
            "ERR" => {
                let mut args = Args::new(line, "ERR");
                Ok(Event::Err(ErrEvent {
                    code: args.req("code")?.parse()?,
                    context: args.opt().map(str::to_string),
                    text: args.trailing_req("text")?.to_string(),
                    retry_after: u64_tag(line, "retry-after", "ERR")?,
                    max: u64_tag(line, "max", "ERR")?,
                }))
            }
            "MANIFEST" => {
                let mut args = Args::new(line, "MANIFEST");
                let peer = args.req("peer")?.parse()?;
                let version = args.req("version")?;
                let version = version.parse().map_err(|_| ParseError::BadParam {
                    verb: "MANIFEST",
                    what: "version",
                    value: version.to_string(),
                })?;
                let state = args.req("state")?.parse()?;
                let channels = line
                    .tags
                    .get("channels")
                    .map(|v| {
                        v.split(',')
                            .filter(|c| !c.is_empty())
                            .map(str::parse)
                            .collect::<Result<Vec<ChannelName>, _>>()
                    })
                    .transpose()?
                    .unwrap_or_default();
                Ok(Event::Manifest {
                    peer,
                    version,
                    state,
                    channels,
                    history: line
                        .tags
                        .get("history")
                        .map(|v| v.parse())
                        .transpose()?
                        .unwrap_or(HistoryMode::FromEpoch),
                    media: line
                        .tags
                        .get("media")
                        .map(|v| v.parse())
                        .transpose()?
                        .unwrap_or(MediaMode::None),
                    typing: line.tags.get("typing").map(String::as_str) == Some("yes"),
                })
            }
            "NETBLOCKED" => {
                let mut args = Args::new(line, "NETBLOCKED");
                Ok(Event::Netblocked {
                    network: args.req("network")?.parse()?,
                    reason: args.trailing_opt(),
                })
            }
            "MODERATED" => {
                let mut args = Args::new(line, "MODERATED");
                let scope = args.req("scope")?.to_string();
                let account = args.req("account")?.parse()?;
                let action = args.req("action")?.parse()?;
                Ok(Event::Moderated {
                    scope,
                    account,
                    action,
                    by: line
                        .tags
                        .get("by")
                        .filter(|v| !v.is_empty())
                        .map(|v| v.parse())
                        .transpose()?,
                    reason: line.tags.get("reason").filter(|v| !v.is_empty()).cloned(),
                })
            }
            verb => Ok(Event::Unknown {
                verb: verb.to_string(),
            }),
        }
    }

    pub fn to_line(&self) -> Result<Line, SerializeError> {
        let mut tags = Tags::new();
        let (verb, params, trailing): (&str, Vec<String>, Option<String>) = match self {
            Event::Welcome {
                network,
                features,
                attestation,
                motd,
            } => {
                if !features.is_empty() {
                    for feature in features {
                        // Commas separate flags, so a flag containing one has no wire form.
                        if feature.is_empty() || feature.contains(',') {
                            return Err(SerializeError::Unrepresentable("feature flag"));
                        }
                    }
                    tags.insert("features".to_string(), features.join(","));
                }
                if let Some(attestation) = attestation {
                    tags.insert("attestation".to_string(), attestation.clone());
                }
                ("WELCOME", vec![network.to_string()], motd.clone())
            }
            Event::Challenge { nonce } => ("CHALLENGE", vec![nonce.clone()], None),
            Event::Message(message) => {
                message.meta.write_tags(&mut tags)?;
                tags.insert("msgid".to_string(), message.msgid.to_string());
                if let Some(edited) = message.edited {
                    tags.insert("edited".to_string(), edited.to_string());
                }
                if let Some(edited_at) = message.edited_at {
                    tags.insert("edited-at".to_string(), edited_at.to_string());
                }
                (
                    "MESSAGE",
                    vec![message.target.to_string(), message.sender.to_string()],
                    Some(message.body.clone()),
                )
            }
            Event::Member {
                channel,
                user,
                action,
                display,
                count,
            } => {
                if let Some(display) = display {
                    tags.insert("display".to_string(), display.clone());
                }
                if let Some(count) = count {
                    tags.insert("count".to_string(), count.to_string());
                }
                (
                    "MEMBER",
                    vec![channel.to_string(), user.to_string(), action.to_string()],
                    None,
                )
            }
            Event::Typing {
                channel,
                user,
                state,
            } => (
                "TYPING",
                vec![channel.to_string(), user.to_string(), state.to_string()],
                None,
            ),
            Event::Marked { channel, msgid } => {
                ("MARKED", vec![channel.to_string(), msgid.to_string()], None)
            }
            Event::Presence { user, status } => {
                ("PRESENCE", vec![user.to_string(), status.to_string()], None)
            }
            Event::Policy { channel, policy } => (
                "POLICY",
                vec![channel.to_string(), policy.to_string()],
                None,
            ),
            Event::Pong { token } => ("PONG", token.iter().cloned().collect(), None),
            Event::Edited {
                target,
                user,
                msgid,
                edit_of,
                body,
            } => {
                tags.insert("msgid".to_string(), msgid.to_string());
                tags.insert("edit-of".to_string(), edit_of.to_string());
                (
                    "EDITED",
                    vec![target.to_string(), user.to_string()],
                    Some(body.clone()),
                )
            }
            Event::Deleted { target, msgid, by } => {
                if let Some(by) = by {
                    tags.insert("by".to_string(), by.to_string());
                }
                ("DELETED", vec![target.to_string(), msgid.to_string()], None)
            }
            Event::Reaction {
                target,
                msgid,
                emoji,
                op,
                by,
            } => {
                tags.insert("op".to_string(), op.to_string());
                tags.insert("by".to_string(), by.to_string());
                (
                    "REACTION",
                    vec![target.to_string(), msgid.to_string(), emoji.clone()],
                    None,
                )
            }
            Event::Reactions {
                target,
                msgid,
                emoji,
                count,
                by,
            } => {
                if !by.is_empty() {
                    let actors: Vec<String> = by.iter().map(UserRef::to_string).collect();
                    tags.insert("by".to_string(), actors.join(","));
                }
                (
                    "REACTIONS",
                    vec![
                        target.to_string(),
                        msgid.to_string(),
                        emoji.clone(),
                        count.to_string(),
                    ],
                    None,
                )
            }
            Event::BatchStart { id } => {
                tags.insert("id".to_string(), id.clone());
                ("BATCH", vec!["START".to_string()], None)
            }
            Event::BatchEnd {
                id,
                truncated,
                compacted,
            } => {
                tags.insert("id".to_string(), id.clone());
                if *truncated {
                    tags.insert("truncated".to_string(), String::new());
                }
                if *compacted {
                    tags.insert("compacted".to_string(), String::new());
                }
                ("BATCH", vec!["END".to_string()], None)
            }
            Event::Token {
                subject,
                scope,
                token,
                expiry,
            } => {
                tags.insert("token".to_string(), token.clone());
                if let Some(expiry) = expiry {
                    tags.insert("expiry".to_string(), expiry.to_string());
                }
                ("TOKEN", vec![subject.clone(), scope.clone()], None)
            }
            Event::Invited {
                scope,
                invite_id,
                token,
                link,
                max_uses,
                expiry,
            } => {
                tags.insert("token".to_string(), token.clone());
                if let Some(max_uses) = max_uses {
                    tags.insert("max-uses".to_string(), max_uses.to_string());
                }
                if let Some(expiry) = expiry {
                    tags.insert("expiry".to_string(), expiry.to_string());
                }
                (
                    "INVITED",
                    vec![scope.clone(), invite_id.clone()],
                    link.clone(),
                )
            }
            Event::Chanmeta {
                channel,
                key,
                value,
            } => (
                "CHANMETA",
                vec![channel.to_string(), key.clone()],
                Some(value.clone()),
            ),
            Event::NsMeta {
                name,
                visibility,
                owner,
                title,
                description,
                icon,
                recovery_set,
                recovery_pending,
            } => {
                for (k, v) in [
                    ("owner", owner),
                    ("title", title),
                    ("description", description),
                    ("icon", icon),
                ] {
                    if let Some(v) = v {
                        tags.insert(k.to_string(), v.clone());
                    }
                }
                if *recovery_set {
                    tags.insert("recovery-set".to_string(), "yes".to_string());
                }
                if let Some((eta, rung)) = recovery_pending {
                    tags.insert("recovery".to_string(), "pending".to_string());
                    tags.insert("recovery-eta".to_string(), eta.to_string());
                    tags.insert("recovery-rung".to_string(), rung.to_string());
                }
                (
                    "NS-META",
                    vec![name.to_string(), visibility.to_string()],
                    None,
                )
            }
            Event::More { cursor } => ("MORE", vec![cursor.clone()], None),
            Event::ChannelLayout {
                channel,
                category,
                position,
            } => {
                if let Some(category) = category {
                    tags.insert("category".to_string(), category.clone());
                }
                (
                    "CHANNEL-LAYOUT",
                    vec![channel.to_string(), position.to_string()],
                    None,
                )
            }
            Event::Reported { report_id } => ("REPORTED", vec![report_id.clone()], None),
            Event::ReportFiled {
                report_id,
                msgid,
                category,
                state,
                scope,
                reporter,
            } => {
                tags.insert("state".to_string(), state.to_string());
                tags.insert("scope".to_string(), scope.to_string());
                if let Some(reporter) = reporter {
                    tags.insert("reporter".to_string(), reporter.clone());
                }
                (
                    "REPORT-FILED",
                    vec![report_id.clone(), msgid.to_string(), category.clone()],
                    None,
                )
            }
            Event::ReportResolved {
                report_id,
                action,
                by,
                note,
            } => {
                if let Some(by) = by {
                    tags.insert("by".to_string(), by.clone());
                }
                if let Some(note) = note {
                    tags.insert("note".to_string(), note.clone());
                }
                (
                    "REPORT-RESOLVED",
                    vec![report_id.clone(), action.to_string()],
                    None,
                )
            }
            Event::Err(err) => {
                if let Some(retry_after) = err.retry_after {
                    tags.insert("retry-after".to_string(), retry_after.to_string());
                }
                if let Some(max) = err.max {
                    tags.insert("max".to_string(), max.to_string());
                }
                let mut params = vec![err.code.to_string()];
                params.extend(err.context.iter().cloned());
                ("ERR", params, Some(err.text.clone()))
            }
            Event::Manifest {
                peer,
                version,
                state,
                channels,
                history,
                media,
                typing,
            } => {
                if !channels.is_empty() {
                    let list: Vec<String> = channels.iter().map(ChannelName::to_string).collect();
                    tags.insert("channels".to_string(), list.join(","));
                }
                tags.insert("history".to_string(), history.to_string());
                tags.insert("media".to_string(), media.to_string());
                tags.insert(
                    "typing".to_string(),
                    if *typing { "yes" } else { "no" }.to_string(),
                );
                (
                    "MANIFEST",
                    vec![peer.to_string(), version.to_string(), state.to_string()],
                    None,
                )
            }
            Event::Netblocked { network, reason } => {
                ("NETBLOCKED", vec![network.to_string()], reason.clone())
            }
            Event::Moderated {
                scope,
                account,
                action,
                by,
                reason,
            } => {
                if let Some(by) = by {
                    tags.insert("by".to_string(), by.to_string());
                }
                if let Some(reason) = reason {
                    tags.insert("reason".to_string(), reason.clone());
                }
                (
                    "MODERATED",
                    vec![scope.clone(), account.to_string(), action.to_string()],
                    None,
                )
            }
            Event::Unknown { .. } => {
                return Err(SerializeError::Unrepresentable("unknown event"));
            }
        };
        Ok(Line {
            tags,
            verb: verb.to_string(),
            params,
            trailing,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MSGID: &str = "hda.example/01ARZ3NDEKTSV4RRFFQ69G5FAV";

    fn round_trip(reply: &Reply) {
        let wire = reply.serialize().unwrap();
        assert_eq!(&Reply::parse(&wire).unwrap(), reply, "wire: {wire}");
    }

    #[test]
    fn welcome_matches_spec_example() {
        // §3.6 example line.
        let reply =
            Reply::parse("@features=media,backfill,voice,irc-gw WELCOME hda.example :Willkommen")
                .unwrap();
        let Event::Welcome {
            network,
            features,
            attestation,
            motd,
        } = &reply.event
        else {
            panic!("not WELCOME: {reply:?}");
        };
        assert_eq!(network.as_str(), "hda.example");
        assert_eq!(features, &["media", "backfill", "voice", "irc-gw"]);
        assert_eq!(attestation, &None);
        assert_eq!(motd.as_deref(), Some("Willkommen"));
        round_trip(&reply);
    }

    #[test]
    fn welcome_with_attestation_round_trips() {
        round_trip(&Reply::new(Event::Welcome {
            network: "hda.example".parse().unwrap(),
            features: vec![],
            attestation: Some("B64ATT==".into()),
            motd: None,
        }));
    }

    #[test]
    fn challenge_round_trips() {
        round_trip(&Reply::new(Event::Challenge {
            nonce: "B64NONCE==".into(),
        }));
    }

    #[test]
    fn message_event_round_trips_and_requires_msgid() {
        round_trip(&Reply::with_label(
            Event::Message(Box::new(MessageEvent {
                target: "#general".parse().unwrap(),
                sender: "ada@hda.example".parse().unwrap(),
                msgid: MSGID.parse().unwrap(),
                body: "hello there".into(),
                meta: MsgMeta {
                    fmt: Some("md".into()),
                    ..MsgMeta::default()
                },
                edited: None,
                edited_at: None,
            })),
            "req-1", // echo copy carries the sender's label = the ack (§9.2)
        ));
        // DM copy.
        round_trip(&Reply::new(Event::Message(Box::new(MessageEvent {
            target: "@ada".parse().unwrap(),
            sender: "bob@hda.example".parse().unwrap(),
            msgid: MSGID.parse().unwrap(),
            body: String::new(),
            meta: MsgMeta {
                attachments: vec!["ref1".into()],
                ..MsgMeta::default()
            },
            edited: None,
            edited_at: None,
        }))));
        assert_eq!(
            Reply::parse("MESSAGE #general ada@hda.example :hi"),
            Err(ParseError::MissingParam {
                verb: "MESSAGE",
                what: "msgid tag"
            })
        );
    }

    #[test]
    fn member_typing_marked_presence_policy_round_trip() {
        // JOIN response form: label + count= (§6.3).
        let join_echo = Reply::with_label(
            Event::Member {
                channel: "#general".parse().unwrap(),
                user: "ada@hda.example".parse().unwrap(),
                action: MemberAction::Join,
                display: Some("Ada L.".into()),
                count: Some(3),
            },
            "j1",
        );
        assert_eq!(
            join_echo.serialize().unwrap(),
            "@count=3;display=Ada\\sL.;label=j1 MEMBER #general ada@hda.example join"
        );
        round_trip(&join_echo);
        // Broadcast form: no label, tags optional.
        round_trip(&Reply::new(Event::Member {
            channel: "#general".parse().unwrap(),
            user: "ada@hda.example".parse().unwrap(),
            action: MemberAction::Part,
            display: None,
            count: None,
        }));
        round_trip(&Reply::new(Event::Typing {
            channel: "#general".parse().unwrap(),
            user: "ada@hda.example".parse().unwrap(),
            state: TypingState::Start,
        }));
        round_trip(&Reply::new(Event::Marked {
            channel: "#general".parse().unwrap(),
            msgid: MSGID.parse().unwrap(),
        }));
        round_trip(&Reply::new(Event::Presence {
            user: "ada@hda.example".parse().unwrap(),
            status: PresenceStatus::Away,
        }));
        round_trip(&Reply::new(Event::Policy {
            channel: "#general".parse().unwrap(),
            policy: "retained:90d".parse().unwrap(),
        }));
        round_trip(&Reply::new(Event::Pong {
            token: Some("t1".into()),
        }));
    }

    #[test]
    fn err_round_trips_with_tags_and_context() {
        let reply = Reply::with_label(
            Event::Err(ErrEvent {
                code: ErrCode::Throttled,
                context: None,
                text: "slow down".into(),
                retry_after: Some(30),
                max: None,
            }),
            "req-9",
        );
        assert_eq!(
            reply.serialize().unwrap(),
            "@label=req-9;retry-after=30 ERR THROTTLED :slow down"
        );
        round_trip(&reply);

        // CAP-REQUIRED names the capability as context (§8).
        let reply = Reply::new(Event::Err(ErrEvent {
            code: ErrCode::CapRequired,
            context: Some("send".into()),
            text: "missing capability".into(),
            retry_after: None,
            max: Some(10),
        }));
        assert_eq!(
            reply.serialize().unwrap(),
            "@max=10 ERR CAP-REQUIRED send :missing capability"
        );
        round_trip(&reply);

        assert!(matches!(
            Reply::parse("@retry-after=soon ERR THROTTLED :x"),
            Err(ParseError::BadParam {
                verb: "ERR",
                what: "retry-after",
                ..
            })
        ));
    }

    #[test]
    fn batch_form_message_round_trips_with_edit_tags() {
        // §12.1 wire form: final body + edited count/timestamp, no EDITED chain.
        let reply = Reply::new(Event::Message(Box::new(MessageEvent {
            target: "#general".parse().unwrap(),
            sender: "ada@hda.example".parse().unwrap(),
            msgid: MSGID.parse().unwrap(),
            body: "final text".into(),
            meta: MsgMeta::default(),
            edited: Some(3),
            edited_at: Some(1_700_000_000_000),
        })));
        let wire = reply.serialize().unwrap();
        assert!(wire.contains("edited=3"), "{wire}");
        assert!(wire.contains("edited-at=1700000000000"), "{wire}");
        round_trip(&reply);
    }

    #[test]
    fn edited_event_round_trips_live_form() {
        round_trip(&Reply::new(Event::Edited {
            target: "#general".parse().unwrap(),
            user: "ada@hda.example".parse().unwrap(),
            msgid: "hda.example/01ARZ3NDEKTSV4RRFFQ69G5FB0".parse().unwrap(),
            edit_of: MSGID.parse().unwrap(),
            body: "corrected".into(),
        }));
        // Both msgid= and edit-of= are required.
        assert!(Reply::parse(
            "@msgid=hda.example/01ARZ3NDEKTSV4RRFFQ69G5FB0 EDITED #a ada@hda.example :x"
        )
        .is_err());
    }

    #[test]
    fn deleted_tombstone_round_trips() {
        round_trip(&Reply::new(Event::Deleted {
            target: "#general".parse().unwrap(),
            msgid: MSGID.parse().unwrap(),
            by: Some("mod@hda.example".parse().unwrap()),
        }));
        round_trip(&Reply::new(Event::Deleted {
            target: "@ada".parse().unwrap(),
            msgid: MSGID.parse().unwrap(),
            by: None,
        }));
    }

    #[test]
    fn reaction_live_and_summary_forms_round_trip() {
        round_trip(&Reply::new(Event::Reaction {
            target: "#general".parse().unwrap(),
            msgid: MSGID.parse().unwrap(),
            emoji: "🦀".into(),
            op: ReactionOp::Add,
            by: "ada@hda.example".parse().unwrap(),
        }));
        // Batch summary (§12.1): count authoritative, actors capped upstream.
        // Shortcodes travel bare — `:ferris:` would collide with the §4
        // trailing marker (spec §18 #8).
        let reply = Reply::new(Event::Reactions {
            target: "#general".parse().unwrap(),
            msgid: MSGID.parse().unwrap(),
            emoji: "ferris".into(),
            count: 41,
            by: vec![
                "ada@hda.example".parse().unwrap(),
                "bob@hda.example".parse().unwrap(),
            ],
        });
        let wire = reply.serialize().unwrap();
        assert!(
            wire.contains("by=ada@hda.example,bob@hda.example"),
            "{wire}"
        );
        round_trip(&reply);
        // Empty actor list stays representable (by= omitted).
        round_trip(&Reply::new(Event::Reactions {
            target: "#general".parse().unwrap(),
            msgid: MSGID.parse().unwrap(),
            emoji: "x".into(),
            count: 0,
            by: vec![],
        }));
    }

    #[test]
    fn batch_brackets_round_trip() {
        let start = Reply::with_label(Event::BatchStart { id: "b1".into() }, "h1");
        assert_eq!(start.serialize().unwrap(), "@id=b1;label=h1 BATCH START");
        round_trip(&start);

        let end = Reply::with_label(
            Event::BatchEnd {
                id: "b1".into(),
                truncated: true,
                compacted: true,
            },
            "h1",
        );
        // Flag tags carry no value (§4).
        assert_eq!(
            end.serialize().unwrap(),
            "@compacted;id=b1;label=h1;truncated BATCH END"
        );
        round_trip(&end);
        round_trip(&Reply::new(Event::BatchEnd {
            id: "b2".into(),
            truncated: false,
            compacted: false,
        }));
        assert!(Reply::parse("BATCH START").is_err()); // id required
    }

    #[test]
    fn token_event_round_trips() {
        let reply = Reply::with_label(
            Event::Token {
                subject: "ada".into(),
                scope: "ns:gaming".into(),
                token: "B64TOKENBLOB==".into(),
                expiry: Some(3600),
            },
            "g1",
        );
        let wire = reply.serialize().unwrap();
        assert!(wire.contains("token=B64TOKENBLOB=="), "{wire}");
        round_trip(&reply);
        // token= tag is mandatory.
        assert_eq!(
            Reply::parse("TOKEN ada ns:gaming"),
            Err(ParseError::MissingParam {
                verb: "TOKEN",
                what: "token tag"
            })
        );
    }

    #[test]
    fn invited_event_round_trips_with_link() {
        round_trip(&Reply::with_label(
            Event::Invited {
                scope: "ns:gaming".into(),
                invite_id: "inv-1".into(),
                token: "B64UNBOUND==".into(),
                link: Some("weft://hda.example/i/B64UNBOUND==".into()),
                max_uses: Some(5),
                expiry: None,
            },
            "i1",
        ));
        // Minimal form: no link, no limits.
        round_trip(&Reply::new(Event::Invited {
            scope: "#general".into(),
            invite_id: "inv-2".into(),
            token: "B64==".into(),
            link: None,
            max_uses: None,
            expiry: None,
        }));
    }

    #[test]
    fn chanmeta_event_round_trips() {
        round_trip(&Reply::new(Event::Chanmeta {
            channel: "#general".parse().unwrap(),
            key: "topic".into(),
            value: "welcome to the channel".into(),
        }));
    }

    #[test]
    fn channel_layout_round_trips() {
        round_trip(&Reply::with_label(
            Event::ChannelLayout {
                channel: "#gaming/general".parse().unwrap(),
                category: Some("text".into()),
                position: 3,
            },
            "c1",
        ));
        // Uncategorized.
        round_trip(&Reply::new(Event::ChannelLayout {
            channel: "#gaming/lobby".parse().unwrap(),
            category: None,
            position: 0,
        }));
    }

    #[test]
    fn ns_meta_and_more_round_trip() {
        round_trip(&Reply::with_label(
            Event::NsMeta {
                name: "gaming".parse().unwrap(),
                visibility: crate::types::Visibility::Public,
                owner: Some("ada".into()),
                title: Some("The Lounge".into()),
                description: None,
                icon: None,
                recovery_set: true,
                recovery_pending: Some((1_700_000_000_000, 2)),
            },
            "n1",
        ));
        // Minimal: just name + visibility.
        round_trip(&Reply::new(Event::NsMeta {
            name: "quiet".parse().unwrap(),
            visibility: crate::types::Visibility::Unlisted,
            owner: None,
            title: None,
            description: None,
            icon: None,
            recovery_set: false,
            recovery_pending: None,
        }));
        round_trip(&Reply::new(Event::More {
            cursor: "next-page".into(),
        }));
    }

    #[test]
    fn report_events_round_trip() {
        round_trip(&Reply::with_label(
            Event::Reported {
                report_id: "rep-42".into(),
            },
            "r1",
        ));
        // Filed form to handlers: state/scope tags mandatory, reporter shown.
        let filed = Reply::new(Event::ReportFiled {
            report_id: "rep-42".into(),
            msgid: MSGID.parse().unwrap(),
            category: "harassment".into(),
            state: crate::types::ContentState::Verified,
            scope: crate::types::ReportScope::Ns,
            reporter: Some("ada@hda.example".into()),
        });
        let wire = filed.serialize().unwrap();
        assert!(wire.contains("state=verified"), "{wire}");
        assert!(wire.contains("scope=ns"), "{wire}");
        round_trip(&filed);
        // Anonymized form (reporter omitted) still parses.
        round_trip(&Reply::new(Event::ReportFiled {
            report_id: "rep-9".into(),
            msgid: MSGID.parse().unwrap(),
            category: "csam".into(),
            state: crate::types::ContentState::Unverified,
            scope: crate::types::ReportScope::Net,
            reporter: None,
        }));
        // state/scope are mandatory on the way in.
        assert!(Reply::parse(&format!("REPORT-FILED rep-1 {MSGID} spam")).is_err());
        // Handler resolution (full form) and reporter's minimal form.
        round_trip(&Reply::new(Event::ReportResolved {
            report_id: "rep-42".into(),
            action: crate::types::ResolveAction::UserActioned,
            by: Some("mod@hda.example".into()),
            note: Some("banned 7d".into()),
        }));
        round_trip(&Reply::with_label(
            Event::ReportResolved {
                report_id: "rep-42".into(),
                action: crate::types::ResolveAction::Dismissed,
                by: None,
                note: None,
            },
            "r1",
        ));
    }

    #[test]
    fn manifest_event_round_trips() {
        let live = Reply::with_label(
            Event::Manifest {
                peer: "hda.example".parse().unwrap(),
                version: 2,
                state: BridgeState::Live,
                channels: vec![
                    "#general".parse().unwrap(),
                    "#gaming/lobby".parse().unwrap(),
                ],
                history: HistoryMode::Full,
                media: MediaMode::MirrorMax(1_000_000),
                typing: true,
            },
            "m1",
        );
        let wire = live.serialize().unwrap();
        assert!(wire.contains("channels=#general,#gaming/lobby"), "{wire}");
        assert!(wire.contains("history=full"), "{wire}");
        assert!(wire.contains("media=mirror-max:1000000"), "{wire}");
        assert!(wire.contains("typing=yes"), "{wire}");
        assert!(wire.contains("MANIFEST hda.example 2 live"), "{wire}");
        round_trip(&live);

        // Severed form: no channels, strictest defaults.
        round_trip(&Reply::new(Event::Manifest {
            peer: "peer.example".parse().unwrap(),
            version: 5,
            state: BridgeState::Severed,
            channels: vec![],
            history: HistoryMode::FromEpoch,
            media: MediaMode::None,
            typing: false,
        }));
        assert!(Reply::parse("MANIFEST hda.example notanumber live").is_err());
    }

    #[test]
    fn netblocked_event_round_trips() {
        round_trip(&Reply::with_label(
            Event::Netblocked {
                network: "evil.example".parse().unwrap(),
                reason: Some("chronic abuse".into()),
            },
            "nb1",
        ));
        round_trip(&Reply::new(Event::Netblocked {
            network: "evil.example".parse().unwrap(),
            reason: None,
        }));
    }

    #[test]
    fn moderated_event_round_trips() {
        let full = Reply::with_label(
            Event::Moderated {
                scope: "#general".into(),
                account: "bob".parse().unwrap(),
                action: crate::types::ModAction::Mute,
                by: Some("mod".parse().unwrap()),
                reason: Some("spamming".into()),
            },
            "m1",
        );
        let wire = full.serialize().unwrap();
        assert!(wire.contains("by=mod"), "{wire}");
        assert!(wire.contains("MODERATED #general bob mute"), "{wire}");
        round_trip(&full);
        // Minimal form (broadcast, no by/reason).
        round_trip(&Reply::new(Event::Moderated {
            scope: "*".into(),
            account: "eve".parse().unwrap(),
            action: crate::types::ModAction::Ban,
            by: None,
            reason: None,
        }));
    }

    #[test]
    fn unknown_event_is_ignored_not_error() {
        // STREAM is a media event (M6) — still unknown here.
        let reply = Reply::parse("@label=l1 STREAM ACCEPT tok-9 :ready").unwrap();
        assert_eq!(
            reply.event,
            Event::Unknown {
                verb: "STREAM".into()
            }
        );
        assert_eq!(reply.label.as_deref(), Some("l1")); // label still visible for correlation
        assert_eq!(
            Reply::new(Event::Unknown { verb: "X".into() }).serialize(),
            Err(SerializeError::Unrepresentable("unknown event"))
        );
    }
}
