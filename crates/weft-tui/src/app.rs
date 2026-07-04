//! Client state machine, kept free of terminal I/O so it is unit-testable:
//! events in (keys, network lines), wire lines out (via the outbound
//! queue), log entries + state for the renderer.

use crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use ratatui::text::Line;
use tokio::sync::mpsc;
use weft_proto::{
    ChannelName, Command, ErrCode, Event, MemberAction, MsgId, MsgMeta, Reply, Request, Target,
};

use crate::net::NetEvent;
use crate::ui;

pub enum AppEvent {
    Term(TermEvent),
    Net(NetEvent),
}

/// M2 servers verify credentials; the test client uses one fixed dev
/// password (≥12 B, §6.1) and auto-registers unknown accounts.
const PASSWORD: &str = "weft-tui-dev-password";

/// The reaction picker's palette (keys 1–9). Anything else: `/react`.
pub const QUICK_REACTIONS: [&str; 9] = ["👍", "❤️", "😂", "🎉", "🤔", "👀", "🔥", "🦀", "😢"];

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
    /// Set for MESSAGE entries: what selection-mode actions act on.
    pub message: Option<MsgRef>,
}

#[derive(Debug)]
struct Backfill {
    label: String,
    insert_at: usize,
}

/// Everything the selection hotkeys need about a rendered message.
#[derive(Debug, Clone)]
pub struct MsgRef {
    pub msgid: MsgId,
    /// `#channel` or `@user` the message lives in.
    pub target: String,
    pub body: String,
    /// Mine → editable/deletable.
    pub own: bool,
}

pub struct App {
    pub account: String,
    /// Credentials for AUTH/REGISTER. Defaults to the dev password; a
    /// positional arg or `/login` overrides it.
    password: String,
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
    /// DM conversations seen this session (Tab cycles these too).
    pub dm_peers: Vec<String>,
    /// My newest MESSAGE echo — what `/edit` and `/delete` act on.
    last_sent: Option<MsgId>,
    /// Newest MESSAGE seen from anyone — what `/react` acts on.
    last_seen: Option<MsgId>,
    /// Selection-mode cursor: an index into `log` (message entries only).
    pub selected: Option<usize>,
    /// Emoji picker target: a `log` index; digits 1–9 react to it.
    pub picker: Option<usize>,
    /// Tombstoned msgids — excluded from ↑-edit and the picker.
    deleted: std::collections::HashSet<String>,
    /// An in-flight scroll-up backfill: entries with this label are
    /// spliced in at `insert_at` (the top) instead of appended.
    backfill: Option<Backfill>,
    /// Log pane height, stashed by the renderer so selection can keep
    /// itself scrolled into view.
    pub viewport: usize,
    phase: Phase,
    tried_register: bool,
    autojoin: Option<String>,
    labels: u64,
    outbound: mpsc::UnboundedSender<String>,
}

