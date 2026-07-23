//! The IRC ↔ WEFT translation (spec §17), as pure functions over a small
//! per-connection [`St`] so the whole mapping is unit-testable without a
//! socket. `from_irc` turns a client IRC line into WEFT commands (+ any
//! immediate IRC replies); `from_weft` turns a WEFT event line into IRC lines
//! (+ any follow-up WEFT commands, e.g. the AUTH that follows WELCOME).

use std::collections::{HashMap, HashSet};

use weft_proto::{Account, ErrCode, Event, MemberAction, MessageEvent, Reply, Target};

use crate::irc::{self, err, rpl, Message};

/// Effective password when the IRC client sends a short/no `PASS` — a bridged
/// legacy user still needs a WEFT credential (≥12 B, §6.1). Documented
/// tradeoff: this makes nicks self-service on a gateway without SASL.
const DEFAULT_PASSWORD: &str = "weft-irc-gateway-pw";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Collecting NICK/USER (and optional PASS).
    Pre,
    /// HELLO sent, awaiting the negotiate WELCOME.
    HelloSent,
    /// AUTH/REGISTER sent, awaiting the authed WELCOME.
    AuthSent,
    /// Registered — the WEFT session is READY.
    Ready,
}

/// Per-connection translation state.
pub struct St {
    /// Server name used as the prefix of server-originated IRC lines.
    pub server: String,
    nick: Option<String>,
    account: Option<String>,
    user_seen: bool,
    pass: Option<String>,
    stage: Stage,
    tried_register: bool,
    listing: bool,
    /// Per-channel known nicks — best-effort NAMES (WEFT's MEMBER only reports
    /// changes, not the pre-existing roster, so this fills in from joins).
    members: HashMap<String, HashSet<String>>,
}

/// What a translation step produces: WEFT command lines to feed the session,
/// and IRC lines to write straight to the client.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Out {
    pub weft: Vec<String>,
    pub irc: Vec<String>,
}

impl St {
    pub fn new(server: impl Into<String>) -> Self {
        Self {
            server: server.into(),
            nick: None,
            account: None,
            user_seen: false,
            pass: None,
            stage: Stage::Pre,
            tried_register: false,
            listing: false,
            members: HashMap::new(),
        }
    }

    fn nick_or_star(&self) -> &str {
        self.nick.as_deref().unwrap_or("*")
    }

    fn password(&self) -> String {
        match &self.pass {
            Some(p) if p.len() >= 12 => p.clone(),
            _ => DEFAULT_PASSWORD.to_string(),
        }
    }

    fn prefix(&self, nick: &str) -> String {
        format!("{nick}!{nick}@{}", self.server)
    }

    /// `:server <code> <nick> <params…> :<trailing>` — the text is always a
    /// proper trailing.
    fn numeric(&self, code: &str, params: &[&str], trailing: &str) -> String {
        let mut middle = vec![self.nick_or_star()];
        middle.extend_from_slice(params);
        irc::format_msg(Some(&self.server), code, &middle, trailing)
    }

    fn target_to_irc(&self, target: &Target) -> String {
        match target {
            Target::Channel(c) => c.to_string(),
            // A DM addressed to us renders as a PRIVMSG to our own nick.
            Target::User(_) => self.nick_or_star().to_string(),
            // Group DMs have no IRC representation (§17 flattens the model);
            // render as our own nick so a stray line never targets a channel.
            Target::Group(_) => self.nick_or_star().to_string(),
        }
    }
}

