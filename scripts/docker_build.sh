#!/usr/bin/env bash
# docker_build.sh — build the vectorfs image LOCALLY (development shape of
# what .github/workflows/docker.yml does in CI).
#
# The Dockerfile NEVER compiles (see its header); this script
#   1. builds both binaries IN A debian:bookworm TOOLCHAIN CONTAINER — the
#      host glibc is often NEWER than the image's (the binaries link against
#      the build host's symbols), so host-built binaries can refuse to run
#      inside debian:bookworm-slim. Building in rust:1-bookworm makes the
#      staging binaries glibc-identical to the runtime image by construction.
#      The cargo REGISTRY and TARGET dirs are host-mounted, so warm rebuilds
#      are fast; pass HOST_BUILD=1 to skip the container and stage host
#      binaries (only sane when the host glibc <= the image's),
#   2. stages binaries + deploy/entrypoint.sh + the shared pin script into
#      target/docker-stage/ (the layout the Dockerfile expects),
#   3. runs `docker build` against the staging dir for the HOST arch.
#
# Usage: scripts/docker_build.sh [cargo-profile] [image-tag]
#   cargo-profile : debug (default) | release | release-prod | …
#   image-tag     : default vectorfs:dev
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE="${1:-debug}"
TAG="${2:-vectorfs:dev}"

case "$PROFILE" in
    debug) BIN_DIR="target-docker/debug" ;;
    *)     BIN_DIR="target-docker/${PROFILE}" ;;
esac

case "$(uname -m)" in
    x86_64)  ARCH="amd64" ;;
    aarch64) ARCH="arm64" ;;
    *) echo "docker_build: unsupported host arch $(uname -m)" >&2; exit 2 ;;
esac

echo "== docker_build: profile=${PROFILE} arch=${ARCH} tag=${TAG}"
if [[ "${HOST_BUILD:-0}" == "1" ]]; then
    # escape hatch: host toolchain (only sane when host glibc <= image glibc)
    if [[ "$PROFILE" == "debug" ]]; then
        cargo build --workspace
    else
        cargo build --profile "$PROFILE" --workspace
    fi
    BIN_DIR="target/${PROFILE}"; [[ "$PROFILE" == "debug" ]] && BIN_DIR="target/debug"
else
    CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
    if [[ "$PROFILE" == "debug" ]]; then BUILD_CMD=(cargo build --workspace); BIN_DIR="target-docker/debug";
    else BUILD_CMD=(cargo build --profile "$PROFILE" --workspace); BIN_DIR="target-docker/${PROFILE}"; fi
    # apt deps: cmake (aws-lc-sys C build) + protobuf-compiler AND
    # libprotobuf-dev (lance-encoding/prost needs the well-known .proto
    # include dir too — same pair every CI job installs) — the rust image
    # ships neither
    docker run --rm \
        -v "$PWD:/work" -w /work \
        -e CARGO_TARGET_DIR=/work/target-docker \
        -v "${CARGO_HOME}/registry:/usr/local/cargo/registry" \
        rust:1-bookworm \
        bash -c "apt-get update -qq && apt-get install -y -qq --no-install-recommends cmake protobuf-compiler libprotobuf-dev >/dev/null && ${BUILD_CMD[*]}"
fi
for bin in vectorfs vf; do
    [[ -x "${BIN_DIR}/${bin}" ]] || { echo "docker_build: ${BIN_DIR}/${bin} missing" >&2; exit 1; }
done

STAGE="target/docker-stage"
rm -rf "$STAGE"
OTHER_ARCH="$([[ "$ARCH" == amd64 ]] && echo arm64 || echo amd64)"
mkdir -p "${STAGE}/${ARCH}" "${STAGE}/${OTHER_ARCH}"
cp "${BIN_DIR}/vectorfs" "${BIN_DIR}/vf" "${STAGE}/${ARCH}/"
cp Dockerfile deploy/entrypoint.sh scripts/runtime_lib_pins.sh "$STAGE/"

# TARGETARCH is passed explicitly so legacy (non-BuildKit) builders behave
# like BuildKit; with BuildKit it matches the automatic value anyway.
docker build --build-arg TARGETARCH="$ARCH" -t "$TAG" "$STAGE"
echo "== docker_build: built ${TAG} (try: docker run --rm ${TAG} vectorfs --version)"
