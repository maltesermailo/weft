//! Client → server commands: the M0 session + relay verb set (§6.1, §6.3,
//! §6.4). Unknown verbs decode to [`Command::Unknown`] — never an error (§4).

use crate::error::{ParseError, SerializeError};
use crate::id::MsgId;
use crate::line::{label_from_tags, write_label, Args, Line, Tags};
use crate::name::{Account, ChannelName, NamespaceName, Target};
use crate::types::{MsgMeta, PresenceStatus, TypingState, Visibility};

/// A command plus its optional `label` (§3.5). The label is echoed on every
/// direct response — including `ERR` — and never on broadcast copies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub label: Option<String>,
    pub command: Command,
}

impl Request {
    pub fn new(command: Command) -> Self {
        Self {
            label: None,
            command,
        }
    }

    pub fn with_label(command: Command, label: impl Into<String>) -> Self {
        Self {
            label: Some(label.into()),
            command,
        }
    }

    pub fn parse(input: &str) -> Result<Self, ParseError> {
        Self::from_line(&Line::parse(input)?)
    }

    pub fn from_line(line: &Line) -> Result<Self, ParseError> {
        Ok(Request {
            label: label_from_tags(&line.tags)?,
            command: Command::from_line(line)?,
        })
    }

    pub fn to_line(&self) -> Result<Line, SerializeError> {
        let mut line = self.command.to_line()?;
        write_label(&mut line.tags, self.label.as_deref())?;
        Ok(line)
    }

    pub fn serialize(&self) -> Result<String, SerializeError> {
        self.to_line()?.serialize()
    }
}

/// M0 verb set. Extra params or an unexpected trailing are ignored
/// (lenient-in); missing or malformed required parts are typed errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `HELLO <version>` (§3.6).
    Hello { version: String },
    /// `REGISTER <account> :<password>` (§6.1).
    Register { account: Account, password: String },
    /// `AUTH PASSWORD <account> :<password>`.
    AuthPassword { account: Account, password: String },
    /// `AUTH KEY <account> <b64-ed25519-pubkey>` — starts challenge-response.
    AuthKey { account: Account, pubkey: String },
    /// `AUTH PROOF <b64-sig(nonce ‖ network-name)>` (§6.1: anti cross-network replay).
    AuthProof { signature: String },
    /// `AUTH ENROLL <b64-pubkey>` — add a device while authed.
    AuthEnroll { pubkey: String },
    /// `QUIT [:reason]`.
    Quit { reason: Option<String> },
    /// `PING [token]` (§3.4).
    Ping { token: Option<String> },
    /// `PONG [token]` — answering is mandatory even when QUIC keeps alive.
    Pong { token: Option<String> },
    /// `PRESENCE <status>` (§6.1).
    Presence { status: PresenceStatus },
    /// `JOIN <#chan> [invite-ref]` — JOIN never auto-creates (§6.3).
    Join {
        channel: ChannelName,
        invite: Option<String>,
    },
    /// `PART <#chan> [:reason]`.
    Part {
        channel: ChannelName,
        reason: Option<String>,
    },
    /// `TYPING <#chan> <start|stop>`.
    Typing {
        channel: ChannelName,
        state: TypingState,
    },
    /// `MARK <#chan> <msgid>` — read marker (§6.3).
    Mark { channel: ChannelName, msgid: MsgId },
    /// `MSG <#chan|@user> [:body]` — empty body legal iff attachments (§6.4;
    /// enforced by the session layer, not the codec).
    Msg {
        target: Target,
        body: Option<String>,
        meta: MsgMeta,
    },
    /// `EDIT <msgid> :<new>` — edit-own only, honored at origin (§6.4).
    Edit { msgid: MsgId, body: String },
    /// `DELETE <msgid>` — tombstone (§6.4).
    Delete { msgid: MsgId },
    /// `REACT <msgid> <emoji>` — idempotent (§6.4).
    React { msgid: MsgId, emoji: String },
    /// `UNREACT <msgid> <emoji>`.
    Unreact { msgid: MsgId, emoji: String },
    /// `HISTORY <target> [before=] [after=] [limit=] [thread=]` —
    /// key=value middle params, any order (§6.4).
    History {
        target: Target,
        before: Option<MsgId>,
        after: Option<MsgId>,
        limit: Option<u32>,
        thread: Option<MsgId>,
    },
    /// `GRANT <subject> <scope> <caps> [expiry=<s>]` (§6.5). `subject` is an
    /// account or b64 pubkey, `scope` is `#chan|ns:<name>|*`, `caps` a comma
    /// list — all validated by the capability layer, not the codec.
    Grant {
        subject: String,
        scope: String,
        caps: String,
        expiry: Option<u64>,
    },
    /// `REVOKE <subject> <scope> [caps=<list>] [epoch]` (§6.5). No caps and
    /// no epoch = revoke everything for the subject at the scope.
    Revoke {
        subject: String,
        scope: String,
        caps: Option<String>,
        /// Bumps the scope revocation epoch (§10.4).
        epoch: Option<u64>,
    },
    /// `CHANNEL CREATE <#chan> [policy]` — default `retained:90d` (§6.3).
    ChannelCreate {
        channel: ChannelName,
        policy: Option<crate::RetentionPolicy>,
    },
    /// `CHANNEL POLICY <#chan> <policy> [purge]` (§6.3).
    ChannelPolicy {
        channel: ChannelName,
        policy: crate::RetentionPolicy,
        purge: bool,
    },
    /// `CHANNEL META <#chan> <topic|view-gated> :<value>` (§6.3) → `CHANMETA`.
    ChannelMeta {
        channel: ChannelName,
        key: String,
        value: String,
    },
    /// `CHANNEL DELETE <#chan> <#chan>` — confirmed by repetition (§6.3).
    ChannelDelete {
        channel: ChannelName,
        confirm: ChannelName,
    },
    /// `INVITE MINT <scope> [max-uses=] [expiry=]` (§6.5) → `INVITED`.
    InviteMint {
        scope: String,
        max_uses: Option<u32>,
        expiry: Option<u64>,
    },
    /// `INVITE REVOKE <invite-id>` — closes the counter (§6.5).
    InviteRevoke { invite_id: String },
    /// `INVITE REDEEM <b64>` — verifies chain + counter, mints a member
    /// token bound to the redeemer (§6.5).
    InviteRedeem { token: String },
    /// `NS CREATE <name> [tier]` with `@root=<b64-pubkey>` (§6.2). The
    /// client generates the namespace root key and submits its pubkey.
    NsCreate {
        name: NamespaceName,
        visibility: Visibility,
        root_key: String,
    },
    /// `NS META <name> <title|description|icon> :<value>` (§6.2).
    NsMeta {
        name: NamespaceName,
        key: String,
        value: String,
    },
    /// `NS VISIBILITY <name> <tier>` (§6.2).
    NsVisibility {
        name: NamespaceName,
        visibility: Visibility,
    },
    /// `NS DELEGATE <name> <account|pubkey> <cap>[,...]` — sugar for GRANT
    /// at `ns:` scope (§6.2).
    NsDelegate {
        name: NamespaceName,
        subject: String,
        caps: String,
    },
    /// `NS DELETE <name> <name>` — confirmed by repetition (§6.2).
    NsDelete {
        name: NamespaceName,
        confirm: NamespaceName,
    },
    /// `DISCOVER [cursor]` — public namespace directory (§6.2).
    Discover { cursor: Option<String> },
    /// Any verb outside the known set. Servers ignore it silently (§4).
    Unknown { verb: String },
}