/// Translate one inbound IRC line.
pub fn from_irc(msg: &Message, st: &mut St) -> Out {
    let mut out = Out::default();
    match msg.command.as_str() {
        // IRCv3 capability negotiation: we advertise none.
        "CAP" => match msg.arg(0).to_ascii_uppercase().as_str() {
            "LS" | "LIST" => out
                .irc
                .push(irc::format(Some(&st.server), "CAP", &["*", "LS", ""])),
            "REQ" => out.irc.push(irc::format(
                Some(&st.server),
                "CAP",
                &["*", "NAK", msg.arg(1)],
            )),
            _ => {}
        },
        "PASS" if st.stage == Stage::Pre => st.pass = Some(msg.arg(0).to_string()),
        "NICK" => {
            let nick = msg.arg(0);
            if nick.is_empty() {
                out.irc
                    .push(st.numeric(err::NONICKNAMEGIVEN, &[], "no nickname given"));
            } else if let Ok(account) = nick.parse::<Account>() {
                st.nick = Some(account.to_string());
                st.account = Some(account.to_string());
                maybe_register(st, &mut out);
            } else {
                out.irc.push(st.numeric(
                    err::ERRONEUSNICKNAME,
                    &[nick],
                    "erroneous nickname (WEFT accounts are a-z 0-9 - _ .)",
                ));
            }
        }
        "USER" => {
            st.user_seen = true;
            maybe_register(st, &mut out);
        }
        "QUIT" => out.weft.push("QUIT".to_string()),
        "PING" => out.irc.push(irc::format_msg(
            Some(&st.server),
            "PONG",
            &[&st.server],
            msg.arg(0),
        )),
        "PONG" => {}
        other => {
            if st.stage != Stage::Ready {
                out.irc
                    .push(st.numeric(err::NOTREGISTERED, &[], "you have not registered"));
                return out;
            }
            match other {
                "JOIN" => {
                    for chan in msg.arg(0).split(',').filter(|c| !c.is_empty()) {
                        out.weft.push(format!("JOIN {chan}"));
                    }
                }
                "PART" => {
                    for chan in msg.arg(0).split(',').filter(|c| !c.is_empty()) {
                        out.weft.push(format!("PART {chan}"));
                    }
                }
                "PRIVMSG" | "NOTICE" => {
                    let (target, text) = (msg.arg(0), msg.arg(1));
                    if target.is_empty() || text.is_empty() {
                        return out;
                    }
                    // `#chan` stays a channel; a bare nick becomes a WEFT DM.
                    let weft_target = if target.starts_with('#') {
                        target.to_string()
                    } else {
                        format!("@{target}")
                    };
                    out.weft.push(format!("MSG {weft_target} :{text}"));
                }
                "NAMES" => names_reply(st, msg.arg(0), &mut out),
                "LIST" => {
                    st.listing = true;
                    out.weft.push("DISCOVER".to_string());
                }
                // Read-mostly/unsupported verbs (MODE/WHO/TOPIC/…): ignored,
                // like WEFT's own treatment of unknown verbs (§4).
                _ => {}
            }
        }
    }
    out
}

