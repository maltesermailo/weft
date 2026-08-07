# Deployments

**Two independent Compose stacks.**

| Directory                               | What it runs                                     | Needed?                  |
| --------------------------------------- | ------------------------------------------------ | ------------------------ |
| [`weftd/`](weftd/README.md)             | weftd + PostgreSQL + LiveKit + Caddy (TLS)       | yes — this is the server |
| [`weft-matrix/`](weft-matrix/README.md) | the Matrix bridge + companion Synapse + Postgres | only to bridge to Matrix |

**Caddy is part of the weftd stack**, not a third one, because weftd needs the
*certificate* and not merely the proxy: QUIC cannot be reverse-proxied — it is UDP +
TLS 1.3 end to end — so weftd terminates it itself and reads the certificate out of
Caddy's volume. Sharing a named volume means sharing a project. It stays optional
(`COMPOSE_PROFILES=`, then give weftd its own certificate) if you already run a
proxy.

**The bridge is separate** because it needs nothing from inside that stack: it
reaches weftd over weftd's public name, exactly as any third-party appservice would.
No shared network, no ordering between the two `up`s, and tearing the bridge down
cannot touch weftd's data. The one thing it needs from the weftd side is a
`Caddyfile` site block, since federation needs public TLS.

Follow the walkthrough below start to finish; the per-stack READMEs go deeper.

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
cd weft/deploy

openssl rand -hex 32   # → the Postgres password
openssl rand -hex 32   # → the LiveKit secret
```

The Postgres password goes in **one** place, `weftd/.env` — weftd expands
`${POSTGRES_PASSWORD}` from the environment when it reads `weft.toml`. The LiveKit
secret still goes in two (`weft.toml` and `livekit.yaml`); LiveKit's config has no
such expansion.

Everything in Part 1 is in `deploy/weftd/`.

### 4. Edit the config

**`.env`**

```dotenv
POSTGRES_PASSWORD=<the-postgres-password>
```

**`weft.toml`**

```toml
network   = "weft.example.com"        # your domain

[tls]                                 # replace the domain in BOTH paths
cert = "/data/caddy/certificates/acme-v02.api.letsencrypt.org-directory/weft.example.com/weft.example.com.crt"
key  = "/data/caddy/certificates/acme-v02.api.letsencrypt.org-directory/weft.example.com/weft.example.com.key"

[storage]
url = "postgres://weft:${POSTGRES_PASSWORD}@postgres:5432/weft"   # leave as-is

[voice.livekit]
url        = "wss://livekit.example.com"
api_key    = "devkey"                 # fine as-is
api_secret = "LIVEKIT-SECRET"
```

**`livekit.yaml`** — `keys: { devkey: LIVEKIT-SECRET }` (the key *name* is
`api_key` above, its value is `api_secret`).

**`Caddyfile`** — the two site addresses. `weft.example.com` MUST MATCH
`network` above: §10.2's well-known is fetched at
`https://<network>/.well-known/weft`, so the network name and the host serving it
are one and the same.

```caddyfile
weft.example.com    { reverse_proxy weftd:8081 }
livekit.example.com { reverse_proxy livekit:7880 }
```

Its two commented-out Matrix blocks stay commented unless you do Part 2.

Still duplicated, so check before moving on:

| Value          | Both of                                                                  |
| -------------- | ------------------------------------------------------------------------ |
| LiveKit secret | `weft.toml` `api_secret` · `livekit.yaml` `keys:`                        |
| Your domain    | `weft.toml` (`network`, both `[tls]` paths, LiveKit `url`) · `Caddyfile` |

### 5. Start it

