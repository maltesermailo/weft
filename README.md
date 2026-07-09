# weft

Reference implementation of the **WEFT protocol**: a federated chat protocol
combining IRC's operational simplicity with Discord's feature semantics — Rust,
tokio, QUIC-native. Independent sovereign networks federate by explicit signed
manifest peering; a text control plane (`@tags VERB params :trailing`,
netcat-debuggable) rides QUIC with a WebSocket fallback.

- **Server** (`weftd`) — the Rust workspace under `crates/`.
- **Client** (`client/`) — a Tauri + SvelteKit app that runs three ways off one
  codebase: **desktop** (Tauri), **web** (browser over WebSocket), and
  **embedded** (served by `weftd` itself).

Normative spec: [`docs/weft-protocol-spec.md`](docs/weft-protocol-spec.md).
Architecture: [`docs/weftd-server-architecture.md`](docs/weftd-server-architecture.md).

## Prerequisites

- **Rust** — stable toolchain (MSRV 1.75, no nightly). `rustup` recommended.
- **Node.js** + npm — for the client (any recent LTS). pnpm works too.
- **For the web/embedded client only:**
  - `wasm-pack` — `cargo install wasm-pack`
  - the wasm target — `rustup target add wasm32-unknown-unknown`

## Server (`weftd`)

```bash
cargo build                      # build the whole workspace
cargo run -p weftd               # run a localhost dev network (#general, in-memory)
cargo run -p weftd -- weftd.toml # run with a config file
```

With no config, `weftd` starts a `localhost` network with `#general` on an
in-memory store (nothing survives a restart) — enough to connect a client
against. A config file selects listeners, storage (Postgres), operators,
federation peers, etc.

### Tests & lint

```bash
cargo test -p weft-proto         # fast codec suite — run constantly
cargo test --workspace           # everything
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

Postgres-backed store tests are gated on `WEFT_TEST_DATABASE_URL` (skipped when
unset).

## Client

All client commands run from `client/`:

```bash
cd client
npm install
```

### Desktop (Tauri)

```bash
npm run tauri dev                # dev build with hot reload
npm run tauri build              # production desktop bundle
```

### Web (browser)

The browser build compiles the shared protocol/crypto core to WebAssembly
(`weft-client-wasm`) and speaks WEFT to a running `weftd` over WebSocket.

```bash
npm run dev:web                  # wasm-pack build + vite dev server
npm run build:web                # wasm-pack build + static SPA → client/build
```

`dev:web`/`build:web` run `npm run wasm` first (wasm-pack → `static/wasm`); the
plain `dev`/`build` scripts skip it and are for the desktop path. Point the app
at a `weftd` whose `[listen] ws` (or same-origin `/ws`, below) is enabled.

## Embedded: one binary serving the web app

`weftd` can serve the built SPA itself and expose a **same-origin** `/ws` on its
HTTP/HTTPS listener — no separate web server, no separate WS port.

```bash
# 1. build the browser client (produces client/build)
cd client && npm run build:web && cd ..

# 2. build weftd with the SPA embedded, then run it with web serving on
cargo build -p weftd --features web-ui
cargo run  -p weftd --features web-ui -- weftd.toml
```

Enable it in the config:

```toml
[listen]
http = "0.0.0.0:8080"   # (or https = ... for TLS)
web  = true             # serve the SPA at / and mount same-origin /ws
```

The app is then served at `/`, the browser round-trips over `/ws`, and
`/.well-known/weft` + `/admin` stay carved out. The `web-ui` feature requires
`client/build` to exist (step 1); without the feature, `web = true` still mounts
`/ws` but serves no SPA.
