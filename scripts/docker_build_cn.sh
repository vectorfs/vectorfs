#!/usr/bin/env bash
# docker_build_cn.sh — scripts/docker_build.sh 的中国大陆网络变体: same
# three steps (build both binaries in a bookworm TOOLCHAIN CONTAINER, stage
# them + entrypoint + the shared pin script, `docker build` the host arch),
# with every mainland-unfriendly origin rerouted through a mirror. The
# upstream script is untouched; this one drives Dockerfile.cn (staged as
# "Dockerfile" so the staging layout is identical).
#
# Mirror knobs (env-overridable; `${VAR-default}` NOT `:-`, so
# `CN_X= scripts/docker_build_cn.sh …` explicitly selects the direct route):
#
#   CN_REGISTRY_MIRROR  Docker Hub registry mirror prefix
#                       (default docker.m.daocloud.io; docker.1ms.run etc.
#                       also work). Applied to BOTH the rust:1-bookworm
#                       toolchain container and the image's
#                       debian:bookworm-slim base (as DEBIAN_IMAGE build
#                       arg). "" = pull from docker.io directly — right when
#                       the host daemon already carries registry-mirrors.
#   CN_APT_MIRROR       Debian apt mirror hostname carrying /debian AND
#                       /debian-security (default mirrors.ustc.edu.cn;
#                       mirrors.tuna.tsinghua.edu.cn / mirrors.aliyun.com
#                       fine too). "" = deb.debian.org direct. Applied in
#                       the toolchain container AND as the image's APT_MIRROR
#                       build arg.
#   CN_CARGO_MIRROR     crates.io source replacement, written INSIDE the
#                       toolchain container only (mounted read-only at
#                       /usr/local/cargo/config.toml — the host ~/.cargo is
#                       never touched; only the registry/ cache is shared,
#                       and a replaced source caches under its own id, so
#                       crates.io and mirror warm caches coexist):
#                         rsproxy (default)  sparse+https://rsproxy.cn/index/
#                         ustc               sparse+https://mirrors.ustc.edu.cn/crates.io-index/
#                         ""                 direct crates.io
#   CN_GH_PROXY         prefix prepended to github.com release URLs inside
#                       the image build (ort/pdfium .so fetch; stage libs).
#                       Default https://ghfast.top/ — these proxies rotate,
#                       override when dead. Integrity is unaffected: every
#                       artifact still passes the runtime_lib_pins.sh
#                       sha256 gate. "" = github.com direct.
#
# The built image additionally defaults HF_ENDPOINT=https://hf-mirror.com
# (Dockerfile.cn) for the first-boot model prefetch; `docker run -e
# HF_ENDPOINT=` restores huggingface.co.
#
# Usage: scripts/docker_build_cn.sh [cargo-profile] [image-tag]
#   cargo-profile : debug (default) | release | release-prod | …
#   image-tag     : default vectorfs:dev-cn
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE="${1:-debug}"
TAG="${2:-vectorfs:dev-cn}"

CN_REGISTRY_MIRROR="${CN_REGISTRY_MIRROR-docker.m.daocloud.io}"
CN_APT_MIRROR="${CN_APT_MIRROR-mirrors.ustc.edu.cn}"
CN_CARGO_MIRROR="${CN_CARGO_MIRROR-rsproxy}"
CN_GH_PROXY="${CN_GH_PROXY-https://ghfast.top/}"

# crates.io source-replacement config mounted into the toolchain container
# (one sparse registry — no [registries] alias needed; plain replace-with).
CARGO_MIRROR_CONF="$(mktemp)"
trap 'rm -f "$CARGO_MIRROR_CONF"' EXIT
case "$CN_CARGO_MIRROR" in
    rsproxy)
        cat > "$CARGO_MIRROR_CONF" <<'EOF'
[source.crates-io]
replace-with = 'mirror-sparse'

[source.mirror-sparse]
registry = "sparse+https://rsproxy.cn/index/"
EOF
        ;;
    ustc)
        cat > "$CARGO_MIRROR_CONF" <<'EOF'
[source.crates-io]
replace-with = 'mirror-sparse'

[source.mirror-sparse]
registry = "sparse+https://mirrors.ustc.edu.cn/crates.io-index/"
EOF
        ;;
    "")
        echo "== docker_build_cn: CN_CARGO_MIRROR empty — direct crates.io" ;;
    *)
        echo "docker_build_cn: CN_CARGO_MIRROR='${CN_CARGO_MIRROR}' not in {rsproxy,ustc,\"\"}" >&2
        exit 2 ;;
