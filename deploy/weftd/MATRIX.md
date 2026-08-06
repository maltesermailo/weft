# The Matrix bridge, in Docker

The bridge is an **optional part of this stack**, not a separate one: the `matrix`
Compose profile adds a **companion homeserver** (Synapse) and the **bridge daemon**
(`weft-matrix`) to the services in `docker-compose.yml`. It shares the stack's
Postgres, network and Caddy.

One profile rather than a second project because the daemon has to reach weftd
(`weftd:4433` for QUIC, `weftd:8081` for media). Same project ⇒ those names just
resolve; no external-network wiring, no ordering between two `up`s. Leave `matrix`
out of `COMPOSE_PROFILES` and none of it is built or run.

**Synapse rather than conduwuit**, for one reason: appservice registration has to
be *declarative*. Synapse reads it from `app_service_config_files`; conduwuit
registers appservices through its admin room, which would make setup a manual step
performed by hand in a chat window. `matrix.md` decision 1 lists Synapse as
supported, so this is within the design, not a departure from it.

Design: [`docs/architecture/matrix.md`](../../docs/architecture/matrix.md). The
wire contract with weftd:
[`docs/protocol/bridge-session-protocol.md`](../../docs/protocol/bridge-session-protocol.md).

> **This has never run against a real homeserver.** Every test to date drives a
> mock that speaks spec-shaped JSON. Expect at least one mismatch — media-endpoint
> auth, `/context` token semantics, `is_direct` handling. Where that happens the
> daemon logs the failing request with its status, which is the fastest path to
> the answer.

## What talks to what

```
   Matrix federation                     WEFT clients
          │                                    │
          ▼                                    ▼
    ┌──────────┐   appservice API    ┌──────┐ QUIC ┌───────┐
    │ Synapse  │◄───────────────────►│bridge│─────►│ weftd │
    └──────────┘   (txn push /       └──────┘      └───────┘
     puppets +      intents)           │  HTTP media (§13)     ▲
     the bot                           └───────────────────────┘
                          ┌──────────┐
                  all of  │ postgres │  weft · synapse · weftmatrix
                          └──────────┘
```

The bridge is an appservice to Synapse **and** a provider session to weftd.

## Setup

Order matters: the adapter key must exist before weftd can pin it, and weftd must
pin it before the bridge may connect. Steps 1–3 therefore run *before* you add the
profile.

### 1. Edit the config

