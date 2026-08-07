# Should WEFT's control plane be HTTP/3 instead?

Exploration, not a plan. Written 2026-08-07, prompted by the operational friction of
deploying a custom-ALPN QUIC service behind a reverse proxy.

**Conclusion up front: don't move the semantics onto HTTP — but do add a WebTransport
transport, and plan on it becoming the default.** The reasoning matters more than the
verdict, because the three things people mean by "use HTTP/3" have completely
different answers, and only one of them is a good idea.

*Updated 2026-08-07:* WebTransport reached Baseline in March 2026, which removes the
browser-support objection to (a) entirely and makes it a candidate to eventually
retire the WebSocket fallback. Two things still argue against making it the *only*
transport today — see (a).

## What we have

- **Control plane.** A text line grammar (spec §4) over **QUIC stream 0**, ALPN
  `weft/1`, with a **WebSocket fallback** for networks that block UDP (§3.2).
- **Data plane.** Already HTTP: `POST /media`, `GET /media/<hash>` (§13),
  `/.well-known/weft` (§10.2), `/unfurl`, the admin API, the embedded web client.
- **The seam.** `weft_core::ControlStream` is **three methods** — `recv_line`,
  `send_line`, `close`. The entire `weft-transport` crate is **406 lines**. A
  transport is a genuinely pluggable, small thing here; nothing in `weft-core`
  knows what carries a line.

That last fact reframes the question. "Switching transports" is cheap in this
codebase. Switching *protocols* is not, and the two get conflated.

## Three different proposals

### (a) HTTP/3 as a carrier for the same line grammar

Concretely: WebTransport (or RFC 9220 extended `CONNECT`) opens a bidirectional
stream, and §4 lines ride inside it exactly as they do on QUIC stream 0.

- **Cost:** one more `ControlStream` impl. Days, not months. Nothing in the spec
  changes; the grammar, verbs, events, labels and error registry are untouched. One
  quinn endpoint can advertise both `h3` and `weft/1` and branch on the negotiated
  ALPN after the handshake, so it needs no second port.
- **Gain, browsers:** QUIC without WebSocket's TCP head-of-line blocking, and — see
  "stated precisely" below — it is the *only* thing that would let an ordinary HTTP/3
  proxy front the control plane, which is the one real operational complaint against
  the current design.
- **Gain, everything that dials in** (App Services, bridges, federation peers, the
  TUI), which is easy to overlook because there is no browser involved:
  - *Egress traversal.* Today a provider dials `<host>:4433/udp` with ALPN
    `weft/1` — a non-standard UDP port carrying an unrecognised ALPN, which is
    exactly what a PaaS, a corporate network, or UDP-filtering egress drops. The
    same session over WebTransport is `443/h3`, indistinguishable from web traffic.
    For an ecosystem where you do not control where a provider runs, that is the
    difference between connecting and not.
  - *Port collapse, or not needing it.* The obvious version — everything on 443 —
    is available only when nothing else owns 443, i.e. a standalone weftd
    (`[listen] https` + `[acme]`, which it already supports); behind Caddy it would
    wait on the passthrough PR.

    But collapsing onto 443 is not actually required, and the better design does
    not try to. Serve h3 + WebTransport on **its own UDP port** (8443, say),
    leave Caddy on 443, and **advertise the endpoint in `/.well-known/weft`** so no
    client needs to know the port. No contention, no dependency on anyone else's
    PR, and browsers connect to a non-443 WebTransport URL happily (subject only to
    the usual blocked-port list). What this does *not* change is the certificate:
    whoever terminates TLS holds it, so weftd still needs one — via `caddy_data` or
    `[acme]`. Port problem and certificate problem are separate, and only the
    passthrough moves the second one. It then becomes an optimisation rather than a
    blocker.

  Both are properties of the **transport**, so they arrive for anything that adopts
  the new `ControlStream` — no protocol or spec change. Worth stating explicitly
  because it is tempting to conclude the opposite: that appservices should become
  HTTP *request/response*. That is a different proposal and a bad one — it inverts
  the connection direction, so every provider then needs to be reachable, with a
  URL, a certificate, an inbound port and a second token. The Matrix side of
  `weft-matrix` is the worked example: `as_url`, the published port, the Caddy
  block, and five values hand-copied into `appservices/weft-matrix.yaml` all exist
  because Synapse pushes to a listening appservice. WebTransport gives the
  standards-compatible plumbing while keeping the outbound dial.
- **Browser support is no longer the catch.** WebTransport reached **Baseline in
  March 2026** when Safari 26.4 shipped: Chrome 97+, Edge 98+, Firefox 114+, Safari
  26.4+ on macOS and iOS. So it is not condemned to be a permanent third option —
  it could eventually retire the WebSocket fallback rather than sit beside it.
