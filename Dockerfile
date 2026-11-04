# vectorfs container image (M2-T8, design.md §5.10.7 — read that section
# alongside this file; it is the deployment contract this image implements).
#
# Build model: the image NEVER COMPILES. Both binaries are prebuilt
# (release-prod in CI, any profile locally) and staged next to this file by
#   * CI:      .github/workflows/docker.yml  (per-target builds -> staging)
#   * locally: scripts/docker_build.sh <cargo-profile> [tag]
# which then run `docker build` against the STAGING directory (this file is
# copied there). Rationale: keeps the image minimal (no toolchain layers),
# keeps the build hermetic (same binary set as the tarball release), and the
# binaries stay the release-prod artifacts size_guard.sh gated.
#
# Expected staging context:
#   Dockerfile  entrypoint.sh  runtime_lib_pins.sh
#   amd64/vectorfs  amd64/vf   (x86_64-unknown-linux-gnu build)
#   arm64/vectorfs  arm64/vf   (aarch64-unknown-linux-gnu build; an EMPTY
#                               arm64/ is acceptable for single-arch local
#                               builds — only ${TARGETARCH} is copied)
#
# Requires BuildKit (docker 23+ default): TARGETARCH + COPY --chmod. No
# `# syntax=` directive on purpose: the built-in frontend of any BuildKit
# docker supports both, and the directive forces a frontend-image pull that
# breaks on hosts with an unreachable/foreign credential helper.

# ---------------------------------------------------------------------------
# Stage 1 — fetch the PINNED dynamic-load runtime libraries (§5.10.7 ".so pin
# 版本 + SHA"). The pin table is scripts/runtime_lib_pins.sh — the SAME file
# scripts/fetch_runtime_libs.sh sources for dev/CI — so the image and the dev
# tree can never drift onto different ort/pdfium builds (ort's loader refuses
# mismatched minors). curl + ca-certificates live ONLY in this stage.
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS libs
ARG TARGETARCH
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY runtime_lib_pins.sh /tmp/runtime_lib_pins.sh
# curl hardening: --retry + --http1.1 (GitHub release CDNs intermittently
# drop HTTP/2 streams; a pin-verified artifact redeems itself on retry)
RUN set -eux; \
    mkdir -p /out; \
    . /tmp/runtime_lib_pins.sh; \
    for lib in pdfium onnxruntime; do \
        set -- $(runtime_lib_spec_docker "$lib" "${TARGETARCH}"); \
        url="$1"; sha="$2"; member="$3"; out="$4"; \
        echo "fetching ${url}"; \
        curl -fsSL --retry 5 --retry-all-errors --retry-delay 2 --http1.1 \
            "$url" -o /tmp/lib.tgz; \
        echo "${sha}  /tmp/lib.tgz" | sha256sum -c -; \
        member="${member#./}"; \
        tar -xzf /tmp/lib.tgz -C /tmp "$member"; \
        mv "/tmp/$member" "/out/$out"; \
    done; \
    ls -la /out

# ---------------------------------------------------------------------------
# Stage 2 — the runtime image.
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim

# ca-certificates: outbound HTTPS (openai embedder, S3 backends, the
# first-boot model prefetch). The daemon's own TLS is rustls with bundled
# webpki roots, so this is belt-and-braces for anything that reads the
# system store.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    # non-root runtime user (uid/gid 1000, fixed for volume ownership)
    && groupadd --gid 1000 vectorfs \
    && useradd --uid 1000 --gid vectorfs --create-home vectorfs \
    && mkdir -p /vectorfs/data /vectorfs/models /usr/local/lib/vectorfs \
    && chown -R vectorfs:vectorfs /vectorfs

# Prebuilt binaries from the staging context, selected per platform.
ARG TARGETARCH
COPY amd64/ /stage/amd64/
COPY arm64/ /stage/arm64/
RUN cp "/stage/${TARGETARCH}/vectorfs" "/stage/${TARGETARCH}/vf" /usr/local/bin/ \
    && chmod 0755 /usr/local/bin/vectorfs /usr/local/bin/vf \
    && rm -rf /stage

# Pinned runtime libraries (stage 1), pinned to explicit paths the
# load-dynamic resolvers honor (VF_ORT_PATH / VF_PDFIUM_PATH).
COPY --from=libs /out/ /usr/local/lib/vectorfs/

# Container contract (§5.10.7):
#   VF_LISTEN=0.0.0.0  host-only form: -p port mapping reaches the daemon;
#                      the port itself still follows VF_PORT / --port
#   VF_DATA_DIR        default data root = the /vectorfs/data volume
ENV VF_ORT_PATH=/usr/local/lib/vectorfs/libonnxruntime.so \
    VF_PDFIUM_PATH=/usr/local/lib/vectorfs/libpdfium.so \
    VF_LISTEN=0.0.0.0 \
    VF_DATA_DIR=/vectorfs/data

COPY --chmod=0755 entrypoint.sh /usr/local/bin/vectorfs-entrypoint

USER vectorfs
WORKDIR /vectorfs/data
# Models live in a VOLUME, never in an image layer (§5.10.7; the ~1.17 GiB
# gte model would bloat every pull and pin the license artifact to the
# image). air-gapped derivations mount their own model set here.
VOLUME ["/vectorfs/data", "/vectorfs/models"]
EXPOSE 8787

# Zero-extra-dependency healthcheck: `vf` is already in the image and probes
# the no-auth GET /health (credential-less by design — §5.10.7 "healthcheck =
# GET /health"; vf status degrades to the health-only probe without
# VF_ACCESS_KEY). Shell form so the port follows VF_PORT.
# start-period only covers daemon startup — a first-boot text-rag init
# (model download) legitimately stays "starting"/unhealthy until it finishes.
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
    CMD vf status --endpoint "http://127.0.0.1:${VF_PORT:-8787}" >/dev/null || exit 1

# exec-form entrypoint: the script `exec`s the daemon, so `vectorfs` runs as
# PID 1 and receives SIGTERM directly (tokio's signal handler drains ingest
# and shuts down inside Docker's 10s grace — §5.10.5). No tini needed: no
# orphan-reaping responsibility exists for a single-process container.
ENTRYPOINT ["/usr/local/bin/vectorfs-entrypoint"]
