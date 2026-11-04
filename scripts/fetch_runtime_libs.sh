#!/usr/bin/env bash
# fetch_runtime_libs.sh — pinned downloader for the dynamic-load runtime
# libraries (design.md §5.7 "pdfium-render (.so load-dynamic)", §5.10.7
# ".so pin 版本 + SHA"): fetches each named library's PINNED release artifact,
# verifies its sha256 BEFORE extraction, and unpacks only the shared library
# into the repo-local, gitignored runtime-libs/ directory.
#
# Usage: scripts/fetch_runtime_libs.sh <name>...   (name: pdfium | onnxruntime*)
#        * onnxruntime lands with M2-T5; the case arm is a placeholder that
#          fails loudly so callers cannot think it worked.
#
# Pinning discipline (§5.10.7): version + sha256 are HARD-CODED below — no
# floating "latest". Bumping a pin = editing this script (the sha is the trust
# root; CI caches runtime-libs/ keyed on the pins via the stamp file).
#
# Idempotence: after a successful fetch we write runtime-libs/<name>.stamp
# containing the pinned version+sha; a later run with a matching stamp AND an
# existing library file skips the download entirely (CI cache hit path).
#
# Sources:
#   pdfium — bblanchon/pdfium-binaries (BSD-3-Clause / Apache-2.0 Licensed
#   PDFium builds; accepted per the task dependency ruling: pdfium binaries
#   are BSD-3, allowed, unlike GPL/AGPL/CC-BY-NC which stay banned).
set -euo pipefail
cd "$(dirname "$0")/.."

RUNTIME_DIR="runtime-libs"
PDFIUM_RELEASE="chromium/8066"   # PDFium 156.0.8066.0 (2026-09-21)
PDFIUM_BASE_URL="https://github.com/bblanchon/pdfium-binaries/releases/download/${PDFIUM_RELEASE}"

# Pinned sha256 per release asset. NOTE the macOS artifact ships libpdfium.dylib
# while Linux ships libpdfium.so — the unpacked file name follows the platform.
pdfium_asset() {
    case "$(uname -s)/$(uname -m)" in
        Linux/x86_64)  echo "pdfium-linux-x64.tgz 0b43f405477cf2cfc4dbff06905093c3309756c6bca1fb9da99234a2ca97fed2 libpdfium.so" ;;
        Linux/aarch64) echo "pdfium-linux-arm64.tgz 0e6f90dccbc6b81fd5d7106abaf164c4222178f024c204d00d526b60fd2ad535 libpdfium.so" ;;
        Darwin/arm64)  echo "pdfium-mac-arm64.tgz 336219e80580b93c6523f44db7dc1de59cc497b13a7390ddac84223f68ca162b libpdfium.dylib" ;;
        Darwin/x86_64) echo "pdfium-mac-x64.tgz 841ecac278cdd46288dd065873522cf72f3996560d8978f473d336f01d59942c libpdfium.dylib" ;;
        *) echo "unsupported platform for pdfium: $(uname -s)/$(uname -m)" >&2; return 1 ;;
    esac
}

fetch_pdfium() {
    local spec lib_name
    spec="$(pdfium_asset)" || return 1
    read -r asset sha lib_name <<<"$spec"
    if [[ "$sha" == "0" ]]; then
        echo "fetch_runtime_libs: pdfium asset '$asset' has no pinned sha yet — pin it in this script (design.md §5.10.7)" >&2
        return 1
    fi

    local stamp="${RUNTIME_DIR}/pdfium.stamp"
    local stamp_line="${PDFIUM_RELEASE} ${asset} ${sha}"
    if [[ -f "${RUNTIME_DIR}/${lib_name}" && -f "$stamp" && "$(cat "$stamp")" == "$stamp_line" ]]; then
        echo "pdfium: ${RUNTIME_DIR}/${lib_name} already at pinned ${PDFIUM_RELEASE} — skip"
        return 0
    fi

    local tmp
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' RETURN

    echo "pdfium: downloading ${PDFIUM_BASE_URL}/${asset}"
    curl -sSL --fail -o "${tmp}/${asset}" "${PDFIUM_BASE_URL}/${asset}"

    echo "pdfium: verifying sha256"
    echo "${sha}  ${tmp}/${asset}" | sha256sum -c - >/dev/null

    # Extract ONLY the shared library member (lib/libpdfium.*); the archive
    # also carries headers/cmake we do not want in runtime-libs/.
    mkdir -p "$RUNTIME_DIR"
    tar -xzf "${tmp}/${asset}" -C "$tmp" "lib/${lib_name}"
    mv "${tmp}/lib/${lib_name}" "${RUNTIME_DIR}/${lib_name}"
    printf '%s\n' "$stamp_line" >"$stamp"
    echo "pdfium: installed ${RUNTIME_DIR}/${lib_name} (${PDFIUM_RELEASE}, sha256 verified)"
}

fetch_onnxruntime() {
    echo "fetch_runtime_libs: onnxruntime lands with M2-T5 (design.md §5.10.2 embedder kinds); not fetchable yet" >&2
    return 1
}

if [[ $# -eq 0 ]]; then
    echo "usage: $0 <name>...   (available: pdfium)" >&2
    exit 2
fi

for name in "$@"; do
    "fetch_${name}"
done
