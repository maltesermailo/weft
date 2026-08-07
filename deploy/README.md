# Deployments

Two independent Compose stacks.

| Directory                               | What it runs                                     | Needed?                  |
| --------------------------------------- | ------------------------------------------------ | ------------------------ |
| [`weftd/`](weftd/README.md)             | weftd + PostgreSQL + LiveKit + Caddy (TLS)       | yes — this is the server |
| [`weft-matrix/`](weft-matrix/README.md) | the Matrix bridge + companion Synapse + Postgres | only to bridge to Matrix |

Caddy is inside the weftd stack rather than beside it because weftd needs the
*certificate*, not just the proxy. QUIC can't be reverse-proxied, so weftd
terminates it itself and reads the certificate out of Caddy's volume — and a shared
named volume means a shared project. It's still optional if you already run a proxy.

The bridge is separate because it needs nothing from in there. It dials weftd's
public name like any third-party appservice, so there's no shared network, no
ordering between the two `up`s, and removing it can't touch weftd's data.

**How to read this.** Part 1 then Part 2, in order — every step is a command or an
edit. The two appendices are the parts you only need if you hit them, and the
per-stack READMEs cover operating each one.

---

## Part 1 — the weftd stack

You need a Linux server with Docker and Docker Compose, and a **domain you
control** — Let's Encrypt validates it over ports 80/443. This guide writes
`example.com`; substitute yours throughout.

Everything in Part 1 happens in `deploy/weftd/`.

### 1. Point DNS at the server

Two **A records** (AAAA too, if you have IPv6) → the server's public IP:

```
weft.example.com       →  203.0.113.10
livekit.example.com    →  203.0.113.10
```

### 2. Open the firewall

| Port        | Proto | For                                                            |
| ----------- | ----- | -------------------------------------------------------------- |
| 80, 443     | TCP   | Caddy (HTTP + HTTPS)                                           |
| 443         | UDP   | HTTP/3 (optional)                                              |
| 4433        | UDP   | weftd QUIC, for desktop/native clients. **Cannot be proxied.** |
| 50000-50020 | UDP   | LiveKit voice media                                            |

### 3. Get the code and generate two secrets

```sh
git clone <your-weft-repo> weft
cd weft/deploy/weftd

openssl rand -hex 32   # → the Postgres password
openssl rand -hex 32   # → the LiveKit secret
```

The Postgres password goes in one place, `.env`: weftd expands
`${POSTGRES_PASSWORD}` from the environment when it reads `weft.toml`. The LiveKit
secret goes in two, because LiveKit's config has no such expansion.

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

**`livekit.yaml`** — `keys: { devkey: LIVEKIT-SECRET }`. The key *name* is
`api_key` above; its value is `api_secret`.

**`Caddyfile`** — the two site addresses:

```caddyfile
weft.example.com    { reverse_proxy weftd:8081 }
livekit.example.com { reverse_proxy livekit:7880 }
```

`weft.example.com` must match `network` above. §10.2's well-known is fetched at
`https://<network>/.well-known/weft`, so the network name and the host serving it
are the same thing.

Two values are still duplicated by hand. Check them before moving on:

| Value          | Must match in                                                            |
| -------------- | ------------------------------------------------------------------------ |
| LiveKit secret | `weft.toml` `api_secret` · `livekit.yaml` `keys:`                        |
| Your domain    | `weft.toml` (`network`, both `[tls]` paths, LiveKit `url`) · `Caddyfile` |

### 5. Start it

```sh
docker compose up -d            # add --build to compile locally instead of pulling
docker compose logs -f caddy    # → certificates obtained, for both names
docker compose logs -f weftd    # → "same-origin /ws mounted", then "weftd listening"
```

weftd boots on a self-signed placeholder for QUIC and hot-swaps Caddy's real
certificate in once Caddy has it, within a minute. No restart needed.

### 6. Create the first operator

```sh
docker compose exec weftd weftd admin create admin --password '<a-strong-password>'
```

This registers the account *and* flags it operator (§11.3: every capability at `*`,
plus access to `/admin`). To promote an account that already exists, use
`weftd admin grant <account>`.

### 7. Log in