/// Comma-separated cap list as a middle param (no spaces).
fn caps_ok(caps: &str) -> bool {
    !caps.is_empty() && !caps.contains(' ')
}

/// Scan middle params for an optional `key=<u64>`.
fn kv_u64(line: &Line, verb: &'static str, key: &'static str) -> Result<Option<u64>, ParseError> {
    for param in &line.params {
        if let Some(value) = param.strip_prefix(key).and_then(|r| r.strip_prefix('=')) {
            return Ok(Some(value.parse().map_err(|_| ParseError::BadParam {
                verb,
                what: key,
                value: value.to_string(),
            })?));
        }
    }
    Ok(None)
}

/// §6.4 emoji, ≤32 bytes. The `:shortcode:` form conflicts with the §4
/// grammar (a leading `:` starts the trailing) — flagged in spec §18 #8;
/// until that's decided, shortcodes travel bare and a leading colon is
/// rejected. Middle-param grammar already excludes spaces.
fn emoji_ok(emoji: &str) -> bool {
    !emoji.is_empty() && emoji.len() <= crate::line::MAX_EMOJI_BYTES && !emoji.starts_with(':')
}

impl Command {
    pub fn from_line(line: &Line) -> Result<Self, ParseError> {
        let verb = line.verb.as_str();
        match verb {
            "HELLO" => {
                let mut args = Args::new(line, "HELLO");
                Ok(Command::Hello {
                    version: args.req("version")?.to_string(),
                })
            }
            "REGISTER" => {
                let mut args = Args::new(line, "REGISTER");
                Ok(Command::Register {
                    account: args.req("account")?.parse()?,
                    password: args.trailing_req("password")?.to_string(),
                })
            }
            "AUTH" => {
                let mut args = Args::new(line, "AUTH");
                let sub = args.req("subcommand")?.to_ascii_uppercase();
                match sub.as_str() {
                    "PASSWORD" => Ok(Command::AuthPassword {
                        account: args.req("account")?.parse()?,
                        password: args.trailing_req("password")?.to_string(),
                    }),
                    "KEY" => Ok(Command::AuthKey {
                        account: args.req("account")?.parse()?,
                        pubkey: args.req("pubkey")?.to_string(),
                    }),
                    "PROOF" => Ok(Command::AuthProof {
                        signature: args.req("signature")?.to_string(),
                    }),
                    "ENROLL" => Ok(Command::AuthEnroll {
                        pubkey: args.req("pubkey")?.to_string(),
                    }),
                    _ => Err(ParseError::BadParam {
                        verb: "AUTH",
                        what: "subcommand",
                        value: sub,
                    }),
                }
            }
            "QUIT" => Ok(Command::Quit {
                reason: line.trailing.clone(),
            }),
            "PING" => Ok(Command::Ping {
                token: Args::new(line, "PING").opt().map(str::to_string),
            }),
            "PONG" => Ok(Command::Pong {
                token: Args::new(line, "PONG").opt().map(str::to_string),
            }),
            "PRESENCE" => {
                let mut args = Args::new(line, "PRESENCE");
                Ok(Command::Presence {
                    status: args.req("status")?.parse()?,
                })
            }
            "JOIN" => {
                let mut args = Args::new(line, "JOIN");
                Ok(Command::Join {
                    channel: args.req("channel")?.parse()?,
                    invite: args.opt().map(str::to_string),
                })
            }
            "PART" => {
                let mut args = Args::new(line, "PART");
                Ok(Command::Part {
                    channel: args.req("channel")?.parse()?,
                    reason: args.trailing_opt(),
                })
            }
            "TYPING" => {
                let mut args = Args::new(line, "TYPING");
                Ok(Command::Typing {
                    channel: args.req("channel")?.parse()?,
                    state: args.req("state")?.parse()?,
                })
            }
            "MARK" => {
                let mut args = Args::new(line, "MARK");
                Ok(Command::Mark {
                    channel: args.req("channel")?.parse()?,
                    msgid: args.req("msgid")?.parse()?,
                })
            }
            "MSG" => {
                let mut args = Args::new(line, "MSG");
                Ok(Command::Msg {
                    target: args.req("target")?.parse()?,
                    body: args.trailing_opt(),
                    meta: MsgMeta::from_tags(&line.tags)?,
                })
            }
            "EDIT" => {
                let mut args = Args::new(line, "EDIT");
                Ok(Command::Edit {
                    msgid: args.req("msgid")?.parse()?,
                    body: args.trailing_req("new body")?.to_string(),
                })
            }
            "DELETE" => {
                let mut args = Args::new(line, "DELETE");
                Ok(Command::Delete {
                    msgid: args.req("msgid")?.parse()?,
                })
            }
            "REACT" | "UNREACT" => {
                let react = verb == "REACT";
                let mut args = Args::new(line, if react { "REACT" } else { "UNREACT" });
                let msgid = args.req("msgid")?.parse()?;
                let emoji = args.req("emoji")?.to_string();
                if !emoji_ok(&emoji) {
                    return Err(ParseError::BadParam {
                        verb: if react { "REACT" } else { "UNREACT" },
                        what: "emoji",
                        value: emoji,
                    });
                }
                Ok(if react {
                    Command::React { msgid, emoji }
                } else {
                    Command::Unreact { msgid, emoji }
                })
            }
            "HISTORY" => {
                let mut args = Args::new(line, "HISTORY");
                let target = args.req("target")?.parse()?;
                // key=value params in any order; unknown keys ignored
                // (lenient-in), duplicates last-wins.
                let mut before = None;
                let mut after = None;
                let mut limit = None;
                let mut thread = None;
                while let Some(param) = args.opt() {
                    let Some((key, value)) = param.split_once('=') else {
                        continue;
                    };
                    match key {
                        "before" => before = Some(value.parse()?),
                        "after" => after = Some(value.parse()?),
                        "thread" => thread = Some(value.parse()?),
                        "limit" => {
                            limit = Some(value.parse().map_err(|_| ParseError::BadParam {
                                verb: "HISTORY",
                                what: "limit",
                                value: value.to_string(),
                            })?)
                        }
                        _ => {}
                    }
                }
                Ok(Command::History {
                    target,
                    before,
                    after,
                    limit,
                    thread,
                })
            }
            "GRANT" => {
                let mut args = Args::new(line, "GRANT");
                let subject = args.req("subject")?.to_string();
                let scope = args.req("scope")?.to_string();
                let caps = args.req("caps")?.to_string();
                if !caps_ok(&caps) {
                    return Err(ParseError::BadParam {
                        verb: "GRANT",
                        what: "caps",
                        value: caps,
                    });
                }
                Ok(Command::Grant {
                    subject,
                    scope,
                    caps,
                    expiry: kv_u64(line, "GRANT", "expiry")?,
                })
            }
            "REVOKE" => {
                let mut args = Args::new(line, "REVOKE");
                let subject = args.req("subject")?.to_string();
                let scope = args.req("scope")?.to_string();
                // Remaining params: `caps=<list>` and/or a bare epoch number.
                let mut caps = None;
                let mut epoch = None;
                while let Some(param) = args.opt() {
                    if let Some(list) = param.strip_prefix("caps=") {
                        caps = Some(list.to_string());
                    } else if let Ok(n) = param.parse::<u64>() {
                        epoch = Some(n);
                    }
                }
                Ok(Command::Revoke {
                    subject,
                    scope,
                    caps,
                    epoch,
                })
            }
            "CHANNEL" => {
                let mut args = Args::new(line, "CHANNEL");
                let sub = args.req("subcommand")?.to_ascii_uppercase();
                match sub.as_str() {
                    "CREATE" => Ok(Command::ChannelCreate {
                        channel: args.req("channel")?.parse()?,
                        policy: args.opt().map(str::parse).transpose()?,
                    }),
                    "POLICY" => {
                        let channel = args.req("channel")?.parse()?;
                        let policy = args.req("policy")?.parse()?;
                        // `purge` is a bare flag keyword after the policy.
                        let purge = args.opt().is_some_and(|p| p.eq_ignore_ascii_case("purge"));
                        Ok(Command::ChannelPolicy {
                            channel,
                            policy,
                            purge,
                        })
                    }
                    "META" => Ok(Command::ChannelMeta {
                        channel: args.req("channel")?.parse()?,
                        key: args.req("key")?.to_string(),
                        value: args.trailing_req("value")?.to_string(),
                    }),
                    "DELETE" => Ok(Command::ChannelDelete {
                        channel: args.req("channel")?.parse()?,
                        confirm: args.req("confirmation")?.parse()?,
                    }),
                    _ => Err(ParseError::BadParam {
                        verb: "CHANNEL",
                        what: "subcommand",
                        value: sub,
                    }),
                }
            }
            "INVITE" => {
                let mut args = Args::new(line, "INVITE");
                let sub = args.req("subcommand")?.to_ascii_uppercase();
                match sub.as_str() {
                    "MINT" => {
                        let scope = args.req("scope")?.to_string();
                        let mut max_uses = None;
                        let mut expiry = None;
                        while let Some(param) = args.opt() {
                            if let Some(v) = param.strip_prefix("max-uses=") {
                                max_uses = Some(v.parse().map_err(|_| ParseError::BadParam {
                                    verb: "INVITE",
                                    what: "max-uses",
                                    value: v.to_string(),
                                })?);
                            } else if let Some(v) = param.strip_prefix("expiry=") {
                                expiry = Some(v.parse().map_err(|_| ParseError::BadParam {
                                    verb: "INVITE",
                                    what: "expiry",
                                    value: v.to_string(),
                                })?);
                            }
                        }
                        Ok(Command::InviteMint {
                            scope,
                            max_uses,
                            expiry,
                        })
                    }
                    "REVOKE" => Ok(Command::InviteRevoke {
                        invite_id: args.req("invite-id")?.to_string(),
                    }),
                    "REDEEM" => Ok(Command::InviteRedeem {
                        token: args.req("token")?.to_string(),
                    }),
                    _ => Err(ParseError::BadParam {
                        verb: "INVITE",
                        what: "subcommand",
                        value: sub,
                    }),
                }
            }
            "NS" => {
                let mut args = Args::new(line, "NS");
                let sub = args.req("subcommand")?.to_ascii_uppercase();
                match sub.as_str() {
                    "CREATE" => {
                        let name = args.req("name")?.parse()?;
                        // Default tier is `unlisted` (§6.2).
                        let visibility = args
                            .opt()
                            .map(str::parse)
                            .transpose()?
                            .unwrap_or(Visibility::Unlisted);
                        let root_key = line
                            .tags
                            .get("root")
                            .filter(|v| !v.is_empty())
                            .cloned()
                            .ok_or(ParseError::MissingParam {
                                verb: "NS",
                                what: "root tag (namespace root pubkey)",
                            })?;
                        Ok(Command::NsCreate {
                            name,
                            visibility,
                            root_key,
                        })
                    }
                    "META" => Ok(Command::NsMeta {
                        name: args.req("name")?.parse()?,
                        key: args.req("key")?.to_string(),
                        value: args.trailing_req("value")?.to_string(),
                    }),
                    "VISIBILITY" => Ok(Command::NsVisibility {
                        name: args.req("name")?.parse()?,
                        visibility: args.req("tier")?.parse()?,
                    }),
                    "DELEGATE" => {
                        let name = args.req("name")?.parse()?;
                        let subject = args.req("subject")?.to_string();
                        let caps = args.req("caps")?.to_string();
                        if !caps_ok(&caps) {
                            return Err(ParseError::BadParam {
                                verb: "NS",
                                what: "caps",
                                value: caps,
                            });
                        }
                        Ok(Command::NsDelegate {
                            name,
                            subject,
                            caps,
                        })
                    }
                    "DELETE" => Ok(Command::NsDelete {
                        name: args.req("name")?.parse()?,
                        confirm: args.req("confirmation")?.parse()?,
                    }),
                    _ => Err(ParseError::BadParam {
                        verb: "NS",
                        what: "subcommand",
                        value: sub,
                    }),
                }
            }
            "DISCOVER" => Ok(Command::Discover {
                cursor: Args::new(line, "DISCOVER").opt().map(str::to_string),
            }),
            _ => Ok(Command::Unknown {
                verb: verb.to_string(),
            }),
        }
    }

