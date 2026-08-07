# The Caddy stack

TLS and reverse proxy for the other two: automatic Let's Encrypt on 80/443 in front
of weftd's HTTP surface, LiveKit's signalling, and — optionally — the Matrix
bridge's homeserver.

> **Setup walkthrough: [`../README.md`](../README.md)** — one ordered list across
> all three stacks. This file is the reference behind it.

Its own Compose project so it can be **replaced wholesale**. If you already run
nginx, Traefik, HAProxy or a cloud load balancer, don't bring this up: point yours
at the same published ports, and read [What it has to
provide](#what-it-has-to-provide) for the two non-obvious requirements.

## How it reaches the other stacks

Through the **host**, not a shared Docker network. `extra_hosts` maps
`host.docker.internal` to the host gateway, and the upstreams are ports the other
stacks publish:

| Site | Upstream | Published by |
| --------------------- | ------------------------------- | -------------------- |
| `weft.example.com` | `host.docker.internal:8081` | `../weftd` |
| `livekit.example.com` | `host.docker.internal:7880` | `../weftd` |
| `matrix.example.com` | `host.docker.internal:8008` | `../weft-matrix` |

Those ports therefore have to be bound on an address a container can see —
`0.0.0.0`, not `127.0.0.1`. They are **plain HTTP**: firewall them, or bind the
Docker bridge address instead (see `../weftd/.env`).

## What it has to provide

Two things any replacement proxy must also do:

1. **WebSocket upgrades on `/ws`.** `reverse_proxy` does this transparently; some
   proxies need it spelled out. Without it the web client cannot connect at all.
2. **A certificate weftd can read.** QUIC (`4433/udp`) cannot be reverse-proxied —
   it is UDP + TLS 1.3 end to end — so weftd holds the certificate itself and reads
   it out of `./data`, bind-mounted read-only into that container. That is the one
   coupling between these stacks that isn't a port.

   `./data` is a **bind mount rather than a named volume** precisely so another
   Compose project can read it. If you replace Caddy, either put the certificate
   where `weftd/weft.toml`'s `[tls]` block expects it, or switch weftd to its own
   ACME — `../weftd/README.md` → Standalone TLS.

Caddy stores certificates at a deterministic path
(`data/caddy/certificates/acme-v02.api.letsencrypt.org-directory/<domain>/`), which
is what `[tls]` in `weft.toml` points at. weftd hot-reloads the file, so renewals
apply with no restart.

## Operating

```sh
docker compose up -d
docker compose logs -f            # → "certificate obtained" per domain
docker compose restart            # after editing the Caddyfile
```

**Back up `./data`.** It holds the certificates *and* the ACME account key.

## Not proxied

- **weftd's QUIC**, `4433/udp` — see above.
- **LiveKit's RTC media**, `50000-50020/udp` — direct to the host by design; only
  LiveKit's signalling goes through here.
- **Matrix federation on 8448** — nothing listens there. Delegation sends remote
  servers to 443 instead; `../weft-matrix/README.md` explains the mechanism.
