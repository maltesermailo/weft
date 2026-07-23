//! In-process load generator for the WEFT reference server.
//!
//! It drives the **real** server-side event pipeline — session FSM → channel
//! actor (single-writer ULID mint) → `EventStore::append` → `broadcast`
//! fan-out — over in-memory `ControlStream`s, so there is no QUIC/TLS/framing
//! in the measurement. That is deliberate: it isolates how the *core* fares
//! with hundreds of thousands of events, which is where the actor/store/fan-out
//! bottleneck lives (the socket layer is a separate, well-trodden axis).
//!
//! Topology: `channels × senders_per_channel` sender sessions, each pipelining
//! `messages` MSGs (bounded in-flight `window`) into its channel, plus
//! `subscribers` idle receivers per channel to exercise fan-out. Setup
//! (connect/register/join) is untimed; only the message flood is measured.
//!
//! Run: `cargo run --release -p weftd --bin loadtest -- \
//!         --channels 4 --senders-per-channel 25 --messages 2000 --subscribers 20`

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, Barrier};
use tokio::task::JoinHandle;

use weft_core::{
    run_session, ControlStream, FederationConfig, Keypair, MemBlobStore, MemoryStore, ServerCtx,
    ServerInfo,
};
use weft_proto::{Event, Reply};

const PASSWORD: &str = "loadtest-password-123";

/// A real (non-system) channel message — excludes the `join`/`part` system
/// broadcasts that also arrive as `Event::Message` during setup.
fn is_real_msg(event: &Event) -> bool {
    matches!(event, Event::Message(m) if m.meta.system.is_none())
}

// ---- config ----

struct Cfg {
    channels: usize,
    senders_per_channel: usize,
    messages: usize,
    subscribers: usize,
    window: usize,
    body_len: usize,
}

impl Cfg {
    fn total_senders(&self) -> usize {
        self.channels * self.senders_per_channel
    }
    fn total_events(&self) -> usize {
        self.total_senders() * self.messages
    }
}

fn parse_args() -> Cfg {
    let mut cfg = Cfg {
        channels: 1,
        senders_per_channel: 50,
        messages: 2000,
        subscribers: 0,
        window: 32,
        body_len: 48,
    };
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut val = || {
            args.next()
                .unwrap_or_else(|| panic!("{flag} needs a value"))
                .parse()
                .unwrap_or_else(|_| panic!("bad value for {flag}"))
        };
        match flag.as_str() {
            "--channels" => cfg.channels = val(),
            "--senders-per-channel" => cfg.senders_per_channel = val(),
            "--messages" => cfg.messages = val(),
            "--subscribers" => cfg.subscribers = val(),
            "--window" => cfg.window = val(),
            "--body-len" => cfg.body_len = val(),
            "-h" | "--help" => {
                eprintln!(
                    "flags: --channels --senders-per-channel --messages --subscribers --window --body-len"
                );
                std::process::exit(0);
            }
            other => panic!("unknown flag {other}"),
        }
    }
    cfg.window = cfg.window.max(1).min(cfg.messages.max(1));
    cfg
}

// ---- in-memory ControlStream ----

struct LoadStream {
    from_client: mpsc::UnboundedReceiver<String>,
    to_client: mpsc::UnboundedSender<String>,
}

impl ControlStream for LoadStream {
    async fn recv_line(&mut self) -> std::io::Result<Option<String>> {
        Ok(self.from_client.recv().await)
    }
    async fn send_line(&mut self, line: &str) -> std::io::Result<()> {
        self.to_client
            .send(line.to_string())
            .map_err(|_| std::io::Error::other("client gone"))
    }
}

struct Conn {
    tx: mpsc::UnboundedSender<String>,
    rx: mpsc::UnboundedReceiver<String>,
}

impl Conn {
    fn send(&self, line: String) {
        let _ = self.tx.send(line);
    }
    /// Next parseable reply, or `None` when the session closes.
    async fn recv(&mut self) -> Option<Reply> {
        loop {
            let raw = self.rx.recv().await?;
            if let Ok(reply) = Reply::parse(&raw) {
                return Some(reply);
            }
        }
    }
    /// Drain until a reply matches `pred` (used to step through the setup
    /// handshake, skipping MediaToken/system/etc.).
    async fn recv_until(&mut self, pred: impl Fn(&Event) -> bool) {
        while let Some(reply) = self.recv().await {
            if pred(&reply.event) {
                return;
            }
        }
        panic!("session closed during setup");
    }
}