/// Translate one WEFT event line.
pub fn from_weft(reply: &Reply, st: &mut St) -> Out {
    let mut out = Out::default();
    match &reply.event {
        Event::Welcome { network, .. } => match st.stage {
            Stage::HelloSent => {
                st.stage = Stage::AuthSent;
                let account = st.account.clone().unwrap_or_default();
                out.weft
                    .push(format!("AUTH PASSWORD {account} :{}", st.password()));
            }
            Stage::AuthSent => {
                st.stage = Stage::Ready;
                st.server = network.to_string();
                registration_numerics(st, &mut out);
            }
            _ => {}
        },
        // Auth probe failed: auto-register the nick once (mirrors weft-tui).
        Event::Err(e)
            if st.stage == Stage::AuthSent
                && e.code == ErrCode::AuthFailed
                && !st.tried_register =>
        {
            st.tried_register = true;
            let account = st.account.clone().unwrap_or_default();
            out.weft
                .push(format!("REGISTER {account} :{}", st.password()));
        }
        // Any other pre-Ready failure: registration can't complete.
        Event::Err(_) if st.stage == Stage::AuthSent => {
            out.irc.push(st.numeric(
                err::PASSWDMISMATCH,
                &[],
                "WEFT authentication failed (try a different nick, or PASS <your-password>)",
            ));
        }
        Event::Member {
            channel,
            user,
            action,
            ..
        } => {
            let nick = user.account.to_string();
            let chan = channel.to_string();
            let is_me = st.account.as_deref() == Some(nick.as_str());
            match action {
                MemberAction::Join => {
                    st.members
                        .entry(chan.clone())
                        .or_default()
                        .insert(nick.clone());
                    out.irc
                        .push(irc::format(Some(&st.prefix(&nick)), "JOIN", &[&chan]));
                    if is_me {
                        names_reply(st, &chan, &mut out);
                    }
                }
                MemberAction::Part => {
                    if let Some(m) = st.members.get_mut(&chan) {
                        m.remove(&nick);
                    }
                    out.irc
                        .push(irc::format(Some(&st.prefix(&nick)), "PART", &[&chan]));
                }
            }
        }
        Event::Message(m) => message_to_irc(st, m, &mut out),
        // §17 degradations: IRC can't edit/delete/react, so project to text.
        Event::Edited {
            target, user, body, ..
        } => out.irc.push(irc::format_msg(
            Some(&st.prefix(&user.account.to_string())),
            "PRIVMSG",
            &[&st.target_to_irc(target)],
            &format!("* edited: {body}"),
        )),
        Event::Deleted { target, .. } => out.irc.push(irc::format_msg(
            Some(&st.server),
            "NOTICE",
            &[&st.target_to_irc(target)],
            "* a message was deleted",
        )),
        Event::Reaction {
            target, emoji, by, ..
        } => out.irc.push(irc::format_msg(
            Some(&st.prefix(&by.account.to_string())),
            "NOTICE",
            &[&st.target_to_irc(target)],
            &format!("* reacted {emoji}"),
        )),
        // DISCOVER → LIST (§17): each public namespace is a list entry.
        Event::NsMeta { name, title, .. } if st.listing => out.irc.push(st.numeric(
            rpl::LIST,
            &[&format!("#{name}"), "0"],
            title.as_deref().unwrap_or(""),
        )),
        Event::More { .. } if st.listing => {
            st.listing = false;
            out.irc.push(st.numeric(rpl::LISTEND, &[], "End of /LIST"));
        }
        // Ready-state errors → the closest numeric, else a NOTICE.
        Event::Err(e) if st.stage == Stage::Ready => match e.code {
            ErrCode::NoSuchTarget => {
                out.irc
                    .push(st.numeric(err::NOSUCHCHANNEL, &["*"], "no such channel or target"))
            }
            ErrCode::CapRequired => out.irc.push(irc::format_msg(
                Some(&st.server),
                "NOTICE",
                &[st.nick_or_star()],
                "WEFT: you lack the capability for that",
            )),
            _ => out.irc.push(irc::format_msg(
                Some(&st.server),
                "NOTICE",
                &[st.nick_or_star()],
                &format!("WEFT {}: {}", e.code, e.text),
            )),
        },
        _ => {}
    }
    out
}

fn maybe_register(st: &mut St, out: &mut Out) {
    if st.stage == Stage::Pre && st.account.is_some() && st.user_seen {
        st.stage = Stage::HelloSent;
        out.weft.push("HELLO weft/1".to_string());
    }
}

fn message_to_irc(st: &St, m: &MessageEvent, out: &mut Out) {
    let sender = m.sender.account.to_string();
    // Suppress our own echo — IRC clients render sent lines locally.
    if st.account.as_deref() == Some(sender.as_str()) || m.body.is_empty() {
        return;
    }
    out.irc.push(irc::format_msg(
        Some(&st.prefix(&sender)),
        "PRIVMSG",
        &[&st.target_to_irc(&m.target)],
        &m.body,
    ));
}

fn names_reply(st: &St, chan: &str, out: &mut Out) {
    let names: Vec<String> = st
        .members
        .get(chan)
        .map(|m| m.iter().cloned().collect())
        .unwrap_or_default();
    out.irc
        .push(st.numeric(rpl::NAMREPLY, &["=", chan], &names.join(" ")));
    out.irc
        .push(st.numeric(rpl::ENDOFNAMES, &[chan], "End of /NAMES list."));
}

