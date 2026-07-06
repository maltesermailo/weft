# Deploying weftd with real TLS

weftd's QUIC transport is **UDP + TLS 1.3, end to end** — a reverse proxy can't
terminate it, so **weftd must hold the QUIC certificate itself.** There are two
supported ways to give it one; pick one.

## Option A — built-in ACME (simplest, no proxy)

weftd obtains + renews its own Let's Encrypt certificate and uses it for QUIC.
Validation is HTTP-01, so weftd's HTTP listener must be reachable by the CA on
**port 80**.

```toml
# weftd.toml
[listen]
quic = "0.0.0.0:4433"
http = "0.0.0.0:80"        # must be reachable by Let's Encrypt on :80

[acme]
enabled  = true
domains  = ["weft.example.com"]
email    = "admin@example.com"
staging  = false           # true while testing (untrusted certs, high limits)
cache_dir = "/var/lib/weft/acme"
```

weftd boots immediately (on the cached cert, or a self-signed placeholder), gets
the real cert within seconds, swaps it into the live QUIC endpoint with no
restart, and renews ~30 days before expiry. The account + cert are cached under
`cache_dir` so restarts don't re-issue.

Add Caddy (below) only if/when you also want to serve the web panel or other
HTTP over 443.

## Option B — shared cert file + Caddy/certbot (if you already run a proxy)

Let something else (certbot, or Caddy) obtain the LE cert to disk; weftd reads it
for QUIC and **hot-reloads** it when it changes (renewals apply with no restart).

```bash
certbot certonly --standalone -d weft.example.com   # writes /etc/letsencrypt/live/...
```

```toml
# weftd.toml — point at the LE files; weftd polls their mtime and reloads.
[tls]
cert = "/etc/letsencrypt/live/weft.example.com/fullchain.pem"
key  = "/etc/letsencrypt/live/weft.example.com/privkey.pem"
```

Front the HTTP surface (well-known + future panel) with `deploy/Caddyfile`
(auto-LE for the HTTP domain). Caddy handles TCP/HTTPS; weftd handles QUIC with
the shared cert. If you want Caddy itself to manage the shared file, use its
[`tls`](https://caddyserver.com/docs/caddyfile/directives/tls) cert-management or
a certbot deploy-hook — the key point is that weftd's `[tls]` paths must contain
the current cert, and weftd reloads on change.

## Which to choose

- **One box, no other HTTP:** Option A. Least moving parts.
- **Already running Caddy/nginx, or want the panel on 443 now:** Option B.

Either way, **back up** `[identity] key_file` (your network's signing key) and,
for Postgres, the database.
