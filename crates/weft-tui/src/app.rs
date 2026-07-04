//! Client state machine, kept free of terminal I/O so it is unit-testable:
//! events in (keys, network lines), wire lines out (via the outbound
//! queue), log entries + state for the renderer.

use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::text::Line;
use tokio::sync::mpsc;
use weft_proto::{ChannelName, Command, ErrCode, Event, MemberAction, MsgMeta, Reply, Request};

use crate::net::NetEvent;
use crate::ui;

pub enum AppEvent {
    Term(TermEvent),
    Net(NetEvent),
}

/// M2 servers verify credentials; the test client uses one fixed dev
/// password (≥12 B, §6.1) and auto-registers unknown accounts.
const PASSWORD: &str = "weft-tui-dev-password";

/// Connection phase: HELLO and AUTH are driven automatically (§3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Connecting,
    HelloSent,
    AuthSent,
    Ready,
    Dead,
}

pub struct LogEntry {
    /// The wire line (inbound verbatim; outbound prefixed with `→`).
    pub raw: String,
    /// Human rendering for pretty mode.
    pub pretty: Line<'static>,
}

pub struct App {
    pub account: String,
    pub network: Option<String>,
    pub joined: Vec<String>,
    pub current: Option<String>,
    pub input: String,
    pub log: Vec<LogEntry>,
    /// Scrollback offset in lines from the bottom.
    pub scroll: usize,
    /// Raw-wire display mode (Ctrl+R) — the netcat view.
    pub raw_mode: bool,
    pub quit: bool,
    phase: Phase,
    tried_register: bool,
    autojoin: Option<String>,
    labels: u64,
    outbound: mpsc::UnboundedSender<String>,
}

impl App {
    pub fn new(
        account: String,
        autojoin: Option<String>,
        outbound: mpsc::UnboundedSender<String>,
    ) -> Self {
        let mut app = Self {
            account,
            network: None,
            joined: Vec::new(),
            current: None,
            input: String::new(),
            log: Vec::new(),
            scroll: 0,
            raw_mode: false,
            quit: false,
            phase: Phase::Connecting,
            tried_register: false,
            autojoin,
            labels: 0,
            outbound,
        };
        app.note("connecting… (Ctrl+R raw view, /help for commands)");
        app
    }

