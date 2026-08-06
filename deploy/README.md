# Deployments

One Compose stack, in [`weftd/`](weftd/README.md): weftd (with the embedded web
client), PostgreSQL, LiveKit (voice) and Caddy (automatic HTTPS).

The optional **Matrix bridge** is a *profile* of that same stack, not a stack of
its own — `COMPOSE_PROFILES=caddy,matrix` adds a companion Synapse homeserver plus
the `weft-matrix` daemon. Setup: [`weftd/MATRIX.md`](weftd/MATRIX.md).

It lives in the same Compose project because the daemon has to reach `weftd:4433`
(QUIC) and `weftd:8081` (media): one project means those names resolve with no
external-network wiring and no ordering between two `up`s. It still tears down
cleanly — the bridge has its own databases and volumes, so removing the profile
never touches weftd's data.
