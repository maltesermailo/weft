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

### 1. Choose the homeserver's name, and point DNS at it

Synapse's `server_name` is the suffix of every MXID (`@weft_<ulid>:<server_name>`)
and it is **permanent** — baked into every event the server signs, so changing it
later invalidates all of them. It is *not* the hostname the server runs on: remote
servers fetch `/.well-known/matrix/server` from `https://<server_name>/` and that
file tells them where to actually connect. So there are two shapes.

| `server_name`        | MXIDs                        | Well-known served by                     | Federation lands on  |
| -------------------- | ---------------------------- | ---------------------------------------- | -------------------- |
| `matrix.example.com` | `@weft_…:matrix.example.com` | Synapse (`serve_server_wellknown: true`) | `matrix.example.com` |
| `example.com` (apex) | `@weft_…:example.com`        | Caddy, in an apex site block             | `matrix.example.com` |

**Direct (subdomain)** is what the shipped files assume: no apex record, no
delegation, nothing to configure beyond the site block. One record to add:

```
matrix.example.com     →  203.0.113.10
```

**Delegated (apex)** is worth it when you want Matrix identities on your main
domain — `@weft_…:example.com` rather than `@weft_…:matrix.example.com`. Recipe
below. Two records to add, the apex among them, because Caddy needs a certificate
for it:

```
matrix.example.com     →  203.0.113.10
example.com            →  203.0.113.10
```

#### Delegated (apex) — the full recipe

Note that the apex block does **not** have to be weftd's. weftd can live on its own
subdomain: `/.well-known/weft` is fetched at `https://<network>/`, so weftd's host
and its `network` are the same string, and that is independent of Matrix's
`server_name`. The apex then serves exactly one file.

**`Caddyfile`** — add an apex block, and keep the `matrix.…` one:

```caddyfile
example.com {
	handle /.well-known/matrix/* {
		header Content-Type application/json
		respond `{"m.server": "matrix.example.com:443"}`
	}

	# Everything else on the apex. Drop this block if the apex already serves a
	# site from elsewhere — but then THAT server has to answer the well-known
	# above, because the apex is the Matrix authority either way.
	handle {
		redir https://weft.example.com{uri}
	}
}

matrix.example.com {
	reverse_proxy synapse:8008
}
```

Both apex routes are `handle` blocks deliberately: those are mutually exclusive and
first-match, so the well-known cannot be swallowed by the catch-all. A bare
directive next to a `handle` leaves that to Caddy's internal directive ordering,
which is not something to bet federation on.

**`homeserver.yaml`**

```yaml
server_name: "example.com"                        # the apex — the MXID suffix
public_baseurl: "https://matrix.example.com/"     # where it actually answers
serve_server_wellknown: false                     # Caddy owns that file now
```

`serve_server_wellknown: false` is hygiene rather than necessity. Synapse can only
ever name *itself* — the endpoint returns `{"m.server": "<server_name>:443"}` —
so with an apex `server_name` its copy would say `example.com:443`, published on
`matrix.example.com`, pointing back at a host that serves only the JSON. Nothing
queries it (discovery fetches the well-known for `server_name` only, then resolves
the returned host by port/SRV/A — it does not recurse), but it is a confidently
wrong answer to find while troubleshooting, and it becomes actively wrong the day
the apex is re-pointed.

**`weft-matrix.toml`** — `[matrix] domain = "example.com"`, the apex, matching
`server_name` exactly. Not weftd's network name; the two identity spaces are
separate and only this one appears in MXIDs.

<details>
<summary>Variant: let the apex front the federation API instead</summary>

If you would rather proxy the well-known than hard-code it — the common nginx
pattern — the apex must also front `/_matrix/*`, because the file Synapse returns
names the apex and remote servers will then send federation traffic there:

```caddyfile
example.com {
	handle /.well-known/matrix/* { reverse_proxy synapse:8008 }
	handle /_matrix/* { reverse_proxy synapse:8008 }
	handle { redir https://weft.example.com{uri} }
}
```

with `serve_server_wellknown: true` and `public_baseurl: "https://example.com/"`.
Equally correct; it just puts the whole federation surface on the apex instead of
one static file, and the certificate remote servers validate becomes the apex's.