impl App {
    pub fn new(
        account: String,
        password: Option<String>,
        autojoin: Option<String>,
        outbound: mpsc::UnboundedSender<String>,
    ) -> Self {
        let mut app = Self {
            account,
            password: password.unwrap_or_else(|| PASSWORD.to_string()),
            network: None,
            joined: Vec::new(),
            current: None,
            input: String::new(),
            log: Vec::new(),
            scroll: 0,
            raw_mode: false,
            quit: false,
            dm_peers: Vec::new(),
            last_sent: None,
            last_seen: None,
            selected: None,
            picker: None,
            deleted: std::collections::HashSet::new(),
            backfill: None,
            viewport: 20,
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
            AppEvent::Term(TermEvent::Mouse(mouse)) => self.on_mouse(mouse),
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
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // Emoji picker: digits 1–9 react to the target, Esc backs out.
        if let Some(index) = self.picker {
            match key.code {
                KeyCode::Char(c @ '1'..='9') if !ctrl => {
                    let emoji = QUICK_REACTIONS[c as usize - '1' as usize];
                    if let Some(msg) = self.selected_message(index) {
                        self.send_command(Command::React {
                            msgid: msg.msgid,
                            emoji: emoji.to_string(),
                        });
                    }
                    self.picker = None;
                    self.selected = None;
                }
                _ => self.picker = None, // Esc or anything else: cancel
            }
            return;
        }
        // Selection mode (Discord-style): Up/Down walk messages, then
        // e / d / r / m act on the highlighted one.
        if let Some(index) = self.selected {
            match key.code {
                KeyCode::Up => return self.move_selection(index, true),
                KeyCode::Down => return self.move_selection(index, false),
                KeyCode::Esc => {
                    self.selected = None;
                    return;
                }
                KeyCode::Char('e') if !ctrl => return self.select_edit(index),
                KeyCode::Char('d') if !ctrl => return self.select_delete(index),
                KeyCode::Char('r' | '+') if !ctrl => {
                    self.picker = Some(index);
                    return;
                }
                KeyCode::Char('e') if ctrl => {
                    self.picker = Some(index);
                    return;
                }
                KeyCode::Char('m') if !ctrl => return self.select_mark(index),
                // Anything else drops back to typing.
                _ => self.selected = None,
            }
        }
        match key.code {
            KeyCode::Char('c') if ctrl => self.quit(),
            KeyCode::Esc => self.quit(),
            KeyCode::Char('r') if ctrl => self.raw_mode = !self.raw_mode,
            // Ctrl+E: the "add a smiley" button — picker on the newest message.
            KeyCode::Char('e') if ctrl => {
                self.picker = self
                    .log
                    .iter()
                    .rposition(|e| e.message.as_ref().is_some_and(|m| self.live(m)));
            }
            // ↑ on empty input = edit your last message (Discord). Falls
            // back to selection mode when you haven't sent anything.
            KeyCode::Up if self.input.is_empty() && (alt || ctrl) => self.enter_selection(),
            KeyCode::Up if self.input.is_empty() => self.edit_last_or_select(),
            KeyCode::Char(c) if !ctrl => self.input.push(c),
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Tab => self.cycle_channel(),
            KeyCode::PageUp => {
                self.scroll = (self.scroll + 10).min(self.log.len());
                self.maybe_backfill();
            }
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            _ => {}
        }
    }

    /// Wheel scrolling; reaching the top pages older history in, exactly
    /// like PageUp.
    fn on_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll = (self.scroll + 3).min(self.log.len());
                self.maybe_backfill();
            }
            MouseEventKind::ScrollDown => self.scroll = self.scroll.saturating_sub(3),
            _ => {}
        }
    }

    /// Not tombstoned (edits/reactions on deleted messages just bounce
    /// off NO-SUCH-TARGET, so don't offer them).
    fn live(&self, msg: &MsgRef) -> bool {
        !self.deleted.contains(&msg.msgid.to_string())
    }

    /// Scrolled to the top of what we have? Page older history in
    /// (Discord-style infinite scroll), anchored before the oldest
    /// message we know for the current conversation.
    fn maybe_backfill(&mut self) {
        let at_top = self.scroll + self.viewport >= self.log.len();
        if !at_top || self.backfill.is_some() {
            return;
        }
        let Some(current) = self.current.clone() else {
            return;
        };
        let Ok(target) = current.parse::<Target>() else {
            return;
        };
        let before = self
            .log
            .iter()
            .find_map(|e| e.message.as_ref().filter(|m| m.target == current))
            .map(|m| m.msgid.clone());
        let label = self.send_command(Command::History {
            target,
            before,
            after: None,
            limit: Some(20),
            thread: None,
        });
        self.backfill = Some(Backfill {
            label,
            insert_at: 0,
        });
    }

    fn edit_last_or_select(&mut self) {
        let last_own = self
            .log
            .iter()
            .rev()
            .filter_map(|e| e.message.as_ref())
            .find(|m| m.own && self.live(m));
        match last_own {
            Some(msg) => self.input = format!("/edit {} {}", msg.msgid, msg.body),
            None => self.enter_selection(),
        }
    }

    // ---- selection mode ----

    fn enter_selection(&mut self) {
        // Start at the newest message entry.
        self.selected = self.log.iter().rposition(|entry| entry.message.is_some());
        self.scroll_selection_into_view();
    }

    fn move_selection(&mut self, from: usize, older: bool) {
        let next = if older {
            self.log[..from]
                .iter()
                .rposition(|entry| entry.message.is_some())
        } else {
            self.log[from + 1..]
                .iter()
                .position(|entry| entry.message.is_some())
                .map(|offset| from + 1 + offset)
        };
        if let Some(next) = next {
            self.selected = Some(next);
            self.scroll_selection_into_view();
        } else if !older {
            // Walking past the newest message leaves selection mode.
            self.selected = None;
        }
    }

    fn scroll_selection_into_view(&mut self) {
        let Some(index) = self.selected else { return };
        let len = self.log.len();
        let end = len.saturating_sub(self.scroll); // exclusive
        let start = end.saturating_sub(self.viewport);
        if index >= end {
            self.scroll = len - index - 1;
        } else if index < start {
            self.scroll = len.saturating_sub(index + self.viewport);
        }
    }

    fn selected_message(&self, index: usize) -> Option<MsgRef> {
        self.log.get(index).and_then(|entry| entry.message.clone())
    }

    fn select_edit(&mut self, index: usize) {
        let Some(msg) = self.selected_message(index) else {
            return;
        };
        if !msg.own {
            return self.note("not your message (edit-own, §6.4)");
        }
        // Prefill the input with the current body, Discord-style.
        self.input = format!("/edit {} {}", msg.msgid, msg.body);
        self.selected = None;
    }

    fn select_delete(&mut self, index: usize) {
        let Some(msg) = self.selected_message(index) else {
            return;
        };
        if !msg.own {
            return self.note("not your message (delete-own, §6.4)");
        }
        self.send_command(Command::Delete { msgid: msg.msgid });
        self.selected = None;
    }

    fn select_mark(&mut self, index: usize) {
        let Some(msg) = self.selected_message(index) else {
            return;
        };
        match msg.target.parse::<ChannelName>() {
            Ok(channel) => {
                self.send_command(Command::Mark {
                    channel,
                    msgid: msg.msgid,
                });
            }
            Err(_) => self.note("MARK is channel-only (§6.3)"),
        }
        self.selected = None;
    }

    fn cycle_channel(&mut self) {
        let targets: Vec<&String> = self.joined.iter().chain(self.dm_peers.iter()).collect();
        if targets.is_empty() {
            return;
        }
        let next = match &self.current {
            None => 0,
            Some(current) => targets
                .iter()
                .position(|c| *c == current)
                .map_or(0, |i| (i + 1) % targets.len()),
        };
        self.current = Some(targets[next].clone());
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
                if self.joined.iter().any(|c| c == args)
                    || self.dm_peers.iter().any(|c| c == args)
                {
                    self.current = Some(args.to_string());
                } else {
                    self.note("no such conversation (Tab cycles)");
                }
            }
            // M3 mutations. Explicit-msgid forms exist for testing; the
            // no-msgid forms act on tracked messages (see /help).
            "edit" | "e" => match self.msgid_and_rest(args, self.last_sent.clone()) {
                Some((msgid, text)) if !text.is_empty() => {
                    self.send_command(Command::Edit {
                        msgid,
                        body: text.to_string(),
                    });
                }
                _ => self.note("usage: /edit [msgid] <new text> — edits your last message"),
            },
            "delete" | "del" => match self.msgid_and_rest(args, self.last_sent.clone()) {
                Some((msgid, "")) => {
                    if self.last_sent.as_ref() == Some(&msgid) {
                        self.last_sent = None;
                    }
                    self.send_command(Command::Delete { msgid });
                }
                _ => self.note("usage: /delete [msgid] — deletes your last message"),
            },
            "react" | "r" => self.react(args, true),
            "unreact" => self.react(args, false),
            "history" | "hist" => {
                let (target, limit) = parse_history_args(args, self.current.as_deref());
                match target {
                    Some(target) => match target.parse::<Target>() {
                        Ok(target) => {
                            self.send_command(Command::History {
                                target,
                                before: None,
                                after: None,
                                limit: Some(limit),
                                thread: None,
                            });
                        }
                        Err(_) => self.note("usage: /history [#chan|@user] [limit]"),
                    },
                    None => self.note("no current conversation — /join or /msg first"),
                }
            }
            "mark" => {
                let (Some(current), Some(msgid)) = (self.current.clone(), self.last_seen.clone())
                else {
                    return self.note("nothing to mark yet");
                };
                match current.parse::<ChannelName>() {
                    Ok(channel) => {
                        self.send_command(Command::Mark { channel, msgid });
                    }
                    Err(_) => self.note("MARK is channel-only (§6.3)"),
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
            "select" => self.enter_selection(),
            // Re-authenticate with explicit credentials (e.g. an account
            // that predates this client's dev password).
            "login" => match args.split_once(' ') {
                Some((account, password)) if !password.is_empty() => {
                    match account.parse::<weft_proto::Account>() {
                        Ok(parsed) => {
                            self.account = parsed.to_string();
                            self.password = password.to_string();
                            self.tried_register = true; // explicit creds: no auto-register
                            self.phase = Phase::AuthSent;
                            self.send_command(Command::AuthPassword {
                                account: parsed,
                                password: password.to_string(),
                            });
                        }
                        Err(_) => self.note("invalid account name"),
                    }
                }
                _ => self.note("usage: /login <account> <password>"),
            },
            "status" | "s" => match args.parse::<weft_proto::PresenceStatus>() {
                Ok(status) => {
                    self.send_command(Command::Presence { status });
                }
                Err(_) => self.note("usage: /status <online|away|dnd|invisible>"),
            },
            "quit" | "q" => self.quit(),
            "help" | "h" | "?" => {
                for line in [
                    "/join #chan · /part [#chan] · /msg <#chan|@user> <text> · /channel <target>",
                    "/edit [msgid] <text> (your last msg) · /delete [msgid] · /react [msgid] <emoji> · /unreact",
                    "/history [target] [limit] · /mark · /status <online|away|dnd|invisible> · /raw · /ping · /quit",
                    "↑: edit your last message · Ctrl+E: react picker (1-9) · Alt+↑ or /select: browse messages",
                    "while browsing — e:edit d:delete r/+:react picker m:mark Esc:back",
                    "Tab: cycle channels+DMs · Ctrl+R: raw wire · wheel/PgUp: scroll+load older · /login · Esc: quit",
                ] {
                    self.note(line);
                }
            }
            _ => self.note("unknown command (/help)"),
        }
    }

    /// `[msgid] rest…` — explicit msgid if the first token parses as one,
    /// otherwise the fallback (tracked last-sent/last-seen).
    fn msgid_and_rest<'a>(
        &self,
        args: &'a str,
        fallback: Option<MsgId>,
    ) -> Option<(MsgId, &'a str)> {
        let (first, rest) = args.split_once(' ').unwrap_or((args, ""));
        if let Ok(msgid) = first.parse::<MsgId>() {
            return Some((msgid, rest.trim()));
        }
        fallback.map(|msgid| (msgid, args.trim()))
    }

    fn react(&mut self, args: &str, add: bool) {
        match self.msgid_and_rest(args, self.last_seen.clone()) {
            Some((msgid, emoji)) if !emoji.is_empty() && !emoji.contains(' ') => {
                let emoji = emoji.to_string();
                self.send_command(if add {
                    Command::React { msgid, emoji }
                } else {
                    Command::Unreact { msgid, emoji }
                });
            }
            _ => self.note("usage: /react [msgid] <emoji> — reacts to the last message seen"),
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
    fn send_command(&mut self, command: Command) -> String {
        self.labels += 1;
        let label = format!("t{}", self.labels);
        let request = Request::with_label(command, label.clone());
        match request.serialize() {
            Ok(line) => self.send_raw(line),
            Err(e) => self.note(&format!("cannot serialize: {e}")),
        }
        label
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
                    password: self.password.clone(),
                });
                self.phase = Phase::AuthSent;
            }
            // Unknown account on this server: register it once (dev flow;
            // REGISTER doubles as auth, so WELCOME lands in this same arm).
            // The triggering AUTH-FAILED is an internal probe step, not a
            // real error — swallow it (no scary red line) and note the
            // registration instead.
            (Phase::AuthSent, Event::Err(err))
                if err.code == ErrCode::AuthFailed && !self.tried_register =>
            {
                self.tried_register = true;
                self.note(&format!("new account — registering '{}'", self.account));
                let account = self.account.clone();
                self.send_command(Command::Register {
                    account: account.parse().expect("validated in main"),
                    password: self.password.clone(),
                });
                return; // don't log the probe's AUTH-FAILED
            }
            // Auth failed AND the register fallback hit a taken name: the
            // account exists with a different password than the one this
            // client started with. Prefill /login so the right password is
            // one keystroke sequence away (the observed "can't login").
            (Phase::AuthSent, Event::Err(err)) if err.code == ErrCode::Conflict => {
                let account = self.account.clone();
                self.note(&format!(
                    "'{account}' exists with a different password — type it and press Enter"
                ));
                self.input = format!("/login {account} ");
            }
            // Explicit /login with a wrong password: offer another try.
            (Phase::AuthSent, Event::Err(err))
                if err.code == ErrCode::AuthFailed && self.tried_register =>
            {
                let account = self.account.clone();
                self.note(&format!("wrong password for '{account}' — try again"));
                self.input = format!("/login {account} ");
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
                        self.current = Some(name.clone());
                        // A real client backfills on join (§9.7); doing it
                        // here also exercises HISTORY/BATCH constantly.
                        if let Ok(target) = name.parse::<Target>() {
                            self.send_command(Command::History {
                                target,
                                before: None,
                                after: None,
                                limit: Some(20),
                                thread: None,
                            });
                        }
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
        if let Event::Deleted { msgid, .. } = &reply.event {
            self.deleted.insert(msgid.to_string());
        }
        // Scroll-up backfill: splice this batch in at the top so history
        // reads continuously; old messages must not clobber the
        // newest-message tracking below.
        let backfilling = self
            .backfill
            .as_ref()
            .is_some_and(|b| reply.label.as_deref() == Some(b.label.as_str()));
        if backfilling {
            let done = matches!(reply.event, Event::BatchEnd { .. });
            let entry = ui::reply_entry(raw, &reply, &self.account);
            let backfill = self.backfill.as_mut().expect("checked above");
            let at = backfill.insert_at;
            self.log.insert(at, entry);
            backfill.insert_at += 1;
            // Everything below the splice point shifts down one.
            if let Some(selected) = &mut self.selected {
                if *selected >= at {
                    *selected += 1;
                }
            }
            if let Some(picker) = &mut self.picker {
                if *picker >= at {
                    *picker += 1;
                }
            }
            if done {
                self.backfill = None;
            }
            return;
        }
        // Track msgids so /edit, /delete, /react and /mark work without
        // typing 40-char ids, and surface DM conversations for Tab.
        if let Event::Message(msg) = &reply.event {
            self.last_seen = Some(msg.msgid.clone());
            if msg.sender.account.as_str() == self.account {
                self.last_sent = Some(msg.msgid.clone());
            }
            if let Target::User(_) = &msg.target {
                let peer = if msg.sender.account.as_str() == self.account {
                    msg.target.to_string()
                } else {
                    format!("@{}", msg.sender.account)
                };
                if !self.dm_peers.contains(&peer) {
                    self.dm_peers.push(peer.clone());
                }
                if self.current.is_none() {
                    self.current = Some(peer);
                }
            }
        }
        self.log.push(ui::reply_entry(raw, &reply, &self.account));
    }

    fn note(&mut self, text: &str) {
        self.log.push(ui::note_entry(text));
    }
}

