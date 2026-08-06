# Deployments

One Compose stack, in [`weftd/`](weftd/README.md): weftd (with the embedded web client),
PostgreSQL, LiveKit (voice) and Caddy (automatic HTTPS). The optional **Matrix
bridge** is a *profile* of that same stack, not a stack of its own.

Follow the walkthrough below start to finish. It is the whole happy path; the two
reference docs go deeper where you need it:

- [`weftd/README.md`](weftd/README.md) — running without Caddy, standalone TLS,
  shipping a prebuilt image, local dev with no domain, day-2 operations.
- [`weftd/MATRIX.md`](weftd/MATRIX.md) — what the bridge is, how to verify it, how
  to operate and remove it.

---

## Part 1 — the weftd stack

### 0. What you need

- A Linux server with Docker + Docker Compose, and some RAM to spare.
- A **domain you control** — Let's Encrypt validates it over ports 80/443. This
  guide uses `example.com`; substitute yours everywhere.

### 1. Point DNS at the server

Two **A records** (AAAA too if you have IPv6) → the server's public IP:

```
weft.example.com       →  203.0.113.10
livekit.example.com    →  203.0.113.10
```

### 2. Open the firewall

| Port        | Proto | For                                                        |
| ----------- | ----- | ---------------------------------------------------------- |
| 80, 443     | TCP   | Caddy (HTTP + HTTPS)                                       |
| 443         | UDP   | HTTP/3 (optional)                                          |
| 4433        | UDP   | weftd QUIC, desktop/native clients. **Cannot be proxied.** |
| 50000-50020 | UDP   | LiveKit voice media                                        |

### 3. Get the code and generate two secrets

```sh
git clone <your-weft-repo> weft
cd weft/deploy/weftd

openssl rand -hex 32   # → the Postgres password
openssl rand -hex 32   # → the LiveKit secret
```

Keep both strings to hand. Each goes in **two** files — they are not sourced from
`.env`, so the pair has to match.

### 4. Edit four files

**`weft.toml`**

```toml
network   = "weft.example.com"        # your domain

[tls]                                 # replace the domain in BOTH paths
cert = "/data/caddy/certificates/acme-v02.api.letsencrypt.org-directory/weft.example.com/weft.example.com.crt"
key  = "/data/caddy/certificates/acme-v02.api.letsencrypt.org-directory/weft.example.com/weft.example.com.key"

[storage]
url = "postgres://weft:POSTGRES-PASSWORD@postgres:5432/weft"

[voice.livekit]
url        = "wss://livekit.example.com"
api_key    = "devkey"                 # fine as-is
api_secret = "LIVEKIT-SECRET"
```

**`livekit.yaml`** — `keys: { devkey: LIVEKIT-SECRET }` (the key *name* is
`api_key` above, its value is `api_secret`).

**`Caddyfile`** — the two site addresses:

```caddyfile
weft.example.com    { reverse_proxy weftd:8081 }
livekit.example.com { reverse_proxy livekit:7880 }
```

**`docker-compose.yml`** — `POSTGRES_PASSWORD:` on the `postgres` service.

Before moving on, check each value appears in both of its places:

| Value             | Both of                                                                  |
| ----------------- | ------------------------------------------------------------------------ |
| Postgres password | `weft.toml` `[storage] url` · `docker-compose.yml` `POSTGRES_PASSWORD`   |
| LiveKit secret    | `weft.toml` `api_secret` · `livekit.yaml` `keys:`                        |
| Your domain       | `weft.toml` (`network`, both `[tls]` paths, LiveKit `url`) · `Caddyfile` |

### 5. Start it

```sh
docker compose up -d          # add --build to compile locally instead of pulling
docker compose logs -f caddy  # → certificates obtained, for both names
docker compose logs -f weftd  # → "same-origin /ws mounted", then "weftd listening"
```

weftd boots on a self-signed placeholder for QUIC and **hot-swaps** Caddy's real
certificate in once Caddy has it (within a minute) — no restart.

### 6. Create the first operator

```sh
docker compose exec weftd weftd admin create admin --password '<a-strong-password>'
```

Registers the account **and** flags it operator (§11.3 — every capability at `*`,
and access to `/admin`). To promote an account that already exists, use
`weftd admin grant <account>`.

