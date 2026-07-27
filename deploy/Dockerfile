# syntax=docker/dockerfile:1
#
# weftd container, built WITH the embedded web client (`--features web-ui`).
# Build context is the repo root (see docker-compose.yml's `build.context: ..`).

# ---- builder: browser client (wasm + SPA) + weftd ----
FROM rust:1-bookworm AS builder

# Node (SvelteKit build), pnpm, and the wasm toolchain (the client bundles a
# wasm build of weft-client-wasm — see client/package.json `build:web`).
#
# Install Node from the official nodejs.org **tarball**, NOT the NodeSource apt
# repo: `deb.nodesource.com` sits behind a CDN that intermittently 403s CI runner
# IPs, which breaks `apt-get install nodejs` with a "repository is not signed"
# error mid-build. The static tarball has no apt-repo/signing dependency, is
# well-mirrored, and pins an exact version for reproducibility. All network fetches
# retry to ride out transient CDN blips.
#
# Must satisfy Vite 8's engine floor (Node >=20.19 or >=22.12, per client's
# `vite ^8`); we track the 22 LTS line.
ENV NODE_VERSION=22.19.0
RUN apt-get update \
 && apt-get install -y --no-install-recommends curl ca-certificates xz-utils \
 && rm -rf /var/lib/apt/lists/* \
 && arch="$(dpkg --print-architecture)" \
 && case "$arch" in \
      amd64) node_arch=x64 ;; \
      arm64) node_arch=arm64 ;; \
      *) echo "unsupported architecture: $arch" >&2; exit 1 ;; \
    esac \
 && curl -fsSL --retry 5 --retry-all-errors --retry-delay 2 \
      -o /tmp/node.tar.xz \
      "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-${node_arch}.tar.xz" \
 && tar -xJf /tmp/node.tar.xz -C /usr/local --strip-components=1 --no-same-owner \
 && rm /tmp/node.tar.xz \
 && npm install -g pnpm \
 && rustup target add wasm32-unknown-unknown \
 && curl -fsSL --retry 5 --retry-all-errors --retry-delay 2 \
      https://rustwasm.github.io/wasm-pack/installer/init.sh | sh \
 && node --version && pnpm --version

WORKDIR /src
COPY . .

# The weftd feature set: `web-ui` (embed the SPA) + `voice`. **`voice` is integral
# to the deployed image** — it compiles the WEFT-RT SFU AND the §16 M-lk-3b
# libwebrtc media relay (so cross-network calls / federated voice never expose a
# client's IP to the peer network). `voice` therefore always pulls libwebrtc.
ARG WEFTD_FEATURES=web-ui,voice

# libwebrtc build deps — the `voice` feature's `livekit` client SDK (`webrtc-sys`)
# downloads a prebuilt libwebrtc and links it against libc++ (list may need tuning
# per libwebrtc release).
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      clang libclang-dev libc++-dev libc++abi-dev libssl-dev pkg-config \
 && rm -rf /var/lib/apt/lists/*

# 1) Build the browser client → client/build (wasm-pack + vite). 2) Build weftd
# embedding it, with the selected features.
RUN cd client && pnpm install --frozen-lockfile && pnpm run build:web
RUN cargo build --release -p weftd --features "${WEFTD_FEATURES}"

# ---- runtime ----
FROM debian:bookworm-slim
# ca-certificates: outbound TLS (SMTP, §11.10 well-known fetch). libc++/libssl:
# the libwebrtc media relay linked into the voice build.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates libc++1 libc++abi1 libssl3 \
 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/weftd /usr/local/bin/weftd

# QUIC control plane (UDP) + HTTP (SPA, /ws, /.well-known, /media).
EXPOSE 4433/udp 8081
# Config is a plain TOML the operator mounts + edits (no .env, no templating).
ENTRYPOINT ["weftd", "/etc/weft/weft.toml"]
