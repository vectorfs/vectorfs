#!/usr/bin/env bash
# size_guard.sh — release-prod binary size gate (design.md §5.7):
#   fail  > 175 MiB (hard cap; M0 adjudication: engine (datafusion+arrow+lance)
#                    is a fixed, non-croppable cost)
#   warn  > 155 MiB (early-warning line)
# Prints the measured size for CI step summaries (also written to
# $GITHUB_STEP_SUMMARY when running in Actions). Exit 0 = within hard cap.
#
# Usage: size_guard.sh [binary]
#   binary defaults to target/release-prod/vectorfs (the big SERVER binary;
#   the CI size-guard job). The release pipeline passes the cross-compiled
#   artifact path per target — the argument is resolved AFTER the repo-root
#   cd, so both absolute and repo-relative paths work. The thin `vf` client
#   bin is far smaller and intentionally not gated.
set -euo pipefail
cd "$(dirname "$0")/.."

BIN="${1:-target/release-prod/vectorfs}"
if [[ ! -f "$BIN" ]]; then
    echo "FAIL: $BIN not built; run: cargo build --profile release-prod --workspace" >&2
    exit 1
fi

bytes=$(stat -c %s "$BIN")
mib=$((bytes / (1024 * 1024)))
WARN=155   # MiB
FAIL=175   # MiB

line="vectorfs binary size [${BIN}]: ${bytes} bytes (${mib} MiB) — warn >${WARN}MiB, fail >${FAIL}MiB"
echo "$line"
if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
    echo "$line" >>"$GITHUB_STEP_SUMMARY"
fi

if (( mib > FAIL )); then
    echo "FAIL: ${mib} MiB exceeds the ${FAIL} MiB hard cap (design.md §5.7)" >&2
    exit 1
fi
if (( mib > WARN )); then
    echo "WARN: ${mib} MiB exceeds the ${WARN} MiB early-warning line (design.md §5.7)" >&2
fi
echo "size_guard: PASS (${mib} MiB)"