### 7. Log in

Open **`https://weft.example.com`**. The web client is served by weftd and
connects back over `wss://weft.example.com/ws`. Create a voice channel and join
from two browsers to check LiveKit.

**Stop here if you don't bridge to Matrix.** Everything below is optional.

---

## Part 2 — the Matrix bridge (optional)

Adds a **companion homeserver** (Synapse — dedicated to the bridge, nobody signs
up on it) and the **bridge daemon**. Same Compose project, because the daemon has
to reach `weftd:4433` and `weftd:8081`.

The order below is forced by a circular dependency: the daemon's key must exist
before weftd can pin it, and weftd must pin it before the daemon may connect.
So the profile goes on **last**.

### 1. Edit the bridge's config

Still in `deploy/weftd`:

- **`weft-matrix.toml`** — `[matrix] domain` (the homeserver's name, e.g.
  `matrix.example.com`), `as_token` and `hs_token` (**change both** —
  anyone holding `hs_token` can inject events as any Matrix user), and `admins`
  (MXIDs allowed to run the `!weft` console) if you want it.
- **`homeserver.yaml`** — `server_name` = the *same* domain, and the Synapse
  Postgres password.
- **`initdb/10-matrix.sql`** — the same Synapse password.
- **`.env`** — `MATRIX_BIND` / `MATRIX_PORT`, where the homeserver's port is
  published.

### 2. Create the two databases

`initdb/` only runs on an *empty* Postgres volume, and yours has data from Part 1,
so do it by hand — once:

```sh
docker compose exec -T postgres psql -U weft -d postgres < initdb/10-matrix.sql
```

(Synapse needs a `C`-collation database, which `POSTGRES_DB` cannot express; that
is what the script is for. `weftmatrix` is the daemon's own store.)

### 3. Create the adapter key

```sh
docker compose run --rm bridge keygen /etc/weft/weft-matrix.toml
```

Prints the public key. Idempotent — it only creates the file if absent.

### 4. Pin the key in `weft.toml`

```toml
[[plugin.remote]]
id      = "matrix"
key     = "<the key from step 3>"
bot     = "matrixbot"     # optional: weftd provisions a native bot account
schemes = ["matrix"]
```

```sh
docker compose up -d weftd
```

Until this matches, the bridge is refused with `AUTH-FAILED` — by design.

### 5. Generate the appservice registration

```sh
docker compose run --rm registration     # → /appservices/weft-matrix.yaml
docker compose run --rm synapse-keys     # → Synapse's signing key
```

The registration is *generated* from `weft-matrix.toml` into a volume Synapse
mounts read-only, so the tokens exist in one file rather than two that drift.
Re-run it (and restart Synapse) after changing the tokens, the domain or the
puppet prefix.

### 6. Turn the profile on

```sh
# .env
COMPOSE_PROFILES=caddy,matrix
```

```sh
docker compose up -d
docker compose logs -f bridge     # → the adapter pubkey, then "connected to weftd"
```

### 7. Federation TLS (only if real Matrix users elsewhere should reach it)

Add to the `Caddyfile`:

```caddyfile
matrix.example.com {
	reverse_proxy synapse:8008
	handle /.well-known/matrix/* {
		header Content-Type application/json
		respond `{"m.server": "matrix.example.com:443"}`
	}
}
```

Without this the bridge still works for rooms **on the companion server** — enough
to exercise both traffic directions, media, DMs and typing.

Verification steps (consume a room, project a namespace, the `!weft` console) are
in [`weftd/MATRIX.md`](weftd/MATRIX.md#verifying-it-works). Note the warning there:
**the bridge has never been run against a real homeserver.**

---

## Day 2

```sh
docker compose logs -f weftd      # tail
docker compose up -d weftd        # apply a weft.toml edit
docker compose pull && docker compose up -d   # update to the latest images
docker compose down               # stop; data stays in the named volumes
```

**Back up** the `pgdata` volume (the database), `weftd_media` (uploaded files),
weftd's signing key from `[identity] key_file`, and — with the bridge — the
`matrix_keys` volume, which holds the one thing the daemon cannot rebuild.