- **`weft-matrix.toml`** — set `[matrix] domain` (the homeserver's name),
  `as_token` / `hs_token` (change both; anyone with `hs_token` can inject events as
  any Matrix user), and `admins` if you want the operator console.
- **`homeserver.yaml`** — `server_name` = the *same* domain, plus the Synapse
  Postgres password.
- **`initdb/10-matrix.sql`** — the same Synapse password (it creates the role).
  See the note below: this runs **only** on an empty Postgres volume.
- **`.env`** — `MATRIX_BIND` / `MATRIX_PORT`, how the homeserver's port is
  published.

### 2. Create the adapter key

```sh
docker compose run --rm bridge keygen /etc/weft/weft-matrix.toml
```

Prints the public key. Idempotent — it creates the key file only if absent, so
running it again just prints the same key.

### 3. Pin it in `weft.toml`

(The annotated block lives in the repo's `weftd.example.toml`.)

```toml
[[plugin.remote]]
id      = "matrix"
key     = "<the key from step 2>"
bot     = "matrixbot"     # optional: weftd provisions a native bot account
schemes = ["matrix"]
```

`docker compose up -d weftd` to apply it. Until this matches, the bridge is
refused with `AUTH-FAILED` — by design, not a misconfiguration you can work
around.

### 4. Generate the appservice registration

```sh
docker compose run --rm registration
```

Writes `/appservices/weft-matrix.yaml` into a volume Synapse mounts read-only and
loads via `app_service_config_files`. Generated rather than hand-written so the
tokens live in exactly one place — `weft-matrix.toml` — instead of being copied
into two files that can drift.

Re-run it (and restart Synapse) after changing the tokens, the domain or the
puppet prefix.

### 5. Generate Synapse's signing key

```sh
docker compose run --rm synapse-keys
```

`--generate-keys` fills in what is *missing* and leaves `homeserver.yaml` alone,
which is why that file stays hand-edited.

### 6. Enable the profile

In `.env`:

```sh
COMPOSE_PROFILES=caddy,matrix
```

Then:

```sh
docker compose up -d --build
docker compose logs -f bridge
```

A healthy start logs the adapter pubkey, `connected to weftd`, and — on a fresh
database — nothing about recovery (there is nothing to recover).

### If Postgres already has data

`initdb/` scripts run only when the data directory is empty, so an existing
deployment needs the two databases created by hand — once:

```sh
docker compose exec -T postgres psql -U weft -d postgres < initdb/10-matrix.sql
```

## Verifying it works

0. **Synapse loaded the registration.** `docker compose logs synapse | grep -i
   appservice` — a rejected registration is fatal at startup, so a running Synapse
   means the file parsed and the namespace was claimed.
1. **The provider is up.** `docker compose logs weftd | grep provider` shows
   `provider registered scheme`.
2. **Consume a room.** In a WEFT client, join `matrix://<hs>/<space>` — for a room
   on the companion server, `matrix://matrix.weft.example/<alias>`. The bridge
   resolves, joins, enumerates and asserts it; the namespace appears in `DISCOVER`.
3. **Project a namespace.** As an ns-admin: `NS META <ns-id> bridge:matrix :open`
   (needs `public` visibility). Its **`permanent`** channels appear as Matrix rooms
   — a `retained:*` one deliberately does not (§3, locked decision 2), and the
   channel `NS CREATE` seeds is `retained:90d`, so set it `permanent` if you want
   to see it.
4. **Message both ways**, then edits, reactions, an attachment.
5. **The console** (if `admins` is set): DM the bot `!weft status`.

## Federation and TLS

Matrix federation needs the homeserver on a public name with real TLS, plus
`/.well-known/matrix/server`. The stack's Caddy fronts it — add to `Caddyfile`:

```caddyfile
matrix.weft.example {
	# Client + federation API.
	reverse_proxy synapse:8008
	# Delegation: tells other servers where to find us.
	handle /.well-known/matrix/* {
		header Content-Type application/json
		respond `{"m.server": "matrix.weft.example:443"}`
	}
}
```

Federation is not required to test: with `federation_domain_whitelist: []` in
`homeserver.yaml`, the bridge still works for rooms **on this server**, and
projected Spaces are usable by local Matrix clients. That exercises provisioning,
both traffic directions, media, DMs, typing and the console without any public
TLS.

## Operating

- **State recovery.** The daemon's database is a cache: structure ids are
  deterministic, and Matrix carries the markers. Drop `weftmatrix` and it rebuilds
  at boot — or run `!weft recover`. The one exception is the bridging **ban list**,
  which lives in the bot's Matrix account data precisely so it survives
  (matrix.md §20a).
- **`puppet_prefix` is permanent.** Changing it orphans every existing puppet.
  Recovery warns when it sees the mismatch but cannot repair it.
- **The adapter key is not recoverable.** Back up the `matrix_keys` volume, or be
  ready to re-pin.
- **Bans** are set from weftd's admin panel (a namespace's bridging toggle), not
  here; the bridge stores and enforces them.
- **Removing the bridge:** drop `matrix` from `COMPOSE_PROFILES`,
  `docker compose up -d --remove-orphans`, then `DROP DATABASE weftmatrix;` and
  `DROP DATABASE synapse;`. weftd's own data is untouched — that is why the bridge
  gets its own databases rather than sharing `weft`.

## Known gaps

- No live-homeserver validation yet (see the note at the top).
- The stack is Synapse-only in practice: conduwuit works with the daemon, but its
  admin-room registration is not something this deployment automates.
- `MEDIA BLOCK` after the fact does not retro-redact the mapped Matrix events.
- One realm per daemon: bridging a second homeserver needs a second instance.
- Read receipts are never bridged (WEFT's `MARK` is private; Matrix receipts are
  public).