</details>

### 2. Edit the bridge's config

Still in `deploy/weftd`:

- **`weft-matrix.toml`** — `[matrix] domain` = the name you just chose,
  `as_token` and `hs_token` (**change both** — anyone holding `hs_token` can
  inject events as any Matrix user), `[daemon] database_url` with the **same
  Postgres password as `weft.toml`** (the bridge opens its own store before it
  connects to anything, so a stale placeholder here shows up as
  `password authentication failed for user "weft"` in a reconnect loop and
  nothing else), and `admins` (MXIDs allowed to run the `!weft` console) if you
  want it.
- **`homeserver.yaml`** — `server_name` = **exactly** the same string, and the
  Synapse Postgres password.
- **`initdb/10-matrix.sql`** — the same Synapse password.
- **`Caddyfile`** — uncomment the `matrix.…` site block and set your domain (plus
  the apex block, on the delegated shape). This is what makes federation work over
  **443**, so port 8448 never has to be opened.
- **`.env`** — `MATRIX_BIND` / `MATRIX_PORT`, where the homeserver's port is
  published on the host (keep it on loopback; Caddy fronts it).

### 3. Create the two databases

`initdb/` only runs on an *empty* Postgres volume, and yours has data from Part 1,
so do it by hand — once:

```sh
docker compose exec -T postgres psql -U weft -d postgres < initdb/10-matrix.sql
```

(Synapse needs a `C`-collation database, which `POSTGRES_DB` cannot express; that
is what the script is for. `weftmatrix` is the daemon's own store.)

### 4. Create the adapter key

```sh
docker compose run --rm --build adapter-key
```

Prints the public key. Idempotent — it only creates the file if absent. `--build`
compiles the bridge image locally; drop it once you are pulling a published one.

(Not `run bridge keygen …`: `run` starts the target's dependencies, and the bridge
depends on Synapse, which cannot start until step 6.)

### 5. Pin the key in `weft.toml`

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

### 6. Turn the profile on

```sh
# .env
COMPOSE_PROFILES=caddy,matrix
```

```sh
docker compose up -d --build
docker compose logs -f bridge     # → the adapter pubkey, then "connected to weftd"
```

Two things happen on their own here, so there is no step for either:

- **The appservice registration** is derived from `weft-matrix.toml` into the volume
  Synapse mounts, by a one-shot Synapse waits for. Every `up` re-derives it, so a
  changed token cannot leave a stale registration behind, and a missing one cannot
  leave Synapse crash-looping on `FileNotFoundError`.
- **Synapse's signing key** is written on first boot into `synapse_data`.
  **Back that volume up** — the key is the identity remote homeservers pin, and
  losing it means every server you have federated with rejects your events.

### 7. Check that federation works

```sh
docker compose logs synapse | grep -i appservice   # a bad registration is fatal, so
                                                   # a running Synapse means it loaded
curl https://matrix.example.com/.well-known/matrix/server
# → {"m.server":"matrix.example.com:443"}
```

**That `:443` is what keeps port 8448 shut.** A remote server discovers us in this
order: `/.well-known/matrix/server` → SRV record → `<server_name>:8448` as a last
resort. The well-known short-circuits it to 443, which is why nothing here listens
on 8448. If the curl above comes back **without** a port, remote servers will fall
back to 8448 and federation breaks — fix it by either publishing 8448 through
Caddy:

```caddyfile
matrix.example.com:8448 {
	reverse_proxy synapse:8008
}
```

(then open 8448/TCP in the firewall), or by serving the delegation from Caddy with
the port spelled out, instead of letting Synapse answer it:

```caddyfile
handle /.well-known/matrix/server {
	header Content-Type application/json
	respond `{"m.server": "matrix.example.com:443"}`
}
```

Then run your `server_name` through
**<https://federationtester.matrix.org>** — it checks DNS, delegation, the
certificate and the signing key the way a remote homeserver would, and names
whatever is wrong. It must come back green before a Matrix user on another server
can join anything here.

The remaining verification (consume a room, project a namespace, the `!weft`
console) is in [`weftd/MATRIX.md`](weftd/MATRIX.md#verifying-it-works). Note the
warning there: **the bridge has never been run against a real homeserver.**

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
