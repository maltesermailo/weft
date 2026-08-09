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

# Which commit this image is, stamped into the binary so its first log line says
# so (`build-images.sh` passes it). weftd and the bridge are separate images, and
# telling "the bridge predates the fix" from "the fix does not work" is otherwise
# guesswork. Absent = "unknown", so a plain `docker build` still works.
ARG WEFT_BUILD=unknown
ENV WEFT_BUILD=$WEFT_BUILD

# No system deps beyond the base image: reqwest uses rustls (no OpenSSL), and the
# daemon links nothing native.
RUN cargo build --release -p weft-matrix

FROM debian:bookworm-slim
# ca-certificates: outbound TLS to the homeserver and to weftd's media plane
# when either is fronted by HTTPS.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/weft-matrix /usr/local/bin/weft-matrix

# The appservice API the homeserver pushes transactions to.
EXPOSE 9010
# Config is a plain TOML the operator mounts + edits (same convention as weftd).
ENTRYPOINT ["weft-matrix"]
CMD ["/etc/weft/weft-matrix.toml"]