Open **`https://weft.example.com`**. The web client is served by weftd and connects
back over `wss://weft.example.com/ws`. Create a voice channel and join from two
browsers to check LiveKit.

**Stop here if you don't bridge to Matrix.**

---

## Part 2 — the Matrix bridge (optional)

The second stack, `weft-matrix/`: a companion homeserver (Synapse, dedicated to the
bridge — nobody signs up on it), the bridge daemon, and their own Postgres.

One ordering rule drives the steps below. The daemon's key must exist before weftd
can pin it, and weftd must pin it before the daemon may connect — so the bridge
starts **last**.

### 1. Choose the homeserver's name, and point DNS at it

Synapse's `server_name` is the suffix of every MXID (`@weft_<ulid>:<server_name>`).
It is **permanent**: it's baked into every event the server signs, so changing it
later invalidates all of them.

It is *not* the hostname the server runs on. Remote servers fetch
`/.well-known/matrix/server` from `https://<server_name>/`, and that file tells them
where to actually connect — which gives two workable shapes:

| `server_name`        | MXIDs                        | Well-known served by    | Federation lands on  |
| -------------------- | ---------------------------- | ----------------------- | -------------------- |
| `matrix.example.com` | `@weft_…:matrix.example.com` | Synapse itself          | `matrix.example.com` |
| `example.com` (apex) | `@weft_…:example.com`        | Caddy, in an apex block | `matrix.example.com` |

**Take the first one** unless you specifically want Matrix identities on your main
domain. It needs no delegation and no apex record — just one more A record:

```
matrix.example.com     →  203.0.113.10
```

For the apex shape, follow [Appendix A](#appendix-a--mxids-on-your-main-domain)
instead, then come back here.

### 2. Edit the config

In `deploy/weft-matrix/`:

- **`.env`** — this stack's own `POSTGRES_PASSWORD` (unrelated to weftd's), and
  `MATRIX_BIND`/`MATRIX_PORT` for the homeserver's published port.
- **`weft-matrix.toml`** — `[matrix] domain` = the name from step 1. `as_token` and
  `hs_token`: **change both**, since anyone holding `hs_token` can inject events as
  any Matrix user. `[weft] endpoint` and `media_url` = weftd's public name
  (`weft.example.com:4433`, `https://weft.example.com`) — this stack reaches weftd
  over the internet, not a shared network. `admins` if you want the `!weft` console.
  Leave `[daemon] database_url`; it already reads `${POSTGRES_PASSWORD}`.
- **`weft-matrix.yaml`** — the appservice registration Synapse loads. Substitute
  your `server_name` into the puppet regex, **escaping every dot**. It repeats five
  values from `weft-matrix.toml` because Synapse has no environment expansion; the
  file tabulates each one and what a mismatch does.
- **`homeserver.yaml`** — `server_name` = exactly the same string as
  `[matrix] domain`, plus this stack's Postgres password (literal, same reason).
- **`initdb/10-matrix.sql`** — that password once more; it creates Synapse's role.

And one file in the other stack, **`../weftd/Caddyfile`** — a site block for the
homeserver. This is what makes federation work over 443, so 8448 never has to be
opened.

```caddyfile
matrix.example.com {
	reverse_proxy localhost:8008        # containerised: host.docker.internal:8008
}
```

The upstream is a **host port**, not a service name: the two stacks share no
network. The bundled Caddy runs in a container, where the host is
`host.docker.internal` — its `extra_hosts` already maps that name.

### 3. Create the adapter key

```sh
cd weft-matrix
docker compose run --rm --build adapter-key
```

Prints the public key. Idempotent: it creates the key file only if absent. `--build`
compiles the image locally; drop it once you pull a published one.

