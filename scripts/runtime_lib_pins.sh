#!/usr/bin/env bash
# runtime_lib_pins.sh — the SINGLE SOURCE OF TRUTH for the pinned dynamic-load
# runtime library downloads (design.md §5.10.7 ".so pin 版本 + SHA"):
# version + sha256 are HARD-CODED here and NOWHERE ELSE. Two consumers share
# this table so a pin bump lands everywhere at once:
#   * scripts/fetch_runtime_libs.sh  — repo-local runtime-libs/ for dev/CI
#     (adds the stamp/idempotence layer on top),
#   * Dockerfile (COPY'd into the build context) — installs the .so files
#     into /usr/local/lib/vectorfs/ inside the image, keyed on TARGETARCH.
# That sharing is the whole point of this file: the image and the dev tree
# can never drift onto different ort/pdfium builds (ort's dylib loader
# refuses mismatched minors, so drift would fail at dlopen time in prod).
#
# API (sourced, not run):
#   runtime_lib_spec <pdfium|onnxruntime> <os> <arch>
#       os   ∈ linux | mac
#       arch ∈ x64 | arm64
#     prints one line: "<download-url> <sha256> <archive-member> <installed-name>"
#     (archive member = path inside the tarball; installed name = the plain
#     soname ort/pdfium dlopen expects).
#   runtime_lib_spec_docker <pdfium|onnxruntime> <TARGETARCH>
#       TARGETARCH ∈ amd64 | arm64 (Docker buildx automatic arg) — maps to
#       the linux/x64|arm64 rows. macOS rows exist only for the dev script;
#       the image is linux-only by design.
#
# Sources (see fetch_runtime_libs.sh header for the license notes):
#   pdfium       — bblanchon/pdfium-binaries, release chromium/8066
#   onnxruntime  — microsoft/onnxruntime, release v1.31.0 (PAIRS WITH the
#                  `ort` crate pin ort-sys 2.0.0-rc.15, which bundles ORT
#                  1.31.0; bump both together or dlopen refuses the library)

PDFIUM_RELEASE="chromium/8066"   # PDFium 156.0.8066.0 (2026-09-21)
PDFIUM_BASE_URL="https://github.com/bblanchon/pdfium-binaries/releases/download/${PDFIUM_RELEASE}"
ORT_RELEASE="v1.31.0"            # ONNX Runtime 1.31.0 (2026-10-09); pairs with ort-sys 2.0.0-rc.15
ORT_BASE_URL="https://github.com/microsoft/onnxruntime/releases/download/${ORT_RELEASE}"

runtime_lib_spec() {
    local name="$1" os="$2" arch="$3"
    case "${name}/${os}/${arch}" in
        # ---------------- pdfium (unpacked member: lib/libpdfium.so|.dylib) --
        pdfium/linux/x64)
            echo "${PDFIUM_BASE_URL}/pdfium-linux-x64.tgz 0b43f405477cf2cfc4dbff06905093c3309756c6bca1fb9da99234a2ca97fed2 lib/libpdfium.so libpdfium.so" ;;
        pdfium/linux/arm64)
            echo "${PDFIUM_BASE_URL}/pdfium-linux-arm64.tgz 0e6f90dccbc6b81fd5d7106abaf164c4222178f024c204d00d526b60fd2ad535 lib/libpdfium.so libpdfium.so" ;;
        pdfium/mac/x64)
            echo "${PDFIUM_BASE_URL}/pdfium-mac-x64.tgz 841ecac278cdd46288dd068873522cf72f3996560d8978f473d336f01d59942c lib/libpdfium.dylib libpdfium.dylib" ;;
        pdfium/mac/arm64)
            echo "${PDFIUM_BASE_URL}/pdfium-mac-arm64.tgz 336219e80580b93c6523f44db7dc1de59cc497b13a7390ddac84223f68ca162b lib/libpdfium.dylib libpdfium.dylib" ;;
        # ---------------- onnxruntime (real library under a VERSIONED name;
        # ---------------- extracted and installed under the plain soname) ----
        onnxruntime/linux/x64)
            echo "${ORT_BASE_URL}/onnxruntime-linux-x64-1.31.0.tgz cc5c72baf5ae5c8238a6841f0897227be2d02826b9cf98eaf02fdefaeeb03a57 onnxruntime-linux-x64-1.31.0/lib/libonnxruntime.so.1.31.0 libonnxruntime.so" ;;
        onnxruntime/linux/arm64)
            echo "${ORT_BASE_URL}/onnxruntime-linux-aarch64-1.31.0.tgz c5b8b3cca31f3d643a3b313b8f42e2d0f16a4fcefd02cda343d97bd5306afaab onnxruntime-linux-aarch64-1.31.0/lib/libonnxruntime.so.1.31.0 libonnxruntime.so" ;;
        onnxruntime/mac/arm64)
            echo "${ORT_BASE_URL}/onnxruntime-osx-arm64-1.31.0.tgz 1031b36dd3bdaa8d976a6130d9cf2a05e9be320b93ae804f8427b91dc3432afc ./onnxruntime-osx-arm64-1.31.0/lib/libonnxruntime.1.31.0.dylib libonnxruntime.dylib" ;;
        # NOTE macOS-x64: upstream stopped publishing osx-x64 assets (arm64
        # only) — an Intel-Mac box must supply its own dylib via VF_ORT_PATH.
        *)
            echo "runtime_lib_pins: no pinned asset for ${name}/${os}/${arch}" >&2
            return 1 ;;
    esac
}

runtime_lib_spec_docker() {
    local name="$1" target_arch="$2" arch
    case "${target_arch}" in
        amd64) arch="x64" ;;
        arm64) arch="arm64" ;;
        *) echo "runtime_lib_pins: unsupported TARGETARCH '${target_arch}'" >&2; return 1 ;;
    esac
    runtime_lib_spec "${name}" linux "${arch}"
}
