# Fix — intermittent QUIC disconnects on quiet connections

*Reported as "the TUI intermittently disconnects randomly". Root-caused,
reproduced with a failing test, fixed in weft-transport, re-verified.*

## Root cause

quinn's **default transport config** is `max_idle_timeout = 30 s` with
`keep_alive_interval = None` (verified in `quinn-proto-0.11.15
src/config/transport.rs:365,381`). QUIC negotiates the effective idle timeout
as the minimum of both endpoints, so every connection carried a silent 30 s
death timer.

The protocol's keepalive cadence (§3.4) is **60 s** — the TUI's application
PING and any conformant client's PING arrive too late. Result: any connection
with no traffic for ~30 s was reaped at the transport layer, underneath the
session logic (whose own 180 s idle limit never got the chance to matter).
"Intermittent/random" was simply "whenever nobody spoke for half a minute."

Why nothing caught it earlier: every conformance test completes in
milliseconds and the TUI smoke test ran ~5 s — all inside the 30 s window.

## Fix (weft-transport)

- Shared `transport_config()`: `max_idle_timeout = 120 s` on **both** sides —
  comfortably above the 60 s PING cadence, still bounded so dead peers are
  reaped.
- Client endpoints (`insecure::client_endpoint`, used by weft-tui and the
  conformance suite): `keep_alive_interval = 15 s`. §3.4 explicitly blesses
  this — "QUIC keepalive may substitute for sending, not for answering."
- Server: no QUIC keepalive (liveness is the client's burden); the session
  layer's line-based idle limits stay authoritative on top.

## Verification

New conformance test `quic_survives_a_long_silent_gap`: join, 45 s of dead
silence, then MSG must still round-trip with its label.

- **Before the fix: FAILED** at 45 s (connection already dead) — diagnosis
  confirmed, not assumed.
- **After the fix: passes.**

It is `#[ignore]`d (45 s wall time) so the routinely-run suite stays fast;
run it with `cargo test -p weftd --test conformance -- --ignored`.
Full suite: 79 tests green, clippy `-D warnings` clean.

## Follow-up observation (spec vs. server, unresolved)

§3.4 says a client may rely on QUIC keepalive instead of *sending* PINGs, but
weftd's READY idle check counts inbound **lines** (180 s) and the server never
originates PINGs — a conformant PING-less client would still be dropped at
180 s despite a live transport. Harmless for current clients (the TUI pings
every 60 s), but the M2 session work should either originate server PINGs or
treat transport-level liveness as "sending" per spec. Recorded here so it
isn't rediscovered the hard way.