fn spawn_session(ctx: &Arc<ServerCtx>) -> (Conn, JoinHandle<()>) {
    let (to_server, from_client) = mpsc::unbounded_channel();
    let (to_client, from_server) = mpsc::unbounded_channel();
    let stream = LoadStream {
        from_client,
        to_client,
    };
    let handle = tokio::spawn(run_session(stream, Arc::clone(ctx)));
    (
        Conn {
            tx: to_server,
            rx: from_server,
        },
        handle,
    )
}

/// HELLO + REGISTER + JOIN (all untimed setup).
async fn setup(conn: &mut Conn, account: &str, channel: &str) {
    conn.send("HELLO weft/1".to_string());
    conn.recv_until(|e| matches!(e, Event::Welcome { .. }))
        .await;
    conn.send(format!("REGISTER {account} :{PASSWORD}"));
    conn.recv_until(|e| matches!(e, Event::Welcome { .. }))
        .await;
    conn.send(format!("JOIN {channel}"));
    conn.recv_until(|e| matches!(e, Event::Policy { .. })).await;
}

// ---- per-client work ----

struct SenderResult {
    latencies_ns: Vec<u64>,
    cross_deliveries: u64,
}

/// Pipeline `messages` MSGs with a bounded in-flight window, recording
/// send→echo-ack latency. Our own echoes carry our label; unlabeled Messages
/// are fan-out copies from other senders in the channel.
async fn run_sender(mut conn: Conn, channel: String, cfg: Arc<Cfg>) -> SenderResult {
    let body = "x".repeat(cfg.body_len);
    let mut send_ts: Vec<Instant> = Vec::with_capacity(cfg.messages);
    let mut latencies = Vec::with_capacity(cfg.messages);
    let mut cross = 0u64;
    let mut sent = 0usize;
    let mut acked = 0usize;

    while acked < cfg.messages {
        // Keep the in-flight window full.
        while sent < cfg.messages && sent - acked < cfg.window {
            send_ts.push(Instant::now());
            conn.send(format!("@label={sent} MSG {channel} :{body}"));
            sent += 1;
        }
        // A quiet gap means the channel drained (or lag dropped our acks under
        // heavy same-channel fan-out) — stop rather than block forever.
        match tokio::time::timeout(Duration::from_secs(3), conn.recv()).await {
            Ok(Some(reply)) => match (&reply.event, reply.label) {
                (Event::Message(m), Some(label)) if m.meta.system.is_none() => {
                    // Our ack. Labels come back in send order per session.
                    if let Ok(seq) = label.parse::<usize>() {
                        latencies.push(send_ts[seq].elapsed().as_nanos() as u64);
                        acked += 1;
                    }
                }
                (event, _) if is_real_msg(event) => cross += 1, // fan-out copy from a peer
                _ => {} // system join/part, media token, etc.
            },
            Ok(None) => break, // session died
            Err(_) => break,   // quiet: no ack for 3s
        }
    }

    SenderResult {
        latencies_ns: latencies,
        cross_deliveries: cross,
    }
}

/// Drain fan-out deliveries until `expected` are seen or the channel goes quiet.
async fn run_subscriber(mut conn: Conn, expected: usize) -> u64 {
    let mut got = 0u64;
    while (got as usize) < expected {
        match tokio::time::timeout(Duration::from_secs(15), conn.recv()).await {
            Ok(Some(reply)) => {
                if is_real_msg(&reply.event) {
                    got += 1;
                }
            }
            _ => break, // closed or quiet
        }
    }
    got
}

// ---- reporting ----

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn ms(ns: u64) -> f64 {
    ns as f64 / 1_000_000.0
}

