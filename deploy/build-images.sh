#!/usr/bin/env bash
#
# Build (and optionally push) both deployment images: weftd and the Matrix bridge.
#
#   ./deploy/build-images.sh                    both, for THIS machine's arch, kept local
#   ./deploy/build-images.sh --push             both, linux/amd64, pushed to ghcr.io
#   ./deploy/build-images.sh --only weftd       just one
#   ./deploy/build-images.sh --tag v0.2.0       a tag other than :latest
#
# Env overrides: OWNER, REGISTRY, TAG, PLATFORM.
#
# Why the default is local-only: a push has to match the *server's* architecture,
# and getting that wrong produces an image that only fails at `docker run` time
# with `exec format error`. So pushing is opt-in and defaults to linux/amd64,
# while a plain run builds for whatever you are on — right for testing, wrong for
# deploying from an Apple Silicon machine.

set -euo pipefail

cd "$(dirname "$0")/.."   # repo root: both Dockerfiles take it as their context

REGISTRY="${REGISTRY:-ghcr.io}"
TAG="${TAG:-latest}"
# Owner from the git remote so this is not hardcoded to one fork.
OWNER="${OWNER:-$(git remote get-url origin 2>/dev/null |
  sed -E 's#^(https://[^/]+/|git@[^:]+:)##; s#/[^/]+$##; s#\.git$##')}"

PUSH=false
ONLY=""
PLATFORM="${PLATFORM:-}"
BUILD_ID="$(git describe --always --dirty 2>/dev/null || echo unknown)"

while [ $# -gt 0 ]; do
  case "$1" in
    --push)     PUSH=true ;;
    --only)     ONLY="${2:?--only needs an image: weftd|matrix}"; shift ;;
    --tag)      TAG="${2:?--tag needs a value}"; shift ;;
    --platform) PLATFORM="${2:?--platform needs a value}"; shift ;;
    # Print the header comment and stop at the first line of code, so the help
    # cannot drift out of sync with the block above.
    -h|--help)  awk 'NR>1 && /^#/ { sub(/^# ?/, ""); print; next } NR>1 { exit }' "$0"; exit 0 ;;
    *)          echo "unknown argument: $1 (try --help)" >&2; exit 2 ;;
  esac
  shift
done

if [ -z "$OWNER" ]; then
  echo "cannot determine the registry owner — set OWNER=<user-or-org>" >&2
  exit 2
fi

# A push targets the server, which is amd64 unless you know otherwise.
if [ -z "$PLATFORM" ] && [ "$PUSH" = true ]; then
  PLATFORM="linux/amd64"
fi

# Cross-building runs the whole compile under QEMU. For this build — clang plus a
# prebuilt libwebrtc — that is not a small tax, so say so rather than let it look
# like a hang half an hour in.
host_arch="$(uname -m)"
case "$host_arch" in
  arm64|aarch64) host_platform="linux/arm64" ;;
  x86_64|amd64)  host_platform="linux/amd64" ;;
  *)             host_platform="" ;;
esac
if [ -n "$PLATFORM" ] && [ -n "$host_platform" ] && [ "$PLATFORM" != "$host_platform" ]; then
  echo "!! building $PLATFORM on $host_platform — emulated, expect tens of minutes." >&2
  echo "!! for iteration, drop --push and build natively; to deploy fast, build on an" >&2
  echo "!! amd64 host (or let CI do it)." >&2
  echo >&2
fi

# `--platform` and `--push` both need the container driver; the default `docker`
# driver cannot cross-build or write a manifest to a registry.
if [ -n "$PLATFORM" ] || [ "$PUSH" = true ]; then
  docker buildx inspect weft >/dev/null 2>&1 ||
    docker buildx create --name weft --driver docker-container --bootstrap >/dev/null
  BUILDER=(--builder weft)
else
  BUILDER=()
fi

if [ "$PUSH" = true ]; then
  # Fail here rather than after a long build.
  docker login "$REGISTRY" --get-login >/dev/null 2>&1 || true
  echo "→ pushing to $REGISTRY/$OWNER as :$TAG"
fi

build_one() {
  local name="$1" dockerfile="$2"
  local image="$REGISTRY/$OWNER/$name"

  echo
  echo "=== $image:$TAG ${PLATFORM:+($PLATFORM)} ==="

  local args=(build)
  # `"${arr[@]}"` on an *empty* array is an unbound-variable error under `set -u`
  # in bash 3.2 — which is what macOS ships — so the length is checked first.
  # Without this the local (no-builder) path died silently after printing its
  # header.
  [ ${#BUILDER[@]} -gt 0 ] && args+=("${BUILDER[@]}")
  args+=(
    --file "$dockerfile"
    --tag "$image:$TAG"
    # Both daemons log this on startup, so a running container can be matched to a
    # commit. `-dirty` because a locally built image often is.
    --build-arg "WEFT_BUILD=$BUILD_ID"
  )
  [ -n "$PLATFORM" ] && args+=(--platform "$PLATFORM")

  if [ "$PUSH" = true ]; then
    args+=(--push)
  elif [ ${#BUILDER[@]} -gt 0 ]; then
    # The container driver keeps results in its own store; --load hands the image
    # to the local daemon so `docker run` and compose can see it.
    args+=(--load)
  fi

  docker buildx "${args[@]}" .
}

# Sequential on purpose: both are CPU-bound Rust builds, so running them at once
# on one machine just splits the same cores and makes each look hung for twice as
# long. Interleaved output would also make a failure hard to attribute.
case "$ONLY" in
  weftd)  build_one weft-weftd  deploy/weftd/Dockerfile ;;
  matrix) build_one weft-matrix deploy/weft-matrix/Dockerfile ;;
  "")     build_one weft-weftd  deploy/weftd/Dockerfile
          build_one weft-matrix deploy/weft-matrix/Dockerfile ;;
  *)      echo "--only takes weftd or matrix, got: $ONLY" >&2; exit 2 ;;
esac

echo
echo "done."
if [ "$PUSH" = true ]; then
  echo "deploy with:"
  echo "  ssh <server> 'cd deploy/weftd && docker compose pull weftd && docker compose up -d weftd'"
fi
