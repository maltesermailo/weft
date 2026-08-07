# The Matrix bridge stack

The **`weft-matrix` daemon**, the **companion homeserver** (Synapse) it is an
appservice to, and a **Postgres** for the two of them.

> **Setup walkthrough: [`../README.md`](../README.md) → Part 2** — one ordered list
> across all three stacks, so it cannot drift. This file is the reference behind it:
> what the pieces are, how to verify, how to operate and remove it.

Its own Compose project, independent of `../weftd` (which is where Caddy lives too —
weftd needs its certificate for QUIC). It reaches weftd over weftd's **public name** (`[weft] endpoint` in `weft-matrix.toml`) exactly as a third-party
appservice would — no shared Docker network, no ordering between the two `up`s, and
tearing this down cannot touch weftd's data. The bridge and Synapse *do* share this
project's network, because Synapse has to call the bridge back (`url:` in
`weft-matrix.yaml`).

**Synapse rather than conduwuit**, for one reason: appservice registration has to be
*declarative*. Synapse reads it from `app_service_config_files`; conduwuit registers
appservices through its admin room, which would make setup a manual step performed
by hand in a chat window. `matrix.md` decision 1 lists Synapse as supported, so this
is within the design, not a departure from it.

Design: [`docs/architecture/matrix.md`](../../docs/architecture/matrix.md). The wire
contract with weftd:
[`docs/protocol/bridge-session-protocol.md`](../../docs/protocol/bridge-session-protocol.md).

> **This has never run against a real homeserver.** Every test to date drives a mock
> that speaks spec-shaped JSON. Expect at least one mismatch — media-endpoint auth,
> `/context` token semantics, `is_direct` handling. Where that happens the daemon
> logs the failing request with its status, which is the fastest path to the answer.

## What talks to what

```
                  this stack                    ../weftd
   ┌──────────────────────────────────────┐
   │  ┌──────────┐  appservice  ┌──────┐  │   QUIC 4433   ┌───────┐
   │  │ Synapse  │◄────────────►│bridge│──┼──────────────►│ weftd │
   │  └──────────┘  (txn push / └──────┘  │   HTTP media  └───────┘
   │   puppets +     intents)      │      │  (§13, 8081)      ▲
   │   the bot                     │      └───────────────────┘
   │        └──────┬───────────────┘      │
   │          ┌──────────┐                │
   │          │ postgres │ synapse ·      │
   │          └──────────┘ weftmatrix     │
   └──────────────────────────────────────┘
        ▲                                      the two arrows leaving this box
        │ federation (443, via ../weftd's Caddy)  go to weftd's PUBLIC name
   remote homeservers
```

The bridge is an appservice to Synapse **and** a provider session to weftd.

## Setup

**[`../README.md`](../README.md) → Part 2.** In outline: choose the `server_name`
and add its A record, edit the five files here (plus `../weftd/Caddyfile` and
weftd's `weft.toml`), `keygen`, pin the key in weftd, then start this stack.

That order is forced, not stylistic: the adapter key must exist before weftd can pin
it, and weftd must pin it before the bridge may connect — so the bridge starts
**last**, and the steps before it run via `docker compose run --rm`.

### If Postgres already has data

`initdb/` runs only on an empty volume, so an existing deployment needs Synapse's
database created by hand — once (the daemon's own database comes from
`POSTGRES_DB`, so it already exists):

```sh
docker compose exec -T postgres psql -U weft -d postgres < initdb/10-matrix.sql
```

Synapse needs a `C`-collation database, which `POSTGRES_DB` cannot express; that is
what the script is for.

## Verifying it works

0. **Synapse loaded the registration.** `docker compose logs synapse | grep -i
   appservice` — a rejected registration is fatal at startup, so a running Synapse
   means the file parsed and the namespace was claimed.
1. **The provider is up.** `docker compose logs -f bridge` shows `connected to
   weftd`, and over in `../weftd`, `docker compose logs weftd | grep provider` shows
   `provider registered scheme`. A loop of `AUTH-FAILED` means the pinned key does
   not match; a loop of `password authentication failed` means this stack's own
   `POSTGRES_PASSWORD` is wrong — the daemon opens its store before it connects to
   anything.
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
   `deploy/README.md` step 7 has both.

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
- **The adapter key is not recoverable.** Back up the `keys` volume, or be ready to
  re-pin.
- **Nor is Synapse's signing key.** It is written into `synapse_data` on first boot
  and is the identity remote homeservers pin; back that volume up too.
- **Synapse logs to stderr** at INFO (`docker compose logs synapse`), because
  `homeserver.yaml` deliberately leaves `log_config` unset — see the note there. For
  DEBUG you have to write a log config (a Python `dictConfig` document) and point
  `log_config` at it.
- **Bans** are set from weftd's admin panel (a namespace's bridging toggle), not
  here; the bridge stores and enforces them.
- **Removing the bridge:** `docker compose down -v` here, and delete the
  `[[plugin.remote]]` block from `../weftd/weft.toml`. Nothing of weftd's is touched
  — that is the point of the separation.

## Known gaps

- No live-homeserver validation yet (see the note at the top).
- The stack is Synapse-only in practice: conduwuit works with the daemon, but its
  admin-room registration is not something this deployment automates.
- `MEDIA BLOCK` after the fact does not retro-redact the mapped Matrix events.
- One realm per daemon: bridging a second homeserver needs a second instance.
- Read receipts are never bridged (WEFT's `MARK` is private; Matrix receipts are
  public).