#[cfg(target_os = "linux")]
fn rss_mb() -> Option<f64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages as f64 * 4096.0 / (1024.0 * 1024.0))
}
#[cfg(not(target_os = "linux"))]
fn rss_mb() -> Option<f64> {
    None
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let cfg = Arc::new(parse_args());

    // One ServerCtx over a MemoryStore, with the channels pre-seeded.
    let store = Arc::new(MemoryStore::default());
    let chan_names: Vec<String> = (0..cfg.channels).map(|i| format!("#load{i}")).collect();
    let info = ServerInfo {
        network: "loadtest.example".parse().unwrap(),
        motd: None,
        features: Vec::new(),
    };
    let ctx = Arc::new(ServerCtx::new(
        info,
        chan_names
            .iter()
            .map(|c| (c.parse().unwrap(), "retained:90d".parse().unwrap())),
        Keypair::generate(),
        true, // registration open
        Arc::clone(&store),
        Arc::new(MemBlobStore::default()),
        "permanent".parse().unwrap(),
        Vec::<weft_proto::Account>::new().into_iter(),
        true,
        10_000, // namespace quota — irrelevant here (we create none)
        FederationConfig::default(),
    ));

    let total_senders = cfg.total_senders();
    let total_subs = cfg.channels * cfg.subscribers;
    eprintln!(
        "WEFT in-process load test (session → channel actor → MemoryStore → broadcast; transport excluded)\n\
         channels={} senders/channel={} ({} senders) messages/sender={} subscribers/channel={} window={}\n\
         target: {} events ingested, fan-out to {} members/channel\n\
         setting up {} sessions…",
        cfg.channels,
        cfg.senders_per_channel,
        total_senders,
        cfg.messages,
        cfg.subscribers,
        cfg.window,
        cfg.total_events(),
        cfg.senders_per_channel + cfg.subscribers,
        total_senders + total_subs,
    );

    // Barrier: every client + main, so the timed window starts only once all
    // sessions are registered + joined.
    let barrier = Arc::new(Barrier::new(total_senders + total_subs + 1));
    let mut sender_tasks: Vec<JoinHandle<SenderResult>> = Vec::new();
    let mut sub_tasks: Vec<JoinHandle<u64>> = Vec::new();
    let mut sessions: Vec<JoinHandle<()>> = Vec::new();

    let mut acct = 0usize;
    for (ci, chan) in chan_names.iter().enumerate() {
        for _ in 0..cfg.senders_per_channel {
            let (mut conn, sess) = spawn_session(&ctx);
            sessions.push(sess);
            let account = format!("s{acct}");
            acct += 1;
            let channel = chan.clone();
            let cfg = Arc::clone(&cfg);
            let barrier = Arc::clone(&barrier);
            sender_tasks.push(tokio::spawn(async move {
                setup(&mut conn, &account, &channel).await;
                barrier.wait().await;
                run_sender(conn, channel, cfg).await
            }));
        }
        // Subscribers expect every sender's messages in this channel.
        let expected = cfg.senders_per_channel * cfg.messages;
        for _ in 0..cfg.subscribers {
            let (mut conn, sess) = spawn_session(&ctx);
            sessions.push(sess);
            let account = format!("sub{ci}_{acct}");
            acct += 1;
            let channel = chan.clone();
            let barrier = Arc::clone(&barrier);
            sub_tasks.push(tokio::spawn(async move {
                setup(&mut conn, &account, &channel).await;
                barrier.wait().await;
                run_subscriber(conn, expected).await
            }));
        }
    }

    // Release everyone and start the clock.
    barrier.wait().await;
    let start = Instant::now();

    let mut latencies: Vec<u64> = Vec::with_capacity(cfg.total_events());
    let mut cross_deliveries = 0u64;
    for t in sender_tasks {
        let r = t.await.expect("sender task panicked");
        latencies.extend_from_slice(&r.latencies_ns);
        cross_deliveries += r.cross_deliveries;
    }
    let ingest_elapsed = start.elapsed();

    let mut sub_deliveries = 0u64;
    for t in sub_tasks {
        sub_deliveries += t.await.expect("subscriber task panicked");
    }
    let total_elapsed = start.elapsed();

    // Ingested = every ack we collected (one per accepted+persisted MSG).
    let ingested = latencies.len() as u64;
    let deliveries = ingested + cross_deliveries + sub_deliveries;
    latencies.sort_unstable();

    let secs = ingest_elapsed.as_secs_f64().max(1e-9);
    let total_secs = total_elapsed.as_secs_f64().max(1e-9);

    println!("\n─── results ───");
    println!(
        "ingested   {ingested:>12} events in {:>7.3}s  →  {:>12.0} events/s",
        secs,
        ingested as f64 / secs
    );
    println!(
        "delivered  {deliveries:>12} fan-out copies in {:>6.3}s  →  {:>12.0} deliveries/s",
        total_secs,
        deliveries as f64 / total_secs
    );
    println!(
        "ack latency  p50 {:>7.3}ms   p95 {:>7.3}ms   p99 {:>7.3}ms   max {:>7.3}ms",
        ms(percentile(&latencies, 50.0)),
        ms(percentile(&latencies, 95.0)),
        ms(percentile(&latencies, 99.0)),
        ms(percentile(&latencies, 100.0)),
    );

    // Sanity: every accepted MSG is one persisted event; ingested should equal
    // the target unless a sender fell short.
    if ingested as usize != cfg.total_events() {
        println!(
            "note: ingested {} ≠ target {} (a sender fell short — likely backpressure/timeout)",
            ingested,
            cfg.total_events()
        );
    }
    if let Some(mb) = rss_mb() {
        println!(
            "process RSS {mb:.0} MB (≈ {:.0} B/event held)",
            mb * 1_048_576.0 / ingested.max(1) as f64
        );
    } else {
        println!("(RSS not sampled on this OS; the MemoryStore holds all {ingested} events in RAM — watch top/Activity Monitor)");
    }

    drop(sessions); // sessions exit when their client tx drops
}