Not `run bridge keygen …` — `run` starts the target's dependencies, and the bridge
depends on Synapse, which can't start until its config is complete.

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
(cd ../weftd && docker compose up -d weftd)
```

Until this matches, the bridge is refused with `AUTH-FAILED`. By design.

### 5. Start the bridge

```sh
docker compose up -d --build
docker compose logs -f bridge                   # → pubkey, then "connected to weftd"
(cd ../weftd && docker compose restart caddy)   # picks up the matrix site block
```

Synapse's signing key is written on first boot into the `synapse_data` volume, so
there's no step for it. **Back that volume up:** the key is the identity remote
homeservers pin, and losing it means every server you've federated with rejects your
events.

### 6. Check that federation works

```sh
docker compose logs synapse | grep -i appservice
curl https://matrix.example.com/.well-known/matrix/server
# → {"m.server":"matrix.example.com:443"}
```

A bad registration is fatal at startup, so a running Synapse means it loaded. The
`:443` in the well-known is what keeps port 8448 shut — if it's missing or the curl
fails, see [Appendix B](#appendix-b--when-federation-doesnt-work).

Then run your `server_name` through **<https://federationtester.matrix.org>**. It
checks DNS, delegation, the certificate and the signing key the way a remote
homeserver would, and names whatever is wrong. It must come back green before a
Matrix user elsewhere can join anything here.

The rest of the verification — consume a room, project a namespace, the `!weft`
console — is in
[`weft-matrix/README.md`](weft-matrix/README.md#verifying-it-works). Note the
warning there: **the bridge has never been run against a real homeserver.**

---

## Day 2

Each stack is operated on its own, from its own directory:

```sh
docker compose logs -f                          # tail
docker compose up -d <service>                  # apply a config edit
docker compose pull && docker compose up -d     # update images
docker compose down                             # stop; data stays in the volumes
```

Back up, per stack:

| Stack         | What                  | Why                                          |
| ------------- | --------------------- | -------------------------------------------- |
| `weftd`       | `pgdata` volume       | the database                                 |
| `weftd`       | `weftd_media` volume  | uploaded files (content-addressed blobs)     |
| `weftd`       | `[identity] key_file` | your network's signing key                   |
| `weftd`       | `caddy_data` volume   | certificates + the ACME account key          |
| `weft-matrix` | `keys` volume         | the adapter key weftd pins — not recoverable |
| `weft-matrix` | `synapse_data` volume | Synapse's signing key — likewise             |

To remove the bridge: `docker compose down -v` in `weft-matrix/`, and delete the
`[[plugin.remote]]` block from `weftd/weft.toml`. Nothing of weftd's is touched —
that's the point of the separation.

---

## Appendix A — MXIDs on your main domain

The delegated-apex shape from Part 2 step 1: `server_name` is your apex, while
Synapse still runs on `matrix.example.com`. MXIDs read `@weft_…:example.com`.

It costs an apex A record (Caddy needs a certificate for it) and one extra Caddy
block, because the apex — not the subdomain — is the authority remote servers ask:

```
matrix.example.com     →  203.0.113.10
example.com            →  203.0.113.10
```

### The Caddyfile

The apex block does not have to be weftd's: Matrix's `server_name` and weftd's
`network` are independent. So which listing you want depends on where **weftd**
lives. Both below are the **whole file** — the weftd and LiveKit blocks from Part 1
are still in there, and dropping either takes the web client or voice with it.

**If weftd is on a subdomain** (`network = "weft.example.com"`), the apex serves the
delegation and sends everything else to the app:

```caddyfile
weft.example.com {
	reverse_proxy weftd:8081
}

livekit.example.com {
	reverse_proxy livekit:7880
}