```sh
docker compose up -d            # add --build to compile locally instead of pulling
docker compose logs -f caddy    # → certificates obtained, for both names
docker compose logs -f weftd    # → "same-origin /ws mounted", then "weftd listening"
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

A third stack, `weft-matrix/`: a **companion homeserver** (Synapse — dedicated to
the bridge, nobody signs up on it), the **bridge daemon**, and their own Postgres.
It reaches weftd over weftd's public name, the same way any third-party appservice
would, so it shares nothing with Part 1 but the network cable.

The order below is forced by a circular dependency: the daemon's key must exist
before weftd can pin it, and weftd must pin it before the daemon may connect. So
the bridge starts **last**.

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

Everything below is in `deploy/weft-matrix/`, except the last item.

- **`.env`** — this stack's own `POSTGRES_PASSWORD` (unrelated to weftd's), and
  `MATRIX_BIND`/`MATRIX_PORT` for the homeserver's published port.
- **`weft-matrix.toml`** — `[matrix] domain` = the name you chose in step 1;
  `as_token` and `hs_token` (**change both** — anyone holding `hs_token` can inject
  events as any Matrix user); `[weft] endpoint` and `media_url` = **weftd's public
  name** (`weft.example.com:4433` and `https://weft.example.com`), since this stack
  reaches it through the internet, not a shared network; and `admins` (MXIDs allowed
  to run the `!weft` console) if you want it. `[daemon] database_url` already reads
  `${POSTGRES_PASSWORD}` from `.env` — leave it.
- **`weft-matrix.yaml`** — the appservice registration Synapse loads. It repeats
  five values from `weft-matrix.toml` (`url`, both tokens, the bot localpart, the
  puppet regex) because Synapse has no environment expansion; the file lists each
  one and what a mismatch does. Substitute your `server_name` into the regex,
  **escaping every dot**.
- **`homeserver.yaml`** — `server_name` = **exactly** the same string as
  `[matrix] domain`, plus this stack's Postgres password (again literal, for the
  same reason).
- **`initdb/10-matrix.sql`** — the same password once more; it creates Synapse's
  role.
- **`../weftd/Caddyfile`** — uncomment the `matrix.…` block (and the apex one, on
  the delegated shape). It points at `host.docker.internal:8008`, because Caddy lives
  in the weftd stack and reaches this one through the host. This is what makes
  federation work over **443**, so port 8448 never has to be opened.

### 3. Create the adapter key

```sh
cd weft-matrix
docker compose run --rm --build adapter-key
```

Prints the public key. Idempotent — it only creates the file if absent. `--build`
compiles the image locally; drop it once you are pulling a published one.

(Not `run bridge keygen …`: `run` starts the target's dependencies, and the bridge
depends on Synapse, which cannot start until its config is complete.)

### 4. Pin the key in weftd

In `weftd/weft.toml`:

```toml
[[plugin.remote]]
id      = "matrix"
key     = "<the key from step 3>"
bot     = "matrixbot"     # optional: weftd provisions a native bot account
schemes = ["matrix"]
```

```sh
(cd weftd && docker compose up -d weftd)
```

Until this matches, the bridge is refused with `AUTH-FAILED` — by design.

### 5. Start the bridge

```sh
cd weft-matrix
docker compose up -d --build
docker compose logs -f bridge     # → the adapter pubkey, then "connected to weftd"
(cd ../weftd && docker compose restart caddy)   # picks up the matrix site block
```

Synapse's **signing key** is written on first boot into the `synapse_data` volume —
no step of its own. **Back that volume up**: the key is the identity remote
homeservers pin, and losing it means every server you have federated with rejects
your events.

### 6. Check that federation works

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
	reverse_proxy host.docker.internal:8008
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
console) is in [`weft-matrix/README.md`](weft-matrix/README.md#verifying-it-works). Note the
warning there: **the bridge has never been run against a real homeserver.**

---

## Day 2

Each stack is operated on its own, from its own directory:

```sh
docker compose logs -f              # tail
docker compose up -d <service>      # apply a config edit
docker compose pull && docker compose up -d    # update images
docker compose down                 # stop; data stays in the named volumes
```

**Back up**, per stack:

| Stack         | What                  | Why                                          |
| ------------- | --------------------- | -------------------------------------------- |
| `weftd`       | `pgdata` volume       | the database                                 |
| `weftd`       | `weftd_media` volume  | uploaded files (content-addressed blobs)     |
| `weftd`       | `[identity] key_file` | your network's signing key                   |
| `weftd`       | `caddy_data` volume   | certificates + the ACME account key          |
| `weft-matrix` | `keys` volume         | the adapter key weftd pins — not recoverable |
| `weft-matrix` | `synapse_data` volume | Synapse's signing key — likewise             |

Removing the bridge is `docker compose down -v` in `weft-matrix/` and deleting the
`[[plugin.remote]]` block from `weftd/weft.toml`. Nothing of weftd's is touched —
that is the point of the separation.
