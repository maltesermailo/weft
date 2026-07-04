//! Rendering: log pane (pretty or raw wire), status bar, input line —
//! and the LogEntry constructors that decide how each event reads.

use ratatui::layout::{Constraint, Layout, Position};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use weft_proto::{Event, Reply};

use crate::app::{App, LogEntry, MsgRef};

pub fn render(frame: &mut Frame, app: &mut App) {
    let [log_area, status_area, input_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    // Newest lines stick to the bottom; `scroll` walks back into history.
    let height = log_area.height as usize;
    app.viewport = height.max(1); // selection uses this to stay visible
    let end = app.log.len().saturating_sub(app.scroll);
    let start = end.saturating_sub(height);
    let lines: Vec<Line> = app.log[start..end]
        .iter()
        .enumerate()
        .map(|(offset, entry)| {
            let mut line = if app.raw_mode {
                Line::from(entry.raw.clone())
            } else {
                entry.pretty.clone()
            };
            if app.selected == Some(start + offset) {
                line = line.style(Style::default().add_modifier(Modifier::REVERSED));
            }
            line
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), log_area);

    let status = if app.picker.is_some() {
        " add a reaction │ press 1-9 · Esc cancel ".to_string()
    } else if app.selected.is_some() {
        " message selected │ e edit · d delete · r/+ react · m mark · ↑↓ move · Esc back "
            .to_string()
    } else {
        format!(
            " {}@{} │ {} │ {} joined{} │ ↑ select · Tab switch · Ctrl+R raw · /help ",
            app.account,
            app.network.as_deref().unwrap_or("connecting…"),
            app.current.as_deref().unwrap_or("no channel"),
            app.joined.len() + app.dm_peers.len(),
            if app.raw_mode { " │ RAW" } else { "" },
        )
    };
    frame.render_widget(
        Paragraph::new(status).style(Style::default().add_modifier(Modifier::REVERSED)),
        status_area,
    );

    let prompt = if app.picker.is_some() {
        // The smiley palette takes over the input line while picking.
        let palette: Vec<String> = crate::app::QUICK_REACTIONS
            .iter()
            .enumerate()
            .map(|(i, e)| format!("{} {e}", i + 1))
            .collect();
        format!("react: {}", palette.join("  "))
    } else {
        format!("› {}", app.input)
    };
    let cursor_x = input_area.x + prompt.chars().count() as u16;
    frame.render_widget(Paragraph::new(prompt), input_area);
    frame.set_cursor_position(Position::new(cursor_x, input_area.y));
}

// ---- log entry constructors ----

const DIM: Style = Style::new().fg(Color::DarkGray);
const ERR: Style = Style::new().fg(Color::Red);

/// Stable per-account color so speakers are tellable-apart at a glance.
fn account_color(account: &str) -> Color {
    const PALETTE: [Color; 6] = [
        Color::Cyan,
        Color::Green,
        Color::Yellow,
        Color::Magenta,
        Color::Blue,
        Color::LightRed,
    ];
    let hash: usize = account.bytes().map(usize::from).sum();
    PALETTE[hash % PALETTE.len()]
}

/// Msgids are 40+ chars; the tail of the ULID is enough to eyeball
/// correlation in pretty mode (raw mode has the full ids).
fn short(msgid: &weft_proto::MsgId) -> String {
    let s = msgid.to_string();
    format!("…{}", &s[s.len().saturating_sub(6)..])
}

pub fn note_entry(text: &str) -> LogEntry {
    LogEntry {
        raw: format!("* {text}"),
        pretty: Line::styled(format!("* {text}"), DIM),
        message: None,
    }
}

pub fn outbound_entry(line: &str) -> LogEntry {
    LogEntry {
        raw: format!("→ {line}"),
        pretty: Line::styled(format!("→ {line}"), DIM),
        message: None,
    }
}

pub fn unparseable_entry(raw: &str, error: &str) -> LogEntry {
    LogEntry {
        raw: raw.to_string(),
        pretty: Line::styled(format!("?? {raw} ({error})"), ERR),
        message: None,
    }
}

pub fn reply_entry(raw: String, reply: &Reply, me: &str) -> LogEntry {
    let label = reply
        .label
        .as_deref()
        .map(|l| format!(" ⟨{l}⟩"))
        .unwrap_or_default();
    let message = match &reply.event {
        Event::Message(msg) => Some(MsgRef {
            msgid: msg.msgid.clone(),
            target: msg.target.to_string(),
            body: msg.body.clone(),
            own: msg.sender.account.as_str() == me,
        }),
        _ => None,
    };
    let pretty = match &reply.event {
        Event::Message(msg) => {
            let account = msg.sender.account.as_str();
            let style = if account == me {
                Style::new()
                    .fg(account_color(account))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(account_color(account))
            };
            let edited = msg
                .edited
                .map(|n| format!(" (edited ×{n})"))
                .unwrap_or_default();
            Line::from(vec![
                Span::styled(format!("{} ", msg.target), DIM),
                Span::styled(format!("<{}>", msg.sender.account), style),
                Span::raw(format!(" {}", msg.body)),
                Span::styled(edited, DIM),
                Span::styled(label, DIM), // echo marker: the visible ack
            ])
        }
        Event::Member {
            channel,
            user,
            action,
            count,
            ..
        } => {
            let count = count.map(|c| format!(" ({c} members)")).unwrap_or_default();
            Line::styled(format!("{channel} ✦ {user} {action}{count}{label}"), DIM)
        }
        Event::Typing {
            channel,
            user,
            state,
        } => Line::styled(
            format!("{channel} ✎ {user} typing {state}"),
            Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ),
        Event::Policy { channel, policy } => {
            Line::styled(format!("{channel} ✦ retention: {policy}{label}"), DIM)
        }
        Event::Welcome { network, motd, .. } => {
            let motd = motd
                .as_deref()
                .map(|m| format!(" — {m}"))
                .unwrap_or_default();
            Line::styled(
                format!("✓ {network}{motd}{label}"),
                Style::new().fg(Color::Green),
            )
        }
        Event::Err(err) => {
            let context = err
                .context
                .as_deref()
                .map(|c| format!(" {c}"))
                .unwrap_or_default();
            Line::styled(format!("! {}{context}: {}{label}", err.code, err.text), ERR)
        }
        Event::Pong { token } => Line::styled(
            format!("· pong {}{label}", token.as_deref().unwrap_or("")),
            DIM,
        ),
        Event::Edited {
            target,
            user,
            edit_of,
            body,
            ..
        } => Line::from(vec![
            Span::styled(format!("{target} "), DIM),
            Span::styled(
                format!("<{}>", user.account),
                Style::new().fg(account_color(user.account.as_str())),
            ),
            Span::raw(format!(" {body}")),
            Span::styled(format!(" ✎ edited {}{label}", short(edit_of)), DIM),
        ]),
        Event::Deleted { target, msgid, by } => {
            let by = by
                .as_ref()
                .map(|u| format!(" by {}", u.account))
                .unwrap_or_default();
            Line::styled(
                format!("{target} ✗ message {} deleted{by}{label}", short(msgid)),
                DIM,
            )
        }
        Event::Reaction {
            target,
            msgid,
            emoji,
            op,
            by,
        } => {
            let verb = match op {
                weft_proto::ReactionOp::Add => "reacted",
                weft_proto::ReactionOp::Remove => "unreacted",
            };
            Line::styled(
                format!(
                    "{target} {} {verb} {emoji} → {}{label}",
                    by.account,
                    short(msgid)
                ),
                DIM,
            )
        }
        Event::Reactions {
            target,
            msgid,
            emoji,
            count,
            by,
        } => {
            let actors: Vec<&str> = by.iter().map(|u| u.account.as_str()).collect();
            Line::styled(
                format!(
                    "{target} {emoji} ×{count} on {} ({}){label}",
                    short(msgid),
                    actors.join(", ")
                ),
                DIM,
            )
        }
        Event::BatchStart { .. } => Line::styled(format!("── history ──{label}"), DIM),
        Event::BatchEnd {
            truncated,
            compacted,
            ..
        } => {
            let mut flags = Vec::new();
            if *compacted {
                flags.push("compacted");
            }
            if *truncated {
                flags.push("older messages expired");
            }
            let flags = if flags.is_empty() {
                String::new()
            } else {
                format!(" ({})", flags.join(", "))
            };
            Line::styled(format!("── end{flags} ──{label}"), DIM)
        }
        Event::Marked { channel, msgid } => Line::styled(
            format!("✓ {channel} read up to {}{label}", short(msgid)),
            DIM,
        ),
        Event::Presence { user, status } => Line::styled(
            format!("● {} is {status}{label}", user.account),
            Style::new().fg(account_color(user.account.as_str())),
        ),
        // Not expected from an M1 server, but render something sane.
        other => Line::styled(format!("? {other:?}"), DIM),
    };
    LogEntry {
        raw,
        pretty,
        message,
    }
}