    pub fn on_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Term(TermEvent::Key(key)) => self.on_key(key),
            AppEvent::Term(_) => {} // resize etc. — redraw happens anyway
            AppEvent::Net(net) => self.on_net(net),
        }
    }

    // ---- keyboard ----

    fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => self.quit(),
            KeyCode::Esc => self.quit(),
            KeyCode::Char('r') if ctrl => self.raw_mode = !self.raw_mode,
            KeyCode::Char(c) if !ctrl => self.input.push(c),
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Tab => self.cycle_channel(),
            KeyCode::PageUp => self.scroll = (self.scroll + 10).min(self.log.len()),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            _ => {}
        }
    }

    fn cycle_channel(&mut self) {
        if self.joined.is_empty() {
            return;
        }
        let next = match &self.current {
            None => 0,
            Some(current) => self
                .joined
                .iter()
                .position(|c| c == current)
                .map_or(0, |i| (i + 1) % self.joined.len()),
        };
        self.current = Some(self.joined[next].clone());
    }

    fn quit(&mut self) {
        // Best effort — main gives the net task a moment to flush this.
        self.send_command(Command::Quit { reason: None });
        self.quit = true;
    }

    // ---- input line ----

    fn submit(&mut self) {
        let text = std::mem::take(&mut self.input);
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        match text.strip_prefix('/') {
            Some(command) => self.command(command),
            None => match self.current.clone() {
                Some(channel) => self.send_msg(&channel, text),
                None => self.note("no current channel — /join one first"),
            },
        }
    }

    fn command(&mut self, command: &str) {
        let (name, args) = command.split_once(' ').unwrap_or((command, ""));
        match name {
            "join" | "j" => match args.parse::<ChannelName>() {
                Ok(channel) => {
                    self.send_command(Command::Join {
                        channel,
                        invite: None,
                    });
                }
                Err(_) => self.note("usage: /join #channel"),
            },
            "part" | "p" => {
                let target = if args.is_empty() {
                    self.current.clone()
                } else {
                    Some(args.to_string())
                };
                match target.as_deref().map(str::parse::<ChannelName>) {
                    Some(Ok(channel)) => {
                        self.send_command(Command::Part {
                            channel,
                            reason: None,
                        });
                    }
                    _ => self.note("usage: /part [#channel]"),
                }
            }
            "msg" | "m" => match args.split_once(' ') {
                Some((target, body)) if !body.is_empty() => {
                    let target = target.to_string();
                    self.send_msg(&target, body);
                }
                _ => self.note("usage: /msg <#channel|@user> <text>"),
            },
            "channel" | "c" => {
                if self.joined.iter().any(|c| c == args) {
                    self.current = Some(args.to_string());
                } else {
                    self.note("not joined to that channel (Tab cycles)");
                }
            }
            // The escape hatch that makes this a *test* client: send any
            // line verbatim, valid or not.
            "raw" => {
                if args.is_empty() {
                    self.note("usage: /raw <wire line>");
                } else {
                    self.send_raw(args.to_string());
                }
            }
            "ping" => {
                self.send_command(Command::Ping {
                    token: Some("tui".to_string()),
                });
            }
            "quit" | "q" => self.quit(),
            "help" | "h" | "?" => {
                for line in [
                    "/join #chan · /part [#chan] · /msg <target> <text> · /channel #chan",
                    "/raw <line> · /ping · /quit — plain text goes to the current channel",
                    "Tab: switch channel · Ctrl+R: raw wire view · PgUp/PgDn: scroll · Esc: quit",
                ] {
                    self.note(line);
                }
            }
            _ => self.note("unknown command (/help)"),
        }
    }

    fn send_msg(&mut self, target: &str, body: &str) {
        match target.parse() {
            Ok(target) => {
                self.send_command(Command::Msg {
                    target,
                    body: Some(body.to_string()),
                    meta: MsgMeta::default(),
                });
            }
            Err(_) => self.note("bad target — use #channel or @user"),
        }
    }

    // ---- outbound ----

    /// Serialize a typed command with a fresh label (§3.5: label everything
    /// so responses correlate — and so silent drops are visible).
    fn send_command(&mut self, command: Command) {
        self.labels += 1;
        let request = Request::with_label(command, format!("t{}", self.labels));
        match request.serialize() {
            Ok(line) => self.send_raw(line),
            Err(e) => self.note(&format!("cannot serialize: {e}")),
        }
    }

    fn send_raw(&mut self, line: String) {
        self.log.push(ui::outbound_entry(&line));
        let _ = self.outbound.send(line);
    }

    // ---- network ----

    fn on_net(&mut self, event: NetEvent) {
        match event {
            NetEvent::Connected => {
                self.note("transport up — negotiating");
                self.send_command(Command::Hello {
                    version: "weft/1".to_string(),
                });
                self.phase = Phase::HelloSent;
            }
            NetEvent::Closed(reason) => {
                self.note(&format!("✕ {reason}"));
                self.phase = Phase::Dead;
            }
            NetEvent::Line(raw) => match Reply::parse(&raw) {
                Ok(reply) => self.on_reply(raw, reply),
                Err(e) => {
                    self.log.push(ui::unparseable_entry(&raw, &e.to_string()));
                }
            },
        }
    }

    fn on_reply(&mut self, raw: String, reply: Reply) {
        // Automatic §3.3 progression: HELLO → WELCOME → AUTH → WELCOME.
        match (&self.phase, &reply.event) {
            (Phase::HelloSent, Event::Welcome { network, .. }) => {
                self.network = Some(network.to_string());
                let account = self.account.clone();
                self.send_command(Command::AuthPassword {
                    account: account.parse().expect("validated in main"),
                    password: PASSWORD.to_string(),
                });
                self.phase = Phase::AuthSent;
            }
            // Unknown account on this server: register it once (dev flow;
            // REGISTER doubles as auth, so WELCOME lands in this same arm).
            (Phase::AuthSent, Event::Err(err))
                if err.code == ErrCode::AuthFailed && !self.tried_register =>
            {
                self.tried_register = true;
                self.note("account unknown here — registering");
                let account = self.account.clone();
                self.send_command(Command::Register {
                    account: account.parse().expect("validated in main"),
                    password: PASSWORD.to_string(),
                });
            }
            (Phase::AuthSent, Event::Welcome { .. }) => {
                self.phase = Phase::Ready;
                self.note("authenticated");
                if let Some(channel) = self.autojoin.take() {
                    self.input = format!("/join {channel}");
                    self.submit();
                }
            }
            _ => {}
        }
        // Track own membership from MEMBER echoes/broadcasts.
        if let Event::Member {
            channel,
            user,
            action,
            ..
        } = &reply.event
        {
            if user.account.as_str() == self.account {
                let name = channel.to_string();
                match action {
                    MemberAction::Join => {
                        if !self.joined.contains(&name) {
                            self.joined.push(name.clone());
                        }
                        self.current = Some(name);
                    }
                    MemberAction::Part => {
                        self.joined.retain(|c| c != &name);
                        if self.current.as_deref() == Some(name.as_str()) {
                            self.current = self.joined.first().cloned();
                        }
                    }
                }
            }
        }
        self.log.push(ui::reply_entry(raw, &reply, &self.account));
    }

    fn note(&mut self, text: &str) {
        self.log.push(ui::note_entry(text));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> AppEvent {
        AppEvent::Term(TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn type_line(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
        app.on_event(key(KeyCode::Enter));
    }

    fn harness() -> (App, mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            App::new("ada".to_string(), Some("#general".to_string()), tx),
            rx,
        )
    }

    fn feed(app: &mut App, line: &str) {
        app.on_event(AppEvent::Net(NetEvent::Line(line.to_string())));
    }

    #[test]
    fn handshake_hello_auth_autojoin() {
        let (mut app, mut out) = harness();
        app.on_event(AppEvent::Net(NetEvent::Connected));
        assert_eq!(out.try_recv().unwrap(), "@label=t1 HELLO weft/1");

        feed(&mut app, "@label=t1 WELCOME test.example :hi");
        assert_eq!(app.network.as_deref(), Some("test.example"));
        assert_eq!(
            out.try_recv().unwrap(),
            format!("@label=t2 AUTH PASSWORD ada :{PASSWORD}")
        );

        feed(&mut app, "@label=t2 WELCOME test.example");
        assert_eq!(out.try_recv().unwrap(), "@label=t3 JOIN #general");
    }

    #[test]
    fn auth_failed_falls_back_to_register_once() {
        let (mut app, mut out) = harness();
        app.on_event(AppEvent::Net(NetEvent::Connected));
        out.try_recv().unwrap(); // HELLO
        feed(&mut app, "WELCOME test.example");
        out.try_recv().unwrap(); // AUTH PASSWORD

        feed(&mut app, "@label=t2 ERR AUTH-FAILED :authentication failed");
        assert_eq!(
            out.try_recv().unwrap(),
            format!("@label=t3 REGISTER ada :{PASSWORD}")
        );
        // A second failure (e.g. CONFLICT-adjacent race) must not loop.
        feed(&mut app, "@label=t3 ERR AUTH-FAILED :authentication failed");
        assert!(out.try_recv().is_err());

        // REGISTER's WELCOME completes the handshake and autojoins.
        feed(&mut app, "WELCOME test.example");
        assert!(out.try_recv().unwrap().ends_with("JOIN #general"));
    }

    #[test]
    fn member_echo_tracks_joined_and_current() {
        let (mut app, _out) = harness();
        feed(
            &mut app,
            "@count=1;label=t3 MEMBER #general ada@test.example join",
        );
        assert_eq!(app.joined, vec!["#general"]);
        assert_eq!(app.current.as_deref(), Some("#general"));

        // Someone else's join must not affect our membership.
        feed(&mut app, "@count=2 MEMBER #general bob@test.example join");
        assert_eq!(app.joined, vec!["#general"]);

        feed(&mut app, "@label=t9 MEMBER #general ada@test.example part");
        assert!(app.joined.is_empty());
        assert_eq!(app.current, None);
    }

    #[test]
    fn plain_text_becomes_msg_to_current_channel() {
        let (mut app, mut out) = harness();
        feed(&mut app, "@count=1 MEMBER #general ada@test.example join");
        type_line(&mut app, "hello there");
        assert_eq!(
            out.try_recv().unwrap(),
            "@label=t1 MSG #general :hello there"
        );
    }

    #[test]
    fn slash_raw_sends_verbatim() {
        let (mut app, mut out) = harness();
        type_line(&mut app, "/raw @label=x;fmt=md MSG #a :spaced  body");
        assert_eq!(
            out.try_recv().unwrap(),
            "@label=x;fmt=md MSG #a :spaced  body"
        );
    }

    #[test]
    fn slash_join_validates_channel() {
        let (mut app, mut out) = harness();
        type_line(&mut app, "/join nope");
        assert!(out.try_recv().is_err(), "invalid channel must not be sent");
        type_line(&mut app, "/join #ok");
        assert_eq!(out.try_recv().unwrap(), "@label=t1 JOIN #ok");
    }

    #[test]
    fn esc_sends_quit_and_flags_exit() {
        let (mut app, mut out) = harness();
        app.on_event(key(KeyCode::Esc));
        assert!(app.quit);
        assert_eq!(out.try_recv().unwrap(), "@label=t1 QUIT");
    }
}
