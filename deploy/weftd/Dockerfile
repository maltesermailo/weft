# syntax=docker/dockerfile:1
#
# weftd container, built WITH the embedded web client (`--features web-ui`).
# Build context is the repo root (see docker-compose.yml's `build.context: ../..`).

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
#
# Both steps run behind BuildKit **cache mounts**, which is what makes a CI build
# incremental rather than from-scratch. The layer cache alone cannot help here:
# `COPY . .` above is part of these layers' cache key, so every commit
# invalidates them and they re-run regardless. What actually costs the 10+
# minutes is redoing the work — the pnpm fetch, the libwebrtc download, and a
# full cargo build — and a cache mount is the only thing that survives a
# changed layer. It is the same warm `target/` that makes the build ~3 min
# locally.
#
# The mounts are NOT part of the image, so the binary has to be copied out of
# `target/` inside the same RUN; a later `COPY --from` could not see it.
# The cargo mounts are here too, not just below: `build:web` runs wasm-pack, which
# compiles `weft-client-wasm` into the same `target/`. Without them that compile
# would be redone every build — and the next RUN masks `target/` with the mount
# anyway, so its artifacts would be thrown away regardless.
RUN --mount=type=cache,target=/pnpm-store,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cd client \
 && pnpm install --frozen-lockfile --store-dir /pnpm-store \
 && pnpm run build:web

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release -p weftd --features "${WEFTD_FEATURES}" \
 && mkdir -p /out \
 && cp /src/target/release/weftd /out/weftd

# ---- runtime ----
FROM debian:bookworm-slim
# ca-certificates: outbound TLS (SMTP, §11.10 well-known fetch). libc++/libssl:
# the libwebrtc media relay linked into the voice build.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates libc++1 libc++abi1 libssl3 \
 && rm -rf /var/lib/apt/lists/*

# From `/out`, not `target/`: the build's target dir is a cache mount and so
# is not present in the builder image.
COPY --from=builder /out/weftd /usr/local/bin/weftd

# QUIC control plane (UDP) + HTTP (SPA, /ws, /.well-known, /media).
EXPOSE 4433/udp 8081
# Config is a plain TOML the operator mounts + edits (no .env, no templating).
ENTRYPOINT ["weftd", "/etc/weft/weft.toml"]
