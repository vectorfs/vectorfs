#!/usr/bin/env bash
# size_guard.sh — release-prod binary size gates (design.md §5.7 + §0
# three-crate-split ruling 2026-11-05):
#
#   SERVER (`vectorfs`, vectorfs-server):
#     fail  > 175 MiB (hard cap; M0 adjudication: engine (datafusion+arrow+
#                      lance) is a fixed, non-croppable cost)
#     warn  > 155 MiB (early-warning line)
#   CLIENT (`vf`, vectorfs-client — its OWN lines since the split):
#     fail  > 15 MiB (measured 6 MiB at the split; ~2.5x headroom — the
#                     small-client guarantee is a dependency-list property,
#                     and this gate is what keeps it from silently eroding)
#     warn  > 10 MiB (early-warning line)
#
# Prints the measured sizes for CI step summaries (also written to
# $GITHUB_STEP_SUMMARY when running in Actions). Exit 0 = all within caps.
#
# Usage: size_guard.sh [server-binary]
#   server-binary defaults to target/release-prod/vectorfs (the CI
#   size-guard job). The release pipeline passes the cross-compiled artifact
#   path per target — the argument is resolved AFTER the repo-root cd, so
#   both absolute and repo-relative paths work. The `vf` client binary is
#   looked up NEXT TO the server binary (both land in the same
#   target/<target>/release-prod/ directory on a workspace build).
set -euo pipefail
cd "$(dirname "$0")/.."

SERVER_BIN="${1:-target/release-prod/vectorfs}"
CLIENT_BIN="$(dirname "$SERVER_BIN")/vf"

for BIN in "$SERVER_BIN" "$CLIENT_BIN"; do
    if [[ ! -f "$BIN" ]]; then
        echo "FAIL: $BIN not built; run: cargo build --profile release-prod --workspace" >&2
        exit 1
    fi
done

rc=0

check() { # name bin warn fail
    local name="$1" bin="$2" warn="$3" fail="$4"
    local bytes mib line
    bytes=$(stat -c %s "$bin")
    mib=$((bytes / (1024 * 1024)))
    line="${name} binary size [${bin}]: ${bytes} bytes (${mib} MiB) — warn >${warn}MiB, fail >${fail}MiB"
    echo "$line"
    if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
        echo "$line" >>"$GITHUB_STEP_SUMMARY"
    fi
    if (( mib > fail )); then
        echo "FAIL: ${name} ${mib} MiB exceeds the ${fail} MiB hard cap (design.md §5.7)" >&2
        rc=1
    elif (( mib > warn )); then
        echo "WARN: ${name} ${mib} MiB exceeds the ${warn} MiB early-warning line (design.md §5.7)" >&2
    fi
}

check vectorfs "$SERVER_BIN" 155 175
check vf "$CLIENT_BIN" 10 15

if (( rc != 0 )); then
    exit "$rc"
fi
echo "size_guard: PASS (server + client within their lines)"
