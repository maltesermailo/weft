//! Rendering: log pane (pretty or raw wire), status bar, input line —
//! and the LogEntry constructors that decide how each event reads.

use ratatui::layout::{Constraint, Layout, Position};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use weft_proto::{Event, Reply};

use crate::app::{App, LogEntry};

pub fn render(frame: &mut Frame, app: &App) {
    let [log_area, status_area, input_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    // Newest lines stick to the bottom; `scroll` walks back into history.
    let height = log_area.height as usize;
    let end = app.log.len().saturating_sub(app.scroll);
    let start = end.saturating_sub(height);
    let lines: Vec<Line> = app.log[start..end]
        .iter()
        .map(|entry| {
            if app.raw_mode {
                Line::from(entry.raw.clone())
            } else {
                entry.pretty.clone()
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), log_area);

    let status = format!(
        " {}@{} │ {} │ {} joined{} │ Tab switch · Ctrl+R raw · /help ",
        app.account,
        app.network.as_deref().unwrap_or("connecting…"),
        app.current.as_deref().unwrap_or("no channel"),
        app.joined.len(),
        if app.raw_mode { " │ RAW" } else { "" },
    );
    frame.render_widget(
        Paragraph::new(status).style(Style::default().add_modifier(Modifier::REVERSED)),
        status_area,
    );

    let prompt = format!("› {}", app.input);
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

pub fn note_entry(text: &str) -> LogEntry {
    LogEntry {
        raw: format!("* {text}"),
        pretty: Line::styled(format!("* {text}"), DIM),
    }
}

pub fn outbound_entry(line: &str) -> LogEntry {
    LogEntry {
        raw: format!("→ {line}"),
        pretty: Line::styled(format!("→ {line}"), DIM),
    }
}

pub fn unparseable_entry(raw: &str, error: &str) -> LogEntry {
    LogEntry {
        raw: raw.to_string(),
        pretty: Line::styled(format!("?? {raw} ({error})"), ERR),
    }
}

pub fn reply_entry(raw: String, reply: &Reply, me: &str) -> LogEntry {
    let label = reply
        .label
        .as_deref()
        .map(|l| format!(" ⟨{l}⟩"))
        .unwrap_or_default();
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
            Line::from(vec![
                Span::styled(format!("{} ", msg.target), DIM),
                Span::styled(format!("<{}>", msg.sender.account), style),
                Span::raw(format!(" {}", msg.body)),
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
        // Not expected from an M1 server, but render something sane.
        other => Line::styled(format!("? {other:?}"), DIM),
    };
    LogEntry { raw, pretty }
}