fn registration_numerics(st: &St, out: &mut Out) {
    let nick = st.nick_or_star();
    let host = format!("{nick}!{nick}@{}", st.server);
    for line in [
        st.numeric(rpl::WELCOME, &[], &format!("Welcome to the WEFT-IRC gateway {host}")),
        st.numeric(rpl::YOURHOST, &[], &format!("Your host is {}, a WEFT-IRC gateway", st.server)),
        st.numeric(rpl::CREATED, &[], "This gateway projects a WEFT network onto IRC"),
        irc::format(Some(&st.server), rpl::MYINFO, &[nick, &st.server, "weft-irc", "o", "o"]),
        st.numeric(
            rpl::ISUPPORT,
            &[&format!("NETWORK={}", st.server), "CHANTYPES=#", "CASEMAPPING=ascii"],
            "are supported by this server",
        ),
        st.numeric(rpl::MOTDSTART, &[], &format!("- {} Message of the day -", st.server)),
        st.numeric(rpl::MOTD, &[], "- Bridged to a WEFT network. Edits/reactions render as text; e2ee channels are invisible."),
        st.numeric(rpl::ENDOFMOTD, &[], "End of /MOTD command."),
    ] {
        out.irc.push(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn irc(line: &str) -> Message {
        crate::irc::parse(line).unwrap()
    }

    fn weft(line: &str) -> Reply {
        Reply::parse(line).unwrap()
    }

    #[test]
    fn registration_handshake_maps_to_hello_auth_then_numerics() {
        let mut st = St::new("weft.example");
        // NICK alone doesn't register; USER completes it → HELLO.
        assert!(from_irc(&irc("NICK ada"), &mut st).weft.is_empty());
        let out = from_irc(&irc("USER ada 0 * :Ada L"), &mut st);
        assert_eq!(out.weft, vec!["HELLO weft/1"]);

        // WELCOME → AUTH PASSWORD.
        let out = from_weft(&weft("WELCOME weft.example :hi"), &mut st);
        assert_eq!(out.weft, vec!["AUTH PASSWORD ada :weft-irc-gateway-pw"]);
        // Unknown account → REGISTER once, silently.
        let out = from_weft(&weft("ERR AUTH-FAILED :nope"), &mut st);
        assert_eq!(out.weft, vec!["REGISTER ada :weft-irc-gateway-pw"]);
        assert!(out.irc.is_empty());
        // Authed WELCOME → the 001..005 + MOTD burst.
        let out = from_weft(&weft("WELCOME weft.example"), &mut st);
        assert!(out.irc[0].contains(" 001 ada :Welcome"), "{:?}", out.irc);
        assert!(out.irc.iter().any(|l| l.contains(" 376 ")));
    }

    #[test]
    fn pass_supplies_the_weft_password() {
        let mut st = St::new("weft.example");
        from_irc(&irc("PASS correct-horse-battery"), &mut st);
        from_irc(&irc("NICK bob"), &mut st);
        from_irc(&irc("USER bob 0 * :Bob"), &mut st);
        let out = from_weft(&weft("WELCOME weft.example"), &mut st);
        assert_eq!(out.weft, vec!["AUTH PASSWORD bob :correct-horse-battery"]);
    }

    #[test]
    fn pre_registration_verbs_are_refused() {
        let mut st = St::new("weft.example");
        from_irc(&irc("NICK ada"), &mut st);
        from_irc(&irc("USER ada 0 * :Ada"), &mut st);
        // Still HelloSent (not Ready) — JOIN is premature.
        let out = from_irc(&irc("JOIN #general"), &mut st);
        assert!(out.irc[0].contains(" 451 "), "{:?}", out.irc);
        assert!(out.weft.is_empty());
    }

    /// Drive a state to READY for the post-registration tests.
    fn ready(server: &str) -> St {
        let mut st = St::new(server);
        from_irc(&irc("NICK ada"), &mut st);
        from_irc(&irc("USER ada 0 * :Ada"), &mut st);
        from_weft(&weft(&format!("WELCOME {server}")), &mut st);
        from_weft(&weft(&format!("WELCOME {server}")), &mut st);
        st
    }

    #[test]
    fn join_namespaced_channel_passes_through() {
        let mut st = ready("weft.example");
        // A namespaced channel is a normal IRC JOIN target (the `/` is legal).
        let out = from_irc(&irc("JOIN #gaming/general"), &mut st);
        assert_eq!(out.weft, vec!["JOIN #gaming/general"]);
        // The WEFT MEMBER echo becomes an IRC JOIN + NAMES.
        let out = from_weft(
            &weft("@count=1 MEMBER #gaming/general ada@weft.example join"),
            &mut st,
        );
        assert!(out.irc[0].contains("JOIN #gaming/general"), "{:?}", out.irc);
        assert!(out
            .irc
            .iter()
            .any(|l| l.contains(" 353 ") && l.contains("ada")));
        assert!(out.irc.iter().any(|l| l.contains(" 366 ")));
    }

    #[test]
    fn privmsg_maps_both_ways_and_suppresses_own_echo() {
        let mut st = ready("weft.example");
        // Outbound channel message.
        let out = from_irc(&irc("PRIVMSG #general :hello all"), &mut st);
        assert_eq!(out.weft, vec!["MSG #general :hello all"]);
        // A bare nick target becomes a WEFT DM.
        let out = from_irc(&irc("PRIVMSG bob :hi"), &mut st);
        assert_eq!(out.weft, vec!["MSG @bob :hi"]);
        // Our own echo is suppressed; someone else's is delivered.
        let mine = from_weft(
            &weft("@msgid=weft.example/01ARZ3NDEKTSV4RRFFQ69G5FAV MESSAGE #general ada@weft.example :hello all"),
            &mut st,
        );
        assert!(mine.irc.is_empty(), "own echo must not be re-shown");
        let theirs = from_weft(
            &weft("@msgid=weft.example/01ARZ3NDEKTSV4RRFFQ69G5FB0 MESSAGE #general bob@weft.example :hey"),
            &mut st,
        );
        assert_eq!(
            theirs.irc,
            vec![":bob!bob@weft.example PRIVMSG #general :hey"]
        );
    }

    #[test]
    fn edit_and_delete_degrade_to_text() {
        let mut st = ready("weft.example");
        let edited = from_weft(
            &weft("@msgid=weft.example/01ARZ3NDEKTSV4RRFFQ69G5FB0;edit-of=weft.example/01ARZ3NDEKTSV4RRFFQ69G5FAV EDITED #general bob@weft.example :fixed"),
            &mut st,
        );
        assert_eq!(
            edited.irc,
            vec![":bob!bob@weft.example PRIVMSG #general :* edited: fixed"]
        );
        let deleted = from_weft(
            &weft("DELETED #general weft.example/01ARZ3NDEKTSV4RRFFQ69G5FAV"),
            &mut st,
        );
        assert!(deleted.irc[0].contains("NOTICE #general :* a message was deleted"));
    }

    #[test]
    fn ping_is_answered_at_irc_level() {
        let mut st = ready("weft.example");
        let out = from_irc(&irc("PING :tok123"), &mut st);
        assert_eq!(out.weft, Vec::<String>::new());
        assert_eq!(out.irc, vec![":weft.example PONG weft.example :tok123"]);
    }

    #[test]
    fn list_maps_to_discover() {
        let mut st = ready("weft.example");
        let out = from_irc(&irc("LIST"), &mut st);
        assert_eq!(out.weft, vec!["DISCOVER"]);
        let entry = from_weft(&weft("@title=The\\sLounge NS-META gaming public"), &mut st);
        assert!(
            entry.irc[0].contains(" 322 ada #gaming 0 :The Lounge"),
            "{:?}",
            entry.irc
        );
        let end = from_weft(&weft("MORE cursor-1"), &mut st);
        assert!(end.irc[0].contains(" 323 "));
    }
}
