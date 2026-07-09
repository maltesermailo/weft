# Web client + weftd embedding — design plan

Status: **design, for approval** (2026-07-09). Make the existing SvelteKit client
run three ways off **one codebase**:

1. **Desktop** — the Tauri app (today, unchanged for users).
2. **Web** — in a browser, speaking WEFT over **WebSocket** to a weftd.
3. **Embedded** — weftd serves that web build itself (single binary, no external
   web server).

Decision taken (this thread): the browser gets the protocol logic via a **WASM
client-core** — the Rust connection/auth/codec compiled to WebAssembly — *not* a
TypeScript re-port. One source of truth for the wire + crypto.

## 1. Where the logic lives today

- `client/src/**` — the SvelteKit UI. Talks to its backend through a clean,
  **structured** boundary: `invoke("verb", args)` for commands and a `WeftEvent`
  stream for updates (`lib/weft.ts`). It never sees a wire line.
- `client/src-tauri/src/weft.rs` (1530 ln) — the **protocol client**: the
  connection loop (`run_connection`), the §6.1 auth handshake **including Ed25519
  device-key signing** (`weft_crypto::sign_challenge`), the command builders
  (`build_federate`, …), and reply→`WeftEvent` parsing. Deps: `weft-proto`,
  `weft-crypto`, `weft-transport` (QUIC).
- `client/src-tauri/src/lib.rs` (623 ln) — Tauri glue: `#[tauri::command]`
  handlers + the `AppHandle`/`Emitter` event pump.
- `weftd` already has: a **WS listener** (`[listen] ws`, tokio-tungstenite,
  spec §3) the browser can connect to, and an **axum HTTP surface**
  (`wellknown.rs::router`, plus the admin API) to hang static-serving off.
- The SvelteKit app already uses **`adapter-static`** (SPA, `index.html`
  fallback) — it builds to a static bundle with no Node server.

**Key fact:** `weft-proto` and `weft-crypto` are pure (no I/O, no tokio) → they
compile to WASM as-is. Only `weft-transport` (quinn) is native-only. So the
**transport is the one seam** to abstract; everything else in `weft.rs` is
portable.

## 2. Target architecture

```
crates/weft-client-core   (new, portable — native + wasm)
  connection FSM (run loop), auth handshake (Ed25519 via weft-crypto),
  command builders, reply → ClientEvent parsing.
  Abstracts two things it does NOT own:
    trait ClientStream  { send_line / recv_line }   ← transport
    trait EventSink     { emit(ClientEvent) }        ← how updates reach the UI
  deps: weft-proto, weft-crypto     (NO weft-transport, NO tauri)

client/src-tauri          (native binding — thin)
  ClientStream = QUIC/WS via weft-transport;  EventSink = Tauri Emitter;
  #[tauri::command]s delegate to weft-client-core.

crates/weft-client-wasm   (new, wasm binding — thin, wasm-bindgen)
  ClientStream = browser WebSocket (web-sys/gloo-net);
  EventSink = a JS callback;  #[wasm_bindgen] exports the command fns.
  Built with wasm-pack → an ES module the frontend imports.

client/src/lib/weft.ts    (backend switch)
  one TS interface; picks TauriBackend (invoke) or WasmBackend (wasm module)
  at runtime by feature-detecting Tauri. UI code is untouched.
```

The `ClientEvent` enum is exactly today's `WeftEvent` moved into the core; both
bindings serialize it to the same JS shape the UI already consumes, so
`lib/types.ts` and every component stay as-is.

## 3. The two abstractions

- **`ClientStream`** — `async send_line(&str)` / `async recv_line() -> Option<String>`,
  mirroring weft-core's server-side `ControlStream` but client-facing. Native:
  wrap `weft_transport::QuicControlStream` (and the WS client). Wasm: a
  `WebSocket` whose `onmessage` feeds an async queue and `send()` writes text
  frames. The core's connection loop is transport-agnostic over this.
- **`EventSink`** — `fn emit(&self, ev: ClientEvent)`. Native: `AppHandle::emit`.
  Wasm: invoke a stored `js_sys::Function`. Replaces the current hard-coded
  Tauri `emit()`.

Command intake is symmetric: the core exposes `fn command(line: String)` (build
via the existing `build_*` fns, enqueue to the outbound side). Native invoke
handlers call it; the wasm binding exports each `build_*` + a `send`.

## 4. Crypto & device keys in the browser

`weft-crypto` (ed25519-dalek) compiles to WASM, so **password auth and
device-key AUTH KEY/PROOF both work in the browser** — no WebCrypto re-port.
Device *keypairs* need browser-side persistence: the wasm binding stores them via
**IndexedDB** (through JS), analogous to the Tauri secure store. Honest caveat:
browser storage is softer than an OS keychain — password auth is the baseline;
device-key enrollment in the web build is opt-in and documented as such.

## 5. weftd embedding

- Build: `pnpm build` (adapter-static) → `client/build/`.
- **Embed the bundle in the weftd binary** with `rust-embed` (single artifact, no
  external files to ship). A cargo feature `web-ui` gates the dependency + the
  bytes so a headless build stays lean.
- **Serve** it from the existing axum app: a fallback route returns the embedded
  asset (or `index.html` for SPA client-routing), mounted under the HTTP/HTTPS
  listener, behind a config flag `[listen] web = true`.
- **Same-origin WebSocket.** For a browser served from `https://host`, connecting
  back to `wss://host/ws` (same origin) is cleanest — so weftd should expose the
  WS upgrade as an **axum route on the HTTP/HTTPS listener** (bridging into the
  same `run_session` path the standalone `[listen] ws` socket uses), not only as
  the separate `ws` port. The wasm client derives its WS URL from
  `window.location` (`wss://<host>/ws`).

