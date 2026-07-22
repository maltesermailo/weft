# Deploying weft

A **Docker Compose stack** that brings up the whole system — **weftd** (built with
the embedded web client), **PostgreSQL**, **LiveKit** (voice), and **Caddy**
(automatic HTTPS + reverse proxy) — plus a **standalone-TLS reference** at the
bottom for running weftd without Caddy.

The tutorial below is the happy path. It assumes a **Linux server with a public IP
and a domain you control** (needed for automatic Let's Encrypt certificates). For
laptop-only hacking with no domain, see [Just trying it locally?](#just-trying-it-locally-no-domain).

---

## Tutorial: full deployment

### 0. Prerequisites

- A server (VPS) with Docker + Docker Compose installed, and some RAM to spare —
  the first build compiles weftd **and** the browser client.
- A domain you control. This guide uses `example.com`; swap in yours.

### 1. Point DNS at the server

Create two **A records** (AAAA too if you have IPv6) → your server's public IP:

```
weft.example.com       →  203.0.113.10
livekit.example.com    →  203.0.113.10
```

Both must resolve publicly — Let's Encrypt validates them over ports 80/443. Any
names work (e.g. `chat.mydomain.com` + `livekit.mydomain.com`).

### 2. Open the firewall

Allow these on the host / cloud security group:

| Port | Proto | For |
|---|---|---|
| 80, 443 | TCP | Caddy (HTTP + HTTPS) |
| 443 | UDP | HTTP/3 (optional) |
| 4433 | UDP | weftd QUIC (desktop/native clients) |
| 50000–50020 | UDP | LiveKit voice media |

### 3. Get the code and generate secrets

```bash
git clone <your-weft-repo> weft
cd weft/deploy

openssl rand -hex 32   # → Postgres password
openssl rand -hex 32   # → LiveKit secret
```

Keep those two strings handy — each goes in **two** places (there's no `.env`, so
matching values are duplicated; keep them in sync).

### 4. Edit the four config files

**`weft.toml`**

```toml
network   = "weft.example.com"        # ← your domain
operators = []                        # ← operators live in Postgres now; use the CLI

[tls]                                 # ← replace weft.example.com in BOTH paths
cert = "/data/caddy/certificates/acme-v02.api.letsencrypt.org-directory/weft.example.com/weft.example.com.crt"
key  = "/data/caddy/certificates/acme-v02.api.letsencrypt.org-directory/weft.example.com/weft.example.com.key"

[storage]
url = "postgres://weft:PASTE-POSTGRES-PASSWORD@postgres:5432/weft"

[voice.livekit]
url        = "wss://livekit.example.com"   # ← your LiveKit subdomain
api_key    = "devkey"                       # fine as-is
api_secret = "PASTE-LIVEKIT-SECRET"
```

Leave `[listen] web = true` — it serves the web client and its same-origin `/ws`.

**`livekit.yaml`**

```yaml
keys:
  devkey: PASTE-LIVEKIT-SECRET          # ← same secret as weft.toml api_secret
```

(LiveKit's format is `<key-name>: <secret>`, so `devkey` here = `api_key` in
weft.toml, and its value = `api_secret`.)

**`Caddyfile`**

```
weft.example.com    { reverse_proxy weftd:8081 }
livekit.example.com { reverse_proxy livekit:7880 }
```

**`docker-compose.yml`** (the `postgres` service)

```yaml
POSTGRES_PASSWORD: PASTE-POSTGRES-PASSWORD   # ← same as weft.toml storage.url
```

**Match check** — the same value must appear in each pair:

| Value | Places |
|---|---|
| Postgres password | `weft.toml` `storage.url` · `docker-compose.yml` `POSTGRES_PASSWORD` |
| LiveKit secret | `weft.toml` `api_secret` · `livekit.yaml` `keys:` |
| Your domain | `weft.toml` (`network`, `[tls]` paths, LiveKit `url`) · `Caddyfile` |

### 5. Build and start

```bash
docker compose up -d --build
```

The **first build is slow** — it compiles weftd (release) and the SvelteKit +
wasm client. Later starts are instant.

### 6. Watch it come up

```bash
docker compose logs -f caddy    # → certificates obtained for both domains
docker compose logs -f weftd    # → "same-origin /ws mounted", then "weftd listening"
```

weftd boots immediately on a self-signed placeholder for QUIC and **hot-swaps** in
Caddy's real certificate once Caddy finishes (within a minute) — no restart
needed.

### 7. Log in

Open **`https://weft.example.com`**. The web client loads (served by weftd) and
connects over `wss://weft.example.com/ws`.

**Create your first operator** (§11.3 — holds every capability at `*`, and
unlocks `/admin`). Operator status lives in Postgres now, so use the CLI:

```bash
docker compose exec weftd weftd admin create admin --password '<a-strong-password>'
# later: grant <account> / revoke <account> / list
```

This registers the `admin` account **and** flags it operator; log in with it in
the web client (or at `/admin` with `[admin] enabled = true`). To promote an
account someone already registered, use `weftd admin grant <account>` instead.

### 8. Try voice

Create a voice channel and join from two browsers → LiveKit carries the audio (via
`wss://livekit.example.com`); server-side mute/kick works through weftd.

Federated (cross-network) voice needs the libwebrtc relay driver, which is **not**
in this image by design (see `docs/voice-livekit-plan.md`) — same-network voice
works out of the box.

### 9. (Optional) email verification

To actually mail verification codes (§10.5), set in `weft.toml`:

```toml
[smtp]
enabled  = true
host     = "smtp.your-provider.com"
port     = 587
username = "…"
password = "…"
from     = "noreply@example.com"
```

Left disabled, weftd records claims and logs the code instead of sending it.

---

## Day-2 operations

```bash
docker compose logs -f weftd      # tail logs
docker compose restart weftd      # apply an edit to weft.toml
docker compose up -d --build      # rebuild after pulling new code
docker compose down               # stop (data persists in named volumes)
```

**Back up** the `pgdata` volume (your database) and the `weftd_media` volume
(uploaded images/files — content-addressed blobs). If you set `[identity]
key_file` in `weft.toml`, back that up too — it's your network's signing key.

---

## Prebuilt image (build on a fast machine, run on the server)

The first build compiles Rust + the web client — slow, and RAM-hungry, on a small
VPS. Build the image on your desktop and ship it instead.

**⚠ Architecture must match the server.** If you build on Apple Silicon / arm64
but the server is x86-64, add `--platform linux/amd64` (Docker Desktop /
`buildx` cross-builds it).

### Option A — save / load a tarball (no registry)

On your desktop, in the repo:

```bash
# Cross-build for the server's arch if it differs from yours:
docker build --platform linux/amd64 -f deploy/Dockerfile -t weft-weftd:latest .
docker save weft-weftd:latest | gzip > weftd-image.tar.gz
scp weftd-image.tar.gz  you@server:~/weft/deploy/
```

On the server:

```bash
cd ~/weft/deploy
gunzip -c weftd-image.tar.gz | docker load     # loads weft-weftd:latest
docker compose up -d                            # reuses it — no rebuild
```

`docker compose up` (without `--build`) uses the loaded `weft-weftd:latest`
image; only postgres/livekit/caddy are pulled.

### Option B — GitHub Container Registry (ghcr.io)

**Automated (recommended):** `.github/workflows/docker.yml` builds + pushes
`ghcr.io/<owner>/weft-weftd` on every push to the default branch (`:latest`), on
`v*` tags (`:1.2.3`), and on manual dispatch — using the built-in `GITHUB_TOKEN`
(no secrets to set up). After the first run, make the package **public** in your
GitHub *Packages* settings so servers can pull it without logging in.

**Manual:** create a PAT with `write:packages`, then from your machine:

```bash
echo "$GHCR_TOKEN" | docker login ghcr.io -u <github-username> --password-stdin
docker build --platform linux/amd64 -f deploy/Dockerfile -t ghcr.io/<owner>/weft-weftd:latest .
docker push ghcr.io/<owner>/weft-weftd:latest
```

**On the server**, point the weftd service at the registry image in
`docker-compose.yml` and drop its `build:` block:

```yaml
  weftd:
    image: ghcr.io/<owner>/weft-weftd:latest
    # (remove the build: block)
```

Then (with `docker login ghcr.io` first if the package is private):

```bash
docker compose pull weftd && docker compose up -d
```

---

## How the pieces connect

- **Caddy** terminates public TLS (443) and reverse-proxies `weft.example.com` →
  `weftd:8081` (the SPA, same-origin `/ws`, `/.well-known/weft`, `/media`, all
  plain HTTP behind Caddy) and `livekit.example.com` → `livekit:7880`. It
  auto-obtains + renews the certs.
- **QUIC** (weftd `4433/udp`, for desktop/native clients) can't be proxied — a
  reverse proxy can't terminate it. weftd reads Caddy's cert from the shared
  `caddy_data` volume (mounted read-only at `/data`) via the `[tls]` block.
- **LiveKit** signaling rides Caddy (wss); its **media** is UDP `50000-50020`
  direct to the host. weftd's own Room-API calls (mute/kick) go internal to
  `http://livekit:7880` (`[voice.livekit] api_url`).
- **The web client** connects to same-origin `wss://weft.example.com/ws` — served
  on weftd's HTTP listener when `[listen] web = true` (the image is built
  `--features web-ui`, so the SPA is embedded).

---

## Just trying it locally? (no domain)

The Caddy/Let's Encrypt path needs a real public domain. For laptop hacking, skip
Docker and run the dev loop directly:

```bash
cargo run -p weftd            # localhost dev network (memory store, self-signed)
cd client && pnpm dev         # web client against it
```

That's the fast inner loop; the Compose stack above is for a real deployment.

---

## Standalone TLS (running weftd without Caddy)

The Compose stack already gives weftd its QUIC cert via Caddy's shared volume. If
you run weftd **standalone**, it must hold the QUIC cert itself — **UDP + TLS 1.3,
end to end**, which a proxy can't terminate. Two ways:

### Option A — built-in ACME (simplest, no proxy)

weftd obtains + renews its own Let's Encrypt certificate for QUIC. Validation is
HTTP-01, so weftd's HTTP listener must be reachable by the CA on **port 80**.

```toml
[listen]
quic = "0.0.0.0:4433"
http = "0.0.0.0:80"        # must be reachable by Let's Encrypt on :80

[acme]
enabled   = true
domains   = ["weft.example.com"]
email     = "admin@example.com"
staging   = false          # true while testing (untrusted certs, high limits)
cache_dir = "/var/lib/weft/acme"
```

Boots immediately (cached cert or self-signed placeholder), gets the real cert
within seconds, swaps it into QUIC with no restart, renews ~30 days before expiry.

### Option B — shared cert file + certbot

Let certbot obtain the cert to disk; weftd reads it for QUIC and **hot-reloads**
on change.

```bash
certbot certonly --standalone -d weft.example.com
```

```toml
[tls]
cert = "/etc/letsencrypt/live/weft.example.com/fullchain.pem"
key  = "/etc/letsencrypt/live/weft.example.com/privkey.pem"
```
