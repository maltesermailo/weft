# M1 Review — echo server (weft-core, weft-transport, weftd)

*Self-review of the M1 implementation. Status at time of review: 73 tests green
workspace-wide (49 proto, 17 core, 7 conformance), clippy `-D warnings` clean,
`cargo fmt` applied, real binary smoke-tested end-to-end over a raw WebSocket.*

## Scope reviewed

| Crate | Contents |
|---|---|
| `weft-proto` (+1 change) | `MEMBER` gained the `count=` tag (§6.3 JOIN response) — proto-first with round-trip tests, per CLAUDE.md; spec §7 row updated |
| `weft-core` | `ControlStream` trait (`stream.rs`), session FSM + dispatch (`session.rs`), channel actors (`channel.rs`), immutable registry (`registry.rs`), `ServerCtx` (`context.rs`) |
| `weft-transport` | QUIC: rustls/ALPN `weft/1` setup + `LinesCodec`-framed control stream with the 8 KiB cap at the framing layer; WS: one text frame = one line |
| `weftd` | TOML config, tracing, acceptors + the two trait adapters, self-signed dev certs (rcgen), embeddable `start()` for tests |
| Conformance | Black-box QUIC + WS tests against a real in-process server: full flow, cross-connection relay, cross-transport relay, ALPN rejection, version mismatch, malformed |

Key mechanics: sessions are one task per connection selecting over the stream
and a bounded event queue; channel actors exclusively own membership and mint
monotonic ULIDs (inbox order = channel order, §9.1); per-channel forwarder
tasks translate `broadcast::RecvError::Lagged` into `ERR SLOW` (§9.2). The
sender's own MESSAGE broadcast copy becomes the labeled echo-ack by popping a
per-channel FIFO of pending labels — correct because one mpsc sender into one
actor preserves publish order. `(session,label)` dedup replays the stored echo
line verbatim for 5 minutes; a label still awaiting its echo is dropped.

## Architectural decision (deviation from the architecture doc)

The architecture doc places `ControlStream` in `weft-transport/traits.rs`, but
CLAUDE.md's strict layering says `weft-core` depends on proto only and
transport must not be interpreted by core. Both can't hold, so the trait lives
in **weft-core** (hexagonal port), transport exposes concrete line-stream
types, and `weftd` provides the two ~15-line adapters. This keeps
`weft-transport` deps = proto ONLY, as required. Similarly, the WS fallback
uses tokio-tungstenite rather than axum for now — axum arrives with
`/.well-known/weft` in M2, avoiding an HTTP stack this milestone doesn't need.

## Issues found and fixed during review

1. **Final ERR lost on server-initiated close (real bug, caught by
   conformance).** After `ERR UNSUPPORTED` (version mismatch) the acceptor
   closed the QUIC connection immediately; `Connection::close` abandons
   un-acked stream data and quinn resets an unfinished `SendStream` on drop —
   the client saw the connection die without the error. Fixed with a
   `ControlStream::close()` hook (flush + FIN) called by the session, and the
   acceptor now waits up to 3 s for the peer to close first.
2. **`clippy::result_large_err`** — boxed the tungstenite variant of
   `TransportError`.

## Deliberate M1 simplifications (each with its landing milestone)

- **Anonymous AUTH**: `AUTH PASSWORD` accepts any credentials and claims the
  account (architecture doc M1 "AUTH(anon)"). The security invariants
  (constant-time compare, uniform `AUTH-FAILED`) activate with the real
  account store in M2 — `session.rs::on_unauthed` is the seam.
- **Static channels from config**: JOIN never auto-creates (§6.3) and
  `CHANNEL CREATE` is M4, so M1 channels exist only via config. That makes the
  registry an immutable `HashMap` — zero locks; the doc's `DashMap` + lazy
  actor spawn/park comes with dynamic channels.
- **Not-a-member MSG/TYPING answers `CAP-REQUIRED send`** when the channel
  exists, `NO-SUCH-TARGET` otherwise. Safe only because all M1 channels are
  public; the M4 visibility work must route private channels through
  `NO-SUCH-TARGET` (flagged in a comment at the branch).
- **`ERR SLOW` without the forced HISTORY resync** — HISTORY is M3; the spec's
  recovery contract is only half-deliverable until then.
- **Verbs parked with honest `UNSUPPORTED`**: DMs (M3), MARK (M3), PRESENCE
  (feature-gated), attachments (M6), invites (M4), AUTH KEY/ENROLL (M2),
  REGISTER → `FORBIDDEN` (no account store = registration closed).

## Known edge cases accepted (documented in code)

- **Re-JOIN drops pending echo labels**: the old broadcast receiver dies with
  the old subscription, so their echoes are gone either way; a client that
  re-JOINs with MSGs in flight may retry a message and duplicate it. Narrow,
  client-induced, fixed properly by M3 msgid-based receive dedup.
- **PART racing an own in-flight MSG** can emit the echo without its label
  (pending queue died with the membership); the client retries into
  `CAP-REQUIRED`. Same family as above.
- **Dedup map growth** is bounded only by client send rate within the 5-minute
  window — rate limiting beyond THROTTLED plumbing is deliberately deferred
  (CLAUDE.md "do not add").
- **`Server::shutdown` closes the QUIC endpoint but not live WS sessions**
  (the TCP accept loop is aborted; established sockets drain when clients
  leave). Graceful drain across both transports is deferred.
- **Idle detection counts only inbound lines** (180 s in READY ≈ two missed
  §3.4 keepalives); the server does not yet originate PINGs — dead peers in
  busy channels are instead caught by write errors.
- **TYPING is not rate-limited** (spec RECOMMENDED 1/3 s) — same deferred
  rate-limiting bucket.

## Test strategy notes

- Core tests run the *entire* domain layer — session FSM, actors, registry,
  fan-out — over an in-memory mock stream, no sockets (the layering payoff:
  17 tests in ~0s, including a paused-clock test of the 30 s pre-auth timeout).
- Conformance tests are genuinely black-box: real QUIC handshake (including a
  wrong-ALPN rejection test) and a real WS client, plus a cross-transport
  relay test (QUIC sender → WS receiver in one channel).
- Not covered: the SLOW/lag path end-to-end (needs deterministic overflow of a
  512-slot broadcast ring behind a blocked consumer — worth a dedicated
  harness when HISTORY resync lands in M3) and multi-line malformed flooding
  over QUIC (covered networkless in core).

## Next step

M2 — identity: weft-crypto (Ed25519 attestations, deterministic CBOR), AUTH
KEY challenge-response (`nonce‖network-name`, §6.1), `AUTH ENROLL`,
`/.well-known/weft` (axum), real certificates replacing the per-boot rcgen
self-signed one.
