# Review — weft-tui terminal test client

*Self-review. Status at time of review: 79 tests green workspace-wide (6 new
app-logic tests), clippy `-D warnings` clean, `cargo fmt` applied, verified
live against weftd under a pseudo-TTY.*

## What it is

`crates/weft-tui` — a ratatui/crossterm client for exercising weftd by hand:

- Connects over QUIC (ALPN `weft/1`), auto-drives HELLO → AUTH (§3.3), and
  optionally auto-joins a channel: `weft-tui [host:port] [account] [#channel]`.
- IRC-style usage: plain text goes to the current channel; `/join`, `/part`,
  `/msg`, `/channel`, `/ping`, `/quit`, Tab cycles channels.
- Test-client affordances: **Ctrl+R** toggles a raw wire view (the netcat
  view); `/raw <line>` sends any line verbatim (valid or not — for poking the
  MALFORMED/unknown-verb paths); every outbound line is logged dim; labels are
  auto-attached (`t1`, `t2`, …) and rendered as `⟨label⟩` on responses, which
  makes echo-acks and label correlation *visible* — the point of the tool.
- Membership tracked from own MEMBER echoes; per-account colors; errors red.

Structure keeps the state machine testable: `app.rs` (events in → wire lines
out, no terminal I/O), `net.rs` (owns the stream: inbound pump, outbound
queue, 60 s keepalive PING so the server's 180 s idle limit never kills a
quiet client), `ui.rs` (rendering + log-entry formatting), `main.rs` (args,
blocking input thread, redraw loop with burst coalescing).

## DRY refactor alongside

The conformance suite's cert-blind TLS client moved into
`weft-transport::insecure` behind a new `insecure-client` feature (compiled
only for test tooling; the weftd release binary never includes it). The
conformance tests and the TUI now share it — ~55 duplicated lines deleted.

## Verification

- 6 unit tests drive the app with synthetic key/net events and assert the
  exact wire lines produced: handshake progression (HELLO → AUTH → autojoin on
  the second WELCOME), membership tracking from MEMBER echoes (ignoring other
  users' joins), plain-text → MSG, `/raw` verbatim passthrough, channel
  validation before send, Esc → QUIT.
- Live check: weftd + weft-tui under a `script` pseudo-TTY, scripted
  keystrokes. The captured frames show the full session — WELCOME with motd,
  join response with member count, retention line, the labeled MESSAGE echo
  ⟨t4⟩, clean QUIT.

## Notes / accepted limitations

- **Cert-blind by design** and labeled as such in the crate description and
  module docs; when M2 publishes keys via `/.well-known/weft`, the TUI should
  grow proper verification (and lose the feature flag).
- **No reconnect**: a dropped connection shows `✕ …` and the client stays up
  for post-mortem reading; restart to reconnect. Fine for a test tool.
- **No line wrapping in the log** (long messages clip) and **no unicode-width
  cursor math** (wide glyphs offset the cursor slightly). Cosmetic; kept the
  scroll model trivial.
- **Pretty/raw is a whole-log toggle**, not per-entry; raw mode shows inbound
  lines verbatim and outbound with a `→` prefix.
- The first `script`-based smoke run rendered nothing — the pty defaulted to
  a 1-row terminal, not a code bug; forcing `stty rows 30 cols 110` showed
  full rendering. Worth remembering when scripting TUI tests.
- Terminal state is restored via `ratatui::init/restore` (panic hook
  included), and quitting flushes the trailing QUIT with a 150 ms grace.

## Possible follow-ups (not done)

- Per-channel buffers instead of one timeline; unread markers via MARK (M3).
- A `--ws` flag to exercise the WebSocket fallback path interactively.
- Scripted "conversation replay" mode for load-testing channels.
