#!/usr/bin/env bash
# install.sh — install the vf-search Agent Skill (focused semantic search
# over a vectorfs deployment) into an agent's skills directory.
#
#   curl -fsSL https://raw.githubusercontent.com/vectorfs/vectorfs/main/skills/vf-search/install.sh | bash
#
# Optional arguments (or env vars, for the curl|bash form):
#   --version <v>   git ref to install from (default: main; e.g. v0.2.0)
#   --to <dir>      skills directory ROOT (default: ~/.claude/skills; created)
#                   — the skill lands in <dir>/vf-search/
#                   (e.g. --to ~/.zcode/skills for ZCode)
#                   [env: VERSION / INSTALL_DIR]
#
# Behavior notes:
#   * plain markdown downloads — no checksums (the binary installer keeps
#     that bar; docs carry no such contract);
#   * GITHUB_TOKEN is honored if set.
set -euo pipefail

REPO="vectorfs/vectorfs"
VERSION="${VERSION:-main}"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.claude/skills}"

die() { echo "install-skill: error: $*" >&2; exit 1; }
note() { echo "install-skill: $*"; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version) VERSION="${2:?}"; shift 2 ;;
        --to) INSTALL_DIR="${2:?}"; shift 2 ;;
        -h|--help) sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown argument: $1 (see --help)" ;;
    esac
done

command -v curl >/dev/null || die "curl not found"

BASE="https://raw.githubusercontent.com/$REPO/$VERSION/skills/vf-search"
DEST="$INSTALL_DIR/vf-search"
FILES=(SKILL.md)

AUTH=()
[[ -n "${GITHUB_TOKEN:-}" ]] && AUTH=(-H "Authorization: Bearer $GITHUB_TOKEN")

mkdir -p "$DEST"
for F in "${FILES[@]}"; do
    note "fetching $F"
    curl -fsSL "${AUTH[@]}" -o "$DEST/$F" "$BASE/$F" \
        || die "download failed: $BASE/$F — does ref '$VERSION' exist?"
done

note "installed: $DEST (${#FILES[@]} file)"
note "restart your agent (skills load at startup) to pick it up"
