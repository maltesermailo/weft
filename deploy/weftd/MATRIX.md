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

**The walkthrough is [`../README.md`](../README.md) → Part 2** — one ordered list,
kept in one place so it cannot drift from Part 1. In outline: choose the
`server_name` and add its A record, edit the four files, create the two databases,
`keygen`, pin the key in `weft.toml`, generate the registration and Synapse's
signing key, then turn the profile on.

That order is forced, not stylistic: the adapter key must exist before weftd can
pin it, and weftd must pin it before the bridge may connect — so
`COMPOSE_PROFILES=…,matrix` goes on **last**, and everything before it runs via
`docker compose run --rm`, which enables a profiled service for that one command.

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

Federation is not optional in practice — remote homeservers reaching projected
Spaces is the point of outbound projection. It needs three things, and the shipped
files provide all three:

1. **A public name with real TLS in front of Synapse.** Uncomment the matrix site
   block in `Caddyfile` (`matrix.weft.example { reverse_proxy synapse:8008 }`) and
   add the A record.
2. **`/.well-known/matrix/server`, served from `https://<server_name>/`** — that
   host and no other, because `server_name` is the authority in every MXID. Which
   host that is, is your choice, and it is the whole difference between the two
   supported shapes:
   - **Direct**: `server_name` = `matrix.weft.example`, the host Synapse already
     answers on. `serve_server_wellknown: true` (the shipped default) has Synapse
     serve the file itself. Nothing else to do.
   - **Delegated**: `server_name` = your apex, Synapse still running on the
     subdomain, so an apex Caddy block answers the file with the subdomain as its
     target. MXIDs read `@weft_…:weft.example` instead of
     `@weft_…:matrix.weft.example`. **Step-by-step recipe:
     [`../README.md`](../README.md) → Part 2, step 1.**

   The constraint that decides the details: Synapse's endpoint can only ever name
   *itself* — it returns `{"m.server": "<server_name>:443"}`. So on the delegated
   shape Caddy has to author that file (`respond` with the subdomain), and Synapse's
   own copy is turned off. Proxying the path to Synapse instead is possible, but
   then the apex must front `/_matrix/*` too, since the answer will name the apex.
3. **Nothing on 8448.** Remote servers try the well-known first, then SRV, then
   `<server_name>:8448` as a last resort. Delegation short-circuits that to 443, so
   8448 stays closed. Verify with
   `curl https://<server_name>/.well-known/matrix/server` — the answer must carry
   `:443`. If it doesn't, either publish `matrix.weft.example:8448` through Caddy
   (and open the port) or author the file in Caddy with the port spelled out.
   `deploy/README.md` step 8 has both.

Then run `server_name` through <https://federationtester.matrix.org>, which checks
DNS, delegation, the certificate and the signing key the way a remote homeserver
would.

**For a first smoke test without any of this:** add
`federation_domain_whitelist: []` to `homeserver.yaml`. The bridge still works for
rooms **on this server**, and projected Spaces are usable by local Matrix clients —
enough to exercise provisioning, both traffic directions, media, DMs, typing and
the console. Remove it before you expect anyone else to join.

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
- **Nor is Synapse's signing key.** It is written into `synapse_data` on first boot
  and is the identity remote homeservers pin; back that volume up too.
- **Synapse logs to stdout** (`docker compose logs synapse`), per
  `synapse-log.config`. Set `root.level: DEBUG` there and recreate the container to
  debug a federation or appservice problem — it is very verbose.
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
