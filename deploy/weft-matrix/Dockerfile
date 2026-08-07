# syntax=docker/dockerfile:1
#
# weft-matrix: the Matrix bridge daemon (an appservice to a companion homeserver
# on one side, a weftd provider session on the other). Build context is the repo
# root — see docker-compose.yml's `build.context: ../..`.
#
# Note the toolchain: this crate declares `rust-version = 1.85`, above the
# workspace's 1.75 MSRV, because it rides ruma. `rust:1-bookworm` is current
# stable, so it satisfies both.

FROM rust:1-bookworm AS builder

WORKDIR /src
COPY . .

# No system deps beyond the base image: reqwest uses rustls (no OpenSSL), and the
# daemon links nothing native.
#
# Cache mounts, for the reason spelled out in ../weftd/Dockerfile: `COPY . .` is
# in this layer's cache key, so the layer cache can never spare this build — only
# a warm registry + `target/` can, and ruma is a long compile to redo per commit.
# The binary is copied out of the mount because a mount is not in the image.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release -p weft-matrix \
 && mkdir -p /out \
 && cp /src/target/release/weft-matrix /out/weft-matrix

FROM debian:bookworm-slim
# ca-certificates: outbound TLS to the homeserver and to weftd's media plane
# when either is fronted by HTTPS.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /out/weft-matrix /usr/local/bin/weft-matrix

# The appservice API the homeserver pushes transactions to.
EXPOSE 9010
# Config is a plain TOML the operator mounts + edits (same convention as weftd).
ENTRYPOINT ["weft-matrix"]
CMD ["/etc/weft/weft-matrix.toml"]