/// `/history [target] [limit]` in either order-lite form: a bare number is
/// the limit, anything else is the target.
fn parse_history_args(args: &str, current: Option<&str>) -> (Option<String>, u32) {
    let mut target = current.map(str::to_string);
    let mut limit = 20;
    for token in args.split_whitespace() {
        if let Ok(n) = token.parse::<u32>() {
            limit = n;
        } else {
            target = Some(token.to_string());
        }
    }
    (target, limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> AppEvent {
        AppEvent::Term(TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn key_mod(code: KeyCode, modifiers: KeyModifiers) -> AppEvent {
        AppEvent::Term(TermEvent::Key(KeyEvent::new(code, modifiers)))
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
            App::new("ada".to_string(), None, Some("#general".to_string()), tx),
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
        // The probe's AUTH-FAILED is swallowed — no scary red line logged.
        assert!(
            !app.log.iter().any(|e| e.raw.contains("AUTH-FAILED")),
            "the auto-register probe error must not surface"
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
        // Joining auto-fetches history (§9.7)...
        assert_eq!(
            out.try_recv().unwrap(),
            "@label=t1 HISTORY #general limit=20"
        );
        // ...then plain text goes to the channel.
        type_line(&mut app, "hello there");
        assert_eq!(
            out.try_recv().unwrap(),
            "@label=t2 MSG #general :hello there"
        );
    }

    const MSGID: &str = "test.example/01ARZ3NDEKTSV4RRFFQ69G5FAV";

    /// Prime the app with a joined channel, draining the auto-history line.
    fn joined_app() -> (App, mpsc::UnboundedReceiver<String>) {
        let (mut app, mut out) = harness();
        feed(&mut app, "@count=1 MEMBER #general ada@test.example join");
        out.try_recv().unwrap(); // HISTORY
        (app, out)
    }

    #[test]
    fn edit_delete_act_on_last_sent_message() {
        let (mut app, mut out) = joined_app();
        // Own echo tracks last_sent.
        feed(
            &mut app,
            &format!("@msgid={MSGID} MESSAGE #general ada@test.example :typo"),
        );
        type_line(&mut app, "/edit fixed it");
        assert_eq!(
            out.try_recv().unwrap(),
            format!("@label=t2 EDIT {MSGID} :fixed it")
        );
        type_line(&mut app, "/delete");
        assert_eq!(out.try_recv().unwrap(), format!("@label=t3 DELETE {MSGID}"));
        // last_sent consumed by delete: another /edit refuses politely.
        type_line(&mut app, "/edit nothing left");
        assert!(out.try_recv().is_err());
    }

    #[test]
    fn react_targets_last_seen_and_explicit_msgids_win() {
        let (mut app, mut out) = joined_app();
        // Someone else's message becomes last_seen (but not last_sent).
        feed(
            &mut app,
            &format!("@msgid={MSGID} MESSAGE #general bob@test.example :react to me"),
        );
        type_line(&mut app, "/react 👍");
        assert_eq!(
            out.try_recv().unwrap(),
            format!("@label=t2 REACT {MSGID} 👍")
        );
        type_line(&mut app, &format!("/unreact {MSGID} 👍"));
        assert_eq!(
            out.try_recv().unwrap(),
            format!("@label=t3 UNREACT {MSGID} 👍")
        );
        // Bob's message must not be editable via /edit (no last_sent).
        type_line(&mut app, "/edit hijack");
        assert!(out.try_recv().is_err());
    }

    #[test]
    fn history_and_mark_commands() {
        let (mut app, mut out) = joined_app();
        type_line(&mut app, "/history 5");
        assert_eq!(
            out.try_recv().unwrap(),
            "@label=t2 HISTORY #general limit=5"
        );
        type_line(&mut app, "/history @bob");
        assert_eq!(out.try_recv().unwrap(), "@label=t3 HISTORY @bob limit=20");

        feed(
            &mut app,
            &format!("@msgid={MSGID} MESSAGE #general bob@test.example :newest"),
        );
        type_line(&mut app, "/mark");
        assert_eq!(
            out.try_recv().unwrap(),
            format!("@label=t4 MARK #general {MSGID}")
        );
    }

    #[test]
    fn selection_mode_edit_delete_react_mark() {
        let (mut app, mut out) = joined_app();
        feed(
            &mut app,
            &format!("@msgid={MSGID} MESSAGE #general ada@test.example :my words"),
        );
        feed(&mut app, "@msgid=test.example/01ARZ3NDEKTSV4RRFFQ69G5FB0 MESSAGE #general bob@test.example :bob's words");

        // Alt+Up browses messages, starting at the NEWEST: bob's.
        app.on_event(key_mod(KeyCode::Up, KeyModifiers::ALT));
        let selected = app.selected.expect("selection starts at newest");
        assert!(app.log[selected].raw.contains("bob"));
        // 'e' on someone else's message refuses; selection persists via note.
        app.on_event(key(KeyCode::Char('e')));
        assert!(out.try_recv().is_err(), "no wire traffic for refused edit");

        // Up again → ada's own message; 'e' prefills the input for editing.
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Up));
        let selected = app.selected.expect("still selecting");
        assert!(app.log[selected].raw.contains("my words"));
        app.on_event(key(KeyCode::Char('e')));
        assert_eq!(app.input, format!("/edit {MSGID} my words"));
        assert_eq!(app.selected, None, "action leaves selection mode");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(
            out.try_recv().unwrap(),
            format!("@label=t2 EDIT {MSGID} :my words")
        );

        // 'd' deletes immediately; 'm' marks; 'r' opens the picker.
        app.on_event(key_mod(KeyCode::Up, KeyModifiers::ALT)); // newest (bob's)
        app.on_event(key(KeyCode::Up)); // older → ada's message
        app.on_event(key(KeyCode::Char('d')));
        assert_eq!(out.try_recv().unwrap(), format!("@label=t3 DELETE {MSGID}"));
        app.on_event(key_mod(KeyCode::Up, KeyModifiers::ALT));
        app.on_event(key(KeyCode::Char('m')));
        assert!(out
            .try_recv()
            .unwrap()
            .starts_with("@label=t4 MARK #general"));

        // 'r' opens the smiley picker; a digit reacts instantly.
        app.on_event(key_mod(KeyCode::Up, KeyModifiers::ALT));
        app.on_event(key(KeyCode::Char('r')));
        assert!(app.picker.is_some());
        app.on_event(key(KeyCode::Char('1')));
        let wire = out.try_recv().unwrap();
        assert!(
            wire.ends_with("REACT test.example/01ARZ3NDEKTSV4RRFFQ69G5FB0 👍"),
            "{wire}"
        );
        assert_eq!(app.picker, None);

        // Esc leaves selection mode without quitting.
        app.on_event(key_mod(KeyCode::Up, KeyModifiers::ALT));
        assert!(app.selected.is_some());
        app.on_event(key(KeyCode::Esc));
        assert_eq!(app.selected, None);
        assert!(!app.quit, "Esc in selection mode must not quit");
    }

    #[test]
    fn page_up_at_the_top_backfills_older_history() {
        let (mut app, mut out) = joined_app();
        app.viewport = 5; // small window so "top" is reachable
        feed(
            &mut app,
            &format!("@msgid={MSGID} MESSAGE #general bob@test.example :oldest known"),
        );

        // At the top → PageUp requests the page before our oldest message.
        app.on_event(key(KeyCode::PageUp));
        assert_eq!(
            out.try_recv().unwrap(),
            format!("@label=t2 HISTORY #general before={MSGID} limit=20")
        );
        // No double-fire while the batch is in flight.
        app.on_event(key(KeyCode::PageUp));
        assert!(out.try_recv().is_err());

        // The batch splices in ABOVE existing entries, in order.
        let len_before = app.log.len();
        feed(&mut app, "@id=b9;label=t2 BATCH START");
        feed(&mut app, "@label=t2;msgid=test.example/01ARZ3NDEKTSV4RRFFQ69G5F00 MESSAGE #general bob@test.example :ancient");
        feed(&mut app, "@compacted;id=b9;label=t2 BATCH END");
        assert_eq!(app.log.len(), len_before + 3);
        assert!(app.log[0].raw.contains("BATCH START"));
        assert!(app.log[1].raw.contains("ancient"));
        assert!(app.log[2].raw.contains("BATCH END"));
        // Old messages must not steal newest-message tracking: ↑ still
        // has no own message to edit, and the next backfill anchors on
        // the NEW oldest (the spliced-in one).
        app.on_event(key(KeyCode::PageUp));
        let wire = out.try_recv().unwrap();
        assert!(
            wire.contains("before=test.example/01ARZ3NDEKTSV4RRFFQ69G5F00"),
            "{wire}"
        );
    }

    #[test]
    fn mouse_wheel_scrolls_and_backfills_at_top() {
        let (mut app, mut out) = joined_app();
        app.viewport = 3;
        feed(
            &mut app,
            &format!("@msgid={MSGID} MESSAGE #general bob@test.example :anchor"),
        );
        let wheel = |kind| {
            AppEvent::Term(TermEvent::Mouse(crossterm::event::MouseEvent {
                kind,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }))
        };
        app.on_event(wheel(MouseEventKind::ScrollDown));
        assert_eq!(app.scroll, 0, "can't scroll below the bottom");
        app.on_event(wheel(MouseEventKind::ScrollUp));
        assert!(app.scroll > 0);
        // Wheel-up at the top triggers the same backfill as PageUp.
        for _ in 0..4 {
            app.on_event(wheel(MouseEventKind::ScrollUp));
        }
        let wire = out.try_recv().unwrap();
        assert!(wire.contains("HISTORY #general before="), "{wire}");
    }

    #[test]
    fn wrong_start_password_recovers_via_prefilled_login() {
        let (mut app, mut out) = harness();
        app.on_event(AppEvent::Net(NetEvent::Connected));
        out.try_recv().unwrap(); // HELLO
        feed(&mut app, "WELCOME test.example");
        out.try_recv().unwrap(); // AUTH PASSWORD (dev password — wrong)

        feed(&mut app, "ERR AUTH-FAILED :authentication failed");
        out.try_recv().unwrap(); // REGISTER fallback
        feed(&mut app, "ERR CONFLICT :account name is taken");
        // The client prefills /login: user types the password and Enter.
        assert_eq!(app.input, "/login ada ");
        for c in "real-password-123".chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
        app.on_event(key(KeyCode::Enter));
        assert_eq!(
            out.try_recv().unwrap(),
            "@label=t4 AUTH PASSWORD ada :real-password-123"
        );
        // A wrong retry keeps offering the prompt instead of dead-ending.
        feed(&mut app, "ERR AUTH-FAILED :authentication failed");
        assert_eq!(app.input, "/login ada ");
        // The right one lands in READY via the normal WELCOME arm.
        app.input.clear();
        type_line(&mut app, "/login ada correct-horse-battery");
        out.try_recv().unwrap();
        feed(&mut app, "WELCOME test.example");
        assert!(out.try_recv().unwrap().ends_with("JOIN #general")); // autojoin proceeds
    }

    #[test]
    fn status_command_sends_presence() {
        let (mut app, mut out) = harness();
        type_line(&mut app, "/status away");
        assert_eq!(out.try_recv().unwrap(), "@label=t1 PRESENCE away");
        type_line(&mut app, "/status busy");
        assert!(out.try_recv().is_err(), "invalid status refused locally");
    }

    #[test]
    fn dms_surface_as_cycleable_conversations() {
        let (mut app, mut out) = harness();
        // Inbound DM from bob: conversation appears, becomes current.
        feed(
            &mut app,
            &format!("@msgid={MSGID} MESSAGE @ada bob@test.example :hi ada"),
        );
        assert_eq!(app.dm_peers, vec!["@bob"]);
        assert_eq!(app.current.as_deref(), Some("@bob"));
        // Plain text now goes to the DM.
        type_line(&mut app, "hey bob");
        assert_eq!(out.try_recv().unwrap(), "@label=t1 MSG @bob :hey bob");
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
