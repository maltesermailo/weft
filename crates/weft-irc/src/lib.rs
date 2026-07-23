//! # weft-irc — WEFT-IRC gateway (§17)
//!
//! An RFC 2812 front-end exposed as a [`weft_core::ControlStream`]: it accepts
//! an IRC client socket and translates IRC ↔ WEFT *at the line boundary*, so
//! `weft_core::run_session` drives the ordinary WEFT session FSM, actors, and
//! store — the gateway is a projection, not a parallel server.
//!
//! One IRC line can yield several WEFT commands (registration →
//! `HELLO`+`AUTH`) and vice-versa; the mapping is the pure, unit-tested
//! [`translate`] module, and this file is just the async I/O around it. The
//! socket read runs in its own task feeding an mpsc so [`IrcStream::recv_line`]
//! stays cancel-safe under `run_session`'s `select!` loop.

#![forbid(unsafe_code)]

mod irc;
mod translate;

use std::collections::VecDeque;
use std::io;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use weft_core::ControlStream;
use weft_proto::Reply;

use translate::St;

/// Bound on unread IRC lines buffered from the socket before backpressure.
const INBOUND_CAP: usize = 64;

/// A gateway connection: an IRC socket presented to `run_session` as a stream
/// of WEFT command lines.
pub struct IrcStream {
    inbound: mpsc::Receiver<String>,
    writer: OwnedWriteHalf,
    st: St,
    /// WEFT commands produced by translation, awaiting `recv_line`.
    queue: VecDeque<String>,
}

impl IrcStream {
    /// Wrap an accepted IRC socket. `server` is this network's name — the
    /// prefix of server-originated IRC lines and numerics.
    pub fn new(tcp: TcpStream, server: impl Into<String>) -> Self {
        let (read, write) = tcp.into_split();
        Self {
            inbound: spawn_reader(read),
            writer: write,
            st: St::new(server),
            queue: VecDeque::new(),
        }
    }

    async fn write_lines(&mut self, lines: &[String]) -> io::Result<()> {
        for line in lines {
            self.writer.write_all(line.as_bytes()).await?;
            self.writer.write_all(b"\r\n").await?;
        }
        if !lines.is_empty() {
            self.writer.flush().await?;
        }
        Ok(())
    }
}

impl ControlStream for IrcStream {
    async fn recv_line(&mut self) -> io::Result<Option<String>> {
        loop {
            if let Some(weft) = self.queue.pop_front() {
                return Ok(Some(weft));
            }
            match self.inbound.recv().await {
                None => return Ok(None), // client gone
                Some(line) => {
                    let Some(msg) = irc::parse(&line) else {
                        continue; // blank/garbage line
                    };
                    let out = translate::from_irc(&msg, &mut self.st);
                    self.write_lines(&out.irc).await?;
                    self.queue.extend(out.weft);
                }
            }
        }
    }

    async fn send_line(&mut self, line: &str) -> io::Result<()> {
        // Only well-formed WEFT events translate; anything else is ignored
        // (the gateway never forwards raw WEFT lines to an IRC client).
        if let Ok(reply) = Reply::parse(line) {
            let out = translate::from_weft(&reply, &mut self.st);
            self.write_lines(&out.irc).await?;
            // Follow-up WEFT commands (e.g. AUTH after WELCOME) go to the queue.
            self.queue.extend(out.weft);
        }
        Ok(())
    }

    async fn close(&mut self) -> io::Result<()> {
        self.writer.shutdown().await
    }
}

/// Per-line byte cap for the IRC reader — matches the native transports' 8 KiB
/// line limit (§4) so a peer can't force unbounded buffering by never sending a
/// newline (threat-model D-6). RFC 2812's own limit is 512 B; this is generous
/// for IRCv3 tags while still bounded.
const MAX_IRC_LINE: u64 = 8192;

/// Read newline-framed IRC lines off the socket in a dedicated task so the
/// stream's `recv_line` await is a cancel-safe `mpsc::recv`.
fn spawn_reader(read: OwnedReadHalf) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel(INBOUND_CAP);
    tokio::spawn(async move {
        let mut reader = BufReader::new(read);
        let mut buf = String::new();
        loop {
            buf.clear();
            // Bound each line to MAX_IRC_LINE: a fresh `take` budget per line
            // (+1 so an over-cap line without a newline is detectable). A line
            // that hits the cap without terminating is a flood → close.
            let mut limited = (&mut reader).take(MAX_IRC_LINE + 1);
            match limited.read_line(&mut buf).await {
                Ok(0) => break, // EOF
                Ok(_) => {
                    if buf.len() as u64 > MAX_IRC_LINE {
                        break; // oversized line (no terminator within the cap)
                    }
                    if tx.send(std::mem::take(&mut buf)).await.is_err() {
                        break; // session gone
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}