esac

# Image refs: mirror-prefixed (docker.io official images live under library/).
if [[ -n "$CN_REGISTRY_MIRROR" ]]; then
    RUST_IMAGE="${CN_REGISTRY_MIRROR}/library/rust:1-bookworm"
    DEBIAN_IMAGE="${CN_REGISTRY_MIRROR}/library/debian:bookworm-slim"
else
    RUST_IMAGE="rust:1-bookworm"
    DEBIAN_IMAGE="debian:bookworm-slim"
fi

# apt mirror sed, run FIRST inside the toolchain container (same hostname
# swap Dockerfile.cn applies to the image layers).
APT_SED=""
if [[ -n "$CN_APT_MIRROR" ]]; then
    APT_SED="sed -i 's|deb.debian.org|${CN_APT_MIRROR}|g; s|security.debian.org|${CN_APT_MIRROR}|g' /etc/apt/sources.list.d/debian.sources && "
fi

case "$PROFILE" in
    debug) BIN_DIR="target-docker/debug" ;;
    *)     BIN_DIR="target-docker/${PROFILE}" ;;
esac

case "$(uname -m)" in
    x86_64)  ARCH="amd64" ;;
    aarch64) ARCH="arm64" ;;
    *) echo "docker_build_cn: unsupported host arch $(uname -m)" >&2; exit 2 ;;
esac

echo "== docker_build_cn: profile=${PROFILE} arch=${ARCH} tag=${TAG}"
echo "== docker_build_cn: registry=${CN_REGISTRY_MIRROR:-<direct>} apt=${CN_APT_MIRROR:-<direct>} cargo=${CN_CARGO_MIRROR:-<direct>} gh=${CN_GH_PROXY:-<direct>}"
if [[ "${HOST_BUILD:-0}" == "1" ]]; then
    # escape hatch: host toolchain (only sane when host glibc <= image glibc).
    # crates.io routing here is the HOST's own ~/.cargo config — this script
    # never writes outside the toolchain container.
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
    # ships neither. protoc comes from the apt mirror (bookworm's 3.21.12
    # satisfies the 3.12..3.21 window CI pins 21.12 for), no GitHub fetch.
    # -e CARGO_NET_RETRY: crates mirror hiccups redeem like GitHub ones.
    docker run --rm \
        -v "$PWD:/work" -w /work \
        -e CARGO_TARGET_DIR=/work/target-docker \
        -e CARGO_NET_RETRY=5 \
        -v "${CARGO_HOME}/registry:/usr/local/cargo/registry" \
        -v "${CARGO_MIRROR_CONF}:/usr/local/cargo/config.toml:ro" \
        "$RUST_IMAGE" \
        bash -c "${APT_SED}apt-get update -qq && apt-get install -y -qq --no-install-recommends cmake protobuf-compiler libprotobuf-dev >/dev/null && ${BUILD_CMD[*]}"
fi
for bin in vectorfs vf; do
    [[ -x "${BIN_DIR}/${bin}" ]] || { echo "docker_build_cn: ${BIN_DIR}/${bin} missing" >&2; exit 1; }
done

STAGE="target/docker-stage"
rm -rf "$STAGE"
OTHER_ARCH="$([[ "$ARCH" == amd64 ]] && echo arm64 || echo amd64)"
mkdir -p "${STAGE}/${ARCH}" "${STAGE}/${OTHER_ARCH}"
cp "${BIN_DIR}/vectorfs" "${BIN_DIR}/vf" "${STAGE}/${ARCH}/"
# staged AS "Dockerfile" — the staging contract (CI + upstream script) is
# name-exact, and the build then needs no -f.
cp Dockerfile.cn "$STAGE/Dockerfile"
cp deploy/entrypoint.sh scripts/runtime_lib_pins.sh "$STAGE/"

# TARGETARCH is passed explicitly so legacy (non-BuildKit) builders behave
# like BuildKit; with BuildKit it matches the automatic value anyway. The
# mirror args accept "" (Dockerfile.cn treats empty = direct origin).
docker build \
    --build-arg TARGETARCH="$ARCH" \
    --build-arg DEBIAN_IMAGE="$DEBIAN_IMAGE" \
    --build-arg APT_MIRROR="$CN_APT_MIRROR" \
    --build-arg GH_PROXY="$CN_GH_PROXY" \
    -t "$TAG" "$STAGE"
echo "== docker_build_cn: built ${TAG} (try: docker run --rm ${TAG} vectorfs --version)"
