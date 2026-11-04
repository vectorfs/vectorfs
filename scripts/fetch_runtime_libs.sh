#!/usr/bin/env bash
# fetch_runtime_libs.sh — pinned downloader for the dynamic-load runtime
# libraries (design.md §5.7 "pdfium-render (.so load-dynamic)", §5.10.7
# ".so pin 版本 + SHA"): fetches each named library's PINNED release artifact,
# verifies its sha256 BEFORE extraction, and unpacks only the shared library
# into the repo-local, gitignored runtime-libs/ directory.
#
# Usage: scripts/fetch_runtime_libs.sh <name>...   (name: pdfium | onnxruntime)
#
# Pinning discipline (§5.10.7): the version+sha256 table lives in
# scripts/runtime_lib_pins.sh — the SINGLE source shared with the Dockerfile
# (the image installs the same pins; see that file's header for why sharing
# is load-bearing). Bumping a pin = editing the pins script; CI caches
# runtime-libs/ keyed on the pins via the stamp file.
#
# Idempotence: after a successful fetch we write runtime-libs/<name>.stamp
# containing the pinned version+sha; a later run with a matching stamp AND an
# existing library file skips the download entirely (CI cache hit path).
#
# Sources:
#   pdfium — bblanchon/pdfium-binaries (BSD-3-Clause / Apache-2.0 Licensed
#   PDFium builds; accepted per the task dependency ruling: pdfium binaries
#   are BSD-3, allowed, unlike GPL/AGPL/CC-BY-NC which stay banned).
#   onnxruntime — microsoft/onnxruntime official GitHub releases (MIT).
#   v1.31.0 is pinned to MATCH the `ort` crate pin (ort-sys 2.0.0-rc.15
#   bundles ORT 1.31.0 in its dist table); ort's dylib loader refuses older
#   minors, so bumping one pin without the other fails at dlopen time.
set -euo pipefail
cd "$(dirname "$0")/.."

# shared pin table (also sourced by the Dockerfile build stage)
# shellcheck source=runtime_lib_pins.sh
source "$(dirname "$0")/runtime_lib_pins.sh"

RUNTIME_DIR="runtime-libs"

# uname -> pins-script platform axis
os_of() { case "$(uname -s)" in Linux) echo linux ;; Darwin) echo mac ;; *) echo "unsupported OS: $(uname -s)" >&2; return 1 ;; esac; }
arch_of() { case "$(uname -m)" in x86_64) echo x64 ;; aarch64|arm64) echo arm64 ;; *) echo "unsupported arch: $(uname -m)" >&2; return 1 ;; esac; }

fetch_one() {
    local name="$1" spec url asset sha member lib_name
    spec="$(runtime_lib_spec "$name" "$(os_of)" "$(arch_of)")" || return 1
    # pins row shape: "<download-url> <sha256> <archive-member> <installed-name>"
    read -r url sha member lib_name <<<"$spec"
    asset="$(basename "$url")"

    local stamp="${RUNTIME_DIR}/${name}.stamp"
    # stamp line keeps the pre-refactor byte shape (release + asset + sha) so
    # existing CI caches stay valid across this refactor
    local release
    release="$(release_of "$name")"
    local stamp_line="${release} ${asset} ${sha}"
    if [[ -f "${RUNTIME_DIR}/${lib_name}" && -f "$stamp" && "$(cat "$stamp")" == "$stamp_line" ]]; then
        echo "${name}: ${RUNTIME_DIR}/${lib_name} already at pinned ${release} — skip"
        return 0
    fi

    local tmp
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' RETURN

    echo "${name}: downloading ${url}"
    # retry + http1.1: GitHub release CDNs intermittently drop HTTP/2
    # streams; a pin-verified artifact redeems itself on retry
    curl -fsSL --retry 5 --retry-all-errors --retry-delay 2 --http1.1 \
        -o "${tmp}/${asset}" "${url}"

    echo "${name}: verifying sha256"
    echo "${sha}  ${tmp}/${asset}" | sha256sum -c - >/dev/null

    # Extract ONLY the library member (headers/cmake stay out). macOS archives
    # carry a "./" path prefix, Linux ones do not — strip it either way.
    mkdir -p "$RUNTIME_DIR"
    tar -xzf "${tmp}/${asset}" -C "$tmp" "${member#./}"
    mv "${tmp}/${member#./}" "${RUNTIME_DIR}/${lib_name}"
    printf '%s\n' "$stamp_line" >"$stamp"
    echo "${name}: installed ${RUNTIME_DIR}/${lib_name} (${release}, sha256 verified)"
}

release_of() {
    case "$1" in
        pdfium)      echo "$PDFIUM_RELEASE" ;;
        onnxruntime) echo "$ORT_RELEASE" ;;
    esac
}

if [[ $# -eq 0 ]]; then
    echo "usage: $0 <name>...   (available: pdfium, onnxruntime)" >&2
    exit 2
fi

for name in "$@"; do
    case "$name" in
        pdfium|onnxruntime) fetch_one "$name" ;;
        *) echo "unknown library '${name}' (available: pdfium, onnxruntime)" >&2; exit 2 ;;
    esac
done