## 6. Phases (each shippable, each green)

- **P1 ✅ (2026-07-09) — extract `weft-client-core`.** New portable crate (deps:
  `weft-proto`, `weft-crypto`, `serde` — no transport, runtime, or Tauri): the
  `ClientEvent` enum, the `Mode`/`Phase` types, the `on_line` reply-parse + auth
  FSM (generalized `AppHandle` → the new **`EventSink`** trait), and all `build_*`
  command builders. `src-tauri/weft.rs` is now a **thin native binding** (1530 →
  154 lines): the tokio connection loop, the QUIC `ClientStream`, DNS `resolve`,
  and a `TauriSink: EventSink` (Tauri `emit`); it re-exports the core so
  `lib.rs`'s `weft::build_*`/`weft::Mode` are untouched. Workspace + Tauri app
  build; clippy clean. *(The connection loop stays per-binding — it uses tokio,
  which the wasm binding will replace with a JS-driven loop in P2.)*
- **P2 ✅ (2026-07-09) — wasm binding + frontend switch.** New `weft-client-wasm`
  crate (`cdylib`+`rlib`, wasm-bindgen): a `JsSink: EventSink` (serde → a JS
  callback), a **WebSocket-driven** `Conn` (the tokio run loop is replaced by
  `onopen`→HELLO / `onmessage`→`core::on_line` / `onclose`; pre-READY commands
  buffer then flush), and a `#[wasm_bindgen] WeftClient` whose `invoke(cmd, args)`
  dispatch mirrors all ~62 Tauri commands (device/namespace-key ones stubbed →
  P4). `weft.ts` now branches on `__TAURI_INTERNALS__`: desktop → Tauri
  `invoke`/`listen`/notification-plugin; browser → a lazily-loaded `WeftClient`
  (`invoke`), a fan-out `webListeners` set (`onWeft`), and the browser
  `Notification` API. Build: `npm run wasm` (`wasm-pack --target web` →
  `static/wasm`, served at `/wasm/`) wired into `dev:web`/`build:web`; desktop
  `dev`/`build` stay wasm-free. Two wasm-only toolchain notes: `getrandom` is
  pulled at **both** 0.2 (ed25519-dalek) and 0.3 (ulid→rand) — each needs its JS
  shim (`features=["js"]` / `["wasm_js"]`), and 0.3 additionally needs
  `--cfg getrandom_backend="wasm_js"`, supplied by a **wasm32-scoped**
  `.cargo/config.toml` (native builds untouched). *Green:* svelte-check clean,
  `npm run build:web` produces the SPA + wasm; browser end-to-end needs the
  same-origin `/ws` route from P3.
- **P3 ✅ (2026-07-09) — weftd embed.** New `weftd::web` module mounts two things
  onto the existing `http`/`https` axum app when `[listen] web = true`: (1) a
  **same-origin `/ws`** route — `WebSocketUpgrade` → an `AxumWsLines:
  ControlStream` adapter → the ordinary `run_session` path (one text frame = one
  line, matching `WsControlStream`); (2) an **`index.html` SPA fallback** serving
  the `client/build` bundle embedded via `rust-embed`, behind the **`web-ui`**
  cargo feature (off by default → headless builds stay lean and need no prebuilt
  SPA). Specific routes (`/ws`, `/.well-known`, `/admin`) win over the fallback;
  `.wasm` is served as `application/wasm` for streaming instantiation. One
  wrinkle solved: `run_session` needs `S: Sync` (some `&self` handlers await,
  e.g. `announce_manifest`), and axum's `WebSocket` is `!Sync` — so the adapter
  holds the `split()` sink/stream halves (each a `Send + Sync` `BiLock`) instead
  of the raw socket. Build note: `--features web-ui` requires `client/build` to
  exist (`pnpm build:web` first). *Green:* a `same_origin_ws_route` conformance
  test (full HELLO→REGISTER→JOIN→MSG echo over `/ws`), and a two-process smoke
  test — `weftd` serves `/` (HTML), `/wasm/*_bg.wasm` (`application/wasm`), SPA
  deep-link fallback, with `/.well-known/weft` still carved out.
- **P4 — device keys in the browser** (IndexedDB persistence) + polish
  (reconnect, the `weft://` deep-link handler for web).

## 7. Open decisions **[DECIDE]**

1. **Same-origin WS route vs the separate `ws` port.** Add an axum WebSocket
   upgrade on the HTTP/HTTPS listener (browser uses `wss://host/ws`, one port,
   TLS-terminated by weftd) — *recommended* — or have the browser connect to the
   standalone `[listen] ws` socket (simpler server, but a second port + its own
   TLS story). *(Rec: axum same-origin route.)*
2. **Embed vs directory-serve the SPA.** `rust-embed` into the binary
   (single-artifact deploy) vs `ServeDir` from a path (rebuild the UI without
   recompiling weftd). *(Rec: `rust-embed` behind the `web-ui` feature; a
   `[listen] web_dir` override for dev.)*
3. **Serve path.** App at `/` (weftd is the web server) vs a subpath like `/app`
   (leaves `/` free). *(Rec: `/`, with `/.well-known` + `/admin` already carved
   out.)*
4. **wasm build in CI.** wasm-pack as a vite pre-build step vs a committed
   prebuilt artifact. *(Rec: wasm-pack in the build; document the toolchain.)*
