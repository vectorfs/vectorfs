#!/usr/bin/env bash
# install-cli.sh — install the `vf` client CLI (and optionally the
# `vectorfs` daemon binary) from the repo's GitHub Releases.
#
#   curl -fsSL https://raw.githubusercontent.com/vectorfs/vectorfs/main/scripts/install-cli.sh | bash
#
# Optional arguments (or env vars, for the curl|bash form):
#   --version <v>   release to install (default: latest; e.g. v0.1.0)
#   --to <dir>      install directory (default: ~/.local/bin; created)
#   --with-server   also install the `vectorfs` daemon binary
#                   [env: VERSION / INSTALL_DIR / WITH_SERVER=1]
#
# Behavior notes:
#   * downloads the release tarball AND sha256sums.txt — the asset is
#     verified before anything is installed (integrity is part of this
#     project's wire contract; the installer honors the same bar);
#   * the release pipeline ships linux x86_64 + aarch64 (glibc). Other
#     platforms fail with the cargo-install alternative instead of a
#     confusing download 404;
#   * GITHUB_TOKEN is honored if set (for the day the repo goes private).
set -euo pipefail

REPO="vectorfs/vectorfs"
VERSION="${VERSION:-latest}"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
WITH_SERVER="${WITH_SERVER:-0}"

die() { echo "install-cli: error: $*" >&2; exit 1; }
note() { echo "install-cli: $*"; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version) VERSION="${2:?}"; shift 2 ;;
        --to) INSTALL_DIR="${2:?}"; shift 2 ;;
        --with-server) WITH_SERVER=1; shift ;;
        -h|--help) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown argument: $1 (see --help)" ;;
    esac
done

# --- platform -> release target triple -------------------------------------
OS="$(uname -s)"; ARCH="$(uname -m)"
case "$OS:$ARCH" in
    Linux:x86_64)  TARGET="x86_64-unknown-linux-gnu" ;;
    Linux:aarch64|Linux:arm64) TARGET="aarch64-unknown-linux-gnu" ;;
    MINGW*_NT-*|MSYS*_NT-*|CYGWIN_NT-*)
        die "this bash runs under Windows — use PowerShell instead: \
iwr -useb https://raw.githubusercontent.com/$REPO/main/scripts/install-cli.ps1 | iex" ;;
    Darwin:*) die "the release pipeline ships linux + windows only; on macOS use: \
cargo install --git https://github.com/$REPO --profile release-prod vectorfs-client" ;;
    *) die "unsupported platform $OS/$ARCH (release assets: linux x86_64/aarch64 + \
windows x86_64 client zip)" ;;
esac

# --- resolve the version tag (no API: follow the /releases/latest redirect) --
AUTH=()
[[ -n "${GITHUB_TOKEN:-}" ]] && AUTH=(-H "Authorization: Bearer $GITHUB_TOKEN")
if [[ "$VERSION" == "latest" ]]; then
    redirect="$(curl -fsSI "${AUTH[@]}" -o /dev/null -w '%{redirect_url}' \
        "https://github.com/$REPO/releases/latest")" \
        || die "cannot reach github.com/$REPO/releases/latest"
    VERSION="$(basename "$redirect")"   # .../tag/v0.1.0 -> v0.1.0
    [[ "$VERSION" == v* ]] || die "unexpected latest redirect: $redirect"
fi
SEMVER="${VERSION#v}"
BASE="https://github.com/$REPO/releases/download/$VERSION"
ASSET="vectorfs-$SEMVER-$TARGET.tar.gz"

# --- download to a private tmpdir -------------------------------------------
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
note "downloading $ASSET ($VERSION)"
curl -fsSL "${AUTH[@]}" -o "$TMP/$ASSET" "$BASE/$ASSET" \
    || die "download failed — does $VERSION exist for $TARGET? see: \
https://github.com/$REPO/releases"
curl -fsSL "${AUTH[@]}" -o "$TMP/sha256sums.txt" "$BASE/sha256sums.txt" \
    || die "sha256sums.txt download failed (broken release)"

# --- verify BEFORE installing (subset check of the release's own sums) ------
command -v sha256sum >/dev/null || die "sha256sum not found (coreutils)"
grep -F "  $ASSET" "$TMP/sha256sums.txt" > "$TMP/check.txt" \
    || die "$ASSET missing from the release's sha256sums.txt"
( cd "$TMP" && sha256sum -c check.txt ) >/dev/null \
    || die "CHECKSUM MISMATCH for $ASSET — not installing"

# --- install ----------------------------------------------------------------
command -v tar >/dev/null || die "tar not found"
mkdir -p "$INSTALL_DIR"
BINS=(vf)
[[ "$WITH_SERVER" == "1" ]] && BINS+=(vectorfs)
tar -xzf "$TMP/$ASSET" -C "$TMP"
for BIN in "${BINS[@]}"; do
    [[ -f "$TMP/$BIN" ]] || die "$BIN missing from the tarball"
    install -m 0755 "$TMP/$BIN" "$INSTALL_DIR/$BIN"
done

# --- report + next steps ----------------------------------------------------
for BIN in "${BINS[@]}"; do note "installed: $INSTALL_DIR/$BIN"; done
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) note "NOTE: $INSTALL_DIR is not in PATH — add: export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac
"$INSTALL_DIR/vf" --version
note "next: vf config init --endpoint http://127.0.0.1:8787 --access-key AK --secret-key SK"
