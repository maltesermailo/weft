# Should WEFT's control plane be HTTP/3 instead?

Exploration, not a plan. Written 2026-08-07, prompted by the operational friction of
deploying a custom-ALPN QUIC service behind a reverse proxy.

**Conclusion up front: no — but adopt a WebTransport transport additively if browser
QUIC becomes worth having.** The reasoning matters more than the verdict, because
the three things people mean by "use HTTP/3" have completely different answers.

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
  changes; the grammar, verbs, events, labels and error registry are untouched.
- **Gain:** browsers get QUIC without WebSocket's TCP head-of-line blocking. Some
  middleboxes that pass HTTP/3 but drop unknown ALPN start working.
- **Catch:** WebTransport's browser support is narrower than WebSocket's, so it
  can't *replace* the WS fallback — it's a third option, which means three
  transports to test rather than two.

Verdict: **legitimate and additive.** More valuable than it first looks — see
"stated precisely" below: this is also the only thing that would let an ordinary
HTTP/3 proxy front the control plane, which is the one real operational complaint
against the current design. Not urgent, though: the WS fallback already delivers
browser reach.

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

## Recommendation

1. **Keep the line grammar and the verb/event model.** They fit the traffic, and
   §4's text form is a real asset for debugging and for the IRC gateway's existence.
2. **Treat transports as the pluggable thing they already are.** WebTransport as a
   third `ControlStream` is additive, needs no spec change, and is worth more than it
   first looks: it is what makes the control plane frontable by an ordinary HTTP/3
   proxy, which is the one real operational complaint against the current design.
3. **Fix the deployment friction at the deployment layer.** Document `[acme]` as
   the no-proxy path more prominently; it is the actual answer to "QUIC can't be
   proxied", and it exists.
4. **Don't split federation.** Watch for third-party implementers; if the custom
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