- **The catches that remain**, as of 2026-08:
  - *Proxy passthrough is not shipped.* Caddy's WebTransport reverse-proxy support
    is an experimental, unmerged PR (#7669, active through May 2026). Until it
    lands, WebTransport buys browsers QUIC and buys operators nothing — the
    certificate stays on weftd either way.
  - *The libraries are pre-1.0 and the protocol is still an IETF draft.*
    `wtransport` states it is not production-ready; `web-transport-quinn` is
    narrower and does the minimum for one session owning the whole connection,
    which happens to be exactly our shape. Both are quinn-based, so our QUIC and
    rustls stack is unaffected — but check that neither pulls a second rustls
    crypto provider (`cargo tree -i aws-lc-rs`); the workspace pins ring, and
    `cargo deny` bans a second.

Verdict: **build it, additively, now; make it the default later.** Adding a
transport costs one file. *Replacing* raw QUIC with it would break
`weft-appservice`, `weft-tui`, the federation dialer and `run_bridge_client`
simultaneously, for no operational gain until the proxy story is real — and it would
stake our only transport on a pre-1.0 implementation of a draft. Revisit the default
when Caddy's passthrough merges; that is the event that turns this from a client
nicety into the answer to the certificate coupling.

### (b) Replace the verb/event model with HTTP semantics

This is the real "drop the proprietary protocol": resources and methods instead of
`VERB params :trailing`, status codes instead of the §8 error registry, headers
instead of `@tags`.

Four things break, and none of them are cosmetic.

1. **Traffic shape.** Chat is *push-dominant*: §7 events outnumber commands by a
   wide margin, and they are unsolicited. HTTP's push story is SSE
   (unidirectional, text-only, no backpressure signal we can use) or long-polling.
   Both are strictly worse than an ordered bidirectional stream. We would be
   choosing the one protocol family whose central assumption — client asks, server
   answers — is inverted from our traffic.
2. **Two error models.** §8 has one code per condition, and invariant 1 depends on
   that: `NO-SUCH-TARGET` is deliberately identical for nonexistent, private,
   view-gated and expired, with a matching timing envelope. Mapping that onto
   status codes means either lying with 404 everywhere (losing the registry's
   precision) or carrying our codes in bodies anyway (two mechanisms for one job).
3. **Ordering.** §9.1 rests on a per-channel total order that the channel actor
   establishes and one ordered stream preserves end to end. HTTP/3 request streams
   are deliberately independent — that's the whole point of QUIC multiplexing — so
   we would have to pin everything to one stream and reinvent what we already have.
4. **Debuggability.** "netcat-debuggable" is a stated design goal (§4). HTTP/3 is
   binary, QPACK-compressed and TLS-only: you cannot netcat it, in principle, ever.
   Under (a) the grammar survives inside the stream; under (b) it's gone.

Verdict: **no.** This trades a protocol that fits the problem for one that doesn't,
and the spec gets *longer*, not shorter, because HTTP semantics become a second
thing to specify precisely.

### (c) HTTP/3 for federation (server-to-server) only

The strongest case, because this is where the operational pain actually is. §11
peering needs a custom dialer with an SSRF guard (invariant 13), key pinning, and
`/.well-known/weft` fetching — and that last one is *already* HTTPS. Matrix does
S2S over plain HTTPS and it is operationally boring, which is a compliment.

But WEFT federation is not request/response. A peering is a **long-lived session**
that forwards live events one hop (§11.4) — `run_bridge_client` is a pump, not an
RPC client. Putting that on HTTP means either polling (latency and waste) or a
persistent stream inside HTTP, which is proposal (a) again with extra framing.

What *would* fit HTTP: the discrete, idempotent exchanges — manifest
propose/accept, key fetch, `REPORT-FORWARD`. Splitting those out is defensible, but
it means two transports for one relationship and a second authentication path.

Verdict: **not worth the split.** Revisit if third parties start implementing
federation and the custom dialer proves to be the barrier.

## The strongest argument for switching, stated precisely

It is tempting to write "QUIC cannot be reverse-proxied". That is wrong, and being
wrong about it hides what the actual constraint is. Caddy, nginx and Cloudflare all
speak HTTP/3 perfectly well. Three distinct facts:

1. **ALPN.** Caddy's HTTPS server advertises `h3`, `h2`, `http/1.1`. weftd's client
   offers only `weft/1` (`weft-transport/src/quic.rs`: `alpn_protocols =
   vec![b"weft/1"]`). No overlap, so the TLS handshake fails before any byte flows —
   a proxy cannot opt into an ALPN it has no handler for.
2. **Nothing to route.** Granted the connection, `reverse_proxy` operates on HTTP
   requests: method, path, headers. Stream 0 carries §4 lines. There is no request.
3. **L4 forwarding works and is a different thing.** Plain UDP port-forwarding,
   `caddy-l4`, nginx `stream {}` with `udp` — these relay datagrams blindly, and
   QUIC's connection IDs make that viable for a single backend. But TLS still
   terminates at **weftd**, so weftd still holds the certificate. It changes nothing
   about the coupling, and buys nothing when weftd is already reachable on 4433/udp.

So the precise statement is: **weftd's QUIC isn't HTTP, so no HTTP proxy can
terminate it — and L7 termination is what would move the certificate.** That is what
caused the friction in `deploy/`: weftd holds its own certificate, so it reads
Caddy's `caddy_data` volume, so Caddy cannot be a separate Compose project, so
`[tls]` paths embed the domain and pointing them at the wrong subdomain silently
leaves QUIC on a self-signed placeholder while HTTPS looks healthy.

This raises the value of **(a)** above what that section credits. Expressing the
control plane as HTTP/3 — WebTransport, or extended `CONNECT` — is exactly the
condition under which Caddy's h3 support starts applying to us. It would not merely
be a browser performance nicety; it would let a standard proxy terminate the control
plane and delete the certificate-sharing coupling entirely. Still additive, still no
spec change: the §4 grammar rides inside the stream either way.

Two answers that need no protocol change at all:

- **weftd already has built-in ACME** (`[acme]`): it obtains and renews its own
  certificate with no proxy and no shared volume. The deployment doesn't use it
  because Caddy is there anyway — a default, not a protocol limit.
- If a proxy terminated HTTP/3 and spoke HTTP/1.1 to weftd behind it, which is what
  most deployments do, we would be paying HTTP's costs to get TCP's performance. The
  QUIC properties we want survive only end to end.

## Transport discovery in `/.well-known/weft` (worth doing regardless)

§10.2's document carries `protocol`, `network` and `signing-key` — nothing about how
to *reach* the server. A client is told the QUIC port by hand or assumes 4433, which
is why the port question above looks harder than it is. Advertising transports fixes
it for all of them at once:

```json
{
  "protocol": "weft/1",
  "network": "weft.example.com",
  "signing-key": "…",
  "transports": {
    "webtransport": "https://weft.example.com:8443/weft",
    "quic": "weft.example.com:4433",
    "ws": "wss://weft.example.com/ws"
  }
}
```

One fetch, every way in, and a deployment can move ports without reconfiguring a
single client. Same role Matrix's `/.well-known/matrix/server` plays for delegation;
we already have both the file (`weftd::wellknown`) and an SSRF-guarded fetcher
(`weftd::dialer::fetch_signing_key`).

Normative caveat for the spec text: the document is authoritative for its network and
may legitimately name any host, so a client **must** still check that the `network`
in `WELCOME` matches the name it asked for. Without that, a tampered well-known is a
silent redirect.

This is independent of WebTransport and useful on its own — it is what makes adding
*any* transport a deployment decision instead of a client-configuration change.

## Recommendation

1. **Keep the line grammar and the verb/event model.** They fit the traffic, and
   §4's text form is a real asset for debugging and for the IRC gateway's existence.
2. **Treat transports as the pluggable thing they already are.** WebTransport as a
   third `ControlStream` is additive and worth more than it first looks: standards-
   shaped `443/h3`-style reachability for everything that dials in, without giving up
   the outbound dial.
3. **Advertise transports in `/.well-known/weft`** (above). Do this first: it is what
   lets a new transport live on its own port without every client learning about it,
   and it removes the dependency on Caddy's passthrough for the port question.
4. **Fix the remaining deployment friction at the deployment layer.** Document
   `[acme]` as the no-proxy path more prominently; it is the actual answer to the
   certificate coupling, and it exists.
5. **Don't split federation.** Watch for third-party implementers; if the custom
   dialer is what stops them, that's the signal to revisit (c).

## What would change this

- Operators wanting the control plane behind their existing proxy, with one
  certificate and no shared volume → (a) is the answer, and the only one.
- A browser-only future where WebSocket is deprecated or throttled → (a) becomes
  necessary rather than nice.
- A third-party implementer reporting that the custom ALPN, not the semantics, is
  what blocks them → revisit (c) for the idempotent exchanges.
- Middlebox reality shifting so that neither QUIC-with-custom-ALPN nor WebSocket
  gets through, while HTTP/3 does → (a) becomes the primary transport, still
  without touching the grammar.

None of these is about HTTP/3 being a better protocol for chat. They're about
reach, which is exactly what a transport layer is for — and why the answer is to
add transports, never to move the semantics into one.