example.com {
	handle /.well-known/matrix/* {
		header Content-Type application/json
		respond `{"m.server": "matrix.example.com:443"}`
	}

	# Drop this block if the apex already serves a site from elsewhere — but then
	# THAT server has to answer the well-known above, because the apex is the
	# Matrix authority either way.
	handle {
		redir https://weft.example.com{uri}
	}
}

matrix.example.com {
	reverse_proxy localhost:8008        # containerised: host.docker.internal:8008
}
```

**If weftd IS the apex** (`network = "example.com"`), there's no redirect and no
`weft.example.com` block at all. The apex catch-all is the reverse proxy weftd
already needed, with the delegation in front of it:

```caddyfile
example.com {
	handle /.well-known/matrix/* {
		header Content-Type application/json
		respond `{"m.server": "matrix.example.com:443"}`
	}

	handle {
		reverse_proxy weftd:8081
	}
}

livekit.example.com {
	reverse_proxy livekit:7880
}

matrix.example.com {
	reverse_proxy localhost:8008        # containerised: host.docker.internal:8008
}
```

That second shape is the tidiest overall — one domain, WEFT accounts reading
`user@example.com` and MXIDs `@weft_…:example.com`. Three things about it:

- **The two well-knowns don't collide.** `/.well-known/weft` is a different path,
  and the `handle` matches only the `matrix` subtree, so weftd still serves its own.
- **LiveKit keeps its subdomain.** Only weftd moves to the apex; `[voice.livekit]
  url` stays `wss://livekit.example.com`.
- **`weft.toml`'s `[tls]` paths must name the apex**, since that's now the
  certificate weftd reads for QUIC. Point them at a subdomain Caddy no longer holds a
  cert for and QUIC silently stays on the self-signed placeholder while HTTPS looks
  perfectly healthy.

Both apex routes are `handle` blocks deliberately. Those are mutually exclusive and
first-match, so the well-known can't be swallowed by the catch-all. A bare directive
next to a `handle` would leave that to Caddy's internal directive ordering, which is
not something to bet federation on.

### The other two files

**`homeserver.yaml`**

```yaml
server_name: "example.com"                        # the apex — the MXID suffix
public_baseurl: "https://matrix.example.com/"     # where it actually answers
serve_server_wellknown: false                     # Caddy owns that file now
```

**`weft-matrix.toml`** — `[matrix] domain = "example.com"`, matching `server_name`
exactly. Not weftd's network name: the two identity spaces are separate, and only
this one appears in MXIDs.

`serve_server_wellknown: false` is hygiene rather than necessity. Synapse can only
ever name *itself* — its endpoint returns `{"m.server": "<server_name>:443"}` — so
with an apex `server_name` its copy would say `example.com:443`, published on
`matrix.example.com`, pointing back at a host that serves only the JSON. Nothing
queries it, because discovery fetches the well-known for `server_name` only and then
resolves the returned host by port/SRV/A without recursing. But it's a confidently
wrong answer to find while troubleshooting, and it becomes actually wrong the day the
apex is re-pointed.

<details>
<summary>Variant: let the apex front the federation API instead</summary>

If you'd rather proxy the well-known than hard-code it — the common nginx pattern —
the apex must also front `/_matrix/*`, because the file Synapse returns names the
apex and remote servers will then send federation traffic there:

```caddyfile
example.com {
	handle /.well-known/matrix/* { reverse_proxy localhost:8008 }
	handle /_matrix/* { reverse_proxy localhost:8008 }
	handle { redir https://weft.example.com{uri} }
}
```

with `serve_server_wellknown: true` and `public_baseurl: "https://example.com/"`.
Equally correct. It just puts the whole federation surface on the apex instead of one
static file, and the certificate remote servers validate becomes the apex's.

</details>

---

## Appendix B — when federation doesn't work

**Check the right host.** Discovery only ever fetches
`/.well-known/matrix/server` from `https://<server_name>/`. Querying
`matrix.example.com` when `server_name` is the apex gets you Synapse's own copy,
which names the apex — an inert, plausible-looking wrong answer. Curl the
`server_name`, nothing else.

**A missing `:443` means 8448.** Remote servers try the well-known, then an SRV
record, then `<server_name>:8448` as a last resort. Delegation short-circuits that to
443, which is why nothing here listens on 8448. If the answer carries no port, fix it
one of two ways.

Publish 8448 through Caddy:

```caddyfile
matrix.example.com:8448 {
	reverse_proxy localhost:8008        # containerised: host.docker.internal:8008
}
```

…and open 8448/TCP in the firewall. Or author the delegation in Caddy with the port
spelled out, instead of letting Synapse answer it:

```caddyfile
handle /.well-known/matrix/server {
	header Content-Type application/json
	respond `{"m.server": "matrix.example.com:443"}`
}
```

**For a first smoke test with no public TLS at all**, add
`federation_domain_whitelist: []` to `homeserver.yaml`. The bridge still works for
rooms on the companion server, which exercises provisioning, both traffic directions,
media, DMs, typing and the console. Remove it before you expect anyone else to join.