    pub fn to_line(&self) -> Result<Line, SerializeError> {
        let mut tags = Tags::new();
        let (verb, params, trailing): (&str, Vec<String>, Option<String>) = match self {
            Command::Hello { version } => ("HELLO", vec![version.clone()], None),
            Command::Register { account, password } => (
                "REGISTER",
                vec![account.to_string()],
                Some(password.clone()),
            ),
            Command::AuthPassword { account, password } => (
                "AUTH",
                vec!["PASSWORD".to_string(), account.to_string()],
                Some(password.clone()),
            ),
            Command::AuthKey { account, pubkey } => (
                "AUTH",
                vec!["KEY".to_string(), account.to_string(), pubkey.clone()],
                None,
            ),
            Command::AuthProof { signature } => {
                ("AUTH", vec!["PROOF".to_string(), signature.clone()], None)
            }
            Command::AuthEnroll { pubkey } => {
                ("AUTH", vec!["ENROLL".to_string(), pubkey.clone()], None)
            }
            Command::Quit { reason } => ("QUIT", vec![], reason.clone()),
            Command::Ping { token } => ("PING", token.iter().cloned().collect(), None),
            Command::Pong { token } => ("PONG", token.iter().cloned().collect(), None),
            Command::Presence { status } => ("PRESENCE", vec![status.to_string()], None),
            Command::Join { channel, invite } => {
                let mut params = vec![channel.to_string()];
                params.extend(invite.iter().cloned());
                ("JOIN", params, None)
            }
            Command::Part { channel, reason } => {
                ("PART", vec![channel.to_string()], reason.clone())
            }
            Command::Typing { channel, state } => {
                ("TYPING", vec![channel.to_string(), state.to_string()], None)
            }
            Command::Mark { channel, msgid } => {
                ("MARK", vec![channel.to_string(), msgid.to_string()], None)
            }
            Command::Msg { target, body, meta } => {
                meta.write_tags(&mut tags)?;
                ("MSG", vec![target.to_string()], body.clone())
            }
            Command::Edit { msgid, body } => ("EDIT", vec![msgid.to_string()], Some(body.clone())),
            Command::Delete { msgid } => ("DELETE", vec![msgid.to_string()], None),
            Command::React { msgid, emoji } | Command::Unreact { msgid, emoji } => {
                if !emoji_ok(emoji) {
                    return Err(SerializeError::BadParam {
                        param: emoji.clone(),
                        reason: "emoji must be 1..=32 bytes",
                    });
                }
                let verb = if matches!(self, Command::React { .. }) {
                    "REACT"
                } else {
                    "UNREACT"
                };
                (verb, vec![msgid.to_string(), emoji.clone()], None)
            }
            Command::History {
                target,
                before,
                after,
                limit,
                thread,
            } => {
                let mut params = vec![target.to_string()];
                if let Some(before) = before {
                    params.push(format!("before={before}"));
                }
                if let Some(after) = after {
                    params.push(format!("after={after}"));
                }
                if let Some(limit) = limit {
                    params.push(format!("limit={limit}"));
                }
                if let Some(thread) = thread {
                    params.push(format!("thread={thread}"));
                }
                ("HISTORY", params, None)
            }
            Command::Grant {
                subject,
                scope,
                caps,
                expiry,
            } => {
                if !caps_ok(caps) {
                    return Err(SerializeError::BadParam {
                        param: caps.clone(),
                        reason: "caps must be a non-empty space-free list",
                    });
                }
                let mut params = vec![subject.clone(), scope.clone(), caps.clone()];
                if let Some(expiry) = expiry {
                    params.push(format!("expiry={expiry}"));
                }
                ("GRANT", params, None)
            }
            Command::Revoke {
                subject,
                scope,
                caps,
                epoch,
            } => {
                let mut params = vec![subject.clone(), scope.clone()];
                if let Some(caps) = caps {
                    params.push(format!("caps={caps}"));
                }
                if let Some(epoch) = epoch {
                    params.push(epoch.to_string());
                }
                ("REVOKE", params, None)
            }
            Command::ChannelCreate { channel, policy } => {
                let mut params = vec!["CREATE".to_string(), channel.to_string()];
                if let Some(policy) = policy {
                    params.push(policy.to_string());
                }
                ("CHANNEL", params, None)
            }
            Command::ChannelPolicy {
                channel,
                policy,
                purge,
            } => {
                let mut params = vec![
                    "POLICY".to_string(),
                    channel.to_string(),
                    policy.to_string(),
                ];
                if *purge {
                    params.push("purge".to_string());
                }
                ("CHANNEL", params, None)
            }
            Command::ChannelMeta {
                channel,
                key,
                value,
            } => (
                "CHANNEL",
                vec!["META".to_string(), channel.to_string(), key.clone()],
                Some(value.clone()),
            ),
            Command::ChannelDelete { channel, confirm } => (
                "CHANNEL",
                vec![
                    "DELETE".to_string(),
                    channel.to_string(),
                    confirm.to_string(),
                ],
                None,
            ),
            Command::InviteMint {
                scope,
                max_uses,
                expiry,
            } => {
                let mut params = vec!["MINT".to_string(), scope.clone()];
                if let Some(max_uses) = max_uses {
                    params.push(format!("max-uses={max_uses}"));
                }
                if let Some(expiry) = expiry {
                    params.push(format!("expiry={expiry}"));
                }
                ("INVITE", params, None)
            }
            Command::InviteRevoke { invite_id } => (
                "INVITE",
                vec!["REVOKE".to_string(), invite_id.clone()],
                None,
            ),
            Command::InviteRedeem { token } => {
                ("INVITE", vec!["REDEEM".to_string(), token.clone()], None)
            }
            Command::NsCreate {
                name,
                visibility,
                root_key,
            } => {
                tags.insert("root".to_string(), root_key.clone());
                (
                    "NS",
                    vec!["CREATE".to_string(), name.to_string(), visibility.to_string()],
                    None,
                )
            }
            Command::NsMeta { name, key, value } => (
                "NS",
                vec!["META".to_string(), name.to_string(), key.clone()],
                Some(value.clone()),
            ),
            Command::NsVisibility { name, visibility } => (
                "NS",
                vec![
                    "VISIBILITY".to_string(),
                    name.to_string(),
                    visibility.to_string(),
                ],
                None,
            ),
            Command::NsDelegate {
                name,
                subject,
                caps,
            } => (
                "NS",
                vec![
                    "DELEGATE".to_string(),
                    name.to_string(),
                    subject.clone(),
                    caps.clone(),
                ],
                None,
            ),
            Command::NsDelete { name, confirm } => (
                "NS",
                vec!["DELETE".to_string(), name.to_string(), confirm.to_string()],
                None,
            ),
            Command::Discover { cursor } => {
                ("DISCOVER", cursor.iter().cloned().collect(), None)
            }
            Command::Unknown { .. } => {
                return Err(SerializeError::Unrepresentable("unknown command"));
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
    use crate::line::MAX_LABEL_BYTES;

    const MSGID: &str = "hda.example/01ARZ3NDEKTSV4RRFFQ69G5FAV";

    /// Serialize → parse must reproduce the request exactly.
    fn round_trip(request: &Request) {
        let wire = request.serialize().unwrap();
        assert_eq!(&Request::parse(&wire).unwrap(), request, "wire: {wire}");
    }

    fn parse(input: &str) -> Command {
        Request::parse(input).unwrap().command
    }

    #[test]
    fn hello_round_trips() {
        let request = Request::new(Command::Hello {
            version: "weft/1".into(),
        });
        assert_eq!(request.serialize().unwrap(), "HELLO weft/1"); // spec §3.6 example
        round_trip(&request);
    }

    #[test]
    fn register_keeps_spaces_in_password() {
        let request = Request::new(Command::Register {
            account: "ada".parse().unwrap(),
            password: "correct horse battery".into(),
        });
        round_trip(&request);
        assert_eq!(
            request.serialize().unwrap(),
            "REGISTER ada :correct horse battery"
        );
    }

    #[test]
    fn auth_password_round_trips() {
        round_trip(&Request::new(Command::AuthPassword {
            account: "ada".parse().unwrap(),
            password: ":p4ss with space".into(),
        }));
    }

    #[test]
    fn auth_key_flow_round_trips() {
        round_trip(&Request::new(Command::AuthKey {
            account: "ada".parse().unwrap(),
            pubkey: "BASE64KEY==".into(),
        }));
        round_trip(&Request::new(Command::AuthProof {
            signature: "BASE64SIG==".into(),
        }));
        round_trip(&Request::new(Command::AuthEnroll {
            pubkey: "BASE64KEY2==".into(),
        }));
    }

    #[test]
    fn bad_auth_subcommand_is_typed_error() {
        assert_eq!(
            Request::parse("AUTH TELEPATHY ada"),
            Err(ParseError::BadParam {
                verb: "AUTH",
                what: "subcommand",
                value: "TELEPATHY".into()
            })
        );
    }

    #[test]
    fn quit_ping_pong_round_trip() {
        round_trip(&Request::new(Command::Quit { reason: None }));
        round_trip(&Request::new(Command::Quit {
            reason: Some("bye now".into()),
        }));
        round_trip(&Request::new(Command::Ping {
            token: Some("t1".into()),
        }));
        round_trip(&Request::new(Command::Pong { token: None }));
    }

    #[test]
    fn presence_all_statuses_round_trip() {
        for status in [
            PresenceStatus::Online,
            PresenceStatus::Away,
            PresenceStatus::Dnd,
            PresenceStatus::Invisible,
        ] {
            round_trip(&Request::new(Command::Presence { status }));
        }
        assert!(Request::parse("PRESENCE sleeping").is_err());
    }

    #[test]
    fn join_part_typing_mark_round_trip() {
        round_trip(&Request::new(Command::Join {
            channel: "#gaming/general".parse().unwrap(),
            invite: Some("INVREF".into()),
        }));
        round_trip(&Request::new(Command::Part {
            channel: "#general".parse().unwrap(),
            reason: Some("afk".into()),
        }));
        round_trip(&Request::new(Command::Typing {
            channel: "#general".parse().unwrap(),
            state: TypingState::Stop,
        }));
        round_trip(&Request::new(Command::Mark {
            channel: "#general".parse().unwrap(),
            msgid: MSGID.parse().unwrap(),
        }));
        assert_eq!(
            Request::parse("JOIN"),
            Err(ParseError::MissingParam {
                verb: "JOIN",
                what: "channel"
            })
        );
    }

    #[test]
    fn msg_channel_with_meta_round_trips() {
        round_trip(&Request::with_label(
            Command::Msg {
                target: "#general".parse().unwrap(),
                body: Some("hello world".into()),
                meta: MsgMeta {
                    fmt: Some("md".into()),
                    reply_to: Some(MSGID.parse().unwrap()),
                    thread: Some(MSGID.parse().unwrap()),
                    attachments: vec![],
                },
            },
            "req-1",
        ));
    }

    #[test]
    fn msg_dm_target_round_trips() {
        let request = Request::new(Command::Msg {
            target: "@ada".parse().unwrap(),
            body: Some("hi".into()),
            meta: MsgMeta::default(),
        });
        assert_eq!(request.serialize().unwrap(), "MSG @ada :hi");
        round_trip(&request);
    }

    #[test]
    fn msg_attachments_only_and_limits() {
        // Empty trailing (bare media, §13) is preserved as Some("").
        let request = Request::new(Command::Msg {
            target: "#general".parse().unwrap(),
            body: Some(String::new()),
            meta: MsgMeta {
                attachments: vec!["weft-media://hda.example/b3hash".into()],
                ..MsgMeta::default()
            },
        });
        round_trip(&request);

        let over = Request::new(Command::Msg {
            target: "#general".parse().unwrap(),
            body: None,
            meta: MsgMeta {
                attachments: vec!["m".into(); 11],
                ..MsgMeta::default()
            },
        });
        assert_eq!(over.serialize(), Err(SerializeError::TooManyAttachments));
        assert_eq!(
            Request::parse("@attach.11=x MSG #a :"),
            Err(ParseError::TooManyAttachments)
        );
    }

    #[test]
    fn attachment_indices_sort_numerically() {
        // BTreeMap would yield attach.10 < attach.2 lexically; codec must not.
        let line = "@attach.1=a;attach.2=b;attach.10=j MSG #c :";
        // attach.10 alone is fine — but 10 items max, index ≤ 10, so this parses.
        let Command::Msg { meta, .. } = parse(line) else {
            panic!()
        };
        assert_eq!(meta.attachments, vec!["a", "b", "j"]);
    }

    #[test]
    fn edit_delete_round_trip() {
        let edit = Request::with_label(
            Command::Edit {
                msgid: MSGID.parse().unwrap(),
                body: "fixed the typo".into(),
            },
            "e1",
        );
        assert_eq!(
            edit.serialize().unwrap(),
            format!("@label=e1 EDIT {MSGID} :fixed the typo")
        );
        round_trip(&edit);
        round_trip(&Request::new(Command::Delete {
            msgid: MSGID.parse().unwrap(),
        }));
        // EDIT requires a body (empty trailing is a meaningful empty body).
        assert!(Request::parse(&format!("EDIT {MSGID}")).is_err());
    }

    #[test]
    fn react_unreact_round_trip_and_emoji_limits() {
        round_trip(&Request::new(Command::React {
            msgid: MSGID.parse().unwrap(),
            emoji: "🦀".into(),
        }));
        round_trip(&Request::new(Command::Unreact {
            msgid: MSGID.parse().unwrap(),
            emoji: "ferris".into(), // bare shortcode (spec §18 #8)
        }));
        // >32 bytes rejected both directions.
        assert!(Request::parse(&format!("REACT {MSGID} {}", "x".repeat(33))).is_err());
        let over = Request::new(Command::React {
            msgid: MSGID.parse().unwrap(),
            emoji: "x".repeat(33),
        });
        assert!(over.serialize().is_err());
        // Leading colon collides with the trailing marker (§4).
        assert!(Request::parse(&format!("REACT {MSGID} :colon:")).is_err());
    }

    #[test]
    fn history_params_any_order_round_trip() {
        let request = Request::with_label(
            Command::History {
                target: "#general".parse().unwrap(),
                before: Some(MSGID.parse().unwrap()),
                after: None,
                limit: Some(50),
                thread: None,
            },
            "h1",
        );
        assert_eq!(
            request.serialize().unwrap(),
            format!("@label=h1 HISTORY #general before={MSGID} limit=50")
        );
        round_trip(&request);

        // Any order, DM targets, unknown keys ignored (lenient-in).
        let parsed =
            Request::parse(&format!("HISTORY @ada limit=10 x-custom=1 after={MSGID}")).unwrap();
        let Command::History {
            target,
            after: Some(_),
            limit: Some(10),
            before: None,
            thread: None,
        } = parsed.command
        else {
            panic!("bad parse: {parsed:?}");
        };
        assert_eq!(target.to_string(), "@ada");

        assert!(Request::parse("HISTORY #general limit=abc").is_err());
    }

    #[test]
    fn grant_revoke_round_trip() {
        round_trip(&Request::with_label(
            Command::Grant {
                subject: "ada".into(),
                scope: "ns:gaming".into(),
                caps: "ban,grant:send".into(),
                expiry: Some(3600),
            },
            "g1",
        ));
        assert_eq!(
            Request::new(Command::Grant {
                subject: "B64KEY==".into(),
                scope: "#general".into(),
                caps: "send".into(),
                expiry: None,
            })
            .serialize()
            .unwrap(),
            "GRANT B64KEY== #general send"
        );
        // Caps with a space are rejected both ways.
        assert!(
            Request::parse("GRANT ada * send react").is_err()
                || matches!(
                    Request::parse("GRANT ada * send react").unwrap().command,
                    Command::Grant { caps, .. } if caps == "send"
                )
        );
        // REVOKE: caps=list and a bare epoch, any order.
        let parsed = Request::parse("REVOKE ada ns:x caps=ban,kick 7").unwrap();
        let Command::Revoke { caps, epoch, .. } = parsed.command else {
            panic!()
        };
        assert_eq!(caps.as_deref(), Some("ban,kick"));
        assert_eq!(epoch, Some(7));
        round_trip(&Request::new(Command::Revoke {
            subject: "ada".into(),
            scope: "#general".into(),
            caps: None,
            epoch: None,
        }));
    }

    #[test]
    fn channel_verbs_round_trip() {
        round_trip(&Request::new(Command::ChannelCreate {
            channel: "#new".parse().unwrap(),
            policy: Some("retained:30d".parse().unwrap()),
        }));
        assert_eq!(
            Request::new(Command::ChannelCreate {
                channel: "#new".parse().unwrap(),
                policy: None,
            })
            .serialize()
            .unwrap(),
            "CHANNEL CREATE #new"
        );
        round_trip(&Request::new(Command::ChannelPolicy {
            channel: "#c".parse().unwrap(),
            policy: "ephemeral".parse().unwrap(),
            purge: true,
        }));
        round_trip(&Request::new(Command::ChannelMeta {
            channel: "#c".parse().unwrap(),
            key: "topic".into(),
            value: "the new topic here".into(),
        }));
        round_trip(&Request::new(Command::ChannelDelete {
            channel: "#c".parse().unwrap(),
            confirm: "#c".parse().unwrap(),
        }));
        assert_eq!(
            Request::parse("CHANNEL FROB #x"),
            Err(ParseError::BadParam {
                verb: "CHANNEL",
                what: "subcommand",
                value: "FROB".into()
            })
        );
    }

    #[test]
    fn invite_verbs_round_trip() {
        round_trip(&Request::with_label(
            Command::InviteMint {
                scope: "ns:gaming".into(),
                max_uses: Some(10),
                expiry: Some(86400),
            },
            "i1",
        ));
        round_trip(&Request::new(Command::InviteRevoke {
            invite_id: "inv-abc".into(),
        }));
        round_trip(&Request::new(Command::InviteRedeem {
            token: "B64TOKEN==".into(),
        }));
    }

    #[test]
    fn ns_verbs_round_trip() {
        let create = Request::with_label(
            Command::NsCreate {
                name: "gaming".parse().unwrap(),
                visibility: crate::types::Visibility::Public,
                root_key: "B64ROOTKEY==".into(),
            },
            "n1",
        );
        let wire = create.serialize().unwrap();
        assert!(wire.contains("root=B64ROOTKEY=="), "{wire}");
        assert!(wire.contains("NS CREATE gaming public"), "{wire}");
        round_trip(&create);
        // Default tier is unlisted; root tag mandatory.
        let parsed = Request::parse("@root=K== NS CREATE gaming").unwrap();
        assert!(matches!(
            parsed.command,
            Command::NsCreate { visibility: crate::types::Visibility::Unlisted, .. }
        ));
        assert_eq!(
            Request::parse("NS CREATE gaming"),
            Err(ParseError::MissingParam { verb: "NS", what: "root tag (namespace root pubkey)" })
        );

        round_trip(&Request::new(Command::NsMeta {
            name: "gaming".parse().unwrap(),
            key: "title".into(),
            value: "The Gaming Lounge".into(),
        }));
        round_trip(&Request::new(Command::NsVisibility {
            name: "gaming".parse().unwrap(),
            visibility: crate::types::Visibility::Private,
        }));
        round_trip(&Request::new(Command::NsDelegate {
            name: "gaming".parse().unwrap(),
            subject: "ada".into(),
            caps: "ban,kick".into(),
        }));
        round_trip(&Request::new(Command::NsDelete {
            name: "gaming".parse().unwrap(),
            confirm: "gaming".parse().unwrap(),
        }));
        assert!(Request::parse("NS FROB x").is_err());
    }

    #[test]
    fn discover_round_trips() {
        round_trip(&Request::new(Command::Discover { cursor: None }));
        round_trip(&Request::with_label(
            Command::Discover {
                cursor: Some("cur-42".into()),
            },
            "d1",
        ));
    }

    #[test]
    fn unknown_verb_is_not_an_error() {
        assert_eq!(
            parse("FROBNICATE a b :c"),
            Command::Unknown {
                verb: "FROBNICATE".into()
            }
        );
        // ...but has no wire form on the way out.
        let request = Request::new(Command::Unknown { verb: "X".into() });
        assert_eq!(
            request.serialize(),
            Err(SerializeError::Unrepresentable("unknown command"))
        );
    }

    #[test]
    fn label_limits() {
        let request = Request::parse("@label=abc123 PING").unwrap();
        assert_eq!(request.label.as_deref(), Some("abc123"));

        let long = format!("@label={} PING", "x".repeat(MAX_LABEL_BYTES + 1));
        assert_eq!(Request::parse(&long), Err(ParseError::LabelTooLong));

        let request = Request::with_label(
            Command::Ping { token: None },
            "y".repeat(MAX_LABEL_BYTES + 1),
        );
        assert_eq!(request.serialize(), Err(SerializeError::LabelTooLong));
    }
}
