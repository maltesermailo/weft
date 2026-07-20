# Deploying weft

Two layers here: a **Docker Compose stack** that brings up the whole system
(weftd + PostgreSQL + LiveKit voice) from one `.env`, and a **TLS guide** for the
QUIC certificate you need in production. Start with Compose; read the TLS section
before going live.

## Docker Compose — full stack (web client + Postgres + LiveKit + Caddy)

Brings up everything: **weftd** built **with the embedded web client**
(`--features web-ui`), **PostgreSQL**, **LiveKit** (voice), and **Caddy** (auto
TLS + reverse proxy for HTTP/WebSocket). **No `.env`** — you edit plain config
files directly.

### 1. Point DNS at the box

Two names must resolve to this host (A/AAAA records), and ports 80 + 443 must be
reachable so Caddy can obtain Let's Encrypt certs:

- `weft.example.com` → the web client + API
- `livekit.weft.example.com` → LiveKit signaling (wss)

### 2. Edit the config (four files, no templating)

| File | Set |
|---|---|
| `weft.toml` | `network`, `operators`, `storage.url` password, `[voice.livekit]` `url`/keys, `[smtp]` |
| `livekit.yaml` | `keys:` — **must match** `[voice.livekit] api_key/api_secret` |
| `Caddyfile` | the two site addresses — **must match** `network` + LiveKit `url` |
| `docker-compose.yml` | `POSTGRES_PASSWORD` — **must match** `weft.toml` `storage.url` |

Generate strong secrets: `openssl rand -hex 32` (LiveKit secret + Postgres
password). The "MUST MATCH" pairs are duplicated because there's no `.env` — keep
them in sync.

### 3. Up

```bash
cd deploy
docker compose up -d --build
```

Open `https://weft.example.com` — the web client is served by weftd (embedded),
talks WEFT over the same-origin `wss://weft.example.com/ws`. Register the handle
you set as `operators[0]`; it holds every capability at `*` (§11.3).

### How the pieces connect

- **Caddy** terminates public TLS (443) and reverse-proxies `weft.example.com` →
  `weftd:8081` (the SPA, `/ws`, `/.well-known/weft`, `/media`) and
  `livekit.example.com` → `livekit:7880`. It auto-obtains + renews the certs.
- **QUIC** (weftd `4433/udp`, for desktop/native clients) can't be proxied. weftd
  reads Caddy's LE cert from the **shared `caddy_data` volume** for QUIC — enable
  the `[tls]` block in `weft.toml` (path includes your domain). Boot tolerates the
  cert not existing yet (self-signed placeholder → hot-swaps when Caddy writes it).
- **LiveKit** signaling rides Caddy (wss); its **media** is UDP `50000-50020`
  direct to the host (open these on your firewall; `use_external_ip` advertises
  the host IP). weftd's own Room-API calls go internal to `http://livekit:7880`
  (`api_url`).
- **Federated (cross-network) voice** needs the libwebrtc relay driver, not in
  this image by design (`docs/voice-livekit-plan.md`); same-network voice works
  out of the box.

Email verification (§10.5): set `[smtp] enabled = true` + the fields; otherwise
weftd logs the code. Postgres data persists in the `pgdata` volume — back it up.

---

## Real TLS without Compose / for QUIC directly

The Compose stack above already gives weftd its QUIC cert via Caddy's shared
volume. If you run weftd **standalone** (no Caddy), it must hold the QUIC cert
itself — **UDP + TLS 1.3, end to end**, which a proxy can't terminate. Two ways:

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
