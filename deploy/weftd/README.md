# The weftd stack

**weftd** (built with the embedded web client), **PostgreSQL**, **LiveKit** (voice)
and **Caddy** (automatic HTTPS + reverse proxy).

Caddy is in here rather than beside it because weftd needs the *certificate*, not
just the proxy: QUIC cannot be reverse-proxied — UDP + TLS 1.3 end to end — so weftd
terminates it itself and reads the certificate out of Caddy's `caddy_data` volume. A
shared named volume needs one project. It is still optional; see [Running without
Caddy](#running-without-caddy-http-on-the-host).

The Matrix bridge is a **separate** stack, [`../weft-matrix/`](../weft-matrix/README.md):
it needs nothing from in here and reaches weftd over its public name. The one thing it
needs from this side is a `Caddyfile` site block.

> **Setup walkthrough: [`../README.md`](../README.md)** — one ordered list across both
> stacks, so it cannot drift. This file is the reference behind it: prebuilt images,
> how the pieces connect, local dev, running without Caddy, standalone TLS, day-2.

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
Back up the `caddy_data` volume too — it holds the certificates *and* the ACME
account key.

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
docker build --platform linux/amd64 -f deploy/weftd/Dockerfile -t weft-weftd:latest .
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
docker build --platform linux/amd64 -f deploy/weftd/Dockerfile -t ghcr.io/<owner>/weft-weftd:latest .
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
  `weftd:8081` (the SPA, same-origin `/ws`, `/.well-known/weft`, `/media`, all plain
  HTTP behind Caddy) and `livekit.example.com` → `livekit:7880`, by service name on
  this project's network. It auto-obtains and renews the certificates. A site block
  for the Matrix bridge's homeserver is the exception — that upstream is in another
  project, reached through the host (`localhost:8008` from the host,
  `host.docker.internal:8008` from inside the container). See
  [`../weft-matrix/README.md`](../weft-matrix/README.md).
- **QUIC** (weftd `4433/udp`, for desktop/native clients) can't be proxied — a
  reverse proxy can't terminate it. weftd reads Caddy's certificate from the shared
  `caddy_data` volume, mounted read-only at `/data`, via the `[tls]` block. That
  sharing is the whole reason Caddy is in this project.
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

## Running without Caddy (HTTP on the host)

Use this when something else already terminates TLS (an external load balancer,
an existing nginx/Traefik, Cloudflare) or on a trusted LAN — you keep the Compose
stack but drop the bundled proxy and publish weftd's HTTP port straight on the
host. It's driven by `.env`, no compose-file edits:

```dotenv
# deploy/weftd/.env
COMPOSE_PROFILES=          # empty → do NOT start Caddy
WEFT_HTTP_BIND=0.0.0.0     # advertise weftd's HTTP port on every host interface
WEFT_HTTP_PORT=8081        # host port (change if 8081 is taken)
```

```bash
docker compose up -d       # postgres + livekit + weftd
curl http://<host>:8081/.well-known/weft
```

weftd now serves the web client, same-origin `/ws`, `/.well-known/weft`, and
`/media` as **plain HTTP** on `<host>:8081` — put your own TLS in front, or use it
as-is on a trusted network. Notes:

- **QUIC (4433/udp) still needs its own cert** — a proxy can't terminate it. Without
  Caddy writing certificates for it to read, weftd boots on a self-signed placeholder
  (native clients must opt into insecure mode, or give it a real cert — see
  [Standalone TLS](#standalone-tls-running-weftd-without-caddy) for the `[acme]` /
  `[tls]` options).
- **LiveKit voice signaling** was fronted by Caddy too; front it yourself (or set
  `[voice] enabled = false` in `weft.toml`) if you need voice in this mode.

The default (`COMPOSE_PROFILES=caddy`, `WEFT_HTTP_BIND=127.0.0.1`) keeps the full
proxied stack: weftd's HTTP stays loopback-only — Caddy reaches it by service name,
not through the host — and Caddy fronts it on 443.

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
