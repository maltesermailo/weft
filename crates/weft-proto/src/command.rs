//! Client → server commands: the M0 session + relay verb set (§6.1, §6.3,
//! §6.4). Unknown verbs decode to [`Command::Unknown`] — never an error (§4).

use crate::error::{ParseError, SerializeError};
use crate::id::MsgId;
use crate::line::{label_from_tags, write_label, Args, Line, Tags};
use crate::name::{Account, ChannelName, Target};
use crate::types::{MsgMeta, PresenceStatus, TypingState};

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
    /// Any verb outside the known set. Servers ignore it silently (§4).
    Unknown { verb: String },
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
